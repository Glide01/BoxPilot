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

/// On Windows, ensure the process is running with admin rights. If not,
/// re-launch self via `ShellExecuteW("runas", ...)` (UAC prompt) and exit.
/// Required because sing-box management touches TUN adapters, the system
/// proxy registry, and DNS — all admin-only operations. The relaunch
/// forwards argv so a deep link survives the elevation hop (browser launches
/// us non-elevated with the URI as argv[1]).
#[cfg(target_os = "windows")]
fn ensure_elevated() {
    use std::os::windows::ffi::OsStrExt;
    use windows::core::{w, PCWSTR};
    use windows::Win32::Foundation::{CloseHandle, HANDLE, HWND};
    use windows::Win32::Security::{
        GetTokenInformation, TokenElevation, TOKEN_ELEVATION, TOKEN_QUERY,
    };
    use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
    use windows::Win32::UI::Shell::ShellExecuteW;
    use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

    unsafe {
        let mut token = HANDLE::default();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token).is_ok() {
            let mut elevation = TOKEN_ELEVATION::default();
            let mut size = 0u32;
            let ok = GetTokenInformation(
                token,
                TokenElevation,
                Some(&mut elevation as *mut _ as *mut _),
                std::mem::size_of::<TOKEN_ELEVATION>() as u32,
                &mut size,
            )
            .is_ok();
            let _ = CloseHandle(token);
            if ok && elevation.TokenIsElevated != 0 {
                return;
            }
        }
    }

    let exe = match std::env::current_exe() {
        Ok(p) => p,
        Err(_) => return,
    };
    let mut exe_w: Vec<u16> = exe.as_os_str().encode_wide().collect();
    exe_w.push(0);

    // Quote-wrap each argument. Deep-link URIs contain no quotes (they're
    // percent-encoded), so plain wrapping is sufficient.
    let params = std::env::args()
        .skip(1)
        .map(|a| format!("\"{}\"", a))
        .collect::<Vec<_>>()
        .join(" ");
    let params_w: Vec<u16> = std::ffi::OsStr::new(&params)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();

    unsafe {
        ShellExecuteW(
            HWND::default(),
            w!("runas"),
            PCWSTR::from_raw(exe_w.as_ptr()),
            if params.is_empty() {
                PCWSTR::null()
            } else {
                PCWSTR::from_raw(params_w.as_ptr())
            },
            PCWSTR::null(),
            SW_SHOWNORMAL,
        );
    }
    std::process::exit(0);
}

fn main() {
    // A browser-launched deep link arrives as argv[1] in a fresh,
    // non-elevated process. If a primary instance is already running, hand
    // the link over BEFORE the elevation check — the common path then needs
    // no UAC prompt at all. An empty forward (no URI) just keeps a plain
    // second launch from spawning a duplicate sing-box manager.
    let deeplink_arg = std::env::args()
        .nth(1)
        .filter(|arg| box_pilot_gui::core::deeplink::is_deeplink(arg));
    if box_pilot_gui::core::single_instance::try_forward(deeplink_arg.as_deref()) {
        return;
    }

    #[cfg(target_os = "windows")]
    ensure_elevated();

    // Launch attempts reaching this instance: our own argv link, plus every
    // one the pipe server forwards later. A cold start with no link is not
    // an *attempt to reach a running instance*, so it sends nothing —
    // `Plain` only ever originates from the pipe.
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

    gpui_platform::application().with_assets(AppAssets).run(move |cx| {
        gpui_component::init(cx);

        // Anywhere in the main window (`RootView` keeps focus inside its
        // context). Ctrl+S not while typing in a text field (gpui-component's
        // `Input` context): a stray save chord there shouldn't toggle sing-box.
        cx.bind_keys([
            KeyBinding::new("ctrl-u", UpdateSubscription, Some(KEY_CONTEXT)),
            KeyBinding::new(
                "ctrl-s",
                ToggleProcess,
                Some(&format!("{KEY_CONTEXT} && !Input")),
            ),
        ]);
        // Keyboard navigation: Tab walks the controls (out of a text field
        // too), Ctrl+1..7 open the pages in sidebar order.
        cx.bind_keys([
            KeyBinding::new("tab", FocusNext, Some(KEY_CONTEXT)),
            KeyBinding::new("shift-tab", FocusPrevious, Some(KEY_CONTEXT)),
            KeyBinding::new("ctrl-1", ShowHome, Some(KEY_CONTEXT)),
            KeyBinding::new("ctrl-2", ShowGroups, Some(KEY_CONTEXT)),
            KeyBinding::new("ctrl-3", ShowConnections, Some(KEY_CONTEXT)),
            KeyBinding::new("ctrl-4", ShowProfiles, Some(KEY_CONTEXT)),
            KeyBinding::new("ctrl-5", ShowLogs, Some(KEY_CONTEXT)),
            KeyBinding::new("ctrl-6", ShowTools, Some(KEY_CONTEXT)),
            KeyBinding::new("ctrl-7", ShowSettings, Some(KEY_CONTEXT)),
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
        drop(app_state);

        cx.spawn(async move |cx| cx.update(app_window::show)).detach();
    });
}
