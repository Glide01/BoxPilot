//! Left navigation column: app title header, five page items, and a footer
//! holding a live up/down network-speed row (only while connected) above the
//! connection-status row (dot + label). Pure function — `RootView` supplies the
//! active page, status, speeds, and the navigation callback.

use crate::ui::pages::ActivePage;
use gpui::prelude::FluentBuilder;
use gpui::*;
use gpui_component::{
    sidebar::{Sidebar, SidebarFooter, SidebarHeader, SidebarMenu, SidebarMenuItem},
    Icon, IconName, Sizable, StyledExt,
};

/// 侧边栏底部单个网速读数:方向箭头 + 格式化速率(如 ↓ 1.2 MB/s)。
fn footer_speed(icon: &'static str, value: String, color: Hsla) -> impl IntoElement {
    div()
        .h_flex()
        .items_center()
        .gap_1()
        .child(Icon::default().path(icon).with_size(px(12.)).text_color(color))
        .child(div().text_xs().text_color(color).child(value))
}

pub fn sidebar(
    active: ActivePage,
    dot_color: Hsla,
    status_label: &'static str,
    // (download, upload) 已格式化速率;仅在已连接时为 `Some`,否则隐藏网速行。
    speed: Option<(String, String)>,
    speed_color: Hsla,
    on_nav: impl Fn(ActivePage, &mut Window, &mut App) + Clone + 'static,
) -> impl IntoElement {
    let items = [
        (ActivePage::Home, "Home", IconName::LayoutDashboard),
        (ActivePage::Groups, "Groups", IconName::Globe),
        (ActivePage::Profiles, "Profiles", IconName::GalleryVerticalEnd),
        (ActivePage::Logs, "Logs", IconName::SquareTerminal),
        (ActivePage::Settings, "Settings", IconName::Settings),
    ];

    Sidebar::new("nav")
        .collapsible(false)
        .w(px(190.))
        .header(
            SidebarHeader::new().child(
                div()
                    .text_base()
                    .font_weight(FontWeight::BOLD)
                    .child("BoxPilot"),
            ),
        )
        .child(SidebarMenu::new().children(items.map(|(page, label, icon)| {
            let on_nav = on_nav.clone();
            SidebarMenuItem::new(label)
                .icon(icon)
                .active(active == page)
                .on_click(move |_, window, cx| on_nav(page, window, cx))
        })))
        .footer(
            SidebarFooter::new().child(
                div()
                    .v_flex()
                    .w_full()
                    .gap_1()
                    // 网速行(仅已连接显示),↓/↑ 上下堆叠,在连接状态行上方。
                    .when_some(speed, |this, (down, up)| {
                        this.child(footer_speed("icons/arrow-down.svg", down, speed_color))
                            .child(footer_speed("icons/arrow-up.svg", up, speed_color))
                    })
                    // 连接状态行:状态点 + 标签。
                    .child(
                        div()
                            .h_flex()
                            .items_center()
                            .gap_2()
                            .child(div().w_2().h_2().rounded_full().bg(dot_color))
                            .child(div().text_sm().child(status_label)),
                    ),
            ),
        )
}
