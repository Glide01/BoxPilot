//! 主页,自上而下:
//! - 状态卡:圆形电源按钮 + 状态标题;其下一行在断开时说明将用哪个 profile
//!   连接,运行中分列运行时长与 sing-box 版本(不用 " · " 拼接)。
//! - 运行状态卡(仅运行中):内存 / 连接数 / 累计上传下载 + 近两分钟流量图。
//! - 快捷设置:代理模式、系统代理、Clash 模式(仅运行中且 ≥2 个模式)同在
//!   一张分组卡里,行间细线分隔。
//! - 订阅卡:当前 profile 名、更新时间、Update 按钮与用量条。

use crate::actions::{ToggleProcess, KEY_CONTEXT};
use crate::core::bytefmt::format_bytes;
use crate::core::presentation::{runtime_info, updated_label, ConnectionStatus};
use crate::i18n::s;
use crate::state::{AppState, ClashMode};
use crate::ui::card_frame;
use crate::ui::traffic_chart::{self, TrafficChart};
use crate::ui::widgets::{
    capitalize_first, empty_state, empty_state_button, full_text_tooltip, grouped_card, meta_row,
    minute_ticker, section_heading, setting_row, stat, usage_meter,
};
use gpui::{prelude::FluentBuilder, *};
use gpui_component::{
    button::Button, scroll::ScrollableElement, spinner::Spinner, switch::Switch, tab::TabBar,
    theme::Theme, tooltip::Tooltip, ActiveTheme, Disableable, Icon, IconName, Sizable, StyledExt,
    ThemeStyled,
};
use std::time::SystemTime;

/// 电源按钮直径与图标尺寸(px)。Compact enough that, connected, the hero,
/// stats, quick settings and subscription all fit a 1000×700 window
/// without scrolling; the same size in every state, so the button never
/// jumps under the pointer that just clicked it.
const POWER_BUTTON_DIAMETER: f32 = 56.;
const POWER_ICON_SIZE: f32 = 22.;
/// Letters of the profile's name the subscription card shows whole beside
/// its Update button in the narrowest window; longer ones get a tooltip.
const PROFILE_NAME_ROOM: usize = 40;

/// `color` raised `amount` in lightness (HSL), for the top of a gradient.
fn lighter(color: Hsla, amount: f32) -> Hsla {
    Hsla {
        l: (color.l + amount).min(1.),
        ..color
    }
}

pub struct HomePage {
    app_state: Entity<AppState>,
    /// The power button's: Tab reaches it, Enter / Space press it.
    power_focus: FocusHandle,
    /// The live traffic chart in the stats card, its own (cached) view so
    /// its per-mouse-move hover repaints only the chart.
    traffic_chart: Entity<TrafficChart>,
    /// Re-renders once a minute: the subscription card's "updated N min
    /// ago" and expiry countdown move with the clock, and while sing-box is
    /// stopped no status sample ticks the page.
    _ticker: Task<()>,
}

impl HomePage {
    pub fn new(app_state: Entity<AppState>, cx: &mut Context<Self>) -> Self {
        let process = app_state.read(cx).process.clone();
        let traffic = app_state.read(cx).traffic.clone();
        let clash_mode = app_state.read(cx).clash_mode.clone();
        cx.observe(&app_state, |_, _, cx| cx.notify()).detach();
        cx.observe(&process, |_, _, cx| cx.notify()).detach();
        // Status samples arrive once a second, which is also what ticks the
        // uptime label.
        cx.observe(&traffic, |_, _, cx| cx.notify()).detach();
        cx.observe(&clash_mode, |_, _, cx| cx.notify()).detach();
        let traffic_chart = cx.new(|cx| TrafficChart::new(traffic, cx));
        Self {
            app_state,
            power_focus: cx.focus_handle().tab_stop(true),
            traffic_chart,
            _ticker: minute_ticker(cx),
        }
    }
}

