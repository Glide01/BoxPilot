//! 分组页:分组纵向堆叠,每组 = 标题行(chevron + 组名 + 当前节点 + Test
//! 按钮)+ 两列节点卡片网格(名字 + 延迟徽标 / 协议类型)。点标题行左半区
//! 折叠/展开该组(默认全部折叠,展开状态仅存内存)。运行中(Clash API)点卡片
//! 切换节点、可整组测速;sing-box 未启动(或启动后连不上 Clash API)时
//! 节点列表为空,显示空状态。

use crate::core::clash_api::{classify_delay, DelayLevel, GroupKind};
use crate::state::{AppState, DelayState, GroupSource, ProxyGroups};
use crate::ui::card_frame;
use crate::ui::widgets::{empty_card, page_header, pill, PillTone};
use gpui::prelude::FluentBuilder;
use gpui::*;
use std::collections::HashSet;
use gpui_component::{
    button::Button,
    scroll::ScrollableElement,
    theme::Theme,
    ActiveTheme, Disableable, Icon, IconName, Sizable, StyledExt,
};

pub struct GroupsPage {
    proxy_groups: Entity<ProxyGroups>,
    /// 展开的组名,空集 = 全部折叠(默认)。仅内存:切页保留(页面实体
    /// 常驻),重启回到全折叠。按名字键控,分组列表重建后状态自然延续,
    /// 新出现的组默认折叠;陈旧键无害。
    expanded: HashSet<String>,
}

impl GroupsPage {
    pub fn new(app_state: Entity<AppState>, cx: &mut Context<Self>) -> Self {
        let proxy_groups = app_state.read(cx).proxy_groups.clone();
        cx.observe(&proxy_groups, |_, _, cx| cx.notify()).detach();
        Self {
            proxy_groups,
            expanded: HashSet::new(),
        }
    }

    fn toggle_group(&mut self, group: &str, cx: &mut Context<Self>) {
        if !self.expanded.remove(group) {
            self.expanded.insert(group.to_string());
        }
        cx.notify();
    }
}

fn delay_color(state: DelayState, theme: &Theme) -> Hsla {
    match state {
        DelayState::Ok(ms) => match classify_delay(ms) {
            DelayLevel::Fast => theme.success,
            DelayLevel::Medium => theme.warning,
            DelayLevel::Slow => theme.danger,
        },
        DelayState::Timeout => theme.danger,
    }
}

fn delay_label(state: DelayState) -> SharedString {
    match state {
        DelayState::Ok(ms) => format!("{}ms", ms).into(),
        DelayState::Timeout => "timeout".into(),
    }
}

/// 单张节点卡片。`live` = 运行中(前景色+测速徽标);`selectable` = 可点切换
/// (selector 组才为真,urltest 自动选路只读);`delay` 为 None 不显示徽标。
#[allow(clippy::too_many_arguments)]
fn node_card(
    id: SharedString,
    group_name: String,
    node: String,
    node_type: String,
    selected: bool,
    live: bool,
    selectable: bool,
    delay: Option<DelayState>,
    proxy_groups: Entity<ProxyGroups>,
    theme: &Theme,
) -> Stateful<Div> {
    let primary = theme.primary;
    let name_color = if live {
        theme.foreground
    } else {
        theme.muted_foreground
    };

    let mut name_row = div()
        .h_flex()
        .items_center()
        .justify_between()
        .gap_2()
        .child(
            div()
                .flex_1()
                .min_w_0()
                .overflow_hidden()
                .text_ellipsis()
                .whitespace_nowrap()
                .text_sm()
                .text_color(name_color)
                .child(node.clone()),
        );
    if let Some(delay) = delay {
        name_row = name_row.child(
            div()
                .text_xs()
                .text_color(delay_color(delay, theme))
                .child(delay_label(delay)),
        );
    }

    let mut card = div()
        .id(id)
        .flex_1()
        .min_w_0()
        .px_3()
        .py_2()
        .rounded_md()
        .border_1()
        .border_color(if selected { theme.primary } else { theme.border })
        .bg(if selected {
            theme.primary.opacity(0.08)
        } else {
            theme.background
        })
        .v_flex()
        .gap_1()
        .child(name_row)
        .child(
            div()
                .text_xs()
                .text_color(theme.muted_foreground)
                .child(node_type),
        );

    if selectable {
        card = card
            .cursor_pointer()
            .hover(move |s| s.border_color(primary))
            .on_click(move |_, _, cx| {
                proxy_groups.update(cx, |state, cx| {
                    state.select(group_name.clone(), node.clone(), cx)
                });
            });
    }
    card
}

