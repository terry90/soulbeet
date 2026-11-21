use musicbrainz_rs::{
    entity::{
        artist::{Artist, ArtistType},
        artist_credit::ArtistCredit,
        recording::Recording,
        release::{Release, ReleaseStatus},
        release_group::{ReleaseGroup, ReleaseGroupPrimaryType, ReleaseGroupSecondaryType},
        BrowseResult,
    },
    Browse, Fetch, MusicBrainzClient, Search,
};
use serde::Serialize;
use shared::failure::{cut_chars, Service, RAW_MAX_CHARS};
use shared::metadata::{Album, AlbumWithTracks, SearchResult, Track};
use shared::recommendation::{fold_name, without_marks};
use std::{collections::HashSet, future::Future, sync::OnceLock, time::Duration};
use tokio::time::sleep;
use tracing::{info, warn};

use crate::MetadataError;

/// Timeout for individual MusicBrainz requests (15 seconds)
const REQUEST_TIMEOUT_SECS: u64 = 15;

/// Maximum retries for transient errors
const MAX_RETRIES: u32 = 3;

/// Base delay for exponential backoff (milliseconds)
const BASE_DELAY_MS: u64 = 500;

/// Maximum backoff delay cap (milliseconds)
const MAX_BACKOFF_MS: u64 = 5000;

// This ensures the client is initialized only once with a proper user agent.
fn musicbrainz_client() -> &'static MusicBrainzClient {
    static CLIENT: OnceLock<MusicBrainzClient> = OnceLock::new();
    CLIENT.get_or_init(|| {
        let version = env!("CARGO_PKG_VERSION");
        let mut client = MusicBrainzClient::new(&format!(
            "Soulbeet/{version} ( https://github.com/terry90/soulbeet )"
        ))
        .expect("Failed to create MusicBrainz client - invalid user agent format");
        // MUSICBRAINZ_HOST points the client at a MusicBrainz mirror
        // (host[:port], no scheme). Mirrors the `musicbrainz.host` option
        // beets exposes for the same purpose.
        if let Ok(host) = std::env::var("MUSICBRAINZ_HOST") {
            let host = host.trim();
            if !host.is_empty() {
                client.musicbrainz_domain = host.to_string();
            }
        }
        client
    })
}

/// The artist credits as one comma-separated text; None when MusicBrainz
/// sent no artist credit.
fn format_artist_credit(credits: &Option<Vec<ArtistCredit>>) -> Option<String> {
    credits.as_ref().map(|credits| {
        credits
            .iter()
            .map(|credit| credit.name.clone())
            .collect::<Vec<String>>()
            .join(", ")
    })
}

/// What the web UI shows as the artist of a track or album MusicBrainz
/// sent no artist credit for.
const NO_CREDIT: &str = "Unknown Artist";

/// `format_artist_credit` for the web UI's tracks and albums, NO_CREDIT for
/// none.
fn credited_artist(credits: &Option<Vec<ArtistCredit>>) -> String {
    format_artist_credit(credits).unwrap_or_else(|| NO_CREDIT.to_string())
}

/// Formats a duration from milliseconds to a MM:SS string.
fn format_duration(duration_ms: &Option<u32>) -> Option<String> {
    duration_ms.map(|ms| {
        let total_seconds = ms / 1000;
        let minutes = total_seconds / 60;
        let seconds = total_seconds % 60;
        format!("{minutes:02}:{seconds:02}")
    })
}

/// What MusicBrainz answered is cut to RAW_MAX_CHARS where it enters `raw`,
/// as a cause's text is: the error's text holds it, and serde's text quotes
/// the value it could not read in full.
impl From<musicbrainz_rs::Error> for MetadataError {
    fn from(e: musicbrainz_rs::Error) -> Self {
        use musicbrainz_rs::Error;
        let service = Service::Musicbrainz;
        let cut = |raw: String| cut_chars(raw, RAW_MAX_CHARS);
        match e {
            Error::NotFound(_) => MetadataError::NotFound {
                service,
                status: None,
                raw: cut(e.to_string()),
            },
            // MusicBrainz's own `{error, help}` body, other than "Not Found"
            // (musicbrainz_rs `src/entity/api.rs:8-24`).
            Error::MusicbrainzError(query, body) => MetadataError::Rejected {
                service,
                status: None,
                raw: cut(format!(
                    "{query}: {}",
                    serde_json::to_string(&body).expect("two strings serialize")
                )),
            },
            // musicbrainz_rs retries MusicBrainz's 503 rate-limit answer
            // (`src/config.rs:9`, `src/fetching.rs:33-59`) until it gives up.
            Error::MaxRetriesExceeded => MetadataError::RateLimited {
                service,
                status: None,
                raw: e.to_string(),
            },
            Error::ReqwestError(e) => MetadataError::Unavailable {
                service,
                status: e.status().map(|s| s.as_u16()),
                raw: cut(e.to_string()),
            },
            Error::SerdeJsonError(e) => MetadataError::Unreadable {
                service,
                status: None,
                raw: cut(e.to_string()),
            },
            Error::MissingRetryToken | Error::InvalidUserAgent(_) => unreachable!(
                "musicbrainz_rs 44c25c8 never constructs MissingRetryToken, and gives \
                 InvalidUserAgent only from MusicBrainzClient::new, which Soulbeet calls \
                 once under expect: {e}"
            ),
        }
    }
}

/// Retries an async operation with exponential backoff and request timeout.
/// Only retries transient errors (`MetadataError::is_transient`): no answer
/// within REQUEST_TIMEOUT_SECS, or an HTTP exchange that failed.
async fn with_retry<T, F, Fut>(operation_name: &str, mut operation: F) -> Result<T, MetadataError>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, musicbrainz_rs::Error>>,
{
    let mut attempt = 0;
    loop {
        // Respect MusicBrainz rate limit (1 req/sec)
        crate::http::mb_rate_limit().await;

        let result =
            tokio::time::timeout(Duration::from_secs(REQUEST_TIMEOUT_SECS), operation()).await;
        let e = match result {
            Ok(Ok(value)) => return Ok(value),
            Ok(Err(e)) => MetadataError::from(e),
            Err(_timeout) => MetadataError::Unavailable {
                service: Service::Musicbrainz,
                status: None,
                raw: format!("no answer within {REQUEST_TIMEOUT_SECS} s"),
            },
        };
        attempt += 1;
        if !e.is_transient() || attempt == MAX_RETRIES {
            warn!(
                "{} failed (attempt {}/{}): {}",
                operation_name, attempt, MAX_RETRIES, e
            );
            return Err(e);
        }
        let delay = std::cmp::min(BASE_DELAY_MS * 2u64.pow(attempt - 1), MAX_BACKOFF_MS);
        warn!(
            "{} failed (attempt {}/{}), retrying in {}ms: {}",
            operation_name, attempt, MAX_RETRIES, delay, e
        );
        sleep(Duration::from_millis(delay)).await;
    }
}

