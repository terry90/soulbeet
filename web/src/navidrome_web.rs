//! Lightweight OpenSubsonic browser client powered by [`NavidromeSessionInfo`]
//! from `GET /api/client/bootstrap`.
//!
//! Used directly by the Web Studio UI to browse playlists, albums, starred songs,
//! local search hits, and submit OpenSubsonic `scrobble` events without proxying
//! heavy media or browse payloads through Soulbeet's server.

#[cfg(feature = "web")]
use serde::Deserialize;
use shared::client::NavidromeSessionInfo;
#[cfg(feature = "web")]
use ui::web_cover_art_url;
use ui::PlayerTrack;

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SubsonicPlaylistSummary {
    pub id: String,
    pub name: String,
    pub song_count: u32,
    pub duration: u32,
    pub cover_art: Option<String>,
    pub comment: String,
    pub owner: String,
    pub public: bool,
    pub is_smart: bool,
    pub category: ui::PlaylistCategory,
}

impl SubsonicPlaylistSummary {
    /// Returns the authenticated Web cover-art proxy URL for this playlist's collage artwork.
    /// Falls back to `pl-{id}` when Navidrome omits `coverArt` on non-empty playlists.
    pub fn cover_art_url(&self, size: u32) -> Option<String> {
        if let Some(cid) = self
            .cover_art
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            return Some(ui::web_cover_art_url(cid, size));
        }
        let id = self.id.trim();
        if !id.is_empty() && self.song_count > 0 {
            return Some(ui::web_cover_art_url(&format!("pl-{id}"), size));
        }
        None
    }
}

