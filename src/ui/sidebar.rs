//! Left navigation column: the app's icon and name (unless the title bar
//! shows them), the page items, and a status tile at the bottom (dot +
//! status, then the live up/down speeds while connected or the active
//! profile's name otherwise). Pure function —
//! `RootView` supplies the active page, status, detail line, badges, and the
//! navigation callback.

use crate::i18n::s;
use crate::ui::pages::ActivePage;
use crate::ui::theme::{CARD_RADIUS, PANEL_INSET};
use gpui::prelude::FluentBuilder;
use gpui::*;
use gpui_component::{
    sidebar::{Sidebar, SidebarHeader, SidebarMenu, SidebarMenuItem},
    Icon, IconName, Sizable, StyledExt,
};

/// Bottom padding gpui-component's `Sidebar` puts under its footer slot
/// (`pb_3`).
const SIDEBAR_FOOTER_PADDING_BOTTOM: f32 = 12.;

/// 侧边栏底部单个网速读数:方向箭头 + 格式化速率(如 ↓ 1.2 MB/s)。
fn footer_speed(icon: &'static str, value: String, color: Hsla) -> impl IntoElement {
    div()
        .h_flex()
        .flex_1()
        .min_w_0()
        .items_center()
        .gap_1()
        .child(
            Icon::default()
                .path(icon)
                .with_size(px(12.))
                .flex_none()
                .text_color(color),
        )
        .child(
            div()
                .min_w_0()
                .truncate()
                .text_xs()
                .text_color(color)
                .child(value),
        )
}

/// What the footer's status tile shows under the status: the live speeds
/// while connected, else the profile sing-box would start with.
pub enum StatusDetail {
    /// (download, upload), formatted.
    Speed(String, String),
    Profile(String),
    None,
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

/// The app's icon and name: at the top of the sidebar, or `compact` in
/// BoxPilot's own title bar.
pub fn brand(compact: bool) -> Div {
    let (icon, name) = if compact {
        (px(18.), div().text_sm())
    } else {
        (px(24.), div().text_base())
    };
    div()
        .h_flex()
        .items_center()
        .gap_2()
        .child(img("brand/icon.png").size(icon).flex_none())
        .child(name.font_weight(FontWeight::SEMIBOLD).child("BoxPilot"))
}

#[allow(clippy::too_many_arguments)]
pub fn sidebar(
    show_brand: bool,
    active: ActivePage,
    dot_color: Hsla,
    status_label: &'static str,
    detail: StatusDetail,
    colors: SidebarColors,
    optional: OptionalPages,
    badges: Badges,
    on_nav: impl Fn(ActivePage, &mut Window, &mut App) + Clone + 'static,
) -> impl IntoElement {
    let nav = &s().nav;
    let items = [
        (ActivePage::Home, nav.home, Icon::new(IconName::LayoutDashboard)),
        (ActivePage::Groups, nav.groups, Icon::new(IconName::Globe)),
        (ActivePage::Connections, nav.connections, Icon::new(IconName::Network)),
        (ActivePage::Tailscale, nav.tailscale, Icon::new(IconName::Frame)),
        // gauge.svg / shield-check.svg aren't in gpui-component's IconName
        // set; AppAssets serves them.
        (ActivePage::Vpn, nav.vpn, Icon::empty().path("icons/shield-check.svg")),
        (ActivePage::Profiles, nav.profiles, Icon::new(IconName::GalleryVerticalEnd)),
        (ActivePage::Logs, nav.logs, Icon::new(IconName::SquareTerminal)),
        (ActivePage::Tools, nav.tools, Icon::empty().path("icons/gauge.svg")),
        (ActivePage::Settings, nav.settings, Icon::new(IconName::Settings)),
    ];
    // Tailscale / VPN are offered only while the running config needs them.
    let items = items.into_iter().filter(|(page, ..)| match page {
        ActivePage::Tailscale => optional.tailscale,
        ActivePage::Vpn => optional.vpn,
        _ => true,
    });

    // Keyed by the active page so a page switch starts the items with fresh
    // element state. gpui only tracks hover on an element while it has a
    // hover style, and gpui-component drops that style on the active item:
    // the item clicked while hovered would otherwise keep a stale hover flag
    // and show hover text colours after it stops being active.
    let badge_color = colors.badge;
    Sidebar::new(("nav", active as usize))
        .collapsible(false)
        .w(px(208.))
        // With BoxPilot's own title bar the name is up there instead.
        .when(show_brand, |sidebar| {
            sidebar.header(SidebarHeader::new().child(brand(false).px_1().py_1()))
        })
        .child(SidebarMenu::new().children(items.map(|(page, label, icon)| {
            let on_nav = on_nav.clone();
            let badge = match page {
                ActivePage::Settings => badges.settings,
                _ => false,
            };
            let selected = active == page;
            SidebarMenuItem::new(label)
                .icon(if selected {
                    icon.text_color(colors.accent)
                } else {
                    icon
                })
                .active(selected)
                .when(badge, |item| {
                    item.suffix(move |_, _| {
                        div()
                            .flex_shrink_0()
                            .size(px(6.))
                            .rounded_full()
                            .bg(badge_color)
                    })
                })
                .on_click(move |_, window, cx| on_nav(page, window, cx))
        })))
        // The tile goes in as the footer itself, not in a `SidebarFooter`:
        // that adds 8px of padding (so the tile sat inset from the nav
        // items) and a hover background, which made the tile look
        // clickable. The footer slot is inset 12px like the items; the
        // negative margin takes its 12px bottom padding down to the
        // `PANEL_INSET` the content panel keeps from the window's bottom
        // edge (with or without our title bar), so the tile ends on the
        // panel's line.
        .footer(
            // 状态卡:状态点 + 标签,下一行是实时网速(已连接)或当前 profile。
            div()
                .v_flex()
                .w_full()
                .mb(px(PANEL_INSET - SIDEBAR_FOOTER_PADDING_BOTTOM))
                .gap_1()
                .p_3()
                .rounded(px(CARD_RADIUS))
                .bg(colors.tile)
                .border_1()
                .border_color(colors.tile_border)
                .child(
                    div()
                        .h_flex()
                        .items_center()
                        .gap_2()
                        .child(div().size(px(8.)).rounded_full().bg(dot_color))
                        .child(
                            div()
                                .text_sm()
                                .font_weight(FontWeight::MEDIUM)
                                .child(status_label),
                        ),
                )
                .map(|tile| match detail {
                    StatusDetail::Speed(down, up) => tile.child(
                        div()
                            .h_flex()
                            .items_center()
                            .gap_3()
                            .child(footer_speed("icons/arrow-down.svg", down, colors.muted))
                            .child(footer_speed("icons/arrow-up.svg", up, colors.muted)),
                    ),
                    StatusDetail::Profile(name) => tile.child(
                        div()
                            .text_xs()
                            .text_color(colors.muted)
                            .truncate()
                            .child(name),
                    ),
                    StatusDetail::None => tile,
                }),
        )
}

/// The colours the sidebar takes from the theme.
#[derive(Clone, Copy)]
pub struct SidebarColors {
    /// The selected entry's icon, and the update dot.
    pub accent: Hsla,
    pub badge: Hsla,
    /// Secondary text in the status tile.
    pub muted: Hsla,
    /// The status tile.
    pub tile: Hsla,
    pub tile_border: Hsla,
}