/// `field:"value"`. Inside a quoted phrase only `"` and `\` mean anything to
/// Lucene, so they are escaped.
fn phrase(field: &str, value: &str) -> String {
    let value = value.replace('\\', "\\\\").replace('"', "\\\"");
    format!("{field}:\"{value}\"")
}

/// The `query=` parameter of a MusicBrainz search for the Lucene query
/// `lucene`, form-encoded so `&`, `#`, `+` and `%` reach MusicBrainz as text.
/// lucene_query_builder 0.3.0 quotes only a value holding a space and escapes
/// nothing (`QueryString`'s Display), and musicbrainz_rs appends the
/// parameter to the URL as it is (src/api/search_query.rs:114-127).
fn query_param(lucene: &str) -> String {
    format!(
        "query={}",
        url::form_urlencoded::byte_serialize(lucene.as_bytes()).collect::<String>()
    )
}

/// The artist search for `name`. Only the artist index holds aliases, such
/// as "Shirō Sagisu" for 鷺巣詩郎, and its `artist` field is the name alone.
fn artist_search_query(name: &str) -> String {
    query_param(&format!(
        "{} OR {}",
        phrase("artist", name),
        phrase("alias", name)
    ))
}

/// A track or album search for `title` in `title_field`. With an artist
/// filter, the artist's `name_field` holds the name as typed, or the artist
/// is one of `found`, the artists the artist search found for that name.
fn titled_query(
    title_field: &str,
    title: &str,
    name_field: &str,
    artist: Option<(&str, &[ArtistMatch])>,
) -> String {
    let title = phrase(title_field, title);
    let lucene = match artist {
        None => title,
        Some((name, found)) => {
            let by: Vec<String> = std::iter::once(phrase(name_field, name))
                .chain(found.iter().map(|a| phrase("arid", &a.mbid)))
                .collect();
            format!("({}) AND {title}", by.join(" OR "))
        }
    };
    query_param(&lucene)
}

/// `s` folded as `fold_name` folds it, without combining marks: the artist
/// index ignores them, so "Shiro Sagisu" finds the alias "Shirō Sagisu" and
/// "フィッシュマンス" finds "フィッシュマンズ".
fn search_fold(s: &str) -> String {
    fold_name(&without_marks(s))
}

/// Whether `wanted`, folded by `search_fold`, is the name or an alias of `m`.
fn carries_name(m: &ArtistMatch, wanted: &str) -> bool {
    search_fold(&m.name) == wanted || m.aliases.iter().any(|a| search_fold(a) == wanted)
}

/// The artists of `found` that carry `name` whole. The artist search matches
/// the name anywhere in a name or alias, so "Queen" also finds Ella
/// Fitzgerald by her alias "Queen of Jazz".
fn named_artists(name: &str, found: Vec<ArtistMatch>) -> Vec<ArtistMatch> {
    let wanted = search_fold(name);
    found
        .into_iter()
        .filter(|m| carries_name(m, &wanted))
        .collect()
}

/// Artists the artist search returns for an artist filter, of which those
/// that carry the typed name are matched by MBID: MusicBrainz's default page,
/// which keeps their MBIDs within a short query URL.
const FILTER_ARTISTS: u8 = 25;

/// An enumeration to specify the type of search.
#[derive(Debug)]
pub enum SearchType {
    Track,
    Album,
}

/// Performs a refined search for music, prioritizing canonical releases.
pub async fn search(
    artist: &Option<String>,
    query: &str,
    search_type: SearchType,
    limit: u8,
) -> Result<Vec<SearchResult>, MetadataError> {
    let client = musicbrainz_client();
    let mut results = Vec::new();

    info!(
        "Starting {:?} search for query: '{}', artist: '{:?}'",
        search_type, query, artist
    );

    // The recording and release group indexes hold artist names but no
    // aliases, and one search cannot reach into another index, so an artist
    // typed by an alias ("Shiro Sagisu" for 鷺巣詩郎) is found in the artist
    // index first. Only the artists that carry the typed name are matched by
    // MBID; the name clause still finds every result it found before.
    let found = match artist {
        Some(name) => named_artists(name, search_artists(name, FILTER_ARTISTS).await?),
        None => Vec::new(),
    };
    let by_artist = artist.as_deref().map(|name| (name, found.as_slice()));

    match search_type {
        SearchType::Track => {
            let search_results = with_retry("MusicBrainz track search", || {
                let search_query = titled_query("recording", query, "artistname", by_artist);
                async move {
                    Recording::search(search_query)
                        .limit(limit)
                        .with_releases()
                        .with_ratings()
                        .execute_with_client(client)
                        .await
                }
            })
            .await?;

            // Sort by rating (descending) - higher rated recordings first
            let mut recordings: Vec<_> = search_results.entities;
            recordings.sort_by(|a, b| {
                let a_rating = a.rating.as_ref().and_then(|r| r.value).unwrap_or(0.0);
                let b_rating = b.rating.as_ref().and_then(|r| r.value).unwrap_or(0.0);
                b_rating
                    .partial_cmp(&a_rating)
                    .unwrap_or(std::cmp::Ordering::Equal)
            });

            let mut unique_tracks = HashSet::new();

            for recording in recordings {
                let artist_name = credited_artist(&recording.artist_credit);
                let album_title = recording
                    .releases
                    .as_ref()
                    .and_then(|r| r.first())
                    .map(|r| r.title.clone())
                    .unwrap_or_default();

                // Use a combination of title, artist, and album to define a unique track
                let key = (
                    recording.title.to_lowercase(),
                    artist_name.to_lowercase(),
                    album_title.to_lowercase(),
                );

                if !unique_tracks.contains(&key) {
                    let first_release = recording.releases.as_ref().and_then(|r| r.first());
                    let track = Track {
                        id: recording.id.clone(),
                        title: recording.title.clone(),
                        artist: artist_name.clone(),
                        album_id: first_release.map(|release| release.id.clone()),
                        album_title: first_release.map(|r| r.title.clone()),
                        release_date: first_release.and_then(|r| r.date.clone().map(|d| d.0)),
                        duration: format_duration(&recording.length),
                        mbid: Some(recording.id.clone()),
                        release_mbid: first_release.map(|r| r.id.clone()),
                    };
                    unique_tracks.insert(key);
                    results.push(SearchResult::Track(track));
                }
            }
        }
        SearchType::Album => {
            let search_results = with_retry("MusicBrainz album search", || {
                let search_query = titled_query("releasegroup", query, "artist", by_artist);
                async move {
                    ReleaseGroup::search(search_query)
                        .limit(limit)
                        .with_releases()
                        .with_ratings()
                        .execute_with_client(client)
                        .await
                }
            })
            .await?;

            // Sort by rating (descending) - higher rated albums first
            let mut release_groups: Vec<_> = search_results.entities;
            release_groups.sort_by(|a, b| {
                let a_rating = a.rating.as_ref().and_then(|r| r.value).unwrap_or(0.0);
                let b_rating = b.rating.as_ref().and_then(|r| r.value).unwrap_or(0.0);
                b_rating
                    .partial_cmp(&a_rating)
                    .unwrap_or(std::cmp::Ordering::Equal)
            });

            for release_group in release_groups {
                if release_group.primary_type != Some(ReleaseGroupPrimaryType::Album)
                    && release_group.primary_type != Some(ReleaseGroupPrimaryType::Ep)
                {
                    continue;
                }

                if let Some(final_release) = album_release(&release_group) {
                    results.push(SearchResult::Album(Album {
                        id: final_release.id.clone(),
                        title: release_group.title.clone(),
                        artist: credited_artist(&release_group.artist_credit),
                        release_date: final_release.date.as_ref().map(|d| d.0.clone()),
                        mbid: Some(final_release.id.clone()),
                        cover_url: None,
                        release_types: release_types(&release_group),
                    }));
                }
            }
        }
    }

    Ok(results)
}

