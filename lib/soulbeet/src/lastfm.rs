use reqwest::Client;
use serde::Deserialize;
use shared::failure::{cut_chars, Service, UserAction, UserActionCode, RAW_MAX_CHARS};
use shared::metadata::{Album, AlbumWithTracks, Provider, SearchResult, Track};
use shared::recommendation::{
    ArtistPopularity, Listen, RankedArtist, RankedTrack, SimilarArtist, SimilarTrack, TimePeriod,
    WeightedTag,
};
use tracing::info;

use crate::error::{Result, SoulseekError};
use crate::http::SendError;
use crate::MetadataError;

const LASTFM_API_BASE: &str = "https://ws.audioscrobbler.com/2.0/";

/// The `resilient_send` context of every Last.fm request.
const LASTFM_CONTEXT: &str = "Last.fm API";

/// What the user does when Last.fm refused the API key Soulful sent, or no
/// key is saved.
pub fn configure_lastfm() -> UserAction {
    UserAction::new(
        UserActionCode::ConfigureProvider,
        "Set a valid Last.fm API key",
        "Settings > Library",
    )
}

/// What a Last.fm request got instead of the answer it asked for.
#[derive(Debug)]
enum LastFmFailure {
    /// No successful HTTP answer, and none with Last.fm's error body.
    Send(SendError),
    /// Last.fm's own error body, `{"error": code, "message": ...}`.
    Api { code: u16, message: String },
    /// A successful answer that does not parse.
    Parse(String),
}

/// The body Last.fm answers an API error with.
#[derive(Deserialize)]
struct LastFmErrorBody {
    error: u16,
    #[serde(default)]
    message: String,
}

impl From<LastFmErrorBody> for LastFmFailure {
    fn from(body: LastFmErrorBody) -> Self {
        LastFmFailure::Api {
            code: body.error,
            message: body.message,
        }
    }
}

impl From<SendError> for LastFmFailure {
    fn from(e: SendError) -> Self {
        match serde_json::from_str::<LastFmErrorBody>(&e.body) {
            Ok(body) => body.into(),
            Err(_) => LastFmFailure::Send(e),
        }
    }
}

impl From<LastFmFailure> for SoulseekError {
    fn from(f: LastFmFailure) -> Self {
        match f {
            LastFmFailure::Send(e) => e.into(),
            LastFmFailure::Api { code, message } => SoulseekError::Api {
                status: 500,
                message: format!("Last.fm error {code}: {message}"),
            },
            LastFmFailure::Parse(e) => SoulseekError::Api {
                status: 500,
                message: format!("Failed to parse Last.fm response: {e}"),
            },
        }
    }
}

