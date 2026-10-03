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

use crate::core::bytefmt::{format_bytes, format_speed};
use crate::core::connections_view::{
    chain_label, connection_age_ms, format_elapsed, host_label, inbound_label, network_label,
    process_name, rule_label, select_connections, summarize, ConnectionSort, ConnectionView,
};
use crate::core::singbox_api::Connection;
use crate::i18n::s;
use crate::state::{AppState, Connections};
use crate::ui::widgets::{empty_card, page_header};
use crate::ui::{card_frame, locale};
use gpui::prelude::FluentBuilder;
use gpui::*;
use gpui_component::{
    button::{Button, ButtonVariants},
    input::{Input, InputEvent, InputState},
    scroll::ScrollableElement,
    theme::Theme,
    ActiveTheme, Disableable, Icon, IconName, Sizable, StyledExt,
};
use std::rc::Rc;
use std::time::{SystemTime, UNIX_EPOCH};

/// Fixed row height — `uniform_list` lays every row out at the first row's
/// size, so all rows must match.
const ROW_HEIGHT: f32 = 52.;

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
}

impl ConnectionsPage {
    pub fn new(app_state: Entity<AppState>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let connections = app_state.read(cx).connections.clone();
        cx.observe(&connections, |_, _, cx| cx.notify()).detach();

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
    }
}

fn unix_millis_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Single-line text that ellipsizes instead of wrapping.
fn clipped(text: impl Into<SharedString>) -> Div {
    div()
        .min_w_0()
        .overflow_hidden()
        .text_ellipsis()
        .whitespace_nowrap()
        .child(text.into())
}

fn toggle_pill(
    id: &'static str,
    label: impl Into<SharedString>,
    active: bool,
    on_click: impl Fn(&mut Window, &mut App) + 'static,
) -> Button {
    let button = Button::new(id).label(label).small();
    let button = if active {
        button.primary()
    } else {
        button.ghost()
    };
    button.on_click(move |_, window, cx| on_click(window, cx))
}

