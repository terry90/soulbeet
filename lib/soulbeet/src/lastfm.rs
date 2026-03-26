use reqwest::Client;
use serde::Deserialize;
use shared::metadata::{Album, AlbumWithTracks, SearchResult, Track};
use shared::recommendation::{
    ArtistPopularity, Listen, RankedArtist, RankedTrack, SimilarArtist, SimilarTrack, TimePeriod,
    WeightedTag,
};
use tracing::{info, warn};

use crate::error::{Result, SoulseekError};

const LASTFM_API_BASE: &str = "https://ws.audioscrobbler.com/2.0/";

#[derive(Debug, Deserialize)]
struct LastFmImage {
    #[serde(rename = "#text")]
    url: String,
    size: String,
}

impl LastFmImage {
    fn get_largest(images: &[LastFmImage]) -> Option<String> {
        let sizes = ["extralarge", "large", "medium", "small"];
        for size in sizes {
            if let Some(img) = images.iter().find(|i| i.size == size) {
                if !img.url.is_empty() {
                    return Some(img.url.clone());
                }
            }
        }
        images
            .iter()
            .find(|i| !i.url.is_empty())
            .map(|i| i.url.clone())
    }
}

#[derive(Debug, Deserialize)]
struct AlbumSearchResponse {
    results: AlbumSearchResults,
}

#[derive(Debug, Deserialize)]
struct AlbumSearchResults {
    #[serde(rename = "albummatches")]
    album_matches: AlbumMatches,
}

#[derive(Debug, Deserialize)]
struct AlbumMatches {
    album: Vec<LastFmAlbum>,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct LastFmAlbum {
    name: String,
    artist: String,
    url: String,
    #[serde(default)]
    mbid: Option<String>,
    #[serde(default)]
    image: Vec<LastFmImage>,
}

#[derive(Debug, Deserialize)]
struct TrackSearchResponse {
    results: TrackSearchResults,
}

#[derive(Debug, Deserialize)]
struct TrackSearchResults {
    #[serde(rename = "trackmatches")]
    track_matches: TrackMatches,
}

#[derive(Debug, Deserialize)]
struct TrackMatches {
    track: Vec<LastFmTrack>,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct LastFmTrack {
    name: String,
    artist: String,
    url: String,
    #[serde(default)]
    mbid: Option<String>,
    #[serde(default)]
    listeners: Option<String>,
}

#[derive(Debug, Deserialize)]
struct AlbumInfoResponse {
    album: LastFmAlbumInfo,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct LastFmAlbumInfo {
    name: String,
    artist: String,
    #[serde(default)]
    mbid: Option<String>,
    url: String,
    #[serde(default)]
    image: Vec<LastFmImage>,
    #[serde(default)]
    tracks: Option<LastFmTracks>,
    #[serde(default)]
    wiki: Option<LastFmWiki>,
}

#[derive(Debug, Deserialize)]
struct LastFmTracks {
    track: LastFmTrackList,
}

// Last.fm returns either a single track object or an array
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum LastFmTrackList {
    Single(Box<LastFmAlbumTrack>),
    Multiple(Vec<LastFmAlbumTrack>),
}

impl LastFmTrackList {
    fn into_vec(self) -> Vec<LastFmAlbumTrack> {
        match self {
            LastFmTrackList::Single(track) => vec![*track],
            LastFmTrackList::Multiple(tracks) => tracks,
        }
    }
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct LastFmAlbumTrack {
    name: String,
    #[serde(default)]
    duration: Option<u32>,
    #[serde(default)]
    mbid: Option<String>,
    #[serde(default)]
    artist: Option<LastFmTrackArtist>,
    #[serde(rename = "@attr", default)]
    attr: Option<TrackAttr>,
}

#[derive(Debug, Deserialize)]
struct LastFmTrackArtist {
    name: String,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct TrackAttr {
    rank: u32,
}

#[derive(Debug, Deserialize)]
struct LastFmWiki {
    #[serde(default)]
    published: Option<String>,
}

pub struct LastFmProvider {
    client: Client,
    api_key: String,
    username: Option<String>,
}

impl LastFmProvider {
    fn build_client() -> Client {
        Client::builder()
            .timeout(std::time::Duration::from_secs(15))
            .connect_timeout(std::time::Duration::from_secs(5))
            .build()
            .expect("failed to build HTTP client")
    }

