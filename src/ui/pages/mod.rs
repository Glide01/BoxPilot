//! Page views for the sidebar-navigation layout. One entity per page;
//! `RootView` keeps all five alive and renders the active one.

pub mod groups;
pub mod home;
pub mod logs;
pub mod profiles;
pub mod settings;

pub use groups::GroupsPage;
pub use home::HomePage;
pub use logs::LogsPage;
pub use profiles::ProfilesPage;
pub use settings::SettingsPage;

/// Which page the sidebar has selected. Plain field on `RootView` —
/// switching pages is just `active_page = …; cx.notify()`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ActivePage {
    Home,
    Groups,
    Profiles,
    Logs,
    Settings,
}
