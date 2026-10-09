//! 连接页,日志浏览器式布局(与日志页一致):控制行(搜索框 + Hide direct
//! 快速过滤开关(隐藏直连 / 拦截 / DNS 连接,规则见
//! `connections_view::is_direct_or_dns`;开关记在设置
//! `connections_hide_direct` 里)+ Active/Closed 切换 + 排序:下拉选排序键
//! Newest/Traffic/Speed/Host/Rule/Chain,旁边的按钮切换升序 / 降序;换键时
//! 回到该键的自然方向,数值与时间大的在前、文字 A→Z;控制行不折行,窄时
//! 搜索框让位);页头一行汇总(打开数 —— 有过滤时换成「N of M shown」、实时
//! 速率、总流量)和 Close all(过滤让一部分打开的连接不显示时变成
//! 「Close N matching」,只关闭过滤后剩下的打开连接:在一个后台任务里逐个
//! `close`);下面是带表头的连接表,每行一行:建立时间(等宽,毫秒部分
//! 淡色)、网络徽标(TCP/UDP,与柱状图同色)、主机 + 进程名、出站链
//! (组 → 节点)、实时速率与累计流量(各两行:上行在上、下行在下;已关闭的
//! 连接没有速率)、存活时长、关闭按钮(仅打开的连接)。网络/协议、入站、
//! 规则在详情面板里。
//!
//! 列表用 gpui 的 `uniform_list` 虚拟化:只渲染可见行,上千条已关闭 + 大量
//! 打开的连接也不卡。过滤 / 排序结果(按 id 的有序列表)和 Close all 的
//! 目标按 `(revision, view, sort, direction, query, hide_direct)` 缓存,只有
//! 数据或条件变了才重算;行内容在渲染时按 id 从 `ConnectionTable` 现取,
//! 时长随渲染时刻走。
//! 过滤 / 排序 / 显示字符串都是 `core::connections_view` 的纯函数。
//!
//! Clicking a row opens the details panel (`connection_details`) over the
//! right of the list and highlights the row; while it is open Esc closes it
//! and Up / Down select the neighbouring row of the current list (the page
//! sets `CONNECTION_DETAILS_CONTEXT` then, and keeps keyboard focus on its
//! own handle). The selection lives here; the panel follows it.

use super::connection_details::{ConnectionDetailsPanel, DetailsDismissed};
use crate::actions::{
    CloseConnectionDetails, SelectNextConnection, SelectPreviousConnection,
    CONNECTION_DETAILS_CONTEXT,
};
use crate::core::bytefmt::{format_bytes, format_speed};
use crate::core::connection_details::{step_selection, Step};
use crate::core::connections_view::{
    chain_label, close_targets, connection_age_ms, format_elapsed, host_label, process_name,
    select_connections, summarize, CloseTargets, ConnectionFilter, ConnectionSort, ConnectionView,
    SortDirection,
};
use crate::core::singbox_api::Connection;
use crate::core::timefmt::format_clock_ms;
use crate::i18n::s;
use crate::state::{AppState, Connections};
use crate::ui::locale;
use crate::ui::pages::ActivePage;
use crate::ui::widgets::{
    choice_select, connect_button, empty_state, form_input, full_text_tooltip, page_header,
    page_layout, row_hover_bg, segmented, tag_badge, toolbar_search, warn_orange, IconLabel,
    Segment, TextLabel,
};
use gpui::prelude::FluentBuilder;
use gpui::*;
use gpui_component::{
    button::{Button, ButtonVariants},
    input::{InputEvent, InputState},
    scroll::ScrollableElement,
    theme::Theme,
    ActiveTheme, Disableable, Icon, IconName, Selectable, Sizable, StyledExt,
};
use std::rc::Rc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Fixed row height — `uniform_list` lays every row out at the first row's
/// size, so all rows must match.
const ROW_HEIGHT: f32 = 36.;
/// Column widths and the space between columns, shared by the header and
/// the rows. Sized so the host and chain keep some room in an 880px window
/// beside the expanded sidebar.
const COLUMN_GAP: f32 = 8.;
const TIME_WIDTH: f32 = 104.;
const NETWORK_WIDTH: f32 = 56.;
/// The rate and traffic cells stack up over down, so they fit the widest
/// figure (`↑ 1023.9 KB/s`, `↑ 1023.9 MB`) in the mono font, not a pair.
const RATE_WIDTH: f32 = 96.;
const TRAFFIC_WIDTH: f32 = 84.;
/// Line height in the stacked cells: two lines inside `ROW_HEIGHT`.
const STACKED_LINE: f32 = 14.;
const AGE_WIDTH: f32 = 56.;
const CLOSE_WIDTH: f32 = 28.;
/// The details panel's width; on a narrow window it takes most of the list
/// (`DETAILS_MAX_FRACTION`), leaving the selected row's start in view.
const DETAILS_WIDTH: f32 = 380.;
const DETAILS_MAX_FRACTION: f32 = 0.86;
/// The panel's slide-in.
const DETAILS_SLIDE: Duration = Duration::from_millis(180);
const DETAILS_SLIDE_PX: f32 = 28.;

