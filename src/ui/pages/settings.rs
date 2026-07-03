//! 设置页:Shell 环境复制、清除缓存。订阅/profile 管理在 `ProfilesPage`。

use crate::core::presentation::sanitize_port;
use crate::core::settings::{
    powershell_proxy_command, wsl_proxy_command, StatusLevel, CLASH_API_PORT, PROXY_PORT,
};
use crate::state::AppState;
use crate::ui::widgets::{page_header, setting_row};
use crate::ui::{card_frame, toast};
use gpui::*;
use gpui_component::{
    button::Button,
    input::{Input, InputEvent, InputState},
    ActiveTheme, Disableable, Sizable, StyledExt,
};

pub struct SettingsPage {
    app_state: Entity<AppState>,
    port_input: Entity<InputState>,
    clash_api_port_input: Entity<InputState>,
}

impl SettingsPage {
    pub fn new(app_state: Entity<AppState>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let process = app_state.read(cx).process.clone();
        cx.observe(&app_state, |_, _, cx| cx.notify()).detach();
        cx.observe(&process, |_, _, cx| cx.notify()).detach();

        let proxy_port = app_state.read(cx).settings.proxy_port;
        let port_input = Self::port_field(proxy_port, PROXY_PORT, AppState::set_proxy_port, window, cx);
        let clash_port = app_state.read(cx).settings.clash_api_port;
        let clash_api_port_input =
            Self::port_field(clash_port, CLASH_API_PORT, AppState::set_clash_api_port, window, cx);

        Self {
            app_state,
            port_input,
            clash_api_port_input,
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
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let state = self.app_state.read(cx);
        let can_clear = state.process.read(cx).is_stopped() && !state.is_updating();
        let app_state_clear = self.app_state.clone();
        let proxy_port = state.settings.proxy_port;
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

        div()
            .v_flex()
            .size_full()
            .gap_4()
            .child(page_header(theme, "Settings"))
            .child(
                card_frame(theme)
                    .child(section_label("NETWORK"))
                    .child(
                        setting_row(theme, "Local proxy port", None).child(
                            div()
                                .w(px(96.))
                                .on_mouse_down_out(|_, window, _| window.blur())
                                .child(Input::new(&self.port_input).cleanable(false)),
                        ),
                    )
                    .child(
                        setting_row(theme, "Clash API port", None).child(
                            div()
                                .w(px(96.))
                                .on_mouse_down_out(|_, window, _| window.blur())
                                .child(Input::new(&self.clash_api_port_input).cleanable(false)),
                        ),
                    ),
            )
            .child(
                card_frame(theme)
                    .child(section_label("SHELL ENVIRONMENT"))
                    .child(
                        div()
                            .h_flex()
                            .gap_2()
                            .w_full()
                            .child(copy_btn(
                                "ps-env",
                                "PowerShell",
                                powershell_proxy_command(proxy_port),
                                "Copied PowerShell proxy command.",
                            ))
                            .child(copy_btn(
                                "wsl-env",
                                "WSL",
                                wsl_proxy_command(proxy_port),
                                "Copied WSL proxy command.",
                            )),
                    ),
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
    }
}
