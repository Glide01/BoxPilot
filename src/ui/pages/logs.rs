use crate::core::log_merge::ViewText;
use crate::core::presentation::log_count_label;
use crate::core::singbox_api::LogLevel;
use crate::i18n::s;
use crate::state::{AppState, LogBuffer};
use crate::ui::card_frame;
use crate::ui::widgets::{connect_button, empty_state, page_header, segmented, Segment};
use gpui::{prelude::FluentBuilder, *};
use gpui_component::{
    button::Button,
    input::{Editor, EditorState, TextDecoration, TextDecorationCollection},
    ActiveTheme, IconName, Sizable, StyledExt,
};

/// The level control's choices, most severe first. `panic`/`fatal` lines
/// show under every one of them. The names stay English in every UI
/// language: they are sing-box's own level names, the same words the
/// coloured badges in the log text show (`ERROR`, `INFO`…).
const LEVEL_CHOICES: [(&str, &str, LogLevel); 5] = [
    ("level-error", "Error", LogLevel::Error),
    ("level-warn", "Warn", LogLevel::Warn),
    ("level-info", "Info", LogLevel::Info),
    ("level-debug", "Debug", LogLevel::Debug),
    ("level-trace", "Trace", LogLevel::Trace),
];

/// 日志页:标题行(标题 + 计数 + 级别 pills + 清空)在内容卡片**外面**(与
/// Groups 页一致);卡片内是一个只读的 `Editor`,承载按当前级别过滤后的日志
/// 文本——用户能用鼠标拖选、复制(⌘/Ctrl+C 或右键菜单)、Ctrl+F 搜索,不换行
/// 所以长行可横向滚动。每行的级别词(`INFO` 等)用 text decoration 上色,
/// 即级别徽标。卡片 `flex_1 + min_h_0` 占满标题行外的剩余高度。
///
/// 级别:默认跟随 sing-box 配置的 `log.level`(API 报告),用户可放宽到
/// Debug/Trace 或收紧;选回配置级别即恢复跟随。来源与去重见
/// `core::log_merge`。
///
/// 刷新:`set_value` 会清掉选区并把滚动复位到顶部,所以每次换文本后还原——
/// 选区按行 id 映射到新文本(`ViewText::map_offset`);停在底部时滚到新的底部
/// 跟随最新行(还没布局过就把光标放到末行,首次布局时编辑器自己滚过去);
/// 否则按行 id 保持同一批行可见(`ViewText::map_row`),上方淘汰旧行、下方
/// 追加新行都不会让视图跳走。
///
/// 不可见时不刷新:页面自上次刷新后没渲染过(不在前台,或窗口没画),新日志
/// 只记 `stale`,不重组文本、不 `set_value`;下次渲染开头补一次刷新。忙碌的
/// 日志流因此不会在别的页面上持续占用 UI 线程。
pub struct LogsPage {
    app_state: Entity<AppState>,
    /// 只读编辑器,承载日志文本,供选中 / 复制 / 搜索 / 横向滚动。
    viewer: Entity<EditorState>,
    /// 级别徽标的着色。
    badges: TextDecorationCollection,
    /// `badges` 按哪种主题(深色?)上的色;切换浅色/深色后渲染时重新上色。
    badges_dark: bool,
    /// 当前灌进 viewer 的内容(用于判断是否变化,以及映射选区 / 滚动)。
    shown: ViewText,
    /// 是否跟随最新行。仅在 viewer 画过当前内容后按"是否停在底部"重算。
    follow: bool,
    /// 自上次刷新以来本页是否渲染过——没渲染过(页面不可见)时 viewer 的
    /// 布局 / 滚动还是旧的,不能据此判断。
    painted: bool,
    /// 上次刷新设下但可能还没生效的滚动位置(页面不可见时一直挂着)。
    pending_offset: Option<Point<Pixels>>,
    /// LogBuffer 变了但因页面不可见跳过了刷新;下次渲染时补上。
    stale: bool,
    /// AppState 的 LogBuffer(固定不变)。
    logs: Entity<LogBuffer>,
}

