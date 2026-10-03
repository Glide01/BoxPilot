//! Left navigation column: app title header, the page items, and a footer
//! holding a live up/down network-speed row (only while connected) above the
//! connection-status row (dot + label). Pure function — `RootView` supplies the
//! active page, status, speeds, badges, and the navigation callback.

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

/// Sidebar entries offered only while the running config needs them.
#[derive(Clone, Copy, Default)]
pub struct OptionalPages {
    /// The running config has Tailscale endpoints.
    pub tailscale: bool,
    /// The running config has OpenConnect / OpenVPN endpoints or USB/IP
    /// servers.
    pub vpn: bool,
}

/// Dots on sidebar items asking for attention.
#[derive(Clone, Copy, Default)]
pub struct Badges {
    /// A BoxPilot update is available and not skipped
    /// (`AppState::update_available`).
    pub settings: bool,
}

pub fn sidebar(
    active: ActivePage,
    dot_color: Hsla,
    status_label: &'static str,
    // (download, upload) 已格式化速率;仅在已连接时为 `Some`,否则隐藏网速行。
    speed: Option<(String, String)>,
    speed_color: Hsla,
    optional: OptionalPages,
    badges: Badges,
    badge_color: Hsla,
    on_nav: impl Fn(ActivePage, &mut Window, &mut App) + Clone + 'static,
) -> impl IntoElement {
    let items = [
        (ActivePage::Home, "Home", Icon::new(IconName::LayoutDashboard)),
        (ActivePage::Groups, "Groups", Icon::new(IconName::Globe)),
        (ActivePage::Connections, "Connections", Icon::new(IconName::Network)),
        (ActivePage::Tailscale, "Tailscale", Icon::new(IconName::Frame)),
        // gauge.svg / shield-check.svg aren't in gpui-component's IconName
        // set; AppAssets serves them.
        (ActivePage::Vpn, "VPN", Icon::empty().path("icons/shield-check.svg")),
        (ActivePage::Profiles, "Profiles", Icon::new(IconName::GalleryVerticalEnd)),
        (ActivePage::Logs, "Logs", Icon::new(IconName::SquareTerminal)),
        (ActivePage::Tools, "Tools", Icon::empty().path("icons/gauge.svg")),
        (ActivePage::Settings, "Settings", Icon::new(IconName::Settings)),
    ];
    // Tailscale / VPN are offered only while the running config needs them.
    let items = items.into_iter().filter(|(page, ..)| match page {
        ActivePage::Tailscale => optional.tailscale,
        ActivePage::Vpn => optional.vpn,
        _ => true,
    });

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
            let badge = match page {
                ActivePage::Settings => badges.settings,
                _ => false,
            };
            SidebarMenuItem::new(label)
                .icon(icon)
                .active(active == page)
                .when(badge, |item| {
                    item.suffix(move |_, _| {
                        div()
                            .flex_shrink_0()
                            .size_2()
                            .rounded_full()
                            .bg(badge_color)
                    })
                })
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
