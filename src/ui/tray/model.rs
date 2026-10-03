//! What the tray shows, independent of the platform backend: a
//! [`TraySnapshot`] of the app state, the menu it turns into
//! ([`menu_entries`]), and the [`TrayCommand`]s the menu sends back. Pure —
//! no gpui, no platform types — so it is unit-tested here and both backends
//! render the same menu.

use crate::core::presentation::ConnectionStatus;
use crate::i18n::Language;

/// Everything a tray click can ask for. Backends never act on a click
/// themselves: their callbacks run off the gpui thread (ksni's D-Bus
/// thread) or inside a window procedure (tray-icon), so they only forward a
/// command into the channel `tray::init` drains on the UI thread.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TrayCommand {
    ShowWindow,
    ToggleConnection,
    /// `true` = Proxy mode, `false` = TUN (as `AppSettings::proxy_mode`).
    SetProxyMode(bool),
    SetSystemProxy(bool),
    SetClashMode(String),
    SetActiveProfile(String),
    Quit,
    /// The tray host went away (Linux: the StatusNotifierWatcher left the
    /// bus). The window must not stay closed with no way back.
    HostLost,
    /// The tray host is back after a [`TrayCommand::HostLost`].
    HostRestored,
}

impl TrayCommand {
    /// A stable string id for a menu item that sends this command
    /// (tray-icon's `MenuId`): distinct commands get distinct ids.
    #[cfg_attr(not(target_os = "windows"), allow(dead_code))]
    pub fn menu_id(&self) -> String {
        match self {
            TrayCommand::ShowWindow => "show".into(),
            TrayCommand::ToggleConnection => "toggle".into(),
            TrayCommand::SetProxyMode(proxy) => format!("proxy-mode:{}", u8::from(*proxy)),
            TrayCommand::SetSystemProxy(on) => format!("system-proxy:{}", u8::from(*on)),
            TrayCommand::SetClashMode(mode) => format!("clash-mode:{mode}"),
            TrayCommand::SetActiveProfile(id) => format!("profile:{id}"),
            TrayCommand::Quit => "quit".into(),
            TrayCommand::HostLost => "host-lost".into(),
            TrayCommand::HostRestored => "host-restored".into(),
        }
    }
}

/// The slice of app state the tray shows. Rebuilt on every observed change;
/// backends are only touched when it actually differs from the last one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TraySnapshot {
    pub status: ConnectionStatus,
    /// `AppSettings::proxy_mode`: `true` = Proxy, `false` = TUN.
    pub proxy_mode: bool,
    pub system_proxy: bool,
    /// Selectable Clash modes; empty unless switchable right now (running,
    /// two or more modes).
    pub clash_modes: Vec<String>,
    pub clash_current: String,
    /// `(id, name)` of every profile, in the user's order.
    pub profiles: Vec<(String, String)>,
    /// The UI language: a switch is a change, so the menu is rebuilt in it.
    pub active_profile: String,
    pub language: Language,
}

impl TraySnapshot {
    /// Hover text: "BoxPilot — Connected" etc.
    pub fn tooltip(&self) -> String {
        (self.language.strings().tray.tooltip)(self.status.label_in(self.language.strings()))
    }

    /// Whether the icon is the coloured (connected) one.
    pub fn connected(&self) -> bool {
        self.status == ConnectionStatus::Connected
    }
}

/// One platform-neutral menu entry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MenuEntry {
    Item {
        label: String,
        enabled: bool,
        command: TrayCommand,
    },
    /// A check item; `command` is what clicking it asks for (the toggled
    /// value).
    Check {
        label: String,
        checked: bool,
        command: TrayCommand,
    },
    /// A submenu of mutually exclusive options. `selected` indexes
    /// `options`; `None` = nothing selected.
    Radio {
        label: String,
        options: Vec<(String, TrayCommand)>,
        selected: Option<usize>,
    },
    Separator,
}

