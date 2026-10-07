//! Windows tray and macOS menu bar icon: `tray-icon` + its `muda` menu.
//! Built on the UI thread. On Windows the icon's hidden window is pumped by
//! gpui's own `GetMessageW` loop; left click opens the window, right click
//! shows the menu. On macOS it is an `NSStatusItem` on gpui's main run
//! loop; any click shows the menu, as menu bar icons do there, and "Show
//! BoxPilot" is its first item.
//!
//! tray-icon reports clicks from inside its window procedure (Windows) or
//! AppKit's menu actions (macOS), through process-wide handlers: they only
//! look up and forward a `TrayCommand` into the channel and never touch
//! gpui.

#[cfg(target_os = "macos")]
use super::icon::padded_tray_icon;
#[cfg(target_os = "windows")]
use super::icon::tray_icon;
use super::model::{menu_entries, MenuEntry, TrayCommand, TraySnapshot};
use futures_channel::mpsc::UnboundedSender;
use std::collections::HashMap;
use std::sync::{Arc, Mutex, Once};
use tray_icon::menu::{
    CheckMenuItem, IsMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem, Submenu,
};
use tray_icon::{Icon, TrayIcon, TrayIconBuilder};
#[cfg(target_os = "windows")]
use tray_icon::{MouseButton, MouseButtonState, TrayIconEvent};

/// Menu item id → the command it sends, for the current menu. Shared with
/// the menu event handler.
type CommandMap = Arc<Mutex<HashMap<String, TrayCommand>>>;

/// Handles to the items of the menu on display, for in-place updates.
enum BuiltItem {
    Item(MenuItem),
    Check(CheckMenuItem),
    Radio(Vec<CheckMenuItem>),
    Separator,
}

struct BuiltMenu {
    /// Which items exist, by id; equal shapes update in place.
    shape: Vec<String>,
    items: Vec<BuiltItem>,
}

pub struct Backend {
    tray: TrayIcon,
    menu: BuiltMenu,
    commands_by_id: CommandMap,
    /// `[disconnected, connected]`.
    icons: [Icon; 2],
    connected: bool,
    tooltip: String,
}

/// `&` starts a mnemonic in Win32 menu text, and muda strips it on macOS
/// too; profile names may contain one.
fn menu_text(label: &str) -> String {
    label.replace('&', "&&")
}

fn check_id(label: &str) -> String {
    format!("check:{label}")
}

fn shape(entries: &[MenuEntry]) -> Vec<String> {
    let mut shape = Vec::new();
    for entry in entries {
        match entry {
            MenuEntry::Item { command, .. } => shape.push(command.menu_id()),
            MenuEntry::Check { label, .. } => shape.push(check_id(label)),
            MenuEntry::Radio { label, options, .. } => {
                shape.push(format!("radio:{label}"));
                shape.extend(
                    options
                        .iter()
                        .map(|(label, command)| format!("{}={label}", command.menu_id())),
                );
            }
            MenuEntry::Separator => shape.push("-".into()),
        }
    }
    shape
}

fn command_map(entries: &[MenuEntry]) -> HashMap<String, TrayCommand> {
    let mut map = HashMap::new();
    for entry in entries {
        match entry {
            MenuEntry::Item { command, .. } => {
                map.insert(command.menu_id(), command.clone());
            }
            MenuEntry::Check { label, command, .. } => {
                map.insert(check_id(label), command.clone());
            }
            MenuEntry::Radio { options, .. } => {
                for (_, command) in options {
                    map.insert(command.menu_id(), command.clone());
                }
            }
            MenuEntry::Separator => {}
        }
    }
    map
}

fn build_menu(entries: &[MenuEntry]) -> Result<(Menu, BuiltMenu), String> {
    let menu = Menu::new();
    let mut items = Vec::new();
    for entry in entries {
        match entry {
            MenuEntry::Item {
                label,
                enabled,
                command,
            } => {
                let item = MenuItem::with_id(command.menu_id(), menu_text(label), *enabled, None);
                menu.append(&item).map_err(|e| e.to_string())?;
                items.push(BuiltItem::Item(item));
            }
            MenuEntry::Check { label, checked, .. } => {
                let item =
                    CheckMenuItem::with_id(check_id(label), menu_text(label), true, *checked, None);
                menu.append(&item).map_err(|e| e.to_string())?;
                items.push(BuiltItem::Check(item));
            }
            MenuEntry::Radio {
                label,
                options,
                selected,
            } => {
                let options: Vec<CheckMenuItem> = options
                    .iter()
                    .enumerate()
                    .map(|(ix, (label, command))| {
                        CheckMenuItem::with_id(
                            command.menu_id(),
                            menu_text(label),
                            true,
                            *selected == Some(ix),
                            None,
                        )
                    })
                    .collect();
                let refs: Vec<&dyn IsMenuItem> =
                    options.iter().map(|item| item as &dyn IsMenuItem).collect();
                let submenu = Submenu::with_items(menu_text(label), true, &refs)
                    .map_err(|e| e.to_string())?;
                menu.append(&submenu).map_err(|e| e.to_string())?;
                items.push(BuiltItem::Radio(options));
            }
            MenuEntry::Separator => {
                menu.append(&PredefinedMenuItem::separator())
                    .map_err(|e| e.to_string())?;
                items.push(BuiltItem::Separator);
            }
        }
    }
    Ok((
        menu,
        BuiltMenu {
            shape: shape(entries),
            items,
        },
    ))
}