/// The Clash mode row of the quick settings card. `None` unless there is a
/// choice to offer (running, ≥2 modes).
fn clash_mode_row(theme: &Theme, clash_mode: Entity<ClashMode>, cx: &App) -> Option<AnyElement> {
    let state = clash_mode.read(cx);
    if !state.is_switchable() {
        return None;
    }
    let modes = state.modes.clone();
    let selected = state.current_index();
    let tab_modes = modes.clone();
    Some(
        setting_row(theme, s().home.clash_mode, None)
            .child(
                TabBar::new("clash-mode")
                    .segmented()
                    .when_some(selected, |this, ix| this.selected_index(ix))
                    .on_click(move |ix: &usize, _, cx| {
                        let Some(mode) = modes.get(*ix).cloned() else {
                            return;
                        };
                        clash_mode.update(cx, |state, cx| state.select(mode, cx));
                    })
                    .children(tab_modes),
            )
            .into_any_element(),
    )
}

impl Render for HomePage {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let state = self.app_state.read(cx);

        let t = s();
        if !state.settings.has_profiles() {
            let app_state_add = self.app_state.clone();
            let theme = cx.theme();
            return empty_state(
                theme,
                Icon::default().path("icons/power.svg"),
                t.home.no_subscription_title,
                t.home.no_subscription_hint,
            )
            .size_full()
            .child(
                div().mt_3().child(
                    empty_state_button("home-add-subscription")
                        .icon(Icon::new(IconName::Plus))
                        .label(t.home.add_subscription)
                        .on_click(move |_, window, cx| {
                            super::profiles::ProfilesPage::open_profile_dialog(
                                app_state_add.clone(),
                                None,
                                false,
                                window,
                                cx,
                            );
                        }),
                ),
            )
            .into_any_element();
        }

        let process = state.process.read(cx);

        let status = ConnectionStatus::from_flags(state.is_starting(cx), process.is_running());
        let is_updating = state.is_updating();
        let proxy_mode = state.settings.proxy_mode;
        let system_proxy = state.settings.set_system_proxy;
        let status_title = status.label();
        let connected = status == ConnectionStatus::Connected;

        let traffic = state.traffic.read(cx);
        let runtime = traffic.status;
        let info = if connected {
            runtime_info(
                traffic.started_at,
                traffic.version.as_deref(),
                SystemTime::now(),
            )
        } else {
            Default::default()
        };
        let clash_mode = state.clash_mode.clone();
        let traffic_chart = AnyView::from(self.traffic_chart.clone()).cached(
            StyleRefinement::default()
                .w_full()
                .h(px(traffic_chart::HEIGHT)),
        );

        let active = state.settings.active_profile();
        let profile_name = active.map(|p| p.name.clone()).unwrap_or_default();
        let now = SystemTime::now();
        let sub_label = capitalize_first(&updated_label(
            active.and_then(|p| p.last_updated_secs),
            now,
            t.home.not_updated_yet,
        ));
        let usage = active.and_then(|p| p.usage);

        let app_state_toggle = self.app_state.clone();
        let app_state_mode = self.app_state.clone();
        let app_state_system = self.app_state.clone();
        let app_state_update = self.app_state.clone();

        let theme = cx.theme();

        // —— 电源按钮(三态:断开 / 启动中 / 已连接) ——
        // Keyed by status: a new state is a new element, so a tooltip shown
        // while the pointer stays on the button (built once, from the old
        // status) goes away instead of still offering "Connect" after the
        // click connected.
        let power_base = div()
            .id(("power-button", status as usize))
            .flex_none()
            .size(px(POWER_BUTTON_DIAMETER))
            .rounded_full()
            .flex()
            .items_center()
            .justify_center();

