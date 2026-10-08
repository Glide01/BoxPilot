# TUN goes through a privileged helper that runs its own sing-box on a config it has checked

**Status: proposed.** This is a design; nothing here is implemented yet.
Its trade-offs are settled by separation of tasks (课题分离, below).
Accepting it refines ADR 0005, and draws a boundary around ADR 0002
without changing it (see "Conflicts with earlier ADRs").

TUN mode needs privileges the logged-in user doesn't have: a TUN device,
routes and DNS. BoxPilot gets them three different ways today:

- **Linux** gives a root-owned copy of sing-box the `CAP_NET_ADMIN` file
  capability and runs it **as the user** (ADR 0003).
- **Windows** elevates the **whole GUI** through UAC at every start
  (`ensure_elevated` in `main.rs`), and sing-box inherits Administrator.
- **macOS** has no TUN mode. ADR 0005 chose "a privileged launchd helper"
  for it, and noted that such a helper conflicts with ADR 0002.

This ADR designs that helper:

- **Windows is its first user.** The helper lets the GUI stop running as
  Administrator, which today lends admin rights to whatever a profile
  asks for.
- **macOS is its second.** The helper brings TUN there.
- **Linux keeps its setcap copy,** for the reason given at the end of
  "The problem".

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

Against (1), BoxPilot defends the privilege *it* lends, not the user's
own (see the next section).

**Accepted, as Tailscale and Mullvad accept it:** a process running as the
owner can do what the owner can do through BoxPilot, including starting
TUN with a config the policy allows. The helper's job is to make sure
that is *all* it gets.

