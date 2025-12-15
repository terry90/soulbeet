use dioxus::prelude::*;

pub mod auth;
pub mod download;
pub mod folder;
pub mod guard;
pub mod search;
pub mod system;
pub mod user;

pub use auth::*;
pub use download::*;
pub use folder::*;
pub use guard::*;
pub use search::*;
pub use system::*;
pub use user::*;

pub fn server_error<E: std::fmt::Display>(e: E) -> ServerFnError {
    ServerFnError::ServerError {
        message: e.to_string(),
        code: 500,
        details: None,
    }
}
