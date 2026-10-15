//! Home: what the running sing-box is doing, all of it from the sing-box
//! API — the dashboard the sidebar opens on.
//!
//! - Header: the uptime under the title (`GetStartedAt`), and the Clash
//!   mode on the right (`GetClashModeStatus` / `SetClashMode`, only with
//!   ≥2 modes to pick from).
//! - A row of facts: the exit node the first group leads to
//!   (`SubscribeGroups`), memory and goroutines (`SubscribeStatus`), and the
//!   sing-box version (`GetVersion`).
//! - Node delay: each of the first groups' node with its latest URL test,
//!   a button to test them all (`URLTest`) and one to the Tools page
//!   (`StartNetworkQualityTest` / `StartSTUNTest`).
//! - Upload and download: the rate now, its peak and the last two minutes
//!   (`SubscribeStatus`).
//! - Active connections: how many, by process and network
//!   (`SubscribeConnections`), and Close all (`CloseAllConnections`).
//! - Traffic over the last hour, all or proxied only, and who moved the
//!   most by process, domain or outbound (`SubscribeStatus` and
//!   `SubscribeConnections`, added up per minute by BoxPilot: the API keeps
//!   no history).
//! - The totals since sing-box started (`SubscribeStatus`).
//!
//! Until sing-box is connected Home is its power button, the status, and
//! the profile Connect would start with. Proxy Mode and System Proxy aren't
//! the API's to switch — BoxPilot writes them into the config it starts
//! sing-box with — so they live in Settings › Network.

use crate::actions::{ShowTools, ToggleProcess, KEY_CONTEXT};
use crate::core::bytefmt::{format_bytes, format_speed};
use crate::core::home_view::{group_leaves, open_breakdown, top_talkers, RankBy, Talker};
use crate::core::minute_series::{unix_minute, WINDOW_MINUTES};
use crate::core::presentation::{runtime_info, ConnectionStatus};
use crate::core::singbox_api::DelayState;
use crate::i18n::s;
use crate::state::{AppState, ClashMode};
use crate::ui::card_frame;
use crate::ui::pages::ActivePage;
use crate::ui::traffic_chart::{self, axis_label, axis_max, peak, Direction, RateChart};
use crate::ui::widgets::{
    api_stalled_notice, empty_state, empty_state_button, meta_row, minute_ticker, page_header,
    page_layout, power_button, scroll_page, segmented, stat, Control, ControlSize, IconLabel,
    Segment, TextLabel,
};
use gpui::{prelude::FluentBuilder, *};
use gpui_component::{
    button::{Button, ButtonVariants},
    spinner::Spinner,
    theme::Theme,
    tooltip::Tooltip,
    ActiveTheme, Disableable, Icon, IconName, StyledExt, ThemeStyled,
};
use std::time::SystemTime;

/// 电源按钮直径与图标尺寸(px), while sing-box isn't connected.
const POWER_BUTTON_DIAMETER: f32 = 56.;
const POWER_ICON_SIZE: f32 = 22.;
/// How many groups the delay card lists under the exit node.
const DELAY_GROUPS: usize = 3;
/// How many rows the traffic ranking lists.
const RANKING_ROWS: usize = 5;
/// The traffic chart's plot height.
const CHART_HEIGHT: f32 = 120.;
/// Narrower windows than this stack the delay card over the two rate
/// cards: beside it, each rate card is too narrow for its figure.
const WIDE_LAYOUT_MIN_WIDTH: f32 = 1100.;

pub struct HomePage {
    app_state: Entity<AppState>,
    /// The power button's: Tab reaches it, Enter / Space press it.
    power_focus: FocusHandle,
    /// The rate cards' live charts, their own (cached) views so their
    /// per-mouse-move hover repaints only the chart.
    upload_chart: Entity<RateChart>,
    download_chart: Entity<RateChart>,
    /// The traffic chart shows proxied connections' bytes only.
    proxied_only: bool,
    rank_by: RankBy,
    /// Re-renders once a minute: the uptime moves with the clock, and the
    /// traffic chart's minutes move left even while nothing is sent.
    _ticker: Task<()>,
}

