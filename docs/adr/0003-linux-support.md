# Linux is an official target, shipped as an AppImage

BoxPilot was Windows-only. It compiled on Linux, but gpui's X11/Wayland
backends were off, and every Windows-specific piece (single instance, process
stop, system proxy reset, TUN privilege, the `sing-box.exe` file name) was a
no-op or wrong there. Linux is now a release target: CI builds it next to the
MSI and publishes it with every release.

It ships as **one x86_64 AppImage** (`BoxPilot-<version>-x86_64.AppImage`),
built on Ubuntu 22.04 so that it only needs that release's glibc. The AppImage
holds `usr/bin/box-pilot` and the bundled `usr/bin/sing-box` side by side,
which is where BoxPilot already looks for sing-box (next to its own
executable). The graphics and input libraries (libxkbcommon, libxcb,
libwayland-client, the Vulkan/EGL driver) and xdg-desktop-portal come from the
host system, not the AppImage.

**The GUI never runs as root.** On Windows BoxPilot elevates itself through
UAC at startup; on Linux it always runs as the logged-in user, so its settings,
sing-box's cache file and the desktop's proxy settings all belong to that
user. Only the one thing that needs privilege, sing-box's TUN device, gets it:

- In TUN mode, unless BoxPilot already runs as root, sing-box is started from
  a **root-owned copy at `/usr/local/lib/boxpilot/sing-box`** that carries
  file capabilities (`cap_net_admin,cap_net_bind_service,cap_net_raw+ep`).
  Mixed (proxy) mode always runs the bundled sing-box directly.
- Before each TUN start, BoxPilot checks that the copy exists, reports the
  same sing-box version as the bundled one, and has `cap_net_admin` in its
  `security.capability` attribute. If any check fails, it asks once, and on
  confirmation runs `pkexec` to `install` the bundled binary there as
  root:root 0755 and `setcap` it. That shows the system password prompt. If
  the user cancels or the grant fails, BoxPilot says so and doesn't start
  sing-box.
- **The grant is per sing-box version.** An AppImage update that bundles a
  different sing-box fails the version check, so BoxPilot asks again. The
  privileged copy never drifts from the sing-box the AppImage ships.

Why a copy at all: the AppImage is a read-only squashfs mounted `nosuid`, so
the bundled binary can't be given capabilities, and the kernel would ignore
them on that mount anyway. Why a root-owned location: no process running as
the user can replace or modify the privileged binary, or the directory it sits
in, to get its own code run with those capabilities.

## Consequences

- **Accepted risk: the capabilities are not tied to BoxPilot.** Any process
  running as the user can start `/usr/local/lib/boxpilot/sing-box` with its
  own config and get `CAP_NET_ADMIN`. The file is mode 0755, so that holds
  for every local account, not only the one that granted it. That means it can
  create TUN devices, rewrite routes and firewall rules, and bind low ports. It
  does not give root, and it is the same power the user grants by choosing
  TUN mode at all. We accept it in exchange for one prompt instead of one per
  connect. Removing the file (`sudo rm -r /usr/local/lib/boxpilot`) revokes
  it.
- **Single instance** uses a lock file and a Unix socket in
  `$XDG_RUNTIME_DIR` (`boxpilot.lock`, `boxpilot.sock`, mode 0600). Without
  that variable they fall back to a per-UID name in `/tmp`. The lock holder
  removes any stale socket and binds a new one. Later launches connect and
  write one line in **the same wire format as the Windows named pipe**, then
  exit. So `LaunchAttempt` handling and ADR 0001's rule are shared with
  Windows: `xdg-open` starts a new process per link click, just like Windows
  does.
- **Deep-link handlers are registered by the AppImage itself.** When run as
  an AppImage (`$APPIMAGE` is set), BoxPilot writes
  `~/.local/share/applications/boxpilot.desktop` (pointing `Exec` at
  `$APPIMAGE`, with the `sing-box` / `boxpilot` scheme handlers) and its icon
  under `~/.local/share/icons` at every startup. It only writes them when
  they changed. It then runs `xdg-mime default` and
  `update-desktop-database`, best effort. Moving the AppImage is fixed by
  running it once from the new place. A build run outside an AppImage
  registers nothing.
- **Stopping sing-box sends SIGTERM, then SIGKILL after 3 seconds.**
  Windows keeps its existing stop. On Linux the grace period lets sing-box
  remove its `auto_route` rules and the system proxy it set. sing-box is also
  started with `PR_SET_PDEATHSIG` = SIGTERM, so it exits with BoxPilot even if
  BoxPilot crashes. The TUN device needs no cleanup on Linux; it goes away
  when sing-box closes it.
- **System proxy reset is conservative.** On GNOME (`org.gnome.system.proxy`)
  and on KDE (`kwriteconfig6`, else `kwriteconfig5`), BoxPilot clears the
  desktop proxy only when it is set to manual and points at `127.0.0.1`, the
  address sing-box's `set_system_proxy` writes. A proxy the user configured
  themselves is left alone. Other desktops are skipped.
- **Window activation on Wayland may only flash.** Compositors' focus-stealing
  prevention can turn `activate_window` into an attention request instead of
  raising the window. ADR 0001's invariant still holds, but on Wayland
  "surfaces" may mean "asks for attention". See ADR 0001.

## Considered options

**Run the whole GUI as root (pkexec at startup, like UAC on Windows).**
Rejected: a root GUI on the user's X11/Wayland session is fragile and widely
blocked. Settings and cache files would end up root-owned in the user's home,
and `set_system_proxy` would write root's dconf instead of the user's.

**A privileged helper or system service that runs sing-box as root.** This is
how sing-box's own desktop clients work. Rejected for now: it means a daemon,
an IPC protocol and an install step outside the AppImage. All of that would
buy what one `setcap` already gives. A root sing-box would also write its
cache and proxy settings as root.

**`pkexec` on every TUN connect.** No lingering capability, but a password
prompt on every connect. Rejected as too annoying for a toggle people use
daily.

**Capabilities on a copy in a user-writable directory** (e.g. under
`~/.local`). It still needs root for `setcap`, and the user's own processes
could replace the file or the directory it lives in. A root-owned path costs
nothing extra.

**deb/rpm or Flatpak instead of an AppImage.** Native packages mean one build
and repository per distribution family. Flatpak's sandbox gets in the way of
TUN, file capabilities and desktop proxy settings. One AppImage runs on every
glibc distribution new enough for it. Native packages remain possible later
from the same `usr/bin` layout.