/// What the cached row order was derived from.
#[derive(Clone, PartialEq)]
struct RowsKey {
    revision: u64,
    view: ConnectionView,
    sort: ConnectionSort,
    direction: SortDirection,
    query: String,
    hide_direct: bool,
}

impl RowsKey {
    fn filter(&self) -> ConnectionFilter<'_> {
        ConnectionFilter {
            query: &self.query,
            hide_direct: self.hide_direct,
        }
    }
}

pub struct ConnectionsPage {
    /// Holds the remembered quick filter (`connections_hide_direct`).
    app_state: Entity<AppState>,
    connections: Entity<Connections>,
    filter_input: Entity<InputState>,
    view: ConnectionView,
    sort: ConnectionSort,
    direction: SortDirection,
    scroll: UniformListScrollHandle,
    /// Ids of the rows to show, in display order, and what they came from.
    rows: Rc<Vec<String>>,
    rows_key: Option<RowsKey>,
    /// What Close all closes under the same filter (refreshed with `rows`).
    close_targets: Rc<CloseTargets>,
    /// The connection the details panel shows; `None` = panel closed.
    selected: Option<SharedString>,
    /// Where `selected` last was in `rows`, so Up / Down carry on from there
    /// after it left the list (closed off the Active tab, filtered out).
    selected_ix: Option<usize>,
    /// Bumped each time the panel opens, to replay its slide-in.
    details_opened: u64,
    details: Entity<ConnectionDetailsPanel>,
    /// Holds keyboard focus on this page (any click in it), so the panel's
    /// key context is on the dispatch path.
    focus_handle: FocusHandle,
}

impl ConnectionsPage {
    pub fn new(app_state: Entity<AppState>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let connections = app_state.read(cx).connections.clone();
        cx.observe(&connections, |this: &mut Self, connections, cx| {
            // sing-box stopped: the list is gone, and the panel with it.
            if !connections.read(cx).live && this.selected.is_some() {
                this.close_details(cx);
            }
            cx.notify();
        })
        .detach();

        let details = cx.new(|cx| ConnectionDetailsPanel::new(connections.clone(), cx));
        cx.subscribe(&details, |this, _, _: &DetailsDismissed, cx| {
            this.close_details(cx)
        })
        .detach();

        let filter_input = cx
            .new(|cx| InputState::new(window, cx).placeholder(s().connections.filter_placeholder));
        locale::observe(window, cx, |this: &mut Self, window, cx| {
            this.filter_input.update(cx, |input, cx| {
                input.set_placeholder(s().connections.filter_placeholder, window, cx)
            });
        })
        .detach();
        cx.subscribe_in(&filter_input, window, |_, _, ev: &InputEvent, _, cx| {
            if matches!(ev, InputEvent::Change) {
                cx.notify();
            }
        })
        .detach();

        Self {
            app_state,
            connections,
            filter_input,
            view: ConnectionView::default(),
            sort: ConnectionSort::default(),
            direction: ConnectionSort::default().natural_direction(),
            scroll: UniformListScrollHandle::new(),
            rows: Rc::new(Vec::new()),
            rows_key: None,
            close_targets: Rc::new(CloseTargets::All),
            selected: None,
            selected_ix: None,
            details_opened: 0,
            details,
            focus_handle: cx.focus_handle(),
        }
    }