impl HomePage {
    pub fn new(app_state: Entity<AppState>, cx: &mut Context<Self>) -> Self {
        let state = app_state.read(cx);
        let process = state.process.clone();
        let traffic = state.traffic.clone();
        let clash_mode = state.clash_mode.clone();
        let proxy_groups = state.proxy_groups.clone();
        let connections = state.connections.clone();
        cx.observe(&app_state, |_, _, cx| cx.notify()).detach();
        cx.observe(&process, |_, _, cx| cx.notify()).detach();
        // Status samples arrive once a second.
        cx.observe(&traffic, |_, _, cx| cx.notify()).detach();
        cx.observe(&clash_mode, |_, _, cx| cx.notify()).detach();
        cx.observe(&proxy_groups, |_, _, cx| cx.notify()).detach();
        cx.observe(&connections, |_, _, cx| cx.notify()).detach();
        let upload_chart = cx.new(|cx| RateChart::new(traffic.clone(), Direction::Upload, cx));
        let download_chart = cx.new(|cx| RateChart::new(traffic, Direction::Download, cx));
        Self {
            app_state,
            power_focus: cx.focus_handle().tab_stop(true),
            upload_chart,
            download_chart,
            proxied_only: false,
            rank_by: RankBy::default(),
            _ticker: minute_ticker(cx),
        }
    }

    /// Until sing-box is connected: the power button, the status and the
    /// profile Connect would start with, centred on the page.
    fn render_idle(
        &self,
        status: ConnectionStatus,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let t = s();
        let profile_name = self
            .app_state
            .read(cx)
            .settings
            .active_profile()
            .map(|p| p.name.clone())
            .unwrap_or_default();
        let theme = cx.theme();
        let app_state = self.app_state.clone();
        // Always takes a click: connect, or, while starting (the Linux TUN
        // gate and the macOS helper's included), cancel the start. A tab
        // stop (Enter / Space press it, like a button); the ring only when
        // the focus came from the keyboard, and a click doesn't take the
        // focus at all.
        let power_focused = self.power_focus.is_focused(window) && window.last_input_was_keyboard();
        let power = power_button(
            "power-button",
            status,
            POWER_BUTTON_DIAMETER,
            POWER_ICON_SIZE,
            theme,
        )
        .track_focus(&self.power_focus)
        .on_mouse_down(MouseButton::Left, |_, window, _| window.prevent_default())
        .when(power_focused, |this| this.focus_ring_style(window, cx))
        .cursor_pointer()
        .on_click(move |_, _, cx| {
            app_state.update(cx, |state, cx| state.toggle_process(cx));
        })
        .tooltip(move |window, cx| {
            Tooltip::new(status.power_action_label())
                .action(&ToggleProcess, Some(KEY_CONTEXT))
                .build(window, cx)
        });
        let detail = (!profile_name.is_empty()).then(|| {
            let line = if status == ConnectionStatus::Starting {
                t.home.starting_with
            } else {
                t.home.ready_with
            };
            div()
                .max_w(px(360.))
                .truncate()
                .text_sm()
                .text_color(theme.muted_foreground)
                .child(line(&profile_name))
        });
        let hero = div()
            .absolute()
            .inset_0()
            .v_flex()
            .items_center()
            .justify_center()
            .gap_2()
            // A little above the middle, where the eye puts the centre.
            .pb_8()
            .child(div().mb_2().child(power))
            .child(
                div()
                    .text_xl()
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(theme.foreground)
                    .child(status.label()),
            )
            .children(detail);
        page_layout(
            page_header(theme, ActivePage::Home),
            div().size_full().child(hero),
        )
        .into_any_element()
    }
}

