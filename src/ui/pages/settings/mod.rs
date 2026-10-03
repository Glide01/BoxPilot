//! 设置页:Shell 环境复制、清除缓存。订阅/profile 管理在 `ProfilesPage`。
//!
//! Feature rows live in one slot file each (`language`, `appearance`,
//! `window`, `lan`, `diagnostics`, `updates`). Every slot exposes the same
//! `rows(app_state, window, cx) -> Vec<AnyElement>`, called once per render
//! and placed into the card layout below; a card made only of slot rows is
//! omitted while its slots return nothing.

mod appearance;
mod diagnostics;
mod lan;
mod language;
mod updates;
mod window;

use crate::core::presentation::sanitize_port;
#[cfg(not(target_os = "windows"))]
use crate::core::settings::fish_proxy_command;
#[cfg(target_os = "windows")]
use crate::core::settings::powershell_proxy_command;
use crate::core::settings::{posix_proxy_command, StatusLevel, PROXY_PORT};
use crate::state::AppState;
use crate::ui::widgets::{page_header, setting_row};
use crate::ui::{card_frame, toast};
use gpui::{prelude::FluentBuilder, *};
use gpui_component::{
    button::Button,
    input::{Input, InputEvent, InputState},
    scroll::ScrollableElement,
    switch::Switch,
    ActiveTheme, Disableable, Sizable, StyledExt,
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
        let port_input = Self::port_field(proxy_port, PROXY_PORT, AppState::set_proxy_port, window, cx);

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
        general_rows.extend(window::rows(&app_state, window, cx));
        let lan_rows = lan::rows(&app_state, window, cx);
        let diagnostics_rows = diagnostics::rows(&app_state, window, cx);
        let update_rows = updates::rows(&app_state, window, cx);

        let state = self.app_state.read(cx);
        let can_clear = state.process.read(cx).is_stopped() && !state.is_updating();
        let app_state_clear = self.app_state.clone();
        let app_state_ipv6 = self.app_state.clone();
        let proxy_port = state.settings.proxy_port;
        let tun_ipv6 = state.settings.tun_ipv6;
        let sing_box_version = state
            .sing_box_version
            .clone()
            .unwrap_or_else(|| "Unknown".to_string());
        let theme = cx.theme();

        let section_label = |text: &'static str| {
            div()
                .text_xs()
                .text_color(theme.muted_foreground)
                .child(text)
        };

        let copy_btn = |id: &'static str,
                        label: &'static str,
                        cmd: String,
                        toast_msg: &'static str| {
            let tooltip = cmd.clone();
            Button::new(id)
                .outline()
                .small()
                .w(px(144.))
                .label(label)
                .tooltip(tooltip)
                .on_click(move |_, _, cx| {
                    cx.write_to_clipboard(ClipboardItem::new_string(cmd.clone()));
                    toast::show(StatusLevel::Success, toast_msg, cx);
                })
        };

        let shell_buttons = div().h_flex().gap_2().w_full();
        #[cfg(target_os = "windows")]
        let shell_buttons = shell_buttons
            .child(copy_btn(
                "ps-env",
                "PowerShell",
                powershell_proxy_command(proxy_port),
                "Copied PowerShell proxy command.",
            ))
            .child(copy_btn(
                "wsl-env",
                "WSL",
                posix_proxy_command(proxy_port),
                "Copied WSL proxy command.",
            ));
        #[cfg(not(target_os = "windows"))]
        let shell_buttons = shell_buttons
            .child(copy_btn(
                "posix-env",
                "bash/zsh",
                posix_proxy_command(proxy_port),
                "Copied bash/zsh proxy command.",
            ))
            .child(copy_btn(
                "fish-env",
                "fish",
                fish_proxy_command(proxy_port),
                "Copied fish proxy command.",
            ));

        let cards = div()
            .v_flex()
            .gap_4()
            .when(!general_rows.is_empty(), |cards| {
                cards.child(
                    card_frame(theme)
                        .child(section_label("GENERAL"))
                        .children(general_rows),
                )
            })
            .child(
                card_frame(theme)
                    .child(section_label("NETWORK"))
                    .child(
                        setting_row(theme, "Local proxy port", None).child(
                            div()
                                .w(px(96.))
                                .on_mouse_down_out(|_, window, cx| window.blur(cx))
                                .child(Input::new(&self.port_input).cleanable(false)),
                        ),
                    )
                    .children(lan_rows),
            )
            .child(
                card_frame(theme).child(section_label("TUN")).child(
                    setting_row(theme, "IPv6", Some("Proxies IPv6 traffic in TUN mode.")).child(
                        Switch::new("tun-ipv6")
                            .checked(tun_ipv6)
                            .on_click(move |checked: &bool, _, cx| {
                                let value = *checked;
                                app_state_ipv6
                                    .update(cx, |state, cx| state.set_tun_ipv6(value, cx));
                            }),
                    ),
                ),
            )
            .child(
                card_frame(theme)
                    .child(section_label("SHELL ENVIRONMENT"))
                    .child(shell_buttons),
            )
            .child(
                card_frame(theme).child(
                    setting_row(
                        theme,
                        "Clear Cache",
                        Some("Resets cache.db — node selections go back to defaults. Available while disconnected."),
                    )
                    .child(
                        Button::new("clear-cache")
                            .outline()
                            .small()
                            .label("Clear Cache")
                            .disabled(!can_clear)
                            .on_click(move |_, _, cx| {
                                app_state_clear
                                    .update(cx, |state, cx| state.clear_cache(cx));
                            }),
                    ),
                ),
            )
            .when(!diagnostics_rows.is_empty(), |cards| {
                cards.child(
                    card_frame(theme)
                        .child(section_label("TROUBLESHOOTING"))
                        .children(diagnostics_rows),
                )
            })
            .child(
                card_frame(theme)
                    .child(section_label("ABOUT"))
                    .child(
                        setting_row(theme, "BoxPilot", None).child(
                            div()
                                .text_sm()
                                .text_color(theme.muted_foreground)
                                .child(env!("CARGO_PKG_VERSION")),
                        ),
                    )
                    .children(update_rows)
                    .child(
                        setting_row(theme, "sing-box", None).child(
                            div()
                                .text_sm()
                                .text_color(theme.muted_foreground)
                                .child(sing_box_version),
                        ),
                    ),
            );

        div()
            .v_flex()
            .size_full()
            .gap_4()
            .child(page_header(theme, "Settings"))
            .child(div().flex_1().min_h_0().child(cards.overflow_y_scrollbar()))
    }
}