    /// Open the panel on `id` (or move it there) and highlight its row.
    fn select(&mut self, id: SharedString, window: &mut Window, cx: &mut Context<Self>) {
        if self.selected.is_none() {
            self.details_opened += 1;
        }
        self.selected_ix = self.rows.iter().position(|row| row.as_str() == id.as_ref());
        self.details
            .update(cx, |panel, cx| panel.set_selected(Some(id.to_string()), cx));
        self.selected = Some(id);
        // Out of the filter box, if that had focus: the panel's keys are
        // bound outside `Input`.
        if !self.focus_handle.is_focused(window) {
            self.focus_handle.focus(window, cx);
        }
        cx.notify();
    }

    fn close_details(&mut self, cx: &mut Context<Self>) {
        if self.selected.take().is_none() {
            return;
        }
        self.selected_ix = None;
        self.details
            .update(cx, |panel, cx| panel.set_selected(None, cx));
        cx.notify();
    }

    fn on_close_details(
        &mut self,
        _: &CloseConnectionDetails,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.close_details(cx);
    }

    fn on_select_previous(
        &mut self,
        _: &SelectPreviousConnection,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.step(Step::Previous, window, cx);
    }

    fn on_select_next(
        &mut self,
        _: &SelectNextConnection,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.step(Step::Next, window, cx);
    }

    /// Select the row above / below in the current (filtered, sorted) list
    /// and scroll it into view.
    fn step(&mut self, step: Step, window: &mut Window, cx: &mut Context<Self>) {
        self.refresh_rows(cx);
        let current = self
            .selected
            .as_ref()
            .and_then(|id| self.rows.iter().position(|row| row.as_str() == id.as_ref()));
        let Some(ix) = step_selection(self.rows.len(), current, self.selected_ix, step) else {
            return;
        };
        self.scroll.scroll_to_item(ix, ScrollStrategy::Nearest);
        if current != Some(ix) {
            let id = SharedString::from(self.rows[ix].clone());
            self.select(id, window, cx);
        }
    }

    fn set_view(&mut self, view: ConnectionView, cx: &mut Context<Self>) {
        if self.view != view {
            self.view = view;
            self.scroll.scroll_to_item(0, ScrollStrategy::Top);
            cx.notify();
        }
    }

    /// A new key starts in its natural direction (newest / biggest first,
    /// text A→Z); re-picking the current one keeps the direction.
    fn set_sort(&mut self, sort: ConnectionSort, cx: &mut Context<Self>) {
        if self.sort != sort {
            self.sort = sort;
            self.direction = sort.natural_direction();
            self.scroll.scroll_to_item(0, ScrollStrategy::Top);
            cx.notify();
        }
    }

    fn reverse_sort(&mut self, cx: &mut Context<Self>) {
        self.direction = self.direction.reversed();
        self.scroll.scroll_to_item(0, ScrollStrategy::Top);
        cx.notify();
    }

    /// Flip the quick filter ("Hide direct"), remembered in the settings.
    fn toggle_hide_direct(&mut self, cx: &mut Context<Self>) {
        let hide = !self.app_state.read(cx).settings.connections_hide_direct;
        self.app_state
            .update(cx, |state, cx| state.set_connections_hide_direct(hide, cx));
        self.scroll.scroll_to_item(0, ScrollStrategy::Top);
        cx.notify();
    }