    pub fn new(api_key: impl Into<String>) -> Self {
        Self {
            client: Self::build_client(),
            api_key: api_key.into(),
            username: None,
        }
    }

    pub fn with_user(api_key: impl Into<String>, username: impl Into<String>) -> Self {
        Self {
            client: Self::build_client(),
            api_key: api_key.into(),
            username: Some(username.into()),
        }
    }

    pub fn from_env() -> Option<Self> {
        std::env::var("LASTFM_API_KEY").ok().map(Self::new)
    }

    fn require_username(&self) -> Result<&str> {
        self.username.as_deref().ok_or(SoulseekError::Api {
            status: 400,
            message: "Last.fm username not configured".to_string(),
        })
    }

    /// Send a GET request to the Last.fm API with retries and deserialize the response.
    async fn api_request<T: serde::de::DeserializeOwned>(
        &self,
        params: &[(&str, &str)],
    ) -> Result<T> {
        let mut all_params: Vec<(&str, &str)> = params.to_vec();
        all_params.push(("api_key", &self.api_key));
        all_params.push(("format", "json"));

        let url = reqwest::Url::parse_with_params(LASTFM_API_BASE, &all_params).map_err(|e| {
            SoulseekError::Api {
                status: 500,
                message: format!("Failed to build URL: {}", e),
            }
        })?;

        crate::http::lastfm_rate_limit().await;

        let client = self.client.clone();
        let url_str = url.to_string();
        let response = crate::http::resilient_send(|| client.get(&url_str), "Last.fm API").await?;

        response.json().await.map_err(|e| SoulseekError::Api {
            status: 500,
            message: format!("Failed to parse Last.fm response: {}", e),
        })
    }

    // --- Metadata search internals (used by MetadataProvider) ---

    async fn search_albums_internal(
        &self,
        artist: Option<&str>,
        query: &str,
        limit: usize,
    ) -> Result<Vec<LastFmAlbum>> {
        let album_query = if let Some(artist) = artist {
            format!("{} {}", artist, query)
        } else {
            query.to_string()
        };

        let limit_str = limit.to_string();
        let data: AlbumSearchResponse = self
            .api_request(&[
                ("method", "album.search"),
                ("album", &album_query),
                ("limit", &limit_str),
            ])
            .await?;

        Ok(data.results.album_matches.album)
    }

    async fn search_tracks_internal(
        &self,
        artist: Option<&str>,
        query: &str,
        limit: usize,
    ) -> Result<Vec<LastFmTrack>> {
        let limit_str = limit.to_string();
        let mut params = vec![
            ("method", "track.search"),
            ("track", query),
            ("limit", limit_str.as_str()),
        ];

        if let Some(artist) = artist {
            params.push(("artist", artist));
        }

        let data: TrackSearchResponse = self.api_request(&params).await?;
        Ok(data.results.track_matches.track)
    }

    async fn get_album_info(&self, artist: &str, album: &str) -> Result<LastFmAlbumInfo> {
        info!("Fetching album info from Last.fm: {} - {}", artist, album);
        let data: AlbumInfoResponse = self
            .api_request(&[
                ("method", "album.getInfo"),
                ("artist", artist),
                ("album", album),
            ])
            .await?;
        Ok(data.album)
    }

    // --- Scrobble/user internals ---

    async fn get_recent_tracks_internal(&self, limit: u32) -> Result<Vec<LastFmRecentTrack>> {
        let username = self.require_username()?;
        let limit_str = limit.to_string();
        let data: RecentTracksResponse = self
            .api_request(&[
                ("method", "user.getRecentTracks"),
                ("user", username),
                ("limit", &limit_str),
            ])
            .await?;
        Ok(data.recenttracks.track)
    }

