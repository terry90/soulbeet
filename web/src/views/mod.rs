mod activity;
mod dashboard;
mod detail_views;
mod discover;
mod home;
mod library;
mod login;
mod mix_detail;
mod search;
mod settings;
mod studio_shell;

pub use activity::ActivityPage;
pub use dashboard::DashboardPage;
pub use detail_views::{AlbumDetailPage, ArtistDetailPage, PlaylistDetailPage};
pub use discover::DiscoverPage;
pub use home::HomePage;
pub use library::LibraryPage;
pub(crate) use login::leave_login;
pub use login::{LoginErrorPage, LoginErrorReturnPage, LoginPage, LoginReturnPage, ReturnTo};
pub use mix_detail::MixDetailPage;
pub use search::SearchPage;
pub use settings::SettingsPage;
pub(crate) use settings::{SettingsTab, SettingsTabRequest, SsoLinkNotice, SsoLinkResultPage};
#[cfg(test)]
pub(crate) use studio_shell::navidrome_banner;
pub use studio_shell::StudioShell;
