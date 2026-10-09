# Privileged helper: macOS verification checklist

The macOS helper (ADR 0006, phase 2) is built: the launchd daemon, its
install and uninstall scripts, its payload in the DMG, the GUI's side (its
client, Settings › TUN's install, reinstall and remove, TUN's availability
following the helper), and sing-box's sandbox profile, enforced: deny by
default, allowing what CI measured sing-box doing. It has not run on a
Mac a person uses, on an Intel Mac, on macOS 12 or 13, or under the GUI,
and sing-box's sandbox has met only CI's profiles. Run this on clean Macs (macOS 12, 13 and the current release; Apple
silicon and Intel) with an administrator account and a standard account
before the first release that ships it. Each item names what must hold.

**CI covers part of it.** The release workflow's `macos` job builds the
DMG, checks the helper's payload in it with the helper's own manifest
parser (both architectures), and on GitHub's Apple-silicon runner
(`macos-14`, a VM) installs the helper from that DMG and drives the daemon
from outside with a smoke client (`crates/boxpilot-helper/examples/mac_smoke.rs`),
as the runner's account (the owner, an administrator with passwordless
sudo), as root and as a fresh standard account, through
`packaging/macos/helper-smoke.sh` (one step per CI step), then with the
GUI's own client code (`examples/gui_helper_smoke.rs`, built from
BoxPilot's crate, never shipped). Items or parts marked **CI** are checked
there. `sudo` stands in for the administrator
prompt, and the runner is a VM with one network service, so a release still
needs the manual run; what is unmarked is manual only. The helper's POSIX
layer (`src/posix`: the socket, the peer's uid, the accept loop, the
verification, `posix_spawn`, stop and reap, the supervisor) is also
unit-tested on Linux and, by the job's Test step, on macOS.

## Install, upgrade, uninstall

- The administrator prompt (phase 2: `osascript`, Settings › TUN) shows
  BoxPilot's text and installs. Cancelling it changes nothing. A standard
  account is asked for an administrator's name and password.
  (**CI** runs `helper-install.sh` with sudo instead; the AppleScript's
  constant text and its arguments are unit-tested.)
- `helper-install.sh <Contents> <uid>` installs, all root:wheel (**CI**,
  with `stat`):
  - `/Library/PrivilegedHelperTools/io.github.glide01.boxpilot.helper` 0755;
  - `/Library/Application Support/BoxPilot Helper` and `bin` 0755, holding
    exactly `sing-box` 0755 and `manifest.json` 0644 (**CI**), byte for
    byte the app's sing-box (**CI**), hashed by the manifest (**CI**);
  - `state` 0700, with `owner` 0600 naming the uid (**CI**);
  - `/Library/LaunchDaemons/io.github.glide01.boxpilot.helper.plist` 0644,
    the repository's (**CI**), and `plutil -lint` clean (**CI**).
- Running it again is harmless (**CI**); running it with another account's
  uid makes that account the owner, and only that one (**CI**). A uid of
  0, with a leading zero, not a number, or a relative `Contents` path is
  refused before anything changes (**CI**).
- `launchctl print system/io.github.glide01.boxpilot.helper` shows the job
  loaded, with the helper's path (**CI**). The socket
  `/var/run/io.github.glide01.boxpilot.helper.sock` exists, root:wheel
  0666, before any client connects (**CI**).
- macOS 13 and later: the "Background Items Added" notification, and
  System Settings › General › Login Items lists the helper under BoxPilot's
  name (`AssociatedBundleIdentifiers`). Turning it off there unloads it;
  the GUI then treats it as not installed (see "GUI" below; **CI** checks
  the GUI's reading of an unloaded job, after `launchctl bootout`).
- Installing from a downloaded, quarantined DMG: the installed helper and
  sing-box run (the install clears their quarantine flag), and Gatekeeper
  shows nothing for them.
- `ls -lde` of every directory from `/` to each installed path (**CI**
  prints it): no ACL entry grants a non-administrator anything. The helper
  judges owners and mode bits only; only root or an administrator can add
  an ACL to these root-owned directories, but a misconfiguration would go
  unnoticed by it.
- Upgrade: an app with a different sing-box or helper; `hello` reports the
  installed sing-box's hash, the GUI asks to reinstall (see "GUI" below;
  **CI** checks that the GUI reads another sing-box hash as stale), and the
  reinstall replaces both while the old helper stops its sing-box first.
