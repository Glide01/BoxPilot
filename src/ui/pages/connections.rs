//! 连接页,日志浏览器式布局(与日志页一致):控制行(搜索框 + Hide direct
//! 快速过滤开关(隐藏直连 / 拦截 / DNS 连接,规则见
//! `connections_view::is_direct_or_dns`;开关记在设置
//! `connections_hide_direct` 里)+ Active/Closed 切换 + 排序:下拉选排序键
//! Newest/Traffic/Speed/Host/Rule/Chain,旁边的按钮切换升序 / 降序;换键时
//! 回到该键的自然方向,数值与时间大的在前、文字 A→Z;控制行不折行,窄时
//! 搜索框让位);页头一行汇总(打开数 —— 有过滤时换成「N of M shown」、实时
//! 速率、总流量;暂停时前面加 Paused 徽标)、Pause / Resume 和 Close all
//! (过滤让一部分打开的连接不显示时变成「Close N matching」,只关闭过滤后
//! 剩下的打开连接:在一个后台任务里逐个 `close`);下面是带表头的连接表,
//! 每行一行,列由 `core::connection_columns` 决定(列的取舍和宽度记在设置
//! `connections_columns` 里)。默认六列:建立时间(等宽,毫秒部分淡色)、
//! 主机(前面是网络徽标 TCP/UDP,与柱状图同色;后面是进程名)、出站链
//! (组 → 节点)、实时速率与累计流量(各两行:上行在上、下行在下;已关闭的
//! 连接没有速率)、存活时长;另有网络(徽标 + 嗅探到的协议)、目标地址、
//! 进程、来源、入站、规则可以打开 —— 网络或进程有了自己的列,主机格里就
//! 不再重复徽标或进程名。最后是关闭按钮(仅打开的连接),它不是一列,总在。
//! 控制行最右的 Columns 按钮(右键表头也一样)打开列菜单:勾选显示 / 隐藏
//! 各列,另有 Reset columns 恢复默认的列和宽度;主机和出站链(基本信息)
//! 至少留一列,只剩一列时它的菜单项不可点,悬停说明原因。排序与列显示无关,
//! 隐藏的列照样能当排序键。
//!
//! 列宽(`layout_columns`,列表宽度在绘制时量出,变了下一帧重排):其余列
//! 用用户给的宽度,主机和出站链按权重(默认 5 : 4)分剩下的宽度,不小于各自
//! 的最小宽度;不够时建立时间先去掉毫秒(只按宽度决定格式,不会自己藏起
//! 来),再让其余列向各自的最小宽度收,还不够才整表横向滚动(表头和行一起
//! 滚)。主机和出站链在两帧之间按比例跟着窗口宽度走,表头和行用同一份宽度,
//! 始终对齐。表头相邻两列之间的缝是拖动手柄(`drag_boundary`):拖动时缝
//! 跟着指针走,远离主机 / 出站链那一侧的列变宽变窄,另一侧最近的主机或出站
//! 链让出 / 收回宽度;双击手柄恢复那一列的默认宽度。拖动中的宽度只画在页面
//! 上(`ColumnDrag`),松开鼠标才写进设置。出站链放不下时前面的组先省略、
//! 节点留着(`节点选… → 香港-01`);文字列可能被截断时悬停显示全文。
//!
//! 列表用 gpui 的 `uniform_list` 虚拟化:只渲染可见行,上千条已关闭 + 大量
//! 打开的连接也不卡。过滤 / 排序结果(按 id 的有序列表)按
//! `(数据来源, view, sort, direction, query, hide_direct)` 缓存,Close all
//! 的目标按 `(revision, query, hide_direct)` 缓存,只有数据或条件变了才
//! 重算;行内容在渲染时按 id 从 `ConnectionTable` 现取,时长随渲染时刻走。
//! 过滤 / 排序 / 显示字符串都是 `core::connections_view` 的纯函数。
//!
//! Pause 把当时的 `ConnectionTable` 复制一份(连同时刻)冻结在页面状态里
//! (`Frozen`,不持久化),它就是列表的数据来源:哪些行、顺序、每行的值和
//! 时长、两个视图的计数都来自这份副本,切视图 / 排序 / 过滤也在副本上重算;
//! Resume 丢掉副本回到实时数据,sing-box 停止时也一样。页头汇总、Close all
//! 的目标和详情面板始终跟实时数据走;暂停期间关闭了的连接(比如点了行上的
//! 关闭按钮)那一行变成已关闭的样子,冻结的数值不变。
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
use crate::core::connection_columns::{
    boundary_target, drag_boundary, layout_columns, time_shows_millis, ColumnId, ColumnLayout,
    ColumnSettings, PlacedColumn, CLOSE_WIDTH, COLUMN_GAP, ROW_INSET,
};
use crate::core::connection_details::{step_selection, Step};
use crate::core::connections_view::{
    chain_hops, close_targets, connection_age_ms, format_elapsed, host_label, inbound_label,
    process_name, rule_label, select_connections, summarize, CloseTargets, ConnectionFilter,
    ConnectionSort, ConnectionView, SortDirection, CHAIN_SEPARATOR,
};
use crate::core::singbox_api::{Connection, ConnectionTable};
use crate::core::timefmt::format_clock_ms;
use crate::i18n::s;
use crate::state::{AppState, Connections};
use crate::ui::locale;
use crate::ui::pages::ActivePage;
use crate::ui::widgets::{
    choice_select, connect_button, control_input, empty_state, full_text_tooltip, may_truncate,
    page_header, page_layout, page_scrollbar, row_hover_bg, segmented, tag_badge, toolbar_search,
    warn_orange, Control, ControlSize, IconLabel, Segment, TextLabel,
};
use gpui::prelude::FluentBuilder;
use gpui::*;
use gpui_component::{
    button::{Button, ButtonVariants},
    input::{InputEvent, InputState},
    menu::{ContextMenuExt, DropdownMenu, PopupMenu, PopupMenuItem},
    scroll::ScrollableElement,
    theme::Theme,
    tooltip::Tooltip,
    ActiveTheme, Disableable, Icon, IconName, Selectable, Sizable, StyledExt,
};
use std::rc::Rc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Fixed row height — `uniform_list` lays every row out at the first row's
/// size, so all rows must match.
const ROW_HEIGHT: f32 = 36.;
/// The column headings' row.
const HEADER_HEIGHT: f32 = 32.;
/// Before the first frame has measured the list: about the list in the
/// window BoxPilot opens at, corrected on the next frame.
const GUESSED_LIST_WIDTH: f32 = 870.;
/// The grab band of a column boundary in the header: the gap between two
/// headings.
const RESIZE_HANDLE_WIDTH: f32 = COLUMN_GAP;
/// The TCP / UDP badge (in front of the host while the Network column is
/// hidden).
const NETWORK_BADGE_WIDTH: f32 = 34.;
/// Line height in the stacked cells: two lines inside `ROW_HEIGHT`.
const STACKED_LINE: f32 = 14.;
/// Generous average advance of one Latin letter (a CJK one counts two,
/// see `widgets::may_truncate`) in the host's font and in the small one of
/// the other text columns, to tell from a column's width whether its text
/// may be cut short.
const HOST_LETTER: f32 = 8.5;
const SMALL_LETTER: f32 = 7.;
/// The details panel's width; on a narrow window it takes most of the list
/// (`DETAILS_MAX_FRACTION`), leaving the selected row's start in view.
const DETAILS_WIDTH: f32 = 380.;
const DETAILS_MAX_FRACTION: f32 = 0.86;
/// The panel's slide-in.
const DETAILS_SLIDE: Duration = Duration::from_millis(180);
const DETAILS_SLIDE_PX: f32 = 28.;