        let power_button = match status {
            ConnectionStatus::Starting => power_base
                // A faint accent wash: blue-50 / blue-200 on white, a dim
                // navy on the dark background.
                .bg(theme.primary.opacity(0.08))
                .border_2()
                .border_color(theme.primary.opacity(0.35))
                .child(
                    Spinner::new()
                        .with_size(px(POWER_ICON_SIZE))
                        .color(theme.primary),
                ),
            ConnectionStatus::Connected => power_base
                .bg(linear_gradient(
                    180.,
                    // 上浅下深:顶部比 primary 亮一档(浅色下约 blue-500)。
                    linear_color_stop(lighter(theme.primary, 0.08), 0.),
                    linear_color_stop(theme.primary, 1.),
                ))
                // Hover: the same gradient in the theme's hover accent, like
                // a primary button.
                .hover(|style| {
                    style.bg(linear_gradient(
                        180.,
                        linear_color_stop(lighter(theme.primary_hover, 0.08), 0.),
                        linear_color_stop(theme.primary_hover, 1.),
                    ))
                })
                // A soft lift, not a glow: on the dark background a wide
                // accent shadow reads as a halo, so it stays faint there.
                .shadow(vec![BoxShadow {
                    color: theme
                        .primary
                        .opacity(if theme.is_dark() { 0.2 } else { 0.28 }),
                    offset: point(px(0.), px(3.)),
                    blur_radius: px(10.),
                    spread_radius: px(0.),
                    inset: false,
                }])
                .child(
                    Icon::default()
                        .path("icons/power.svg")
                        .with_size(px(POWER_ICON_SIZE))
                        .text_color(theme.primary_foreground),
                ),
            ConnectionStatus::Disconnected => power_base
                .bg(theme.background)
                .border_2()
                .border_color(theme.border)
                .shadow_sm()
                .hover(|s| s.border_color(theme.primary).text_color(theme.primary))
                .text_color(theme.muted_foreground)
                .child(
                    Icon::default()
                        .path("icons/power.svg")
                        .with_size(px(POWER_ICON_SIZE)),
                ),
        };

        // 仅启动中(含 Linux TUN gate)禁用:不挂 on_click,半透明 + 禁止光标
        // (同 gpui-component Button 的 loading 态)。拉订阅(含后台自动更新)
        // 不挡开关,与 Ctrl+S 一致。
        // A tab stop (Enter / Space press it, like a button); the ring only
        // when the focus came from the keyboard, and a click doesn't take
        // the focus at all.
        let power_focused = self.power_focus.is_focused(window) && window.last_input_was_keyboard();
        let power_button = power_button
            .track_focus(&self.power_focus)
            .on_mouse_down(MouseButton::Left, |_, window, _| window.prevent_default())
            .when(power_focused, |this| this.focus_ring_style(window, cx))
            .map(|this| {
                if status.can_toggle() {
                    this.cursor_pointer().on_click(move |_, _, cx| {
                        app_state_toggle.update(cx, |state, cx| state.toggle_process(cx));
                    })
                } else {
                    this.opacity(0.8).cursor_not_allowed()
                }
            })
            .tooltip(move |window, cx| {
                Tooltip::new(status.power_action_label())
                    .when(status.can_toggle(), |this| {
                        this.action(&ToggleProcess, Some(KEY_CONTEXT))
                    })
                    .build(window, cx)
            });

        // 状态标题下的一行:断开时说明 Connect 会用哪个 profile;运行中把
        // sing-box 版本与运行时长分开摆放(间距分隔,不用 " · ")。The
        // ticking uptime goes last, so its changing width moves nothing.
        let status_detail = match status {
            ConnectionStatus::Disconnected | ConnectionStatus::Starting
                if !profile_name.is_empty() =>
            {
                let line = if status == ConnectionStatus::Starting {
                    t.home.starting_with
                } else {
                    t.home.ready_with
                };
                Some(
                    div()
                        .text_sm()
                        .text_color(theme.muted_foreground)
                        .truncate()
                        .child(line(&profile_name))
                        .into_any_element(),
                )
            }
            ConnectionStatus::Connected if info.uptime.is_some() || info.version.is_some() => Some(
                meta_row(theme, info.version.into_iter().chain(info.uptime))
                    .gap_4()
                    .text_sm()
                    .into_any_element(),
            ),
            _ => None,
        }
        // Nothing to say yet (sing-box just started, its version and uptime
        // not in): an empty line of the same height, so the card doesn't
        // shrink and grow back as the hero changes state.
        .unwrap_or_else(|| {
            div()
                .text_sm()
                .invisible()
                .child("\u{a0}")
                .into_any_element()
        });

        let hero = card_frame(theme)
            .px_5()
            // Connected: a faint wash of the accent says so at a glance; the
            // border only leans towards the accent, so the card keeps the
            // weight of its neighbours.
            .when(connected, |card| {
                let tint = if theme.is_dark() { 0.14 } else { 0.25 };
                card.bg(theme.primary.opacity(0.05))
                    .border_color(theme.border.blend(theme.primary.opacity(tint)))
            })
            .child(
                div()
                    .h_flex()
                    .items_center()
                    .gap_4()
                    .child(power_button)
                    .child(
                        div()
                            .v_flex()
                            .flex_1()
                            .min_w_0()
                            .gap_1()
                            .child(
                                div()
                                    .text_xl()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_color(theme.foreground)
                                    .child(status_title),
                            )
                            .child(status_detail),
                    ),
            );