impl LogsPage {
    pub fn new(app_state: Entity<AppState>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let logs = app_state.read(cx).logs.clone();

        let viewer = cx.new(|cx| {
            EditorState::new(window, cx)
                .line_number(false)
                .folding(false)
                .soft_wrap(false)
        });
        let badges = viewer.update(cx, |s, cx| s.create_decorations_collection(Vec::new(), cx));

        // LogBuffer 变化(新日志 / 清空 / 切换级别)→ 把最新文本灌进 viewer,
        // 并重渲染标题计数。页面自上次刷新后没渲染过(不可见)就只标记
        // `stale`,留到下次渲染。
        cx.observe_in(&logs, window, |this, logs, window, cx| {
            if this.painted {
                this.refresh(&logs, window, cx);
            } else {
                this.stale = true;
            }
            cx.notify();
        })
        .detach();

        // Stopped / started: the empty state's Connect button follows.
        let process = app_state.read(cx).process.clone();
        cx.observe(&process, |_, _, cx| cx.notify()).detach();

        let mut page = Self {
            app_state,
            viewer,
            badges,
            badges_dark: cx.theme().is_dark(),
            shown: ViewText::default(),
            follow: true,
            painted: false,
            pending_offset: None,
            stale: false,
            logs: logs.clone(),
        };
        page.refresh(&logs, window, cx);
        page
    }

    fn refresh(&mut self, logs: &Entity<LogBuffer>, window: &mut Window, cx: &mut Context<Self>) {
        let view = {
            let logs = logs.read(cx);
            ViewText::compose(logs.entries(), logs.threshold())
        };
        if view.text == self.shown.text {
            return;
        }

        let (offset, line_height, height, selection) = {
            let viewer = self.viewer.read(cx);
            (
                viewer.scroll_offset(),
                viewer.line_height(),
                viewer.text_bounds().map(|b| b.size.height),
                viewer.selected_range(),
            )
        };
        // Only a painted layout says where the user is.
        let offset = match (self.painted, self.pending_offset) {
            (false, Some(pending)) => pending,
            _ => offset,
        };
        if let (true, Some(line_height), Some(height)) = (self.painted, line_height, height) {
            let content = line_height * self.shown.line_count() as f32;
            self.follow = -offset.y + height >= content - line_height * 1.5;
        }

        let decorations = badge_decorations(&view, cx);
        self.viewer
            .update(cx, |s, cx| s.set_value(view.text.clone(), window, cx));
        self.badges.set(decorations, cx);
        self.badges_dark = cx.theme().is_dark();

        // Keep a selection on the same characters.
        if selection.start != selection.end {
            let start = view.map_offset(&self.shown, selection.start);
            let end = view.map_offset(&self.shown, selection.end);
            if let (Some(start), Some(end)) = (start, end) {
                self.viewer
                    .update(cx, |s, cx| s.set_selected_range(start..end, cx));
            }
        }

        // A deferred scroll offset wins over the editor's own scroll-to-caret.
        let target = match (self.follow, line_height, height) {
            (true, Some(line_height), Some(height)) => {
                let content = line_height * view.line_count() as f32;
                Some(point(offset.x, -(content - height).max(px(0.))))
            }
            (true, _, _) => {
                // Never laid out: put the caret on the last line; the first
                // layout scrolls it into view with the real viewport size.
                let end = view.last_line_start();
                self.viewer
                    .update(cx, |s, cx| s.set_selected_range(end..end, cx));
                None
            }
            (false, Some(line_height), _) => {
                // Stay on the same lines.
                let scrolled = -offset.y;
                let row = (scrolled / line_height).floor().max(0.) as usize;
                let within = scrolled - line_height * row as f32;
                let new_row = view.map_row(&self.shown, row).unwrap_or(0);
                Some(point(offset.x, -(line_height * new_row as f32 + within)))
            }
            (false, None, _) => None,
        };
        if let Some(target) = target {
            self.viewer
                .update(cx, |s, cx| s.set_scroll_offset(target, cx));
        }
        self.pending_offset = target;
        self.shown = view;
        self.painted = false;
    }
}