/// Detects whether a Navidrome playlist is smart or Soulbeet-managed from its comment.
/// Only matches explicit Soulbeet management prefixes or explicit smart-playlist markers
/// so normal personal playlists (e.g. "My Discovery Gems" or "Smart Indie") remain editable.
#[cfg(any(feature = "web", test))]
pub fn detect_is_smart_playlist(_name: &str, comment: &str) -> bool {
    let comment_lower = comment.trim().to_ascii_lowercase();
    comment_lower.starts_with("soulbeet discovery")
        || comment_lower.contains("soulbeet discovery")
        || comment_lower.starts_with("soulbeet:contextual:")
        || comment_lower.contains("soulbeet:contextual:")
        || comment_lower.contains("smart playlist")
        || comment_lower.starts_with("smart:")
        || comment_lower.contains("[smart]")
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SubsonicAlbumSummary {
    pub id: String,
    pub name: String,
    pub artist: String,
    pub song_count: u32,
    pub year: Option<u32>,
    pub cover_art_url: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SubsonicArtistSummary {
    pub id: String,
    pub name: String,
    pub album_count: u32,
    pub cover_art_url: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct NavidromeSearchHits {
    pub songs: Vec<PlayerTrack>,
    pub albums: Vec<SubsonicAlbumSummary>,
    pub artists: Vec<SubsonicArtistSummary>,
}

#[cfg(feature = "web")]
mod wire {
    use super::*;

    #[derive(Debug, Deserialize)]
    pub(super) struct SubsonicEnvelope<T> {
        #[serde(rename = "subsonic-response")]
        pub(super) response: SubsonicBody<T>,
    }

    #[derive(Debug, Deserialize)]
    pub(super) struct SubsonicErrorBody {
        pub(super) code: i32,
        #[serde(default)]
        pub(super) message: Option<String>,
    }

    #[derive(Debug, Deserialize)]
    pub(super) struct SubsonicBody<T> {
        pub(super) status: String,
        #[serde(default)]
        pub(super) error: Option<SubsonicErrorBody>,
        #[serde(flatten)]
        pub(super) data: T,
    }

    #[derive(Debug, Deserialize, Default)]
    pub(super) struct SongDetailData {
        #[serde(default)]
        pub(super) song: Option<RawSong>,
    }

    #[derive(Debug, Deserialize, Default)]
    pub(super) struct PlaylistsData {
        #[serde(default)]
        pub(super) playlists: Option<PlaylistsContainer>,
    }

    #[derive(Debug, Deserialize, Default)]
    pub(super) struct PlaylistsContainer {
        #[serde(default)]
        pub(super) playlist: Vec<RawPlaylist>,
    }

    #[derive(Debug, Deserialize)]
    pub(super) struct RawPlaylist {
        pub(super) id: String,
        pub(super) name: String,
        #[serde(rename = "songCount", default)]
        pub(super) song_count: u32,
        #[serde(default)]
        pub(super) duration: u32,
        #[serde(rename = "coverArt", default)]
        pub(super) cover_art: Option<String>,
        #[serde(default)]
        pub(super) comment: Option<String>,
        #[serde(default)]
        pub(super) owner: Option<String>,
        #[serde(default)]
        pub(super) public: bool,
    }

    #[derive(Debug, Deserialize, Default)]
    pub(super) struct PlaylistDetailData {
        #[serde(default)]
        pub(super) playlist: Option<RawPlaylistDetail>,
    }

    #[derive(Debug, Deserialize, Default)]
    pub(super) struct RawPlaylistDetail {
        #[serde(default)]
        pub(super) id: String,
        #[serde(default)]
        pub(super) name: String,
        #[serde(default)]
        pub(super) duration: u32,
        #[serde(rename = "coverArt", default)]
        pub(super) cover_art: Option<String>,
        #[serde(default)]
        pub(super) comment: Option<String>,
        #[serde(default)]
        pub(super) owner: Option<String>,
        #[serde(default)]
        pub(super) public: bool,
        #[serde(default)]
        pub(super) entry: Vec<RawSong>,
    }

    #[derive(Debug, Deserialize, Default)]
    pub(super) struct AlbumList2Data {
        #[serde(rename = "albumList2", default)]
        pub(super) album_list2: Option<AlbumListContainer>,
    }

    #[derive(Debug, Deserialize, Default)]
    pub(super) struct AlbumListContainer {
        #[serde(default)]
        pub(super) album: Vec<RawAlbum>,
    }

    #[derive(Debug, Deserialize)]
    pub(super) struct RawAlbum {
        pub(super) id: String,
        #[serde(default)]
        pub(super) name: Option<String>,
        #[serde(default)]
        pub(super) title: Option<String>,
        #[serde(default)]
        pub(super) artist: Option<String>,
        #[serde(rename = "songCount", default)]
        pub(super) song_count: u32,
        #[serde(default)]
        pub(super) year: Option<u32>,
        #[serde(rename = "coverArt", default)]
        pub(super) cover_art: Option<String>,
    }

    #[derive(Debug, Deserialize, Default)]
    pub(super) struct AlbumDetailData {
        #[serde(default)]
        pub(super) album: Option<RawAlbumDetail>,
    }

    #[derive(Debug, Deserialize, Default)]
    pub(super) struct RawAlbumDetail {
        #[serde(default)]
        pub(super) id: String,
        #[serde(default)]
        pub(super) name: Option<String>,
        #[serde(default)]
        pub(super) title: Option<String>,
        #[serde(default)]
        pub(super) artist: Option<String>,
        #[serde(rename = "songCount", default)]
        pub(super) song_count: u32,
        #[serde(default)]
        pub(super) year: Option<u32>,
        #[serde(rename = "coverArt", default)]
        pub(super) cover_art: Option<String>,
        #[serde(default)]
        pub(super) song: Vec<RawSong>,
    }

    #[derive(Debug, Deserialize, Default)]
    pub(super) struct Starred2Data {
        #[serde(default)]
        pub(super) starred2: Option<Starred2Container>,
    }

    #[derive(Debug, Deserialize, Default)]
    pub(super) struct Starred2Container {
        #[serde(default)]
        pub(super) song: Vec<RawSong>,
    }

    #[derive(Debug, Deserialize, Default)]
    pub(super) struct Search3Data {
        #[serde(rename = "searchResult3", default)]
        pub(super) search_result3: Option<Search3Container>,
    }

    #[derive(Debug, Deserialize, Default)]
    pub(super) struct Search3Container {
        #[serde(default)]
        pub(super) song: Vec<RawSong>,
        #[serde(default)]
        pub(super) album: Vec<RawAlbum>,
        #[serde(default)]
        pub(super) artist: Vec<RawArtist>,
    }

    #[derive(Debug, Deserialize)]
    pub(super) struct RawArtist {
        pub(super) id: String,
        pub(super) name: String,
        #[serde(rename = "albumCount", default)]
        pub(super) album_count: u32,
        #[serde(rename = "coverArt", default)]
        pub(super) cover_art: Option<String>,
        #[serde(rename = "artistImageUrl", default)]
        pub(super) artist_image_url: Option<String>,
    }

    #[derive(Debug, Deserialize, Default)]
    pub(super) struct ArtistDetailData {
        #[serde(default)]
        pub(super) artist: Option<RawArtistDetail>,
    }

    #[derive(Debug, Deserialize, Default)]
    pub(super) struct RawArtistDetail {
        #[serde(default)]
        pub(super) id: String,
        #[serde(default)]
        pub(super) name: String,
        #[serde(rename = "albumCount", default)]
        pub(super) album_count: u32,
        #[serde(rename = "coverArt", default)]
        pub(super) cover_art: Option<String>,
        #[serde(rename = "artistImageUrl", default)]
        pub(super) artist_image_url: Option<String>,
        #[serde(default)]
        pub(super) album: Vec<RawAlbum>,
    }

    #[derive(Debug, Deserialize, Default)]
    pub(super) struct ArtistsData {
        #[serde(default)]
        pub(super) artists: Option<ArtistsContainer>,
    }

    #[derive(Debug, Deserialize, Default)]
    pub(super) struct ArtistsContainer {
        #[serde(default)]
        pub(super) index: Vec<ArtistIndexBucket>,
    }

    #[derive(Debug, Deserialize, Default)]
    pub(super) struct ArtistIndexBucket {
        #[serde(default)]
        pub(super) artist: Vec<RawArtist>,
    }

    #[derive(Debug, Deserialize)]
    pub(super) struct RawSong {
        pub(super) id: String,
        pub(super) title: String,
        #[serde(default)]
        pub(super) artist: Option<String>,
        #[serde(default)]
        pub(super) album: Option<String>,
        #[serde(rename = "coverArt", default)]
        pub(super) cover_art: Option<String>,
        #[serde(default)]
        pub(super) duration: Option<u32>,
        #[serde(rename = "userRating", default)]
        pub(super) user_rating: Option<u8>,
        #[serde(default)]
        pub(super) starred: Option<String>,
        #[serde(default)]
        pub(super) suffix: Option<String>,
        #[serde(rename = "bitRate", default)]
        pub(super) bit_rate: Option<u32>,
        #[serde(rename = "bitDepth", default)]
        pub(super) bit_depth: Option<u32>,
        #[serde(rename = "samplingRate", default)]
        pub(super) sampling_rate: Option<u32>,
    }

    impl RawSong {
        pub(super) fn into_player_track(self, _info: &NavidromeSessionInfo) -> PlayerTrack {
            let cover_id = self.cover_art.clone().unwrap_or_else(|| self.id.clone());
            let cover_art_url = Some(web_cover_art_url(&cover_id, 256));
            let codec_badge = format_codec_badge(
                self.suffix.as_deref(),
                self.bit_rate,
                self.bit_depth,
                self.sampling_rate,
            );
            PlayerTrack {
                id: self.id,
                title: self.title,
                artist: self.artist.unwrap_or_else(|| "Unknown Artist".to_string()),
                album: self.album,
                cover_art_url,
                cover_art_id: Some(cover_id),
                duration_secs: self.duration,
                is_discovery: false,
                discovery_track_id: None,
                days_remaining: None,
                starred: self.starred.is_some(),
                rating: self.user_rating.unwrap_or(0).min(5),
                codec_badge,
                is_offline_cached: false,
                reason_label: None,
            }
        }
    }

    impl RawAlbum {
        pub(super) fn into_summary(self, _info: &NavidromeSessionInfo) -> SubsonicAlbumSummary {
            let name = self
                .name
                .or(self.title)
                .unwrap_or_else(|| "Untitled Album".to_string());
            let cover_id = self.cover_art.unwrap_or_else(|| self.id.clone());
            SubsonicAlbumSummary {
                id: self.id,
                name,
                artist: self.artist.unwrap_or_else(|| "Unknown Artist".to_string()),
                song_count: self.song_count,
                year: self.year,
                cover_art_url: Some(web_cover_art_url(&cover_id, 300)),
            }
        }
    }

    pub(super) fn non_placeholder_artist_image(url: Option<String>) -> Option<String> {
        url.filter(|u| {
            let trimmed = u.trim();
            !trimmed.is_empty() && !trimmed.contains("2a96cbd8b46e442fc41c2b86b821562f")
        })
    }

    impl RawArtist {
        pub(super) fn into_summary(self, _info: &NavidromeSessionInfo) -> SubsonicArtistSummary {
            let cover_art_url = self
                .cover_art
                .filter(|c| !c.trim().is_empty())
                .map(|cid| web_cover_art_url(&cid, 256))
                .or_else(|| non_placeholder_artist_image(self.artist_image_url));
            SubsonicArtistSummary {
                id: self.id,
                name: self.name,
                album_count: self.album_count,
                cover_art_url,
            }
        }
    }

    fn format_codec_badge(
        suffix: Option<&str>,
        bit_rate: Option<u32>,
        bit_depth: Option<u32>,
        sampling_rate: Option<u32>,
    ) -> Option<String> {
        let ext = suffix?.trim().to_ascii_uppercase();
        if ext.is_empty() {
            return None;
        }
        if matches!(ext.as_str(), "FLAC" | "ALAC" | "WAV" | "AIFF") {
            if let (Some(depth), Some(rate)) = (bit_depth, sampling_rate) {
                if depth > 0 && rate > 0 {
                    let khz = (rate as f64) / 1000.0;
                    if (khz.fract()).abs() < 0.01 {
                        return Some(format!("{ext} {depth}/{}", khz as u32));
                    }
                    return Some(format!("{ext} {depth}/{khz:.1}"));
                }
            }
            return Some(ext);
        }
        if let Some(br) = bit_rate.filter(|&b| b > 0) {
            Some(format!("{ext} {br}"))
        } else {
            Some(ext)
        }
    }
}

#[cfg(feature = "web")]
use wire::*;

#[cfg(feature = "web")]
const SUBSONIC_FETCH_TIMEOUT_MS: u32 = 15_000;

/// Returns `true` when an error string from [`fetch_subsonic`] indicates Subsonic error 40
/// (wrong username or password / expired Navidrome session credentials).
#[cfg(any(feature = "web", test))]
pub fn is_subsonic_auth_error(err: &str) -> bool {
    err.contains("Navidrome error 40:")
}

#[cfg(feature = "web")]
async fn fetch_subsonic<T: serde::de::DeserializeOwned>(
    info: &NavidromeSessionInfo,
    endpoint: &str,
    params: &[(&str, &str)],
) -> Result<T, String> {
    use futures::future::{select, Either};
    use futures::pin_mut;
    use wasm_bindgen::JsCast;
    use wasm_bindgen_futures::JsFuture;

    let url = ui::subsonic_rest_url(info, endpoint, params);
    let window = web_sys::window().ok_or_else(|| "Browser window unavailable".to_string())?;

    let controller = web_sys::AbortController::new().ok();
    let init = web_sys::RequestInit::new();
    if let Some(ref ctrl) = controller {
        let signal = ctrl.signal();
        init.set_signal(Some(&signal));
    }

    let fetch_fut = JsFuture::from(window.fetch_with_str_and_init(&url, &init));
    let timeout_fut = ui::sleep_ms(SUBSONIC_FETCH_TIMEOUT_MS);
    pin_mut!(fetch_fut);
    pin_mut!(timeout_fut);

    let resp_val = match select(fetch_fut, timeout_fut).await {
        Either::Left((res, _)) => {
            res.map_err(|_| "Could not reach Navidrome from browser".to_string())?
        }
        Either::Right((_, _)) => {
            if let Some(ref ctrl) = controller {
                ctrl.abort();
            }
            return Err("Navidrome request timed out after 15s".to_string());
        }
    };

    let resp: web_sys::Response = resp_val
        .dyn_into()
        .map_err(|_| "Unexpected Navidrome response type".to_string())?;
    if !resp.ok() {
        return Err(format!("Navidrome HTTP {}", resp.status()));
    }
    let text_promise = resp
        .text()
        .map_err(|_| "Could not read Navidrome response body".to_string())?;
    let text_val = JsFuture::from(text_promise)
        .await
        .map_err(|_| "Failed reading Navidrome response text".to_string())?;
    let text = text_val
        .as_string()
        .ok_or_else(|| "Navidrome response was not text".to_string())?;
    let env: SubsonicEnvelope<T> =
        serde_json::from_str(&text).map_err(|e| format!("Invalid Navidrome JSON: {e}"))?;
    if env.response.status != "ok" {
        if let Some(err) = env.response.error {
            let msg = err.message.unwrap_or_else(|| "Unknown error".to_string());
            if err.code == 40 {
                dioxus::logger::tracing::warn!(
                    "Navidrome Subsonic error 40 on {endpoint}: {msg}"
                );
            }
            return Err(format!("Navidrome error {}: {msg}", err.code));
        }
        return Err(format!("Navidrome returned status '{}'", env.response.status));
    }
    Ok(env.response.data)
}

#[cfg(feature = "web")]
pub async fn fetch_song(
    info: &NavidromeSessionInfo,
    song_id: &str,
) -> Result<Option<PlayerTrack>, String> {
    let data: SongDetailData = fetch_subsonic(info, "getSong", &[("id", song_id)]).await?;
    Ok(data.song.map(|s| s.into_player_track(info)))
}

#[cfg(feature = "web")]
pub async fn fetch_playlist_live_count(
    info: &NavidromeSessionInfo,
    playlist_id: &str,
) -> Option<u32> {
    let data: PlaylistDetailData =
        fetch_subsonic(info, "getPlaylist", &[("id", playlist_id)])
            .await
            .ok()?;
    data.playlist.map(|p| p.entry.len() as u32)
}

#[cfg(feature = "web")]
pub async fn fetch_playlists(
    info: &NavidromeSessionInfo,
) -> Result<Vec<SubsonicPlaylistSummary>, String> {
    let data: PlaylistsData = fetch_subsonic(info, "getPlaylists", &[]).await?;
    let mut playlists: Vec<SubsonicPlaylistSummary> = data
        .playlists
        .map(|p| p.playlist)
        .unwrap_or_default()
        .into_iter()
        .map(|pl| {
            let comment = pl.comment.unwrap_or_default();
            let owner = pl.owner.unwrap_or_default();
            let is_smart = detect_is_smart_playlist(&pl.name, &comment);
            let category = ui::classify_playlist_category(&pl.id, Some(&comment), is_smart, &[]);
            SubsonicPlaylistSummary {
                id: pl.id,
                name: pl.name,
                song_count: pl.song_count,
                duration: pl.duration,
                cover_art: pl.cover_art,
                comment,
                owner,
                public: pl.public,
                is_smart,
                category,
            }
        })
        .collect();

    // Navidrome's `getPlaylists` `songCount` can include stale `missing = true` rows on smart /
    // Soulbeet playlists. Resolve exact live non-missing counts via concurrent `getPlaylist` calls.
    let resolve_all = playlists.len() <= 25;
    let indices_to_resolve: Vec<usize> = playlists
        .iter()
        .enumerate()
        .filter_map(|(idx, pl)| {
            (resolve_all || pl.is_smart || pl.category.is_soulbeet_mix()).then_some(idx)
        })
        .collect();

    if !indices_to_resolve.is_empty() {
        use futures::stream::{self, StreamExt};
        let resolved: Vec<(usize, Option<u32>)> = stream::iter(indices_to_resolve.into_iter().map(
            |idx| {
                let pid = playlists[idx].id.clone();
                async move { (idx, fetch_playlist_live_count(info, &pid).await) }
            },
        ))
        .buffer_unordered(6)
        .collect()
        .await;
        for (idx, live_count) in resolved {
            if let Some(count) = live_count {
                playlists[idx].song_count = count;
            }
        }
    }

    Ok(playlists)
}

#[cfg(not(feature = "web"))]
pub async fn fetch_playlists(
    _info: &NavidromeSessionInfo,
) -> Result<Vec<SubsonicPlaylistSummary>, String> {
    Ok(Vec::new())
}

#[cfg(feature = "web")]
pub async fn fetch_playlist_tracks(
    info: &NavidromeSessionInfo,
    playlist_id: &str,
) -> Result<Vec<PlayerTrack>, String> {
    let data: PlaylistDetailData =
        fetch_subsonic(info, "getPlaylist", &[("id", playlist_id)]).await?;
    Ok(data
        .playlist
        .map(|p| p.entry)
        .unwrap_or_default()
        .into_iter()
        .map(|s| s.into_player_track(info))
        .collect())
}

#[cfg(not(feature = "web"))]
pub async fn fetch_playlist_tracks(
    _info: &NavidromeSessionInfo,
    _playlist_id: &str,
) -> Result<Vec<PlayerTrack>, String> {
    Ok(Vec::new())
}

/// Supported Navidrome OpenSubsonic `getAlbumList2` sort orders for the Library Albums view.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AlbumListSort {
    #[default]
    AlphabeticalByName,
    AlphabeticalByArtist,
    Newest,
    Recent,
    Frequent,
}

impl AlbumListSort {
    pub const ALL: [Self; 5] = [
        Self::AlphabeticalByName,
        Self::AlphabeticalByArtist,
        Self::Newest,
        Self::Recent,
        Self::Frequent,
    ];

    pub fn as_subsonic_type(self) -> &'static str {
        match self {
            Self::AlphabeticalByName => "alphabeticalByName",
            Self::AlphabeticalByArtist => "alphabeticalByArtist",
            Self::Newest => "newest",
            Self::Recent => "recent",
            Self::Frequent => "frequent",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::AlphabeticalByName => "A-Z",
            Self::AlphabeticalByArtist => "Artist",
            Self::Newest => "Newest",
            Self::Recent => "Recently Played",
            Self::Frequent => "Most Played",
        }
    }
}

/// Page size for paginated `getAlbumList2` requests (well within Navidrome's 500-item server cap).
pub const ALBUM_LIST_PAGE_SIZE: u32 = 200;

#[cfg(feature = "web")]
pub async fn fetch_album_list(
    info: &NavidromeSessionInfo,
    sort: AlbumListSort,
    size: u32,
    offset: u32,
) -> Result<Vec<SubsonicAlbumSummary>, String> {
    let size_s = size.clamp(1, 500).to_string();
    let offset_s = offset.to_string();
    let data: AlbumList2Data = fetch_subsonic(
        info,
        "getAlbumList2",
        &[
            ("type", sort.as_subsonic_type()),
            ("size", &size_s),
            ("offset", &offset_s),
        ],
    )
    .await?;
    Ok(data
        .album_list2
        .map(|a| a.album)
        .unwrap_or_default()
        .into_iter()
        .map(|a| a.into_summary(info))
        .collect())
}

#[cfg(not(feature = "web"))]
pub async fn fetch_album_list(
    _info: &NavidromeSessionInfo,
    _sort: AlbumListSort,
    _size: u32,
    _offset: u32,
) -> Result<Vec<SubsonicAlbumSummary>, String> {
    Ok(Vec::new())
}

pub async fn fetch_recent_albums(
    info: &NavidromeSessionInfo,
    size: u32,
) -> Result<Vec<SubsonicAlbumSummary>, String> {
    fetch_album_list(info, AlbumListSort::Recent, size, 0).await
}

#[cfg(feature = "web")]
pub async fn fetch_album_tracks(
    info: &NavidromeSessionInfo,
    album_id: &str,
) -> Result<Vec<PlayerTrack>, String> {
    let data: AlbumDetailData = fetch_subsonic(info, "getAlbum", &[("id", album_id)]).await?;
    Ok(data
        .album
        .map(|a| a.song)
        .unwrap_or_default()
        .into_iter()
        .map(|s| s.into_player_track(info))
        .collect())
}

#[cfg(not(feature = "web"))]
pub async fn fetch_album_tracks(
    _info: &NavidromeSessionInfo,
    _album_id: &str,
) -> Result<Vec<PlayerTrack>, String> {
    Ok(Vec::new())
}

#[cfg(feature = "web")]
pub async fn fetch_starred_tracks(
    info: &NavidromeSessionInfo,
) -> Result<Vec<PlayerTrack>, String> {
    let data: Starred2Data = fetch_subsonic(info, "getStarred2", &[]).await?;
    Ok(data
        .starred2
        .map(|s| s.song)
        .unwrap_or_default()
        .into_iter()
        .map(|s| {
            let mut t = s.into_player_track(info);
            t.starred = true;
            t
        })
        .collect())
}

#[cfg(not(feature = "web"))]
pub async fn fetch_starred_tracks(
    _info: &NavidromeSessionInfo,
) -> Result<Vec<PlayerTrack>, String> {
    Ok(Vec::new())
}

#[cfg(feature = "web")]
pub async fn search_navidrome(
    info: &NavidromeSessionInfo,
    query: &str,
) -> Result<NavidromeSearchHits, String> {
    let data: Search3Data = fetch_subsonic(
        info,
        "search3",
        &[
            ("query", query),
            ("songCount", "24"),
            ("albumCount", "8"),
            ("artistCount", "8"),
        ],
    )
    .await?;
    let Some(res) = data.search_result3 else {
        return Ok(NavidromeSearchHits::default());
    };
    Ok(NavidromeSearchHits {
        songs: res
            .song
            .into_iter()
            .map(|s| s.into_player_track(info))
            .collect(),
        albums: res
            .album
            .into_iter()
            .map(|a| a.into_summary(info))
            .collect(),
        artists: res
            .artist
            .into_iter()
            .map(|a| a.into_summary(info))
            .collect(),
    })
}

#[cfg(not(feature = "web"))]
pub async fn search_navidrome(
    _info: &NavidromeSessionInfo,
    _query: &str,
) -> Result<NavidromeSearchHits, String> {
    Ok(NavidromeSearchHits::default())
}

#[cfg(feature = "web")]
pub async fn fetch_playlist_detail(
    info: &NavidromeSessionInfo,
    playlist_id: &str,
) -> Result<Option<(SubsonicPlaylistSummary, Vec<PlayerTrack>)>, String> {
    let data: PlaylistDetailData =
        fetch_subsonic(info, "getPlaylist", &[("id", playlist_id)]).await?;
    let Some(pl) = data.playlist else {
        return Ok(None);
    };
    let comment = pl.comment.unwrap_or_default();
    let owner = pl.owner.unwrap_or_default();
    let is_smart = detect_is_smart_playlist(&pl.name, &comment);
    let category = ui::classify_playlist_category(&pl.id, Some(&comment), is_smart, &[]);
    let tracks: Vec<PlayerTrack> = pl
        .entry
        .into_iter()
        .map(|s| s.into_player_track(info))
        .collect();
    let summary = SubsonicPlaylistSummary {
        id: if pl.id.is_empty() {
            playlist_id.to_string()
        } else {
            pl.id
        },
        name: pl.name,
        song_count: tracks.len() as u32,
        duration: pl.duration,
        cover_art: pl.cover_art,
        comment,
        owner,
        public: pl.public,
        is_smart,
        category,
    };
    Ok(Some((summary, tracks)))
}

#[cfg(not(feature = "web"))]
pub async fn fetch_playlist_detail(
    _info: &NavidromeSessionInfo,
    _playlist_id: &str,
) -> Result<Option<(SubsonicPlaylistSummary, Vec<PlayerTrack>)>, String> {
    Ok(None)
}

#[cfg(feature = "web")]
pub async fn fetch_album_detail(
    info: &NavidromeSessionInfo,
    album_id: &str,
) -> Result<Option<(SubsonicAlbumSummary, Vec<PlayerTrack>)>, String> {
    let data: AlbumDetailData = fetch_subsonic(info, "getAlbum", &[("id", album_id)]).await?;
    let Some(alb) = data.album else {
        return Ok(None);
    };
    let tracks: Vec<PlayerTrack> = alb
        .song
        .into_iter()
        .map(|s| s.into_player_track(info))
        .collect();
    let name = alb
        .name
        .or(alb.title)
        .unwrap_or_else(|| "Untitled Album".to_string());
    let cover_id = alb.cover_art.unwrap_or_else(|| album_id.to_string());
    let summary = SubsonicAlbumSummary {
        id: if alb.id.is_empty() {
            album_id.to_string()
        } else {
            alb.id
        },
        name,
        artist: alb.artist.unwrap_or_else(|| "Unknown Artist".to_string()),
        song_count: if alb.song_count > 0 {
            alb.song_count
        } else {
            tracks.len() as u32
        },
        year: alb.year,
        cover_art_url: Some(web_cover_art_url(&cover_id, 400)),
    };
    Ok(Some((summary, tracks)))
}

#[cfg(not(feature = "web"))]
pub async fn fetch_album_detail(
    _info: &NavidromeSessionInfo,
    _album_id: &str,
) -> Result<Option<(SubsonicAlbumSummary, Vec<PlayerTrack>)>, String> {
    Ok(None)
}

#[cfg(feature = "web")]
pub async fn fetch_artist_detail(
    info: &NavidromeSessionInfo,
    artist_id: &str,
) -> Result<Option<(SubsonicArtistSummary, Vec<SubsonicAlbumSummary>)>, String> {
    let data: ArtistDetailData = fetch_subsonic(info, "getArtist", &[("id", artist_id)]).await?;
    let Some(art) = data.artist else {
        return Ok(None);
    };
    let albums: Vec<SubsonicAlbumSummary> = art
        .album
        .into_iter()
        .map(|a| a.into_summary(info))
        .collect();
    let cover_art_url = art
        .cover_art
        .filter(|c| !c.trim().is_empty())
        .map(|cid| web_cover_art_url(&cid, 300))
        .or_else(|| non_placeholder_artist_image(art.artist_image_url))
        .or_else(|| albums.first().and_then(|a| a.cover_art_url.clone()));
    let summary = SubsonicArtistSummary {
        id: if art.id.is_empty() {
            artist_id.to_string()
        } else {
            art.id
        },
        name: art.name,
        album_count: if art.album_count > 0 {
            art.album_count
        } else {
            albums.len() as u32
        },
        cover_art_url,
    };
    Ok(Some((summary, albums)))
}

#[cfg(not(feature = "web"))]
pub async fn fetch_artist_detail(
    _info: &NavidromeSessionInfo,
    _artist_id: &str,
) -> Result<Option<(SubsonicArtistSummary, Vec<SubsonicAlbumSummary>)>, String> {
    Ok(None)
}

#[cfg(feature = "web")]
pub async fn fetch_artists(
    info: &NavidromeSessionInfo,
) -> Result<Vec<SubsonicArtistSummary>, String> {
    let data: ArtistsData = fetch_subsonic(info, "getArtists", &[]).await?;
    let mut out = Vec::new();
    if let Some(container) = data.artists {
        for bucket in container.index {
            for artist in bucket.artist {
                out.push(artist.into_summary(info));
            }
        }
    }
    Ok(out)
}

#[cfg(not(feature = "web"))]
pub async fn fetch_artists(
    _info: &NavidromeSessionInfo,
) -> Result<Vec<SubsonicArtistSummary>, String> {
    Ok(Vec::new())
}

#[cfg(feature = "web")]
pub async fn fetch_library_tracks(
    info: &NavidromeSessionInfo,
    offset: u32,
    size: u32,
) -> Result<Vec<PlayerTrack>, String> {
    let size_s = size.clamp(1, 500).to_string();
    let offset_s = offset.to_string();
    let data: Search3Data = fetch_subsonic(
        info,
        "search3",
        &[
            ("query", ""),
            ("songCount", &size_s),
            ("songOffset", &offset_s),
            ("albumCount", "0"),
            ("artistCount", "0"),
        ],
    )
    .await?;
    Ok(data
        .search_result3
        .map(|r| r.song)
        .unwrap_or_default()
        .into_iter()
        .map(|s| s.into_player_track(info))
        .collect())
}

#[cfg(not(feature = "web"))]
pub async fn fetch_library_tracks(
    _info: &NavidromeSessionInfo,
    _offset: u32,
    _size: u32,
) -> Result<Vec<PlayerTrack>, String> {
    Ok(Vec::new())
}

#[cfg(feature = "web")]
pub async fn send_scrobble(info: &NavidromeSessionInfo, song_id: &str, submission: bool) -> bool {
    let sub = if submission { "true" } else { "false" };
    match fetch_subsonic::<serde_json::Value>(
        info,
        "scrobble",
        &[("id", song_id), ("submission", sub)],
    )
    .await
    {
        Ok(_) => true,
        Err(err) => {
            dioxus::logger::tracing::warn!(
                "Navidrome scrobble failed for {song_id} (submission={sub}): {err}"
            );
            !is_subsonic_auth_error(&err)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_smart_and_soulbeet_playlists_without_false_positives() {
        assert!(detect_is_smart_playlist("Comfort Zone", "Soulbeet Discovery: conservative"));
        assert!(detect_is_smart_playlist("Summer Anthems", "soulbeet:contextual:summer"));
        assert!(detect_is_smart_playlist("Favorites", "Auto smart playlist"));
        assert!(!detect_is_smart_playlist("Weekly Discovery", ""));
        assert!(!detect_is_smart_playlist("Smart Indie Mix", "Favorite smart pop songs"));
        assert!(!detect_is_smart_playlist("Road Trip", "Songs for the highway"));
    }

    #[test]
    fn album_list_sort_defaults_to_full_catalog_and_maps_exact_subsonic_types() {
        assert_eq!(AlbumListSort::default(), AlbumListSort::AlphabeticalByName);
        assert_eq!(
            AlbumListSort::AlphabeticalByName.as_subsonic_type(),
            "alphabeticalByName"
        );
        assert_eq!(
            AlbumListSort::AlphabeticalByArtist.as_subsonic_type(),
            "alphabeticalByArtist"
        );
        assert_eq!(AlbumListSort::Newest.as_subsonic_type(), "newest");
        assert_eq!(AlbumListSort::Recent.as_subsonic_type(), "recent");
        assert_eq!(AlbumListSort::Frequent.as_subsonic_type(), "frequent");
    }

    #[test]
    fn recognizes_subsonic_error_40_auth_failures() {
        assert!(is_subsonic_auth_error("Navidrome error 40: Wrong username or password"));
        assert!(!is_subsonic_auth_error("Navidrome error 70: Song not found"));
        assert!(!is_subsonic_auth_error("Request timed out"));
    }
}