**Out of scope:** attackers who already have root or Administrator,
physical access, and the moment of the one-time install (see "Install,
upgrade, removal").

## Whose task is it (separation of tasks, 课题分离)

Every trade-off in this ADR is settled by one question: **whose task is
it?** A risk belongs to whoever's privilege is at stake and whoever made
the choice. BoxPilot does its own task completely, and stays out of
everyone else's.

| Whose task | What it covers | So BoxPilot … |
|---|---|---|
| The user's | which profiles to trust; what a profile does with the user's own privilege; privilege the user brings of their own accord (running BoxPilot as root or Administrator) | runs the profile as written (ADR 0002) and doesn't police it |
| BoxPilot's | privilege BoxPilot acquires and lends: the helper's root or SYSTEM, and today's self-elevation on Windows | lets no profile get more of it than "bring TUN up or down" |
| The administrator's | who may control machine-wide networking on a shared machine | follows what the OS says (an administrator prompt, group membership), with no setting of its own |
| The OS's | passwords, peer identity, file permissions | uses them and never reimplements them |
| Upstream's | sing-box's features and its own policy | neither forks nor patches sing-box |
| Other projects' | their unfixed bugs | cites only published fixes |

**The line between the first two rows is the whole design.** The same
profile runs as written at the user's privilege, and under the policy at
privilege BoxPilot lends. Windows today is the one place where the two
are mixed, because BoxPilot elevates everything. That is why Windows
comes first.

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

**At the user's privilege, each of these is the user's own power, and the
user's task. At root or SYSTEM, each one is a privilege-escalation
primitive, and it is BoxPilot's task, because BoxPilot lent that
privilege.** BoxPilot never writes any of these fields itself: it injects
only `inbounds`, `cache_file.enabled` and its own loopback `api` service.
Everything above comes from the profile.

Upstream projects have reached the same conclusion twice:

- **sing-box 1.14.0-alpha.45:** "configurations that use privileges
  unrelated to networking are now rejected by default; an insecure mode
  is available to allow them."
  - The policy lives only in sing-box's own desktop client daemon
    (`experimental/boxdd`), which confines file access to its working
    directory (resolving symlinks) and checks features one by one.
  - **The plain `sing-box run` that BoxPilot bundles registers no
    policy.**
- **mihomo 1.19.6 (May 2025):** "For security reasons, all paths
  appearing in the configuration file will be limited to workdir."
  `SAFE_PATHS` widens that.

**Why Linux is already fine.** The setcap copy still runs as the user.
`CAP_NET_ADMIN` lets it open a TUN device and edit routes. It does not
bypass file permissions, and a program the config starts doesn't inherit
it. So what BoxPilot lends there is network-only, and everything else a
hostile config does happens at the user's own privilege, as in Proxy
mode. That stops being true once sing-box runs as root (macOS) or as
Administrator (Windows today).

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
| `Hello` | protocol version → helper version, the installed sing-box's version and hash, whether the caller may start |
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

It applies **only to privilege BoxPilot lends.** A profile that runs at
the user's own privilege never meets it.

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
- **Files travel as content.** Which files a profile may read is decided
  by the user's own access, the user's task; the helper never reads one
  on its own privilege.
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
  
  A profile that needs one of these still runs in Proxy mode, at the
  user's own privilege. Choosing between the two is the user's call.
- **Control planes.** The profile's own `clash_api`, `v2ray_api` and
  `api` services do not run on the privileged path: a control plane over
  a root process is BoxPilot's task, not the profile's. Only the helper's
  `api` runs, on loopback with a per-run secret. A profile's dashboard
  keeps working in Proxy mode, as written.
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

### 4. Who may ask: kernel identity, and the administrator decides

**Identity:**

- **Windows:** impersonate the pipe client, and read the user SID and
  groups from its token.
- **macOS:** the peer's uid from the socket: `LOCAL_PEERCRED`, or
  `LOCAL_PEERTOKEN` for the full audit token.
- **Not used:** PIDs (they are reused, so checks race), bundle IDs, the
  code signature of an ad-hoc build, or a secret compiled into a public
  binary.

**Authorization** is the administrator's task, so it comes from the OS,
not from a BoxPilot setting:

- **Windows:** members of Administrators (elevated or not) and of Network
  Configuration Operators may `Start` and `Stop`, as in WireGuard for
  Windows. Who belongs to those groups is the administrator's call.
- **macOS:** the *owner*, the account an administrator authorized
  through the install prompt, recorded in a root-owned file.
  - Another account takes over with its own administrator prompt.
  - The last account authorized holds it, as with ADR 0003's grant on
    Linux.
- **Everyone else** gets `Hello` and `Status` only: no logs, no control.
  This is Tailscale's operator model.

### 5. Transports the OS protects; no loopback HTTP

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
- **macOS:** a Unix socket that **launchd creates** from the daemon's
  plist (`Sockets`, adopted with `launch_activate_socket`), in root-owned
  `/var/run`.
  - The mode is 0666, because every user shares group `staff`.
  - Every connection is authorized by uid (rule 4).
- **No TCP listener at all,** so DNS rebinding has nothing to reach
  (Tailscale's TS-2022-004 and TS-2022-005).

### 6. Lifecycle and cleanup

- **sing-box is session-bound.** It stops when the connection that
  started it closes. A root sing-box nobody is asking for any more would
  be a task nobody owns, so a crashed GUI never leaves one behind. That
  matches `PR_SET_PDEATHSIG` on Linux today, and ADR 0004's "quitting
  stops sing-box". It also replaces the macOS pid file for TUN.
- **The helper starts on demand and exits when idle,** so no root process
  lingers while TUN is off.
  - **Windows:** a demand-start service whose DACL grants interactive
    users `SERVICE_START` and `SERVICE_QUERY_STATUS` only, never
    `SERVICE_CHANGE_CONFIG`, which would let them repoint its binary.
  - **macOS:** launchd socket activation.
- **Crash cleanup belongs to the helper,** which runs as root, under
  ADR 0005's conservative rules:
  - **Windows:** remove stale `sing-tun` adapters
    (`remove_tun_adapter`).
  - **macOS:** reset the system proxy only while it still points at
    `127.0.0.1`, and flush mDNSResponder (which needs root; see
    ADR 0005).
- **No shells at runtime.** Every tool is called by absolute path, never
  through a shell, and no `sh` or AppleScript string is built at runtime.

### 7. Install, upgrade, removal

**Windows:**

- **The per-machine MSI,** which is already elevated, installs the
  service through its `ServiceInstall` / `ServiceControl` tables. No
  custom actions (Mandiant, 2023). So TUN needs no prompt of its own on
  Windows.
- **A fixed directory.** The helper and its own copy of sing-box go in
  `[ProgramFiles64Folder]BoxPilot\Helper`, which is *not* configurable.
  Today's MSI lets the user choose `APPLICATIONFOLDER`, and a SYSTEM
  service must never run from a folder its user picked.
- **State** goes in `%ProgramData%\BoxPilot\Helper`, created with a
  protected SYSTEM + Administrators DACL. On start, the service checks
  that ACL, and refuses a folder a user created first: `ProgramData`
  lets users create subfolders.

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
  privileges` runs a fixed install script, and the same prompt records
  the owner (rule 4).
  - The AppleScript text is a constant. Values (the bundle path, the
    owner's uid) arrive through `on run argv`, and reach the shell only
    as positional arguments wrapped in `quoted form of`, never inside the
    script text.
  - That is the rule `privilege::grant_command` already follows, and the
    same hostile-path test applies.
- **What it installs,** all root:wheel and not writable by anyone else:
  - `/Library/PrivilegedHelperTools/io.github.glide01.boxpilot.helper`;
  - sing-box, its manifest and the per-owner state in
    `/Library/Application Support/BoxPilot Helper/`;
  - the plist in `/Library/LaunchDaemons/`.
- **Login Items.** macOS 13+ lists the helper under Login Items. If the
  user turns it off there, BoxPilot treats it as not installed.

**Upgrade.** `Hello` reports the helper's protocol version and the hash of
its sing-box. When they don't match what this BoxPilot ships:

- on Windows, the MSI upgrade replaces both;
- on macOS, the GUI asks to reinstall, as ADR 0003's per-version grant
  does today.

The helper never runs a sing-box it didn't install.

**Removal** works without the app: the MSI uninstall on Windows, and two
documented commands on macOS (`launchctl bootout`, then removing the
three paths). On macOS, Settings › TUN also has a "Remove helper" button
(one administrator prompt).

**Install-time trust.** On macOS the payload is copied out of the
user-writable app bundle, so a same-user process could swap it during
that one prompt. That is the same exposure as any installer, and as
ADR 0003's grant: the user's session at that moment is the user's task.
After install, nothing in the privileged path is user-writable.

**Passwords** are the OS's task. BoxPilot never sees, stores or forwards
the administrator password; only the OS prompt does. (v2rayN stored the
user's sudo password for TUN until 2025.)

### 8. Per platform

| | Windows (phase 1) | macOS (phase 2) | Linux |
|---|---|---|---|
| sing-box runs as | SYSTEM, via the helper | root, via the helper | the user + `CAP_NET_ADMIN` (ADR 0003) |
| The GUI runs as | the user (`ensure_elevated` goes) | the user | the user |
| Transport | named pipe under ProtectedPrefix | launchd-created Unix socket | — |
| Caller identity | the impersonated token | `LOCAL_PEERCRED` uid | — |
| Who may start TUN | Administrators, Network Configuration Operators | the owner an administrator authorized | the user an administrator granted (ADR 0003) |
| Config policy | enforced by the helper | enforced by the helper | none: the profile runs at the user's privilege |
| Proxy mode | unprivileged, as written | unprivileged, as written | unprivileged, as written |

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

**Privilege the user brings is theirs.** BoxPilot no longer elevates
itself anywhere. When the user runs BoxPilot as Administrator or root of
their own accord, that privilege is theirs to lend. BoxPilot then starts
sing-box directly, as written, the way `TunPlan::UseBundled` already does
on Linux. Only privilege BoxPilot has to obtain goes through the helper.

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
  authorization (2). Check on every spawn path (3). Becoming an owner
  takes an administrator (4).
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
  manager confined to its work directory; `sing-box run` has neither. →
  BoxPilot's helper enforces its own (2). If `sing-box run` ever exposes
  that policy, it becomes a second layer, not the only one.
- **WireGuard for Windows.**
  - Its tunnel service drops every privilege but
    `SeLoadDriverPrivilege`.
  - Its UI gets inherited unnamed pipes, so there is no named endpoint to
    attack.
  - It serves Administrators, and Network Configuration Operators when an
    administrator allows it.
  - `PostUp` scripts stay off unless an admin-only registry value
    (`DangerousScriptExecution`) turns them on.
  
  → Authorization from OS groups (4). On the privileged path there is no
  GUI switch for a refused feature; if one is ever needed, it is an
  admin-only setting, as with WireGuard. Measuring what wintun needs and
  dropping the rest is Windows-phase hardening.
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

- **ADR 0002 keeps its scope.**
  - "A config's own controllers run exactly as written" governs the
    profile at the user's own privilege: Proxy mode everywhere, TUN on
    Linux, and both modes on Windows once the GUI is no longer elevated.
  - The privileged path is ground ADR 0002 never covered; there the
    policy above applies.
  - Windows today is where the two were mixed, because BoxPilot lent
    admin to everything. Phase 1 separates them by not elevating, not by
    policing profiles.
- **ADR 0005 is refined.** The plan stands, with four changes:
  - its `SMAppService` branch waits until BoxPilot has a Developer ID;
  - callers are authenticated by uid, not by code signature;
  - its "paths forced into its own directory … or sandboxed" becomes:
    paths refused unless they come as attachments, the listed features
    refused, *and* a sandbox;
  - macOS becomes the second phase, after Windows.
- **ADR 0003 is unchanged,** apart from the two hardening notes above.
- **Windows `ensure_elevated` goes in phase 1.** The deep-link pipe's
  `Everyone` + low-integrity DACL exists only so a non-elevated sender
  can reach an elevated GUI. Once the GUI isn't elevated, the pipe can
  take the default DACL, add `PIPE_REJECT_REMOTE_CLIENTS`, and cap the
  size of what it reads.

## Trade-offs, decided

Each is settled by the separation above. The owner can still override
any of them.

1. **Dashboards in helper TUN mode: off.** A control plane over a root
   process is BoxPilot's task. Wanting a dashboard with every profile
   feature is the user's choice, and Proxy mode serves it as written.
2. **No policy for starts at the user's own privilege.**
   - What a profile does there is the user's task, so ADR 0002 stands
     unchanged.
   - An earlier draft of this ADR recommended refusing program-running
     features on every start. That would have been BoxPilot doing the
     user's task, so it is withdrawn.
3. **Shared machines: the administrator decides,** through the OS
   (rule 4). BoxPilot adds no multi-user setting of its own.
4. **sing-box is session-bound.** Keeping a root process alive for a GUI
   that is gone is a task nobody gave the helper.
5. **Windows first, then macOS.**
   - Windows' self-elevation is BoxPilot's own task, and it lends admin
     to profiles today.
   - macOS lacks a feature, but nothing there is exposed.
   - Security comes first.

## Consequences

- **Code.**
  - The repository becomes a Cargo workspace. The helper is its own
    crate (`boxpilot-helper`), so the privileged binary's dependency
    graph has no gpui and no reqwest.
  - The config policy and the protocol live in a small, pure crate
    (no I/O, no gpui) that both the GUI and the helper use.
  - `ProcessSession` gets a second backend, a helper session, next to
    the local child.
  - On Windows, `ensure_elevated` is removed, and TUN uses the helper
    the MSI installs.
  - On macOS, `TUN_AVAILABLE` becomes "the helper is installed and
    current". "Install helper" is a prompt like Linux's grant prompt, and
    Settings › TUN shows the helper's state with a Remove button.
  - The UI term is "Privileged helper" (特权助手, already used in the
    README). It goes into `CONTEXT.md` when this ADR is accepted.
- **The portable exe** uses the helper if the MSI installed one. Run as
  Administrator by the user's own choice, it starts sing-box directly, as
  written, as it does today. With neither, its TUN mode is unavailable,
  and Home says how to get it.
- **Cache file.** In helper TUN mode, `cache.db` lives in the helper's
  tree, so the selected nodes and the Clash mode are remembered per mode.
  BoxPilot can replay the last selection through the API on start.
- **System proxy.**
  - **Windows:** the user's proxy setting is the user's task. In phase 1
    the GUI sets it itself, as the user; a SYSTEM sing-box never writes
    it.
  - **macOS:** a root sing-box's `networksetup` also works on standard
    accounts, which closes a gap ADR 0005 notes.
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
  - an account the administrator hasn't authorized gets no `Start`;
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
- **Elevate the whole GUI** (Windows today). It lends admin rights to
  profile content, mixing the user's task with BoxPilot's, and it puts
  subscription parsing, deep links, HTTP and rendering in an admin
  process. It ends in phase 1.
- **A policy on every start** (an earlier draft's recommendation). It
  would guard users against profiles they chose, at their own privilege:
  the user's task, not BoxPilot's. Rejected.
- **A first-come owner** (the first account to connect becomes the
  owner, with no prompt). It would make BoxPilot decide who controls
  machine-wide networking, which is the administrator's task. Rejected.
- **sing-box's own desktop daemon as the helper.** It would bring
  upstream's policy for free, but it is not usable: on Windows its pipe
  admits only its own signed clients (ADR 0002), and there is none for
  macOS.
- **A Go helper that embeds sing-box with upstream's policy registered.**
  Upstream would maintain the policy feature by feature. But building and
  patching sing-box is upstream's task, the hook is internal API, and the
  helper would be Go in a Rust project. Revisit if `sing-box run` ever
  exposes the policy.
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
