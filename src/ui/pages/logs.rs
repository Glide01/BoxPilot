//! 日志页,日志浏览器式布局(与连接页一致):控制行(搜索框 + 级别切换 +
//! 复制 + 清空 + Live 徽标);一行计数(显示的行数,以及缓冲里 Error / Warn
//! 各有几行);下面是带表头的日志表,最新的在上,每行一行:到达时间(等宽,
//! 毫秒部分淡色)、级别徽标、来源(`router`、`inbound/mixed[…]`)、消息(前面
//! 是淡色的连接标记 `[id 时长]`)。拆分见 `core::log_merge::LineParts`。
//!
//! 点一行选中它(再点取消):表格下方的卡片显示整行原文,可复制。复制按钮
//! 复制表里显示的全部行(按时间顺序)。
//!
//! 列宽:时间、级别、来源可拖动表头里两列之间的缝调宽窄,消息占剩下的
//! 宽度(`core::log_columns`);拖动时只画在页面上的副本(`ColumnDrag`),
//! 松开鼠标才写进设置。双击缝恢复那一列的默认宽度,右键表头可全部重置。
//!
//! 搜索:空白分隔的词都要出现(不区分大小写),`-词` 排除含它的行
//! (`core::log_merge::LogQuery`)。级别:默认跟随 sing-box 配置的
//! `log.level`(API 报告),用户可放宽到 Debug/Trace 或收紧;选回配置级别即
//! 恢复跟随。来源与去重见 `core::log_merge`。
//!
//! 列表用 `uniform_list` 虚拟化,行(按 id,最新在前)按 `RowsKey` 缓存,
//! 日志或条件变了才重算。停在顶部时新行出现在顶上;往下翻着看时,新行插在
//! 上方不会把正在看的行挤走:滚动位置随之下移。页面不在前台时不渲染,也就
//! 不重算。

use crate::core::log_columns::{
    drag_boundary, layout_columns, time_shows_millis, LogColumn, LogColumnWidths, LogLayout,
    COLUMN_GAP, ROW_INSET,
};
use crate::core::log_merge::{LogEntry, LogQuery};
use crate::core::presentation::log_count_label;
use crate::core::settings::StatusLevel;
use crate::core::singbox_api::LogLevel;
use crate::core::timefmt::format_clock_ms;
use crate::i18n::s;
use crate::state::{AppState, LogBuffer};
use crate::ui::pages::ActivePage;
use crate::ui::widgets::{
    connect_button, control_input, empty_state, page_header, page_layout, page_scrollbar,
    row_hover_bg, segmented, tag_badge, toolbar_search, warn_orange, Control, ControlSize, Segment,
    TextLabel,
};
use crate::ui::{card_frame, locale, toast};
use gpui::{prelude::FluentBuilder, *};
use gpui_component::{
    button::{Button, ButtonVariants},
    input::{InputEvent, InputState},
    menu::{ContextMenuExt, PopupMenu, PopupMenuItem},
    theme::Theme,
    ActiveTheme, Disableable, Icon, IconName, Sizable, StyledExt,
};
use std::rc::Rc;

/// The level control's choices, most severe first. `panic`/`fatal` lines
/// show under every one of them. The names stay English in every UI
/// language: they are sing-box's own level names, the same words the
/// badges in the table show (`ERROR`, `INFO`…).
const LEVEL_CHOICES: [(&str, &str, LogLevel); 5] = [
    ("level-error", "Error", LogLevel::Error),
    ("level-warn", "Warn", LogLevel::Warn),
    ("level-info", "Info", LogLevel::Info),
    ("level-debug", "Debug", LogLevel::Debug),
    ("level-trace", "Trace", LogLevel::Trace),
];

/// Fixed row height — `uniform_list` lays every row out at the first row's
/// size, so all rows must match.
const ROW_HEIGHT: f32 = 32.;
/// The column headings' row.
const HEADER_HEIGHT: f32 = 32.;
/// Before the first frame has measured the list: about the list in the
/// window BoxPilot opens at, corrected on the next frame.
const GUESSED_LIST_WIDTH: f32 = 870.;
/// The grab band of a column boundary in the header: the gap between two
/// headings.
const RESIZE_HANDLE_WIDTH: f32 = COLUMN_GAP;
/// The selected line's card grows with its text up to this, then scrolls.
const DETAIL_MAX_HEIGHT: f32 = 132.;

