use serde::Deserialize;

/// Generic Subsonic API response wrapper.
/// The API nests all responses under `subsonic-response`.
#[derive(Debug, Deserialize)]
pub struct SubsonicEnvelope<T> {
    #[serde(rename = "subsonic-response")]
    pub response: SubsonicResponse<T>,
}

#[derive(Debug, Deserialize)]
pub struct SubsonicResponse<T> {
    pub status: String,
    pub error: Option<SubsonicError>,
    #[serde(flatten)]
    pub body: T,
}

#[derive(Debug, Deserialize)]
pub struct SubsonicError {
    pub code: u32,
    pub message: String,
}

// Body types for different endpoints

#[derive(Debug, Deserialize)]
pub struct PingBody {}

#[derive(Debug, Deserialize)]
pub struct AlbumList2Body {
    #[serde(rename = "albumList2")]
    pub album_list: Option<AlbumList2>,
}

#[derive(Debug, Deserialize)]
pub struct AlbumList2 {
    #[serde(default)]
    pub album: Vec<SubsonicAlbum>,
}

#[derive(Debug, Deserialize)]
pub struct AlbumBody {
    pub album: Option<SubsonicAlbumDetail>,
}

#[derive(Debug, Deserialize)]
pub struct PlaylistsBody {
    pub playlists: Option<PlaylistsWrapper>,
}

#[derive(Debug, Deserialize)]
pub struct PlaylistsWrapper {
    #[serde(default)]
    pub playlist: Vec<SubsonicPlaylist>,
}

#[derive(Debug, Deserialize)]
pub struct PlaylistBody {
    pub playlist: Option<SubsonicPlaylistDetail>,
}

#[derive(Debug, Deserialize)]
pub struct StarredBody {
    #[serde(rename = "starred2")]
    pub starred: Option<StarredContent>,
}

#[derive(Debug, Deserialize)]
pub struct StarredContent {
    #[serde(default)]
    pub song: Vec<SubsonicSong>,
    #[serde(default)]
    pub album: Vec<SubsonicAlbum>,
}

#[derive(Debug, Deserialize)]
pub struct SearchResult3Body {
    #[serde(rename = "searchResult3")]
    pub search_result: Option<SearchResult3>,
}

#[derive(Debug, Deserialize)]
pub struct SearchResult3 {
    #[serde(default)]
    pub song: Vec<SubsonicSong>,
    #[serde(default)]
    pub album: Vec<SubsonicAlbum>,
}

// Core data types

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SubsonicSong {
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub artist: Option<String>,
    #[serde(default)]
    pub album: Option<String>,
    #[serde(default)]
    pub album_id: Option<String>,
    #[serde(default)]
    pub genre: Option<String>,
    #[serde(default)]
    pub duration: Option<u32>,
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub user_rating: Option<u8>,
    #[serde(default)]
    pub average_rating: Option<f64>,
    #[serde(default)]
    pub play_count: Option<u32>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SubsonicAlbum {
    pub id: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub artist: Option<String>,
    #[serde(default)]
    pub song_count: Option<u32>,
    #[serde(default)]
    pub duration: Option<u32>,
    #[serde(default)]
    pub genre: Option<String>,
    #[serde(default)]
    pub user_rating: Option<u8>,
    #[serde(default)]
    pub average_rating: Option<f64>,
}

impl SubsonicAlbum {
    pub fn display_name(&self) -> &str {
        self.name
            .as_deref()
            .or(self.title.as_deref())
            .unwrap_or("Unknown Album")
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SubsonicAlbumDetail {
    pub id: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub artist: Option<String>,
    #[serde(default)]
    pub song_count: Option<u32>,
    #[serde(default)]
    pub song: Vec<SubsonicSong>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SubsonicPlaylist {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub song_count: Option<u32>,
    #[serde(default)]
    pub duration: Option<u32>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SubsonicPlaylistDetail {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub song_count: Option<u32>,
    #[serde(default)]
    pub entry: Vec<SubsonicSong>,
}

/// A player registered with Navidrome (native API: GET /api/player).
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlayerInfo {
    pub id: String,
    pub name: String,
    pub client: String,
    pub report_real_path: bool,
}
