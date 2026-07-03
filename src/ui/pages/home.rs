//! 主页:焦点式大圆连接按钮 + 设置行(代理模式 / 系统代理)+ 订阅条。

use crate::core::presentation::{updated_label, ConnectionStatus};
use crate::state::AppState;
use crate::ui::card_frame;
use crate::ui::widgets::setting_row;
use gpui::{prelude::FluentBuilder, *};
use gpui_component::{
    button::{Button, ButtonVariants}, spinner::Spinner, switch::Switch, tab::TabBar,
    ActiveTheme, Disableable,
    Icon, Sizable, StyledExt,
};
use std::time::SystemTime;

/// 电源按钮直径与图标尺寸(px)。
const POWER_BUTTON_DIAMETER: f32 = 96.;
const POWER_ICON_SIZE: f32 = 36.;

pub struct HomePage {
    app_state: Entity<AppState>,
}

impl HomePage {
    pub fn new(app_state: Entity<AppState>, cx: &mut Context<Self>) -> Self {
        let process = app_state.read(cx).process.clone();
        cx.observe(&app_state, |_, _, cx| cx.notify()).detach();
        cx.observe(&process, |_, _, cx| cx.notify()).detach();
        Self { app_state }
    }
}

impl Render for HomePage {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let state = self.app_state.read(cx);

        if !state.settings.has_profiles() {
            let app_state_add = self.app_state.clone();
            let theme = cx.theme();
            return div()
                .v_flex()
                .size_full()
                .items_center()
                .justify_center()
                .gap_3()
                .child(
                    Icon::default()
                        .path("icons/power.svg")
                        .with_size(px(48.))
                        .text_color(theme.muted_foreground),
                )
                .child(
                    div()
                        .text_lg()
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(theme.foreground)
                        .child("No subscription yet"),
                )
                .child(
                    div()
                        .text_sm()
                        .text_color(theme.muted_foreground)
                        .child("Add one to get started"),
                )
                .child(
                    Button::new("home-add-subscription")
                        .primary()
                        .label("Add subscription")
                        .on_click(move |_, window, cx| {
                            super::profiles::ProfilesPage::open_profile_dialog(
                                app_state_add.clone(),
                                None,
                                false,
                                window,
                                cx,
                            );
                        }),
                )
                .into_any_element();
        }

        let process = state.process.read(cx);

        let status =
            ConnectionStatus::from_flags(process.is_starting(), process.is_running());
        let is_updating = state.is_updating();
        let busy = status == ConnectionStatus::Starting || is_updating;
        let proxy_mode = state.settings.proxy_mode;
        let system_proxy = state.settings.set_system_proxy;
        let status_title = status.label();

        let active = state.settings.active_profile();
        let profile_name = active.map(|p| p.name.clone()).unwrap_or_default();
        let sub_label = updated_label(
            active.and_then(|p| p.last_updated_secs),
            SystemTime::now(),
            "not updated yet",
        );

        let app_state_toggle = self.app_state.clone();
        let app_state_mode = self.app_state.clone();
        let app_state_system = self.app_state.clone();
        let app_state_update = self.app_state.clone();

        let theme = cx.theme();

        // —— 电源按钮(三态:断开 / 启动中 / 已连接) ——
        let power_base = div()
            .id("power-button")
            .size(px(POWER_BUTTON_DIAMETER))
            .rounded_full()
            .flex()
            .items_center()
            .justify_center();

        let power_button = match status {
            ConnectionStatus::Starting => power_base
                .bg(rgb(0xEFF6FF)) // blue-50
                .border_2()
                .border_color(rgb(0xBFDBFE)) // blue-200
                .child(
                    Spinner::new()
                        .with_size(px(POWER_ICON_SIZE))
                        .color(theme.primary),
                ),
            ConnectionStatus::Connected => power_base
                .bg(linear_gradient(
                    180.,
                    linear_color_stop(rgb(0x3B82F6), 0.), // blue-500,上浅下深
                    linear_color_stop(theme.primary, 1.),
                ))
                .shadow(vec![BoxShadow {
                    color: theme.primary.opacity(0.4),
                    offset: point(px(0.), px(8.)),
                    blur_radius: px(24.),
                    spread_radius: px(0.),
                }])
                .child(
                    Icon::default()
                        .path("icons/power.svg")
                        .with_size(px(POWER_ICON_SIZE))
                        .text_color(gpui::white()),
                ),
            ConnectionStatus::Disconnected => power_base
                .bg(theme.background)
                .border_2()
                .border_color(theme.border)
                .shadow_sm()
                .hover(|s| s.border_color(theme.muted_foreground))
                .child(
                    Icon::default()
                        .path("icons/power.svg")
                        .with_size(px(POWER_ICON_SIZE))
                        .text_color(theme.muted_foreground),
                ),
        };

