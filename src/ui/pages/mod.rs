//! Page views for the sidebar-navigation layout. One entity per page;
//! `RootView` keeps all of them alive and renders the active one.

pub mod connections;
pub mod groups;
pub mod home;
pub mod logs;
pub mod profiles;
pub mod settings;
pub mod tools;
pub mod tailscale;

pub use connections::ConnectionsPage;
pub use groups::GroupsPage;
pub use home::HomePage;
pub use logs::LogsPage;
pub use profiles::ProfilesPage;
pub use settings::SettingsPage;
pub use tools::ToolsPage;
pub use tailscale::TailscalePage;

/// Which page the sidebar has selected. Plain field on `RootView` —
/// switching pages is just `active_page = …; cx.notify()`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ActivePage {
    Home,
    Groups,
    Connections,
    /// Only while the running config has Tailscale endpoints.
    Tailscale,
    Profiles,
    Logs,
    Tools,
    Settings,
}
