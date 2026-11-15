pub mod auth;
#[cfg(feature = "server")]
pub mod base_url;
pub mod build_id;
pub mod config;
#[cfg(feature = "server")]
pub mod connection;
pub mod connections;
#[cfg(feature = "server")]
pub mod core;
pub mod crypto;
pub mod db;
pub mod folder_policy;
pub mod globals;
#[cfg(feature = "server")]
pub mod import_queue;
#[cfg(feature = "server")]
pub mod importer_health;
#[cfg(feature = "server")]
pub mod ledger;
#[cfg(feature = "server")]
pub mod login_limiter;
#[cfg(feature = "server")]
pub mod mcp;
pub mod models;
#[cfg(feature = "server")]
pub mod oauth;
#[cfg(feature = "server")]
pub mod secret_key;
pub mod security;
pub mod services;
#[cfg(feature = "server")]
pub mod transfer_registry;

pub mod server_fns;
#[cfg(all(test, feature = "server"))]
mod test_navidrome;

pub use server_fns::*;
