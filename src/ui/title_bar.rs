//! The window's top strip, when BoxPilot draws it itself: the app's name on
//! the left, an empty strip to drag the window by, and minimize /
//! maximize-restore / close on the right — in the window chrome's colour,
//! so the sidebar runs up to the top edge and no system bar sits above it.
//!
//! - **Windows** always: the native title bar is hidden
//!   (`appears_transparent`) and the strip's areas are reported to the
//!   OS through gpui's `WindowControlArea` hit-testing — caption (drag,
//!   double-click to maximize, right-click system menu) and min / max /
//!   close buttons. The OS acts on them itself, so Windows 11
//!   snap layouts appear over maximize and close posts `WM_CLOSE` like the
//!   native button did, through `app_window`'s `on_window_should_close`
//!   (ADR 0004's tray behaviour).
//! - **macOS**: no strip. The title bar is transparent and the content runs
//!   under it (`appears_transparent`, full-size content view), and the
//!   system's traffic lights stay, at the top of the sidebar
//!   (`traffic_light_position`), with the app's name under them
//!   (`sidebar_top`) and the content panel running up beside them. The
//!   window opens with `app_owns_titlebar_drag`: AppKit then leaves the
//!   whole top to us — no native drag or double-click, which would race
//!   ours — and the sidebar's top and the window's top edge (`top_edge`)
//!   move the window with `start_window_move` and handle the double-click
//!   with `titlebar_double_click` (the user's Dock setting: zoom, minimize
//!   or nothing).
//! - **Linux** only when the window ends up client-decorated. BoxPilot asks
//!   for server-side decorations, which X11 window managers and most
//!   Wayland compositors (KDE, wlroots) grant; the WM's own title bar then
//!   stays and nothing is drawn here. GNOME's Wayland compositor offers no
//!   server-side decorations, and gpui falls back to client-side ones —
//!   without this strip such a window had no way to be moved, maximized
//!   or closed by the mouse. gpui-component's `Root` already draws the
//!   resize edges and shadow of a client-decorated window.

use crate::ui::app_window;
use gpui::prelude::FluentBuilder;
use gpui::*;
use gpui_component::{ActiveTheme, Icon, IconName, Sizable, StyledExt};

/// The strip's height, Windows 11's caption height.
pub const TITLE_BAR_HEIGHT: Pixels = px(32.);
/// Windows 11's caption buttons are 46 px wide.
const CAPTION_BUTTON_WIDTH: Pixels = px(46.);
const CAPTION_ICON_SIZE: Pixels = px(14.);
/// Where macOS's traffic lights sit: inset like the sidebar items, and
/// centred in a row of `TITLE_BAR_HEIGHT` (AppKit's buttons are 16 px
/// tall).
const TRAFFIC_LIGHTS: Point<Pixels> = point(px(12.), px(8.));
/// The title's inset in the strip, on the sidebar's line.
const TITLE_INSET: Pixels = px(19.);
/// The top padding gpui-component's `Sidebar` puts above its header
/// (`pt_3`).
const SIDEBAR_HEADER_PADDING_TOP: Pixels = px(12.);
/// The close button's hover colour on Windows 11, in both themes.
const CLOSE_HOVER: u32 = 0xC42B1C;

/// The main window's title bar options. The title stays: it names the
/// window in the taskbar, Alt+Tab and the window list.
pub fn titlebar_options() -> TitlebarOptions {
    TitlebarOptions {
        title: Some("BoxPilot".into()),
        appears_transparent: cfg!(any(target_os = "windows", target_os = "macos")),
        traffic_light_position: cfg!(target_os = "macos").then_some(TRAFFIC_LIGHTS),
    }
}

/// Whether AppKit should leave the title bar's dragging and double-click
/// to the window (see the module docs). Ignored off macOS.
pub const APP_OWNS_TITLEBAR_DRAG: bool = cfg!(target_os = "macos");

/// Whether this window gets BoxPilot's strip (see the module docs).
/// Changes only with the window's decorations, and a decoration change
/// re-renders the window.
pub fn draws_strip(window: &Window) -> bool {
    if cfg!(target_os = "windows") {
        true
    } else if cfg!(target_os = "linux") {
        matches!(window.window_decorations(), Decorations::Client { .. })
    } else {
        false
    }
}

/// The strip: `title` (the app's name) at the left of the drag area, the
/// caption buttons at the right.
pub fn title_bar(title: impl IntoElement, window: &mut Window, cx: &mut App) -> impl IntoElement {
    let controls = window.window_controls();
    let maximized = window.is_maximized();
    let title = div().pl(TITLE_INSET).child(title);
    div()
        .id("title-bar")
        .flex()
        .flex_row()
        .flex_shrink_0()
        .w_full()
        .h(TITLE_BAR_HEIGHT)
        .child(drag_area(title, window, cx))
        .when(controls.minimize, |bar| {
            bar.child(caption_button(Caption::Minimize, window, cx))
        })
        .when(controls.maximize, |bar| {
            bar.child(caption_button(
                if maximized {
                    Caption::Restore
                } else {
                    Caption::Maximize
                },
                window,
                cx,
            ))
        })
        .child(caption_button(Caption::Close, window, cx))
}

/// macOS: the sidebar's header, `title` (the app's name) under the traffic
/// lights — alone in full screen, which hides them. It reaches up over the
/// header's padding to the window's top edge and moves the window, as the
/// title bar would.
pub fn sidebar_top(title: impl IntoElement, window: &mut Window, cx: &mut App) -> Stateful<Div> {
    let lights = if window.is_fullscreen() {
        px(0.)
    } else {
        TITLE_BAR_HEIGHT
    };
    let area = div()
        .id("sidebar-top")
        .w_full()
        .mt(-SIDEBAR_HEADER_PADDING_TOP)
        .pt(lights.max(SIDEBAR_HEADER_PADDING_TOP))
        .child(title);
    moves_window(area, "sidebar-top-pressed", window, cx)
}

