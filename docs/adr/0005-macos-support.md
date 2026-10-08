# macOS is a release target, Proxy mode only for now

BoxPilot shipped for Windows and Linux. gpui-pre has a Metal backend for
macOS, and most of the Linux work (ADR 0003) already applied to any Unix:
the data dir permissions, the `sing-box` file name, sing-box looked up next
to BoxPilot's own executable. What was missing was every place that said
`target_os = "linux"` where it meant "Unix", plus what macOS does
differently: how launches reach a running app, the menu bar, packaging.
macOS is now built, packaged and published by CI next to the MSI and the
AppImage — with one deliberate gap: **no TUN mode yet**.

It ships as **one DMG per architecture**,
`BoxPilot-<version>-macos-arm64.dmg` and `…-macos-x86_64.dmg`, for
**macOS 12 or later** (`LSMinimumSystemVersion`, and
`MACOSX_DEPLOYMENT_TARGET` in CI). Each holds `BoxPilot.app` with
`Contents/MacOS/BoxPilot` and the bundled `Contents/MacOS/sing-box` side by
side, and an `/Applications` link to drag it onto. Both are built on an
Apple-silicon runner: gpui compiles its Metal shaders with `xcrun metal`,
which needs full Xcode, so there is no cross build from Linux; x86_64 is
cross-compiled there with the same Xcode. No universal binary: two smaller
downloads, and each leg can fail on its own.

**Unsigned, in practice.** BoxPilot has no Apple Developer ID, so the app is
signed ad hoc (`codesign -s -`, sing-box first, then the bundle) and not
notarized. Gatekeeper blocks the first open of a downloaded copy; the
release notes say how to allow it (System Settings › Privacy & Security ›
"Open Anyway", or `xattr -dr com.apple.quarantine`). The ad hoc signature
still matters: Apple silicon refuses to run unsigned code at all.

## How it works

- **Launch attempts arrive as Apple events.** macOS starts no new process
  for a link click or for opening an app that is already running:
  LaunchServices sends the running app `application:openURLs:` (on a cold
  start too — the link is never in argv) and a reopen event (Dock icon,
  Finder, Spotlight). `main` turns them into `LaunchAttempt`s on the same
  channel the single-instance server feeds — one per deep link, or `Plain`
  for a reopen (and for an open-URLs event without a deep link, so it still
  surfaces the window). ADR 0001's invariant and its `view_attached()` gate
  are unchanged; only the delivery is new. gpui calls the reopen handler
  only while no window is visible; with one up, AppKit brings it forward
  itself.
- **The link schemes are registered by the bundle**, through
  `CFBundleURLTypes` in `Info.plist`; LaunchServices picks them up when the
  app is first seen. This replaces Linux's `desktop_integration`: nothing is
  written at runtime.
- **Single instance for terminal launches.** Running the binary inside the
  bundle from a terminal (or `open -n`) does start a second process. The
  Linux backend — a lock file and a Unix socket, same wire format — now
  serves every Unix. macOS has no `$XDG_RUNTIME_DIR`, so the files live in
  `$TMPDIR`, which macOS makes per-user (with the uid in their names, as in
  any shared temp dir).
- **Stopping sing-box** sends SIGTERM, then SIGKILL after 3 seconds, as on
  Linux, so sing-box can take down the system proxy it set.
- **No PDEATHSIG, so the pid file.** macOS has no way to have a child
  killed with its parent. A crashed BoxPilot leaves sing-box holding the
  local proxy port and the system proxy. (It may die on its own on its next
  log line, as Go exits on SIGPIPE when stdout is gone — but then without
  clearing the system proxy.) The Linux pid file (`<data dir>/sing-box.pid`,
  now `core::pid_file`) covers it: every start records the pid, and the
  next start stops a recorded sing-box that is still ours. "Ours" on macOS:
  its executable path (`proc_pidpath`) is the bundled sing-box; or, for an
  app that has moved since (dragged elsewhere, or run from a randomized App
  Translocation path), it is a `sing-box` whose argv (`KERN_PROCARGS2`)
  carries `-D` our data dir.
- **System proxy reset is conservative**, the ADR 0003 rule. sing-box sets
  the web, secure web and SOCKS proxy of a network service with
  `networksetup`. After a crash or a SIGKILL BoxPilot lists every service
  and turns each of those proxies off only while it is enabled and points
  at `127.0.0.1`. Best effort: a service that errors is skipped. DNS: a
  best-effort `dscacheutil -flushcache` before each start (mDNSResponder's
  own cache needs root).