/// Same shape: refresh labels and check marks without replacing the menu,
/// so an update never destroys a menu that is open on screen. Also undoes
/// muda's own toggle of a clicked check item when the click changed
/// nothing (e.g. the radio option that was already selected).
fn update_in_place(built: &BuiltMenu, entries: &[MenuEntry]) {
    for (item, entry) in built.items.iter().zip(entries) {
        match (item, entry) {
            (BuiltItem::Item(item), MenuEntry::Item { label, enabled, .. }) => {
                item.set_text(menu_text(label));
                item.set_enabled(*enabled);
            }
            (BuiltItem::Check(item), MenuEntry::Check { checked, .. }) => {
                item.set_checked(*checked);
            }
            (BuiltItem::Radio(options), MenuEntry::Radio { selected, .. }) => {
                for (ix, option) in options.iter().enumerate() {
                    option.set_checked(*selected == Some(ix));
                }
            }
            _ => {}
        }
    }
}

fn icon(connected: bool) -> Result<Icon, String> {
    #[cfg(target_os = "windows")]
    let image = tray_icon(32, connected);
    // 22pt at 2x, the box itself ~16pt (see `padded_tray_icon`).
    #[cfg(target_os = "macos")]
    let image = padded_tray_icon(44, 32, connected);
    Icon::from_rgba(image.rgba, image.size, image.size).map_err(|e| e.to_string())
}

/// tray-icon's handlers are process-wide and settable once.
fn install_handlers(commands: UnboundedSender<TrayCommand>, commands_by_id: CommandMap) {
    static INSTALL: Once = Once::new();
    INSTALL.call_once(move || {
        // macOS: a click opens the menu instead (`with_menu_on_left_click`).
        #[cfg(target_os = "windows")]
        let clicks = commands.clone();
        #[cfg(target_os = "windows")]
        TrayIconEvent::set_event_handler(Some(move |event: TrayIconEvent| {
            let show = matches!(
                event,
                TrayIconEvent::Click {
                    button: MouseButton::Left,
                    button_state: MouseButtonState::Up,
                    ..
                } | TrayIconEvent::DoubleClick {
                    button: MouseButton::Left,
                    ..
                }
            );
            if show {
                let _ = clicks.unbounded_send(TrayCommand::ShowWindow);
            }
        }));
        MenuEvent::set_event_handler(Some(move |event: MenuEvent| {
            let command = commands_by_id
                .lock()
                .ok()
                .and_then(|map| map.get(&event.id.0).cloned());
            if let Some(command) = command {
                let _ = commands.unbounded_send(command);
            }
        }));
    });
}

impl Backend {
    /// Create the tray icon. UI thread only (tray-icon's window lives on the
    /// thread that pumps it; AppKit's status items on the main thread).
    pub fn new(
        snapshot: &TraySnapshot,
        commands: UnboundedSender<TrayCommand>,
    ) -> Result<Self, String> {
        let entries = menu_entries(snapshot);
        let commands_by_id: CommandMap = Arc::new(Mutex::new(command_map(&entries)));
        install_handlers(commands, commands_by_id.clone());

        let icons = [icon(false)?, icon(true)?];
        let connected = snapshot.connected();
        let tooltip = snapshot.tooltip();
        let (menu, built) = build_menu(&entries)?;
        let tray = TrayIconBuilder::new()
            .with_id("boxpilot")
            .with_tooltip(&tooltip)
            .with_icon(icons[usize::from(connected)].clone())
            .with_menu(Box::new(menu))
            .with_menu_on_left_click(cfg!(target_os = "macos"))
            .build()
            .map_err(|e| e.to_string())?;
        Ok(Self {
            tray,
            menu: built,
            commands_by_id,
            icons,
            connected,
            tooltip,
        })
    }

    pub fn update(&mut self, snapshot: &TraySnapshot) {
        let entries = menu_entries(snapshot);
        if let Ok(mut map) = self.commands_by_id.lock() {
            *map = command_map(&entries);
        }
        if shape(&entries) == self.menu.shape {
            update_in_place(&self.menu, &entries);
        } else {
            match build_menu(&entries) {
                Ok((menu, built)) => {
                    self.tray.set_menu(Some(Box::new(menu)));
                    self.menu = built;
                }
                Err(e) => eprintln!("Tray menu rebuild failed: {e}"),
            }
        }

        let tooltip = snapshot.tooltip();
        if tooltip != self.tooltip {
            let _ = self.tray.set_tooltip(Some(&tooltip));
            self.tooltip = tooltip;
        }
        let connected = snapshot.connected();
        if connected != self.connected {
            let _ = self
                .tray
                .set_icon(Some(self.icons[usize::from(connected)].clone()));
            self.connected = connected;
        }
    }
}
