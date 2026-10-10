//! 主页,自上而下:
//! - 状态卡:圆形电源按钮 + 状态标题;其下一行在断开时说明将用哪个 profile
//!   连接,运行中分列运行时长与 sing-box 版本(不用 " · " 拼接)。
//! - 运行状态卡(仅运行中):内存 / 连接数 / 累计上传下载 + 近两分钟流量图。
//! - 快捷设置:代理模式、系统代理、Clash 模式(仅运行中且 ≥2 个模式)同在
//!   一张分组卡里,行间细线分隔。
//! - 配置卡:与 Profiles 页的行同一结构 —— 首行左边当前 profile 名(点开是
//!   切换 profile 的菜单),右边更新按钮(⟳ + 多久前更新);次行来源(订阅
//!   域名 / 本地文件);末行用量条(订阅服务器报了才有)。

use crate::actions::{ShowProfiles, ToggleProcess, UpdateSubscription, KEY_CONTEXT};
use crate::core::bytefmt::format_bytes;
use crate::core::presentation::{
    profile_freshness, profile_row_info, runtime_info, ConnectionStatus,
};
use crate::i18n::s;
use crate::state::{AppState, ClashMode};
use crate::ui::card_frame;
use crate::ui::pages::ActivePage;
use crate::ui::traffic_chart::{self, TrafficChart};
use crate::ui::widgets::{
    api_stalled_notice, empty_state, empty_state_button, freshness_button, grouped_card,
    may_truncate, meta_row, minute_ticker, page_header, page_layout, power_button,
    profile_source_line, scroll_page, section_heading, segmented, setting_row, shorten, stat,
    usage_meter, Control, ControlSize, IconLabel, Segment, TextLabel, CONTROL_LINE_HEIGHT,
};
use gpui::{prelude::FluentBuilder, *};
use gpui_component::{
    button::{Button, ButtonVariants},
    menu::{DropdownMenu, PopupMenuItem},
    switch::Switch,
    theme::Theme,
    tooltip::Tooltip,
    ActiveTheme, Icon, IconName, StyledExt, ThemeStyled,
};
use std::time::SystemTime;

/// 电源按钮直径与图标尺寸(px)。Compact enough that, connected, the hero,
/// stats, quick settings and subscription all fit a 1000×700 window
/// without scrolling; the same size in every state, so the button never
/// jumps under the pointer that just clicked it.
const POWER_BUTTON_DIAMETER: f32 = 56.;
const POWER_ICON_SIZE: f32 = 22.;
/// Letters of the profile's name the profile card shows whole beside its
/// update button in the narrowest window; longer ones get a tooltip.
const PROFILE_NAME_ROOM: usize = 40;
/// Widest the profile switcher's menu grows, and the letters of a name
/// that fit in it (longer ones are shortened: menu items clip).
const PROFILE_MENU_MAX_WIDTH: f32 = 360.;
const PROFILE_MENU_NAME_ROOM: usize = 40;
/// Tallest the profile switcher's menu grows before it scrolls.
const PROFILE_MENU_MAX_HEIGHT: f32 = 320.;

pub struct HomePage {
    app_state: Entity<AppState>,
    /// The power button's: Tab reaches it, Enter / Space press it.
    power_focus: FocusHandle,
    /// The live traffic chart in the stats card, its own (cached) view so
    /// its per-mouse-move hover repaints only the chart.
    traffic_chart: Entity<TrafficChart>,
    /// Re-renders once a minute: the profile card's "25 min ago" and
    /// expiry countdown move with the clock, and while sing-box is
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
    let segments = modes.iter().cloned().map(Segment::new).collect();
    Some(
        setting_row(theme, s().home.clash_mode, None)
            .child(segmented(
                theme,
                "clash-mode",
                ControlSize::Regular,
                segments,
                selected,
                move |ix, _, cx| {
                    let Some(mode) = modes.get(ix).cloned() else {
                        return;
                    };
                    clash_mode.update(cx, |state, cx| state.select(mode, cx));
                },
            ))
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
            let empty = empty_state(
                theme,
                Icon::default().path("icons/power.svg"),
                t.home.no_subscription_title,
                t.home.no_subscription_hint,
            )
            .action(
                empty_state_button("home-add-subscription")
                    .icon_label(IconName::Plus, t.home.add_subscription)
                    .on_click(move |_, window, cx| {
                        super::profiles::ProfilesPage::open_profile_dialog(
                            app_state_add.clone(),
                            None,
                            false,
                            window,
                            cx,
                        );
                    }),
            );
            return page_layout(
                page_header(theme, ActivePage::Home),
                div().size_full().child(empty),
            )
            .into_any_element();
        }