/// One list row. Every row has the same structure (closed rows keep an
/// empty slot where the close button goes) so heights stay uniform.
fn connection_row(
    connection: &Connection,
    now_ms: i64,
    connections: &Entity<Connections>,
    theme: &Theme,
) -> Stateful<Div> {
    let closed = connection.is_closed();
    let hover_bg = theme.muted.opacity(0.5);

    let mut title = div().h_flex().items_center().gap_2().child(
        clipped(host_label(connection))
            .flex_shrink(1.)
            .text_sm()
            .text_color(if closed {
                theme.muted_foreground
            } else {
                theme.foreground
            }),
    );
    if let Some(process) = process_name(connection) {
        title = title.child(
            clipped(process.to_string())
                .flex_shrink_0()
                .max_w(px(160.))
                .text_xs()
                .text_color(theme.muted_foreground),
        );
    }

    let details = format!(
        "{} · {} · {}",
        network_label(connection),
        inbound_label(connection),
        rule_label(connection)
    );
    let subtitle = div()
        .h_flex()
        .items_center()
        .gap_2()
        .text_xs()
        .child(
            clipped(chain_label(connection))
                .flex_shrink_0()
                .max_w(relative(0.55))
                .text_color(if closed {
                    theme.muted_foreground
                } else {
                    theme.primary
                }),
        )
        .child(clipped(details).flex_1().text_color(theme.muted_foreground));

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
                    connections.update(cx, |state, cx| state.close(id.clone(), cx));
                }),
        )
    });

    div()
        .id(SharedString::from(format!("conn-row-{}", connection.id)))
        .h(px(ROW_HEIGHT))
        .w_full()
        .px_3()
        .h_flex()
        .items_center()
        .gap_3()
        .border_b_1()
        .border_color(theme.border)
        .hover(move |style| style.bg(hover_bg))
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

        let summary_label = format!(
            "{} · ↑ {}  ↓ {} · {} ↑ {}  ↓ {}",
            (t.open_count)(summary.open as u64),
            format_speed(summary.up_rate),
            format_speed(summary.down_rate),
            t.total,
            format_bytes(summary.up_total),
            format_bytes(summary.down_total),
        );
        let title_block = div()
            .h_flex()
            .items_center()
            .gap_2()
            .min_w_0()
            .child(page_header(theme, t.title))
            .when(live, |this| {
                this.child(
                    clipped(summary_label)
                        .text_sm()
                        .text_color(theme.muted_foreground),
                )
            });
        let close_all = {
            let connections = connections.clone();
            Button::new("connections-close-all")
                .outline()
                .small()
                .label(t.close_all)
                .disabled(!live || summary.open == 0)
                .on_click(move |_, _, cx| {
                    connections.update(cx, |state, cx| state.close_all(cx));
                })
        };
        let header = div()
            .h_flex()
            .items_center()
            .justify_between()
            .gap_2()
            .w_full()
            .child(title_block)
            .child(close_all);

        let page = cx.entity().downgrade();
        let mut controls = div()
            .h_flex()
            .flex_wrap()
            .items_center()
            .gap_2()
            .w_full()
            .child(
                div().flex_1().min_w(px(200.)).child(
                    Input::new(&self.filter_input)
                        .small()
                        .cleanable(true)
                        .prefix(
                            Icon::new(IconName::Search)
                                .small()
                                .text_color(theme.muted_foreground),
                        ),
                ),
            );
        for (id, label, view) in [
            (
                "connections-active",
                (t.active_tab)(summary.open as u64),
                ConnectionView::Active,
            ),
            (
                "connections-closed",
                (t.closed_tab)(summary.closed as u64),
                ConnectionView::Closed,
            ),
        ] {
            let page = page.clone();
            controls = controls.child(toggle_pill(id, label, self.view == view, move |_, cx| {
                page.update(cx, |this, cx| this.set_view(view, cx)).ok();
            }));
        }
        controls = controls.child(div().w_px().h_4().bg(theme.border).mx_1());
        for (id, label, sort) in [
            ("connections-newest", t.newest, ConnectionSort::Newest),
            ("connections-traffic", t.traffic, ConnectionSort::Traffic),
        ] {
            let page = page.clone();
            controls = controls.child(toggle_pill(id, label, self.sort == sort, move |_, cx| {
                page.update(cx, |this, cx| this.set_sort(sort, cx)).ok();
            }));
        }

        let body = if !live {
            empty_card(theme, IconName::Network, t.empty_title, t.empty_hint).into_any_element()
        } else if self.rows.is_empty() {
            let (title, hint) = match (query_empty, self.view) {
                (false, _) => (t.no_match_title, t.no_match_hint),
                (true, ConnectionView::Active) => (t.no_active_title, t.no_active_hint),
                (true, ConnectionView::Closed) => (t.no_closed_title, t.no_closed_hint),
            };
            empty_card(theme, IconName::Network, title, hint).into_any_element()
        } else {
            let rows = self.rows.clone();
            let list_connections = connections.clone();
            let list = uniform_list("connections-list", rows.len(), move |range, _, cx| {
                let now_ms = unix_millis_now();
                let theme = cx.theme();
                let state = list_connections.read(cx);
                range
                    .map(|ix| match state.table.get(&rows[ix]) {
                        Some(connection) => {
                            connection_row(connection, now_ms, &list_connections, theme)
                                .into_any_element()
                        }
                        // Ids are refreshed with every revision before the
                        // list renders, so this is only a defensive blank.
                        None => div().h(px(ROW_HEIGHT)).into_any_element(),
                    })
                    .collect::<Vec<_>>()
            })
            .track_scroll(&self.scroll)
            .size_full();

            card_frame(theme)
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
                )
                .into_any_element()
        };

        div()
            .v_flex()
            .size_full()
            .gap_4()
            .child(header)
            .child(controls)
            .child(body)
    }
}