/// The Clash mode switch in the header. `None` unless there is a choice to
/// offer (running, ≥2 modes).
fn clash_mode_switch(theme: &Theme, clash_mode: Entity<ClashMode>, cx: &App) -> Option<AnyElement> {
    let state = clash_mode.read(cx);
    if !state.is_switchable() {
        return None;
    }
    let modes = state.modes.clone();
    let selected = state.current_index();
    let segments = modes.iter().cloned().map(Segment::new).collect();
    Some(
        div()
            .h_flex()
            .items_center()
            .gap_2()
            .child(
                div()
                    .text_sm()
                    .text_color(theme.muted_foreground)
                    .child(s().home.clash_mode),
            )
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

/// A formatted amount split for display: the number large, its unit small
/// after it ("1.4" "MB/s").
fn figure(theme: &Theme, text: &str) -> Div {
    let (number, unit) = text.split_once(' ').unwrap_or((text, ""));
    div()
        .h_flex()
        .items_baseline()
        .gap_1()
        .whitespace_nowrap()
        .child(
            div()
                .text_3xl()
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(theme.foreground)
                .child(number.to_string()),
        )
        .when(!unit.is_empty(), |row| {
            row.child(
                div()
                    .text_sm()
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(theme.muted_foreground)
                    .child(unit.to_string()),
            )
        })
}

/// A card's caption: small and muted, like the facts row's.
fn caption(theme: &Theme, text: impl Into<SharedString>) -> Div {
    div()
        .text_xs()
        .font_weight(FontWeight::MEDIUM)
        .text_color(theme.muted_foreground)
        .whitespace_nowrap()
        .child(text.into())
}

/// A card's head row: caption on the left, `trailing` on the right.
fn card_head(theme: &Theme, text: impl Into<SharedString>) -> Div {
    div()
        .h_flex()
        .items_center()
        .gap_2()
        .min_h(ControlSize::Regular.height())
        .child(caption(theme, text))
}

/// Columns of small figures under a card's main one, set apart by rules.
fn columns(theme: &Theme, items: Vec<Div>) -> Div {
    let border = theme.border;
    div()
        .h_flex()
        .w_full()
        .mt_auto()
        .pt_2()
        .children(items.into_iter().enumerate().map(move |(ix, item)| {
            div()
                .flex_1()
                .min_w_0()
                .when(ix > 0, |col| col.pl_4().border_l_1().border_color(border))
                .when(ix == 0, |col| col.pr_4())
                .child(item)
        }))
}

/// A delay as Home writes it: "86 ms", "Timeout" or "Not tested".
fn delay_text(delay: Option<&DelayState>) -> String {
    match delay {
        Some(DelayState::Ok(ms)) => format!("{ms} ms"),
        Some(DelayState::Timeout) => s().groups.timeout.to_string(),
        None => s().home.not_tested.to_string(),
    }
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
        if status != ConnectionStatus::Connected {
            return self.render_idle(status, window, cx);
        }

        let traffic = state.traffic.read(cx);
        let runtime = traffic.status;
        // The API stopped answering mid-run: the figures below are the last
        // ones it gave.
        let stalled = traffic.stalled;
        // Before the first sample the figures are unknown, not zero.
        let has_sample = traffic.has_sample();
        let known = |value: String| if has_sample { value } else { "…".to_string() };
        let info = runtime_info(
            traffic.started_at,
            traffic.version.as_deref(),
            SystemTime::now(),
        );
        let version = traffic.version.clone();
        let history = traffic.history();
        let (up_peak, down_peak) = (
            peak(history, Direction::Upload),
            peak(history, Direction::Download),
        );
        let (up_rate, down_rate) = (traffic.up, traffic.down);
        let now_minute = unix_minute(SystemTime::now());

        let groups_state = state.proxy_groups.read(cx);
        let leaves: Vec<(String, Option<String>, Option<DelayState>)> =
            group_leaves(&groups_state.groups, DELAY_GROUPS)
                .into_iter()
                .map(|(group, leaf)| {
                    let delay = leaf.and_then(|leaf| groups_state.delays.get(leaf).copied());
                    (group.to_string(), leaf.map(str::to_string), delay)
                })
                .collect();
        let testing = groups_state.testing_any_group();

        let connections = state.connections.read(cx);
        let breakdown = open_breakdown(connections.table.iter());
        let open = connections.table.open_count();
        let talkers = top_talkers(connections.table.iter(), self.rank_by, RANKING_ROWS);
        let moved_any = connections
            .table
            .iter()
            .any(|c| c.uplink_total > 0 || c.downlink_total > 0);
        let minutes = if self.proxied_only {
            connections.proxied_minutes.window(now_minute)
        } else {
            traffic.minutes().window(now_minute)
        };
        let clash_mode = state.clash_mode.clone();
        let proxy_groups = state.proxy_groups.clone();
        let connections_entity = state.connections.clone();
        let wide = window.viewport_size().width >= px(WIDE_LAYOUT_MIN_WIDTH);

        let theme = cx.theme();
        let colors = traffic_chart::series_colors(theme);

        // —— Header: uptime under the title, Clash mode on the right ——
        let mut header = page_header(theme, ActivePage::Home);
        if stalled {
            header = header.context(api_stalled_notice(theme));
        } else if let Some(uptime) = info.uptime {
            header = header.context(meta_row(theme, [uptime]));
        }
        if let Some(switch) = clash_mode_switch(theme, clash_mode, cx) {
            header = header.action(switch);
        }

        // —— Facts row ——
        let exit_node = leaves
            .first()
            .and_then(|(_, leaf, _)| leaf.clone())
            .unwrap_or_else(|| "—".to_string());
        let facts = div()
            .h_flex()
            .w_full()
            .gap_4()
            .child(stat(theme, t.home.exit_node, exit_node))
            .child(stat(
                theme,
                t.home.memory,
                known(format_bytes(runtime.memory)),
            ))
            .child(stat(
                theme,
                t.home.goroutines,
                known(runtime.goroutines.to_string()),
            ))
            .child(stat(
                theme,
                t.home.sing_box_version,
                version.unwrap_or_else(|| "…".to_string()),
            ));

        // —— Node delay ——
        let test_button = Button::new("home-test-delays")
            .ghost()
            .icon_control(ControlSize::Inline)
            .map(|button| {
                if testing {
                    button.icon(Spinner::new())
                } else {
                    button.icon(Icon::empty().path("icons/zap.svg"))
                }
            })
            .tooltip(t.groups.test_all)
            .loading(testing)
            .disabled(leaves.is_empty())
            .on_click(move |_, _, cx| {
                proxy_groups.update(cx, |state, cx| state.test_all(cx));
            });
        let diagnostics = Button::new("home-diagnostics")
            .outline()
            .control(ControlSize::Regular)
            .text_label(t.home.diagnostics)
            .on_click(|_, window, cx| window.dispatch_action(Box::new(ShowTools), cx));
        // A delay reads as a figure; "Timeout" / "Not tested" in its place
        // are words, set smaller and muted.
        let exit_delay = match leaves.first().map(|(_, _, delay)| delay.as_ref()) {
            Some(Some(delay @ DelayState::Ok(_))) => figure(theme, &delay_text(Some(delay))),
            Some(delay) => div()
                .text_xl()
                .font_weight(FontWeight::MEDIUM)
                .text_color(theme.muted_foreground)
                .child(delay_text(delay)),
            None => figure(theme, "—"),
        };
        let exit_name = leaves.first().and_then(|(_, leaf, _)| leaf.clone());
        let delay_columns: Vec<Div> = leaves
            .iter()
            .map(|(group, leaf, delay)| {
                div()
                    .v_flex()
                    .min_w_0()
                    .gap_0p5()
                    .child(caption(theme, group.clone()).truncate())
                    .child(
                        div()
                            .text_lg()
                            .font_weight(FontWeight::SEMIBOLD)
                            .whitespace_nowrap()
                            .child(delay_text(delay.as_ref())),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .truncate()
                            .child(leaf.clone().unwrap_or_else(|| "—".to_string())),
                    )
            })
            .collect();
        let delay_card = card_frame(theme)
            .flex_1()
            .min_w_0()
            .child(
                card_head(theme, t.home.node_delay)
                    .child(test_button)
                    .child(div().flex_1())
                    .child(diagnostics),
            )
            .child(
                div()
                    .h_flex()
                    .items_baseline()
                    .gap_3()
                    .min_w_0()
                    .child(exit_delay)
                    .children(exit_name.map(|name| {
                        div()
                            .min_w_0()
                            .truncate()
                            .text_sm()
                            .text_color(theme.muted_foreground)
                            .child(name)
                    })),
            )
            .when(!delay_columns.is_empty(), |card| {
                card.child(columns(theme, delay_columns))
            });

        // —— Upload / download ——
        let rate_card =
            |label: &'static str, rate: u64, peak: u64, chart: AnyElement, color: Hsla| {
                card_frame(theme)
                    .flex_1()
                    .min_w_0()
                    .pb_0()
                    .overflow_hidden()
                    .child(
                        card_head(theme, label)
                            .child(div().size_2().rounded_full().bg(color))
                            .child(div().flex_1())
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(theme.muted_foreground)
                                    .whitespace_nowrap()
                                    .child((t.home.peak)(&format_speed(peak))),
                            ),
                    )
                    .child(figure(theme, &known(format_speed(rate))))
                    .child(div().flex_1())
                    // Edge to edge along the card's foot.
                    .child(div().mx(px(-16.)).child(chart))
            };
        let chart_view = |chart: &Entity<RateChart>| {
            AnyView::from(chart.clone())
                .cached(
                    StyleRefinement::default()
                        .w_full()
                        .h(px(traffic_chart::HEIGHT)),
                )
                .into_any_element()
        };
        let rates = div()
            .h_flex()
            .items_stretch()
            .gap_4()
            .flex_1()
            .min_w_0()
            .child(rate_card(
                t.common.upload,
                up_rate,
                up_peak,
                chart_view(&self.upload_chart),
                colors.upload,
            ))
            .child(rate_card(
                t.common.download,
                down_rate,
                down_peak,
                chart_view(&self.download_chart),
                colors.download,
            ));
        let top_row = if wide {
            div()
                .h_flex()
                .items_stretch()
                .gap_4()
                .child(delay_card)
                .child(rates)
        } else {
            div().v_flex().gap_4().child(delay_card).child(rates)
        };

        // —— Active connections ——
        let close_all = Button::new("home-close-all")
            .outline()
            .control(ControlSize::Regular)
            .text_label(t.connections.close_all)
            .disabled(open == 0)
            .on_click(move |_, _, cx| {
                connections_entity.update(cx, |state, cx| state.close_all(cx));
            });
        let small = |value: usize, label: &'static str| {
            div()
                .v_flex()
                .min_w_0()
                .child(
                    div()
                        .text_lg()
                        .font_weight(FontWeight::SEMIBOLD)
                        .child(value.to_string()),
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .truncate()
                        .child(label),
                )
        };
        let connections_card = card_frame(theme)
            .flex_1()
            .child(
                card_head(theme, t.home.active_connections)
                    .child(div().flex_1())
                    .child(close_all),
            )
            .child(figure(theme, &known(runtime.connections_in.to_string())))
            .child(columns(
                theme,
                vec![
                    small(breakdown.processes, t.home.processes),
                    small(breakdown.tcp, "TCP"),
                    small(breakdown.udp, "UDP"),
                ],
            ));

        // —— Totals this run ——
        let (up_total, down_total) = (runtime.uplink_total, runtime.downlink_total);
        let total = up_total.saturating_add(down_total);
        let up_share = if total > 0 {
            up_total as f32 / total as f32
        } else {
            0.5
        };
        let split = |label: &'static str, bytes: u64| {
            div().v_flex().gap_0p5().child(caption(theme, label)).child(
                div()
                    .text_lg()
                    .font_weight(FontWeight::SEMIBOLD)
                    .whitespace_nowrap()
                    .child(known(format_bytes(bytes))),
            )
        };
        let bar = |color: Hsla| div().h(px(10.)).rounded(px(5.)).bg(color);
        let (up_bar, down_bar) = if total > 0 {
            (bar(colors.upload), bar(colors.download))
        } else {
            (bar(theme.muted), bar(theme.muted))
        };
        let totals_card = card_frame(theme)
            .flex_1()
            .child(
                card_head(theme, t.home.run_total)
                    .child(div().flex_1())
                    .child(caption(theme, t.home.run_total_hint)),
            )
            .child(figure(theme, &known(format_bytes(total))))
            .child(
                div()
                    .mt_auto()
                    .v_flex()
                    .gap_2()
                    .child(
                        div()
                            .h_flex()
                            .justify_between()
                            .child(split(t.common.upload, up_total))
                            .child(
                                div()
                                    .items_end()
                                    .child(split(t.common.download, down_total)),
                            ),
                    )
                    .child(
                        div()
                            .h_flex()
                            .w_full()
                            .gap_1()
                            .child(up_bar.w(relative(up_share)).min_w(px(6.)))
                            .child(down_bar.flex_1().min_w(px(6.))),
                    ),
            );

        // —— Traffic: per minute, and the ranking ——
        let page = cx.entity().downgrade();
        let series_switch = segmented(
            theme,
            "home-traffic-series",
            ControlSize::Inline,
            vec![
                Segment::new(t.home.traffic_all),
                Segment::new(t.home.traffic_proxied),
            ],
            Some(usize::from(self.proxied_only)),
            move |ix, _, cx| {
                page.update(cx, |this, cx| {
                    this.proxied_only = ix == 1;
                    cx.notify();
                })
                .ok();
            },
        );
        let top = axis_max(minutes.iter().copied().max().unwrap_or(0));
        let bars = minutes.iter().enumerate().map(|(ix, &bytes)| {
            let height = if bytes == 0 {
                3.
            } else {
                (bytes as f32 / top as f32 * CHART_HEIGHT).max(3.)
            };
            div()
                .id(("home-minute", ix))
                .flex_1()
                .min_w_0()
                .h(px(height))
                .rounded_t(px(2.))
                .bg(if bytes == 0 {
                    theme.border
                } else {
                    colors.download
                })
                .when(bytes > 0, |bar| {
                    let minutes_ago = (WINDOW_MINUTES - 1 - ix) as u16;
                    let when = if minutes_ago == 0 {
                        t.chart.now.to_string()
                    } else {
                        (t.chart.mins_ago)(minutes_ago)
                    };
                    let tip: SharedString = format!("{when}  {}", format_bytes(bytes)).into();
                    bar.tooltip(move |window, cx| Tooltip::new(tip.clone()).build(window, cx))
                })
        });
        let gridline = |at: f32, label: SharedString| {
            div()
                .absolute()
                .left_0()
                .right_0()
                .top(px(at))
                .h_flex()
                .items_center()
                .gap_2()
                .child(div().flex_1().h(px(1.)).bg(theme.border.opacity(0.7)))
                .child(
                    div()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child(label),
                )
        };
        let plot = div()
            .relative()
            .w_full()
            .h(px(CHART_HEIGHT + 8.))
            .child(gridline(0., axis_label(top)))
            .child(gridline(CHART_HEIGHT / 2., axis_label(top / 2)))
            .child(
                div()
                    .absolute()
                    .left_0()
                    .right(px(64.))
                    .bottom_0()
                    .h(px(CHART_HEIGHT))
                    .h_flex()
                    .items_end()
                    .gap(px(2.))
                    .children(bars),
            );
        let axis = div()
            .h_flex()
            .justify_between()
            .mr(px(64.))
            .text_xs()
            .text_color(theme.muted_foreground)
            .child((t.chart.mins_ago)(WINDOW_MINUTES as u16))
            .child((t.chart.mins_ago)(WINDOW_MINUTES as u16 / 2))
            .child(t.chart.now);

        let page = cx.entity().downgrade();
        const RANKS: [RankBy; 3] = [RankBy::Process, RankBy::Domain, RankBy::Outbound];
        let rank_switch = segmented(
            theme,
            "home-rank-by",
            ControlSize::Inline,
            vec![
                Segment::new(t.home.rank_process),
                Segment::new(t.home.rank_domain),
                Segment::new(t.home.rank_outbound),
            ],
            RANKS.iter().position(|by| *by == self.rank_by),
            move |ix, _, cx| {
                page.update(cx, |this, cx| {
                    this.rank_by = RANKS[ix];
                    cx.notify();
                })
                .ok();
            },
        );
        let most = talkers.first().map_or(1, |talker| talker.bytes.max(1));
        let ranking = if talkers.is_empty() {
            // Bytes moved, but no process to put them on: sing-box looks
            // processes up only in TUN mode (or for a process rule).
            let empty = if self.rank_by == RankBy::Process && moved_any {
                t.home.no_process_info
            } else {
                t.home.no_ranking
            };
            div()
                .py_4()
                .text_sm()
                .text_color(theme.muted_foreground)
                .child(empty)
        } else {
            div()
                .v_flex()
                .gap_3()
                .children(talkers.iter().map(|talker| talker_row(theme, talker, most)))
        };
        let traffic_card = card_frame(theme)
            .flex_1()
            .min_w_0()
            .child(
                card_head(theme, t.home.traffic_hour)
                    .child(div().flex_1())
                    .child(series_switch),
            )
            .child(plot)
            .child(axis)
            .child(div().mt_2().h_flex().child(rank_switch))
            .child(ranking);

        let bottom_row = div()
            .h_flex()
            .items_stretch()
            .gap_4()
            .child(
                div()
                    .v_flex()
                    .flex_1()
                    .min_w_0()
                    .gap_4()
                    .child(connections_card)
                    .child(totals_card),
            )
            .child(traffic_card);

        let body = div()
            .v_flex()
            .w_full()
            .gap_4()
            .pb_2()
            .child(facts)
            .child(top_row)
            .child(bottom_row);
        // 窗口矮时整页滚动。
        scroll_page(header, body).into_any_element()
    }
}