/// What the cached rows were derived from.
#[derive(Clone, PartialEq)]
struct RowsKey {
    /// The buffer's oldest and newest ids and its length: any change to it
    /// (new lines, evictions, a clear) moves one of them.
    first: Option<u64>,
    last: Option<u64>,
    len: usize,
    threshold: LogLevel,
    query: String,
}

/// A column boundary being dragged: the column on its left, where the
/// pointer went down, the widths and their layout then (each step is
/// computed from those, so nothing drifts), and the widths as the drag has
/// them now — drawn, but only saved to the settings when the button comes
/// up.
struct ColumnDrag {
    column: LogColumn,
    start_x: Pixels,
    settings: LogColumnWidths,
    layout: LogLayout,
    current: LogColumnWidths,
}

/// The value gpui carries while a column boundary is dragged; it draws
/// nothing (the columns themselves follow the pointer).
struct ColumnResize;

impl Render for ColumnResize {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        Empty
    }
}

pub struct LogsPage {
    app_state: Entity<AppState>,
    /// AppState 的 LogBuffer(固定不变)。
    logs: Entity<LogBuffer>,
    search: Entity<InputState>,
    scroll: UniformListScrollHandle,
    /// Ids of the lines shown, newest first, and what they came from.
    rows: Rc<Vec<u64>>,
    rows_key: Option<RowsKey>,
    /// The line whose full text the card under the table shows.
    selected: Option<u64>,
    /// The list's width at the last paint: what the columns are laid out
    /// in.
    list_width: Option<Pixels>,
    /// `Some` while a column boundary is being dragged.
    column_drag: Option<ColumnDrag>,
}

impl LogsPage {
    pub fn new(app_state: Entity<AppState>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let logs = app_state.read(cx).logs.clone();
        cx.observe(&logs, |_, _, cx| cx.notify()).detach();
        // Stopped / started: the Live badge and the empty state's Connect
        // button follow.
        let process = app_state.read(cx).process.clone();
        cx.observe(&process, |_, _, cx| cx.notify()).detach();

        let search =
            cx.new(|cx| InputState::new(window, cx).placeholder(s().logs.search_placeholder));
        locale::observe(window, cx, |this: &mut Self, window, cx| {
            this.search.update(cx, |input, cx| {
                input.set_placeholder(s().logs.search_placeholder, window, cx)
            });
        })
        .detach();
        cx.subscribe_in(&search, window, |_, _, ev: &InputEvent, _, cx| {
            if matches!(ev, InputEvent::Change) {
                cx.notify();
            }
        })
        .detach();

        Self {
            app_state,
            logs,
            search,
            scroll: UniformListScrollHandle::new(),
            rows: Rc::new(Vec::new()),
            rows_key: None,
            selected: None,
            list_width: None,
            column_drag: None,
        }
    }

    /// The column widths in effect: the settings', or a drag's while it
    /// lasts.
    fn columns(&self, cx: &App) -> LogColumnWidths {
        match &self.column_drag {
            Some(drag) => drag.current.clone(),
            None => self.app_state.read(cx).settings.logs_columns.clone(),
        }
    }

    fn list_width(&self) -> f32 {
        self.list_width.map_or(GUESSED_LIST_WIDTH, f32::from)
    }

    /// The pointer went down on `column`'s right edge: a drag may follow,
    /// or this is the second click of a double-click, which puts the
    /// column back to its default width.
    fn press_column_boundary(
        &mut self,
        column: LogColumn,
        event: &MouseDownEvent,
        cx: &mut Context<Self>,
    ) {
        let settings = self.columns(cx);
        if event.click_count >= 2 {
            self.column_drag = None;
            let mut reset = settings;
            reset.reset_width(column);
            self.app_state
                .update(cx, |state, cx| state.set_logs_columns(reset, cx));
            cx.notify();
            return;
        }
        let layout = layout_columns(&settings, self.list_width());
        self.column_drag = Some(ColumnDrag {
            column,
            start_x: event.position.x,
            current: settings.clone(),
            settings,
            layout,
        });
        cx.notify();
    }

