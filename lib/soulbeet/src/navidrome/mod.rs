pub mod client;
pub mod error;
pub mod models;

pub use client::{NavidromeClient, NavidromeClientBuilder};
pub use error::{configure_navidrome, relink_navidrome, NavidromeError};
pub use models::ScanStatus;