/// The release an album search shows for `group`: an official one titled as
/// the group, else the first official one. A release titled otherwise is often
/// another edition of the album, such as its extended mixes alone. A release
/// group search sends each release's id, title and status but no date or
/// tracklist, so the order MusicBrainz lists them in decides among the rest.
fn album_release(group: &ReleaseGroup) -> Option<&Release> {
    let official: Vec<&Release> = (group.releases.iter().flatten())
        .filter(|r| r.status == Some(ReleaseStatus::Official))
        .collect();
    let group_title = group.title.to_lowercase();
    (official.iter())
        .find(|r| r.title.to_lowercase() == group_title)
        .or(official.first())
        .copied()
}

/// Fetches a release (album) by its ID and returns it with its full tracklist.
pub async fn find_album(release_id: &str) -> Result<AlbumWithTracks, MetadataError> {
    let client = musicbrainz_client();

    // Fetch the release with recordings (tracks), artist credits for the
    // tracks and its release group for the release types.
    let release = with_retry("MusicBrainz album fetch", || async {
        Release::fetch()
            .id(release_id)
            .with_recordings()
            .with_artist_credits()
            .with_release_groups()
            .execute_with_client(client)
            .await
    })
    .await?;

    Ok(album_from_release(release))
}

/// `find_album`'s answer from the release it fetched.
fn album_from_release(release: Release) -> AlbumWithTracks {
    let mut tracks = Vec::new();

    // A release contains media (like CD 1, CD 2), and each medium has tracks.
    if let Some(media) = &release.media {
        for medium in media {
            if let Some(release_tracks) = &medium.tracks {
                for track in release_tracks {
                    if let Some(recording) = &track.recording {
                        tracks.push(Track {
                            id: recording.id.clone(),
                            title: recording.title.clone(),
                            artist: credited_artist(&recording.artist_credit),
                            album_id: Some(release.id.clone()),
                            album_title: Some(release.title.clone()),
                            release_date: release.date.as_ref().map(|d| d.0.clone()),
                            duration: format_duration(&recording.length),
                            mbid: Some(recording.id.clone()),
                            release_mbid: Some(release.id.clone()),
                        });
                    }
                }
            }
        }
    }

    // First, create the standalone Album object.
    let album = Album {
        id: release.id.clone(),
        title: release.title,
        artist: credited_artist(&release.artist_credit),
        release_date: release.date.map(|d| d.0),
        mbid: Some(release.id),
        cover_url: None,
        release_types: release.release_group.as_ref().and_then(release_types),
    };

    AlbumWithTracks { album, tracks }
}

/// An artist a MusicBrainz artist search found.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ArtistMatch {
    pub mbid: String,
    pub name: String,
    pub sort_name: String,
    pub disambiguation: Option<String>,
    /// MusicBrainz's name for the artist type; None when it gives none or
    /// one musicbrainz_rs does not know.
    pub artist_type: Option<String>,
    pub country: Option<String>,
    pub begin: Option<String>,
    pub end: Option<String>,
    /// Other names MusicBrainz knows the artist by, such as a romanised one.
    /// Kept out of the tool answers, which show the artist as before.
    #[serde(skip)]
    pub aliases: Vec<String>,
}

/// The recording a recording's releases were looked up for.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RecordingInfo {
    pub mbid: String,
    pub title: String,
    /// The artist credit; None when MusicBrainz sent none.
    pub artist: Option<String>,
    pub duration_s: Option<u32>,
    pub disambiguation: Option<String>,
    pub first_release_date: Option<String>,
}

/// The release group a release belongs to.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ReleaseGroupInfo {
    pub mbid: String,
    pub title: String,
    /// As `release_types` gives them.
    pub release_types: Option<Vec<String>>,
    pub first_release_date: Option<String>,
}

/// A release a recording is on.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RecordingRelease {
    pub mbid: String,
    pub title: String,
    pub date: Option<String>,
    pub country: Option<String>,
    /// MusicBrainz's name for the release status; None when it gives none or
    /// one musicbrainz_rs does not know.
    pub status: Option<String>,
    pub release_group: Option<ReleaseGroupInfo>,
}

/// A recording and one page of the releases it is on.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RecordingReleases {
    pub recording: RecordingInfo,
    /// Every release the recording is on, not only this page's.
    pub release_count: u32,
    pub releases: Vec<RecordingRelease>,
}

/// Searches MusicBrainz for artists by name or alias.
pub async fn search_artists(query: &str, limit: u8) -> Result<Vec<ArtistMatch>, MetadataError> {
    let client = musicbrainz_client();
    let found = with_retry("MusicBrainz artist search", || {
        let search_query = artist_search_query(query);
        async move {
            Artist::search(search_query)
                .limit(limit)
                .execute_with_client(client)
                .await
        }
    })
    .await?;
    Ok(found.entities.into_iter().map(artist_match).collect())
}

/// The MBID of the one artist whose name or alias equals `name` once folded;
/// None for no such artist or for several distinct ones, so a near name like
/// "Fishmans+" never stands in for "Fishmans".
pub fn exact_artist(name: &str, found: &[ArtistMatch]) -> Option<String> {
    let wanted = fold_name(name);
    let mut hits = found
        .iter()
        .filter(|m| {
            fold_name(&m.name) == wanted || m.aliases.iter().any(|a| fold_name(a) == wanted)
        })
        .map(|m| m.mbid.as_str());
    let first = hits.next()?;
    hits.all(|mbid| mbid == first).then(|| first.to_string())
}