    /// Recompute the row order only when the data or the view settings moved.
    fn refresh_rows(&mut self, cx: &App) {
        let hide_direct = self.app_state.read(cx).settings.connections_hide_direct;
        let state = self.connections.read(cx);
        let key = RowsKey {
            revision: state.revision,
            view: self.view,
            sort: self.sort,
            direction: self.direction,
            query: self.filter_input.read(cx).value().to_string(),
            hide_direct,
        };
        if self.rows_key.as_ref() == Some(&key) {
            return;
        }
        let ids = select_connections(
            state.table.iter(),
            key.view,
            key.filter(),
            key.sort,
            key.direction,
        )
        .into_iter()
        .map(|c| c.id.clone())
        .collect();
        self.rows = Rc::new(ids);
        self.close_targets = Rc::new(close_targets(state.table.iter(), key.filter()));
        self.rows_key = Some(key);
        // Remember where the selection is while it is listed (kept when it
        // is not: Up / Down continue from there).
        if let Some(id) = &self.selected {
            if let Some(ix) = self.rows.iter().position(|row| row.as_str() == id.as_ref()) {
                self.selected_ix = Some(ix);
            }
        }
    }
}

pub(super) fn unix_millis_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Single-line text that ellipsizes instead of wrapping.
/// Letters of a host the narrowest row shows whole; longer ones get a
/// tooltip with all of it.
const HOST_ROOM: usize = 24;

/// Flex-shrink factor for the part of a line that should give way first
/// when it runs short (gpui shrinks in proportion to factor × width).
const SHRINK_FIRST: f32 = 100.;

fn clipped(text: impl Into<SharedString>) -> Div {
    div()
        .min_w_0()
        .overflow_hidden()
        .text_ellipsis()
        .whitespace_nowrap()
        .child(text.into())
}

/// TCP and UDP badges: the accent blue and the warning orange, as the
/// Logs page's INFO and WARN.
#[derive(Clone, Copy)]
struct NetworkColors {
    tcp: Hsla,
    udp: Hsla,
}

impl NetworkColors {
    fn new(theme: &Theme) -> Self {
        Self {
            tcp: theme.primary,
            udp: warn_orange(theme),
        }
    }

    fn of(&self, network: &str, theme: &Theme) -> Hsla {
        match network {
            "tcp" => self.tcp,
            "udp" => self.udp,
            _ => theme.muted_foreground,
        }
    }
}