        // —— 运行状态卡(仅运行中):内存 / 连接数 / 累计上传 / 累计下载,
        // 下接近两分钟上下行速率图。
        let stats = connected.then(|| {
            card_frame(theme)
                .gap_4()
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .gap_4()
                        .w_full()
                        .child(stat(theme, t.home.memory, format_bytes(runtime.memory)))
                        .child(stat(
                            theme,
                            t.home.connections,
                            runtime.connections_in.to_string(),
                        ))
                        .child(stat(
                            theme,
                            t.home.uploaded,
                            format_bytes(runtime.uplink_total),
                        ))
                        .child(stat(
                            theme,
                            t.home.downloaded,
                            format_bytes(runtime.downlink_total),
                        )),
                )
                .child(traffic_chart)
        });

        // —— 快捷设置:代理模式 / 系统代理 / Clash 模式,一张分组卡 ——
        let mut quick_rows = vec![
            setting_row(theme, t.home.proxy_mode, None)
                .child(
                    TabBar::new("proxy-mode")
                        .segmented()
                        .selected_index(if proxy_mode { 1 } else { 0 })
                        .on_click(move |ix: &usize, _, cx| {
                            let value = *ix == 1;
                            app_state_mode.update(cx, |state, cx| state.set_proxy_mode(value, cx));
                        })
                        .children(vec![t.home.mode_tun, t.home.mode_proxy]),
                )
                .into_any_element(),
            setting_row(theme, t.home.system_proxy, None)
                .child(Switch::new("system-proxy").checked(system_proxy).on_click(
                    move |checked: &bool, _, cx| {
                        let value = *checked;
                        app_state_system.update(cx, |state, cx| state.set_system_proxy(value, cx));
                    },
                ))
                .into_any_element(),
        ];
        quick_rows.extend(clash_mode_row(theme, clash_mode, cx));

        // —— 订阅卡:名字 + 更新时间,Update 按钮,用量条(服务器报了才有) ——
        let subscription = card_frame(theme)
            .child(
                div()
                    .h_flex()
                    .items_center()
                    .justify_between()
                    .gap_3()
                    .w_full()
                    .child(
                        // 名字过长时截断,不把 Update 按钮挤出卡片。
                        div()
                            .v_flex()
                            .gap_0p5()
                            .flex_1()
                            .min_w_0()
                            .child(full_text_tooltip(
                                div()
                                    .text_sm()
                                    .font_weight(FontWeight::MEDIUM)
                                    .text_color(theme.foreground)
                                    .truncate(),
                                "home-profile-name",
                                profile_name,
                                PROFILE_NAME_ROOM,
                            ))
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(theme.muted_foreground)
                                    .truncate()
                                    .child(sub_label),
                            ),
                    )
                    .child(
                        Button::new("home-update")
                            .outline()
                            .small()
                            .label(t.home.update)
                            .map(|this| {
                                if is_updating {
                                    this.icon(Spinner::new())
                                } else {
                                    this.icon(Icon::default().path("icons/refresh-cw.svg"))
                                }
                            })
                            .disabled(is_updating)
                            .on_click(move |_, _, cx| {
                                app_state_update
                                    .update(cx, |state, cx| state.update_subscription(cx));
                            }),
                    ),
            )
            .children(usage.map(|usage| usage_meter(theme, "home-usage", &usage, now)));

        div()
            .v_flex()
            .size_full()
            .gap_4()
            .child(hero)
            .children(stats)
            .child(
                div()
                    .v_flex()
                    .gap_2()
                    .child(section_heading(theme, t.home.quick_settings))
                    .child(grouped_card(theme, quick_rows)),
            )
            .child(
                div()
                    .v_flex()
                    .gap_2()
                    .child(section_heading(theme, t.home.subscription))
                    .child(subscription),
            )
            // 窗口矮时整页滚动。
            .overflow_y_scrollbar()
            .into_any_element()
    }
}