    async fn get_top_artists_internal(
        &self,
        period: &str,
        limit: u32,
    ) -> Result<Vec<LastFmTopArtist>> {
        let username = self.require_username()?;
        let limit_str = limit.to_string();
        let data: TopArtistsResponse = self
            .api_request(&[
                ("method", "user.getTopArtists"),
                ("user", username),
                ("period", period),
                ("limit", &limit_str),
            ])
            .await?;
        Ok(data.topartists.artist)
    }

    async fn get_top_tracks_internal(
        &self,
        period: &str,
        limit: u32,
    ) -> Result<Vec<LastFmTopTrack>> {
        let username = self.require_username()?;
        let limit_str = limit.to_string();
        let data: TopTracksResponse = self
            .api_request(&[
                ("method", "user.getTopTracks"),
                ("user", username),
                ("period", period),
                ("limit", &limit_str),
            ])
            .await?;
        Ok(data.toptracks.track)
    }

    async fn get_similar_artists_internal(
        &self,
        artist: &str,
        limit: u32,
    ) -> Result<Vec<LastFmSimilarArtist>> {
        let limit_str = limit.to_string();
        let data: SimilarArtistsResponse = self
            .api_request(&[
                ("method", "artist.getSimilar"),
                ("artist", artist),
                ("limit", &limit_str),
            ])
            .await?;
        Ok(data.similarartists.artist)
    }

    async fn get_artist_top_tracks_internal(
        &self,
        artist: &str,
        limit: u32,
    ) -> Result<Vec<LastFmArtistTopTrack>> {
        let limit_str = limit.to_string();
        let data: ArtistTopTracksResponse = self
            .api_request(&[
                ("method", "artist.getTopTracks"),
                ("artist", artist),
                ("limit", &limit_str),
            ])
            .await?;
        Ok(data.toptracks.track)
    }

    async fn get_artist_top_tags_internal(&self, artist: &str) -> Result<Vec<LastFmTag>> {
        let data: ArtistTopTagsResponse = self
            .api_request(&[("method", "artist.getTopTags"), ("artist", artist)])
            .await?;
        Ok(data.toptags.tag)
    }

    async fn get_artist_info_internal(&self, artist: &str) -> Result<LastFmArtistInfo> {
        let data: ArtistInfoResponse = self
            .api_request(&[("method", "artist.getInfo"), ("artist", artist)])
            .await?;
        Ok(data.artist)
    }

    async fn get_similar_tracks_internal(
        &self,
        artist: &str,
        track: &str,
        limit: u32,
    ) -> Result<Vec<LastFmSimilarTrack>> {
        let limit_str = limit.to_string();
        let data: SimilarTracksResponse = self
            .api_request(&[
                ("method", "track.getSimilar"),
                ("artist", artist),
                ("track", track),
                ("limit", &limit_str),
            ])
            .await?;
        Ok(data.similartracks.track)
    }

    async fn get_tag_similar_internal(&self, tag: &str) -> Result<Vec<LastFmTagEntry>> {
        let data: TagSimilarResponse = self
            .api_request(&[("method", "tag.getSimilar"), ("tag", tag)])
            .await?;
        Ok(data.similartags.tag)
    }

    async fn get_tag_top_tracks_internal(
        &self,
        tag: &str,
        limit: u32,
    ) -> Result<Vec<LastFmTagTopTrack>> {
        let limit_str = limit.to_string();
        let data: TagTopTracksResponse = self
            .api_request(&[
                ("method", "tag.getTopTracks"),
                ("tag", tag),
                ("limit", &limit_str),
            ])
            .await?;
        Ok(data.tracks.track)
    }

