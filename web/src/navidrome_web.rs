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
use ui::subsonic_cover_art_url;
use ui::PlayerTrack;

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SubsonicPlaylistSummary {
    pub id: String,
    pub name: String,
    pub song_count: u32,
    pub duration: u32,
    pub cover_art: Option<String>,
    pub is_smart: bool,
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
pub struct NavidromeSearchHits {
    pub songs: Vec<PlayerTrack>,
    pub albums: Vec<SubsonicAlbumSummary>,
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
    }

    #[derive(Debug, Deserialize, Default)]
    pub(super) struct PlaylistDetailData {
        #[serde(default)]
        pub(super) playlist: Option<RawPlaylistDetail>,
    }

    #[derive(Debug, Deserialize, Default)]
    pub(super) struct RawPlaylistDetail {
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
        pub(super) fn into_player_track(self, info: &NavidromeSessionInfo) -> PlayerTrack {
            let cover_id = self.cover_art.clone().unwrap_or_else(|| self.id.clone());
            let cover_art_url = Some(subsonic_cover_art_url(info, &cover_id, 256));
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
        pub(super) fn into_summary(self, info: &NavidromeSessionInfo) -> SubsonicAlbumSummary {
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
                cover_art_url: Some(subsonic_cover_art_url(info, &cover_id, 300)),
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
async fn fetch_subsonic<T: serde::de::DeserializeOwned>(
    info: &NavidromeSessionInfo,
    endpoint: &str,
    params: &[(&str, &str)],
) -> Result<T, String> {
    use wasm_bindgen::JsCast;
    use wasm_bindgen_futures::JsFuture;

    let url = ui::subsonic_rest_url(info, endpoint, params);
    let window = web_sys::window().ok_or_else(|| "Browser window unavailable".to_string())?;
    let resp_val = JsFuture::from(window.fetch_with_str(&url))
        .await
        .map_err(|_| "Could not reach Navidrome from browser".to_string())?;
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
pub async fn fetch_playlists(
    info: &NavidromeSessionInfo,
) -> Result<Vec<SubsonicPlaylistSummary>, String> {
    let data: PlaylistsData = fetch_subsonic(info, "getPlaylists", &[]).await?;
    Ok(data
        .playlists
        .map(|p| p.playlist)
        .unwrap_or_default()
        .into_iter()
        .map(|pl| {
            let is_smart = pl
                .comment
                .as_deref()
                .is_some_and(|c| c.to_ascii_lowercase().contains("smart"))
                || pl.name.to_ascii_lowercase().contains("discovery");
            SubsonicPlaylistSummary {
                id: pl.id,
                name: pl.name,
                song_count: pl.song_count,
                duration: pl.duration,
                cover_art: pl.cover_art,
                is_smart,
            }
        })
        .collect())
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

#[cfg(feature = "web")]
pub async fn fetch_recent_albums(
    info: &NavidromeSessionInfo,
    size: u32,
) -> Result<Vec<SubsonicAlbumSummary>, String> {
    let size_s = size.to_string();
    let data: AlbumList2Data =
        fetch_subsonic(info, "getAlbumList2", &[("type", "recent"), ("size", &size_s)]).await?;
    Ok(data
        .album_list2
        .map(|a| a.album)
        .unwrap_or_default()
        .into_iter()
        .map(|a| a.into_summary(info))
        .collect())
}

#[cfg(not(feature = "web"))]
pub async fn fetch_recent_albums(
    _info: &NavidromeSessionInfo,
    _size: u32,
) -> Result<Vec<SubsonicAlbumSummary>, String> {
    Ok(Vec::new())
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
            ("artistCount", "0"),
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
pub async fn send_scrobble(info: &NavidromeSessionInfo, song_id: &str, submission: bool) {
    let sub = if submission { "true" } else { "false" };
    let _ = fetch_subsonic::<serde_json::Value>(
        info,
        "scrobble",
        &[("id", song_id), ("submission", sub)],
    )
    .await;
}
