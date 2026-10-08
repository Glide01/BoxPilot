# TUN goes through a privileged helper that runs its own sing-box on a config it has checked

**Status: proposed.** This is a design; nothing here is implemented yet.
Accepting it narrows ADR 0002 and refines ADR 0005 (see "Conflicts with
earlier ADRs"), and it leaves five questions to the owner (see "Open
questions").

TUN mode needs privileges the logged-in user doesn't have: a TUN device,
routes and DNS. BoxPilot gets them three different ways today:

- **Linux** gives a root-owned copy of sing-box the `CAP_NET_ADMIN` file
  capability and runs it **as the user** (ADR 0003).
- **Windows** elevates the **whole GUI** through UAC at every start
  (`ensure_elevated` in `main.rs`), and sing-box inherits Administrator.
- **macOS** has no TUN mode. ADR 0005 chose "a privileged launchd helper"
  for it, and noted that such a helper conflicts with ADR 0002.

This ADR designs that helper. macOS is its first user. Windows is its
second: the helper lets the Windows GUI stop running as Administrator.
Linux keeps its setcap copy, for the reason given at the end of "The
problem".

## What we defend against

**The goal, in Mullvad's words: neither the app nor its installers may
ever serve as a local privilege escalation vector.** Anything a process
can reach without an administrator prompt may give it the user's own
powers plus "bring TUN up or down", and nothing more.

Attackers, most likely first:

1. **A hostile profile config:** a malicious or compromised subscription,
   or a website that gets an import link confirmed. The config is remote
   input, and BoxPilot runs it nearly as written (ADR 0002).
2. **A website talking to localhost:** DNS rebinding or CSRF against any
   loopback HTTP service.
3. **Another local account** on a shared machine.
4. **Other processes running as the same user:** malware, or a
   compromised app.
5. **Hosts on the LAN,** through a listener a config opens on `0.0.0.0`.

**Accepted, as Tailscale and Mullvad accept it:** a process running as the
owner can do what the owner can do through BoxPilot, including starting
TUN with a config the policy allows. The helper's job is to make sure
that is *all* it gets.

**Out of scope:** attackers who already have root or Administrator,
physical access, and the moment of the one-time install (see "Install,
upgrade, removal").

## The problem: the config is a program's input, and that program may be root

These sing-box config fields reach beyond networking. They were checked
against `option/` in sing-box 1.12, 1.13 and 1.14.2, and against the 1.14
docs.

- **Write, create or delete files:**
  - `log.output`.
  - `experimental.cache_file.path`: a file there that isn't a bbolt
    database is deleted and recreated.
  - `clash_api.external_ui` with `external_ui_download_url`: downloads an
    archive and unpacks it.
  - The tor `data_directory`, and the tailscale `state_directory` and
    `taildrop_directory`.
  - The netns `pid_file`.
- **Read files:**
  - Every TLS `*_path` (certificates, keys, ECH), and the top-level
    `certificate` paths.
  - A local `rule_set`'s `path`, and the hosts DNS server's `path`.
  - SSH `private_key_path`.
  - The OpenVPN and OpenConnect key, certificate and secret paths.
- **Run a program:**
  - The tor outbound's `executable_path`, with `extra_args` ("The path to
    the Tor executable", "List of extra arguments passed to the Tor
    instance when started").
  - OpenConnect's `csd`, `hip` and `tncc` `wrapper_path` ("Path to an
    external … wrapper executable").
- **Change the system beyond networking:**
  - NTP `write_to_system`.
  - The Tailscale SSH server.
  - The `services`: the USB/IP server shares host USB devices, and DERP,
    ssm-api and ccm/ocm keep credentials and state on disk.
- **Open control planes:** `clash_api`, `v2ray_api` and `api` services,
  often on `0.0.0.0` with no secret. `external_ui` then serves a local
  directory over them.
- **Read the environment:** the CLI reads `SUDO_UID` / `SUDO_GID` to chown
  what it creates, and expands environment variables in some paths.

(Inbound-side features, such as hysteria2's file masquerade, can't arrive
at all: BoxPilot already replaces a profile's `inbounds` with its own.)

Run as the user, each of these is the user's own power. **Run as root or
SYSTEM, each one is a privilege-escalation primitive, and a profile config
is attacker input (1).** BoxPilot never writes any of these fields itself:
it injects only `inbounds`, `cache_file.enabled` and its own loopback
`api` service. Everything above comes from the profile.

Upstream projects have reached the same conclusion twice:

- **sing-box 1.14.0-alpha.45:** "configurations that use privileges
  unrelated to networking are now rejected by default; an insecure mode
  is available to allow them." The policy lives only in sing-box's own
  desktop client daemon (`experimental/boxdd`). That daemon confines file
  access to its working directory, resolving symlinks, and checks
  features one by one. **The plain `sing-box run` that BoxPilot bundles
  registers no policy.**
- **mihomo 1.19.6 (May 2025):** "For security reasons, all paths
  appearing in the configuration file will be limited to workdir."
  `SAFE_PATHS` widens that.

**Why Linux is already safe.** The setcap copy still runs as the user.
`CAP_NET_ADMIN` lets it open a TUN device and edit routes. It does not
bypass file permissions, and a program the config starts doesn't inherit
it. So a hostile config gets exactly the user's own file access, as in
Proxy mode, and ADR 0002's "run the config as written" is safe there. It
stops being safe once sing-box runs as root (macOS) or Administrator
(Windows today).

## Decision

A small **privileged helper** (`boxpilot-helper`) does two things:

- it owns the life of a sing-box **it installed itself**;
- it runs that sing-box only on a config **it has checked itself**.

The GUI stays unprivileged and **outside the helper's trust boundary**:
nothing the GUI sends is trusted.

Two questions, two mechanisms:

- **Authentication** decides *who* may ask.
- **The config policy** decides *what* a root sing-box may do.

**Any process running as the owner can ask, so the policy is the security
boundary.** Clash Verge Rev's maintainers came to the same conclusion
after three rounds of fixes: "Owner identity cannot carry this weight"
(see "Lessons").

### 1. A typed, minimal protocol

| Request | Carries → returns |
|---|---|
| `Hello` | protocol version → helper version, the installed sing-box's version and hash, whether the caller is the owner |
| `Start` | the profile config (bytes), its attachments, typed TUN options (IPv6, proxy port, Allow LAN, system proxy) → the API port and secret |
| `Stop` | — |
| `Status` | → running or stopped, the last exit reason |
| `Logs` | → sing-box's stdout/stderr lines for this session |

- **Never** a binary path, arguments, environment variables or file
  paths. Typed options go in, and the helper builds the privileged
  config itself.
- **The helper builds the privileged parts itself:**
  - the `tun` and mixed inbounds, from the typed options;
  - its own `api` service, on a loopback port with a fresh secret. Both
    come back in the `Start` reply, so the GUI's `SingBoxApi` works as it
    does now;
  - the cache file;
  - logging to stdout only.
  
  The injection half of `prepare_config` moves into a module the GUI and
  the helper share.
- **Framing:**
  - length-prefixed messages with hard caps sized for real profiles and
    rule sets (for example 32 MiB in total), checked *before* anything
    is allocated;
  - a read deadline, and one request at a time per connection;
  - malformed input closes the connection.
  
  Rust rules out OpenVPN's stack overflow (CVE-2024-27459). It does not
  rule out allocating whatever size a client declares.

### 2. The config policy is the boundary

The config policy is a pure function in the crate the GUI and the helper
share: JSON in, a checked config or a list of refusals out. It has no
gpui and no I/O, and is unit-tested the way `prepare_config` is. The GUI
runs it so it can explain a refusal before it asks the helper. The
helper runs it again, and only the helper's verdict counts.

- **Parse defensively,** with size and nesting limits.
  - Top-level keys come from an allowlist: `log`, `dns`, `ntp`,
    `certificate`, `endpoints`, `outbounds`, `route`, `experimental`.
  - `inbounds` and `services` must be absent; the helper adds its own.
- **Deny by shape, not by name.**
  - Any key at any depth that names a filesystem location is refused:
    `*_path`, `*_directory`, `path`, `directory`, `output`, `pid_file` and
    the like.
  - The exceptions: the fields the helper fills itself, and references
    to an attachment.
  - A path field upstream adds next year therefore fails closed, instead
    of passing until someone updates a denylist.
- **The fields the helper fills are overwritten, not refused:**
  - `log.output` is dropped;
  - `cache_file.path` is set by the helper;
  - each tailscale endpoint's `state_directory` and `taildrop_directory`
    go in the owner's state directory under the helper's tree. That keeps
    a Tailscale login across connects.
- **Files travel as content.**
  - When a profile references a local file (a `.srs` rule set, a CA
    certificate, a client key), the GUI reads it *as the user* and sends
    it as an attachment.
  - The helper writes each attachment under a name it picks, never the
    caller's, into a fresh root-only run directory (`O_EXCL`, no symlink
    following), and points the field there.
  - Inline forms (`certificate`, `key`, inline rule sets) need no
    attachment.
  - So the privileged side never opens a path a caller named. OpenVPN's
    interactive service shows why string path checks are a losing game:
    after CVE-2024-27903 (plugins loaded from arbitrary paths), its
    config-path checks needed fixing again in 2.6.22 (CVE-2026-63649) and
    2.6.23 (CVE-2026-81830, a prefix match that accepted sibling
    directories).
- **Refused on the privileged path, with the field named in the
  message:**
  - anything that runs a program: the tor outbound and the OpenConnect
    wrappers;
  - non-network system changes: NTP `write_to_system` and the Tailscale
    SSH server;
  - every profile `service`, which the absent-`services` rule already
    covers.
  
  A profile that needs one of these still runs in Proxy mode, as the
  user.
- **Control planes.** The profile's own `clash_api`, `v2ray_api` and
  `api` services do not run on the privileged path. Only the helper's
  `api` does, on loopback with a per-run secret. This narrows ADR 0002;
  see open question 1.
- **Environment.** sing-box starts with a scrubbed environment: no
  `SUDO_*`, and no user `HOME`. Its `-D`, working directory and `HOME`
  are the run directory, and stdin is null.
- **Audited at every `SINGBOX_VERSION` bump.**
  - Diff upstream's `option/` tree and the `api` service's RPCs for
    anything new that takes a path, opens a listener or runs a program.
  - Add a fixture for each new field.
  - The shape rule makes a missed field fail closed. The audit keeps the
    refusal message helpful.

**Defense in depth, under the policy.** Neither layer replaces the
policy.

- **Windows:** sing-box runs in a job object with `KILL_ON_JOB_CLOSE` and
  an active-process limit of 1, so it can't start child processes.
- **macOS:** sing-box runs under a sandbox profile that denies file writes
  outside the helper's tree, and execution of anything but
  `/usr/sbin/networksetup`. The profile is measured before it is
  enforced: TUN needs `utun`, routing sockets and `networksetup` for the
  system proxy.

### 3. The helper runs only its own sing-box

- **The binary's path is fixed at install,** in a directory chain whose
  every directory only administrators can write.
- **Checked on every spawn.** On each spawn, watchdog and restart paths
  included, the helper verifies that ownership chain and the file's
  SHA-256 against the manifest written at install. Clash Verge Rev's
  service had to add exactly this check to its restart paths.
- **Windows:** the helper opens the file with no write or delete sharing
  from the hash check until `CreateProcess`, so it can't change in
  between.
- **Not under `/usr/local` on macOS.** With Homebrew on an Intel Mac it
  belongs to the user, and the chain check would refuse it anyway.

### 4. Authentication by kernel identity

- **macOS:** the peer's uid from the socket: `LOCAL_PEERCRED`, or
  `LOCAL_PEERTOKEN` for the full audit token.
- **Windows:** impersonate the pipe client, and read the user SID from
  its token.
- **Not used:** PIDs (they are reused, so checks race), bundle IDs, the
  code signature of an ad-hoc build, or a secret compiled into a public
  binary.
- **Authorization:**
  - The *owner*, recorded at install in a root-owned file, and
    administrators may `Start` and `Stop`.
  - Other local accounts get `Hello` and `Status` only: no logs, no
    control. This is Tailscale's operator model.

### 5. Transports the OS protects; no loopback HTTP

- **macOS:** a Unix socket that **launchd creates** from the daemon's
  plist (`Sockets`, adopted with `launch_activate_socket`), in root-owned
  `/var/run`.
  - The mode is 0666, because every user shares group `staff`.
  - Every connection is authorized by uid (rule 4).
- **Windows:** a pipe the service creates under
  `\\.\pipe\ProtectedPrefix\Administrators\BoxPilot\helper`. Only
  administrators can create names under that prefix. Tailscale and
  sing-box's own daemon use it, though Microsoft doesn't document it. The
  pipe has:
  - `FILE_FLAG_FIRST_PIPE_INSTANCE`, which fails if the name has been
    squatted;
  - `PIPE_REJECT_REMOTE_CLIENTS`. A remotely reachable service pipe was
    OpenVPN's CVE-2024-24974;
  - the SDDL `D:P(A;;GA;;;SY)(A;;GA;;;BA)(A;;GRGW;;;IU)`, with no
    low-integrity label.
- **No TCP listener at all,** so DNS rebinding has nothing to reach
  (Tailscale's TS-2022-004 and TS-2022-005).

### 6. Lifecycle and cleanup

- **sing-box is session-bound.** It stops when the connection that
  started it closes, so a crashed GUI never leaves a root sing-box
  behind. That matches `PR_SET_PDEATHSIG` on Linux today, and it replaces
  the macOS pid file for TUN.
- **The helper starts on demand and exits when idle,** so no root process
  lingers while TUN is off.
  - **macOS:** launchd socket activation.
  - **Windows:** a demand-start service whose DACL grants interactive
    users `SERVICE_START` and `SERVICE_QUERY_STATUS` only, never
    `SERVICE_CHANGE_CONFIG`, which would let them repoint its binary.
- **Crash cleanup belongs to the helper,** which runs as root, under
  ADR 0005's conservative rules:
  - **macOS:** reset the system proxy only while it still points at
    `127.0.0.1`, and flush mDNSResponder (which needs root; see
    ADR 0005).
  - **Windows:** remove stale `sing-tun` adapters
    (`remove_tun_adapter`).
- **No shells at runtime.** Every tool is called by absolute path, never
  through a shell, and no `sh` or AppleScript string is built at runtime.

### 7. Install, upgrade, removal

**macOS** (no Developer ID, and macOS 12 is supported):

- **Not `SMAppService`.**
  - It needs macOS 13.
  - Apple DTS says the app and its helper must be signed "with the same
    Apple-issued code-signing identity", and that with ad hoc signing
    "you will see problems like this" (lost background permission after
    a restart).
  - An ad-hoc build's designated requirement is that one build's cdhash,
    so XPC's `setCodeSigningRequirement` can't name "BoxPilot" across
    versions either.
- **One administrator prompt instead.** `osascript … with administrator
  privileges` runs a fixed install script.
  - The AppleScript text is a constant. Values (the bundle path, the
    owner's uid) arrive through `on run argv`, and reach the shell only
    as positional arguments wrapped in `quoted form of`, never inside the
    script text. That is the rule `privilege::grant_command` already
    follows, and the same hostile-path test applies.
- **What it installs,** all root:wheel and not writable by anyone else:
  - `/Library/PrivilegedHelperTools/io.github.glide01.boxpilot.helper`;
  - sing-box, its manifest and the per-owner state in
    `/Library/Application Support/BoxPilot Helper/`;
  - the plist in `/Library/LaunchDaemons/`.
- **Login Items.** macOS 13+ lists the helper under Login Items. If the
  user turns it off there, BoxPilot treats it as not installed.

**Windows:**

- **The per-machine MSI,** which is already elevated, installs the
  service through its `ServiceInstall` / `ServiceControl` tables. No
  custom actions (Mandiant, 2023).
- **A fixed directory.** The helper and its own copy of sing-box go in
  `[ProgramFiles64Folder]BoxPilot\Helper`, which is *not* configurable.
  Today's MSI lets the user choose `APPLICATIONFOLDER`, and a SYSTEM
  service must never run from a folder its user picked.
- **State** goes in `%ProgramData%\BoxPilot\Helper`, created with a
  protected SYSTEM + Administrators DACL. On start, the service checks
  that ACL, and refuses a folder a user created first: `ProgramData`
  lets users create subfolders.

**Upgrade.** `Hello` reports the helper's protocol version and the hash of
its sing-box. When they don't match what this BoxPilot ships, the GUI
asks to reinstall, as ADR 0003's per-version grant does today. The helper
never runs a sing-box it didn't install.

**Removal** works without the app:

- the MSI uninstall on Windows;
- on macOS, two documented commands (`launchctl bootout`, then removing
  the three paths).

Settings › TUN also has a "Remove helper" button (one administrator
prompt).

**Install-time trust.** The payload is copied out of the user-writable app
bundle, so a same-user process could swap it during that one prompt. That
is the same exposure as any installer, and as ADR 0003's grant. After
install, nothing in the privileged path is user-writable.

**Passwords.** BoxPilot never sees, stores or forwards the administrator
password; only the OS prompt does. (v2rayN stored the user's sudo
password for TUN until 2025.)

### 8. Per platform

| | macOS (phase 1) | Windows (phase 2) | Linux |
|---|---|---|---|
| sing-box runs as | root, via the helper | SYSTEM, via the helper | the user + `CAP_NET_ADMIN` (ADR 0003) |
| The GUI runs as | the user | the user (`ensure_elevated` goes) | the user |
| Transport | launchd-created Unix socket | named pipe under ProtectedPrefix | — |
| Caller identity | `LOCAL_PEERCRED` uid | the impersonated token's SID | — |
| Config policy | enforced by the helper | enforced by the helper | not needed for privilege; see open question 2 |
| Proxy mode | unprivileged, unchanged | unprivileged | unprivileged |

**Linux stays on setcap.** A capability on a process that still runs as
the user is a smaller trusted surface than a long-lived root daemon with
a protocol. Two hardening notes:

- Never add `CAP_DAC_READ_SEARCH` or `CAP_SYS_PTRACE` to the copy, even
  though sing-box's own systemd unit grants both for process-matching
  rules. The first can read any file.
- If Linux ever gets a daemon, it follows rules 1–7: a socket in a
  root-owned directory, checked with `SO_PEERCRED` (or `SO_PEERPIDFD`).
  Never polkit's PID-based `unix-process` subject, which polkit itself
  documents as racy.

## Lessons from comparable software

Only published advisories, fixes and audits are cited here.

- **Clash Verge Rev**, three rounds:
  - **CVE-2025-50505.** An unauthenticated localhost HTTP endpoint made
    its root/SYSTEM service start a caller-supplied `bin_path`.
  - **The IPC that replaced it** became **CVE-2026-26422**
    ("world-reachable IPC endpoint", CWE-732), fixed in
    clash-verge-service-ipc 2.3.0.
  - **Owner identity.** Even an authenticated owner could have it start
    any core, until the maintainers concluded that "Owner identity cannot
    carry this weight: every local account may legitimately become an
    owner". The service now spawns only core copies an administrator
    staged, and they had to "validate the core path on every spawn, not
    just on IPC", watchdog restarts included.
  
  → No binary path in the protocol (1). Authentication is not
  authorization (2). Check on every spawn path (3).
- **Clash for Windows, CVE-2022-40126.** A Service Mode profile directory
  that users could write led to privilege escalation. → Everything a
  privileged process consumes is admin-only, or travels as content (2, 7).
- **FlClash, issue #1131 (2025).** Its helper verified a caller-supplied
  core path by hash, then started that path: check-then-use on a file the
  caller controls. → The binary is fixed and held from check to exec (3).
- **v2rayN.** It stored the user's sudo password, encrypted, for TUN on
  Linux until 2025, and now keeps it in memory only. → Never handle the
  administrator password (7).
- **mihomo 1.19.6.** It confines every config path to the work directory.
  Its `SAFE_PATHS` escape hatch protects only when the *privileged* side
  sets it. → The policy lives in the helper (2).
- **sing-box 1.14's desktop daemon.** It has a feature policy and a file
  manager confined to its work directory. `sing-box run` has neither. →
  BoxPilot's helper enforces its own (2). If `sing-box run` ever exposes
  that policy, it becomes a second layer, not the only one.
- **WireGuard for Windows.**
  - Its tunnel service drops every privilege but
    `SeLoadDriverPrivilege`.
  - Its UI gets inherited unnamed pipes, so there is no named endpoint to
    attack.
  - `PostUp` scripts stay off unless an admin-only registry value
    (`DangerousScriptExecution`) turns them on.
  
  → On the privileged path there is no GUI switch for a refused
  feature. If one is ever needed, it is an admin-only setting, as with
  WireGuard. Measuring what wintun needs and dropping the rest is phase-2
  hardening.
- **Tailscale, CVE-2022-41924 and CVE-2022-41925.** Websites could reach
  its loopback HTTP API through DNS rebinding, and on Windows reconfigure
  the daemon. Its LocalAPI is now a ProtectedPrefix named pipe on
  Windows, and on Unix a socket that classifies each caller by uid. →
  Rules 4 and 5.
- **OpenVPN interactive service, CVE-2024-27459, -24974 and -27903,**
  reported by Microsoft and fixed in 2.6.10:
  - a client-declared message size was copied into a stack buffer;
  - the service pipe was reachable remotely;
  - plugins loaded from arbitrary paths.
  
  Its config-path checks then needed fixing again in 2.6.22
  (CVE-2026-63649) and 2.6.23 (CVE-2026-81830). → Rules 1, 2 and 5.
- **Mullvad.** Its documented threat model: any local process may reach
  the management interface, but "neither the app nor the installers ever
  serve as a local privilege escalation vector", and websites must not
  reach it. Its 2024 audit found that the Windows installer ran a
  `taskkill.exe` lying next to it rather than the system's
  (MLLVD-CR-24-06, High, fixed in 2024.8). → Rules 5, 6 and 7.
- **Stats, CVE-2025-21606 (macOS).** Its XPC helper's
  `shouldAcceptNewConnection` "unconditionally returns YES". Validate the
  audit token or a code-signing requirement, never the PID. → Rule 4, and
  the reason an ad-hoc-signed BoxPilot falls back to the uid.
- **Windows installers (Mandiant, 2023; Atera CVE-2023-26077 and
  CVE-2023-26078).** SYSTEM custom actions touched user-writable folders
  and spawned console windows. The advice: protected install locations,
  explicit folder ACLs, quiet commands. → Rule 7.

## Conflicts with earlier ADRs

- **ADR 0002 is narrowed.**
  - "A config's own controllers run exactly as written" still holds
    while sing-box runs as the user: Proxy mode everywhere, and TUN on
    Linux.
  - On the privileged path, the profile's `clash_api`, `v2ray_api` and
    `api` services don't run, and the policy above applies.
  - This reverses part of a decision the owner made explicitly, so it
    needs their sign-off (open question 1).
- **ADR 0005 is refined.** The plan stands, with three changes:
  - its `SMAppService` branch waits until BoxPilot has a Developer ID;
  - callers are authenticated by uid, not by code signature;
  - its "paths forced into its own directory … or sandboxed" becomes:
    paths refused unless they come as attachments, the listed features
    refused, *and* a sandbox.
- **ADR 0003 is unchanged,** apart from the two hardening notes above.
- **Windows `ensure_elevated` goes in phase 2.** The deep-link pipe's
  `Everyone` + low-integrity DACL exists only so a non-elevated sender
  can reach an elevated GUI. Once the GUI isn't elevated, the pipe can
  take the default DACL, add `PIPE_REJECT_REMOTE_CLIENTS`, and cap the
  size of what it reads.

## Open questions for the owner

1. **Dashboards in helper TUN mode.**
   - *Proposed:* the profile's own controllers don't run.
   - *Alternative:* run them on loopback only, with a secret, without
     `external_ui`. That keeps Yacd-style dashboards working, at the cost
     of a control plane on a root process that every local account and
     every DNS-rebinding page can probe, guarded by one secret.
2. **The policy for unprivileged starts too?** A profile that makes
   sing-box start a program is dangerous at any privilege level, and
   upstream's own client now refuses that by default.
   - *Recommended:* refuse the tor outbound's `executable_path` and the
     OpenConnect wrappers on every start, with an override that a local
     profile can opt into, but a subscription never can.
   - This narrows ADR 0002 for everyone.
3. **Shared machines.**
   - *Proposed:* only the owner and administrators may start or stop.
   - *Alternative:* any console user.
4. **Session-bound sing-box.**
   - *Proposed:* sing-box stops with its session.
   - *Alternative:* sing-box survives a GUI crash and the GUI reattaches.
5. **Phases.**
   - *Proposed:* macOS first, because it unblocks TUN there.
   - *Then Windows,* because it ends the elevated GUI.

## Consequences

- **Code.**
  - The repository becomes a Cargo workspace. The helper is its own
    crate (`boxpilot-helper`), so the privileged binary's dependency
    graph has no gpui and no reqwest.
  - The config policy and the protocol live in a small, pure crate
    (no I/O, no gpui) that both the GUI and the helper use.
  - `ProcessSession` gets a second backend, a helper session, next to the
    local child.
  - On macOS, `TUN_AVAILABLE` becomes "the helper is installed and
    current".
  - "Install helper" is a prompt like Linux's grant prompt, and
    Settings › TUN shows the helper's state with a Remove button.
  - The UI term is "Privileged helper" (特权助手, already used in the
    README). It goes into `CONTEXT.md` when this ADR is accepted.
- **Cache file.** In helper TUN mode `cache.db` lives in the helper's
  tree, so the selected nodes and the Clash mode are remembered per mode.
  BoxPilot can replay the last selection through the API on start.
- **System proxy.**
  - On macOS, a root sing-box's `networksetup` works on standard
    accounts too, which closes a gap ADR 0005 notes.
  - On Windows, a SYSTEM sing-box's `set_system_proxy` would write
    SYSTEM's settings. So in phase 2 the GUI either sets the user's proxy
    itself, or TUN mode leaves the system proxy off.
- **Profiles that need a refused feature** run in Proxy mode only. A TUN
  start says which field was refused and why.

## Verification before shipping

- **Policy unit tests:**
  - real subscription fixtures pass with their meaning unchanged;
  - there is one hostile fixture per refused class;
  - an unknown path-shaped key fails closed;
  - attachments land only in the run directory.
- **Fuzzing** (`cargo fuzz`) of the frame decoder and the policy.
- **An integration test** through a `--root <dir>` test seam, like
  `BOXPILOT_DATA_DIR`, so the helper runs unprivileged in CI against a
  temporary tree.
- **A release checklist on real machines.** Each of these must hold:
  - another account's connection gets no `Start`;
  - a remote client is refused, and so is an oversized frame;
  - a squatted pipe, socket or `ProgramData` folder is refused;
  - sing-box dies with its session;
  - a sing-box that was swapped or `chmod`ed is refused;
  - nothing listens on TCP except sing-box's own ports.
- **An external review** of the helper and the policy before the first
  release that ships them.

## Considered options

- **setuid-root sing-box.** Rejected, as ADR 0005 already recorded: any
  user process gets a root sing-box running its own config, with nothing
  in between.
- **Elevate the whole GUI** (Windows today). That puts subscription
  parsing, deep links, HTTP and rendering in an admin process. It stays
  only until phase 2.
- **sing-box's own desktop daemon as the helper.** It would bring
  upstream's policy for free, but it is not usable. On Windows its pipe
  admits only its own signed clients (ADR 0002), and there is none for
  macOS.
- **A Go helper that embeds sing-box with upstream's policy registered.**
  Upstream would maintain the policy feature by feature. But BoxPilot
  would build sing-box itself instead of shipping the release binary, the
  hook is internal API, and the helper would be Go in a Rust project.
  Revisit if `sing-box run` ever exposes the policy.
- **A denylist of known-dangerous fields.** Easier against today's
  schema, but it silently reopens the hole the next time upstream adds a
  field. Rejected for deny-by-shape.
- **Loopback HTTP or gRPC with a token** for the helper's own protocol.
  It invites DNS rebinding (Tailscale's CVE-2022-41924), and the token
  has to live in a file that some process can read. Rejected.
- **XPC with `setCodeSigningRequirement`, and `SMAppService`.** The right
  tools with a Developer ID, but unavailable to an ad-hoc-signed
  BoxPilot. Revisit together with a Network Extension.
- **A Network Extension on macOS,** the official client's way. No root
  process at all. It needs a paid Apple Developer account, Swift and
  libbox. It is still the ideal end state.
- **A systemd unit with `AmbientCapabilities` on Linux.** It sandboxes
  well, but it is a root-managed service plus an install step and a
  protocol, all to buy what setcap already gives. Rejected for now.
- **An administrator prompt on every connect,** with no helper. Nothing
  privileged lingers, but it puts a password prompt on a toggle people
  use daily. ADR 0003 rejected this for the same reason.