        let status = state.connection_status(cx);
        let proxy_mode = state.settings.proxy_mode;
        let tun_available = state.tun_available();
        let system_proxy = state.settings.set_system_proxy;
        let status_title = status.label();
        let connected = status == ConnectionStatus::Connected;

        let traffic = state.traffic.read(cx);
        let runtime = traffic.status;
        // The API stopped answering mid-run: the figures below are the last
        // ones it gave.
        let stalled = connected && traffic.stalled;
        // Before the first sample the figures are unknown, not zero.
        let has_sample = traffic.has_sample();
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
        let freshness = active.and_then(|p| {
            profile_freshness(
                p,
                state.updating_profile_id() == Some(p.id.as_str()),
                state.fetch_error(&p.id),
                now,
            )
        });
        let usage = active.and_then(|p| p.usage);
        let source_info = active.map(|p| profile_row_info(&p.source));
        let profiles: Vec<(String, String)> = state
            .settings
            .profiles
            .iter()
            .map(|p| (p.id.clone(), p.name.clone()))
            .collect();
        let active_id = state.settings.active_profile_id.clone();

        let app_state_toggle = self.app_state.clone();
        let app_state_mode = self.app_state.clone();
        let app_state_system = self.app_state.clone();
        let app_state_update = self.app_state.clone();

        let theme = cx.theme();

        // —— 电源按钮(三态:断开 / 启动中 / 已连接) ——
        let power_button = power_button(
            "power-button",
            status,
            POWER_BUTTON_DIAMETER,
            POWER_ICON_SIZE,
            theme,
        );

