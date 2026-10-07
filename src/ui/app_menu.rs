//! The macOS menu bar: the app menu (Settings…, Services, Hide, Quit), an
//! Edit menu and a Window menu, with their standard shortcuts.
//!
//! Every item is a gpui action handled app-wide. Quit is the same `cx.quit()`
//! as the tray's and the close dialog's, so it ends in the one cleanup path
//! (ADR 0004: `on_app_quit` drops `AppState`, which stops sing-box and
//! resets the system proxy) — as do the Dock's Quit and logging out, which
//! AppKit turns into the same terminate. Close Window goes through the
//! close button's own decision (Ask / Minimize to tray / Quit). The Edit
//! items are gpui-component's text-input actions, which `Input` already
//! binds to ⌘C / ⌘V / …; the menu only makes them discoverable and
//! clickable, greyed out while no text field has focus.

use crate::actions::{
    CloseWindow, HideApp, HideOtherApps, MinimizeWindow, OpenSettings, Quit, ShowAllApps,
};
use crate::i18n::{current, s};
use crate::state::AppState;
use crate::ui::app_window;
use gpui::{App, Entity, KeyBinding, Menu, MenuItem, OsAction, SystemMenuType};
use gpui_component::input::{Copy, Cut, Paste, Redo, SelectAll, Undo};
use std::cell::Cell;

/// Bind the shortcuts, handle the actions and set the menus, in the UI
/// language; they are set again whenever it changes.
pub fn init(app_state: &Entity<AppState>, cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("cmd-,", OpenSettings, None),
        KeyBinding::new("cmd-h", HideApp, None),
        KeyBinding::new("alt-cmd-h", HideOtherApps, None),
        KeyBinding::new("cmd-q", Quit, None),
        KeyBinding::new("cmd-m", MinimizeWindow, None),
        KeyBinding::new("cmd-w", CloseWindow, None),
    ]);
    // Deferred: a menu action is dispatched inside the active window's
    // update, and these update that window themselves.
    cx.on_action(|_: &OpenSettings, cx| cx.defer(app_window::show_settings));
    cx.on_action(|_: &HideApp, cx| cx.hide());
    cx.on_action(|_: &HideOtherApps, cx| cx.hide_other_apps());
    cx.on_action(|_: &ShowAllApps, cx| cx.unhide_other_apps());
    cx.on_action(|_: &Quit, cx| cx.quit());
    cx.on_action(|_: &MinimizeWindow, cx| cx.defer(app_window::minimize));
    cx.on_action(|_: &CloseWindow, cx| cx.defer(app_window::close));

    set_menus(cx);
    let shown = Cell::new(current());
    cx.observe(app_state, move |_, cx| {
        if shown.get() != current() {
            shown.set(current());
            set_menus(cx);
        }
    })
    .detach();
}

fn set_menus(cx: &mut App) {
    let t = s();
    let m = &t.app_menu;
    cx.set_menus([
        // macOS titles this one with the app's name itself.
        Menu::new("BoxPilot").items([
            MenuItem::action(m.settings, OpenSettings),
            MenuItem::separator(),
            MenuItem::os_submenu(m.services, SystemMenuType::Services),
            MenuItem::separator(),
            MenuItem::action(m.hide, HideApp),
            MenuItem::action(m.hide_others, HideOtherApps),
            MenuItem::action(m.show_all, ShowAllApps),
            MenuItem::separator(),
            MenuItem::action(t.tray.quit, Quit),
        ]),
        Menu::new(m.edit).items([
            MenuItem::os_action(m.undo, Undo, OsAction::Undo),
            MenuItem::os_action(m.redo, Redo, OsAction::Redo),
            MenuItem::separator(),
            MenuItem::os_action(m.cut, Cut, OsAction::Cut),
            MenuItem::os_action(m.copy, Copy, OsAction::Copy),
            MenuItem::os_action(m.paste, Paste, OsAction::Paste),
            MenuItem::os_action(m.select_all, SelectAll, OsAction::SelectAll),
        ]),
        Menu::new(m.window).items([
            MenuItem::action(m.minimize, MinimizeWindow),
            MenuItem::action(m.close_window, CloseWindow),
        ]),
    ]);
}
