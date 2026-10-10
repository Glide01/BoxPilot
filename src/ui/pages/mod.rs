//! Page views for the sidebar-navigation layout. One entity per page;
//! `RootView` keeps all of them alive and renders the active one.

use crate::state::app_state::AppState;
use gpui::{Context, Entity};
use std::cell::Cell;
use std::rc::Rc;

mod connection_details;
pub mod connections;
pub mod groups;
pub mod home;
pub mod logs;
pub mod profiles;
pub mod settings;
pub mod tailscale;
pub mod tools;
pub mod vpn;

pub use connections::ConnectionsPage;
pub use groups::GroupsPage;
pub use home::HomePage;
pub use logs::LogsPage;
pub use profiles::ProfilesPage;
pub use settings::SettingsPage;
pub use tailscale::TailscalePage;
pub use tools::ToolsPage;
pub use vpn::VpnPage;

/// Which page the sidebar has selected. Plain field on `RootView` —
/// switching pages is just `active_page = …; cx.notify()`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ActivePage {
    Home,
    Groups,
    Connections,
    /// Only while the running config has Tailscale endpoints.
    Tailscale,
    /// OpenConnect / OpenVPN / USB/IP status; only offered while the running
    /// config has any (`VpnStatus::is_visible`).
    Vpn,
    Profiles,
    Logs,
    Tools,
    Settings,
}

impl ActivePage {
    /// The page's name, as the sidebar and the page's title show it.
    pub fn label(self) -> &'static str {
        let nav = &crate::i18n::s().nav;
        match self {
            ActivePage::Home => nav.home,
            ActivePage::Groups => nav.groups,
            ActivePage::Connections => nav.connections,
            ActivePage::Tailscale => nav.tailscale,
            ActivePage::Vpn => nav.vpn,
            ActivePage::Profiles => nav.profiles,
            ActivePage::Logs => nav.logs,
            ActivePage::Tools => nav.tools,
            ActivePage::Settings => nav.settings,
        }
    }

    /// What the page is for: the line under its title while it has
    /// nothing live to put there.
    pub fn hint(self) -> &'static str {
        let nav = &crate::i18n::s().nav;
        match self {
            ActivePage::Home => nav.home_hint,
            ActivePage::Groups => nav.groups_hint,
            ActivePage::Connections => nav.connections_hint,
            ActivePage::Tailscale => nav.tailscale_hint,
            ActivePage::Vpn => nav.vpn_hint,
            ActivePage::Profiles => nav.profiles_hint,
            ActivePage::Logs => nav.logs_hint,
            ActivePage::Tools => nav.tools_hint,
            ActivePage::Settings => nav.settings_hint,
        }
    }
}

/// Re-render the page `cx` belongs to whenever the connection status
/// (Disconnected / Starting / Connected) changes — for pages whose empty
/// state follows it (`widgets::run_empty_state`). Only the edges notify:
/// `AppState` notifies often.
pub(crate) fn rerender_on_status<T: 'static>(app_state: &Entity<AppState>, cx: &mut Context<T>) {
    let last = Rc::new(Cell::new(app_state.read(cx).connection_status(cx)));
    let process = app_state.read(cx).process.clone();
    let check = {
        let app_state = app_state.clone();
        move |cx: &mut Context<T>| {
            let now = app_state.read(cx).connection_status(cx);
            if last.replace(now) != now {
                cx.notify();
            }
        }
    };
    let on_process = check.clone();
    cx.observe(app_state, move |_, _, cx| check(cx)).detach();
    cx.observe(&process, move |_, _, cx| on_process(cx)).detach();
}
