pub mod client;
pub mod error;
pub mod models;

pub use client::{
    NavidromeClient, NavidromeClientBuilder, PlaylistMeta, SongPage, StarTarget, SONG_PAGE,
};
pub use error::{configure_navidrome, relink_navidrome, NavidromeError};
pub use models::{LibraryKind, NativePlaylist, ScanStatus};
