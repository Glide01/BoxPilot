//! 连接页:标题行(标题 + 汇总:打开数、实时速率、总流量 + Close all)、
//! 控制行(过滤输入框 + Active/Closed 切换 + Newest/Traffic 排序),下方卡片
//! 里是连接列表。每行两行文字:主机(域名或目标地址)+ 进程名;出站链
//! (组 → 节点)+ 网络/协议 · 入站 · 规则;右侧实时速率 / 累计流量、存活时长、
//! 关闭按钮(仅打开的连接)。
//!
//! 列表用 gpui 的 `uniform_list` 虚拟化:只渲染可见行,上千条已关闭 + 大量
//! 打开的连接也不卡。过滤 / 排序结果(按 id 的有序列表)按
//! `(revision, view, sort, query)` 缓存,只有数据或条件变了才重算;行内容在
//! 渲染时按 id 从 `ConnectionTable` 现取,时长随渲染时刻走。
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
    chain_label, connection_age_ms, format_elapsed, host_label, inbound_label, network_label,
    process_name, rule_label, select_connections, summarize, ConnectionSort, ConnectionView,
};
use crate::core::singbox_api::Connection;
use crate::i18n::s;
use crate::state::{AppState, Connections};
use crate::ui::widgets::{
    connect_button, empty_state, full_text_tooltip, page_header, row_hover_bg, segmented,
    small_input, Segment, TextLabel,
};
use crate::ui::{card_frame, locale};
use gpui::prelude::FluentBuilder;
use gpui::*;
use gpui_component::{
    button::{Button, ButtonVariants},
    input::{InputEvent, InputState},
    scroll::ScrollableElement,
    theme::Theme,
    ActiveTheme, Disableable, Icon, IconName, Sizable, StyledExt,
};
use std::rc::Rc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Fixed row height — `uniform_list` lays every row out at the first row's
/// size, so all rows must match.
const ROW_HEIGHT: f32 = 52.;
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
    query: String,
}

pub struct ConnectionsPage {
    connections: Entity<Connections>,
    filter_input: Entity<InputState>,
    view: ConnectionView,
    sort: ConnectionSort,
    scroll: UniformListScrollHandle,
    /// Ids of the rows to show, in display order, and what they came from.
    rows: Rc<Vec<String>>,
    rows_key: Option<RowsKey>,
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
            connections,
            filter_input,
            view: ConnectionView::default(),
            sort: ConnectionSort::default(),
            scroll: UniformListScrollHandle::new(),
            rows: Rc::new(Vec::new()),
            rows_key: None,
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

    fn set_sort(&mut self, sort: ConnectionSort, cx: &mut Context<Self>) {
        if self.sort != sort {
            self.sort = sort;
            self.scroll.scroll_to_item(0, ScrollStrategy::Top);
            cx.notify();
        }
    }

    /// Recompute the row order only when the data or the view settings moved.
    fn refresh_rows(&mut self, cx: &App) {
        let state = self.connections.read(cx);
        let key = RowsKey {
            revision: state.revision,
            view: self.view,
            sort: self.sort,
            query: self.filter_input.read(cx).value().to_string(),
        };
        if self.rows_key.as_ref() == Some(&key) {
            return;
        }
        let ids = select_connections(state.table.iter(), key.view, &key.query, key.sort)
            .into_iter()
            .map(|c| c.id.clone())
            .collect();
        self.rows = Rc::new(ids);
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

/// One list row. Every row has the same structure (closed rows keep an
/// empty slot where the close button goes) so heights stay uniform.
fn connection_row(
    connection: &Connection,
    now_ms: i64,
    connections: &Entity<Connections>,
    page: &WeakEntity<ConnectionsPage>,
    selected: bool,
    theme: &Theme,
) -> Stateful<Div> {
    let closed = connection.is_closed();
    let hover_bg = row_hover_bg(theme);

    let mut title = div()
        .h_flex()
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
                .text_color(if closed {
                    theme.muted_foreground
                } else {
                    theme.foreground
                }),
            SharedString::from(format!("conn-host-{}", connection.id)),
            host_label(connection),
            HOST_ROOM,
        ));
    if let Some(process) = process_name(connection) {
        title = title.child(
            clipped(process.to_string())
                .flex_shrink_0()
                .max_w(px(160.))
                .text_xs()
                .text_color(theme.muted_foreground),
        );
    }

    // Chain first (accented while open), then network, inbound and rule,
    // set apart by space. Short of room (a narrow window), the rule gives
    // way first, then the chain; network and inbound are a word each and
    // stay whole rather than all four turning into a bare "…" each (the
    // details panel has them all in full).
    let subtitle = div()
        .h_flex()
        .items_center()
        .gap_3()
        .min_w_0()
        .overflow_hidden()
        .text_xs()
        .child(
            clipped(chain_label(connection))
                .flex_shrink(1.)
                .min_w(px(44.))
                .max_w(relative(0.5))
                .text_color(if closed {
                    theme.muted_foreground
                } else {
                    theme.primary
                }),
        )
        .child(
            div()
                .flex_shrink_0()
                .whitespace_nowrap()
                .text_color(theme.muted_foreground)
                .child(network_label(connection)),
        )
        .child(
            clipped(inbound_label(connection).to_string())
                .flex_shrink_0()
                .max_w(px(120.))
                .text_color(theme.muted_foreground),
        )
        .child(
            clipped(rule_label(connection).to_string())
                .flex_shrink(SHRINK_FIRST)
                .text_color(theme.muted_foreground),
        );