    /// The pointer moved during the drag of `column`'s right edge.
    fn drag_column_boundary(&mut self, column: LogColumn, x: Pixels, cx: &mut Context<Self>) {
        let Some(drag) = self.column_drag.as_mut().filter(|d| d.column == column) else {
            return;
        };
        let delta = f32::from(x - drag.start_x);
        let next = drag_boundary(&drag.settings, &drag.layout, drag.column, delta);
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
            self.app_state
                .update(cx, |state, cx| state.set_logs_columns(drag.current, cx));
        }
        cx.notify();
    }

    /// Recompute the rows only when the buffer or the filters moved. New
    /// lines above a scrolled-down view move the scroll position down with
    /// them, so the lines being read stay put; at the top, the newest stay
    /// in view.
    fn refresh_rows(&mut self, cx: &App) {
        let logs = self.logs.read(cx);
        let entries = logs.entries();
        let key = RowsKey {
            first: entries.front().map(LogEntry::id),
            last: entries.back().map(LogEntry::id),
            len: entries.len(),
            threshold: logs.threshold(),
            query: self.search.read(cx).value().trim().to_string(),
        };
        if self.rows_key.as_ref() == Some(&key) {
            return;
        }
        let query = LogQuery::parse(&key.query);
        let rows: Vec<u64> = entries
            .iter()
            .rev()
            .filter(|e| e.level <= key.threshold && query.matches(e))
            .map(LogEntry::id)
            .collect();

        let same_filters = self
            .rows_key
            .as_ref()
            .is_some_and(|old| old.threshold == key.threshold && old.query == key.query);
        let handle = self.scroll.0.borrow().base_handle.clone();
        if same_filters {
            let newest = self.rows.first().copied();
            let added = match newest {
                Some(newest) => rows.iter().take_while(|id| **id > newest).count(),
                None => 0,
            };
            let offset = handle.offset();
            if added > 0 && offset.y < px(0.) {
                handle.set_offset(point(offset.x, offset.y - px(ROW_HEIGHT) * added as f32));
            }
        } else {
            handle.set_offset(point(px(0.), px(0.)));
        }
        if let Some(id) = self.selected {
            if entries.binary_search_by_key(&id, LogEntry::id).is_err() {
                self.selected = None;
            }
        }
        self.rows = Rc::new(rows);
        self.rows_key = Some(key);
    }

    fn toggle_selected(&mut self, id: u64, cx: &mut Context<Self>) {
        self.selected = if self.selected == Some(id) {
            None
        } else {
            Some(id)
        };
        cx.notify();
    }

    /// Every shown line, oldest first, as sing-box wrote them.
    fn shown_text(&self, cx: &App) -> String {
        let logs = self.logs.read(cx);
        let entries = logs.entries();
        self.rows
            .iter()
            .rev()
            .filter_map(|id| entry(entries, *id))
            .map(|e| e.text.as_str())
            .collect::<Vec<_>>()
            .join("\n")
    }
}

fn copy_to_clipboard(text: String, cx: &mut App) {
    cx.write_to_clipboard(ClipboardItem::new_string(text));
    toast::show(StatusLevel::Success, s().common.copied, cx);
}

fn entry(entries: &std::collections::VecDeque<LogEntry>, id: u64) -> Option<&LogEntry> {
    entries
        .binary_search_by_key(&id, LogEntry::id)
        .ok()
        .map(|ix| &entries[ix])
}

/// A level's colour: red for the failures, orange for warnings, the accent
/// for info, muted for the chatter below it.
fn level_color(level: LogLevel, theme: &Theme) -> Hsla {
    match level {
        LogLevel::Panic | LogLevel::Fatal | LogLevel::Error => theme.danger,
        LogLevel::Warn => warn_orange(theme),
        LogLevel::Info => theme.primary,
        LogLevel::Debug | LogLevel::Trace => theme.muted_foreground,
    }
}

