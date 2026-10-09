//! 设置页:常规、网络、TUN、故障排查(运行配置、清除缓存)、关于。订阅/profile
//! 管理在 `ProfilesPage`。
//!
//! Feature rows live in one slot file each (`language`, `appearance`,
//! `lan`, `helper`, `diagnostics`, `updates`). Every slot exposes the same
//! `rows(app_state, window, cx) -> Vec<AnyElement>`, called once per render
//! and placed into the card layout below; a card made only of slot rows is
//! omitted while its slots return nothing.

mod appearance;
mod diagnostics;
mod helper;
mod lan;
mod language;
mod updates;

use crate::core::presentation::sanitize_port;
use crate::core::settings::PROXY_PORT;
use crate::i18n::s;
use crate::state::AppState;
use crate::ui::pages::ActivePage;
use crate::ui::widgets::{
    control_input, form_column, grouped_card, page_header, page_layout, section_heading,
    setting_row, Control, ControlSize, TextLabel,
};
use gpui::{prelude::FluentBuilder, *};
use gpui_component::{
    button::Button,
    input::{InputEvent, InputState},
    scroll::ScrollableElement,
    switch::Switch,
    ActiveTheme, Disableable, StyledExt,
};

pub struct SettingsPage {
    app_state: Entity<AppState>,
    port_input: Entity<InputState>,
}

impl SettingsPage {
    pub fn new(app_state: Entity<AppState>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let process = app_state.read(cx).process.clone();
        cx.observe(&app_state, |_, _, cx| cx.notify()).detach();
        cx.observe(&process, |_, _, cx| cx.notify()).detach();

        let proxy_port = app_state.read(cx).settings.proxy_port;
        let port_input =
            Self::port_field(proxy_port, PROXY_PORT, AppState::set_proxy_port, window, cx);

        Self {
            app_state,
            port_input,
        }
    }

    /// One port field = one `InputState` + the shared commit rule: on
    /// Enter/Blur, `sanitize_port` (invalid/0 → `default`), canonicalize the
    /// field text back(set_value 不触发 Change,不会成环), then push the
    /// value into `AppState` via `apply`.
    fn port_field(
        initial: u16,
        default: u16,
        apply: fn(&mut AppState, u16, &mut Context<AppState>),
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<InputState> {
        let input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(default.to_string())
                .default_value(initial.to_string())
        });
        cx.subscribe_in(&input, window, {
            let input = input.clone();
            move |this: &mut Self, _, ev: &InputEvent, window, cx| {
                if matches!(ev, InputEvent::PressEnter { .. } | InputEvent::Blur) {
                    let raw = input.read(cx).value().trim().to_string();
                    let port = sanitize_port(&raw, default);
                    if port.to_string() != raw {
                        input.update(cx, |state, cx| {
                            state.set_value(port.to_string(), window, cx)
                        });
                    }
                    this.app_state
                        .update(cx, |state, cx| apply(state, port, cx));
                }
            }
        })
        .detach();
        input
    }
}

impl Render for SettingsPage {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Slot rows first: they need `cx` mutably, the rest of render only
        // borrows it.
        let app_state = self.app_state.clone();
        let mut general_rows = language::rows(&app_state, window, cx);
        general_rows.extend(appearance::rows(&app_state, window, cx));
        let lan_rows = lan::rows(&app_state, window, cx);
        let helper_rows = helper::rows(&app_state, window, cx);
        let diagnostics_rows = diagnostics::rows(&app_state, window, cx);
        let update_rows = updates::rows(&app_state, window, cx);

        let state = self.app_state.read(cx);
        let stopped = state.process.read(cx).is_stopped();
        let can_clear = stopped && !state.is_updating();
        let app_state_clear = self.app_state.clone();
        let app_state_ipv6 = self.app_state.clone();
        let tun_ipv6 = state.settings.tun_ipv6;
        let app_state_close_on_switch = self.app_state.clone();
        let close_on_switch = state.settings.close_connections_on_switch;
        let sing_box_version = state
            .sing_box_version
            .clone()
            .unwrap_or_else(|| s().common.unknown.to_string());
        let theme = cx.theme();
        let t = &s().settings;

        // One headed group per topic: a small heading above a card of rows
        // with hairlines between them.
        let section = |title: &'static str, rows: Vec<AnyElement>| {
            div()
                .v_flex()
                .gap_2()
                .child(section_heading(theme, title))
                .child(grouped_card(theme, rows))
        };
        let value_label = |text: SharedString| {
            div()
                .text_sm()
                .text_color(theme.muted_foreground)
                .child(text)
        };

        let mut network_rows = vec![setting_row(theme, t.local_proxy_port, None)
            .child(
                div()
                    .w(px(96.))
                    .on_mouse_down_out(|_, window, cx| window.blur(cx))
                    .child(control_input(&self.port_input, ControlSize::Regular).cleanable(false)),
            )
            .into_any_element()];
        network_rows.extend(lan_rows);
        network_rows.push(
            setting_row(theme, t.close_on_switch, Some(t.close_on_switch_hint))
                .child(
                    Switch::new("close-on-switch")
                        .checked(close_on_switch)
                        .on_click(move |checked: &bool, _, cx| {
                            let value = *checked;
                            app_state_close_on_switch.update(cx, |state, cx| {
                                state.set_close_connections_on_switch(value, cx)
                            });
                        }),
                )
                .into_any_element(),
        );

        // macOS: the privileged helper first; TUN depends on it there.
        let mut tun_rows = helper_rows;
        tun_rows.push(
            setting_row(theme, t.ipv6, Some(t.ipv6_hint))
                .child(Switch::new("tun-ipv6").checked(tun_ipv6).on_click(
                    move |checked: &bool, _, cx| {
                        let value = *checked;
                        app_state_ipv6.update(cx, |state, cx| state.set_tun_ipv6(value, cx));
                    },
                ))
                .into_any_element(),
        );

        let mut troubleshooting_rows = diagnostics_rows;
        // While connected the hint says why the button is unavailable, so it
        // doesn't read as broken.
        let clear_cache_hint = if stopped {
            t.clear_cache_hint
        } else {
            t.clear_cache_hint_connected
        };
        troubleshooting_rows.push(
            setting_row(theme, t.clear_cache, Some(clear_cache_hint))
                .child(
                    Button::new("clear-cache")
                        .outline()
                        .control(ControlSize::Regular)
                        .text_label(t.clear_cache_action)
                        .disabled(!can_clear)
                        .on_click(move |_, _, cx| {
                            app_state_clear.update(cx, |state, cx| state.clear_cache(cx));
                        }),
                )
                .into_any_element(),
        );

        let mut about_rows = vec![setting_row(theme, "BoxPilot", None)
            .child(value_label(env!("CARGO_PKG_VERSION").into()))
            .into_any_element()];
        about_rows.extend(update_rows);
        about_rows.push(
            setting_row(theme, "sing-box", None)
                .child(value_label(sing_box_version.into()))
                .into_any_element(),
        );

        let cards = div()
            .v_flex()
            .gap_6()
            .pb_2()
            .when(!general_rows.is_empty(), |cards| {
                cards.child(section(t.general, general_rows))
            })
            .child(section(t.network, network_rows))
            .child(section(t.tun, tun_rows))
            .child(section(t.troubleshooting, troubleshooting_rows))
            .child(section(t.about, about_rows));

        page_layout(
            page_header(theme, ActivePage::Settings),
            div().size_full().child(
                div()
                    .w_full()
                    .child(form_column(cards))
                    .overflow_y_scrollbar(),
            ),
        )
    }
}