    async fn get_chart_top_artists_internal(&self, limit: u32) -> Result<Vec<LastFmChartArtist>> {
        let limit_str = limit.to_string();
        let data: ChartTopArtistsResponse = self
            .api_request(&[("method", "chart.getTopArtists"), ("limit", &limit_str)])
            .await?;
        Ok(data.artists.artist)
    }
}

// --- Response types for scrobble/user API ---

// user.getRecentTracks
#[derive(Debug, Deserialize)]
struct RecentTracksResponse {
    recenttracks: RecentTracksPayload,
}

#[derive(Debug, Deserialize)]
struct RecentTracksPayload {
    track: Vec<LastFmRecentTrack>,
}

#[derive(Debug, Deserialize)]
struct LastFmRecentTrack {
    name: String,
    artist: LastFmRecentTrackField,
    album: LastFmRecentTrackField,
    #[serde(default)]
    date: Option<LastFmDate>,
    #[serde(rename = "@attr", default)]
    attr: Option<NowPlayingAttr>,
}

#[derive(Debug, Deserialize)]
struct LastFmRecentTrackField {
    #[serde(rename = "#text")]
    text: String,
}

#[derive(Debug, Deserialize)]
struct LastFmDate {
    uts: String,
}

#[derive(Debug, Deserialize)]
struct NowPlayingAttr {
    #[serde(default)]
    nowplaying: Option<String>,
}

// user.getTopArtists
#[derive(Debug, Deserialize)]
struct TopArtistsResponse {
    topartists: TopArtistsPayload,
}

#[derive(Debug, Deserialize)]
struct TopArtistsPayload {
    artist: Vec<LastFmTopArtist>,
}

#[derive(Debug, Deserialize)]
struct LastFmTopArtist {
    name: String,
    #[serde(default)]
    mbid: Option<String>,
    #[serde(default, rename = "playcount")]
    play_count: Option<String>,
}

// user.getTopTracks
#[derive(Debug, Deserialize)]
struct TopTracksResponse {
    toptracks: TopTracksPayload,
}

#[derive(Debug, Deserialize)]
struct TopTracksPayload {
    track: Vec<LastFmTopTrack>,
}

#[derive(Debug, Deserialize)]
struct LastFmTopTrack {
    name: String,
    artist: LastFmTopTrackArtist,
    #[serde(default)]
    mbid: Option<String>,
    #[serde(default, rename = "playcount")]
    play_count: Option<String>,
}

#[derive(Debug, Deserialize)]
struct LastFmTopTrackArtist {
    name: String,
}

// artist.getSimilar
#[derive(Debug, Deserialize)]
struct SimilarArtistsResponse {
    similarartists: SimilarArtistsPayload,
}

#[derive(Debug, Deserialize)]
struct SimilarArtistsPayload {
    artist: Vec<LastFmSimilarArtist>,
}

#[derive(Debug, Deserialize)]
struct LastFmSimilarArtist {
    name: String,
    #[serde(default, rename = "match")]
    match_score: Option<String>,
    #[serde(default)]
    mbid: Option<String>,
}

// artist.getTopTracks
#[derive(Debug, Deserialize)]
struct ArtistTopTracksResponse {
    toptracks: ArtistTopTracksPayload,
}

#[derive(Debug, Deserialize)]
struct ArtistTopTracksPayload {
    track: Vec<LastFmArtistTopTrack>,
}

#[derive(Debug, Deserialize)]
struct LastFmArtistTopTrack {
    name: String,
    #[serde(default, rename = "playcount")]
    play_count: Option<String>,
    #[serde(default)]
    mbid: Option<String>,
    artist: LastFmTopTrackArtist,
}

// artist.getTopTags
#[derive(Debug, Deserialize)]
struct ArtistTopTagsResponse {
    toptags: ArtistTopTagsPayload,
}

#[derive(Debug, Deserialize)]
struct ArtistTopTagsPayload {
    tag: Vec<LastFmTag>,
}

#[derive(Debug, Deserialize)]
struct LastFmTag {
    name: String,
    #[serde(default)]
    count: Option<u32>,
}

// artist.getInfo
#[derive(Debug, Deserialize)]
struct ArtistInfoResponse {
    artist: LastFmArtistInfo,
}

#[derive(Debug, Deserialize)]
struct LastFmArtistInfo {
    #[serde(default)]
    stats: Option<LastFmArtistStats>,
}

#[derive(Debug, Deserialize)]
struct LastFmArtistStats {
    #[serde(default)]
    listeners: Option<String>,
    #[serde(default, rename = "playcount")]
    play_count: Option<String>,
}

// track.getSimilar
#[derive(Debug, Deserialize)]
struct SimilarTracksResponse {
    similartracks: SimilarTracksPayload,
}

#[derive(Debug, Deserialize)]
struct SimilarTracksPayload {
    track: Vec<LastFmSimilarTrack>,
}

#[derive(Debug, Deserialize)]
struct LastFmSimilarTrack {
    name: String,
    artist: LastFmSimilarTrackArtist,
    #[serde(default, rename = "match")]
    match_score: Option<f64>,
    #[serde(default)]
    mbid: Option<String>,
}

#[derive(Debug, Deserialize)]
struct LastFmSimilarTrackArtist {
    name: String,
}

// tag.getSimilar
#[derive(Debug, Deserialize)]
struct TagSimilarResponse {
    similartags: TagSimilarPayload,
}

#[derive(Debug, Deserialize)]
struct TagSimilarPayload {
    tag: Vec<LastFmTagEntry>,
}

#[derive(Debug, Deserialize)]
struct LastFmTagEntry {
    name: String,
}

// tag.getTopTracks
#[derive(Debug, Deserialize)]
struct TagTopTracksResponse {
    tracks: TagTopTracksPayload,
}

#[derive(Debug, Deserialize)]
struct TagTopTracksPayload {
    track: Vec<LastFmTagTopTrack>,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct LastFmTagTopTrack {
    name: String,
    artist: LastFmTagTopTrackArtist,
    #[serde(default)]
    mbid: Option<String>,
    #[serde(rename = "@attr", default)]
    attr: Option<TagTrackAttr>,
}

#[derive(Debug, Deserialize)]
struct LastFmTagTopTrackArtist {
    name: String,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct TagTrackAttr {
    rank: String,
}

// chart.getTopArtists
#[derive(Debug, Deserialize)]
struct ChartTopArtistsResponse {
    artists: ChartArtistsPayload,
}

#[derive(Debug, Deserialize)]
struct ChartArtistsPayload {
    artist: Vec<LastFmChartArtist>,
}

#[derive(Debug, Deserialize)]
struct LastFmChartArtist {
    #[serde(default)]
    listeners: Option<String>,
}

// --- Helper functions ---

fn format_duration(seconds: Option<u32>) -> Option<String> {
    seconds.map(|s| {
        let minutes = s / 60;
        let secs = s % 60;
        format!("{:02}:{:02}", minutes, secs)
    })
}

fn generate_lastfm_id(artist: &str, name: &str) -> String {
    format!("lastfm:{}:{}", artist.to_lowercase(), name.to_lowercase())
}

fn time_period_to_lastfm(period: TimePeriod) -> &'static str {
    match period {
        TimePeriod::Week => "7day",
        TimePeriod::Month => "1month",
        TimePeriod::Quarter => "3month",
        TimePeriod::HalfYear => "6month",
        TimePeriod::Year => "12month",
        TimePeriod::AllTime => "overall",
    }
}

fn parse_u64(s: &Option<String>) -> u64 {
    s.as_deref()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(0)
}

fn nonempty_mbid(mbid: Option<String>) -> Option<String> {
    mbid.filter(|s| !s.is_empty())
}

// --- MetadataProvider implementation ---

#[async_trait::async_trait]
impl crate::MetadataProvider for LastFmProvider {
    fn id(&self) -> &'static str {
        "lastfm"
    }

