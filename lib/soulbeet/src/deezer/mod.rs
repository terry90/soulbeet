pub mod client;
pub mod models;

pub use client::DeezerClient;
pub use models::{
    DeezerAlbumDetail, DeezerAlbumTrack, DeezerArtistAlbum, DeezerArtistDetail, DeezerPlaylist,
    DeezerPlaylistTrack, DeezerSearchAlbum, DeezerSearchArtist, DeezerSearchTrack, DeezerTrack,
    DeezerTrackAlbum, DeezerTrackArtist,
};
