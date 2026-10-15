//! Settings › Network "Proxy Mode" (TUN or the local proxy alone) and
//! "System Proxy". Not the sing-box API's to switch: BoxPilot writes them
//! into the config it starts sing-box with (`RuntimeOptions`), so a change
//! while sing-box runs restarts it. Home, which shows only what the sing-box
//! API reports and controls, left them for here; the tray has them too.

use super::SettingsPage;
use crate::i18n::s;
use crate::state::AppState;
use crate::ui::widgets::{segmented, setting_row, ControlSize, Segment};
use gpui::{AnyElement, Context, Entity, IntoElement, ParentElement, Window};
use gpui_component::{switch::Switch, ActiveTheme};

/// This slot's rows, in display order.
pub(super) fn rows(
    app_state: &Entity<AppState>,
    _window: &mut Window,
    cx: &mut Context<SettingsPage>,
) -> Vec<AnyElement> {
    let state = app_state.read(cx);
    let proxy_mode = state.settings.proxy_mode;
    let tun_available = state.tun_available();
    let system_proxy = state.settings.set_system_proxy;
    let theme = cx.theme();
    let t = &s().home;

    let app_state_mode = app_state.clone();
    let app_state_system = app_state.clone();
    vec![
        // Without TUN here (macOS without its privileged helper) its tab
        // stays, greyed out, with how to get it under the label. A saved
        // TUN choice stays selected (`AppSettings::proxy_mode`): Proxy can
        // be picked, and a start asks to install the helper.
        setting_row(
            theme,
            t.proxy_mode,
            (!tun_available).then_some(t.tun_needs_helper),
        )
        .child(segmented(
            theme,
            "proxy-mode",
            ControlSize::Regular,
            vec![
                Segment::new(t.mode_tun).disabled(!tun_available),
                Segment::new(t.mode_proxy),
            ],
            Some(if proxy_mode { 1 } else { 0 }),
            move |ix, _, cx| {
                let value = ix == 1;
                app_state_mode.update(cx, |state, cx| state.set_proxy_mode(value, cx));
            },
        ))
        .into_any_element(),
        setting_row(theme, t.system_proxy, None)
            .child(Switch::new("system-proxy").checked(system_proxy).on_click(
                move |checked: &bool, _, cx| {
                    let value = *checked;
                    app_state_system.update(cx, |state, cx| state.set_system_proxy(value, cx));
                },
            ))
            .into_any_element(),
    ]
}