fn level_badge(level: LogLevel, theme: &Theme) -> Div {
    tag_badge(
        theme,
        level.as_str().to_uppercase(),
        level_color(level, theme),
    )
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

/// The arrival time, its milliseconds quieter — left out when `millis` is
/// false (a Time column too narrow for them).
fn clock(at_ms: i64, millis: bool, fg: Hsla, muted: Hsla, theme: &Theme) -> Div {
    let (time, fraction) = format_clock_ms(at_ms);
    div()
        .flex_none()
        .h_flex()
        .overflow_hidden()
        .text_sm()
        .font_family(theme.mono_font_family.clone())
        .child(div().text_color(fg).child(time))
        .when(millis, |clock| {
            clock.child(div().text_color(muted).child(fraction))
        })
}

/// One table row.
fn log_row(
    entry: &LogEntry,
    selected: bool,
    layout: &LogLayout,
    page: &WeakEntity<LogsPage>,
    theme: &Theme,
) -> Stateful<Div> {
    let parts = entry.parts();
    let (fg, muted) = (theme.foreground, theme.muted_foreground);
    let color = level_color(entry.level, theme);
    let hover_bg = row_hover_bg(theme);
    let id = entry.id();
    let page = page.clone();

    let message = div()
        .flex_1()
        .min_w_0()
        .h_flex()
        .gap_2()
        .text_sm()
        .font_family(theme.mono_font_family.clone())
        .children(parts.tag.map(|tag| {
            div()
                .flex_none()
                .whitespace_nowrap()
                .text_color(muted.opacity(0.75))
                .child(tag.to_string())
        }))
        .child(clipped(parts.message.to_string()).text_color(fg));

    div()
        .id(("log-row", id as usize))
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
                // A tinted band in the line's level colour, its outline, and
                // a bar down its leading edge.
                row.bg(color.opacity(0.10))
                    .border_color(color.opacity(0.45))
                    .child(
                        div()
                            .absolute()
                            .left(px(-1.))
                            .top(px(6.))
                            .bottom(px(6.))
                            .w(px(3.))
                            .rounded_full()
                            .bg(color),
                    )
            } else {
                row.border_color(transparent_black())
                    .hover(move |style| style.bg(hover_bg))
            }
        })
        .on_click(move |_, _, cx| {
            page.update(cx, |page, cx| page.toggle_selected(id, cx))
                .ok();
        })
        .child({
            let width = layout.width(LogColumn::Time);
            clock(entry.at_ms, time_shows_millis(width), fg, muted, theme).w(px(width))
        })
        .child(
            div()
                .flex_none()
                .w(px(layout.width(LogColumn::Level)))
                .h_flex()
                .overflow_hidden()
                .child(level_badge(entry.level, theme)),
        )
        .child(
            clipped(parts.source.unwrap_or_default().to_string())
                .flex_none()
                .w(px(layout.width(LogColumn::Source)))
                .text_sm()
                .text_color(muted),
        )
        .child(message)
}

/// The table's column headings, on the rows' columns, with a handle on the
/// right edge of each but Message: drag it to resize (see `drag_boundary`),
/// double-click it for the default width. A right-click resets them all.
fn column_header(
    layout: &LogLayout,
    page: &WeakEntity<LogsPage>,
    app_state: &Entity<AppState>,
    theme: &Theme,
) -> impl IntoElement {
    let t = &s().logs;
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
    for (column, title) in LogColumn::ALL
        .into_iter()
        .zip([t.col_time, t.col_level, t.col_source])
    {
        header = header.child(
            div()
                .relative()
                .flex_none()
                .w(px(layout.width(column)))
                .h_full()
                .h_flex()
                .items_center()
                .child(clipped(title))
                .child(resize_handle(column, page, theme)),
        );
    }
    let app_state = app_state.clone();
    header
        .child(clipped(t.col_message).flex_1())
        .context_menu(move |menu, _, cx| reset_menu(menu, &app_state, cx))
}

/// The headings' right-click menu: Reset column widths.
fn reset_menu(menu: PopupMenu, app_state: &Entity<AppState>, cx: &App) -> PopupMenu {
    let untouched = app_state.read(cx).settings.logs_columns == LogColumnWidths::default();
    let app_state = app_state.clone();
    menu.item(
        PopupMenuItem::new(s().logs.reset_columns)
            .disabled(untouched)
            .on_click(move |_, _, cx| {
                app_state.update(cx, |state, cx| {
                    state.set_logs_columns(LogColumnWidths::default(), cx)
                });
            }),
    )
}