/// Where the listed rows come from: the live table at a revision, or the
/// copy frozen by a pause.
#[derive(Clone, Copy, PartialEq)]
enum RowsSource {
    Live(u64),
    Frozen(u64),
}

/// While paused: the table as it was, and the moment, so rows (ages
/// included) read as they did then. `pause` tells one pause from the next.
struct Frozen {
    table: Rc<ConnectionTable>,
    now_ms: i64,
    pause: u64,
}

/// A column boundary being dragged: which one, where the pointer went
/// down, the columns and their layout then (each step is computed from
/// those, so nothing drifts), and the columns as the drag has them now —
/// drawn, but only saved to the settings when the button comes up.
struct ColumnDrag {
    boundary: usize,
    start_x: Pixels,
    settings: ColumnSettings,
    layout: ColumnLayout,
    current: ColumnSettings,
}

/// The value gpui carries while a column boundary is dragged; it draws
/// nothing (the columns themselves follow the pointer).
struct ColumnResize;

impl Render for ColumnResize {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        Empty
    }
}

/// What every row needs to know about the columns: their layout, and
/// whether Network and Process have columns of their own (then the host
/// cell leaves out its badge and the process name).
struct RowColumns {
    layout: ColumnLayout,
    network_column: bool,
    process_column: bool,
}