/// One list row. Every row has the same structure (closed rows keep an
/// empty slot where the close button goes) so heights stay uniform.
#[allow(clippy::too_many_arguments)]
fn connection_row(
    connection: &Connection,
    now_ms: i64,
    connections: &Entity<Connections>,
    page: &WeakEntity<ConnectionsPage>,
    selected: bool,
    networks: NetworkColors,
    theme: &Theme,
) -> Stateful<Div> {
    let closed = connection.is_closed();
    let hover_bg = row_hover_bg(theme);
    let (fg, muted) = if closed {
        (theme.muted_foreground, theme.muted_foreground.opacity(0.7))
    } else {
        (theme.foreground, theme.muted_foreground)
    };

    let (clock, fraction) = format_clock_ms(connection.created_at);
    let time = div()
        .flex_none()
        .w(px(TIME_WIDTH))
        .h_flex()
        .text_sm()
        .font_family(theme.mono_font_family.clone())
        .child(div().text_color(fg).child(clock))
        .child(div().text_color(muted).child(fraction));

    let network = connection.network.to_lowercase();
    let badge_color = networks.of(&network, theme);
    let badge = div()
        .flex_none()
        .w(px(NETWORK_WIDTH))
        .h_flex()
        .child(tag_badge(
            theme,
            if network.is_empty() {
                "—".to_string()
            } else {
                network.to_uppercase()
            },
            if closed {
                badge_color.opacity(0.6)
            } else {
                badge_color
            },
        ));

    // Host and chain share what the fixed columns leave, two to one.
    let mut host = div()
        .h_flex()
        .flex_grow(2.)
        .flex_basis(px(0.))
        .min_w(px(80.))
        .items_center()
        .gap_2()
        .child(full_text_tooltip(
            div()
                .min_w_0()
                .overflow_hidden()
                .text_ellipsis()
                .whitespace_nowrap()
                .flex_shrink(1.)
                .text_sm()
                .text_color(fg),
            SharedString::from(format!("conn-host-{}", connection.id)),
            host_label(connection),
            HOST_ROOM,
        ));
    if let Some(process) = process_name(connection) {
        host = host.child(
            // Gives way before the host does.
            clipped(process.to_string())
                .flex_shrink(SHRINK_FIRST)
                .max_w(px(140.))
                .text_xs()
                .text_color(muted),
        );
    }

    // Clipped short of room; the details panel has it in full.
    let chain = clipped(chain_label(connection))
        .flex_grow(1.)
        .flex_basis(px(0.))
        .text_xs()
        .text_color(muted);

    // The current rate, up over down; a line that is idle this second
    // fades, and a closed connection has none.
    let idle = muted.opacity(0.5);
    let rate_line = |arrow: &str, rate: u64| {
        div()
            .text_color(if rate == 0 { idle } else { fg })
            .child(format!("{arrow} {}", format_speed(rate)))
    };
    let rate = stacked_cell(RATE_WIDTH, theme).when(!closed, |cell| {
        cell.child(rate_line("↑", connection.uplink))
            .child(rate_line("↓", connection.downlink))
    });

    let traffic = stacked_cell(TRAFFIC_WIDTH, theme)
        .text_color(muted)
        .child(format!("↑ {}", format_bytes(connection.uplink_total)))
        .child(format!("↓ {}", format_bytes(connection.downlink_total)));

    let age = div()
        .flex_none()
        .w(px(AGE_WIDTH))
        .text_right()
        .text_xs()
        .text_color(muted)
        .child(format_elapsed(connection_age_ms(connection, now_ms)));

    let close_slot = div().flex_none().w(px(CLOSE_WIDTH)).when(!closed, |slot| {
        let connections = connections.clone();
        let id = connection.id.clone();
        slot.child(
            Button::new(SharedString::from(format!("conn-close-{}", connection.id)))
                .ghost()
                .xsmall()
                .icon(IconName::Close)
                .tooltip(s().connections.close_connection)
                .on_click(move |_, _, cx| {
                    // Not a click on the row: that would open the details.
                    cx.stop_propagation();
                    connections.update(cx, |state, cx| state.close(id.clone(), cx));
                }),
        )
    });

    let id = SharedString::from(connection.id.clone());
    let page = page.clone();
    let accent = theme.primary;
    div()
        .id(SharedString::from(format!("conn-row-{}", connection.id)))
        .relative()
        .h(px(ROW_HEIGHT))
        .w_full()
        .px_3()
        .h_flex()
        .items_center()
        .gap(px(COLUMN_GAP))
        .rounded(px(8.))
        .border_1()
        .cursor_pointer()
        .map(|row| {
            if selected {
                // Raised like the reference's flagged row: a tinted band,
                // its outline, and a bar down its leading edge.
                row.bg(accent.opacity(0.10))
                    .border_color(accent.opacity(0.45))
                    .child(
                        div()
                            .absolute()
                            .left(px(-1.))
                            .top(px(7.))
                            .bottom(px(7.))
                            .w(px(3.))
                            .rounded_full()
                            .bg(accent),
                    )
            } else {
                row.border_color(transparent_black())
                    .hover(move |style| style.bg(hover_bg))
            }
        })
        .on_click(move |_, window, cx| {
            page.update(cx, |page, cx| page.select(id.clone(), window, cx))
                .ok();
        })
        .child(time)
        .child(badge)
        .child(host)
        .child(chain)
        .child(rate)
        .child(traffic)
        .child(age)
        .child(close_slot)
}