/// One row of the ranking: an initial in a tile, the name and its bytes,
/// and a bar for its share of the top row's.
fn talker_row(theme: &Theme, talker: &Talker, most: u64) -> Div {
    let initial: String = talker
        .name
        .chars()
        .next()
        .map(|c| c.to_uppercase().collect())
        .unwrap_or_default();
    let share = (talker.bytes as f32 / most as f32).clamp(0., 1.);
    div()
        .h_flex()
        .items_center()
        .gap_3()
        .child(
            div()
                .flex_none()
                .size(px(26.))
                .rounded(px(6.))
                .bg(theme.muted)
                .flex()
                .items_center()
                .justify_center()
                .text_xs()
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(theme.foreground)
                .child(initial),
        )
        .child(
            div()
                .v_flex()
                .flex_1()
                .min_w_0()
                .gap_1()
                .child(
                    div()
                        .h_flex()
                        .justify_between()
                        .gap_3()
                        .text_sm()
                        .child(div().min_w_0().truncate().child(talker.name.clone()))
                        .child(
                            div()
                                .flex_none()
                                .font_weight(FontWeight::MEDIUM)
                                .child(format_bytes(talker.bytes)),
                        ),
                )
                .child(
                    div()
                        .w_full()
                        .h(px(4.))
                        .rounded(px(2.))
                        .bg(theme.muted)
                        .child(
                            div()
                                .h_full()
                                .w(relative(share))
                                .rounded(px(2.))
                                .bg(theme.muted_foreground.opacity(0.5)),
                        ),
                ),
        )
}
