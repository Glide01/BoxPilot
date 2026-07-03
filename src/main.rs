#![windows_subsystem = "windows"]

use box_pilot_gui::actions::{ToggleProcess, UpdateSubscription};
use box_pilot_gui::state::AppState;
use box_pilot_gui::ui::RootView;
use gpui::*;
use gpui_component::{ActiveTheme, Root, Theme};
use box_pilot_gui::ui::assets::AppAssets;

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

    let (deeplink_tx, deeplink_rx) = futures_channel::mpsc::unbounded::<String>();
    if let Some(uri) = deeplink_arg {
        let _ = deeplink_tx.unbounded_send(uri);
    }

    // Become the primary instance: pipe server feeds later deep links into
    // the same channel. Two simultaneous cold starts race on the instance
    // mutex; the loser forwards its link to the winner and exits.
    {
        let tx = deeplink_tx.clone();
        match box_pilot_gui::core::single_instance::start_server(Box::new(move |uri| {
            let _ = tx.unbounded_send(uri);
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

    gpui_platform::application().with_assets(AppAssets).run(move |cx| {
        gpui_component::init(cx);
        let theme = Theme::global_mut(cx);
        // 浅色主题默认 primary 是黑色系(shadcn 风);按设计稿改为蓝色强调。
        theme.primary = rgb(0x2563EB).into(); // blue-600
        theme.primary_hover = rgb(0x1D4ED8).into(); // blue-700
        theme.primary_active = rgb(0x1E40AF).into(); // blue-800
        theme.sidebar_accent = rgb(0xEAF1FE).into();
        theme.sidebar_accent_foreground = rgb(0x1D4ED8).into();

        cx.bind_keys([
            KeyBinding::new("ctrl-u", UpdateSubscription, Some("BoxPilot")),
            KeyBinding::new("ctrl-s", ToggleProcess, Some("BoxPilot")),
        ]);

        let app_state = AppState::new(deeplink_rx, cx);
        let bounds = Bounds::centered(None, size(px(860.), px(620.)), cx);

        cx.spawn(async move |cx| {
            cx.open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    window_min_size: Some(size(px(720.), px(500.))),
                    titlebar: Some(TitlebarOptions {
                        title: Some("BoxPilot".into()),
                        ..Default::default()
                    }),
                    ..Default::default()
                },
                |window, cx| {
                    let view = cx.new(|cx| RootView::new(app_state.clone(), window, cx));
                    cx.new(|cx| Root::new(view, window, cx).bg(cx.theme().background))
                },
            )
            .expect("Failed to open window");
        })
        .detach();
    });
}