/// The tray menu for `snapshot`, top to bottom.
pub fn menu_entries(snapshot: &TraySnapshot) -> Vec<MenuEntry> {
    let t = snapshot.language.strings();
    let mut entries = vec![
        MenuEntry::Item {
            label: t.tray.show.into(),
            enabled: true,
            command: TrayCommand::ShowWindow,
        },
        MenuEntry::Separator,
        MenuEntry::Item {
            label: snapshot.status.power_action_label_in(t).into(),
            enabled: snapshot.status.can_toggle(),
            command: TrayCommand::ToggleConnection,
        },
        MenuEntry::Check {
            label: t.tray.system_proxy.into(),
            checked: snapshot.system_proxy,
            command: TrayCommand::SetSystemProxy(!snapshot.system_proxy),
        },
        MenuEntry::Radio {
            label: t.tray.proxy_mode.into(),
            options: vec![
                (t.home.mode_tun.into(), TrayCommand::SetProxyMode(false)),
                (t.home.mode_proxy.into(), TrayCommand::SetProxyMode(true)),
            ],
            selected: Some(usize::from(snapshot.proxy_mode)),
        },
    ];
    if crate::core::singbox_api::is_switchable(&snapshot.clash_modes) {
        entries.push(MenuEntry::Radio {
            label: t.tray.clash_mode.into(),
            options: snapshot
                .clash_modes
                .iter()
                .map(|mode| (mode.clone(), TrayCommand::SetClashMode(mode.clone())))
                .collect(),
            selected: snapshot
                .clash_modes
                .iter()
                .position(|mode| *mode == snapshot.clash_current),
        });
    }
    if snapshot.profiles.len() >= 2 {
        entries.push(MenuEntry::Radio {
            label: t.tray.profile.into(),
            options: snapshot
                .profiles
                .iter()
                .map(|(id, name)| (name.clone(), TrayCommand::SetActiveProfile(id.clone())))
                .collect(),
            selected: snapshot
                .profiles
                .iter()
                .position(|(id, _)| *id == snapshot.active_profile),
        });
    }
    entries.push(MenuEntry::Separator);
    entries.push(MenuEntry::Item {
        label: t.tray.quit.into(),
        enabled: true,
        command: TrayCommand::Quit,
    });
    entries
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot() -> TraySnapshot {
        TraySnapshot {
            status: ConnectionStatus::Disconnected,
            proxy_mode: false,
            system_proxy: false,
            clash_modes: Vec::new(),
            clash_current: String::new(),
            profiles: vec![("p1".into(), "Home".into())],
            active_profile: "p1".into(),
            language: Language::English,
        }
    }

    fn labels(entries: &[MenuEntry]) -> Vec<&str> {
        entries
            .iter()
            .map(|e| match e {
                MenuEntry::Item { label, .. }
                | MenuEntry::Check { label, .. }
                | MenuEntry::Radio { label, .. } => label.as_str(),
                MenuEntry::Separator => "-",
            })
            .collect()
    }

    #[test]
    fn minimal_menu_has_the_fixed_entries_only() {
        let entries = menu_entries(&snapshot());
        assert_eq!(
            labels(&entries),
            [
                "Show BoxPilot",
                "-",
                "Connect",
                "System Proxy",
                "Proxy Mode",
                "-",
                "Quit BoxPilot"
            ]
        );
    }

    #[test]
    fn connection_item_follows_status() {
        let mut snap = snapshot();
        let toggle = |snap: &TraySnapshot| {
            menu_entries(snap)
                .into_iter()
                .find(|e| {
                    matches!(
                        e,
                        MenuEntry::Item {
                            command: TrayCommand::ToggleConnection,
                            ..
                        }
                    )
                })
                .unwrap()
        };
        assert!(
            matches!(toggle(&snap), MenuEntry::Item { enabled: true, ref label, .. } if label == "Connect")
        );
        snap.status = ConnectionStatus::Starting;
        assert!(matches!(
            toggle(&snap),
            MenuEntry::Item { enabled: false, .. }
        ));
        snap.status = ConnectionStatus::Connected;
        assert!(
            matches!(toggle(&snap), MenuEntry::Item { enabled: true, ref label, .. } if label == "Disconnect")
        );
    }

    #[test]
    fn system_proxy_check_asks_for_the_toggled_value() {
        let mut snap = snapshot();
        snap.system_proxy = true;
        let check = menu_entries(&snap)
            .into_iter()
            .find(|e| matches!(e, MenuEntry::Check { .. }))
            .unwrap();
        assert_eq!(
            check,
            MenuEntry::Check {
                label: "System Proxy".into(),
                checked: true,
                command: TrayCommand::SetSystemProxy(false),
            }
        );
    }

    #[test]
    fn proxy_mode_radio_selects_current_mode() {
        let mut snap = snapshot();
        snap.proxy_mode = true;
        let radio = menu_entries(&snap)
            .into_iter()
            .find(|e| matches!(e, MenuEntry::Radio { label, .. } if label == "Proxy Mode"))
            .unwrap();
        let MenuEntry::Radio {
            options, selected, ..
        } = radio
        else {
            unreachable!()
        };
        assert_eq!(selected, Some(1));
        assert_eq!(options[0].1, TrayCommand::SetProxyMode(false));
        assert_eq!(options[1].1, TrayCommand::SetProxyMode(true));
    }

    #[test]
    fn clash_mode_submenu_only_while_switchable() {
        let mut snap = snapshot();
        snap.clash_modes = vec!["Rule".into()];
        snap.clash_current = "Rule".into();
        assert!(!labels(&menu_entries(&snap)).contains(&"Clash Mode"));

        snap.clash_modes = vec!["Rule".into(), "Global".into(), "Direct".into()];
        snap.clash_current = "Global".into();
        let entries = menu_entries(&snap);
        let radio = entries
            .iter()
            .find(|e| matches!(e, MenuEntry::Radio { label, .. } if label == "Clash Mode"))
            .unwrap();
        let MenuEntry::Radio {
            options, selected, ..
        } = radio
        else {
            unreachable!()
        };
        assert_eq!(*selected, Some(1));
        assert_eq!(
            options[2],
            ("Direct".into(), TrayCommand::SetClashMode("Direct".into()))
        );
    }

    #[test]
    fn profile_submenu_only_with_two_or_more_profiles() {
        let mut snap = snapshot();
        assert!(!labels(&menu_entries(&snap)).contains(&"Profile"));

        snap.profiles.push(("p2".into(), "Work".into()));
        snap.active_profile = "p2".into();
        let entries = menu_entries(&snap);
        assert_eq!(
            labels(&entries),
            [
                "Show BoxPilot",
                "-",
                "Connect",
                "System Proxy",
                "Proxy Mode",
                "Profile",
                "-",
                "Quit BoxPilot"
            ]
        );
        let MenuEntry::Radio {
            options, selected, ..
        } = &entries[5]
        else {
            unreachable!()
        };
        assert_eq!(*selected, Some(1));
        assert_eq!(
            options[0],
            ("Home".into(), TrayCommand::SetActiveProfile("p1".into()))
        );
    }

    #[test]
    fn tooltip_and_icon_follow_status() {
        let mut snap = snapshot();
        assert_eq!(snap.tooltip(), "BoxPilot — Disconnected");
        assert!(!snap.connected());
        snap.status = ConnectionStatus::Starting;
        assert_eq!(snap.tooltip(), "BoxPilot — Starting…");
        assert!(!snap.connected());
        snap.status = ConnectionStatus::Connected;
        assert_eq!(snap.tooltip(), "BoxPilot — Connected");
        assert!(snap.connected());
    }

    #[test]
    fn menu_and_tooltip_follow_the_language() {
        let mut snap = snapshot();
        snap.language = Language::SimplifiedChinese;
        snap.status = ConnectionStatus::Connected;
        assert_eq!(snap.tooltip(), "BoxPilot — 已连接");
        assert_eq!(
            labels(&menu_entries(&snap)),
            [
                "显示 BoxPilot",
                "-",
                "断开",
                "系统代理",
                "代理模式",
                "-",
                "退出 BoxPilot"
            ]
        );
        let english = snapshot();
        assert_ne!(english, snap, "a language switch is a snapshot change");
    }

    #[test]
    fn menu_ids_are_distinct_per_command() {
        let commands = [
            TrayCommand::ShowWindow,
            TrayCommand::ToggleConnection,
            TrayCommand::SetProxyMode(true),
            TrayCommand::SetProxyMode(false),
            TrayCommand::SetSystemProxy(true),
            TrayCommand::SetSystemProxy(false),
            TrayCommand::SetClashMode("Rule".into()),
            TrayCommand::SetClashMode("Global".into()),
            TrayCommand::SetActiveProfile("p1".into()),
            TrayCommand::SetActiveProfile("p12".into()),
            TrayCommand::Quit,
            TrayCommand::HostLost,
            TrayCommand::HostRestored,
        ];
        let ids: std::collections::HashSet<String> =
            commands.iter().map(TrayCommand::menu_id).collect();
        assert_eq!(ids.len(), commands.len());
    }
}