/// The grab band on `column`'s right edge: the gap to the next heading,
/// with a hairline that lights up under the pointer.
fn resize_handle(column: LogColumn, page: &WeakEntity<LogsPage>, theme: &Theme) -> Stateful<Div> {
    let group = SharedString::from(format!("log-col-resize-{}", column.key()));
    let (press, drag, up, up_out) = (page.clone(), page.clone(), page.clone(), page.clone());
    div()
        .id(ElementId::Name(group.clone()))
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
                .update(cx, |this, cx| this.press_column_boundary(column, event, cx))
                .ok();
        })
        .on_drag(ColumnResize, |_, _, _, cx| cx.new(|_| ColumnResize))
        .on_drag_move(move |event: &DragMoveEvent<ColumnResize>, _, cx| {
            drag.update(cx, |this, cx| {
                this.drag_column_boundary(column, event.event.position.x, cx)
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

/// The selected line in full, under the table: its level, time and source,
/// the whole text (wrapped, scrolling past a few lines), and its own copy.
fn detail_card(entry: &LogEntry, page: &WeakEntity<LogsPage>, theme: &Theme) -> Div {
    let parts = entry.parts();
    let text = entry.text.clone();
    let close_page = page.clone();
    let id = entry.id();
    card_frame(theme)
        .flex_none()
        .gap_2()
        .py_3()
        .child(
            div()
                .h_flex()
                .items_center()
                .gap_3()
                .child(level_badge(entry.level, theme))
                .child(clock(
                    entry.at_ms,
                    true,
                    theme.foreground,
                    theme.muted_foreground,
                    theme,
                ))
                .child(
                    clipped(parts.source.unwrap_or_default().to_string())
                        .flex_1()
                        .text_sm()
                        .text_color(theme.muted_foreground),
                )
                .child(
                    Button::new("log-copy-line")
                        .ghost()
                        .icon_control(ControlSize::Mini)
                        .icon(IconName::Copy)
                        .tooltip(s().common.copy)
                        .on_click(move |_, _, cx| copy_to_clipboard(text.clone(), cx)),
                )
                .child(
                    Button::new("log-close-line")
                        .ghost()
                        .icon_control(ControlSize::Mini)
                        .icon(IconName::Close)
                        .tooltip(s().logs.close_line)
                        .on_click(move |_, _, cx| {
                            close_page
                                .update(cx, |page, cx| page.toggle_selected(id, cx))
                                .ok();
                        }),
                ),
        )
        .child(
            div()
                .id("log-line-text")
                .max_h(px(DETAIL_MAX_HEIGHT))
                .overflow_y_scroll()
                .text_sm()
                .font_family(theme.mono_font_family.clone())
                .child(entry.text.clone()),
        )
}

impl Render for LogsPage {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.refresh_rows(cx);
        let app_state_entity = self.app_state.clone();
        let logs_entity = self.logs.clone();
        let logs = logs_entity.read(cx);
        let theme = cx.theme();
        let t = &s().logs;

        let total = logs.entries().len();
        let stopped = self.app_state.read(cx).process.read(cx).is_stopped();

        // No lines yet: nothing to search, filter, copy or clear.
        if total == 0 {
            return page_layout(
                page_header(theme, ActivePage::Logs),
                div().size_full().child(
                    empty_state(theme, IconName::SquareTerminal, t.empty_title, t.empty_hint)
                        .when(stopped, |this| this.action(connect_button("logs-connect"))),
                ),
            );
        }

        let threshold = logs.threshold();
        let default_threshold = logs.default_threshold();
        let page = cx.entity().downgrade();

        // The level sing-box's config asks for says so in its tooltip.
        let levels = LEVEL_CHOICES
            .iter()
            .map(|(_, label, level)| {
                let segment = Segment::new(*label);
                if *level == default_threshold {
                    segment.tooltip(s().logs.configured_level)
                } else {
                    segment
                }
            })
            .collect();
        let logs_for_levels = logs_entity.clone();
        let copy_page = page.clone();
        let controls = div()
            .h_flex()
            .flex_wrap()
            .items_center()
            .gap_2()
            .w_full()
            .child(toolbar_search(
                control_input(&self.search, ControlSize::Regular)
                    .cleanable(true)
                    .prefix(
                        Icon::new(IconName::Search)
                            .small()
                            .text_color(theme.muted_foreground),
                    ),
            ))
            // Switches to the right, under the header's actions.
            .child(div().flex_1())
            .child(segmented(
                theme,
                "log-levels",
                ControlSize::Regular,
                levels,
                LEVEL_CHOICES
                    .iter()
                    .position(|(_, _, level)| *level == threshold),
                move |ix, _, cx| {
                    let level = LEVEL_CHOICES[ix].2;
                    logs_for_levels.update(cx, |b, cx| b.set_threshold(level, cx));
                },
            ));
        let head = page_header(theme, ActivePage::Logs)
            .action(
                Button::new("logs-copy")
                    .outline()
                    .icon_control(ControlSize::Regular)
                    .icon(IconName::Copy)
                    .tooltip(t.copy_shown)
                    .disabled(self.rows.is_empty())
                    .on_click(move |_, _, cx| {
                        if let Some(page) = copy_page.upgrade() {
                            let text = page.read(cx).shown_text(cx);
                            copy_to_clipboard(text, cx);
                        }
                    }),
            )
            .action(
                Button::new("logs-clear")
                    .outline()
                    .control(ControlSize::Regular)
                    .text_label(t.clear)
                    .on_click(move |_, _, cx| {
                        app_state_entity.update(cx, |state, cx| state.clear_logs(cx));
                    }),
            );

        // How many lines the table shows ("3 of 10" when filtered), then
        // how many errors and warnings the buffer holds, whatever is shown.
        let shown = self.rows.len();
        let count = log_count_label(shown, total).unwrap_or_else(|| (t.lines)(shown as u64));
        let (errors, warnings) =
            logs.entries()
                .iter()
                .fold((0, 0), |(errors, warnings), e| match e.level {
                    LogLevel::Panic | LogLevel::Fatal | LogLevel::Error => (errors + 1, warnings),
                    LogLevel::Warn => (errors, warnings + 1),
                    _ => (errors, warnings),
                });
        let tally = |n: usize, level: LogLevel, label: &'static str| {
            (n > 0).then(|| {
                div()
                    .h_flex()
                    .items_center()
                    .gap_1p5()
                    .child(
                        div()
                            .size(px(7.))
                            .rounded_full()
                            .bg(level_color(level, theme)),
                    )
                    .child(format!("{n} {label}"))
            })
        };
        let meta = div()
            .h_flex()
            .items_center()
            .gap_4()
            .text_sm()
            .text_color(theme.muted_foreground)
            .child(count)
            .children(tally(errors, LogLevel::Error, "Error"))
            .children(tally(warnings, LogLevel::Warn, "Warn"));

        let (empty, table) = if self.rows.is_empty() {
            let empty = empty_state(theme, IconName::Search, t.no_match_title, t.no_match_hint);
            (Some(empty), None)
        } else {
            let rows = self.rows.clone();
            let list_logs = logs_entity.clone();
            let list_page = page.clone();
            let selected = self.selected;
            let layout = layout_columns(&self.columns(cx), self.list_width());
            let header = column_header(&layout, &page, &self.app_state, theme);
            let layout = Rc::new(layout);
            let list = uniform_list("logs-list", rows.len(), move |range, _, cx| {
                let theme = cx.theme();
                let entries = list_logs.read(cx).entries();
                range
                    .map(|ix| match entry(entries, rows[ix]) {
                        Some(e) => {
                            log_row(e, selected == Some(rows[ix]), &layout, &list_page, theme)
                                .into_any_element()
                        }
                        // Rows are refreshed with the buffer before the list
                        // renders, so this is only a defensive blank.
                        None => div().h(px(ROW_HEIGHT)).into_any_element(),
                    })
                    .collect::<Vec<_>>()
            })
            .track_scroll(&self.scroll)
            .size_full();
            // Measures the list as it paints: a new width lays the columns
            // out again on the next frame (gpui ignores a notify sent while
            // it draws). Message follows the width in between.
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
            // While a boundary is dragged: the resize cursor everywhere over
            // the table, no row hovering under it, and the drag ends
            // wherever the button comes up.
            let drag_cover = self.column_drag.is_some().then(|| {
                let end_page = page.clone();
                div()
                    .id("logs-column-drag")
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
            let table = div()
                .relative()
                .v_flex()
                .flex_1()
                .min_h_0()
                .child(measure)
                .child(header)
                .child(
                    div()
                        .relative()
                        .flex_1()
                        .min_h_0()
                        .pt_1()
                        .child(list)
                        .child(page_scrollbar("logs-scrollbar", &self.scroll)),
                )
                .children(drag_cover);
            (None, Some(table))
        };
        let detail = self
            .selected
            .and_then(|id| entry(logs.entries(), id))
            .map(|e| detail_card(e, &page, theme));

        page_layout(
            head.context(meta),
            div()
                .v_flex()
                .size_full()
                .gap_4()
                .child(controls)
                .children(empty)
                .children(table)
                .children(detail),
        )
    }
}