/// The MusicBrainz web service root: `MUSICBRAINZ_HOST` is reached over
/// http, as musicbrainz_rs reaches it.
fn web_base(host: Option<String>) -> String {
    match host.as_deref().map(str::trim).filter(|h| !h.is_empty()) {
        Some(host) => format!("http://{host}"),
        None => "https://musicbrainz.org".to_string(),
    }
}

fn genre_list(text: &str) -> Vec<String> {
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(String::from)
        .collect()
}

/// Every genre name MusicBrainz knows, one request.
pub async fn genre_names() -> Result<Vec<String>, MetadataError> {
    genre_names_at(&web_base(std::env::var("MUSICBRAINZ_HOST").ok())).await
}

async fn genre_names_at(base: &str) -> Result<Vec<String>, MetadataError> {
    let service = Service::Musicbrainz;
    let url = format!("{base}/ws/2/genre/all?fmt=txt");
    crate::http::mb_rate_limit().await;
    let unavailable = |raw: String| MetadataError::Unavailable {
        service,
        status: None,
        raw: cut_chars(raw, RAW_MAX_CHARS),
    };
    let client = crate::http::build_client(crate::polite::USER_AGENT);
    let response = client
        .get(&url)
        .send()
        .await
        .map_err(|e| unavailable(e.without_url().to_string()))?;
    let status = response.status();
    let body = response
        .text()
        .await
        .map_err(|e| unavailable(e.without_url().to_string()))?;
    if !status.is_success() {
        return Err(MetadataError::Rejected {
            service,
            status: Some(status.as_u16()),
            raw: cut_chars(body, RAW_MAX_CHARS),
        });
    }
    // An empty answer or a proxy's HTML page is no genre list. As an error it
    // is not cached for the 30 days a list is.
    let unreadable = |raw: String| MetadataError::Unreadable {
        service,
        status: Some(status.as_u16()),
        raw: cut_chars(raw, RAW_MAX_CHARS),
    };
    if body.trim().is_empty() {
        return Err(unreadable("MusicBrainz answered no genres".into()));
    }
    if body.trim_start().starts_with('<') {
        return Err(unreadable(body));
    }
    Ok(genre_list(&body))
}

/// The recording `mbid` and the page of releases it is on that starts at
/// `offset`. MusicBrainz answers at most 100 releases per page
/// (musicbrainz.org/doc/MusicBrainz_API "Browse").
pub async fn recording_releases(
    mbid: &str,
    offset: u16,
    limit: u8,
) -> Result<RecordingReleases, MetadataError> {
    let client = musicbrainz_client();
    let recording = with_retry("MusicBrainz recording fetch", || async {
        Recording::fetch()
            .id(mbid)
            .with_artists()
            .execute_with_client(client)
            .await
    })
    .await?;
    let page = with_retry("MusicBrainz recording releases", || async {
        Release::browse()
            .by_recording(mbid)
            .with_release_groups()
            .limit(limit)
            .offset(offset)
            .execute_with_client(client)
            .await
    })
    .await?;
    recording_releases_from(recording, page)
}

fn artist_match(a: Artist) -> ArtistMatch {
    let life_span = a.life_span.unwrap_or_default();
    ArtistMatch {
        mbid: a.id,
        name: a.name,
        sort_name: a.sort_name,
        disambiguation: given(Some(a.disambiguation)),
        artist_type: a
            .artist_type
            .filter(|t| *t != ArtistType::UnrecognizedArtistType)
            .map(|t| serde_name(&t)),
        country: given(a.country),
        begin: given(life_span.begin.map(|d| d.0)),
        end: given(life_span.end.map(|d| d.0)),
        aliases: a
            .aliases
            .unwrap_or_default()
            .into_iter()
            .map(|alias| alias.name)
            .collect(),
    }
}

fn recording_releases_from(
    recording: Recording,
    page: BrowseResult<Release>,
) -> Result<RecordingReleases, MetadataError> {
    let Ok(release_count) = u32::try_from(page.count) else {
        return Err(MetadataError::Unreadable {
            service: Service::Musicbrainz,
            status: None,
            raw: "MusicBrainz answered a negative release count".into(),
        });
    };
    Ok(RecordingReleases {
        recording: RecordingInfo {
            artist: format_artist_credit(&recording.artist_credit),
            mbid: recording.id,
            title: recording.title,
            duration_s: recording.length.map(|ms| ms / 1000).filter(|&s| s > 0),
            disambiguation: given(recording.disambiguation),
            first_release_date: given(recording.first_release_date.map(|d| d.0)),
        },
        release_count,
        releases: page
            .entities
            .into_iter()
            .map(|r| RecordingRelease {
                mbid: r.id,
                title: r.title,
                date: given(r.date.map(|d| d.0)),
                country: given(r.country),
                status: r
                    .status
                    .filter(|s| *s != ReleaseStatus::UnrecognizedReleaseStatus)
                    .map(|s| serde_name(&s)),
                release_group: r.release_group.map(|g| ReleaseGroupInfo {
                    release_types: release_types(&g),
                    mbid: g.id,
                    title: g.title,
                    first_release_date: given(g.first_release_date.map(|d| d.0)),
                }),
            })
            .collect(),
    })
}

/// The release group's primary type (when it has one), then its secondary
/// types, by MusicBrainz's names for them. None when musicbrainz_rs does not
/// know one of them: it reads such a type as an `Unrecognized*` variant,
/// which does not carry MusicBrainz's name. None also when the group has no
/// type at all: a release group "can have" a primary type
/// (musicbrainz.org/doc/Release_Group/Type, "Description"), and one that
/// fits no other is given "Other" ("Any release that does not fit or can't
/// decisively be placed in any of the categories above"), so a group with
/// none is one whose type no editor set, not one known to have none.
/// musicbrainz_rs reads a missing or null "primary-type" as None and a
/// missing "secondary-types" as empty (`ReleaseGroup` is
/// `#[serde(default)]`, src/entity/release_group.rs:18-37).
fn release_types(group: &ReleaseGroup) -> Option<Vec<String>> {
    if group.primary_type == Some(ReleaseGroupPrimaryType::UnrecognizedReleaseGroupPrimaryType)
        || group
            .secondary_types
            .contains(&ReleaseGroupSecondaryType::UnrecognizedReleaseGroupSecondaryType)
        || (group.primary_type.is_none() && group.secondary_types.is_empty())
    {
        return None;
    }
    let primary = group.primary_type.iter().map(serde_name);
    Some(
        primary
            .chain(group.secondary_types.iter().map(serde_name))
            .collect(),
    )
}

