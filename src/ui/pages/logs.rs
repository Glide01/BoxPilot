use crate::core::presentation::log_count_label;
use crate::core::settings::{matches_filter, LogEntry, LogFilter};
use crate::state::AppState;
use crate::ui::card_frame;
use crate::ui::widgets::{empty_card, page_header};
use gpui::*;
use gpui_component::{
    button::{Button, ButtonVariants},
    input::{Input, InputState},
    ActiveTheme, IconName, Sizable, StyledExt,
};
use std::collections::VecDeque;

/// 日志页:标题行(标题 + 计数 + 过滤 pills + 清空)在内容卡片**外面**(与
/// Groups 页一致);卡片内是一个只读的多行 `Input`,承载(按当前过滤后的)
/// 日志文本——这样用户能用鼠标拖选、复制(⌘/Ctrl+C 或右键菜单),并在关掉
/// 软换行后横向滚动查看长行。卡片 `flex_1 + min_h_0` 占满标题行外的剩余高度。
///
/// 只读靠 `Input::disabled(true)`:gpui-component 里 `disabled` 只拦截编辑动作,
/// 聚焦 / 选中 / 复制的监听器仍无条件挂着;`appearance(false)` 去掉边框背景,
/// 看起来就是一块普通文本面板。日志内容在 `observe_in` 里随 LogBuffer / 过滤
/// 变化重新灌入(`set_value`,仅在文本真的变化时调用)。注意 `set_value` 会把
/// 滚动条复位到顶部,所以流式刷新时视图会回到顶部——停止后内容稳定,选中 /
/// 复制 / 滚动都不受影响。
pub struct LogsPage {
    app_state: Entity<AppState>,
    /// 只读多行 Input,承载日志文本,供鼠标选中 / 复制 / 横向滚动。
    viewer: Entity<InputState>,
}

impl LogsPage {
    pub fn new(app_state: Entity<AppState>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let logs = app_state.read(cx).logs.clone();

        let viewer = cx.new(|cx| InputState::new(window, cx).multi_line(true).soft_wrap(false));

        // 初始灌入已有日志(启动时通常为空)。
        let initial = {
            let lb = logs.read(cx);
            Self::compose_text(&lb.entries, lb.filter)
        };
        viewer.update(cx, |s, cx| s.set_value(initial, window, cx));

        // LogBuffer 变化(新日志 / 清空 / 切换过滤)→ 把最新文本灌进只读 viewer
        // (仅文本真的变了才 set_value,避免无谓的滚动复位),并重渲染标题计数。
        cx.observe_in(&logs, window, {
            let viewer = viewer.clone();
            move |_, logs, window, cx| {
                let text = {
                    let lb = logs.read(cx);
                    Self::compose_text(&lb.entries, lb.filter)
                };
                let changed = viewer.read(cx).value().as_ref() != text.as_str();
                if changed {
                    viewer.update(cx, |s, cx| s.set_value(text, window, cx));
                }
                cx.notify();
            }
        })
        .detach();

        Self { app_state, viewer }
    }

    /// 把(按 `filter` 过滤后的)日志条目拼成可选中的纯文本,一行一条。
    /// `entry.message` 已含 `[STDOUT]/[STDERR]` 前缀和原始行内容,直接拼接即可。
    fn compose_text(entries: &VecDeque<LogEntry>, filter: LogFilter) -> String {
        let mut out = String::new();
        for entry in entries.iter().filter(|e| matches_filter(e.level, filter)) {
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(&entry.message);
        }
        out
    }
}

fn filter_pill(
    id: &'static str,
    label: &'static str,
    active: bool,
    on_click: impl Fn(&mut App) + 'static,
) -> Button {
    let mut b = Button::new(id).label(label).small();
    if active {
        b = b.primary();
    } else {
        b = b.ghost();
    }
    b.on_click(move |_, _, cx| on_click(cx))
}

impl Render for LogsPage {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let app_state = self.app_state.read(cx);
        let logs_entity = app_state.logs.clone();
        let logs = logs_entity.read(cx);
        let theme = cx.theme();

        let total = logs.entries.len();
        let filter = logs.filter;

        let filtered_count = if filter == LogFilter::All {
            total
        } else {
            logs.visible_count()
        };
        let count_label = log_count_label(filtered_count, total);

        let logs_for_clear = logs_entity.clone();

        let title_block = div()
            .h_flex()
            .items_center()
            .gap_2()
            .child(page_header(theme, "Logs"))
            .child(
                div()
                    .text_sm()
                    .text_color(theme.muted_foreground)
                    .child(count_label),
            );

        let mut controls = div().h_flex().items_center().gap_2();
        for (id, label, target) in [
            ("filter-all", "All", LogFilter::All),
            ("filter-warn", "Warn", LogFilter::Warn),
            ("filter-error", "Error", LogFilter::Error),
        ] {
            let logs_for_pill = logs_entity.clone();
            controls = controls.child(filter_pill(id, label, filter == target, move |cx| {
                logs_for_pill.update(cx, |b, cx| b.set_filter(target, cx));
            }));
        }
        let controls = controls
            .child(div().w_px().h_4().bg(theme.border).mx_1())
            .child(
                Button::new("logs-clear")
                    .ghost()
                    .small()
                    .label("Clear")
                    .on_click(move |_, _, cx| {
                        logs_for_clear.update(cx, |b, cx| b.clear(cx));
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
            .child(controls);

        let body = if total == 0 {
            empty_card(
                theme,
                IconName::SquareTerminal,
                "No logs yet",
                "Connect to start streaming sing-box output.",
            )
            .into_any_element()
        } else {
            // 只读、无边框、不换行的多行 Input:鼠标可拖选 + 复制 + 横向滚动。
            card_frame(theme)
                .flex_1()
                .min_h_0()
                .child(
                    Input::new(&self.viewer)
                        .appearance(false)
                        .disabled(true)
                        .h_full()
                        .text_sm()
                        .font_family("monospace"),
                )
                .into_any_element()
        };

        div()
            .v_flex()
            .size_full()
            .gap_4()
            .child(header)
            .child(body)
    }
}