/// A fixed-width, right-aligned cell of two small mono lines (up over
/// down).
fn stacked_cell(width: f32, theme: &Theme) -> Div {
    div()
        .flex_none()
        .w(px(width))
        .v_flex()
        .items_end()
        .whitespace_nowrap()
        .text_xs()
        .line_height(px(STACKED_LINE))
        .font_family(theme.mono_font_family.clone())
}

/// The list's column headings, on the rows' columns.
fn column_header(theme: &Theme) -> Div {
    let t = &s().connections;
    let heading = |text: &'static str| div().whitespace_nowrap().child(text);
    div()
        .flex_none()
        .h(px(32.))
        .w_full()
        // The rows' padding and their (transparent) border.
        .px(px(13.))
        .h_flex()
        .items_center()
        .gap(px(COLUMN_GAP))
        .text_xs()
        .font_weight(FontWeight::MEDIUM)
        .text_color(theme.muted_foreground)
        .border_b_1()
        .border_color(theme.border)
        .child(heading(t.col_time).flex_none().w(px(TIME_WIDTH)))
        .child(heading(t.col_network).flex_none().w(px(NETWORK_WIDTH)))
        .child(
            heading(t.col_host)
                .flex_grow(2.)
                .flex_basis(px(0.))
                .min_w(px(80.)),
        )
        .child(
            heading(t.col_chain)
                .flex_grow(1.)
                .flex_basis(px(0.))
                .min_w_0()
                .overflow_hidden(),
        )
        .child(
            heading(t.col_speed)
                .flex_none()
                .w(px(RATE_WIDTH))
                .text_right(),
        )
        .child(
            heading(t.col_traffic)
                .flex_none()
                .w(px(TRAFFIC_WIDTH))
                .text_right(),
        )
        .child(
            heading(t.col_duration)
                .flex_none()
                .w(px(AGE_WIDTH))
                .text_right(),
        )
        .child(div().flex_none().w(px(CLOSE_WIDTH)))
}

impl Render for ConnectionsPage {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.refresh_rows(cx);
        let page = cx.entity().downgrade();
        // Built before `theme` borrows `cx`: the dropdown keeps its state in
        // the window.
        let sort_select = {
            let t = &s().connections;
            let labels = [t.newest, t.traffic, t.speed, t.host, t.rule, t.chain];
            let page = page.clone();
            choice_select(
                "connections-sort",
                ConnectionSort::ALL.into_iter().zip(labels),
                self.sort,
                move |sort, _, cx| {
                    page.update(cx, |this, cx| this.set_sort(sort, cx)).ok();
                },
                window,
                cx,
            )
        };
        let connections = self.connections.clone();
        let state = connections.read(cx);
        let live = state.live;
        let summary = summarize(state.table.iter());
        let narrowed = self
            .rows_key
            .as_ref()
            .is_some_and(|key| key.filter().narrows());
        let hide_direct = self.rows_key.as_ref().is_some_and(|key| key.hide_direct);
        let theme = cx.theme();
        let t = &s().connections;

        // Nothing to filter, switch or close (sing-box stopped, or no
        // connection yet, open or closed): the toolbar, the totals and
        // Close all stay out of the way of the empty state.
        let has_any = live && summary.open + summary.closed > 0;