    fn name(&self) -> &'static str {
        "Last.fm"
    }

    async fn search_albums(
        &self,
        artist: Option<&str>,
        query: &str,
        limit: usize,
    ) -> Result<Vec<SearchResult>> {
        let albums = self.search_albums_internal(artist, query, limit).await?;

        Ok(albums
            .into_iter()
            .map(|a| {
                let mbid = nonempty_mbid(a.mbid);
                let id = mbid
                    .clone()
                    .unwrap_or_else(|| generate_lastfm_id(&a.artist, &a.name));
                let cover_url = LastFmImage::get_largest(&a.image);
                SearchResult::Album(Album {
                    id,
                    title: a.name,
                    artist: a.artist,
                    release_date: None,
                    mbid,
                    cover_url,
                })
            })
            .collect())
    }

    async fn search_tracks(
        &self,
        artist: Option<&str>,
        query: &str,
        limit: usize,
    ) -> Result<Vec<SearchResult>> {
        let tracks = self.search_tracks_internal(artist, query, limit).await?;

        Ok(tracks
            .into_iter()
            .map(|t| {
                let mbid = nonempty_mbid(t.mbid);
                let id = mbid
                    .clone()
                    .unwrap_or_else(|| generate_lastfm_id(&t.artist, &t.name));
                SearchResult::Track(Track {
                    id,
                    title: t.name,
                    artist: t.artist,
                    album_id: None,
                    album_title: None,
                    release_date: None,
                    duration: None,
                    mbid,
                    release_mbid: None,
                })
            })
            .collect())
    }