/// Last.fm's error codes, from its API documentation (the `album.getInfo`
/// page lists them): 4 authentication failed, 10 invalid API key, 26
/// suspended API key; 29 rate limit exceeded; 8 operation failed, 11 service
/// offline, 16 temporary error; 6 invalid parameters, which `album.getInfo`
/// answers for an album it does not have. The code is the error's status,
/// as a Subsonic code is for Navidrome, since it is what Last.fm says went
/// wrong; the HTTP status does not decide it.
///
/// What Last.fm answered is cut to RAW_MAX_CHARS where it enters `raw`, as a
/// cause's text is: the error's text holds it, and a proxy's error page can
/// be tens of thousands of characters.
impl From<LastFmFailure> for MetadataError {
    fn from(f: LastFmFailure) -> Self {
        let service = Service::Lastfm;
        let cut = |raw: String| cut_chars(raw, RAW_MAX_CHARS);
        match f {
            LastFmFailure::Api { code, message } => {
                let raw = cut(message);
                let status = Some(code);
                match code {
                    4 | 10 | 26 => MetadataError::NotConfigured {
                        service,
                        status,
                        raw,
                    },
                    29 => MetadataError::RateLimited {
                        service,
                        status,
                        raw,
                    },
                    8 | 11 | 16 => MetadataError::Unavailable {
                        service,
                        status,
                        raw,
                    },
                    6 => MetadataError::NotFound {
                        service,
                        status,
                        raw,
                    },
                    _ => MetadataError::Rejected {
                        service,
                        status,
                        raw,
                    },
                }
            }
            LastFmFailure::Send(SendError { status, body, .. }) => {
                let raw = cut(body);
                match status {
                    Some(429) => MetadataError::RateLimited {
                        service,
                        status,
                        raw,
                    },
                    None | Some(500..=599) => MetadataError::Unavailable {
                        service,
                        status,
                        raw,
                    },
                    Some(_) => MetadataError::Rejected {
                        service,
                        status,
                        raw,
                    },
                }
            }
            LastFmFailure::Parse(raw) => MetadataError::Unreadable {
                service,
                status: None,
                raw: cut(raw),
            },
        }
    }
}

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

    /// Send a GET request to the Last.fm API with retries and deserialize the
    /// response. A body with Last.fm's numeric `error` field is its error,
    /// whatever the HTTP status.
    async fn api_request<T: serde::de::DeserializeOwned>(
        &self,
        params: &[(&str, &str)],
    ) -> std::result::Result<T, LastFmFailure> {
        let mut all_params: Vec<(&str, &str)> = params.to_vec();
        all_params.push(("api_key", &self.api_key));
        all_params.push(("format", "json"));

        let url = reqwest::Url::parse_with_params(LASTFM_API_BASE, &all_params)
            .expect("LASTFM_API_BASE parses, and parameters are only encoded into its query");

        crate::http::lastfm_rate_limit().await;

        let client = self.client.clone();
        let url_str = url.to_string();
        let response = crate::http::resilient_send(|| client.get(&url_str), LASTFM_CONTEXT).await?;
        // The connection failed while the body was read.
        let body = response
            .bytes()
            .await
            .map_err(|e| LastFmFailure::Send(SendError::unanswered(LASTFM_CONTEXT, e)))?;

        if let Ok(error) = serde_json::from_slice::<LastFmErrorBody>(&body) {
            return Err(error.into());
        }
        serde_json::from_slice(&body).map_err(|e| LastFmFailure::Parse(e.to_string()))
    }

    // --- Metadata search internals (used by MetadataProvider) ---

    async fn search_albums_internal(
        &self,
        artist: Option<&str>,
        query: &str,
        limit: usize,
    ) -> std::result::Result<Vec<LastFmAlbum>, LastFmFailure> {
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
    ) -> std::result::Result<Vec<LastFmTrack>, LastFmFailure> {
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

    async fn get_album_info(
        &self,
        id: &str,
        (artist, album): (&str, &str),
    ) -> std::result::Result<LastFmAlbumInfo, LastFmFailure> {
        info!("Fetching album info from Last.fm: {}", id);
        let params = [
            ("method", "album.getInfo"),
            ("artist", artist),
            ("album", album),
        ];
        let data: AlbumInfoResponse = self.api_request(&params).await?;
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

/// The artist and album a `lastfm:{artist}:{album}` id from `search_albums`
/// names. A search gives an album Last.fm has an MBID for that MBID as its
/// id instead: that id is MusicBrainz's, and MusicBrainz looks it up.
fn album_name(id: &str) -> Option<(&str, &str)> {
    id.strip_prefix(ID_PREFIX)?.split_once(':')
}

/// Whether `id` is an album id of Last.fm's own, which only Last.fm can look
/// up.
pub fn is_album_id(id: &str) -> bool {
    album_name(id).is_some()
}

/// What starts the ids Last.fm results get when Last.fm has no MBID for them.
const ID_PREFIX: &str = "lastfm:";

fn generate_lastfm_id(artist: &str, name: &str) -> String {
    format!(
        "{ID_PREFIX}{}:{}",
        artist.to_lowercase(),
        name.to_lowercase()
    )
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
    fn id(&self) -> Provider {
        Provider::LastFm
    }

    fn name(&self) -> &'static str {
        "Last.fm"
    }

    async fn search_albums(
        &self,
        artist: Option<&str>,
        query: &str,
        limit: usize,
    ) -> std::result::Result<Vec<SearchResult>, MetadataError> {
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
                    release_types: None,
                })
            })
            .collect())
    }

    async fn search_tracks(
        &self,
        artist: Option<&str>,
        query: &str,
        limit: usize,
    ) -> std::result::Result<Vec<SearchResult>, MetadataError> {
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

    async fn get_album(&self, id: &str) -> std::result::Result<AlbumWithTracks, MetadataError> {
        let name = album_name(id).ok_or_else(|| MetadataError::Rejected {
            service: Service::Lastfm,
            status: None,
            raw: format!("{id} is not a Last.fm album id"),
        })?;
        let info = self.get_album_info(id, name).await?;

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
                            release_date: info.wiki.as_ref().and_then(|w| w.published.clone()),
                            duration: format_duration(track.duration),
                            mbid: track_mbid,
                            release_mbid: album_mbid.clone(),
                        }
                    })
                    .collect()
            })
            .unwrap_or_default();

        Ok(AlbumWithTracks {
            album: Album {
                id: id.to_string(),
                title: info.name,
                artist: info.artist,
                release_date: info.wiki.and_then(|w| w.published),
                mbid: album_mbid,
                cover_url,
                release_types: None,
            },
            tracks,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::MetadataProvider;
    use shared::failure::{FailureCode, Stage};

    #[test]
    fn a_long_answer_is_cut_where_it_enters_the_error() {
        // A proxy's 503 page in front of Last.fm, far longer than a cause keeps.
        let page = format!(
            "<!DOCTYPE html><html><body>{}</body></html>",
            "<p>Service Unavailable</p>".repeat(3_000)
        );
        let e = MetadataError::from(LastFmFailure::Send(SendError {
            context: LASTFM_CONTEXT.into(),
            status: Some(503),
            body: page.clone(),
        }));
        let MetadataError::Unavailable { raw, .. } = &e else {
            panic!("{e:?}");
        };
        assert_eq!(raw, &cut_chars(page, RAW_MAX_CHARS));
        assert!(e.to_string().chars().count() <= RAW_MAX_CHARS + "Last.fm is unavailable: ".len());
        let f = e.failure(Stage::Metadata, e.to_string());
        assert_eq!(&f.cause.unwrap().raw, raw);
    }

    #[test]
    fn only_lastfms_own_album_ids_name_an_album() {
        assert_eq!(
            album_name("lastfm:marrow choir:winter songs"),
            Some(("marrow choir", "winter songs"))
        );
        assert!(is_album_id(&generate_lastfm_id(
            "Marrow Choir",
            "Winter Songs"
        )));
        assert!(!is_album_id("8c1e2b5d-0f3a-4e6b-9d7c-2a1b3c4d5e6f"));
    }

    #[tokio::test]
    async fn an_mbid_is_not_looked_up_on_lastfm() {
        let e = LastFmProvider::new("key")
            .get_album("8c1e2b5d-0f3a-4e6b-9d7c-2a1b3c4d5e6f")
            .await
            .unwrap_err();
        assert!(
            matches!(
                e,
                MetadataError::Rejected {
                    service: Service::Lastfm,
                    ..
                }
            ),
            "{e:?}"
        );
    }

    fn kind(e: MetadataError) -> &'static str {
        match e {
            MetadataError::NotFound { .. } => "not_found",
            MetadataError::RateLimited { .. } => "rate_limited",
            MetadataError::Unavailable { .. } => "unavailable",
            MetadataError::NotConfigured { .. } => "not_configured",
            MetadataError::Rejected { .. } => "rejected",
            MetadataError::Unreadable { .. } => "unreadable",
        }
    }

    #[test]
    fn lastfm_errors_come_from_its_error_code() {
        let api = |code: u16, message: &str| LastFmFailure::Api {
            code,
            message: message.into(),
        };
        let by_code: Vec<&str> = [
            api(6, "Album not found"),
            api(
                10,
                "Invalid API key - You must be granted a valid key by last.fm",
            ),
            api(29, "Rate Limit Exceeded"),
            api(11, "Service Offline"),
            api(13, "Invalid method signature supplied"),
        ]
        .into_iter()
        .map(|f| kind(MetadataError::from(f)))
        .collect();
        assert_eq!(
            by_code,
            [
                "not_found",
                "not_configured",
                "rate_limited",
                "unavailable",
                "rejected"
            ]
        );

        let send = |status: Option<u16>| SendError {
            context: "Last.fm API".into(),
            status,
            body: "b".into(),
        };
        let by_status: Vec<&str> = [None, Some(503), Some(429), Some(400)]
            .into_iter()
            .map(|s| kind(MetadataError::from(LastFmFailure::Send(send(s)))))
            .collect();
        assert_eq!(
            by_status,
            ["unavailable", "unavailable", "rate_limited", "rejected"]
        );

        let refused_key = LastFmFailure::from(SendError {
            body: r#"{"message":"Invalid API key - You must be granted a valid key by last.fm","error":10}"#.into(),
            ..send(Some(403))
        });
        assert!(
            matches!(refused_key, LastFmFailure::Api { code: 10, .. }),
            "{refused_key:?}"
        );

        // The failure names the code that chose it, and keeps Last.fm's text.
        let unmapped = MetadataError::from(api(13, "Invalid method signature supplied"))
            .failure(Stage::Metadata, "m");
        let cause = unmapped.cause.unwrap();
        assert_eq!(
            (
                unmapped.code,
                cause.service,
                cause.status,
                cause.raw.as_str()
            ),
            (
                FailureCode::InvalidInput,
                Service::Lastfm,
                Some(13),
                "Invalid method signature supplied"
            )
        );
        let key = MetadataError::from(refused_key).failure(Stage::Metadata, "m");
        let cause = key.cause.unwrap();
        assert_eq!(
            (cause.status, cause.raw.as_str()),
            (
                Some(10),
                "Invalid API key - You must be granted a valid key by last.fm"
            )
        );
    }
}
