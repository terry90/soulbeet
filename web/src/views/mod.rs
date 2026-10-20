mod dashboard;
mod login;
mod search;
mod settings;

pub use dashboard::DashboardPage;
pub use login::LoginPage;
pub use search::SearchPage;
pub use settings::SettingsPage;
pub(crate) use settings::{SettingsTab, SettingsTabRequest};