/// The name serde gives a musicbrainz_rs type variant, which is MusicBrainz's
/// name for every variant other than the `Unrecognized*` ones.
fn serde_name(value: &impl Serialize) -> String {
    match serde_json::to_value(value) {
        Ok(serde_json::Value::String(name)) => name,
        other => unreachable!("musicbrainz_rs type variants serialize to their name: {other:?}"),
    }
}

/// None for a text MusicBrainz leaves empty.
fn given(text: Option<String>) -> Option<String> {
    text.filter(|t| !t.is_empty())
}

pub struct MusicBrainzProvider;

impl MusicBrainzProvider {
    pub fn new() -> Self {
        Self
    }
}

impl Default for MusicBrainzProvider {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl crate::traits::MusicBrainzLookup for MusicBrainzProvider {
    async fn search_artists(
        &self,
        name: &str,
        limit: u8,
    ) -> Result<Vec<ArtistMatch>, MetadataError> {
        search_artists(name, limit).await
    }

    async fn genre_names(&self) -> Result<Vec<String>, MetadataError> {
        genre_names().await
    }
}

#[async_trait::async_trait]
impl crate::MetadataProvider for MusicBrainzProvider {
    async fn search_albums(
        &self,
        artist: Option<&str>,
        query: &str,
        limit: usize,
    ) -> Result<Vec<SearchResult>, MetadataError> {
        let artist_opt = artist.map(String::from);
        search(&artist_opt, query, SearchType::Album, limit.min(100) as u8).await
    }

    async fn search_tracks(
        &self,
        artist: Option<&str>,
        query: &str,
        limit: usize,
    ) -> Result<Vec<SearchResult>, MetadataError> {
        let artist_opt = artist.map(String::from);
        search(&artist_opt, query, SearchType::Track, limit.min(100) as u8).await
    }

