//! Settings › Network "Allow LAN connections": the mixed inbound listens on
//! `0.0.0.0` instead of loopback (`RuntimeOptions::allow_lan`). The hint
//! names where other devices reach the proxy, from `core::lan`'s cached
//! lookup — refreshed whenever the page renders (it observes `AppState`),
//! never polled.

use super::SettingsPage;
use crate::core::lan::{cached_lan_addresses, endpoint_list};
use crate::i18n::s;
use crate::state::AppState;
use gpui::{
    div, AnyElement, Context, Entity, IntoElement, ParentElement, SharedString, Styled, Window,
};
use gpui_component::{switch::Switch, ActiveTheme, StyledExt};

/// This slot's rows, in display order; empty = nothing to show.
pub(super) fn rows(
    app_state: &Entity<AppState>,
    _window: &mut Window,
    cx: &mut Context<SettingsPage>,
) -> Vec<AnyElement> {
    let settings = &app_state.read(cx).settings;
    let allowed = settings.allow_lan;
    let port = settings.proxy_port;
    let hint = hint(allowed, endpoint_list(&cached_lan_addresses(), port), port);

    let theme = cx.theme();
    // Off: a muted description. On: full contrast, since the proxy is
    // now open to the network without a password.
    let hint_color = if allowed {
        theme.foreground
    } else {
        theme.muted_foreground
    };

    let app_state = app_state.clone();
    let switch =
        Switch::new("allow-lan")
            .checked(allowed)
            .on_click(move |checked: &bool, _, cx| {
                let value = *checked;
                app_state.update(cx, |state, cx| state.set_allow_lan(value, cx));
            });

    // `widgets::setting_row`'s layout, with an owned description.
    let row = div()
        .h_flex()
        .items_center()
        .justify_between()
        .gap_4()
        .w_full()
        .child(
            div()
                .v_flex()
                .flex_1()
                .min_w_0()
                .gap_1()
                .child(
                    div()
                        .text_sm()
                        .text_color(theme.foreground)
                        .child(s().settings.allow_lan),
                )
                .child(div().text_xs().text_color(hint_color).child(hint)),
        )
        .child(switch);

    vec![row.into_any_element()]
}

/// The hint under the switch. `endpoints` is `core::lan::endpoint_list`.
fn hint(allowed: bool, endpoints: Option<String>, port: u16) -> SharedString {
    let t = &s().settings;
    let text = match (allowed, endpoints) {
        (true, Some(at)) => (t.lan_on_at)(&at),
        (true, None) => (t.lan_on_port)(port),
        (false, Some(at)) => (t.lan_off_at)(&at),
        (false, None) => t.lan_off.to_string(),
    };
    text.into()
}