    async fn get_album(&self, id: &str) -> Result<AlbumWithTracks> {
        if let Some(rest) = id.strip_prefix("lastfm:") {
            if let Some((artist, album)) = rest.split_once(':') {
                let info = self.get_album_info(artist, album).await?;

                let album_mbid = nonempty_mbid(info.mbid.clone());
                let cover_url = LastFmImage::get_largest(&info.image);

                let tracks = info
                    .tracks
                    .map(|t| {
                        t.track
                            .into_vec()
                            .into_iter()
                            .map(|track| {
                                let track_mbid = nonempty_mbid(track.mbid);
                                Track {
                                    id: generate_lastfm_id(&info.artist, &track.name),
                                    title: track.name,
                                    artist: track
                                        .artist
                                        .map(|a| a.name)
                                        .unwrap_or_else(|| info.artist.clone()),
                                    album_id: Some(id.to_string()),
                                    album_title: Some(info.name.clone()),
                                    release_date: info
                                        .wiki
                                        .as_ref()
                                        .and_then(|w| w.published.clone()),
                                    duration: format_duration(track.duration),
                                    mbid: track_mbid,
                                    release_mbid: album_mbid.clone(),
                                }
                            })
                            .collect()
                    })
                    .unwrap_or_default();

                return Ok(AlbumWithTracks {
                    album: Album {
                        id: id.to_string(),
                        title: info.name,
                        artist: info.artist,
                        release_date: info.wiki.and_then(|w| w.published),
                        mbid: album_mbid,
                        cover_url,
                    },
                    tracks,
                });
            }
        }

        warn!("Cannot fetch album by MBID from Last.fm: {}", id);
        Err(SoulseekError::Api {
            status: 400,
            message: "Last.fm requires artist:album format, not MBID".to_string(),
        })
    }
}

// --- ScrobbleProvider implementation ---

#[async_trait::async_trait]
impl crate::ScrobbleProvider for LastFmProvider {
    fn id(&self) -> &str {
        "lastfm"
    }

    fn name(&self) -> &str {
        "Last.fm"
    }

    async fn get_listens(&self, count: u32) -> Result<Vec<Listen>> {
        let tracks = self.get_recent_tracks_internal(count).await?;
        Ok(tracks
            .into_iter()
            .filter(|t| {
                // Skip "now playing" entries that have no timestamp
                t.attr.as_ref().and_then(|a| a.nowplaying.as_deref()) != Some("true")
            })
            .filter_map(|t| {
                let timestamp = t.date.as_ref()?.uts.parse::<i64>().ok()?;
                Some(Listen {
                    artist: t.artist.text,
                    track: t.name,
                    album: Some(t.album.text).filter(|s| !s.is_empty()),
                    timestamp,
                })
            })
            .collect())
    }

    async fn get_top_artists(&self, period: TimePeriod, count: u32) -> Result<Vec<RankedArtist>> {
        let artists = self
            .get_top_artists_internal(time_period_to_lastfm(period), count)
            .await?;
        Ok(artists
            .into_iter()
            .map(|a| RankedArtist {
                name: a.name,
                mbid: nonempty_mbid(a.mbid),
                play_count: parse_u64(&a.play_count),
            })
            .collect())
    }

