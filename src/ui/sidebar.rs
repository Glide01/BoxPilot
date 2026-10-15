//! Left navigation: the app's mark at the top (unless BoxPilot's title bar
//! shows it), Home, then the pages in groups under small headings — what
//! sing-box proxies (Groups, Connections), the endpoints the running config
//! brings (Tailscale, VPN), what helps troubleshoot it (Logs, Tools) — and at
//! the foot BoxPilot's own Profiles and Settings over sing-box's power
//! button with its status beside it. Always this wide, every label shown:
//! it never collapses to icons. Pure function — `RootView` supplies the
//! active page, status, badges and the navigation callback.

use crate::i18n::s;
use crate::ui::pages::ActivePage;
use crate::ui::theme::PANEL_INSET;
use crate::ui::widgets::text_centered;
use gpui::prelude::FluentBuilder;
use gpui::*;
use gpui_component::{Icon, IconName, StyledExt};

/// The sidebar's width, wide enough for the longest label. On macOS the
/// traffic lights sit over its top (out to 66 px), above the mark.
pub const SIDEBAR_WIDTH: f32 = 208.;
/// A nav item's height (and the mark's and the power button's tile).
const ITEM_SIZE: f32 = 36.;
/// A nav item's icon from its tile's edge (padding and border).
const NAV_INSET: f32 = 11.;
/// The sidebar's power button; the two lines beside it are set tight to
/// make a block its height.
pub const POWER_SIZE: f32 = 32.;
pub const POWER_ICON_SIZE: f32 = 14.;
const ITEM_RADIUS: f32 = 9.;
const ICON_SIZE: f32 = 18.;

/// One speed beside the power button: direction arrow + formatted rate
/// (↓ 1.2 MB/s).
fn footer_speed(icon: &'static str, value: String, colors: SidebarColors) -> Div {
    let value = SharedString::from(value);
    div()
        .h_flex()
        .min_w_0()
        .items_center()
        .gap_1()
        .text_xs()
        .line_height(px(POWER_SIZE / 2.))
        .text_color(colors.fg)
        .child(text_centered(
            Icon::default().path(icon).text_color(colors.muted),
            value.clone(),
        ))
        .child(div().min_w_0().truncate().child(value))
}

/// What the power button has beside it under the status: the live speeds
/// while connected (in the status's place: the filled button says it),
/// else the profile sing-box would start with.
pub enum StatusDetail {
    /// (download, upload), formatted.
    Speed(String, String),
    Profile(String),
    /// Connected, but the sing-box API stopped answering: in place of the
    /// rates it no longer sends.
    Reconnecting,
    None,
}