impl RowColumns {
    fn new(layout: ColumnLayout) -> Self {
        let shown = |id| layout.width_of(id).is_some();
        Self {
            network_column: shown(ColumnId::Network),
            process_column: shown(ColumnId::Process),
            layout,
        }
    }
}

/// Size a header or row cell as its column: a fixed column at its width;
/// Host and Chain share the rest of the row in proportion to their widths,
/// which come to exactly those widths at the list width they were laid out
/// for, and stay in step while the window resizes before the next layout.
fn column_cell(cell: Div, column: PlacedColumn) -> Div {
    if column.id.is_flexible() {
        cell.flex_basis(px(0.))
            .flex_grow(column.width)
            .flex_shrink_0()
            .min_w(px(column.id.min_width()))
    } else {
        cell.flex_none().w(px(column.width))
    }
}

/// Latin letters that surely fit `width` at `letter` px each.
fn room(width: f32, letter: f32) -> usize {
    (width / letter).max(0.) as usize
}

/// What the cached row order was derived from.
#[derive(Clone, PartialEq)]
struct RowsKey {
    source: RowsSource,
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
    /// What Close all closes under the same filter: always from the live
    /// table, paused or not, and what it was derived from (live revision,
    /// query, quick filter).
    close_targets: Rc<CloseTargets>,
    targets_key: Option<(u64, String, bool)>,
    /// `Some` while the list is paused (Pause / Resume).
    frozen: Option<Frozen>,
    /// Pauses so far.
    pauses: u64,
    /// The list's width as last drawn (`None` before the first frame),
    /// which the columns are laid out for (`layout_columns`).
    list_width: Option<Pixels>,
    /// Sideways, when the shown columns' minimums don't fit the list.
    h_scroll: ScrollHandle,
    /// `Some` while a column boundary is being dragged.
    column_drag: Option<ColumnDrag>,
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
            // sing-box stopped: the list is gone, and the panel and any
            // pause with it.
            if !connections.read(cx).live {
                this.frozen = None;
                if this.selected.is_some() {
                    this.close_details(cx);
                }
            }
            cx.notify();
        })
        .detach();

        let proxy_groups = app_state.read(cx).proxy_groups.clone();
        let details =
            cx.new(|cx| ConnectionDetailsPanel::new(connections.clone(), proxy_groups, cx));
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
            targets_key: None,
            frozen: None,
            pauses: 0,
            list_width: None,
            h_scroll: ScrollHandle::new(),
            column_drag: None,
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

    /// Pause: freeze the list as it is (which rows, their order and their
    /// values) until Resume. The details panel stays live.
    fn toggle_pause(&mut self, cx: &mut Context<Self>) {
        if self.frozen.take().is_none() {
            self.pauses += 1;
            self.frozen = Some(Frozen {
                table: Rc::new(self.connections.read(cx).table.clone()),
                now_ms: unix_millis_now(),
                pause: self.pauses,
            });
        }
        cx.notify();
    }

    /// The columns in effect: the settings', or a drag's while it lasts.
    fn columns(&self, cx: &App) -> ColumnSettings {
        match &self.column_drag {
            Some(drag) => drag.current.clone(),
            None => self.app_state.read(cx).settings.connections_columns.clone(),
        }
    }

    fn list_width(&self) -> f32 {
        self.list_width.map_or(GUESSED_LIST_WIDTH, f32::from)
    }

    /// The pointer went down on the boundary after column `boundary`: a
    /// drag may follow, or this is the second click of a double-click,
    /// which puts the column the boundary resizes back to its default
    /// width.
    fn press_column_boundary(
        &mut self,
        boundary: usize,
        event: &MouseDownEvent,
        cx: &mut Context<Self>,
    ) {
        let settings = self.columns(cx);
        let layout = layout_columns(&settings, self.list_width());
        if event.click_count >= 2 {
            self.column_drag = None;
            if let Some((target, _)) = boundary_target(&layout, boundary) {
                let mut reset = settings;
                reset.reset_width(layout.columns[target].id);
                self.app_state
                    .update(cx, |state, cx| state.set_connections_columns(reset, cx));
            }
            cx.notify();
            return;
        }
        self.column_drag = Some(ColumnDrag {
            boundary,
            start_x: event.position.x,
            current: settings.clone(),
            settings,
            layout,
        });
        cx.notify();
    }

    /// The pointer moved during the drag of `boundary`'s handle.
    fn drag_column_boundary(&mut self, boundary: usize, x: Pixels, cx: &mut Context<Self>) {
        let Some(drag) = self.column_drag.as_mut().filter(|d| d.boundary == boundary) else {
            return;
        };
        let delta = f32::from(x - drag.start_x);
        let next = drag_boundary(&drag.settings, &drag.layout, drag.boundary, delta);
        if next != drag.current {
            drag.current = next;
            cx.notify();
        }
    }

    /// The button came up: keep what the drag made.
    fn end_column_drag(&mut self, cx: &mut Context<Self>) {
        let Some(drag) = self.column_drag.take() else {
            return;
        };
        if drag.current != drag.settings {
            self.app_state.update(cx, |state, cx| {
                state.set_connections_columns(drag.current, cx)
            });
        }
        cx.notify();
    }

    /// Recompute the row order only when the data or the view settings
    /// moved, and Close all's targets only when the live data or the filter
    /// did.
    fn refresh_rows(&mut self, cx: &App) {
        let hide_direct = self.app_state.read(cx).settings.connections_hide_direct;
        let state = self.connections.read(cx);
        let source = match &self.frozen {
            Some(frozen) => RowsSource::Frozen(frozen.pause),
            None => RowsSource::Live(state.revision),
        };
        let key = RowsKey {
            source,
            view: self.view,
            sort: self.sort,
            direction: self.direction,
            query: self.filter_input.read(cx).value().to_string(),
            hide_direct,
        };
        let targets_key = (state.revision, key.query.clone(), hide_direct);
        if self.targets_key.as_ref() != Some(&targets_key) {
            self.close_targets = Rc::new(close_targets(state.table.iter(), key.filter()));
            self.targets_key = Some(targets_key);
        }
        if self.rows_key.as_ref() == Some(&key) {
            return;
        }
        let table = match &self.frozen {
            Some(frozen) => &*frozen.table,
            None => &state.table,
        };
        let ids = select_connections(
            table.iter(),
            key.view,
            key.filter(),
            key.sort,
            key.direction,
        )
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

/// Flex-shrink factor for the part of a line that should give way first
/// when it runs short (gpui shrinks in proportion to factor × width).
const SHRINK_FIRST: f32 = 100.;

/// Single-line text that ellipsizes instead of wrapping.
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

/// One list row: a cell per shown column (`RowColumns`), then the close
/// button's slot. Every row has the same structure (closed rows keep an
/// empty slot where the close button goes) so heights stay uniform.
/// `closed_since` marks a paused row whose connection has closed since the
/// pause (its Close button was used, say): it reads as closed, its frozen
/// figures kept.
#[allow(clippy::too_many_arguments)]
fn connection_row(
    connection: &Connection,
    closed_since: bool,
    now_ms: i64,
    connections: &Entity<Connections>,
    page: &WeakEntity<ConnectionsPage>,
    selected: bool,
    columns: &RowColumns,
    networks: NetworkColors,
    theme: &Theme,
) -> Stateful<Div> {
    let closed = connection.is_closed() || closed_since;
    let hover_bg = row_hover_bg(theme);
    let (fg, muted) = if closed {
        (theme.muted_foreground, theme.muted_foreground.opacity(0.7))
    } else {
        (theme.foreground, theme.muted_foreground)
    };
    let cell_id = |column: &str| SharedString::from(format!("conn-{column}-{}", connection.id));
    // A line of small muted text that ellipsizes, its whole text in a
    // tooltip when it may not fit.
    let small_text = |column: &str, text: String, width: f32| {
        full_text_tooltip(
            div()
                .min_w_0()
                .overflow_hidden()
                .text_ellipsis()
                .whitespace_nowrap()
                .text_xs()
                .text_color(muted),
            cell_id(column),
            text,
            room(width, SMALL_LETTER),
        )
    };

    // The network's badge: TCP blue, UDP orange.
    let network = connection.network.to_lowercase();
    let badge_color = networks.of(&network, theme);
    let badge = || {
        div()
            .flex_none()
            .w(px(NETWORK_BADGE_WIDTH))
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
            ))
    };

    let mut row = div()
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
        .cursor_pointer();

    for column in &columns.layout.columns {
        let width = column.width;
        let cell = match column.id {
            ColumnId::Time => {
                let (clock, fraction) = format_clock_ms(connection.created_at);
                div()
                    .h_flex()
                    .overflow_hidden()
                    .text_sm()
                    .font_family(theme.mono_font_family.clone())
                    .child(div().text_color(fg).child(clock))
                    .when(time_shows_millis(width), |time| {
                        time.child(div().text_color(muted).child(fraction))
                    })
            }
            // The badge, then the sniffed protocol if there was one and
            // it fits whole (a lone ellipsis says nothing).
            ColumnId::Network => div()
                .h_flex()
                .items_center()
                .gap_1p5()
                .overflow_hidden()
                .child(badge())
                .when(
                    !connection.protocol.is_empty()
                        && room(width - NETWORK_BADGE_WIDTH - 6., SMALL_LETTER)
                            >= connection.protocol.len(),
                    |cell| {
                        cell.child(
                            clipped(connection.protocol.clone())
                                .text_xs()
                                .text_color(muted),
                        )
                    },
                ),
            // The host, after its network's badge and before the process
            // that opened it, unless those have columns of their own.
            ColumnId::Host => {
                let badge_room = if columns.network_column {
                    0.
                } else {
                    NETWORK_BADGE_WIDTH + COLUMN_GAP
                };
                let mut host = div()
                    .h_flex()
                    .items_center()
                    .gap(px(COLUMN_GAP))
                    .when(!columns.network_column, |host| host.child(badge()))
                    .child(full_text_tooltip(
                        div()
                            .min_w_0()
                            .overflow_hidden()
                            .text_ellipsis()
                            .whitespace_nowrap()
                            .flex_shrink(1.)
                            .text_sm()
                            .text_color(fg),
                        cell_id("host"),
                        host_label(connection),
                        room(width - badge_room, HOST_LETTER),
                    ));
                if let Some(process) = process_name(connection).filter(|_| !columns.process_column)
                {
                    host = host.child(
                        // Gives way before the host does.
                        clipped(process.to_string())
                            .flex_shrink(SHRINK_FIRST)
                            .max_w(px(140.))
                            .text_xs()
                            .text_color(muted),
                    );
                }
                host
            }
            ColumnId::Destination => div().h_flex().child(small_text(
                "destination",
                connection.destination.clone(),
                width,
            )),
            ColumnId::Process => div().h_flex().child(small_text(
                "process",
                process_name(connection).unwrap_or_default().to_string(),
                width,
            )),
            ColumnId::Source => {
                div()
                    .h_flex()
                    .child(small_text("source", connection.source.clone(), width))
            }
            ColumnId::Inbound => div().h_flex().child(small_text(
                "inbound",
                inbound_label(connection).to_string(),
                width,
            )),
            ColumnId::Rule => div().h_flex().child(small_text(
                "rule",
                rule_label(connection).to_string(),
                width,
            )),
            // Group → … → node. Short of room, the groups give way and the
            // node stays: `节点选… → 香港-01`. The tooltip has it all.
            ColumnId::Chain => {
                let hops = chain_hops(connection);
                let label = hops.join(CHAIN_SEPARATOR);
                let long = may_truncate(&label, room(width, SMALL_LETTER));
                div().h_flex().child(
                    div()
                        .id(cell_id("chain"))
                        .h_flex()
                        .min_w_0()
                        .overflow_hidden()
                        .whitespace_nowrap()
                        .text_xs()
                        .text_color(muted)
                        .map(|chain| match hops.split_last() {
                            Some((node, groups)) if !groups.is_empty() => chain
                                .child(
                                    clipped(groups.join(CHAIN_SEPARATOR))
                                        .flex_shrink(SHRINK_FIRST)
                                        .min_w(px(12.)),
                                )
                                .child(div().flex_none().child(CHAIN_SEPARATOR))
                                .child(clipped(node.to_string())),
                            _ => chain.child(clipped(label.clone())),
                        })
                        .when(long, |chain| {
                            let label = SharedString::from(label);
                            chain.tooltip(move |window, cx| {
                                Tooltip::new(label.clone()).build(window, cx)
                            })
                        }),
                )
            }
            // The current rate, up over down; a line that is idle this
            // second fades, and a closed connection has none.
            ColumnId::Speed => {
                let idle = muted.opacity(0.5);
                let rate_line = |arrow: &str, rate: u64| {
                    stacked_line(format!("{arrow} {}", format_speed(rate)))
                        .text_color(if rate == 0 { idle } else { fg })
                };
                stacked_cell(theme).when(!closed, |cell| {
                    cell.child(rate_line("↑", connection.uplink))
                        .child(rate_line("↓", connection.downlink))
                })
            }
            ColumnId::Traffic => stacked_cell(theme)
                .text_color(muted)
                .child(stacked_line(format!(
                    "↑ {}",
                    format_bytes(connection.uplink_total)
                )))
                .child(stacked_line(format!(
                    "↓ {}",
                    format_bytes(connection.downlink_total)
                ))),
            ColumnId::Duration => clipped(format_elapsed(connection_age_ms(connection, now_ms)))
                .text_right()
                .text_xs()
                .text_color(muted),
        };
        row = row.child(column_cell(cell.min_w_0(), *column));
    }

    let close_slot = div().flex_none().w(px(CLOSE_WIDTH)).when(!closed, |slot| {
        let connections = connections.clone();
        let id = connection.id.clone();
        slot.child(
            Button::new(SharedString::from(format!("conn-close-{}", connection.id)))
                .ghost()
                .icon_control(ControlSize::Mini)
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
    row.map(|row| {
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
    .child(close_slot)
}

/// A right-aligned cell of two small mono lines (up over down).
fn stacked_cell(theme: &Theme) -> Div {
    div()
        .v_flex()
        .overflow_hidden()
        .text_xs()
        .line_height(px(STACKED_LINE))
        .font_family(theme.mono_font_family.clone())
}

/// One line of a `stacked_cell`, ellipsized if the column was made too
/// narrow for it.
fn stacked_line(text: String) -> Div {
    div()
        .w_full()
        .text_right()
        .overflow_hidden()
        .text_ellipsis()
        .whitespace_nowrap()
        .child(text)
}

/// The heading of `id`'s column.
fn column_title(id: ColumnId) -> &'static str {
    let t = &s().connections;
    match id {
        ColumnId::Time => t.col_time,
        ColumnId::Network => t.col_network,
        ColumnId::Host => t.col_host,
        ColumnId::Destination => t.col_destination,
        ColumnId::Process => t.col_process,
        ColumnId::Source => t.col_source,
        ColumnId::Inbound => t.col_inbound,
        ColumnId::Rule => t.col_rule,
        ColumnId::Chain => t.col_chain,
        ColumnId::Speed => t.col_speed,
        ColumnId::Traffic => t.col_traffic,
        ColumnId::Duration => t.col_duration,
    }
}

/// The figures' headings sit over their right-aligned figures.
fn right_aligned(id: ColumnId) -> bool {
    matches!(id, ColumnId::Speed | ColumnId::Traffic | ColumnId::Duration)
}

/// The list's column headings, on the rows' columns, with a handle on
/// each boundary between two of them: drag it to resize (see
/// `drag_boundary`), double-click it for the default width.
fn column_header(
    layout: &ColumnLayout,
    page: &WeakEntity<ConnectionsPage>,
    app_state: &Entity<AppState>,
    theme: &Theme,
) -> impl IntoElement {
    let mut header = div()
        .flex_none()
        .h(px(HEADER_HEIGHT))
        .w_full()
        // The rows' padding and their (transparent) border.
        .px(px(ROW_INSET / 2.))
        .h_flex()
        .items_center()
        .gap(px(COLUMN_GAP))
        .text_xs()
        .font_weight(FontWeight::MEDIUM)
        .text_color(theme.muted_foreground)
        .border_b_1()
        .border_color(theme.border);
    for (ix, column) in layout.columns.iter().enumerate() {
        let heading = clipped(column_title(column.id))
            .w_full()
            .when(right_aligned(column.id), |heading| heading.text_right());
        let cell = column_cell(div().relative().h_full().h_flex().items_center(), *column)
            .child(heading)
            .when(boundary_target(layout, ix).is_some(), |cell| {
                cell.child(resize_handle(ix, page, theme))
            });
        header = header.child(cell);
    }
    // A right-click on the headings opens the Columns menu too.
    let app_state = app_state.clone();
    header
        .child(div().flex_none().w(px(CLOSE_WIDTH)))
        .context_menu(move |menu, _, cx| columns_menu(menu, &app_state, cx))
}

/// The Columns menu: every column, the shown ones checked — a click shows
/// or hides it — and Reset columns. The last basic column (Host or Chain)
/// still showing can't be unchecked; its item says why on hover.
fn columns_menu(menu: PopupMenu, app_state: &Entity<AppState>, cx: &App) -> PopupMenu {
    let t = &s().connections;
    let columns = app_state.read(cx).settings.connections_columns.clone();
    let menu = ColumnId::ALL
        .into_iter()
        .fold(menu.min_w(px(160.)), |menu, id| {
            let title = column_title(id);
            let shown = columns.is_visible(id);
            if !columns.can_hide(id) {
                return menu.item(
                    PopupMenuItem::element(move |_, _| {
                        div()
                            .id(SharedString::from(format!("conn-col-keep-{}", id.key())))
                            .w_full()
                            .child(title)
                            .tooltip(|window, cx| {
                                Tooltip::new(s().connections.keep_basic_column).build(window, cx)
                            })
                    })
                    .checked(true)
                    .disabled(true),
                );
            }
            let app_state = app_state.clone();
            menu.item(
                PopupMenuItem::new(title)
                    .checked(shown)
                    .on_click(move |_, _, cx| {
                        app_state.update(cx, |state, cx| {
                            let mut columns = state.settings.connections_columns.clone();
                            if columns.toggle(id) {
                                state.set_connections_columns(columns, cx);
                            }
                        });
                    }),
            )
        });
    let app_state = app_state.clone();
    menu.separator().item(
        PopupMenuItem::new(t.reset_columns)
            .disabled(columns == ColumnSettings::default())
            .on_click(move |_, _, cx| {
                app_state.update(cx, |state, cx| {
                    state.set_connections_columns(ColumnSettings::default(), cx)
                });
            }),
    )
}

/// The grab band on the boundary after column `boundary`: the gap to the
/// next heading, with a hairline that lights up under the pointer.
fn resize_handle(
    boundary: usize,
    page: &WeakEntity<ConnectionsPage>,
    theme: &Theme,
) -> Stateful<Div> {
    let group = SharedString::from(format!("conn-col-resize-{boundary}"));
    let (press, drag, up, up_out) = (page.clone(), page.clone(), page.clone(), page.clone());
    div()
        .id(ElementId::NamedInteger(
            "conn-col-resize".into(),
            boundary as u64,
        ))
        .group(group.clone())
        .absolute()
        .top_0()
        .bottom_0()
        .right(px(-(COLUMN_GAP + RESIZE_HANDLE_WIDTH) / 2.))
        .w(px(RESIZE_HANDLE_WIDTH))
        .h_flex()
        .justify_center()
        .items_center()
        .cursor_col_resize()
        .child(
            div()
                .w(px(1.))
                .h(px(14.))
                .bg(theme.border)
                .group_hover(group, |line| line.h_full().bg(theme.primary)),
        )
        .on_mouse_down(MouseButton::Left, move |event, _, cx| {
            cx.stop_propagation();
            press
                .update(cx, |this, cx| {
                    this.press_column_boundary(boundary, event, cx)
                })
                .ok();
        })
        .on_drag(ColumnResize, |_, _, _, cx| cx.new(|_| ColumnResize))
        .on_drag_move(move |event: &DragMoveEvent<ColumnResize>, _, cx| {
            drag.update(cx, |this, cx| {
                this.drag_column_boundary(boundary, event.event.position.x, cx)
            })
            .ok();
        })
        .on_mouse_up(MouseButton::Left, move |_, _, cx| {
            up.update(cx, |this, cx| this.end_column_drag(cx)).ok();
        })
        .on_mouse_up_out(MouseButton::Left, move |_, _, cx| {
            up_out.update(cx, |this, cx| this.end_column_drag(cx)).ok();
        })
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
        // What the list holds: the live table, or its copy while paused.
        let frozen = self
            .frozen
            .as_ref()
            .map(|frozen| (frozen.table.clone(), frozen.now_ms));
        let listed = match &frozen {
            Some((table, _)) => summarize(table.iter()),
            None => summary,
        };
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
        // count stays whole. A paused list says so first; the totals stay
        // live.
        let count = if narrowed {
            let in_view = match self.view {
                ConnectionView::Active => listed.open,
                ConnectionView::Closed => listed.closed,
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
            .when(frozen.is_some(), |items| {
                items.child(
                    div()
                        .flex_none()
                        .child(tag_badge(theme, t.paused, warn_orange(theme))),
                )
            })
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
                .control(ControlSize::Regular)
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
                control_input(&self.filter_input, ControlSize::Regular)
                    .cleanable(true)
                    .prefix(
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
                    .control(ControlSize::Regular)
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
            ControlSize::Regular,
            vec![
                Segment::new(t.active_tab).count(listed.open),
                Segment::new(t.closed_tab).count(listed.closed),
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
                .h(ControlSize::Regular.height())
                .pl_2p5()
                .pr_0p5()
                .rounded(theme.radius)
                .bg(theme.tab_bar_segmented)
                .child(sort_select)
                .child(
                    Button::new("connections-sort-direction")
                        .ghost()
                        .icon_control(ControlSize::Inline)
                        .icon(Icon::empty().path(direction_icon))
                        .tooltip(direction_tip)
                        .on_click(move |_, _, cx| {
                            reverse_page
                                .update(cx, |this, cx| this.reverse_sort(cx))
                                .ok();
                        }),
                ),
        );

        // Which columns show (and Reset columns); the headings' right-click
        // opens the same menu.
        let columns_app_state = self.app_state.clone();
        controls = controls.child(
            div().flex_none().child(
                Button::new("connections-columns")
                    .outline()
                    .icon_control(ControlSize::Regular)
                    .icon(Icon::empty().path("icons/columns-3.svg"))
                    .tooltip(t.columns)
                    .dropdown_menu_with_anchor(Anchor::TopRight, move |menu, _, cx| {
                        columns_menu(menu, &columns_app_state, cx)
                    }),
            ),
        );

        let mut head = page_header(theme, ActivePage::Connections);
        if has_any {
            let pause_page = page.clone();
            let paused = frozen.is_some();
            let pause = Button::new("connections-pause")
                .outline()
                .control(ControlSize::Regular)
                .selected(paused)
                .toggled(paused)
                .map(|button| {
                    if paused {
                        button.icon_label(IconName::Play, t.resume)
                    } else {
                        button.icon_label(IconName::Pause, t.pause)
                    }
                })
                .tooltip(if paused { t.resume_hint } else { t.pause_hint })
                .on_click(move |_, _, cx| {
                    pause_page.update(cx, |this, cx| this.toggle_pause(cx)).ok();
                });
            head = head.context(summary_items).action(pause).action(close_all);
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
            let frozen = frozen.clone();
            let layout = layout_columns(&self.columns(cx), self.list_width());
            let overflows = layout.overflows(self.list_width());
            let min_row_width = layout.min_width;
            let header = column_header(&layout, &page, &self.app_state, theme);
            let row_columns = Rc::new(RowColumns::new(layout));
            let list = uniform_list("connections-list", rows.len(), move |range, _, cx| {
                let networks = NetworkColors::new(cx.theme());
                let theme = cx.theme();
                let state = list_connections.read(cx);
                let (table, now_ms) = match &frozen {
                    Some((table, now_ms)) => (&**table, *now_ms),
                    None => (&state.table, unix_millis_now()),
                };
                range
                    .map(|ix| match table.get(&rows[ix]) {
                        Some(connection) => connection_row(
                            connection,
                            frozen.is_some()
                                && state
                                    .table
                                    .get(&connection.id)
                                    .is_none_or(Connection::is_closed),
                            now_ms,
                            &list_connections,
                            &list_page,
                            selected.as_deref() == Some(rows[ix].as_str()),
                            &row_columns,
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

            // Measures the list as it paints: a new width lays the columns
            // out again on the next frame (gpui ignores a notify sent
            // while it draws). Host and Chain follow the width in between.
            let measure_page = page.clone();
            let measure = canvas(
                move |bounds, window, cx| {
                    let Some(page) = measure_page.upgrade() else {
                        return;
                    };
                    let width = bounds.size.width;
                    let relayout = page.update(cx, |this, _| {
                        let before = this.list_width.replace(width);
                        before.is_none_or(|before| (before - width).abs() >= px(0.5))
                    });
                    if relayout {
                        window.on_next_frame(move |_, cx| page.update(cx, |_, cx| cx.notify()));
                    }
                },
                |_, _, _, _| {},
            )
            .absolute()
            .top_0()
            .left_0()
            .size_full();
            // The header and the rows scroll sideways together, when the
            // columns' minimums don't fit; the rows scroll down on their
            // own, their scrollbar kept in view over the list's right edge.
            let table = div()
                .id("connections-table")
                .size_full()
                .overflow_x_scroll()
                .restrict_scroll_to_axis()
                .track_scroll(&self.h_scroll)
                .child(
                    div()
                        .v_flex()
                        .h_full()
                        .w_full()
                        .min_w(px(min_row_width))
                        .child(header)
                        .child(div().flex_1().min_h_0().pt_1().child(list)),
                );
            // While a boundary is dragged: the resize cursor everywhere
            // over the list, no row hovering under it, and the drag ends
            // wherever the button comes up.
            let drag_cover = self.column_drag.is_some().then(|| {
                let end_page = page.clone();
                div()
                    .id("connections-column-drag")
                    .absolute()
                    .inset_0()
                    .occlude()
                    .cursor_col_resize()
                    .on_mouse_up(MouseButton::Left, move |_, _, cx| {
                        end_page
                            .update(cx, |this, cx| this.end_column_drag(cx))
                            .ok();
                    })
            });
            let list = div()
                .relative()
                .v_flex()
                .flex_1()
                .min_h_0()
                .child(measure)
                .child(table)
                .child(
                    page_scrollbar("connections-scrollbar", &self.scroll)
                        .top(px(HEADER_HEIGHT + 4.)),
                )
                .when(overflows, |list| list.horizontal_scrollbar(&self.h_scroll))
                .children(drag_cover);
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