impl Render for GroupsPage {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let groups_entity = self.proxy_groups.clone();
        let state = self.proxy_groups.read(cx);
        let enabled = state.source == GroupSource::Api;
        let groups = state.groups.clone();
        let node_types = state.node_types.clone();
        let delays = state.delays.clone();
        let testing = state.testing.clone();
        let theme = cx.theme();

        let title = page_header(theme, "Groups");

        let body = if groups.is_empty() {
            empty_card(
                theme,
                IconName::Globe,
                "No node groups",
                "Connect to see node groups here.",
            )
            .into_any_element()
        } else {
            let sections =
                div()
                    .v_flex()
                    .gap_3()
                    .children(groups.iter().enumerate().map(|(gi, group)| {
                        let is_testing = testing.contains(&group.name);
                        // urltest 自动选路:展示但不可手选(API 也会拒绝 PUT)。
                        let selectable = enabled && group.kind == GroupKind::Selector;
                        let test_btn = {
                            let proxy_groups = groups_entity.clone();
                            let name = group.name.clone();
                            Button::new(SharedString::from(format!("delay-test-{}", gi)))
                                .outline()
                                .small()
                                .label("Test")
                                .loading(is_testing)
                                .disabled(!enabled || is_testing)
                                .on_click(move |_, _, cx| {
                                    proxy_groups.update(cx, |state, cx| {
                                        state.test_delay(name.clone(), cx)
                                    });
                                })
                        };
                        let is_collapsed = !self.expanded.contains(&group.name);
                        let toggle = cx.listener({
                            let name = group.name.clone();
                            move |this: &mut Self, _: &ClickEvent, _, cx| {
                                this.toggle_group(&name, cx)
                            }
                        });
                        // 左半区(chevron+组名+当前节点)是折叠开关;Test
                        // 按钮独立在右侧,不嵌套在点击区内以免触发折叠。
                        let header = div()
                            .h_flex()
                            .items_center()
                            .gap_2()
                            .child(
                                div()
                                    .id(SharedString::from(format!("group-header-{}", gi)))
                                    .h_flex()
                                    .items_center()
                                    .gap_2()
                                    .flex_1()
                                    .min_w_0()
                                    .cursor_pointer()
                                    .on_click(toggle)
                                    .child(
                                        Icon::new(if is_collapsed {
                                            IconName::ChevronRight
                                        } else {
                                            IconName::ChevronDown
                                        })
                                        .small()
                                        .text_color(theme.muted_foreground),
                                    )
                                    .child(
                                        div()
                                            .text_sm()
                                            .font_weight(FontWeight::SEMIBOLD)
                                            .text_color(theme.foreground)
                                            .child(group.name.clone()),
                                    )
                                    .when(group.kind == GroupKind::UrlTest, |this| {
                                        this.child(pill(theme, PillTone::Muted, "auto"))
                                    })
                                    .child(
                                        div()
                                            .flex_1()
                                            .min_w_0()
                                            .overflow_hidden()
                                            .text_ellipsis()
                                            .whitespace_nowrap()
                                            .text_xs()
                                            .text_color(theme.muted_foreground)
                                            .child(group.now.clone()),
                                    ),
                            )
                            .child(test_btn);

                        let mut section = card_frame(theme).child(header);
                        if !is_collapsed {
                            let rows = group.all.chunks(2).enumerate().map(|(row_ix, pair)| {
                                let mut row = div().h_flex().gap_2().children(
                                    pair.iter().enumerate().map(|(col_ix, node)| {
                                        node_card(
                                            SharedString::from(format!(
                                                "node-{}-{}",
                                                gi,
                                                row_ix * 2 + col_ix
                                            )),
                                            group.name.clone(),
                                            node.clone(),
                                            node_types.get(node).cloned().unwrap_or_default(),
                                            *node == group.now,
                                            enabled,
                                            selectable,
                                            if enabled { delays.get(node).copied() } else { None },
                                            groups_entity.clone(),
                                            theme,
                                        )
                                    }),
                                );
                                if pair.len() == 1 {
                                    row = row.child(div().flex_1());
                                }
                                row
                            });
                            section = section.child(div().v_flex().gap_2().children(rows));
                        }
                        section
                    }));

            div()
                .v_flex()
                .flex_1()
                .min_h_0()
                .child(div().flex_1().min_h_0().child(sections.overflow_y_scrollbar()))
                .into_any_element()
        };

        div()
            .v_flex()
            .size_full()
            .gap_4()
            .child(title)
            .child(body)
    }
}