        // Live totals under the title: open count (how many of the view's
        // rows the filter shows, while it narrows them), current rates,
        // and the traffic so far — three groups set apart by space. On a
        // narrow window the totals give way first, then the rates; the
        // count stays whole.
        let count = if narrowed {
            let in_view = match self.view {
                ConnectionView::Active => summary.open,
                ConnectionView::Closed => summary.closed,
            };
            (t.shown_of)(self.rows.len(), in_view)
        } else {
            (t.open_count)(summary.open as u64)
        };
        let summary_items = div()
            .h_flex()
            .items_center()
            .gap_4()
            .min_w_0()
            .text_sm()
            .text_color(theme.muted_foreground)
            .child(div().flex_shrink_0().whitespace_nowrap().child(count))
            .child(clipped(format!(
                "↑ {}  ↓ {}",
                format_speed(summary.up_rate),
                format_speed(summary.down_rate)
            )))
            .child(
                clipped(format!(
                    "{} ↑ {}  ↓ {}",
                    t.total,
                    format_bytes(summary.up_total),
                    format_bytes(summary.down_total)
                ))
                .flex_shrink(SHRINK_FIRST),
            );
        // Under a filter, only the open connections it matches.
        let close_all = {
            let connections = connections.clone();
            let targets = self.close_targets.clone();
            let (label, none) = match &*targets {
                CloseTargets::All => (SharedString::from(t.close_all), summary.open == 0),
                CloseTargets::Matching(ids) => (
                    SharedString::from((t.close_matching)(ids.len() as u64)),
                    ids.is_empty(),
                ),
            };
            Button::new("connections-close-all")
                .outline()
                .small()
                .text_label(label)
                .disabled(none)
                .on_click(move |_, _, cx| {
                    connections.update(cx, |state, cx| match &*targets {
                        CloseTargets::All => state.close_all(cx),
                        CloseTargets::Matching(ids) => state.close_many(ids.clone(), cx),
                    });
                })
        };

        // One line, even beside the expanded sidebar in the narrowest
        // window: the search box gives way (to its minimum) rather than
        // the switches wrapping under it.
        let mut controls = div()
            .h_flex()
            .items_center()
            .gap_2()
            .w_full()
            .child(toolbar_search(
                form_input(&self.filter_input).cleanable(true).prefix(
                    Icon::new(IconName::Search)
                        .small()
                        .text_color(theme.muted_foreground),
                ),
            ))
            // Switches to the right, under the header's actions.
            .child(div().flex_1());
        let hide_page = page.clone();
        controls = controls.child(
            div().flex_none().child(
                Button::new("connections-hide-direct")
                    .outline()
                    .small()
                    .selected(hide_direct)
                    .toggled(hide_direct)
                    .icon_label(IconName::EyeOff, t.hide_direct)
                    .tooltip(t.hide_direct_hint)
                    .on_click(move |_, _, cx| {
                        hide_page
                            .update(cx, |this, cx| this.toggle_hide_direct(cx))
                            .ok();
                    }),
            ),
        );
        const VIEWS: [ConnectionView; 2] = [ConnectionView::Active, ConnectionView::Closed];
        let view_page = page.clone();
        controls = controls.child(segmented(
            theme,
            "connections-view",
            vec![
                Segment::new(t.active_tab).count(summary.open),
                Segment::new(t.closed_tab).count(summary.closed),
            ],
            VIEWS.iter().position(|view| *view == self.view),
            move |ix, _, cx| {
                view_page
                    .update(cx, |this, cx| this.set_view(VIEWS[ix], cx))
                    .ok();
            },
        ));
        // The sort key's menu, then its direction, on one track.
        let reverse_page = page.clone();
        let (direction_icon, direction_tip) = match self.direction {
            SortDirection::Ascending => ("icons/arrow-down-narrow-wide.svg", t.sort_ascending),
            SortDirection::Descending => ("icons/arrow-down-wide-narrow.svg", t.sort_descending),
        };
        controls = controls.child(
            div()
                .flex_none()
                .h_flex()
                .items_center()
                .gap_0p5()
                .h(px(28.))
                .pl_2p5()
                .pr_0p5()
                .rounded(theme.radius)
                .bg(theme.tab_bar_segmented)
                .child(sort_select)
                .child(
                    Button::new("connections-sort-direction")
                        .ghost()
                        .xsmall()
                        .icon(Icon::empty().path(direction_icon))
                        .tooltip(direction_tip)
                        .on_click(move |_, _, cx| {
                            reverse_page
                                .update(cx, |this, cx| this.reverse_sort(cx))
                                .ok();
                        }),
                ),
        );

        let mut head = page_header(theme, ActivePage::Connections);
        if has_any {
            head = head.context(summary_items).action(close_all);
        }