    async fn get_top_tracks(&self, period: TimePeriod, count: u32) -> Result<Vec<RankedTrack>> {
        let tracks = self
            .get_top_tracks_internal(time_period_to_lastfm(period), count)
            .await?;
        Ok(tracks
            .into_iter()
            .map(|t| RankedTrack {
                artist: t.artist.name,
                track: t.name,
                mbid: nonempty_mbid(t.mbid),
                play_count: parse_u64(&t.play_count),
            })
            .collect())
    }

    async fn get_artist_tags(&self, artist: &str) -> Result<Vec<WeightedTag>> {
        let tags = self.get_artist_top_tags_internal(artist).await?;
        if tags.is_empty() {
            return Ok(vec![]);
        }
        let max_count = tags
            .iter()
            .filter_map(|t| t.count)
            .max()
            .unwrap_or(1)
            .max(1) as f64;
        Ok(tags
            .into_iter()
            .map(|t| WeightedTag {
                name: t.name,
                weight: t.count.unwrap_or(0) as f64 / max_count,
            })
            .collect())
    }

    async fn get_artist_popularity(&self, artist: &str) -> Result<ArtistPopularity> {
        let info = self.get_artist_info_internal(artist).await?;
        let stats = info.stats.unwrap_or(LastFmArtistStats {
            listeners: None,
            play_count: None,
        });
        Ok(ArtistPopularity {
            listener_count: parse_u64(&stats.listeners),
            play_count: parse_u64(&stats.play_count),
        })
    }

    async fn get_global_popularity_median(&self) -> Result<u64> {
        let artists = self.get_chart_top_artists_internal(200).await?;
        if artists.is_empty() {
            return Ok(0);
        }
        let mut listener_counts: Vec<u64> =
            artists.iter().map(|a| parse_u64(&a.listeners)).collect();
        listener_counts.sort_unstable();
        let mid = listener_counts.len() / 2;
        Ok(listener_counts[mid])
    }

    async fn get_similar_artists(&self, artist: &str, limit: u32) -> Result<Vec<SimilarArtist>> {
        let artists = self.get_similar_artists_internal(artist, limit).await?;
        Ok(artists
            .into_iter()
            .map(|a| {
                let score = a
                    .match_score
                    .as_deref()
                    .and_then(|s| s.parse::<f64>().ok())
                    .unwrap_or(0.0);
                SimilarArtist {
                    name: a.name,
                    mbid: nonempty_mbid(a.mbid),
                    score,
                }
            })
            .collect())
    }

    async fn get_similar_tracks(
        &self,
        artist: &str,
        track: &str,
        limit: u32,
    ) -> Result<Vec<SimilarTrack>> {
        let tracks = self
            .get_similar_tracks_internal(artist, track, limit)
            .await?;
        Ok(tracks
            .into_iter()
            .map(|t| SimilarTrack {
                artist: t.artist.name,
                track: t.name,
                mbid: nonempty_mbid(t.mbid),
                score: t.match_score.unwrap_or(0.0),
            })
            .collect())
    }

    async fn get_tag_top_tracks(&self, tag: &str, limit: u32) -> Result<Vec<RankedTrack>> {
        let tracks = self.get_tag_top_tracks_internal(tag, limit).await?;
        Ok(tracks
            .into_iter()
            .enumerate()
            .map(|(i, t)| RankedTrack {
                artist: t.artist.name,
                track: t.name,
                mbid: nonempty_mbid(t.mbid),
                // Last.fm tag.getTopTracks doesn't return play counts;
                // use reverse position as a rank proxy.
                play_count: (limit as u64).saturating_sub(i as u64),
            })
            .collect())
    }

    async fn get_related_tags(&self, tag: &str) -> Result<Vec<String>> {
        let tags = self.get_tag_similar_internal(tag).await?;
        Ok(tags.into_iter().map(|t| t.name).collect())
    }

    async fn get_artist_top_tracks(&self, artist: &str, limit: u32) -> Result<Vec<RankedTrack>> {
        let tracks = self.get_artist_top_tracks_internal(artist, limit).await?;
        Ok(tracks
            .into_iter()
            .map(|t| RankedTrack {
                artist: t.artist.name,
                track: t.name,
                mbid: nonempty_mbid(t.mbid),
                play_count: parse_u64(&t.play_count),
            })
            .collect())
    }
}
