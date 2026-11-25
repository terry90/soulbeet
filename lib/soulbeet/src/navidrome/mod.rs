pub mod browse;
pub mod client;
pub mod error;
pub mod models;
pub mod smart_playlist;

pub use client::{
    filepath_rules, NavidromeClient, NavidromeClientBuilder, PlaylistMeta, SmartPlaylistChange,
    SongPage, StarTarget, SONG_PAGE, SUBSONIC_API_VERSION, SUBSONIC_CLIENT_NAME,
};
pub use error::{configure_navidrome, relink_navidrome, NavidromeError};
pub use models::{
    LegacyLyrics, LibraryKind, LyricLine, NativePlaylist, ScanStatus, StructuredLyrics,
};
