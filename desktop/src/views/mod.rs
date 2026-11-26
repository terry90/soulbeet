pub mod discover;
pub mod home;
pub mod library;
pub mod search;
pub mod settings;
pub mod setup;

pub use discover::DiscoverView;
pub use home::HomeView;
pub use library::LibraryView;
pub use search::SearchView;
pub use settings::{ActivityView, SettingsView};
pub use setup::SetupView;
