//! Page views for the sidebar-navigation layout. One entity per page;
//! `RootView` keeps all of them alive and renders the active one.

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
    /// The page's name, as the sidebar, the breadcrumb and the page's title
    /// show it.
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
}