- **Menu bar icon and menus.** The tray is the Windows `tray-icon` backend,
  as an `NSStatusItem` created on the main thread; a click opens the menu,
  as menu bar icons do, and "Show BoxPilot" is its first item. Its icon is
  not ADR 0004's colour / greyscale app icon but a one-colour template glyph
  (`assets/tray/`), a box in outline while disconnected and filled while
  connected, which AppKit tints for a light or dark menu bar. So the tray
  is always available, and closing the window always leaves BoxPilot
  running in it (ADR 0004). The app has a menu bar of its own: Settings… (⌘,),
  Services, Hide (⌘H), Hide Others, Show All, Quit (⌘Q); Edit, whose items
  are gpui-component's text input actions; Window (Minimize ⌘M, Close Window
  ⌘W). Quit is the same `cx.quit()` as the tray's, and AppKit routes the
  Dock's Quit and logging out through the same terminate, so all of them
  reach `on_app_quit` and ADR 0004's single cleanup path. Close Window is
  the close button's own decision. BoxPilot's shortcuts are bound with
  gpui's `secondary` modifier: Cmd on macOS, Ctrl elsewhere.
- **Proxy mode only.** `settings::TUN_AVAILABLE` is false on macOS: a saved
  TUN choice loads as Proxy, `set_proxy_mode` refuses TUN, Home greys out
  the TUN tab with a one-line hint, the tray menu has no Proxy Mode submenu
  and Settings no TUN section. Lifting it is the TUN work below.

## Consequences

- Proxy mode covers the apps that honour the system proxy; others (many
  command-line tools, some games) go direct until TUN exists.
- `networksetup` changes need an administrator account. On a standard
  account sing-box's `set_system_proxy` fails, and so does BoxPilot's reset.
- A link clicked while BoxPilot runs from a terminal (not as an app
  LaunchServices knows) can start the app a second time; that process
  forwards a plain attempt and exits, and the link is lost. Launching the
  app normally avoids it.
- The title bar is BoxPilot's own, as on Windows (`ui::title_bar`): the
  native one is transparent with the content under it, so the sidebar's
  colour runs to the top edge and the traffic lights sit on it. Only the
  traffic lights' inactive grey still follows the system's light/dark
  setting rather than BoxPilot's Appearance choice.

## TUN mode: deferred, decided

A TUN device on macOS needs root (`utun` creation, routes, DNS). There is
no file-capability trick as on Linux. Three ways were weighed:

**setuid root sing-box** (FlClash's approach: an installer marks a copy
setuid root). Rejected. Any process running as the user could start it with
its own config and get a root sing-box — and a sing-box config can do far
more than network: `log.output` writes to any path, `clash_api.external_ui`
serves any directory over HTTP. That is a local root for every process the
user runs.

**A privileged launchd helper** — the plan. A small daemon installed once,
running as root, that owns sing-box's lifecycle: BoxPilot asks it to start
and stop sing-box with a config, over a socket only the installing user can
reach. Because the helper runs sing-box as root, it must restrict what that
sing-box may do: file paths (`log.output`, cache file, rule sets) forced
into its own directory, `external_ui` and similar refused, or the process
sandboxed. **That conflicts with ADR 0002**, which keeps a config's own
controllers running exactly as written; ADR 0002 must be revisited for the
TUN path when this is built. Without a Developer ID the helper is installed
through an administrator prompt (`osascript … with administrator
privileges` copying a launchd plist and the helper); with one, through
`SMAppService`.

**Network Extension** (how the official sing-box client, SFM, does it) is
the ideal: no root process at all, the system manages the tunnel. Rejected
for now: it needs a paid Apple Developer account, a system extension in
Swift, and sing-box as a library (libbox) instead of the bundled binary.

## Considered options

**A universal (fat) binary in one DMG.** One download, but twice the size
for everyone, and one leg's failure blocks both. Two DMGs are what most
Rust GUI apps ship.

**Keep the single-instance socket out of macOS**, relying on LaunchServices
alone. It is cheap to keep, and it stops a terminal-launched second copy
from running a second sing-box against the same ports and system proxy.