        // busy(启动中 / 更新订阅)时不挂 on_click = 禁用。
        let power_button = power_button.when(!busy, |this| {
            this.cursor_pointer().on_click(move |_, _, cx| {
                app_state_toggle.update(cx, |state, cx| state.toggle_process(cx));
            })
        });

        div()
            .v_flex()
            .size_full()
            .gap_4()
            // —— Hero 区:按钮 + 状态标题 ——
            // 下方留白封顶(max_h_24),多余空间归到上方,避免标题与
            // 设置行之间出现大块死空间。
            .child(
                div()
                    .flex_1()
                    .v_flex()
                    .items_center()
                    .gap_4()
                    .child(div().flex_1())
                    .child(power_button)
                    .child(
                        // 状态标题(网速行已移至侧边栏底部连接状态上方)。
                        div()
                            .text_xl()
                            .font_weight(FontWeight::BOLD)
                            .text_color(theme.foreground)
                            .child(status_title),
                    )
                    .child(div().flex_1().max_h_24()),
            )
            // —— 设置行:代理模式 + 系统代理(两张等宽小卡) ——
            // 注意:不要用 h_flex()(自带 items_center,卡片不等高时不拉伸)。
            .child(
                div()
                    .flex()
                    .flex_row()
                    .gap_4()
                    .w_full()
                    .child(
                        // justify_center:两卡被拉到等高时,行内容垂直居中
                        // (右卡比左卡矮一截,否则内容贴顶)。
                        card_frame(theme).flex_1().justify_center().child(
                            setting_row(theme, "Proxy Mode", None).child(
                                TabBar::new("proxy-mode")
                                    .segmented()
                                    .selected_index(if proxy_mode { 1 } else { 0 })
                                    .on_click(move |ix: &usize, _, cx| {
                                        let value = *ix == 1;
                                        app_state_mode.update(cx, |state, cx| {
                                            state.set_proxy_mode(value, cx)
                                        });
                                    })
                                    .children(vec!["TUN", "Proxy"]),
                            ),
                        ),
                    )
                    .child(
                        card_frame(theme).flex_1().justify_center().child(
                            setting_row(theme, "System Proxy", None).child(
                                Switch::new("system-proxy")
                                    .checked(system_proxy)
                                    .on_click(move |checked: &bool, _, cx| {
                                        let value = *checked;
                                        app_state_system.update(cx, |state, cx| {
                                            state.set_system_proxy(value, cx)
                                        });
                                    }),
                            ),
                        ),
                    ),
            )
            // —— 订阅条(逻辑与改版前一致) ——
            .child(
                card_frame(theme).child(
                    div()
                        .h_flex()
                        .items_center()
                        .justify_between()
                        .w_full()
                        .child(
                            div()
                                .h_flex()
                                .items_center()
                                .gap_2()
                                .child(
                                    div()
                                        .text_sm()
                                        .text_color(theme.foreground)
                                        .child(profile_name),
                                )
                                .child(
                                    div()
                                        .text_xs()
                                        .text_color(theme.muted_foreground)
                                        .child(format!("· {}", sub_label)),
                                ),
                        )
                        .child(
                            Button::new("home-update")
                                .outline()
                                .small()
                                .label("Update")
                                .when(is_updating, |this| this.icon(Spinner::new()))
                                .disabled(is_updating)
                                .on_click(move |_, _, cx| {
                                    app_state_update
                                        .update(cx, |state, cx| state.update_subscription(cx));
                                }),
                        ),
                ),
            )
            .into_any_element()
    }
}
