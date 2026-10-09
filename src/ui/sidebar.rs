//! Left navigation rail: the app's mark at the top (unless BoxPilot's
//! title bar shows it), the pages as icons in groups set apart by a short
//! rule, and sing-box's power button at the bottom. Expanded, the rail
//! grows to show each icon's label, the app's name beside the mark and the
//! status beside the button; collapsed, those are tooltips. Pure function —
//! `RootView` supplies the active page, status, badges, whether the rail is
//! expanded, and the navigation callback.

use crate::i18n::s;
use crate::ui::pages::ActivePage;
use crate::ui::theme::PANEL_INSET;
use crate::ui::widgets::text_centered;
use gpui::prelude::FluentBuilder;
use gpui::*;
use gpui_component::{tooltip::Tooltip, Icon, IconName, StyledExt};

/// The collapsed rail: an item and its padding either side — wider on
/// macOS, where the traffic lights sit over it (from 12 px, three 14 px
/// buttons 20 px apart, so out to 66 px): they keep as much room from the
/// content panel as from the window's edge rather than running into it.
pub const RAIL_WIDTH: f32 = if cfg!(target_os = "macos") { 80. } else { 64. };
/// The expanded rail, wide enough for the longest label.
pub const RAIL_EXPANDED_WIDTH: f32 = 208.;
/// A nav item (and the mark and the power button's tile) is a square
/// this size.
const ITEM_SIZE: f32 = 40.;
/// An expanded nav item's icon from its tile's edge (padding and border).
const NAV_INSET: f32 = 11.;
/// The sidebar's power button; the two lines beside it are set tight to
/// make a block its height.
pub const POWER_SIZE: f32 = 32.;
pub const POWER_ICON_SIZE: f32 = 14.;
const ITEM_RADIUS: f32 = 10.;
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
    None,
}

impl StatusDetail {
    /// One line, for the collapsed rail's tooltip.
    pub fn line(&self) -> Option<String> {
        match self {
            StatusDetail::Speed(down, up) => Some(format!("↓ {down}  ↑ {up}")),
            StatusDetail::Profile(name) => Some(name.clone()),
            StatusDetail::None => None,
        }
    }
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

/// The rail's head: the app icon and, `expanded`, the app's name.
pub fn rail_brand(expanded: bool, colors: SidebarColors) -> Div {
    let tile = div()
        .flex_none()
        .size(px(ITEM_SIZE))
        .flex()
        .items_center()
        .justify_center()
        .child(img("brand/icon.png").size(px(34.)));
    div()
        .h_flex()
        .items_center()
        .gap_3()
        .child(tile)
        .when(expanded, |head| {
            head.child(
                div()
                    .text_base()
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(colors.fg)
                    .child("BoxPilot"),
            )
        })
}

/// One nav entry: a square icon tile, or icon and label in a row when
/// `expanded`. The selected one is raised; collapsed, the label is the
/// tooltip.
#[allow(clippy::too_many_arguments)]
fn nav_item(
    page: ActivePage,
    label: &'static str,
    icon: Icon,
    selected: bool,
    badge: bool,
    expanded: bool,
    colors: SidebarColors,
    on_nav: impl Fn(ActivePage, &mut Window, &mut App) + 'static,
) -> Stateful<Div> {
    let (tile, fg, muted) = (colors.tile, colors.fg, colors.muted);
    div()
        .id(("nav", page as usize))
        .relative()
        .flex_none()
        .h(px(ITEM_SIZE))
        .flex()
        .flex_row()
        .items_center()
        .rounded(px(ITEM_RADIUS))
        .border_1()
        .cursor_pointer()
        .map(|item| {
            if expanded {
                item.w_full().px(px(NAV_INSET - 1.)).gap_3()
            } else {
                item.w(px(ITEM_SIZE)).justify_center()
            }
        })
        .map(|item| {
            if selected {
                item.bg(tile)
                    .border_color(colors.tile_border)
                    .text_color(fg)
                    .font_weight(FontWeight::MEDIUM)
            } else {
                item.border_color(transparent_black())
                    .text_color(muted)
                    .hover(move |style| style.bg(tile.opacity(0.6)).text_color(fg))
            }
        })
        .child(
            icon.size(px(ICON_SIZE))
                .flex_none()
                .text_color(if selected { fg } else { muted }),
        )
        .when(expanded, |item| {
            item.child(div().flex_1().min_w_0().truncate().text_sm().child(label))
        })
        .when(badge, |item| {
            let dot = div()
                .size(px(7.))
                .rounded_full()
                .bg(colors.accent)
                .border_1()
                .border_color(colors.rail);
            item.child(if expanded {
                dot.flex_none()
            } else {
                dot.absolute().top(px(6.)).right(px(6.))
            })
        })
        .when(!expanded, |item| {
            item.tooltip(move |window, cx| Tooltip::new(label).build(window, cx))
        })
        .on_click(move |_, window, cx| on_nav(page, window, cx))
}

/// sing-box's power button, Home's in small, and beside it (expanded)
/// two tight lines as tall as the button: the status and the profile, or
/// the speeds. Collapsed, the button stands alone; its tooltip says the
/// rest.
fn status_power(
    power: AnyElement,
    status: SidebarStatus,
    expanded: bool,
    colors: SidebarColors,
) -> Div {
    let tile = div()
        .flex_none()
        .h(px(ITEM_SIZE))
        .flex()
        .flex_row()
        .items_center();
    if !expanded {
        return tile.w(px(ITEM_SIZE)).justify_center().child(power);
    }
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
        StatusDetail::None => vec![label()],
    };
    // The text where the nav labels start; the button a little left of
    // the nav icons' axis, to leave the text room.
    let inset = NAV_INSET + ICON_SIZE / 2. - POWER_SIZE / 2. - 2.;
    tile.w_full()
        .pl(px(inset))
        .gap(px(NAV_INSET + ICON_SIZE + 12. - inset - POWER_SIZE))
        .child(power)
        .child(div().v_flex().flex_1().min_w_0().children(lines))
}