        // Always takes a click: connect, disconnect, or, while starting
        // (the Linux TUN gate and the macOS helper's included), cancel the
        // start. A subscription fetch (auto-update included) doesn't hold
        // it off, as with Ctrl+S.
        // A tab stop (Enter / Space press it, like a button); the ring only
        // when the focus came from the keyboard, and a click doesn't take
        // the focus at all.
        let power_focused = self.power_focus.is_focused(window) && window.last_input_was_keyboard();
        let power_button = power_button
            .track_focus(&self.power_focus)
            .on_mouse_down(MouseButton::Left, |_, window, _| window.prevent_default())
            .when(power_focused, |this| this.focus_ring_style(window, cx))
            .cursor_pointer()
            .on_click(move |_, _, cx| {
                app_state_toggle.update(cx, |state, cx| state.toggle_process(cx));
            })
            .tooltip(move |window, cx| {
                Tooltip::new(status.power_action_label())
                    .action(&ToggleProcess, Some(KEY_CONTEXT))
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
            ConnectionStatus::Connected if stalled => {
                Some(api_stalled_notice(theme).text_sm().into_any_element())
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
        let figure = |value: String| if has_sample { value } else { "…".to_string() };
        let stats = connected.then(|| {
            card_frame(theme)
                .gap_4()
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .gap_4()
                        .w_full()
                        .child(stat(
                            theme,
                            t.home.memory,
                            figure(format_bytes(runtime.memory)),
                        ))
                        .child(stat(
                            theme,
                            t.home.connections,
                            figure(runtime.connections_in.to_string()),
                        ))
                        .child(stat(
                            theme,
                            t.home.uploaded,
                            figure(format_bytes(runtime.uplink_total)),
                        ))
                        .child(stat(
                            theme,
                            t.home.downloaded,
                            figure(format_bytes(runtime.downlink_total)),
                        )),
                )
                .child(traffic_chart)
        });

        // —— 快捷设置:代理模式 / 系统代理 / Clash 模式,一张分组卡 ——
        let mut quick_rows = vec![
            // Without TUN here (macOS without its privileged helper) its tab
            // stays, greyed out, with how to get it under the label. A saved
            // TUN choice stays selected (`AppSettings::proxy_mode`): Proxy
            // can be picked, and a start asks to install the helper.
            setting_row(
                theme,
                t.home.proxy_mode,
                (!tun_available).then_some(t.home.tun_needs_helper),
            )
            .child(segmented(
                theme,
                "proxy-mode",
                ControlSize::Regular,
                vec![
                    Segment::new(t.home.mode_tun).disabled(!tun_available),
                    Segment::new(t.home.mode_proxy),
                ],
                Some(if proxy_mode { 1 } else { 0 }),
                move |ix, _, cx| {
                    let value = ix == 1;
                    app_state_mode.update(cx, |state, cx| state.set_proxy_mode(value, cx));
                },
            ))
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

        // —— 配置卡:名字(切换菜单) + 更新时间,Update 按钮,用量条 ——
        // The name is the switcher: a quiet ghost button with a caret that
        // opens every profile (the current one checked), like a row click
        // on the Profiles page — restarting sing-box onto it if it runs.
        // Pulled left by its padding (below) so the name lines up with the
        // line under it; it ellipsizes inside the button, the tooltip has it
        // all.
        let app_state_switch = self.app_state.clone();
        let long_name = may_truncate(&profile_name, PROFILE_NAME_ROOM);
        let profile_switcher = Button::new("home-profile-switcher")
            .ghost()
            .control(ControlSize::Inline)
            .dropdown_caret(true)
            .text_label(profile_name.clone())
            .font_weight(FontWeight::MEDIUM)
            .when(long_name, |button| button.tooltip(profile_name.clone()))
            // Connected, the stats card pushes this one to the bottom of the
            // window: the menu opens upwards there rather than squeezing in
            // over its own trigger.
            .dropdown_menu_with_anchor(
                if connected {
                    Anchor::BottomLeft
                } else {
                    Anchor::TopLeft
                },
                move |menu, _, _| {
                    let menu = menu
                        .min_w(px(200.))
                        .max_w(px(PROFILE_MENU_MAX_WIDTH))
                        .max_h(px(PROFILE_MENU_MAX_HEIGHT))
                        .scrollable(true);
                    let menu = profiles.iter().fold(menu, |menu, (id, name)| {
                        let app_state = app_state_switch.clone();
                        let id = id.clone();
                        menu.item(
                            PopupMenuItem::new(shorten(name, PROFILE_MENU_NAME_ROOM))
                                .checked(id == active_id)
                                .on_click(move |_, _, cx| {
                                    app_state.update(cx, |state, cx| {
                                        state.set_active_profile(id.clone(), cx)
                                    });
                                }),
                        )
                    });
                    menu.separator()
                        .item(PopupMenuItem::new(s().home.manage_profiles).on_click(
                            |_, window, cx| window.dispatch_action(Box::new(ShowProfiles), cx),
                        ))
                },
            );

        // The same object as a row of the Profiles page: name line with the
        // update button at its end, the source under it, then usage.
        let update_button = freshness.map(|freshness| {
            freshness_button(
                theme,
                "home-update",
                freshness,
                Some(&UpdateSubscription),
                move |_, _, cx| {
                    app_state_update.update(cx, |state, cx| state.update_subscription(cx));
                },
            )
        });
        let profile_card = card_frame(theme)
            .child(
                div()
                    .v_flex()
                    .gap_0p5()
                    .child(
                        div()
                            .h_flex()
                            .items_center()
                            .gap_3()
                            .w_full()
                            .child(
                                // Absolutely placed, the switcher is as wide
                                // as its name (shrink-to-fit) up to the
                                // column's width, where the name ellipsizes;
                                // in the flow it would either stretch across
                                // the card or refuse to shrink. Pulled left
                                // by its padding, so the name lines up with
                                // the source under it.
                                div()
                                    .relative()
                                    .flex_1()
                                    .min_w_0()
                                    .h(CONTROL_LINE_HEIGHT)
                                    .child(
                                        div()
                                            .absolute()
                                            .top_0()
                                            .left(px(-8.))
                                            .max_w_full()
                                            .child(profile_switcher),
                                    ),
                            )
                            // Pulled right by its padding, so its text ends
                            // where the usage line under it does.
                            .children(
                                update_button
                                    .map(|button| div().flex_none().mr(px(-8.)).child(button)),
                            ),
                    )
                    .children(
                        source_info.map(|info| profile_source_line(theme, "home-source", info)),
                    ),
            )
            .children(usage.map(|usage| usage_meter(theme, "home-usage", &usage, now)));

        let body = div()
            .v_flex()
            .w_full()
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
                    .child(section_heading(theme, t.home.profile))
                    .child(profile_card),
            );
        // 窗口矮时整页滚动。
        scroll_page(page_header(theme, ActivePage::Home), body).into_any_element()
    }
}
