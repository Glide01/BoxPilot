# Every launch attempt surfaces the window

Windows launches a **new** process for every `sing-box://` link and every
double-click of the icon; the single-instance pipe forwards each one to the
running instance, which then has to decide what to do about it. We had decided
that per branch, and the branches disagreed: a plain second launch and a valid
import link both called `activate_window()`, but an *unparsable* link only
emitted a toast — into a window that is, by construction, behind the browser
the link was clicked in. Clicking a malformed link did nothing observable.

So the rule is now stated once, over the umbrella concept: **every launch
attempt ends with the user seeing the window.** `LaunchAttempt` (`Plain` /
`DeepLink`) is emitted as `ActivateRequested` at the delivery site before
anything is parsed; what the attempt carried only decides what is shown next.
The per-branch `activate_window()` calls are gone.

## Consequences

The deep-link task holds a gate: it consumes nothing until `RootView::new`
calls `AppState::view_attached()`. gpui drops events emitted before a
subscriber exists, and the window opens several executor turns after
`AppState::new` — so without the gate, an argv link (or one the pipe forwards
while the window is still opening) is parsed into events that reach nobody.
That gate is load-bearing and looks like ceremony; **deleting it silently
restores a race that only shows up in the first few hundred milliseconds of
startup.** It also replaces the old `pending_import` startup check in
`RootView::new`, which existed to paper over exactly this.

Failures are as loud as successes, including for links a hostile page can
generate at will. Accepted deliberately: the same page can already spam *valid*
links, which cost a confirmation dialog rather than a toast, so rate-limiting
failures would buy nothing.

On Linux, `xdg-open` likewise starts a new process per link, and the same flow
applies: the attempt is forwarded over a Unix socket instead of the named pipe
and ends in the same `ActivateRequested`. Wayland's focus-stealing prevention
may turn `activate_window()` into a "request attention" hint (a flashing
taskbar entry) rather than a raise, because the forwarding process has no
activation token to hand over. The invariant still stands; on Wayland,
"surfaces the window" can mean the compositor's attention hint.

## Considered options

**Move the receiver to `RootView`** — spawning the drain task where the
subscribers live kills the race structurally, no gate required. Rejected to
keep the layering: `AppState` orchestrates, views render and prompt. Worth
reopening if deep-link handling ever grows more view-side behaviour.

**Park failures in `pending_status`** — reuses the existing startup handoff,
but that field is drained exactly once by `RootView::new`, so writing to it
while the app is running swallows the message forever. Telling the two cases
apart needs a "view attached" flag — i.e. the gate, minus the tidiness.

## Update

ADR 0004 lets the window close while BoxPilot keeps running in the tray.
The invariant is unchanged ("seeing the window" can now mean reopening it),
but the `ActivateRequested` subscriber moved from `RootView` to app level
(`ui::app_window`), and a reopened window picks up a pending import itself.
