# Privileged helper: macOS verification checklist

The macOS helper (ADR 0006, phase 2, its first part) is built: the launchd
daemon, its install and uninstall scripts, its payload in the DMG, and the
AppleScript prompt as a constant. The GUI doesn't use it yet, so macOS is
still Proxy mode only (ADR 0005). It has not run on a Mac a person uses, on
an Intel Mac, on macOS 12 or 13, or under the GUI. Run this on clean Macs
(macOS 12, 13 and the current release; Apple silicon and Intel) with an
administrator account and a standard account before the first release that
lets the GUI use it. Each item names what must hold.

**CI covers part of it.** The release workflow's `macos` job builds the
DMG, checks the helper's payload in it with the helper's own manifest
parser (both architectures), and on GitHub's Apple-silicon runner
(`macos-14`, a VM) installs the helper from that DMG and drives the daemon
from outside with a smoke client (`crates/boxpilot-helper/examples/mac_smoke.rs`),
as the runner's account (the owner, an administrator with passwordless
sudo), as root and as a fresh standard account, through
`packaging/macos/helper-smoke.sh` (one step per CI step). Items or parts
marked **CI** are checked there. `sudo` stands in for the administrator
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
  the GUI must then treat it as not installed (phase 2).
- Installing from a downloaded, quarantined DMG: the installed helper and
  sing-box run (the install clears their quarantine flag), and Gatekeeper
  shows nothing for them.
- `ls -lde` of every directory from `/` to each installed path (**CI**
  prints it): no ACL entry grants a non-administrator anything. The helper
  judges owners and mode bits only; only root or an administrator can add
  an ACL to these root-owned directories, but a misconfiguration would go
  unnoticed by it.
- Upgrade: an app with a different sing-box or helper; `hello` reports the
  installed sing-box's hash, the GUI asks to reinstall (phase 2), and the
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
  (17), started as another user (18). The GUI's message for each is
  phase 2's.
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
  address, DNS resolves through TUN with the profile's DNS hijacked, the
  proxy reaches the internet by address and by name (**CI**); the loopback
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
- The system proxy (TUN's "System Proxy" option): sing-box sets it on the
  primary network service to 127.0.0.1 and the proxy port, and unsets it
  on a clean stop (**CI**); with sing-box and the helper killed together,
  the next helper start resets it (**CI**). A proxy the user set, or one
  another program set on 127.0.0.1 with another port, survives a helper
  start and a crash cleanup (unit-tested; by hand with a second service or
  another proxy app). On a standard account too: the root sing-box can set
  it where the user's own `networksetup` can't.
- DNS caches are flushed after every run (the helper's log says so).
- Everything in the state directory is root's and private, cache files and
  Tailscale state included (**CI**).
- A Tailscale endpoint keeps its login across connects, per account; a
  profile with a local rule set or CA certificate gets through as
  attachments (the helper's half is in CI: the smoke profile's local rule
  set travels as an attachment, and sing-box starts on it).

## GUI (phase 2)

The GUI isn't in CI, and doesn't use the helper yet.

- Settings › TUN shows the helper's state: not installed, installed and
  current, installed but stale (another sing-box hash or protocol
  version), broken (the exit code's message), turned off in Login Items.
- "Install helper" and "Remove helper" show one administrator prompt each,
  with BoxPilot's text; the password never reaches BoxPilot.
- TUN through the helper: Logs, Groups, Connections and Traffic work on the
  helper's API; quitting BoxPilot stops sing-box; killing BoxPilot stops
  sing-box (its connection closes); Proxy mode still runs as written,
  without the helper.
- The helper, connected for `hello` while BoxPilot runs in Proxy mode with
  the system proxy on, leaves that proxy alone.
