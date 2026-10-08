#![windows_subsystem = "windows"]

use box_pilot_gui::actions::{
    CloseConnectionDetails, FocusNext, FocusPrevious, SelectNextConnection,
    SelectPreviousConnection, ShowConnections, ShowGroups, ShowHome, ShowLogs, ShowProfiles,
    ShowSettings, ShowTools, ToggleProcess, UpdateSubscription, CONNECTION_DETAILS_CONTEXT,
    KEY_CONTEXT,
};
use box_pilot_gui::core::deeplink::LaunchAttempt;
use box_pilot_gui::state::AppState;
use box_pilot_gui::ui::assets::AppAssets;
use box_pilot_gui::ui::{app_window, locale, theme, tray};
use gpui::*;

fn main() {
    // A browser-launched deep link arrives as argv[1] in a fresh process.
    // If a primary instance is already running, hand the link over and
    // exit. An empty forward (no URI) just keeps a plain second launch from
    // spawning a duplicate sing-box manager. BoxPilot never elevates itself:
    // TUN on Windows goes through the privileged helper (ADR 0006), so the
    // primary runs at whatever privilege the user started it with.
    let deeplink_arg = std::env::args()
        .nth(1)
        .filter(|arg| box_pilot_gui::core::deeplink::is_deeplink(arg));
    if box_pilot_gui::core::single_instance::try_forward(deeplink_arg.as_deref()) {
        return;
    }

    // Launch attempts reaching this instance: our own argv link, plus every
    // one the pipe server forwards later (and, on macOS, the Apple events
    // below). A cold start with no link is not an *attempt to reach a
    // running instance*, so it sends nothing — `Plain` only ever originates
    // from the pipe or a macOS reopen.
    let (deeplink_tx, deeplink_rx) = futures_channel::mpsc::unbounded::<LaunchAttempt>();
    if let Some(uri) = deeplink_arg {
        let _ = deeplink_tx.unbounded_send(LaunchAttempt::DeepLink(uri));
    }

    // Become the primary instance: pipe server feeds later attempts into
    // the same channel. Two simultaneous cold starts race on the instance
    // mutex; the loser forwards its link to the winner and exits.
    {
        let tx = deeplink_tx.clone();
        match box_pilot_gui::core::single_instance::start_server(Box::new(move |attempt| {
            let _ = tx.unbounded_send(attempt);
        })) {
            box_pilot_gui::core::single_instance::ServerStart::Primary => {}
            box_pilot_gui::core::single_instance::ServerStart::LostRace => {
                let arg = std::env::args()
                    .nth(1)
                    .filter(|arg| box_pilot_gui::core::deeplink::is_deeplink(arg));
                let _ = box_pilot_gui::core::single_instance::try_forward(arg.as_deref());
                return;
            }
        }
    }

    // AppImage: make this image the handler for our link schemes. Primary
    // only, and on a background thread — never delays the window.
    #[cfg(target_os = "linux")]
    box_pilot_gui::core::desktop_integration::register_if_appimage();

    let app = gpui_platform::application().with_assets(AppAssets);

    // macOS starts no new process for a link click or a second launch from
    // Finder / the Dock: LaunchServices hands them to the running app as
    // Apple events — a cold start's link too, never in argv. Both become
    // launch attempts in the same channel, so ADR 0001's rule (and its
    // `view_attached()` gate) covers them unchanged. gpui calls `on_reopen`
    // only while no window is visible: with one up, AppKit brings it
    // forward itself.
    #[cfg(target_os = "macos")]
    {
        let tx = deeplink_tx.clone();
        app.on_open_urls(move |urls| {
            for attempt in LaunchAttempt::from_open_urls(urls) {
                let _ = tx.unbounded_send(attempt);
            }
        });
        let tx = deeplink_tx.clone();
        app.on_reopen(move |_| {
            let _ = tx.unbounded_send(LaunchAttempt::Plain);
        });
    }

    app.run(move |cx| {
        gpui_component::init(cx);

        // Anywhere in the main window (`RootView` keeps focus inside its
        // context). `secondary` is Cmd on macOS, Ctrl elsewhere. Ctrl+S not
        // while typing in a text field (gpui-component's `Input` context): a
        // stray save chord there shouldn't toggle sing-box.
        cx.bind_keys([
            KeyBinding::new("secondary-u", UpdateSubscription, Some(KEY_CONTEXT)),
            KeyBinding::new(
                "secondary-s",
                ToggleProcess,
                Some(&format!("{KEY_CONTEXT} && !Input")),
            ),
        ]);
        // Keyboard navigation: Tab walks the controls (out of a text field
        // too), Ctrl+1..7 (Cmd on macOS) open the pages in sidebar order.
        cx.bind_keys([
            KeyBinding::new("tab", FocusNext, Some(KEY_CONTEXT)),
            KeyBinding::new("shift-tab", FocusPrevious, Some(KEY_CONTEXT)),
            KeyBinding::new("secondary-1", ShowHome, Some(KEY_CONTEXT)),
            KeyBinding::new("secondary-2", ShowGroups, Some(KEY_CONTEXT)),
            KeyBinding::new("secondary-3", ShowConnections, Some(KEY_CONTEXT)),
            KeyBinding::new("secondary-4", ShowProfiles, Some(KEY_CONTEXT)),
            KeyBinding::new("secondary-5", ShowLogs, Some(KEY_CONTEXT)),
            KeyBinding::new("secondary-6", ShowTools, Some(KEY_CONTEXT)),
            KeyBinding::new("secondary-7", ShowSettings, Some(KEY_CONTEXT)),
        ]);
        // Connections details panel, only while it is open (the page sets
        // the context then) and never while typing in the filter box, whose
        // own Esc / arrow keys stay its own.
        let details = format!("{CONNECTION_DETAILS_CONTEXT} && !Input");
        cx.bind_keys([
            KeyBinding::new("escape", CloseConnectionDetails, Some(&details)),
            KeyBinding::new("up", SelectPreviousConnection, Some(&details)),
            KeyBinding::new("down", SelectNextConnection, Some(&details)),
        ]);

        let app_state = AppState::new(deeplink_rx, cx);
        // The saved language (System resolved from the OS locale), for
        // gpui-component's built-in strings too, before any window opens.
        let language = app_state.read(cx).settings.language;
        locale::apply(language, cx);
        // Light / dark per the saved Appearance preference (System resolved
        // against the OS; the window re-resolves against its own
        // appearance when it opens).
        let theme_pref = app_state.read(cx).settings.theme;
        theme::apply(theme_pref, None, cx);
        // The window lifecycle owns `AppState` from here on (dropped on
        // quit, which stops sing-box); the tray comes up alongside, in the
        // background on Linux.
        app_window::init(app_state.clone(), cx);
        tray::init(&app_state, cx);
        #[cfg(target_os = "macos")]
        box_pilot_gui::ui::app_menu::init(&app_state, cx);
        drop(app_state);

        cx.spawn(async move |cx| cx.update(app_window::show)).detach();
    });
}