/// sing-box's state, in words beside the power button.
pub struct SidebarStatus {
    pub label: &'static str,
    pub detail: StatusDetail,
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

/// The app's icon and name, `compact` in BoxPilot's own title bar.
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

/// The sidebar's head: the app icon and name.
pub fn rail_brand(colors: SidebarColors) -> Div {
    div()
        .h_flex()
        .items_center()
        .gap_2p5()
        .pl(px(NAV_INSET - 6.))
        .child(img("brand/icon.png").size(px(30.)).flex_none())
        .child(
            div()
                .text_base()
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(colors.fg)
                .child("BoxPilot"),
        )
}

/// One nav entry: icon and label in a row; the selected one is raised.
fn nav_item(
    page: ActivePage,
    icon: Icon,
    selected: bool,
    badge: bool,
    colors: SidebarColors,
    on_nav: impl Fn(ActivePage, &mut Window, &mut App) + 'static,
) -> Stateful<Div> {
    let (tile, fg, muted) = (colors.tile, colors.fg, colors.muted);
    div()
        .id(("nav", page as usize))
        .flex_none()
        .w_full()
        .h(px(ITEM_SIZE))
        .h_flex()
        .items_center()
        .gap_3()
        .px(px(NAV_INSET - 1.))
        .rounded(px(ITEM_RADIUS))
        .border_1()
        .cursor_pointer()
        .map(|item| {
            if selected {
                item.bg(tile)
                    .border_color(colors.tile_border)
                    .text_color(fg)
                    .font_weight(FontWeight::MEDIUM)
            } else {
                item.border_color(transparent_black())
                    .text_color(fg)
                    .hover(move |style| style.bg(tile.opacity(0.6)))
            }
        })
        .child(
            icon.size(px(ICON_SIZE))
                .flex_none()
                .text_color(if selected { colors.accent } else { muted }),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .text_sm()
                .child(page.label()),
        )
        .when(badge, |item| {
            item.child(
                div()
                    .flex_none()
                    .size(px(7.))
                    .rounded_full()
                    .bg(colors.accent),
            )
        })
        .on_click(move |_, window, cx| on_nav(page, window, cx))
}

/// A small heading over a group of nav entries.
fn section_heading(title: &'static str, colors: SidebarColors) -> Div {
    div()
        .flex_none()
        .pt_4()
        .pb_1()
        .px(px(NAV_INSET))
        .text_xs()
        .text_color(colors.muted)
        .child(title)
}

/// sing-box's power button, Home's in small, and beside it two tight lines
/// as tall as the button: the status and the profile, or the speeds.
fn status_power(power: AnyElement, status: SidebarStatus, colors: SidebarColors) -> Div {
    let label = || {
        div()
            .text_sm()
            .line_height(px(POWER_SIZE / 2. + 1.))
            .font_weight(FontWeight::MEDIUM)
            .text_color(colors.fg)
            .truncate()
            .child(status.label)
    };
    let detail = |text: String| {
        div()
            .text_xs()
            .line_height(px(POWER_SIZE / 2. - 1.))
            .text_color(colors.muted)
            .truncate()
            .child(text)
    };
    // Two lines, at most: a rate each is the room two rates need.
    let lines = match status.detail {
        StatusDetail::Speed(down, up) => vec![
            footer_speed("icons/arrow-down.svg", down, colors),
            footer_speed("icons/arrow-up.svg", up, colors),
        ],
        StatusDetail::Profile(name) => vec![label(), detail(name)],
        StatusDetail::Reconnecting => {
            vec![label(), detail(s().status.reconnecting.to_string())]
        }
        StatusDetail::None => vec![label()],
    };
    // The text where the nav labels start; the button a little left of
    // the nav icons' axis, to leave the text room.
    let inset = NAV_INSET + ICON_SIZE / 2. - POWER_SIZE / 2. - 2.;
    div()
        .flex_none()
        .w_full()
        .h(px(40.))
        .h_flex()
        .items_center()
        .pl(px(inset))
        .gap(px(NAV_INSET + ICON_SIZE + 12. - inset - POWER_SIZE))
        .child(power)
        .child(div().v_flex().flex_1().min_w_0().children(lines))
}

#[allow(clippy::too_many_arguments)]
pub fn sidebar(
    header: Option<AnyElement>,
    active: ActivePage,
    status: SidebarStatus,
    colors: SidebarColors,
    optional: OptionalPages,
    badges: Badges,
    on_nav: impl Fn(ActivePage, &mut Window, &mut App) + Clone + 'static,
    power: AnyElement,
) -> impl IntoElement {
    let nav = &s().nav;
    let item = |page: ActivePage, icon: Icon| {
        let badge = page == ActivePage::Settings && badges.settings;
        nav_item(page, icon, active == page, badge, colors, on_nav.clone()).into_any_element()
    };

    let mut items = vec![item(ActivePage::Home, Icon::new(IconName::LayoutDashboard))];
    items.push(section_heading(nav.section_proxy, colors).into_any_element());
    items.push(item(ActivePage::Groups, Icon::new(IconName::Globe)));
    items.push(item(ActivePage::Connections, Icon::new(IconName::Network)));
    // Tailscale / VPN are offered only while the running config has them;
    // their heading with them.
    if optional.tailscale || optional.vpn {
        items.push(section_heading(nav.section_endpoints, colors).into_any_element());
        if optional.tailscale {
            items.push(item(ActivePage::Tailscale, Icon::new(IconName::Frame)));
        }
        if optional.vpn {
            // gauge.svg / shield-check.svg aren't in gpui-component's
            // IconName set; AppAssets serves them.
            items.push(item(
                ActivePage::Vpn,
                Icon::empty().path("icons/shield-check.svg"),
            ));
        }
    }
    items.push(section_heading(nav.section_troubleshoot, colors).into_any_element());
    items.push(item(ActivePage::Logs, Icon::new(IconName::SquareTerminal)));
    items.push(item(
        ActivePage::Tools,
        Icon::empty().path("icons/gauge.svg"),
    ));

    div()
        .flex_none()
        .h_full()
        .w(px(SIDEBAR_WIDTH))
        .v_flex()
        .px_3()
        .pt_3()
        // The power button's tile ends on the content panel's bottom line.
        .pb(px(PANEL_INSET))
        .gap_0p5()
        .text_color(colors.fg)
        .when_some(header, |rail, header| rail.child(header).child(div().h_3()))
        .children(items)
        .child(div().flex_1().min_h_4())
        .child(item(
            ActivePage::Profiles,
            Icon::new(IconName::GalleryVerticalEnd),
        ))
        .child(item(ActivePage::Settings, Icon::new(IconName::Settings)))
        .child(
            div()
                .flex_none()
                .my_2()
                .mx_1()
                .h(px(1.))
                .bg(colors.tile_border),
        )
        .child(status_power(power, status, colors))
}

/// The colours the sidebar takes from the theme.
#[derive(Clone, Copy)]
pub struct SidebarColors {
    pub fg: Hsla,
    pub muted: Hsla,
    /// The selected item's icon and the update dot.
    pub accent: Hsla,
    /// The selected item's raised tile.
    pub tile: Hsla,
    /// Its border, and the rule over the power button.
    pub tile_border: Hsla,
}
