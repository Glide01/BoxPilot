# The window can close while BoxPilot keeps running in the tray

BoxPilot had exactly one lifetime: the window's. `AppState` was owned by the
`RootView` inside it, gpui quit when the last window closed, and dropping
`AppState` stopped sing-box and reset the system proxy. A proxy manager is
mostly left alone once connected, though, and a window that must stay open
(or minimized in the taskbar) to keep sing-box up is in the way.

BoxPilot now has a **system tray icon** — colour while connected, greyscale
otherwise, tooltip "BoxPilot — Connected/Disconnected/Starting…" — whose menu
covers what people switch most: Show BoxPilot, Connect/Disconnect, System
Proxy, Proxy Mode, Clash Mode (while switchable), Profile (with two or more)
and Quit BoxPilot. Left click opens the window. And **closing the window can
leave BoxPilot running**: Settings › General "Close button" is Ask (default) /
Minimize to tray / Quit, and the first close under Ask asks "Keep BoxPilot
running in the tray?" with a "Don't ask again" box.

## How it works

- **Lifetime.** `ui::app_window::MainWindow` (a gpui global) owns `AppState`
  for the app's lifetime; gpui runs with `QuitMode::Explicit`. Quitting is
  decided in one place: the window closed without "keep running" → quit;
  tray Quit → quit; the Ask dialog's Quit → quit. `on_app_quit` removes the
  global, which drops `AppState` → `ProcessSession`, whose `Drop` stops
  sing-box and resets the system proxy — the same cleanup as before, just
  reached from the global instead of the window. gpui never drops globals
  itself, so that removal is load-bearing.
- **"Hide" means close.** gpui has no per-window hide, and a Wayland client
  can't hide its toplevel at all. Minimize-to-tray closes the window for real
  (its views are dropped, which also stops them rendering); "Show BoxPilot"
  opens a fresh one at the last bounds. Page state such as the selected page
  starts over; everything that matters lives in `AppState` and survives.
- **No tray, no change.** The close button only keeps BoxPilot running while
  a tray icon is actually showing (`tray::is_available`). Without one — a
  Linux desktop with no StatusNotifier host (e.g. GNOME without the
  AppIndicator extension), a headless/xvfb session, a failed registration —
  closing quits exactly as before, and the Settings row is disabled with "No
  system tray on this desktop — closing quits BoxPilot". If the host goes
  away while the window is closed, the window reopens.
- **Backends.** Windows: `tray-icon` + `muda`, created on the UI thread,
  whose hidden window gpui's own `GetMessageW` loop pumps. Linux: a
  StatusNotifierItem over D-Bus via `ksni` (no GTK), registered on a thread
  of its own so a slow or absent watcher never delays the window. Both only
  render a platform-neutral `TraySnapshot` / `menu_entries` and forward
  clicks as `TrayCommand`s into a channel; the UI-thread task draining it is
  the only place a click turns into an `AppState` call. Neither backend ever
  touches gpui from its callbacks.

## Departure from ADR 0001

ADR 0001's invariant stands: **every launch attempt ends with the user seeing
the window** — now including when the window is closed to the tray, where
"seeing" means it is reopened. What moved is the handler: `ActivateRequested`
is subscribed at app level (`app_window::init`) instead of in `RootView`,
because with the window closed there is no `RootView` to hear it. The
`view_attached()` gate is unchanged; it still holds attempts until the first
`RootView` has wired its subscribers.

Reopening adds one wrinkle the gate can't cover. A launch attempt that
reopens the window emits `ActivateRequested` and, in the same update, either
`ImportRequested` or an "Ignored import link" status. The new `RootView` is
built while handling the first event, and gpui activates its subscriptions
only after the events already queued — so it would miss the second. Two
small handoffs close that gap:

- `RootView::new` checks `pending_import` after `view_attached()` and prompts
  on the next frame (this brings back a startup check ADR 0001 retired, for a
  different reason: the gate covers startup, this covers reopen).
  `prompt_import` takes the request, so a duplicate prompt can't happen.
- `app_window` listens for status events from `AppState`, `ProcessSession`
  and `ClashMode` while the window's own routing isn't wired yet, and toasts
  them directly — "failures are as loud as successes" holds for a link that
  reopened the window. With no window at all (e.g. Connect from the tray
  menu without a profile), the last warning/error is kept and shown when the
  window next opens.

## Consequences

- A Linux TUN-mode Connect from the tray that needs the one-time grant
  reopens the window to ask for it (the prompt lives there).
- Quitting is still the only thing that stops sing-box behind the user's
  back; closing the window under "Minimize to tray" deliberately doesn't.
  The dialog says so ("Quit stops sing-box").
- Windows runs elevated; tray clicks come from the (non-elevated) shell via
  tray-icon's window messages. Needs checking on a real desktop, as do KDE
  and GNOME+AppIndicator.

## Considered options

**Keep the window and hide it** — not possible with gpui-pre 0.3.7 (no
per-window visibility), and impossible on Wayland regardless.

**Minimize instead of close** — keeps every view rendering and the taskbar
entry around; it's what the user already had without a tray.

**GTK-based tray on Linux** (`tray-icon`'s libappindicator backend) — drags
GTK and its main loop into a GPUI app for an icon; StatusNotifierItem over
D-Bus is what KDE, GNOME's AppIndicator extension and most panels speak.