        // The empty state goes on the page's root (see `empty_state`), the
        // list in the body under the header.
        let (empty, list) = if !live {
            let empty = empty_state(theme, IconName::Network, t.empty_title, t.empty_hint)
                .action(connect_button("connections-connect"));
            (Some(empty), None)
        } else if self.rows.is_empty() {
            let (title, hint) = match (narrowed, self.view) {
                // Nothing at all yet (the toolbar is hidden): whatever the
                // view and filter left from before, wait for the first one.
                _ if !has_any => (t.no_active_title, t.no_active_hint),
                (true, _) => (t.no_match_title, t.no_match_hint),
                (false, ConnectionView::Active) => (t.no_active_title, t.no_active_hint),
                (false, ConnectionView::Closed) => (t.no_closed_title, t.no_closed_hint),
            };
            let empty = empty_state(theme, IconName::Network, title, hint);
            (Some(empty), None)
        } else {
            let rows = self.rows.clone();
            let list_connections = connections.clone();
            let list_page = page.clone();
            let selected = self.selected.clone();
            let list = uniform_list("connections-list", rows.len(), move |range, _, cx| {
                let now_ms = unix_millis_now();
                let networks = NetworkColors::new(cx.theme());
                let theme = cx.theme();
                let state = list_connections.read(cx);
                range
                    .map(|ix| match state.table.get(&rows[ix]) {
                        Some(connection) => connection_row(
                            connection,
                            now_ms,
                            &list_connections,
                            &list_page,
                            selected.as_deref() == Some(rows[ix].as_str()),
                            networks,
                            theme,
                        )
                        .into_any_element(),
                        // Ids are refreshed with every revision before the
                        // list renders, so this is only a defensive blank.
                        None => div().h(px(ROW_HEIGHT)).into_any_element(),
                    })
                    .collect::<Vec<_>>()
            })
            .track_scroll(&self.scroll)
            .size_full();

            let list = div()
                .v_flex()
                .flex_1()
                .min_h_0()
                .child(column_header(theme))
                .child(
                    div()
                        .relative()
                        .flex_1()
                        .min_h_0()
                        .pt_1()
                        .child(list)
                        .vertical_scrollbar(&self.scroll),
                );
            (None, Some(list))
        };

        // The details panel overlays the list's right side, below the
        // filter row (which stays usable). A cached view of its own: it
        // re-renders only when its connection's shown fields change.
        let details = self.selected.is_some().then(|| {
            let panel =
                AnyView::from(self.details.clone()).cached(StyleRefinement::default().size_full());
            div()
                .absolute()
                .top_0()
                .right_0()
                .bottom_0()
                .w(px(DETAILS_WIDTH))
                .max_w(relative(DETAILS_MAX_FRACTION))
                .occlude()
                .child(panel)
                .with_animation(
                    ElementId::NamedInteger("conn-details-slide".into(), self.details_opened),
                    Animation::new(DETAILS_SLIDE).with_easing(ease_out_quint()),
                    |panel, delta| {
                        panel
                            .right(px(-(1. - delta) * DETAILS_SLIDE_PX))
                            .opacity(delta)
                    },
                )
        });
        let body = div()
            .relative()
            .flex_1()
            .min_h_0()
            .v_flex()
            .children(list)
            .children(details);

        let body = div()
            .id("connections-page")
            .track_focus(&self.focus_handle)
            .when(self.selected.is_some(), |page| {
                page.key_context(CONNECTION_DETAILS_CONTEXT)
            })
            .on_action(cx.listener(Self::on_close_details))
            .on_action(cx.listener(Self::on_select_previous))
            .on_action(cx.listener(Self::on_select_next))
            .v_flex()
            .size_full()
            .gap_4()
            .when(has_any, |page| page.child(controls))
            // Before the body, so the details panel stays above it.
            .children(empty)
            .child(body);
        page_layout(head, body)
    }
}