- `helper-uninstall.sh` unloads the daemon and removes the helper, the
  plist, `bin` and the socket; the state directory stays and it says so
  (**CI**); `--remove-state` removes the whole support directory (**CI**).
  The two documented commands do the same without the app.

## Daemon lifecycle

- launchd starts it for the first client (**CI**), as root; it exits after
  60 s with no connection and no sing-box, with code 0, and says so in its
  log (**CI**); the next client starts it again (**CI**).
- `launchctl bootout` (and shutdown) sends SIGTERM: the helper stops its
  sing-box first, then exits 0 (**CI**: `launchctl kill SIGTERM` after each
  broken-install recovery, last exit code 0).
- A broken install exits with its `boxpilot_protocol::endpoint::exit` code,
  read with `launchctl print` (**CI** for each): a sing-box that doesn't
  match the manifest (12); `bin` writable by its group when that group is
  staff, or by others (10); a group-writable sing-box (10); a state
  directory others may read (11). The reason is in the helper's log
  (**CI**), or for a broken state directory in the system log (**CI**
  notes it, doesn't fail on it). Each, restored, works again (**CI**). Also
  by hand: a symbolic link anywhere in either path, an owner other than
  root, the helper started from another path (10), no socket from launchd
  (17), started as another user (18). The GUI reads the exit code with
  `launchctl print`, as the user, without root, and Settings › TUN says why
  (**CI** for a tampered sing-box, 12: an account without root reads it,
  and the step fails if the GUI says "turned off" instead).
- Clients waiting while a broken helper exits get the end of the stream at
  once, and launchd doesn't start it again for them in a loop.
- A malformed or missing owner record: the helper runs, and nobody may
  start, root included (**CI**); a restored one counts at once (**CI**).
- `/Library/Application Support` and `/Library/PrivilegedHelperTools` as
  they ship on each macOS version pass the chain check (**CI** for macOS 14
  on Apple silicon: the helper starts at all).

## Socket and authority

- The socket is created by launchd, never by the helper; a stale file at
  its path doesn't stop a reinstall.
- The owner may start and stop (**CI**); root may (**CI**); another account
  is read-only: `hello` says so, `start` and `stop` are refused as
  unauthorized, `status` is answered (**CI**).
- Four read-only connections held by another account don't stop the owner
  from starting; a fifth read-only connection is closed at once (**CI**).
  Eight connections are served at once and a ninth waits until one ends
  (**CI**).
- Another account can neither list the state directory nor open the
  helper's log (**CI**).
- Nothing listens on TCP for the helper (**CI**: the helper has no network
  socket at all).
- A client that stops reading is dropped at the write deadline (**CI**); a
  client vanishing mid-frame stops its sing-box (**CI**).

## sing-box under the helper

- A start the policy refuses comes back `refused`, naming the fields, as
  the GUI's own run of the policy predicts (**CI**).
- A TUN start: `utun` comes up with 172.18.0.1 and 1.1.1.1 is routed
  through it (**CI**); this machine's own connections start from the TUN
  address, DNS resolves through TUN with the profile's DNS hijacked, a
  DNS query through TUN is answered over HTTPS (sing-box verifies the
  resolver's certificate under its sandbox), the proxy reaches the
  internet by address and by name, through the `local` DNS server
  (**CI**); the loopback
  rule holds through the local proxy for `127.0.0.1`, `::1`, `localhost`
  and a name under it (**CI**).
- sing-box is the helper's child, in the helper's process group, as root,
  running the installed binary, with its working directory in a run
  directory under `state/runs` (**CI**), its environment only `HOME`,
  `TMPDIR` (in the run directory) and `PATH=/usr/bin:/bin:/usr/sbin:/sbin`,
  and no descriptor but its stdin and two output pipes (unit-tested on
  macOS by the Test step: `POSIX_SPAWN_CLOEXEC_DEFAULT`). Check the
  environment by hand with `sudo launchctl procinfo <pid>` or `ps eww`.
- sing-box listens on TCP only on loopback and the TUN interface's /30
  (**CI**).
- Stopping, closing the connection, or half a frame then closing: sing-box
  exits, `utun` and its routes go, no run directory is left (**CI**).
- Killing the helper with SIGKILL during TUN: sing-box goes too (**CI**:
  within 20 s; launchd ends the job's process group), and the next helper
  start clears the run directory and the run marker, and says it cleaned
  up after the last helper (**CI**).
- The system proxy (TUN's "System Proxy" option): the helper sets it once
  sing-box is up, on the default route's network service, to 127.0.0.1
  and the proxy port, and unsets it after the stop (**CI**; the helper's
  log names the service); with sing-box and the helper killed together,
  the next helper start resets it (**CI**). A proxy the user set, or one
  another program set on 127.0.0.1 with another port, survives a helper
  start and a crash cleanup (unit-tested; by hand with a second service or
  another proxy app). On a standard account too: the helper can set it
  where the user's own `networksetup` can't. By hand: on Wi-Fi and on
  Ethernet; with a renamed network service.
- DNS caches are flushed once sing-box is up and after every run (the
  helper's log says so).
- sing-box runs under its enforced sandbox profile (`sandboxplan`),
  through `/usr/bin/sandbox-exec`, in the PID the helper spawned (**CI**:
  the kernel's sandbox reports name sing-box, the step fails if none does
  or on any denial `KNOWN_DENIALS` doesn't explain, and it lists any
  sing-box PID the steps saw that no report names; the profile and its
  parameters are unit-tested, and the Test step runs the real sandbox-exec
  with them). The helper's log says "sing-box started under its sandbox
  profile (enforced)". Without `/usr/bin/sandbox-exec` the TUN start fails
  with its path named, and sing-box never starts (unit-tested with a
  stand-in).
- Under the profile, as root, writing outside the run directory and the
  account's state, reading a user's home, root's, another account's state
  or `/private/etc/master.passwd`, running a shell, forking and connecting
  to another local service's socket are denied; the run directory, the
  account's state, routing and utun sockets, IP sockets and mDNSResponder
  are allowed (**CI**: `examples/sandbox_probe.rs`).
- What sing-box does under the profile (**CI** prints it, summed up by
  operation and target with its denials, under "==== sing-box's sandbox
  reports", and again at the very end of the job log; the raw lines are
  `logs/sandbox-reports.raw`). By hand, on macOS 12, 13 and the current
  release, on Intel too, look for denials (`log show --predicate
  'eventMessage CONTAINS "sing-box(" AND eventMessage CONTAINS "deny("'`)
  with what CI's profiles don't use: a Tailscale endpoint, TLS outbounds
  (Trojan, VLESS with TLS, hysteria2), a remote rule set, a CA
  certificate or client key sent as an attachment, `process_name` route
  rules, a WireGuard endpoint.
- Everything in the state directory is root's and private, cache files and
  Tailscale state included (**CI**).
- A Tailscale endpoint keeps its login across connects, per account; a
  profile with a local rule set or CA certificate gets through as
  attachments (the helper's half is in CI: the smoke profile's local rule
  set travels as an attachment, and sing-box starts on it).

## GUI

The GUI's views aren't in CI. Its client code is (**CI**): `open()` and
`hello` as the owner, a TUN start through the GUI's start path
(`start_profile`: a local rule set read as the user and sent as an
attachment, the running view written) and its `stop`, answered well before
its deadline, a connection the helper ends (writing and stopping after it
fail at once, never with the `EINVAL` macOS gives `setsockopt` then), and
the helper's states as Settings › TUN reads them: ready, turned off
(`launchctl bootout`), stale (another sing-box in the app's manifest),
broken (a tampered sing-box). By hand, from BoxPilot.app, with an
administrator account and a standard account:

- **First run:** BoxPilot starts in Proxy mode. Home's TUN tab is greyed
  out and says to install the helper in Settings › TUN; the tray menu has
  no Proxy Mode submenu.
- **Settings › TUN** shows "Privileged helper" with its state, looked at
  again each time Settings is shown: not installed (Install), ready
  (Reinstall, Remove), stale, another account's, broken (its exit code's
  words), turned off in Login Items, and as root "runs as root" with no
  buttons. Run from `cargo run` (no app bundle), it says installing needs
  BoxPilot.app and shows no buttons.
- **Install:** BoxPilot's dialog first, then macOS's administrator prompt
  with BoxPilot's text ("BoxPilot wants to install its privileged helper
  for TUN mode."). The password goes only to macOS: nothing of it in
  BoxPilot's logs or files. Cancelling the prompt changes nothing and says
  so; a standard account is asked for an administrator's name and
  password. Afterwards Settings says ready, Home's TUN tab can be chosen,
  and the tray has its Proxy Mode submenu.
- **Login Items:** with BoxPilot running, turn the helper off in System
  Settings › General › Login Items. Opening Settings shows it as turned
  off, Home's TUN tab greys out, and a TUN start (TUN still chosen) asks
  to reinstall it. Turning it back on there makes it ready again. Check
  whether a reinstall while it is off there turns it on again, or fails
  until it is turned on: the dialog says to turn it on there either way.
- **Upgrade:** install the helper from one build, then open a build with
  another sing-box (or another helper). Settings says it is from another
  BoxPilot version; a TUN start asks to reinstall it; after the reinstall
  TUN starts at once.
- **Another account:** install from account A, then log into account B:
  Settings says another account owns it; B's TUN start asks to install it
  again, saying it takes it over; after that A is the one asked.
- **Remove:** a confirmation, then macOS's prompt with BoxPilot's text. With
  TUN running through the helper, BoxPilot stops it first (no "connection
  lost" message). Afterwards Settings says not installed, TUN stays chosen
  if it was, and the next TUN start asks to install it. The state
  directory stays.
- **Reinstall while TUN runs:** sing-box stops first, the helper is
  replaced, and TUN starts again by itself.
- **TUN start and stop:** Logs, Groups, Connections, Traffic and the Clash
  mode work on the helper's API; the TUN IPv6 switch, the proxy port and
  Allow LAN restart it with the new setting; a profile with a refused
  field (a tor outbound) says which field and why, and runs in Proxy mode.
- **System proxy:** in TUN mode with "System Proxy" on, the primary network
  service's web, secure web and SOCKS proxies point at 127.0.0.1 and the
  proxy port while TUN runs, on a standard account too, and are off after
  a stop. The GUI itself doesn't touch them (the helper does); in Proxy
  mode it still runs as before.
- **BoxPilot crashing while TUN runs:** kill BoxPilot (`kill -9`): its
  connection closes, the helper stops sing-box, `utun` and its routes go,
  and the system proxy the helper set is off (the helper unsets it after
  the run; if the helper itself was killed too, its next start resets
  it). The next BoxPilot starts TUN again without a prompt.
- **BoxPilot crashing in Proxy mode, then TUN:** kill BoxPilot (`kill -9`)
  while Proxy mode runs: its sing-box keeps running. Open BoxPilot again,
  choose TUN and connect: that sing-box is stopped first, and TUN starts
  (no "address already in use").
- **Cancelling a start:** click the power button (or the tray's "Cancel
  connecting") while Starting…: Disconnected at once, and nothing runs
  afterwards. A Proxy-mode start right after a Reinstall that stopped TUN
  waits until the helper's sing-box has stopped, and its system proxy
  stays on.
- **Tray, TUN chosen without the helper:** after Remove, with TUN still
  chosen, the tray's Proxy Mode submenu stays (TUN selected), and picking
  Proxy there switches; then the submenu goes, as on first run.
- **Quitting BoxPilot** while TUN runs stops sing-box, as above.
- **As root** (`sudo` BoxPilot's binary): TUN runs the bundled sing-box
  directly, as written, with no helper and no prompt.
- The helper, connected for `hello` while BoxPilot runs in Proxy mode with
  the system proxy on, leaves that proxy alone.