    let rate = if closed {
        s().connections.closed.to_string()
    } else {
        format!(
            "↑ {}  ↓ {}",
            format_speed(connection.uplink),
            format_speed(connection.downlink)
        )
    };
    let totals = format!(
        "↑ {}  ↓ {}",
        format_bytes(connection.uplink_total),
        format_bytes(connection.downlink_total)
    );
    let traffic = div()
        .v_flex()
        .flex_shrink_0()
        .w(px(170.))
        .items_end()
        .gap_0p5()
        .text_xs()
        .child(div().text_color(theme.foreground).child(rate))
        .child(div().text_color(theme.muted_foreground).child(totals));

    let age = div()
        .flex_shrink_0()
        .w(px(64.))
        .text_right()
        .text_xs()
        .text_color(theme.muted_foreground)
        .child(format_elapsed(connection_age_ms(connection, now_ms)));

    let close_slot = div().flex_shrink_0().w(px(28.)).when(!closed, |slot| {
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
    div()
        .id(SharedString::from(format!("conn-row-{}", connection.id)))
        .relative()
        .h(px(ROW_HEIGHT))
        .w_full()
        .px_3()
        .h_flex()
        .items_center()
        .gap_3()
        .border_b_1()
        .border_color(theme.border)
        .cursor_pointer()
        .when(selected, |row| {
            row.bg(theme.primary.opacity(0.08)).child(
                div()
                    .absolute()
                    .left_0()
                    .top_0()
                    .bottom_0()
                    .w(px(3.))
                    .bg(theme.primary),
            )
        })
        .when(!selected, |row| row.hover(move |style| style.bg(hover_bg)))
        .on_click(move |_, window, cx| {
            page.update(cx, |page, cx| page.select(id.clone(), window, cx))
                .ok();
        })
        .child(
            div()
                .v_flex()
                .flex_1()
                .min_w_0()
                .gap_0p5()
                .child(title)
                .child(subtitle),
        )
        .child(traffic)
        .child(age)
        .child(close_slot)
}

impl Render for ConnectionsPage {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.refresh_rows(cx);
        let connections = self.connections.clone();
        let state = connections.read(cx);
        let live = state.live;
        let summary = summarize(state.table.iter());
        let query_empty = self
            .rows_key
            .as_ref()
            .is_none_or(|k| k.query.trim().is_empty());
        let theme = cx.theme();
        let t = &s().connections;

        // Nothing to filter, switch or close (sing-box stopped, or no
        // connection yet, open or closed): the toolbar, the totals and
        // Close all stay out of the way of the empty state.
        let has_any = live && summary.open + summary.closed > 0;

        // Live totals beside the title: open count, current rates, and the
        // traffic so far — three groups set apart by space. On a narrow
        // window the totals give way first, then the rates; the count
        // stays whole.
        let summary_items = div()
            .h_flex()
            .items_center()
            .gap_4()
            .min_w_0()
            .text_sm()
            .text_color(theme.muted_foreground)
            .child(
                div()
                    .flex_shrink_0()
                    .whitespace_nowrap()
                    .child((t.open_count)(summary.open as u64)),
            )
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
        let title_block = div()
            .h_flex()
            .items_center()
            .gap_4()
            .min_w_0()
            .child(page_header(theme, t.title))
            .when(has_any, |this| this.child(summary_items));
        let close_all = has_any.then(|| {
            let connections = connections.clone();
            Button::new("connections-close-all")
                .outline()
                .small()
                .text_label(t.close_all)
                .disabled(summary.open == 0)
                .on_click(move |_, _, cx| {
                    connections.update(cx, |state, cx| state.close_all(cx));
                })
        });
        let header = div()
            .h_flex()
            .items_center()
            .justify_between()
            .gap_2()
            .w_full()
            .child(title_block)
            .children(close_all);

        let page = cx.entity().downgrade();
        let mut controls = div()
            .h_flex()
            .flex_wrap()
            .items_center()
            .gap_2()
            .w_full()
            .child(
                div().flex_1().min_w(px(200.)).child(
                    small_input(&self.filter_input).cleanable(true).prefix(
                        Icon::new(IconName::Search)
                            .small()
                            .text_color(theme.muted_foreground),
                    ),
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
        const SORTS: [ConnectionSort; 2] = [ConnectionSort::Newest, ConnectionSort::Traffic];
        let sort_page = page.clone();
        controls = controls.child(segmented(
            theme,
            "connections-sort",
            vec![Segment::new(t.newest), Segment::new(t.traffic)],
            SORTS.iter().position(|sort| *sort == self.sort),
            move |ix, _, cx| {
                sort_page
                    .update(cx, |this, cx| this.set_sort(SORTS[ix], cx))
                    .ok();
            },
        ));

        // The empty state goes on the page's root (see `empty_state`), the
        // list in the body under the header.
        let (empty, list) = if !live {
            let empty = empty_state(theme, IconName::Network, t.empty_title, t.empty_hint)
                .action(connect_button("connections-connect"));
            (Some(empty), None)
        } else if self.rows.is_empty() {
            let (title, hint) = match (query_empty, self.view) {
                // Nothing at all yet (the toolbar is hidden): whatever the
                // view and filter left from before, wait for the first one.
                _ if !has_any => (t.no_active_title, t.no_active_hint),
                (false, _) => (t.no_match_title, t.no_match_hint),
                (true, ConnectionView::Active) => (t.no_active_title, t.no_active_hint),
                (true, ConnectionView::Closed) => (t.no_closed_title, t.no_closed_hint),
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

            let list = card_frame(theme)
                .flex_1()
                .min_h_0()
                .p_0()
                .gap_0()
                .overflow_hidden()
                .child(
                    div()
                        .relative()
                        .size_full()
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

        div()
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
            .child(header)
            .when(has_any, |page| page.child(controls))
            // Before the body, so the details panel stays above it.
            .children(empty)
            .child(body)
    }
}