/// `view`'s level badges, coloured from the current theme.
fn badge_decorations(view: &ViewText, cx: &App) -> Vec<TextDecoration> {
    let colors = BadgeColors::new(cx);
    view.badges
        .iter()
        .map(|(range, level)| TextDecoration::new(range.clone(), colors.style(*level)))
        .collect()
}

/// Level-badge styles, from the theme.
struct BadgeColors {
    danger: Hsla,
    warning: Hsla,
    info: Hsla,
    muted: Hsla,
}

impl BadgeColors {
    fn new(cx: &App) -> Self {
        let theme = cx.theme();
        Self {
            danger: theme.danger,
            warning: theme.warning,
            info: theme.info,
            muted: theme.muted_foreground,
        }
    }

    fn style(&self, level: LogLevel) -> HighlightStyle {
        let color = match level {
            LogLevel::Panic | LogLevel::Fatal | LogLevel::Error => self.danger,
            LogLevel::Warn => self.warning,
            LogLevel::Info => self.info,
            LogLevel::Debug | LogLevel::Trace => self.muted,
        };
        HighlightStyle {
            color: Some(color),
            background_color: Some(color.opacity(0.14)),
            font_weight: Some(FontWeight::SEMIBOLD),
            ..Default::default()
        }
    }
}

impl Render for LogsPage {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Catch up on what arrived while hidden — before `painted` flips,
        // so the refresh still treats the layout as not current.
        if std::mem::take(&mut self.stale) {
            let logs = self.logs.clone();
            self.refresh(&logs, window, cx);
        }
        // Light/dark switched (which re-renders every page): the text is
        // unchanged, so recolour the badges in place.
        if cx.theme().is_dark() != self.badges_dark {
            let decorations = badge_decorations(&self.shown, cx);
            self.badges.set(decorations, cx);
            self.badges_dark = cx.theme().is_dark();
        }
        self.painted = true;
        let app_state_entity = self.app_state.clone();
        let logs_entity = self.logs.clone();
        let logs = logs_entity.read(cx);
        let theme = cx.theme();

        let total = logs.entries().len();
        let stopped = self.app_state.read(cx).process.read(cx).is_stopped();
        let threshold = logs.threshold();
        let default_threshold = logs.default_threshold();
        let count_label = log_count_label(logs.visible_count(), total);

        let title_block = div()
            .h_flex()
            .items_center()
            .gap_2()
            .child(page_header(theme, s().logs.title))
            .children(count_label.map(|count| {
                div()
                    .text_sm()
                    .text_color(theme.muted_foreground)
                    .child(count)
            }));

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
        let controls = div()
            .h_flex()
            .items_center()
            .gap_2()
            .child(segmented(
                theme,
                "log-levels",
                levels,
                LEVEL_CHOICES
                    .iter()
                    .position(|(_, _, level)| *level == threshold),
                move |ix, _, cx| {
                    let level = LEVEL_CHOICES[ix].2;
                    logs_for_levels.update(cx, |b, cx| b.set_threshold(level, cx));
                },
            ))
            .child(
                Button::new("logs-clear")
                    .outline()
                    .small()
                    .label(s().logs.clear)
                    .on_click(move |_, _, cx| {
                        app_state_entity.update(cx, |state, cx| state.clear_logs(cx));
                    }),
            );

        let header = div()
            .h_flex()
            .flex_wrap()
            .items_center()
            .justify_between()
            .gap_2()
            .w_full()
            .child(title_block)
            // No lines yet: nothing to filter or clear.
            .when(total > 0, |header| header.child(controls));

        let body = if total == 0 {
            empty_state(
                theme,
                IconName::SquareTerminal,
                s().logs.empty_title,
                s().logs.empty_hint,
            )
            .when(stopped, |this| this.child(connect_button("logs-connect")))
            .into_any_element()
        } else {
            // 只读、无边框、不换行的编辑器:鼠标可拖选 + 复制 + 搜索 + 横向滚动。
            card_frame(theme)
                .flex_1()
                .min_h_0()
                .child(
                    Editor::new(&self.viewer)
                        .appearance(false)
                        .readonly(true)
                        .h_full()
                        .text_sm(),
                )
                .into_any_element()
        };

        div().v_flex().size_full().gap_4().child(header).child(body)
    }
}
