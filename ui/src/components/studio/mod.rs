//! Reusable Refined Midnight Studio 3-pane listening & curation UI components.
//!
//! Designed to be shared identically between `web` (`dioxus-web`) and `desktop` (`dioxus-desktop`).

pub mod command_palette;
pub mod i18n;
pub mod lyrics;
pub mod player_bar;
pub mod right_drawer;
pub mod shell;
pub mod undo_queue;

pub use command_palette::{
    filter_palette_commands, should_ignore_shortcut_target, CommandPalette, PaletteCategory,
    PaletteCommand,
};
pub use i18n::{
    discovery_days_remaining, format_candidate_reason, format_mix_subtitle, format_mix_title,
    format_mix_window_badge, percent_encode_param, subsonic_cover_art_url, subsonic_rest_url,
    subsonic_stream_url,
};
pub use lyrics::{
    find_active_lyric_line, normalize_track_lyrics, parse_lrc_lines, LyricsPanel,
    NormalizedLyricLine,
};
pub use player_bar::StudioPlayerBar;
pub use right_drawer::{DrawerTab, StudioRightDrawer};
pub use shell::{
    classify_local_playlist_category, classify_playlist_category, PlaylistCategory,
    SidebarPlaylistItem, StudioMobileNav, StudioNavSection, StudioSidebar, StudioTopBar,
};
pub use undo_queue::{PendingDropItem, PendingDropQueue};
