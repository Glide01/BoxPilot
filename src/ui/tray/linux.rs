//! Linux tray: a StatusNotifierItem over D-Bus (`ksni`, no GTK). KDE,
//! GNOME with the AppIndicator extension, and most panels host it; without
//! a host, `spawn` fails with `WontShow` and BoxPilot behaves as it does
//! without a tray (closing the window quits).
//!
//! Every `ksni::Tray` callback runs on ksni's own D-Bus thread: it only
//! forwards a `TrayCommand` into the channel and never touches gpui.

use super::icon::tray_icon;
use super::model::{menu_entries, MenuEntry, TrayCommand, TraySnapshot};
use futures_channel::mpsc::UnboundedSender;
use ksni::blocking::{Handle, TrayMethods};
use ksni::menu::{CheckmarkItem, RadioGroup, RadioItem, StandardItem, SubMenu};
use ksni::{Icon, MenuItem, ToolTip};
use std::sync::{mpsc, Arc};

/// Pixmaps in two sizes; the host picks the closest one.
struct Pixmaps {
    connected: Vec<Icon>,
    disconnected: Vec<Icon>,
}

impl Pixmaps {
    fn new() -> Self {
        let make = |connected: bool| {
            [32, 64]
                .into_iter()
                .map(|size| {
                    let image = tray_icon(size, connected);
                    Icon {
                        width: size as i32,
                        height: size as i32,
                        data: image.to_argb32(),
                    }
                })
                .collect()
        };
        Self {
            connected: make(true),
            disconnected: make(false),
        }
    }
}

struct SniTray {
    snapshot: TraySnapshot,
    pixmaps: Arc<Pixmaps>,
    commands: UnboundedSender<TrayCommand>,
}

impl SniTray {
    fn send(&self, command: TrayCommand) {
        let _ = self.commands.unbounded_send(command);
    }
}

impl ksni::Tray for SniTray {
    fn id(&self) -> String {
        "boxpilot".into()
    }

    fn title(&self) -> String {
        "BoxPilot".into()
    }

    fn icon_pixmap(&self) -> Vec<Icon> {
        if self.snapshot.connected() {
            self.pixmaps.connected.clone()
        } else {
            self.pixmaps.disconnected.clone()
        }
    }

    fn tool_tip(&self) -> ToolTip {
        ToolTip {
            title: self.snapshot.tooltip(),
            ..Default::default()
        }
    }

    /// Left click: open the window.
    fn activate(&mut self, _x: i32, _y: i32) {
        self.send(TrayCommand::ShowWindow);
    }

    fn menu(&self) -> Vec<MenuItem<Self>> {
        menu_entries(&self.snapshot)
            .into_iter()
            .map(sni_item)
            .collect()
    }

    fn watcher_offline(&self, _reason: ksni::OfflineReason) -> bool {
        self.send(TrayCommand::HostLost);
        // Keep the service: if the host comes back (a shell restart), the
        // icon re-registers and `watcher_online` reports it.
        true
    }

    fn watcher_online(&self) {
        self.send(TrayCommand::HostRestored);
    }
}

fn sni_item(entry: MenuEntry) -> MenuItem<SniTray> {
    match entry {
        MenuEntry::Item {
            label,
            enabled,
            command,
        } => StandardItem {
            label,
            enabled,
            activate: Box::new(move |tray: &mut SniTray| tray.send(command.clone())),
            ..Default::default()
        }
        .into(),
        MenuEntry::Check {
            label,
            checked,
            command,
        } => CheckmarkItem {
            label,
            checked,
            activate: Box::new(move |tray: &mut SniTray| tray.send(command.clone())),
            ..Default::default()
        }
        .into(),
        MenuEntry::Radio {
            label,
            options,
            selected,
        } => {
            let items = options
                .iter()
                .map(|(label, _)| RadioItem {
                    label: label.clone(),
                    ..Default::default()
                })
                .collect();
            let commands: Vec<TrayCommand> = options.into_iter().map(|(_, c)| c).collect();
            SubMenu {
                label,
                submenu: vec![RadioGroup {
                    // Out of range = nothing checked.
                    selected: selected.unwrap_or(usize::MAX),
                    select: Box::new(move |tray: &mut SniTray, ix: usize| {
                        if let Some(command) = commands.get(ix) {
                            tray.send(command.clone());
                        }
                    }),
                    options: items,
                }
                .into()],
                ..Default::default()
            }
            .into()
        }
        MenuEntry::Separator => MenuItem::Separator,
    }
}

/// A registered StatusNotifierItem.
pub struct Backend {
    handle: Handle<SniTray>,
    /// Feeds the updater thread; dropping it ends that thread.
    updates: mpsc::Sender<TraySnapshot>,
}

impl Backend {
    /// Register the tray. Blocks on D-Bus (connect + register with the
    /// watcher), so call it off the UI thread. `Err` = no tray on this
    /// desktop (no session bus, no watcher, or no host: `WontShow`).
    pub fn spawn(
        snapshot: TraySnapshot,
        commands: UnboundedSender<TrayCommand>,
    ) -> Result<Self, String> {
        let tray = SniTray {
            snapshot,
            pixmaps: Arc::new(Pixmaps::new()),
            commands,
        };
        let handle = tray.spawn().map_err(|e| e.to_string())?;

        // `Handle::update` blocks until ksni's thread has applied the change
        // and signalled it over D-Bus, so updates go through a thread of
        // their own, in order, collapsed to the newest.
        let (updates, pending) = mpsc::channel::<TraySnapshot>();
        let updater = handle.clone();
        std::thread::Builder::new()
            .name("tray-update".into())
            .spawn(move || {
                while let Ok(mut snapshot) = pending.recv() {
                    while let Ok(newer) = pending.try_recv() {
                        snapshot = newer;
                    }
                    updater.update(move |tray| tray.snapshot = snapshot);
                }
            })
            .map_err(|e| e.to_string())?;
        Ok(Self { handle, updates })
    }

    /// Show `snapshot`. Doesn't block.
    pub fn update(&mut self, snapshot: &TraySnapshot) {
        let _ = self.updates.send(snapshot.clone());
    }

    /// Take the icon off the panel (app quit). Doesn't wait for it.
    pub fn shutdown(&self) {
        let _ = self.handle.shutdown();
    }
}