#[allow(clippy::too_many_arguments)]
pub fn sidebar(
    header: Option<AnyElement>,
    expanded: bool,
    active: ActivePage,
    status: SidebarStatus,
    colors: SidebarColors,
    optional: OptionalPages,
    badges: Badges,
    on_nav: impl Fn(ActivePage, &mut Window, &mut App) + Clone + 'static,
    on_toggle: impl Fn(&mut Window, &mut App) + 'static,
    power: AnyElement,
) -> impl IntoElement {
    let nav = &s().nav;
    // Overview / traffic, then what sing-box runs from and says, then the
    // app's own tools and settings.
    let groups: [Vec<(ActivePage, &'static str, Icon)>; 3] = [
        vec![
            (
                ActivePage::Home,
                nav.home,
                Icon::new(IconName::LayoutDashboard),
            ),
            (ActivePage::Groups, nav.groups, Icon::new(IconName::Globe)),
            (
                ActivePage::Connections,
                nav.connections,
                Icon::new(IconName::Network),
            ),
            (
                ActivePage::Tailscale,
                nav.tailscale,
                Icon::new(IconName::Frame),
            ),
            // gauge.svg / shield-check.svg aren't in gpui-component's
            // IconName set; AppAssets serves them.
            (
                ActivePage::Vpn,
                nav.vpn,
                Icon::empty().path("icons/shield-check.svg"),
            ),
        ],
        vec![
            (
                ActivePage::Profiles,
                nav.profiles,
                Icon::new(IconName::GalleryVerticalEnd),
            ),
            (
                ActivePage::Logs,
                nav.logs,
                Icon::new(IconName::SquareTerminal),
            ),
        ],
        vec![
            (
                ActivePage::Tools,
                nav.tools,
                Icon::empty().path("icons/gauge.svg"),
            ),
            (
                ActivePage::Settings,
                nav.settings,
                Icon::new(IconName::Settings),
            ),
        ],
    ];

    let mut items: Vec<AnyElement> = Vec::new();
    for (ix, group) in groups.into_iter().enumerate() {
        if ix > 0 {
            items.push(
                div()
                    .flex_none()
                    .my_2()
                    .h(px(1.))
                    .map(|rule| {
                        if expanded {
                            rule.w_full()
                        } else {
                            rule.w(px(24.))
                        }
                    })
                    .bg(colors.tile_border)
                    .into_any_element(),
            );
        }
        // Tailscale / VPN are offered only while the running config needs
        // them.
        for (page, label, icon) in group.into_iter().filter(|(page, ..)| match page {
            ActivePage::Tailscale => optional.tailscale,
            ActivePage::Vpn => optional.vpn,
            _ => true,
        }) {
            let badge = match page {
                ActivePage::Settings => badges.settings,
                _ => false,
            };
            items.push(
                nav_item(
                    page,
                    label,
                    icon,
                    active == page,
                    badge,
                    expanded,
                    colors,
                    on_nav.clone(),
                )
                .into_any_element(),
            );
        }
    }

    div()
        .flex_none()
        .h_full()
        .w(px(if expanded {
            RAIL_EXPANDED_WIDTH
        } else {
            RAIL_WIDTH
        }))
        .v_flex()
        .when(!expanded, |rail| rail.items_center())
        .px_3()
        .pt_3()
        // The power button's tile ends on the content panel's bottom line.
        .pb(px(PANEL_INSET))
        .gap_1()
        .text_color(colors.fg)
        .when_some(header, |rail, header| rail.child(header).child(div().h_4()))
        .children(items)
        .child(div().flex_1())
        .child(rail_toggle(expanded, colors, on_toggle))
        .child(status_power(power, status, expanded, colors))
}

/// Shows or hides the labels beside the nav icons.
fn rail_toggle(
    expanded: bool,
    colors: SidebarColors,
    on_toggle: impl Fn(&mut Window, &mut App) + 'static,
) -> Stateful<Div> {
    let (tile, fg, muted) = (colors.tile, colors.fg, colors.muted);
    let icon = if expanded {
        IconName::PanelLeftClose
    } else {
        IconName::PanelLeftOpen
    };
    div()
        .id("rail-toggle")
        .flex_none()
        .size(px(ITEM_SIZE))
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(ITEM_RADIUS))
        .cursor_pointer()
        .text_color(muted)
        .hover(move |style| style.bg(tile.opacity(0.6)).text_color(fg))
        .child(Icon::new(icon).size(px(ICON_SIZE)))
        .tooltip(|window, cx| Tooltip::new(s().nav.toggle_sidebar).build(window, cx))
        .on_click(move |_, window, cx| on_toggle(window, cx))
}

/// The colours the sidebar takes from the theme.
#[derive(Clone, Copy)]
pub struct SidebarColors {
    /// The rail itself (the window chrome).
    pub rail: Hsla,
    pub fg: Hsla,
    pub muted: Hsla,
    /// The update dot.
    pub accent: Hsla,
    /// The selected item's raised tile, and the rules between groups.
    pub tile: Hsla,
    pub tile_border: Hsla,
}