/// macOS: the window's top edge, `height` tall across the whole window (the
/// gap above the content panel), moves the window as the title bar would.
/// None in full screen, which can't be moved.
pub fn top_edge(height: Pixels, window: &mut Window, cx: &mut App) -> Option<Stateful<Div>> {
    if !cfg!(target_os = "macos") || window.is_fullscreen() {
        return None;
    }
    let edge = div()
        .id("top-edge")
        .absolute()
        .top_0()
        .left_0()
        .right_0()
        .h(height);
    Some(moves_window(edge, "top-edge-pressed", window, cx))
}

/// Everything left of the buttons moves the window.
fn drag_area(title: impl IntoElement, window: &mut Window, cx: &mut App) -> Stateful<Div> {
    let area = div()
        .id("title-bar-drag")
        .flex_1()
        .min_w_0()
        .h_full()
        .h_flex()
        .items_center()
        .child(title);
    moves_window(area, "title-bar-pressed", window, cx)
}

/// Makes `area` move the window, and maximize (Linux) or do the Dock's
/// title-bar double-click action (macOS) on a double-click. `pressed_key`
/// keys the area's own press state.
fn moves_window(
    area: Stateful<Div>,
    pressed_key: &'static str,
    window: &mut Window,
    cx: &mut App,
) -> Stateful<Div> {
    if cfg!(target_os = "windows") {
        return area.window_control_area(WindowControlArea::Drag);
    }
    // Linux and macOS: move on the first pointer motion after a press here —
    // not on the press itself, which would swallow the double-click — and
    // not for a drag that started elsewhere and crossed the area.
    let pressed = window.use_keyed_state(pressed_key, cx, |_, _| false);
    let on_down = pressed.clone();
    let on_up = pressed.clone();
    let on_out = pressed.clone();
    area.on_mouse_down(MouseButton::Left, move |event, window, cx| {
        if event.click_count == 2 {
            on_down.update(cx, |pressed, _| *pressed = false);
            if cfg!(target_os = "macos") {
                window.titlebar_double_click();
            } else {
                window.zoom_window();
            }
        } else {
            on_down.update(cx, |pressed, _| *pressed = true);
        }
    })
    .on_mouse_up(MouseButton::Left, move |_, _, cx| {
        on_up.update(cx, |pressed, _| *pressed = false);
    })
    .on_mouse_down_out(move |_, _, cx| {
        on_out.update(cx, |pressed, _| *pressed = false);
    })
    .on_mouse_move(move |_, window, cx| {
        if *pressed.read(cx) {
            pressed.update(cx, |pressed, _| *pressed = false);
            window.start_window_move();
        }
    })
    .when(cfg!(target_os = "linux"), |area| {
        area.on_mouse_down(MouseButton::Right, |event, window, _| {
            window.show_window_menu(event.position)
        })
    })
}

#[derive(Clone, Copy)]
enum Caption {
    Minimize,
    Maximize,
    Restore,
    Close,
}

impl Caption {
    fn id(self) -> &'static str {
        match self {
            Self::Minimize => "caption-minimize",
            Self::Maximize => "caption-maximize",
            Self::Restore => "caption-restore",
            Self::Close => "caption-close",
        }
    }

    fn icon(self) -> IconName {
        match self {
            Self::Minimize => IconName::WindowMinimize,
            Self::Maximize => IconName::WindowMaximize,
            Self::Restore => IconName::WindowRestore,
            Self::Close => IconName::WindowClose,
        }
    }

    fn area(self) -> WindowControlArea {
        match self {
            Self::Minimize => WindowControlArea::Min,
            Self::Maximize | Self::Restore => WindowControlArea::Max,
            Self::Close => WindowControlArea::Close,
        }
    }
}

fn caption_button(caption: Caption, window: &Window, cx: &App) -> Stateful<Div> {
    let theme = cx.theme();
    // Dimmed while the window is inactive, as the system's own are.
    let fg = if window.is_window_active() {
        theme.foreground
    } else {
        theme.muted_foreground
    };
    let (hover_bg, active_bg, hover_fg) = match caption {
        Caption::Close => {
            let red = Hsla::from(rgb(CLOSE_HOVER));
            (red, red.opacity(0.9), gpui::white())
        }
        _ => (
            theme.foreground.opacity(0.07),
            theme.foreground.opacity(0.12),
            theme.foreground,
        ),
    };
    let button = div()
        .id(caption.id())
        .flex()
        .flex_shrink_0()
        .items_center()
        .justify_center()
        .w(CAPTION_BUTTON_WIDTH)
        .h_full()
        .text_color(fg)
        .hover(move |style| style.bg(hover_bg).text_color(hover_fg))
        .active(move |style| style.bg(active_bg).text_color(hover_fg))
        .child(Icon::new(caption.icon()).with_size(CAPTION_ICON_SIZE));
    if cfg!(target_os = "windows") {
        // The OS handles the click (see the module docs): no listeners
        // here, or the press would never reach it.
        return button.window_control_area(caption.area());
    }
    button
        .on_mouse_down(MouseButton::Left, |_, window, cx| {
            // Not the start of a window drag.
            window.prevent_default();
            cx.stop_propagation();
        })
        .on_click(move |_, window, cx| {
            cx.stop_propagation();
            match caption {
                Caption::Minimize => window.minimize_window(),
                Caption::Maximize | Caption::Restore => window.zoom_window(),
                Caption::Close => app_window::request_close(window, cx),
            }
        })
}