    async fn get_album(&self, id: &str) -> Result<AlbumWithTracks, MetadataError> {
        find_album(id).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn found(mbid: &str, name: &str, aliases: &[&str]) -> ArtistMatch {
        ArtistMatch {
            mbid: mbid.into(),
            name: name.into(),
            sort_name: name.into(),
            disambiguation: None,
            artist_type: None,
            country: None,
            begin: None,
            end: None,
            aliases: aliases.iter().map(|a| a.to_string()).collect(),
        }
    }

    #[test]
    fn an_artist_resolves_only_on_one_exact_name_or_alias() {
        let fishmans = [
            found("dc436953-1dc3-437d-9656-da401a0e9134", "Fishmans+", &[]),
            found(
                "d0f25f16-7782-42e8-8047-c16a1cb2b450",
                "フィッシュマンズ",
                &["Fishmans"],
            ),
        ];
        assert_eq!(
            exact_artist("Fishmans", &fishmans).as_deref(),
            Some("d0f25f16-7782-42e8-8047-c16a1cb2b450")
        );
        let sleep = [
            found("aaaaaaaa-0000-4000-8000-000000000001", "Sleep", &[]),
            found("aaaaaaaa-0000-4000-8000-000000000002", "Sleep", &[]),
        ];
        assert_eq!(exact_artist("Sleep", &sleep), None);
        assert_eq!(exact_artist("Fishman", &fishmans), None);
    }

    #[test]
    fn aliases_come_from_the_search_answer() {
        let body: serde_json::Value = serde_json::from_str(include_str!(
            "fixtures/musicbrainz/artist_search_arvo_part.json"
        ))
        .unwrap();
        let a: Artist = serde_json::from_value(body["artists"][0].clone()).unwrap();
        let m = artist_match(a);
        assert!(m.aliases.contains(&"Arvo Part".to_string()));
        assert_eq!(
            exact_artist("arvo part", &[m]).as_deref(),
            Some("ae0b2424-d4c5-4c54-82ac-fe3be5453270")
        );
    }

    #[test]
    fn the_genre_list_is_one_genre_per_line_from_the_mirror_when_set() {
        assert_eq!(
            genre_list(include_str!("fixtures/musicbrainz/genres.txt")).len(),
            10
        );
        assert_eq!(web_base(None), "https://musicbrainz.org");
        assert_eq!(web_base(Some("stubs:5050".into())), "http://stubs:5050");
    }

    #[tokio::test]
    async fn only_a_listed_genre_answer_is_a_genre_list() {
        use crate::test_http::{serve, Reply};
        let reply = |status, body: &str| Reply {
            status,
            body: body.into(),
        };
        let (base, _) = serve(vec![
            reply(404, "Not Found"),
            reply(200, ""),
            reply(
                200,
                "<!DOCTYPE html><html><body>Checking your browser</body></html>",
            ),
            reply(200, include_str!("fixtures/musicbrainz/genres.txt")),
        ]);
        assert!(matches!(
            genre_names_at(&base).await,
            Err(MetadataError::Rejected {
                status: Some(404),
                ..
            })
        ));
        assert!(matches!(
            genre_names_at(&base).await,
            Err(MetadataError::Unreadable { .. })
        ));
        assert!(matches!(
            genre_names_at(&base).await,
            Err(MetadataError::Unreadable { .. })
        ));
        assert_eq!(genre_names_at(&base).await.unwrap().len(), 10);
    }

    #[tokio::test]
    async fn musicbrainz_errors_keep_their_kind() {
        let mb_error: musicbrainz_rs::entity::api::MusicbrainzError =
            serde_json::from_value(serde_json::json!({"error": "Invalid mbid.", "help": "For usage, please see: https://musicbrainz.org/development/mmd"})).unwrap();
        let parse = serde_json::from_str::<u8>("x").unwrap_err();
        // A server that closes every connection unanswered: the port stays
        // bound for the test, so nothing else can take it and answer.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            for connection in listener.incoming() {
                drop(connection);
            }
        });
        let dropped = reqwest::get(format!("http://{addr}")).await.unwrap_err();
        let kinds: Vec<&str> = [
            musicbrainz_rs::Error::NotFound("q".into()),
            musicbrainz_rs::Error::MusicbrainzError("q".into(), mb_error),
            musicbrainz_rs::Error::MaxRetriesExceeded,
            musicbrainz_rs::Error::SerdeJsonError(parse),
            musicbrainz_rs::Error::ReqwestError(dropped),
        ]
        .into_iter()
        .map(|e| match MetadataError::from(e) {
            MetadataError::NotFound { .. } => "not_found",
            MetadataError::Rejected { .. } => "rejected",
            MetadataError::RateLimited { .. } => "rate_limited",
            MetadataError::Unreadable { .. } => "unreadable",
            MetadataError::Unavailable { .. } => "unavailable",
        })
        .collect();
        assert_eq!(
            kinds,
            [
                "not_found",
                "rejected",
                "rate_limited",
                "unreadable",
                "unavailable"
            ]
        );
    }

    /// GET https://musicbrainz.org/ws/2/recording/4d04e9b5-ecd1-4ef3-bae0-5c98492af1c8?inc=artists&fmt=json
    const RECORDING: &str = r#"{"title":"Silent Night","length":149000,"video":false,"first-release-date":"1995","id":"4d04e9b5-ecd1-4ef3-bae0-5c98492af1c8","disambiguation":"","artist-credit":[{"artist":{"disambiguation":"","country":"US","sort-name":"Crosby, Bing","type-id":"b6e035f4-3ce9-331c-97df-83397230b0df","name":"Bing Crosby","id":"2437980f-513a-44fc-80f1-b90d9d7fcf8f","type":"Person"},"name":"Bing Crosby","joinphrase":""}]}"#;
    /// GET https://musicbrainz.org/ws/2/release?recording=4d04e9b5-ecd1-4ef3-bae0-5c98492af1c8&inc=release-groups&limit=2&offset=0&fmt=json
    const RELEASES: &str = r#"{"release-count":1,"release-offset":0,"releases":[{"id":"317d1e85-3f9e-463e-939b-bebf47aa2178","title":"White Christmas","status":"Official","status-id":"4e304316-386d-3409-af2e-78857eec5cfe","date":"1995","country":"GB","disambiguation":"","quality":"normal","release-group":{"primary-type-id":"f529b476-6e62-324f-b0aa-1f3e33d313fc","title":"Merry Christmas","first-release-date":"1945","secondary-type-ids":["dd2a21e1-0c00-3729-a7a0-de60b84eb5d1"],"id":"a82c4b78-9e9a-390c-af1f-3c8a8a44c555","disambiguation":"later known as “White Christmas”","primary-type":"Album","secondary-types":["Compilation"]}}]}"#;
    /// GET https://musicbrainz.org/ws/2/artist?query=artist:%22Bing%20Crosby%22&limit=2&fmt=json (first artist)
    const ARTISTS: &str = r#"{"created":"2026-11-15T11:44:54.893Z","count":3,"offset":0,"artists":[{"id":"2437980f-513a-44fc-80f1-b90d9d7fcf8f","type":"Person","type-id":"b6e035f4-3ce9-331c-97df-83397230b0df","score":100,"name":"Bing Crosby","sort-name":"Crosby, Bing","country":"US","life-span":{"begin":"1903-05-03","end":"1977-10-14","ended":true}}]}"#;

    #[test]
    fn a_recording_comes_with_its_length_and_the_releases_it_is_on() {
        let r = recording_releases_from(
            serde_json::from_str(RECORDING).unwrap(),
            serde_json::from_str(RELEASES).unwrap(),
        )
        .unwrap();
        assert_eq!(
            r,
            RecordingReleases {
                recording: RecordingInfo {
                    mbid: "4d04e9b5-ecd1-4ef3-bae0-5c98492af1c8".into(),
                    title: "Silent Night".into(),
                    artist: Some("Bing Crosby".into()),
                    duration_s: Some(149),
                    disambiguation: None,
                    first_release_date: Some("1995".into())
                },
                release_count: 1,
                releases: vec![RecordingRelease {
                    mbid: "317d1e85-3f9e-463e-939b-bebf47aa2178".into(),
                    title: "White Christmas".into(),
                    date: Some("1995".into()),
                    country: Some("GB".into()),
                    status: Some("Official".into()),
                    release_group: Some(ReleaseGroupInfo {
                        mbid: "a82c4b78-9e9a-390c-af1f-3c8a8a44c555".into(),
                        title: "Merry Christmas".into(),
                        release_types: Some(vec!["Album".into(), "Compilation".into()]),
                        first_release_date: Some("1945".into())
                    })
                }],
            }
        );
    }

    #[test]
    fn a_search_sends_its_text_as_text() {
        // Each of these is URL or Lucene syntax when sent as it is.
        let name = r##"Simon & Garfunkel + "#1" \ (live): a/b"##;
        let url = Artist::search(query_param(&phrase("artist", name)))
            .limit(10)
            .as_api_request(musicbrainz_client())
            .url;
        let url = reqwest::Url::parse(&url).unwrap();
        assert_eq!(url.fragment(), None);
        let query: Vec<(String, String)> = url
            .query_pairs()
            .into_owned()
            .filter(|(key, _)| key != "fmt")
            .collect();
        assert_eq!(
            query,
            [
                (
                    "query".to_string(),
                    r##"artist:"Simon & Garfunkel + \"#1\" \\ (live): a/b""##.to_string()
                ),
                ("limit".to_string(), "10".to_string())
            ]
        );
        let none: &[ArtistMatch] = &[];
        assert_eq!(
            titled_query(
                "recording",
                "Silent Night",
                "artistname",
                Some(("Bing Crosby", none))
            ),
            "query=%28artistname%3A%22Bing+Crosby%22%29+AND+recording%3A%22Silent+Night%22"
        );
    }

    /// The Lucene query a `query=` parameter carries.
    fn lucene(param: &str) -> String {
        let (key, value) = url::form_urlencoded::parse(param.as_bytes())
            .into_owned()
            .next()
            .unwrap();
        assert_eq!(key, "query");
        value
    }

    #[test]
    fn an_artist_typed_by_an_alias_is_matched_by_mbid() {
        assert_eq!(
            lucene(&artist_search_query("Shiro Sagisu")),
            r#"artist:"Shiro Sagisu" OR alias:"Shiro Sagisu""#
        );
        // GET https://musicbrainz.org/ws/2/artist?query=artist:"Shiro Sagisu" OR alias:"Shiro Sagisu"&limit=5&fmt=json,
        // fetched 2026-11-21: 鷺巣詩郎, found by the alias "Shirô Sagisu" (the artist index ignores diacritics).
        let body: serde_json::Value = serde_json::from_str(include_str!(
            "fixtures/musicbrainz/artist_search_shiro_sagisu.json"
        ))
        .unwrap();
        let sagisu = artist_match(serde_json::from_value(body["artists"][0].clone()).unwrap());
        let kept = named_artists(
            "Shiro Sagisu",
            vec![
                found(
                    "7c3fd8f4-6ef1-4f3b-9d3a-6c1e5a0d2b11",
                    "Sagisu Tribute Band",
                    &["Shiro Sagisu Orchestra"],
                ),
                sagisu.clone(),
            ],
        );
        assert_eq!(kept, [sagisu]);
        let artist = Some(("Shiro Sagisu", kept.as_slice()));
        assert_eq!(
            lucene(&titled_query(
                "recording",
                "Here to Stay",
                "artistname",
                artist
            )),
            r#"(artistname:"Shiro Sagisu" OR arid:"9b127d23-f877-4541-83cd-44f46393dfb7") AND recording:"Here to Stay""#
        );
        assert_eq!(
            lucene(&titled_query("releasegroup", "Bleach", "artist", artist)),
            r#"(artist:"Shiro Sagisu" OR arid:"9b127d23-f877-4541-83cd-44f46393dfb7") AND releasegroup:"Bleach""#
        );
        assert_eq!(
            lucene(&titled_query("releasegroup", "Bleach", "artist", None)),
            r#"releasegroup:"Bleach""#
        );
    }

    #[test]
    fn an_artist_whose_alias_only_holds_the_typed_name_is_not_matched() {
        // The first two hits of artist:"Queen" OR alias:"Queen", fetched 2026-11-21.
        let queen = found("0383dadf-2a4e-4d10-a46a-e9e041da8eb3", "Queen", &["クイーン"]);
        let kept = named_artists(
            "queen",
            vec![
                queen.clone(),
                found(
                    "54799c0e-eb45-4eea-996d-c4d71a63c499",
                    "Ella Fitzgerald",
                    &["Queen of Jazz"],
                ),
            ],
        );
        assert_eq!(kept, [queen]);
        assert_eq!(
            lucene(&titled_query(
                "recording",
                "Love",
                "artistname",
                Some(("queen", kept.as_slice()))
            )),
            r#"(artistname:"queen" OR arid:"0383dadf-2a4e-4d10-a46a-e9e041da8eb3") AND recording:"Love""#
        );
    }

    #[test]
    fn a_negative_release_count_is_unreadable() {
        let mut page: serde_json::Value = serde_json::from_str(RELEASES).unwrap();
        page["release-count"] = (-1).into();
        let e = recording_releases_from(
            serde_json::from_str(RECORDING).unwrap(),
            serde_json::from_value(page).unwrap(),
        )
        .unwrap_err();
        assert_eq!(
            e.to_string(),
            "Unreadable MusicBrainz answer: MusicBrainz answered a negative release count"
        );
    }

    #[test]
    fn a_release_type_the_crate_does_not_know_gives_no_release_types() {
        let mut page: serde_json::Value = serde_json::from_str(RELEASES).unwrap();
        page["releases"][0]["release-group"]["secondary-types"] =
            serde_json::json!(["Field recording"]);
        let page: musicbrainz_rs::entity::BrowseResult<Release> =
            serde_json::from_value(page).unwrap();
        let group = page.entities[0].release_group.as_ref().unwrap();
        assert_eq!(release_types(group), None);
    }

    #[test]
    fn a_recording_without_an_artist_credit_has_no_artist() {
        let mut recording: serde_json::Value = serde_json::from_str(RECORDING).unwrap();
        recording.as_object_mut().unwrap().remove("artist-credit");
        let r = recording_releases_from(
            serde_json::from_value(recording).unwrap(),
            serde_json::from_str(RELEASES).unwrap(),
        )
        .unwrap();
        assert_eq!(r.recording.artist, None);
    }

    #[test]
    fn a_release_group_with_no_type_set_has_unknown_types() {
        // A release group no editor gave a type: MusicBrainz sends "primary-type": null and an empty
        // "secondary-types" (a lookup or browse answer), or leaves both out (a search answer).
        let types = |primary: serde_json::Value, secondary: serde_json::Value| {
            let mut page: serde_json::Value = serde_json::from_str(RELEASES).unwrap();
            let group = &mut page["releases"][0]["release-group"];
            group["primary-type"] = primary;
            group["primary-type-id"] = serde_json::Value::Null;
            group["secondary-type-ids"] = serde_json::json!([]);
            group["secondary-types"] = secondary;
            let page: BrowseResult<Release> = serde_json::from_value(page).unwrap();
            release_types(page.entities[0].release_group.as_ref().unwrap())
        };
        assert_eq!(types(serde_json::Value::Null, serde_json::json!([])), None);
        let bare: ReleaseGroup = serde_json::from_str(
            r#"{"id":"a82c4b78-9e9a-390c-af1f-3c8a8a44c555","title":"Merry Christmas"}"#,
        )
        .unwrap();
        assert_eq!(release_types(&bare), None);
        // The secondary types an editor did set are known without a primary type.
        assert_eq!(
            types(serde_json::Value::Null, serde_json::json!(["Live"])),
            Some(vec!["Live".to_string()])
        );
    }

    #[test]
    fn an_artist_match_has_its_type_country_and_dates() {
        let page: musicbrainz_rs::entity::search::SearchResult<Artist> =
            serde_json::from_str(ARTISTS).unwrap();
        assert_eq!(
            artist_match(page.entities.into_iter().next().unwrap()),
            ArtistMatch {
                mbid: "2437980f-513a-44fc-80f1-b90d9d7fcf8f".into(),
                name: "Bing Crosby".into(),
                sort_name: "Crosby, Bing".into(),
                disambiguation: None,
                artist_type: Some("Person".into()),
                country: Some("US".into()),
                begin: Some("1903-05-03".into()),
                end: Some("1977-10-14".into()),
                aliases: vec![]
            }
        );
    }

    /// GET https://musicbrainz.org/ws/2/release/317d1e85-3f9e-463e-939b-bebf47aa2178?inc=recordings+artist-credits+release-groups&fmt=json,
    /// fetched 2026-11-15, trimmed to the fields above plus the medium holding track 6 (its other 16 tracks left out).
    const RELEASE: &str = r#"{"id":"317d1e85-3f9e-463e-939b-bebf47aa2178","title":"White Christmas","status":"Official","status-id":"4e304316-386d-3409-af2e-78857eec5cfe","date":"1995","country":"GB","disambiguation":"","quality":"normal","artist-credit":[{"joinphrase":"","name":"Bing Crosby","artist":{"type-id":"b6e035f4-3ce9-331c-97df-83397230b0df","id":"2437980f-513a-44fc-80f1-b90d9d7fcf8f","type":"Person","sort-name":"Crosby, Bing","country":"US","disambiguation":"","name":"Bing Crosby"}}],"release-group":{"title":"Merry Christmas","primary-type":"Album","secondary-type-ids":["dd2a21e1-0c00-3729-a7a0-de60b84eb5d1"],"id":"a82c4b78-9e9a-390c-af1f-3c8a8a44c555","primary-type-id":"f529b476-6e62-324f-b0aa-1f3e33d313fc","disambiguation":"later known as “White Christmas”","secondary-types":["Compilation"],"first-release-date":"1945"},"media":[{"format":"CD","format-id":"9712d52a-4509-3d4b-a1a2-67c88c643e31","position":1,"track-count":17,"track-offset":0,"tracks":[{"artist-credit":[{"joinphrase":"","artist":{"country":"US","disambiguation":"","name":"Bing Crosby","sort-name":"Crosby, Bing","id":"2437980f-513a-44fc-80f1-b90d9d7fcf8f","type-id":"b6e035f4-3ce9-331c-97df-83397230b0df","type":"Person"},"name":"Bing Crosby"}],"number":"6","length":149000,"recording":{"id":"4d04e9b5-ecd1-4ef3-bae0-5c98492af1c8","first-release-date":"1995","artist-credit":[{"joinphrase":"","name":"Bing Crosby","artist":{"id":"2437980f-513a-44fc-80f1-b90d9d7fcf8f","type-id":"b6e035f4-3ce9-331c-97df-83397230b0df","type":"Person","country":"US","disambiguation":"","name":"Bing Crosby","sort-name":"Crosby, Bing"}}],"disambiguation":"","video":false,"length":149000,"title":"Silent Night"},"id":"72b85e69-2569-30a8-a1f2-ad514cd8c9d7","title":"Silent Night","position":6}]}]}"#;

    #[test]
    fn a_looked_up_release_carries_its_release_groups_types() {
        let r = album_from_release(serde_json::from_str(RELEASE).unwrap());
        assert_eq!(
            (r.album.title.as_str(), r.album.release_types.clone()),
            (
                "White Christmas",
                Some(vec!["Album".to_string(), "Compilation".to_string()])
            )
        );
        assert_eq!(
            (r.tracks[0].id.as_str(), r.tracks[0].duration.as_deref()),
            ("4d04e9b5-ecd1-4ef3-bae0-5c98492af1c8", Some("02:29"))
        );
        let mut bare: serde_json::Value = serde_json::from_str(RELEASE).unwrap();
        bare.as_object_mut().unwrap().remove("release-group");
        assert_eq!(
            album_from_release(serde_json::from_value(bare).unwrap())
                .album
                .release_types,
            None
        );
    }

    /// GET https://musicbrainz.org/ws/2/release-group?query=releasegroup:%22Bigger%20Than%20All%20of%20Us%22%20AND%20artist:%22Above%20%26%20Beyond%22&inc=releases&limit=5&fmt=json,
    /// fetched 2026-11-21, its first release group with the artist's aliases and the tags left out.
    const RELEASE_GROUP: &str = r#"{"id":"58a8a8d6-3738-4339-b104-43bb75785dda","type-id":"f529b476-6e62-324f-b0aa-1f3e33d313fc","score":100,"primary-type-id":"f529b476-6e62-324f-b0aa-1f3e33d313fc","artist-credit-id":"ed3cc697-caa6-332d-bd10-789f9fd471b5","count":6,"title":"Bigger Than All of Us","first-release-date":"2025-07-18","primary-type":"Album","artist-credit":[{"name":"Above & Beyond","artist":{"id":"370bd5a3-4abf-4356-8576-3a8fc0c11d65","name":"Above & Beyond","sort-name":"Above & Beyond"}}],"releases":[{"id":"cf554d3c-52be-4609-8f87-ea7fb47a82fe","title":"Bigger Than All of Us"},{"id":"032e6446-2867-4ebe-b2d2-0a96ee460cd8","status-id":"4e304316-386d-3409-af2e-78857eec5cfe","title":"Bigger Than All of Us (extended mixes)","status":"Official"},{"id":"cfe38b07-2375-43ee-81eb-09cc1e4beb1e","status-id":"4e304316-386d-3409-af2e-78857eec5cfe","title":"Bigger Than All of Us (album & extended mixes)","status":"Official"},{"id":"e6f0e965-99cb-47ab-8ccd-616a11ce83e3","status-id":"4e304316-386d-3409-af2e-78857eec5cfe","title":"Bigger Than All of Us","status":"Official"},{"id":"c44a58b2-025d-4835-bcd2-a8affc598d5d","status-id":"4e304316-386d-3409-af2e-78857eec5cfe","title":"Bigger Than All of Us","status":"Official"},{"id":"14e4d22e-d686-4957-bb97-b686ec6459d2","status-id":"4e304316-386d-3409-af2e-78857eec5cfe","title":"Bigger Than All of Us","status":"Official"}]}"#;

    /// Issue #93: the search showed the extended mixes release (11 tracks), the
    /// first official one listed, for the album.
    #[test]
    fn an_album_search_shows_the_official_release_titled_as_its_group() {
        let group: ReleaseGroup = serde_json::from_str(RELEASE_GROUP).unwrap();
        assert_eq!(
            album_release(&group).map(|r| r.id.as_str()),
            Some("e6f0e965-99cb-47ab-8ccd-616a11ce83e3")
        );

        let mut others: serde_json::Value = serde_json::from_str(RELEASE_GROUP).unwrap();
        others["title"] = "Bigger Than All of Us: The Remixes".into();
        let others: ReleaseGroup = serde_json::from_value(others).unwrap();
        assert_eq!(
            album_release(&others).map(|r| r.id.as_str()),
            Some("032e6446-2867-4ebe-b2d2-0a96ee460cd8")
        );

        // Only the release that has no status.
        let mut unofficial: serde_json::Value = serde_json::from_str(RELEASE_GROUP).unwrap();
        let first = unofficial["releases"][0].clone();
        unofficial["releases"] = serde_json::json!([first]);
        let unofficial: ReleaseGroup = serde_json::from_value(unofficial).unwrap();
        assert!(album_release(&unofficial).is_none());
    }

    #[test]
    fn a_recording_musicbrainz_does_not_have_is_not_found() {
        // GET https://musicbrainz.org/ws/2/recording/0a2f7e4b-6c1d-4e8a-9b3f-5d7c1e2a4b6c?inc=artists&fmt=json answers
        // 404 {"help":"For usage, please see: https://musicbrainz.org/development/mmd","error":"Not Found"},
        // which musicbrainz_rs turns into NotFound(url) (src/entity/api.rs:14-19, src/api/api_request.rs:51-67)
        let url = "http://musicbrainz.org/ws/2/recording/0a2f7e4b-6c1d-4e8a-9b3f-5d7c1e2a4b6c?fmt=json&inc=artists";
        let e = MetadataError::from(musicbrainz_rs::Error::NotFound(url.into()));
        assert_eq!(
            e.to_string(),
            format!(
                "MusicBrainz found nothing: Musicbrainz returned \"Not found\" for query \"{url}\""
            )
        );
    }
}
