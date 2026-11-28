//! Discovery 2.0 core logic:
//! - `UserCultureProfile` persistence
//! - Extensible data-driven `ContextualMixDefinition` CRUD (built-in global catalog + custom user/admin mixes)
//! - Pure client-side i18n `ContextualMixSummary` payloads (zero hardcoded prose on the wire)
//! - Live cultural steering & per-user enable/disable overrides
//! - Real playlist materialization:
//!   - Seasonal packs reconciled as Navidrome smart playlists (`Discovery/Seasonal/<slug>/`) with post-expiry cleanup
//!   - Moment/custom mixes materialized into playable blended tracklists (owned Navidrome + fresh staged tracks) with optional static Navidrome playlist sync
//! - Discovery Triage queue & On-Demand Seed Radio with structured `CandidateReason`

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::{Arc, LazyLock, Mutex, PoisonError};
use std::time::{Duration, Instant};

use chrono::{Datelike, Timelike, Utc};
use shared::failure::{Failure, FailureCode, RetryAdvice, Stage};
use shared::navidrome::{DiscoveryStatus, DiscoveryTrack};
use shared::recommendation::{
    canonical_mix_title, discovery_name, Candidate, CandidateReason, CandidateSet,
    ContextualMixDefinition, ContextualMixKind, ContextualMixPlaylist, ContextualMixSummary,
    ContextualMixTrack, CultureBucket, CultureSteering, DiscoveryProfile, MixSelectionRules,
    MixSortOrder, SeedRadioRequest, SeedRadioResponse, SeedRadioTrack, SetMixEnabledRequest,
    UpdateCultureProfileRequest, UpsertContextualMixRequest, UserCultureProfile, WeatherCondition,
    WeatherSnapshot,
};
use soulbeet::engine::contextual::MAX_TRACKS_PER_ARTIST;
use soulbeet::navidrome::models::{NativeBrowseSong, SubsonicNamedGenre, SubsonicSong};
use soulbeet::navidrome::{filepath_rules, NavidromeClient, PlaylistMeta, SmartPlaylistChange};
use soulbeet::{MusicKnowledge, NavidromeError};
use sqlx::SqlitePool;
use tracing::{info, warn};
use uuid::Uuid;

use crate::auth::{UserCtx, UserId};
use crate::core::database_failure;
use crate::core::discovery::{expires_at_with_seasonal, parse_profiles, SeasonalExpiryLookup};
use crate::core::index::NavidromeOf;
use crate::core::library::{navidrome_failure, navidrome_for, NavidromePlace};
use crate::models::contextual::{
    ContextualMixDefinitionRow, ContextualMixStateRow, UserCultureProfileRow,
};
use crate::models::discovery_playlist::DiscoveryTrackRow;
use crate::models::navidrome_library::NavidromeLibraryRow;
use crate::models::user_settings::UserSettings;
use crate::services::NavidromeLookupError;

const LIBRARY_SNAPSHOT_TTL: Duration = Duration::from_secs(45);
const MAX_CACHED_LIBRARY_USERS: usize = 64;

struct CachedLibrarySnapshot {
    songs: Arc<Vec<SubsonicSong>>,
    fetched_at: Instant,
}

static LIBRARY_SNAPSHOT_CACHE: LazyLock<Mutex<HashMap<(String, String), CachedLibrarySnapshot>>> =
    LazyLock::new(Default::default);

fn native_song_to_subsonic(browse: NativeBrowseSong) -> SubsonicSong {
    let song = browse.song;
    let genre = (!browse.genre.trim().is_empty()).then_some(browse.genre);
    let genres = browse
        .genres
        .into_iter()
        .filter(|g| !g.name.trim().is_empty())
        .map(|g| SubsonicNamedGenre { name: g.name })
        .collect();
    let created = browse
        .created_at
        .map(|d| d.to_rfc3339())
        .or_else(|| Some(song.updated_at.to_rfc3339()));
    SubsonicSong {
        id: song.id,
        title: song.title,
        artist: (!song.artist.is_empty()).then_some(song.artist),
        album: (!song.album.is_empty()).then_some(song.album),
        album_id: (!song.album_id.is_empty()).then_some(song.album_id),
        genre,
        genres,
        duration: (song.duration > 0.0).then_some(song.duration.round() as u32),
        user_rating: (song.rating > 0).then_some(song.rating),
        play_count: (song.play_count > 0)
            .then_some(u32::try_from(song.play_count).unwrap_or(u32::MAX)),
        created,
        played: song.play_date.map(|d| d.to_rfc3339()),
        starred: song.starred.then(|| {
            song.starred_at
                .map(|d| d.to_rfc3339())
                .unwrap_or_else(|| "1970-01-01T00:00:00Z".to_string())
        }),
        music_brainz_id: song.mbz_recording_id,
        suffix: (!song.suffix.is_empty()).then_some(song.suffix),
        bit_rate: (song.bit_rate > 0).then_some(song.bit_rate),
        size: (song.size > 0).then_some(song.size),
        year: (song.year > 0).then_some(song.year),
        track: (song.track_number > 0).then_some(song.track_number),
        path: (!song.path.is_empty()).then_some(song.path),
    }
}

/// Fetches all present library songs via Navidrome's native API (`GET /api/song`), which always
/// returns real library-relative paths regardless of `ReportRealPath`, carries `genre`/`genres`
/// and `createdAt`, and reports DB errors via HTTP 5xx instead of silent empty Subsonic `search3`
/// responses.
pub(crate) async fn fetch_all_songs_native(
    navi: &NavidromeClient,
) -> Result<Vec<SubsonicSong>, NavidromeError> {
    match navi.all_browse_songs(false).await {
        Ok(page) => Ok(page
            .songs
            .into_iter()
            .filter(|s| !s.song.missing)
            .map(native_song_to_subsonic)
            .collect()),
        #[cfg(test)]
        Err(NavidromeError::Parse(_)) => navi.get_all_songs_with_ratings().await,
        Err(e) => Err(e),
    }
}

/// Fetches `user_id`'s native Navidrome library snapshot using a 45-second per-user TTL cache so
/// rapid bootstrap/home/mix calls do not repeatedly scan large libraries.
pub(crate) async fn fetch_library_snapshot_cached(
    user_id: &str,
    navi: &NavidromeClient,
) -> Result<Arc<Vec<SubsonicSong>>, NavidromeError> {
    let key = (user_id.to_string(), navi.base_url().as_str().to_string());
    let now = Instant::now();
    {
        let mut cache = LIBRARY_SNAPSHOT_CACHE
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if let Some(entry) = cache.get(&key) {
            if now.saturating_duration_since(entry.fetched_at) < LIBRARY_SNAPSHOT_TTL {
                return Ok(Arc::clone(&entry.songs));
            }
        }
        cache.retain(|_, v| now.saturating_duration_since(v.fetched_at) < LIBRARY_SNAPSHOT_TTL);
    }

    let songs = Arc::new(fetch_all_songs_native(navi).await?);
    let mut cache = LIBRARY_SNAPSHOT_CACHE
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    if cache.len() >= MAX_CACHED_LIBRARY_USERS && !cache.contains_key(&key) {
        if let Some(oldest_key) = cache
            .iter()
            .min_by_key(|(_, v)| v.fetched_at)
            .map(|(k, _)| k.clone())
        {
            cache.remove(&oldest_key);
        }
    }
    cache.insert(
        key,
        CachedLibrarySnapshot {
            songs: Arc::clone(&songs),
            fetched_at: Instant::now(),
        },
    );
    Ok(songs)
}

/// Drops any cached Navidrome library snapshot for `user_id` (called after scans, curation,
/// rating sync, or an explicit mix refresh).
pub(crate) fn invalidate_library_snapshot_cache(user_id: &str) {
    let mut cache = LIBRARY_SNAPSHOT_CACHE
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    cache.retain(|(uid, _), _| uid != user_id);
}

/// Clears stale `song_id` values on pending discovery tracks when the stored `song_id` no longer
/// exists in the live `library` snapshot (e.g. deleted or rescanned under a new ID), so subsequent
/// reconciliation can re-match them and mixes never emit unplayable rows.
pub(crate) async fn clear_stale_discovery_song_ids_in(
    db: &SqlitePool,
    library: &[SubsonicSong],
    tracks: &mut [DiscoveryTrack],
) -> usize {
    if tracks.is_empty() {
        return 0;
    }
    let live_ids: HashSet<&str> = library.iter().map(|s| s.id.as_str()).collect();
    let mut cleared = 0usize;
    for track in tracks.iter_mut() {
        if track.status != DiscoveryStatus::Pending {
            continue;
        }
        let Some(song_id) = track.song_id.as_deref().filter(|s| !s.is_empty()) else {
            continue;
        };
        if !live_ids.contains(song_id) {
            if let Err(e) = DiscoveryTrackRow::clear_song_id_in(db, &track.id).await {
                warn!(
                    "Failed to clear stale song_id '{}' for discovery track '{}': {}",
                    song_id, track.id, e
                );
            }
            track.song_id = None;
            cleared += 1;
        }
    }
    cleared
}

/// Reads `user_id`'s stored `UserCultureProfile`, or returns sensible defaults if unset.
pub(crate) async fn get_culture_profile_in(
    db: &SqlitePool,
    user_id: &str,
) -> Result<UserCultureProfile, Failure> {
    UserCultureProfileRow::get_in(db, user_id)
        .await
        .map_err(|e| database_failure(Stage::Input, e))
        .map(|opt| opt.unwrap_or_default())
}

/// Updates explicit fields of `user_id`'s `UserCultureProfile` while preserving inferred fields.
pub(crate) async fn update_culture_profile_in(
    db: &SqlitePool,
    user_id: &str,
    req: UpdateCultureProfileRequest,
) -> Result<UserCultureProfile, Failure> {
    update_culture_profile_with_geocoder_in(db, user_id, req, |query, preferred| async move {
        crate::core::weather::geocode_city(&query, preferred.as_deref()).await
    })
    .await
}

/// Inner implementation of [`update_culture_profile_in`] with an injectable geocoder for offline tests.
pub(crate) async fn update_culture_profile_with_geocoder_in<F, Fut>(
    db: &SqlitePool,
    user_id: &str,
    req: UpdateCultureProfileRequest,
    geocode: F,
) -> Result<UserCultureProfile, Failure>
where
    F: FnOnce(String, Option<String>) -> Fut,
    Fut: std::future::Future<Output = Result<crate::core::weather::WeatherLocation, Failure>>,
{
    let mut current = get_culture_profile_in(db, user_id).await?;
    if let Some(country) = req.home_country {
        let trimmed = country.trim();
        current.home_country = if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_ascii_uppercase())
        };
    }
    if let Some(langs) = req.preferred_languages {
        current.preferred_languages = langs
            .into_iter()
            .map(|l| l.trim().to_ascii_lowercase())
            .filter(|l| !l.is_empty())
            .collect();
    }
    if let Some(ratio) = req.local_culture_ratio {
        if !(0.0..=1.0).contains(&ratio) {
            return Err(Failure::new(
                FailureCode::InvalidInput,
                Stage::Input,
                RetryAdvice::NotRetryable,
                "local_culture_ratio must be between 0.0 and 1.0",
            ));
        }
        current.local_culture_ratio = ratio;
    }
    if let Some(city_raw) = req.weather_city {
        let trimmed = city_raw.trim();
        if trimmed.is_empty() {
            current.weather_city = None;
            current.weather_lat = None;
            current.weather_lon = None;
        } else {
            if trimmed.chars().count() > crate::core::weather::MAX_WEATHER_CITY_CHARS {
                return Err(Failure::new(
                    FailureCode::InvalidInput,
                    Stage::Input,
                    RetryAdvice::NotRetryable,
                    format!(
                        "weather_city must be at most {} characters",
                        crate::core::weather::MAX_WEATHER_CITY_CHARS
                    ),
                ));
            }
            let already_resolved = current
                .weather_city
                .as_deref()
                .is_some_and(|c| c.eq_ignore_ascii_case(trimmed))
                && current.weather_lat.is_some_and(|lat| {
                    lat.is_finite() && (-90.0..=90.0).contains(&lat)
                })
                && current.weather_lon.is_some_and(|lon| {
                    lon.is_finite() && (-180.0..=180.0).contains(&lon)
                });
            if !already_resolved {
                let preferred_country = current.effective_country().map(str::to_string);
                let loc = geocode(trimmed.to_string(), preferred_country).await?;
                if !loc.lat.is_finite()
                    || !loc.lon.is_finite()
                    || !(-90.0..=90.0).contains(&loc.lat)
                    || !(-180.0..=180.0).contains(&loc.lon)
                {
                    return Err(Failure::new(
                        FailureCode::InvalidInput,
                        Stage::Input,
                        RetryAdvice::NotRetryable,
                        "Geocoded coordinates are out of valid range",
                    ));
                }
                current.weather_city = Some(loc.label.unwrap_or_else(|| trimmed.to_string()));
                current.weather_lat = Some(loc.lat);
                current.weather_lon = Some(loc.lon);
            }
        }
    }

    UserCultureProfileRow::upsert_in(db, user_id, &current)
        .await
        .map_err(|e| database_failure(Stage::Input, e))
}

/// Validates and normalizes a contextual mix slug (`[a-z0-9_-]`, 2..=48 chars).
fn normalize_slug(raw: &str) -> Result<String, Failure> {
    let slug = raw.trim().to_ascii_lowercase();
    let valid = (2..=48).contains(&slug.len())
        && slug
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-');
    if !valid {
        return Err(Failure::new(
            FailureCode::InvalidInput,
            Stage::Input,
            RetryAdvice::NotRetryable,
            "Mix slug must be 2–48 lowercase ASCII letters, digits, hyphens, or underscores",
        ));
    }
    Ok(slug)
}

/// Normalizes a mix slug while mapping legacy seasonal aliases consistently across all endpoints.
fn canonical_slug(raw: &str) -> Result<String, Failure> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "noel" | "holiday" => Ok("winter_holiday".to_string()),
        "nye" => Ok("new_years_eve".to_string()),
        other => normalize_slug(other),
    }
}

/// Normalizes a Subsonic/DB rating into `1..=5` (`0` means unrated -> `None`).
fn normalized_rating(raw: Option<u8>) -> Option<u8> {
    raw.filter(|&r| (1..=5).contains(&r))
}

/// True when `path` sits inside a staged `Discovery/` directory.
fn is_staged_discovery_path(path: &str) -> bool {
    path.split(['/', '\\'])
        .any(|seg| seg.eq_ignore_ascii_case("discovery"))
}

/// Hydrates `DiscoveryTrack.rating` from Navidrome songs when `song_id` matches,
/// falling back to the stored DB rating (with `0` normalized to `None`).
pub(crate) fn hydrate_discovery_ratings(library: &[SubsonicSong], tracks: &mut [DiscoveryTrack]) {
    if tracks.is_empty() {
        return;
    }
    let rating_by_song_id: HashMap<&str, u8> = library
        .iter()
        .filter_map(|s| normalized_rating(s.user_rating).map(|r| (s.id.as_str(), r)))
        .collect();
    for track in tracks.iter_mut() {
        let nav_rating = track
            .song_id
            .as_deref()
            .filter(|s| !s.is_empty())
            .and_then(|id| rating_by_song_id.get(id).copied());
        track.rating = nav_rating.or_else(|| normalized_rating(track.rating));
    }
}

/// True when `track` is a pending discovery track missing its Navidrome `song_id`.
pub(crate) fn needs_song_id_reconciliation(track: &DiscoveryTrack) -> bool {
    track.status == DiscoveryStatus::Pending
        && track.song_id.as_deref().is_none_or(str::is_empty)
}

/// Normalizes path separators to `/`, collapses duplicate slashes, and strips trailing `/`.
fn normalize_slash_path(raw: &str) -> String {
    let replaced = raw.replace('\\', "/");
    let mut out = String::with_capacity(replaced.len());
    let mut prev_slash = false;
    for ch in replaced.chars() {
        if ch == '/' {
            if !prev_slash {
                out.push('/');
            }
            prev_slash = true;
        } else {
            out.push(ch);
            prev_slash = false;
        }
    }
    if out.len() > 1 {
        while out.ends_with('/') && out.len() > 1 {
            out.pop();
        }
    }
    out
}

/// Extracts the subpath starting at the `Discovery` segment (case-insensitive), e.g.
/// `"/music/alice/Discovery/Balanced/01.flac"` -> `Some("Discovery/Balanced/01.flac")`.
fn discovery_suffix(norm_path: &str) -> Option<&str> {
    let mut offset = 0usize;
    for seg in norm_path.split('/') {
        if seg.eq_ignore_ascii_case("discovery") {
            return Some(&norm_path[offset..]);
        }
        offset += seg.len() + 1;
    }
    None
}

/// Returns true when a normalized Navidrome song path looks like Navidrome's synthetic
/// `ReportRealPath=false` metadata path (`{Artist}/{Album}/{Title}.{ext}` or
/// `{Artist}/{Album}/{Track} - {Title}.{ext}`), rather than a real filesystem path.
fn is_likely_fake_navidrome_path(
    song: &SubsonicSong,
    song_norm: &str,
    known_prefixes: &[String],
) -> bool {
    if song_norm.starts_with('/') || song_norm.as_bytes().get(1) == Some(&b':') {
        return false;
    }
    if is_staged_discovery_path(song_norm) {
        return false;
    }
    let norm_lower = song_norm.to_ascii_lowercase();
    for prefix in known_prefixes {
        let bare = prefix.trim_end_matches('/');
        if !prefix.is_empty()
            && norm_lower.starts_with(prefix.as_str())
            && song
                .artist
                .as_deref()
                .is_none_or(|a| !a.eq_ignore_ascii_case(bare))
        {
            return false;
        }
    }
    let segs: Vec<&str> = song_norm.split('/').filter(|s| !s.is_empty()).collect();
    // Navidrome's fakePath is `{AlbumArtist}/{Album}/{Track} - {Title}.{ext}` (2-3 segments).
    if !(2..=3).contains(&segs.len()) {
        return false;
    }
    let filename = segs[segs.len() - 1];
    let stem = filename
        .rsplit_once('.')
        .map(|(s, _)| s)
        .unwrap_or(filename);
    let title_key = discovery_name(&song.title);
    if !title_key.is_empty() && !discovery_name(stem).contains(&title_key) {
        return false;
    }
    true
}

/// Joins a normalized root directory with a normalized relative subpath.
fn join_norm(root: &str, rel: &str) -> String {
    let r = root.trim_end_matches('/');
    let p = rel.trim_start_matches('/');
    if r.is_empty() {
        format!("/{p}")
    } else if p.is_empty() {
        r.to_string()
    } else {
        format!("{r}/{p}")
    }
}

/// Reconciles missing `song_id` values on pending `tracks` against a fetched Navidrome `library`,
/// persisting each matched `song_id` to SQLite (`discovery_tracks.song_id`) and updating `tracks`
/// in place.
///
/// Matching hierarchy:
/// 1. **Path match (primary):** exact local path mapped via `NAVIDROME_MUSIC_PATH` / learned
///    `navidrome_libraries` roots, relative `Discovery/` tail match, then exact and case-insensitive
///    `Discovery/...` directory-scoped suffix match (never bare filename matching).
/// 2. **Discovery-scoped `(artist, title)` match (secondary):** normalized `(artist, title)`
///    restricted to Navidrome songs whose path is inside `Discovery/` so pending tracks never bind
///    to owned/promoted library copies.
/// 3. **Fallback `(artist, title)` match (only when `ReportRealPath` is disabled):** allowed ONLY
///    when `library` contains no real filesystem paths (neither `Discovery/` paths nor real
///    non-Discovery paths), matching a unique unused normalized `(artist, title)` entry.
///
/// Fast path: returns `0` immediately when all pending tracks already have a non-empty `song_id`.
pub(crate) async fn reconcile_discovery_song_ids_in(
    db: &SqlitePool,
    library: &[SubsonicSong],
    music_path: Option<&Path>,
    tracks: &mut [DiscoveryTrack],
) -> usize {
    if !tracks.iter().any(needs_song_id_reconciliation) || library.is_empty() {
        return 0;
    }

    let learned_rows = NavidromeLibraryRow::all_in(db).await.unwrap_or_default();
    let music_root_norm = music_path.map(|p| normalize_slash_path(&p.to_string_lossy()));
    let learned_roots: Vec<(String, String)> = learned_rows
        .iter()
        .map(|row| {
            (
                normalize_slash_path(&row.navidrome_path),
                normalize_slash_path(&row.local_path),
            )
        })
        .collect();

    // Known directory prefixes (e.g. "music/", "alice/") that identify real relative paths.
    let mut known_prefixes: Vec<String> = Vec::new();
    let mut push_prefix = |seg: &str| {
        let s = seg.trim_matches('/').to_ascii_lowercase();
        if !s.is_empty() {
            let p = format!("{s}/");
            if !known_prefixes.contains(&p) {
                known_prefixes.push(p);
            }
        }
    };
    if let Some(ref mp) = music_root_norm {
        if let Some(last) = mp.rsplit('/').find(|s| !s.is_empty()) {
            push_prefix(last);
        }
    }
    for (nav_root, local_root) in &learned_roots {
        if let Some(last) = nav_root.rsplit('/').find(|s| !s.is_empty()) {
            push_prefix(last);
        }
        if let Some(last) = local_root.rsplit('/').find(|s| !s.is_empty()) {
            push_prefix(last);
        }
    }
    for track in tracks.iter() {
        let tnorm = normalize_slash_path(&track.path);
        for seg in tnorm.split('/') {
            if seg.eq_ignore_ascii_case("discovery") {
                break;
            }
            if !seg.is_empty() {
                push_prefix(seg);
            }
        }
    }

    let mut exact_local: HashMap<String, &str> = HashMap::new();
    let mut relative_discovery: Vec<(String, &str)> = Vec::new();
    let mut disc_suffix_exact: HashMap<String, Vec<(&str, String)>> = HashMap::new();
    let mut disc_suffix_lower: HashMap<String, Vec<(&str, String)>> = HashMap::new();
    let mut disc_by_key: HashMap<String, Vec<(&str, String)>> = HashMap::new();
    let mut all_by_key: HashMap<String, Vec<&str>> = HashMap::new();
    let mut any_discovery_path = false;
    let mut library_has_real_paths = false;

    for song in library {
        let key = CandidateSet::key(song.artist.as_deref().unwrap_or(""), &song.title);
        all_by_key.entry(key.clone()).or_default().push(song.id.as_str());

        let Some(raw_path) = song.path.as_deref().filter(|p| !p.trim().is_empty()) else {
            continue;
        };
        let song_norm = normalize_slash_path(raw_path);
        if !is_likely_fake_navidrome_path(song, &song_norm, &known_prefixes) {
            library_has_real_paths = true;
        }
        exact_local
            .entry(song_norm.clone())
            .or_insert(song.id.as_str());

        if !song_norm.starts_with('/') {
            if let Some(ref mp) = music_root_norm {
                exact_local
                    .entry(join_norm(mp, &song_norm))
                    .or_insert(song.id.as_str());
            }
            for (_, local_root) in &learned_roots {
                exact_local
                    .entry(join_norm(local_root, &song_norm))
                    .or_insert(song.id.as_str());
            }
        } else {
            for (nav_root, local_root) in &learned_roots {
                let prefix = if nav_root == "/" {
                    "/".to_string()
                } else {
                    format!("{nav_root}/")
                };
                if let Some(rel) = song_norm.strip_prefix(&prefix) {
                    exact_local
                        .entry(join_norm(local_root, rel))
                        .or_insert(song.id.as_str());
                }
            }
        }

        if let Some(suffix) = discovery_suffix(&song_norm) {
            any_discovery_path = true;
            if !song_norm.starts_with('/') {
                relative_discovery.push((song_norm.clone(), song.id.as_str()));
            }
            disc_suffix_exact
                .entry(suffix.to_string())
                .or_default()
                .push((song.id.as_str(), song_norm.clone()));
            disc_suffix_lower
                .entry(suffix.to_lowercase())
                .or_default()
                .push((song.id.as_str(), song_norm.clone()));
            disc_by_key
                .entry(key)
                .or_default()
                .push((song.id.as_str(), song_norm));
        }
    }

    let mut used_song_ids: HashSet<String> = tracks
        .iter()
        .filter_map(|t| t.song_id.as_deref().filter(|s| !s.is_empty()))
        .map(str::to_string)
        .collect();

    let pick_discovery_candidate =
        |candidates: &[(&str, String)], track: &DiscoveryTrack, used: &HashSet<String>| -> Option<String> {
            let available: Vec<&(&str, String)> = candidates
                .iter()
                .filter(|(id, _)| !used.contains(*id))
                .collect();
            if available.len() == 1 {
                return Some(available[0].0.to_string());
            }
            if available.is_empty() {
                return None;
            }
            // Prefer a candidate whose path contains the track's pack_slug or profile folder.
            let bucket = track
                .pack_slug
                .as_deref()
                .filter(|s| !s.trim().is_empty())
                .unwrap_or(track.profile.as_str());
            let bucket_seg = format!("/{}/", bucket.to_lowercase());
            let narrowed: Vec<&(&str, String)> = available
                .iter()
                .copied()
                .filter(|(_, p)| format!("/{}/", p.to_lowercase()).contains(&bucket_seg))
                .collect();
            if narrowed.len() == 1 {
                return Some(narrowed[0].0.to_string());
            }
            None
        };

    let mut reconciled = 0usize;
    for track in tracks.iter_mut() {
        if !needs_song_id_reconciliation(track) {
            continue;
        }
        let track_norm = normalize_slash_path(&track.path);
        let track_disc_suffix = discovery_suffix(&track_norm);

        // 1. Primary: path match (filtered against already-used song_ids)
        let mut matched_id: Option<String> = exact_local
            .get(&track_norm)
            .filter(|id| !used_song_ids.contains(**id))
            .map(|id| (*id).to_string());

        if matched_id.is_none() {
            // Relative Navidrome path suffix (e.g. `alice/Discovery/Balanced/01.flac` inside `/music/alice/...`)
            let rel_hits: Vec<&str> = relative_discovery
                .iter()
                .filter(|(rel, id)| {
                    !used_song_ids.contains(*id)
                        && (track_norm == *rel
                            || track_norm
                                .strip_suffix(rel.as_str())
                                .is_some_and(|prefix| prefix.ends_with('/')))
                })
                .map(|(_, id)| *id)
                .collect();
            if rel_hits.len() == 1 {
                matched_id = Some(rel_hits[0].to_string());
            }
        }

        if matched_id.is_none() {
            if let Some(suffix) = track_disc_suffix {
                if let Some(cands) = disc_suffix_exact.get(suffix) {
                    matched_id = pick_discovery_candidate(cands, track, &used_song_ids);
                }
                if matched_id.is_none() {
                    if let Some(cands) = disc_suffix_lower.get(&suffix.to_lowercase()) {
                        matched_id = pick_discovery_candidate(cands, track, &used_song_ids);
                    }
                }
            }
        }

        // 2. Secondary: Discovery-scoped (artist, title) match
        if matched_id.is_none() {
            let key = CandidateSet::key(&track.artist, &track.title);
            if let Some(cands) = disc_by_key.get(&key) {
                matched_id = pick_discovery_candidate(cands, track, &used_song_ids);
            }
        }

        // 3. Fallback: unique (artist, title) across library ONLY when ReportRealPath is disabled
        // (i.e. library carries no Discovery/ paths AND no real filesystem paths).
        if matched_id.is_none() && !any_discovery_path && !library_has_real_paths {
            let key = CandidateSet::key(&track.artist, &track.title);
            if let Some(ids) = all_by_key.get(&key) {
                if ids.len() == 1 && !used_song_ids.contains(ids[0]) {
                    matched_id = Some(ids[0].to_string());
                }
            }
        }

        if let Some(song_id) = matched_id {
            if let Err(e) = DiscoveryTrackRow::update_song_id_in(db, &track.id, &song_id).await {
                warn!(
                    "Failed to persist reconciled song_id '{}' for discovery track '{}': {}",
                    song_id, track.id, e
                );
            }
            used_song_ids.insert(song_id.clone());
            track.song_id = Some(song_id);
            reconciled += 1;
        }
    }

    reconciled
}

/// Lazily fetches the caller's Navidrome library ONLY when at least one pending track in `tracks`
/// is missing its `song_id`, then runs `reconcile_discovery_song_ids_in`. Gracefully skips when
/// Navidrome is unlinked or unreachable so listing endpoints never fail.
#[cfg(test)]
pub(crate) async fn maybe_reconcile_discovery_song_ids_in(
    db: &SqlitePool,
    navidrome: &dyn NavidromeOf,
    music_path: Option<&Path>,
    user_id: &str,
    tracks: &mut [DiscoveryTrack],
) -> usize {
    if !tracks.iter().any(needs_song_id_reconciliation) {
        return 0;
    }
    let navi = match navidrome.navidrome_of(&UserId::new(user_id)).await {
        Ok(client) => client,
        Err(NavidromeLookupError::NotLinked | NavidromeLookupError::NoServer) => return 0,
        Err(e) => {
            warn!(
                "Could not resolve Navidrome client to reconcile discovery song_ids for user {}: {}",
                user_id, e
            );
            return 0;
        }
    };
    let library = match fetch_library_snapshot_cached(user_id, &navi).await {
        Ok(songs) => songs,
        Err(e) => {
            warn!(
                "Could not fetch Navidrome songs to reconcile discovery song_ids for user {}: {}",
                user_id, e
            );
            return 0;
        }
    };
    clear_stale_discovery_song_ids_in(db, &library, tracks).await;
    reconcile_discovery_song_ids_in(db, &library, music_path, tracks).await
}

/// Reconciles missing `song_id` values on pending `tracks` AND hydrates live `rating` values from
/// Navidrome (falling back to DB `row.rating`). Gracefully skips when Navidrome is unlinked or
/// unreachable so listing endpoints never fail.
pub(crate) async fn reconcile_and_hydrate_discovery_tracks_in(
    db: &SqlitePool,
    navidrome: &dyn NavidromeOf,
    music_path: Option<&Path>,
    user_id: &str,
    tracks: &mut [DiscoveryTrack],
) {
    if tracks.is_empty() {
        return;
    }
    for track in tracks.iter_mut() {
        track.rating = normalized_rating(track.rating);
    }
    let navi = match navidrome.navidrome_of(&UserId::new(user_id)).await {
        Ok(client) => client,
        Err(NavidromeLookupError::NotLinked | NavidromeLookupError::NoServer) => return,
        Err(e) => {
            warn!(
                "Could not resolve Navidrome client to hydrate discovery tracks for user {}: {}",
                user_id, e
            );
            return;
        }
    };
    let library = match fetch_library_snapshot_cached(user_id, &navi).await {
        Ok(songs) => songs,
        Err(e) => {
            warn!(
                "Could not fetch Navidrome songs to hydrate discovery tracks for user {}: {}",
                user_id, e
            );
            return;
        }
    };
    clear_stale_discovery_song_ids_in(db, &library, tracks).await;
    if tracks.iter().any(needs_song_id_reconciliation) {
        reconcile_discovery_song_ids_in(db, &library, music_path, tracks).await;
    }
    hydrate_discovery_ratings(&library, tracks);
}

/// Lists all contextual mix definitions visible to `ctx` (built-ins + global + caller's custom mixes),
/// with the caller's `enabled_override` applied to `enabled`.
pub(crate) async fn list_mix_definitions_in(
    db: &SqlitePool,
    ctx: &UserCtx,
) -> Result<Vec<ContextualMixDefinition>, Failure> {
    let mut defs = ContextualMixDefinitionRow::list_visible_for_user_in(db, &ctx.user_id)
        .await
        .map_err(|e| database_failure(Stage::Input, e))?;
    let states = ContextualMixStateRow::list_for_user_in(db, &ctx.user_id)
        .await
        .map_err(|e| database_failure(Stage::Input, e))?;
    let overrides: HashMap<&str, bool> = states
        .iter()
        .filter_map(|s| s.enabled_override.map(|v| (s.slug.as_str(), v != 0)))
        .collect();
    for def in &mut defs {
        if let Some(&ov) = overrides.get(def.slug.as_str()) {
            def.enabled = ov;
        }
    }
    Ok(defs)
}

/// Creates or updates a contextual mix definition.
/// - Non-admins create/update private mixes (`owner_user_id = Some(ctx.user_id)`).
/// - Admins may also create/update global mixes (`req.global == true`) or edit built-in parameters.
pub(crate) async fn upsert_mix_definition_in(
    db: &SqlitePool,
    ctx: &UserCtx,
    req: UpsertContextualMixRequest,
) -> Result<ContextualMixDefinition, Failure> {
    if req.kind == ContextualMixKind::Rolling {
        return Err(Failure::new(
            FailureCode::InvalidInput,
            Stage::Input,
            RetryAdvice::NotRetryable,
            "Custom contextual mixes must have kind 'seasonal' or 'moment'",
        ));
    }
    if req.global && !ctx.role.is_admin() {
        return Err(Failure::new(
            FailureCode::PermissionNotGranted,
            Stage::Input,
            RetryAdvice::NotRetryable,
            crate::server_fns::guard::ADMIN_ONLY,
        ));
    }

    if let Some(ref cal) = req.calendar_window {
        if !cal.is_valid() {
            return Err(Failure::new(
                FailureCode::InvalidInput,
                Stage::Input,
                RetryAdvice::NotRetryable,
                "Invalid calendar_window: months must be 1..=12 and days valid for their month",
            ));
        }
    }
    if let Some(ref hw) = req.hour_window {
        if !hw.is_valid() {
            return Err(Failure::new(
                FailureCode::InvalidInput,
                Stage::Input,
                RetryAdvice::NotRetryable,
                "Invalid hour_window: hours must be in 0..=23",
            ));
        }
    }
    if req.days_of_week.iter().any(|&d| !(1..=7).contains(&d)) {
        return Err(Failure::new(
            FailureCode::InvalidInput,
            Stage::Input,
            RetryAdvice::NotRetryable,
            "Invalid days_of_week: values must be ISO weekdays in 1..=7",
        ));
    }
    let mut days_of_week = req.days_of_week;
    days_of_week.sort_unstable();
    days_of_week.dedup();

    if let Some(ref rules) = req.selection_rules {
        if let Some((start, end)) = rules.year_range {
            if start > end {
                return Err(Failure::new(
                    FailureCode::InvalidInput,
                    Stage::Input,
                    RetryAdvice::NotRetryable,
                    "Invalid selection_rules.year_range: start year must be <= end year",
                ));
            }
        }
        if let Some(min_rating) = rules.min_rating {
            if !(1..=5).contains(&min_rating) {
                return Err(Failure::new(
                    FailureCode::InvalidInput,
                    Stage::Input,
                    RetryAdvice::NotRetryable,
                    "Invalid selection_rules.min_rating: must be in 1..=5",
                ));
            }
        }
        const MAX_RULE_DAYS: u32 = 36_500;
        if let Some(days) = rules.not_played_days {
            if days > MAX_RULE_DAYS {
                return Err(Failure::new(
                    FailureCode::InvalidInput,
                    Stage::Input,
                    RetryAdvice::NotRetryable,
                    format!(
                        "Invalid selection_rules.not_played_days: must be <= {MAX_RULE_DAYS}"
                    ),
                ));
            }
        }
        if let Some(days) = rules.added_within_days {
            if days > MAX_RULE_DAYS {
                return Err(Failure::new(
                    FailureCode::InvalidInput,
                    Stage::Input,
                    RetryAdvice::NotRetryable,
                    format!(
                        "Invalid selection_rules.added_within_days: must be <= {MAX_RULE_DAYS}"
                    ),
                ));
            }
        }
    }

    let slug = canonical_slug(&req.slug)?;
    let existing = ContextualMixDefinitionRow::get_by_slug_in(db, &slug)
        .await
        .map_err(|e| database_failure(Stage::Input, e))?;

    if let Some(ref ex) = existing {
        let is_owner = ex.owner_user_id.as_deref() == Some(ctx.user_id.as_str());
        if !is_owner && !ctx.role.is_admin() {
            return Err(Failure::new(
                FailureCode::PermissionNotGranted,
                Stage::Input,
                RetryAdvice::NotRetryable,
                format!("Mix '{slug}' is unavailable or cannot be modified"),
            ));
        }
    }

    let built_in = existing.as_ref().is_some_and(|e| e.built_in);
    let i18n_key = existing
        .as_ref()
        .map(|e| e.i18n_key.clone())
        .unwrap_or_else(|| format!("mix.custom.{slug}"));
    let owner_user_id = if built_in || req.global {
        None
    } else if let Some(ref ex) = existing {
        ex.owner_user_id
            .clone()
            .or_else(|| Some(ctx.user_id.clone()))
    } else {
        Some(ctx.user_id.clone())
    };
    let enabled = req
        .enabled
        .unwrap_or_else(|| existing.as_ref().is_none_or(|e| e.enabled));

    let mut weather_conditions: Vec<WeatherCondition> = Vec::new();
    for cond in req.weather_conditions {
        if !weather_conditions.contains(&cond) {
            weather_conditions.push(cond);
        }
    }

    let def = ContextualMixDefinition {
        slug,
        owner_user_id,
        kind: req.kind,
        i18n_key,
        custom_name: req
            .custom_name
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty()),
        custom_description: req
            .custom_description
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty()),
        built_in,
        enabled,
        theme_tags: req
            .theme_tags
            .into_iter()
            .map(|t| t.trim().to_ascii_lowercase())
            .filter(|t| !t.is_empty())
            .collect(),
        international_queries: req
            .international_queries
            .into_iter()
            .map(|q| q.trim().to_string())
            .filter(|q| !q.is_empty())
            .collect(),
        calendar_window: req.calendar_window,
        hour_window: req.hour_window,
        days_of_week,
        weather_conditions,
        always_surfaced: req.always_surfaced,
        countries: req
            .countries
            .into_iter()
            .map(|c| c.trim().to_ascii_uppercase())
            .filter(|c| !c.is_empty())
            .collect(),
        owned_ratio: req.owned_ratio.clamp(0.0, 1.0),
        default_local_ratio: req.default_local_ratio.map(|r| r.clamp(0.0, 1.0)),
        exclude_from_core_taste: req.exclude_from_core_taste,
        offline_priority: req.offline_priority,
        target_tracks: req.target_tracks.clamp(5, 200),
        sync_to_navidrome: req.sync_to_navidrome,
        selection_rules: req.selection_rules,
    };

    ContextualMixDefinitionRow::upsert_in(db, &def)
        .await
        .map_err(|e| database_failure(Stage::Input, e))
}

/// Deletes a non-built-in custom contextual mix definition owned by `ctx` (or global if admin).
pub(crate) async fn delete_mix_definition_in(
    db: &SqlitePool,
    ctx: &UserCtx,
    slug: &str,
) -> Result<(), Failure> {
    let slug = canonical_slug(slug)?;
    let existing = ContextualMixDefinitionRow::get_by_slug_in(db, &slug)
        .await
        .map_err(|e| database_failure(Stage::Input, e))?
        .ok_or_else(|| {
            Failure::new(
                FailureCode::UnknownHandle,
                Stage::Input,
                RetryAdvice::NotRetryable,
                format!("Contextual mix '{slug}' not found"),
            )
        })?;

    if existing.built_in {
        return Err(Failure::new(
            FailureCode::InvalidInput,
            Stage::Input,
            RetryAdvice::NotRetryable,
            "Built-in mixes cannot be deleted; disable them for your account instead",
        ));
    }

    let is_owner = existing.owner_user_id.as_deref() == Some(ctx.user_id.as_str());
    if !is_owner && !ctx.role.is_admin() {
        return Err(Failure::new(
            FailureCode::PermissionNotGranted,
            Stage::Input,
            RetryAdvice::NotRetryable,
            "Only the mix owner or an admin can delete this mix",
        ));
    }

    ContextualMixDefinitionRow::delete_by_slug_in(db, &slug)
        .await
        .map_err(|e| database_failure(Stage::Input, e))?;
    Ok(())
}

/// Enables or disables any visible contextual mix for the caller without affecting other users.
pub(crate) async fn set_mix_enabled_in(
    db: &SqlitePool,
    ctx: &UserCtx,
    req: SetMixEnabledRequest,
) -> Result<ContextualMixDefinition, Failure> {
    let slug = canonical_slug(&req.slug)?;
    let mut def = ContextualMixDefinitionRow::get_visible_by_slug_in(db, &ctx.user_id, &slug)
        .await
        .map_err(|e| database_failure(Stage::Input, e))?
        .ok_or_else(|| {
            Failure::new(
                FailureCode::UnknownHandle,
                Stage::Input,
                RetryAdvice::NotRetryable,
                format!("Contextual mix '{slug}' not found"),
            )
        })?;
    ContextualMixStateRow::upsert_enabled_override_in(db, &ctx.user_id, &slug, req.enabled)
        .await
        .map_err(|e| database_failure(Stage::Input, e))?;
    def.enabled = req.enabled;
    Ok(def)
}

/// Tokenizes text with diacritic folding and word-boundary splitting (keeping apostrophes joined),
/// so `"Chants de Noël"` -> `["chants", "de", "noel"]` and `"X-Mas"` -> `["x", "mas"]`.
fn fold_tokens(s: &str) -> Vec<String> {
    let folded = shared::recommendation::without_marks(s).to_lowercase();
    let mut tokens = Vec::new();
    let mut cur = String::new();
    for ch in folded.chars() {
        if ch.is_alphanumeric() {
            cur.push(ch);
        } else if matches!(ch, '\'' | '\u{2018}' | '\u{2019}') {
            // Keep contractions/possessives joined.
        } else if !cur.is_empty() {
            tokens.push(std::mem::take(&mut cur));
        }
    }
    if !cur.is_empty() {
        tokens.push(cur);
    }
    tokens
}

fn token_matches_word(token: &str, keyword: &str) -> bool {
    if token == keyword {
        return true;
    }
    match keyword {
        "weihnachten" | "weihnacht" => token.starts_with("weihnacht"),
        "kerst" => token.starts_with("kerst"),
        "jul" => {
            let base = token.strip_suffix('s').unwrap_or(token);
            token.starts_with("jul")
                && !matches!(
                    base,
                    "july" | "julia" | "julian" | "juliet" | "julius" | "jule" | "julie"
                )
        }
        _ => false,
    }
}

fn tokens_contain_phrase(haystack: &[String], needle: &[String]) -> bool {
    if haystack.is_empty() || needle.is_empty() || needle.len() > haystack.len() {
        return false;
    }
    if needle.len() == 1 {
        let kw = &needle[0];
        return haystack.iter().any(|tok| token_matches_word(tok, kw));
    }
    haystack.windows(needle.len()).any(|win| win == needle)
}

/// Generic genre/playlist filler words that must not trigger title/album/artist matching on their own.
fn is_generic_text_token(word: &str) -> bool {
    matches!(
        word,
        "hits"
            | "classics"
            | "classic"
            | "songs"
            | "song"
            | "music"
            | "mix"
            | "playlist"
            | "party"
            | "celebration"
            | "fresh"
            | "anthems"
            | "chill"
            | "cozy"
            | "night"
            | "floor"
            | "pop"
            | "rock"
            | "indie"
            | "acoustic"
            | "dance"
            | "electronic"
            | "disco"
            | "funk"
            | "jazz"
            | "folk"
            | "soul"
            | "reggae"
            | "tropical"
            | "dream"
            | "post"
            | "punk"
            | "synthwave"
            | "darkwave"
            | "samba"
            | "salsa"
            | "cumbia"
            | "afrobeats"
            | "mandopop"
            | "cantopop"
            | "kpop"
            | "cpop"
            | "asian"
            | "indian"
            | "bollywood"
            | "bhangra"
            | "festive"
            | "sun"
            | "soaked"
            | "bossa"
            | "nova"
            | "ambient"
            | "chanson"
            | "electro"
            | "shoegaze"
            | "synthpop"
            | "alternative"
            | "trad"
            | "late"
            | "love"
            | "french"
            | "touch"
            | "patrick"
            | "festival"
            | "r"
            | "b"
            | "and"
            | "the"
            | "of"
            | "de"
            | "la"
            | "le"
            | "du"
            | "des"
            | "in"
            | "for"
    )
}

struct SeasonalMatchers {
    genre_phrases: Vec<(String, Vec<String>)>,
    text_phrases: Vec<(String, Vec<String>)>,
    artist_phrases: Vec<(String, Vec<String>)>,
}

impl SeasonalMatchers {
    fn for_definition(def: &ContextualMixDefinition) -> Self {
        let mut genre_phrases: Vec<(String, Vec<String>)> = Vec::new();
        let mut text_phrases: Vec<(String, Vec<String>)> = Vec::new();
        let mut artist_phrases: Vec<(String, Vec<String>)> = Vec::new();

        let push_unique = |list: &mut Vec<(String, Vec<String>)>, raw: &str| {
            let tokens = fold_tokens(raw);
            if tokens.is_empty() {
                return;
            }
            if !list.iter().any(|(_, existing)| existing == &tokens) {
                list.push((raw.to_string(), tokens));
            }
        };

        for tag in &def.theme_tags {
            let tokens = fold_tokens(tag);
            if tokens.is_empty() {
                continue;
            }
            push_unique(&mut genre_phrases, tag);
            let is_genre_only = tokens.iter().all(|t| is_generic_text_token(t))
                || matches!(
                    tag.to_ascii_lowercase().as_str(),
                    "winter"
                        | "spring"
                        | "summer"
                        | "autumn"
                        | "holiday"
                        | "holidays"
                        | "r&b"
                        | "romance"
                );
            if !is_genre_only {
                push_unique(&mut text_phrases, tag);
                push_unique(&mut artist_phrases, tag);
            }
        }

        // Extract meaningful non-generic keywords from international_queries (e.g. "Christmas", "Halloween").
        for query in &def.international_queries {
            for tok in fold_tokens(query) {
                if tok.chars().count() >= 4
                    && !is_generic_text_token(&tok)
                    && !matches!(tok.as_str(), "winter" | "spring" | "summer" | "autumn")
                {
                    push_unique(&mut genre_phrases, &tok);
                    if !matches!(tok.as_str(), "holiday" | "holidays") {
                        push_unique(&mut text_phrases, &tok);
                    }
                }
            }
        }

        let is_winter_holiday = def.slug == "winter_holiday"
            || def
                .theme_tags
                .iter()
                .any(|t| t.eq_ignore_ascii_case("christmas"));
        if is_winter_holiday {
            // Bare "holiday"/"holidays" match genre tags only, never artist/title/album
            // (prevents false positives like Billie Holiday, The Holidays, Green Day "Holiday",
            // Madonna "Holiday").
            const GENRE_ONLY_HOLIDAY: &[&str] = &["holiday", "holidays"];
            const ARTIST_SAFE_HOLIDAY: &[&str] = &[
                "christmas",
                "xmas",
                "santa",
                "santa claus",
                "jingle",
                "jingle bell",
                "jingle bells",
                "silent night",
                "winter wonderland",
                "advent",
                "feliz navidad",
                "navidad",
                "weihnachten",
                "weihnacht",
                "natale",
                "kerst",
                "yuletide",
                "sleigh ride",
                "mistletoe",
                "rudolph",
                "reindeer",
                "stille nacht",
                "white christmas",
                "frosty the snowman",
                "papa noel",
                "chants de noel",
            ];
            // Title/album/genre keywords (excluded from bare artist matching to avoid false
            // positives like "Noel Gallagher", and using specific phrases instead of bare
            // "carol" / "holiday" to avoid Chuck Berry "Carol" or Green Day "Holiday").
            const TITLE_ALBUM_HOLIDAY: &[&str] = &[
                "noel",
                "christmas carol",
                "christmas carols",
                "carol of the bells",
                "holiday carols",
                "christmas holiday",
                "winter holiday",
                "holiday classics",
                "holiday music",
                "holiday favorites",
                "sleigh",
                "yule",
                "nutcracker",
                "holy night",
                "deck the halls",
                "joy to the world",
                "little drummer boy",
                "vive le vent",
                "mon beau sapin",
                "douce nuit",
                "auld lang syne",
            ];
            for kw in GENRE_ONLY_HOLIDAY {
                push_unique(&mut genre_phrases, kw);
            }
            for kw in ARTIST_SAFE_HOLIDAY {
                push_unique(&mut genre_phrases, kw);
                push_unique(&mut text_phrases, kw);
                push_unique(&mut artist_phrases, kw);
            }
            for kw in TITLE_ALBUM_HOLIDAY {
                push_unique(&mut genre_phrases, kw);
                push_unique(&mut text_phrases, kw);
            }
        } else if def.slug == "valentines" {
            // Match Valentine/romance phrases on genre, title, and album only (never bare artist
            // names, preventing false positives like My Bloody Valentine, Bullet for My Valentine,
            // and My Chemical Romance).
            const TITLE_ALBUM_VALENTINES: &[&str] = &[
                "valentine",
                "valentines",
                "my valentine",
                "saint valentin",
                "love song",
                "love songs",
                "romance",
                "romantic",
                "chanson damour",
            ];
            for kw in TITLE_ALBUM_VALENTINES {
                push_unique(&mut genre_phrases, kw);
                push_unique(&mut text_phrases, kw);
            }
        } else {
            let extra: &[&str] = match def.slug.as_str() {
                "cozy_winter" => &[
                    "wintertime",
                    "midwinter",
                    "snowfall",
                    "fireside",
                    "fireplace",
                    "winter acoustic",
                    "cozy winter",
                    "winter song",
                    "winter songs",
                ],
                "celtic_spring" => &[
                    "celtic",
                    "irish",
                    "st patrick",
                    "saint patrick",
                    "shamrock",
                    "gaelic",
                    "celtic folk",
                    "irish folk",
                    "emerald isle",
                ],
                "summer_solstice" => &[
                    "solstice",
                    "summer solstice",
                    "midsummer",
                    "midsommar",
                    "fete de la musique",
                    "saint jean",
                ],
                "late_summer_reset" => &[
                    "late summer",
                    "end of summer",
                    "indian summer",
                    "september",
                    "back to school",
                    "rentree",
                ],
                "halloween" => &[
                    "halloween",
                    "spooky",
                    "trick or treat",
                    "monster mash",
                    "ghostbusters",
                    "haunted",
                ],
                "new_years_eve" => &[
                    "new year",
                    "new years",
                    "auld lang syne",
                    "bonne annee",
                ],
                "lunar_new_year" => &["lunar new year", "chinese new year", "spring festival"],
                "carnival" => &["carnival", "carnaval", "mardi gras"],
                _ => &[],
            };
            for kw in extra {
                push_unique(&mut genre_phrases, kw);
                push_unique(&mut text_phrases, kw);
                push_unique(&mut artist_phrases, kw);
            }
        }

        Self {
            genre_phrases,
            text_phrases,
            artist_phrases,
        }
    }

    /// Returns the matched seasonal keyword/tag if `song` genuinely belongs to this seasonal theme,
    /// using strict word-boundary matching across all genres, title, album, and artist.
    fn matches(&self, song: &SubsonicSong) -> Option<String> {
        for genre in song.all_genres() {
            let tokens = fold_tokens(genre);
            for (label, needle) in &self.genre_phrases {
                if tokens_contain_phrase(&tokens, needle) {
                    return Some(label.clone());
                }
            }
        }
        let title_tokens = fold_tokens(&song.title);
        for (label, needle) in &self.text_phrases {
            if tokens_contain_phrase(&title_tokens, needle) {
                return Some(label.clone());
            }
        }
        if let Some(album) = song.album.as_deref() {
            let album_tokens = fold_tokens(album);
            for (label, needle) in &self.text_phrases {
                if tokens_contain_phrase(&album_tokens, needle) {
                    return Some(label.clone());
                }
            }
        }
        if let Some(artist) = song.artist.as_deref() {
            let artist_tokens = fold_tokens(artist);
            for (label, needle) in &self.artist_phrases {
                if tokens_contain_phrase(&artist_tokens, needle) {
                    return Some(label.clone());
                }
            }
        }
        None
    }
}

/// Pure-Rust deterministic 64-bit FNV-1a hash with SplitMix64 finalizer so daily track ordering
/// rotates cleanly across calendar dates while remaining stable within the same day and user.
fn daily_track_hash(user_id: &str, mix_slug: &str, date_key: &str, song_id: &str) -> u64 {
    const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const FNV_PRIME: u64 = 0x0100_0000_01b3;
    let mut hash = FNV_OFFSET;
    for part in [user_id, mix_slug, date_key, song_id] {
        for &b in part.as_bytes() {
            hash ^= u64::from(b);
            hash = hash.wrapping_mul(FNV_PRIME);
        }
        hash ^= 0xff;
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash ^= hash >> 30;
    hash = hash.wrapping_mul(0xbf58_476d_1ce4_e5b9);
    hash ^= hash >> 27;
    hash = hash.wrapping_mul(0x94d0_49bb_1331_11eb);
    hash ^= hash >> 31;
    hash
}

/// Parses a Navidrome RFC3339 timestamp into UTC.
fn parse_rfc3339_utc(raw: Option<&str>) -> Option<chrono::DateTime<Utc>> {
    raw.map(str::trim)
        .filter(|s| !s.is_empty())
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
        .map(|dt| dt.with_timezone(&Utc))
}

/// Evaluates optional structured `MixSelectionRules` against an owned library song.
fn matches_selection_rules(
    song: &SubsonicSong,
    rules: &MixSelectionRules,
    now: chrono::DateTime<Utc>,
) -> bool {
    if let Some((min_y, max_y)) = rules.year_range {
        if !song
            .year
            .is_some_and(|y| y >= u32::from(min_y) && y <= u32::from(max_y))
        {
            return false;
        }
    }
    if let Some(days) = rules.not_played_days {
        let Some(cutoff) = now.checked_sub_signed(chrono::Duration::days(i64::from(days))) else {
            return false;
        };
        if !parse_rfc3339_utc(song.played.as_deref()).is_some_and(|dt| dt <= cutoff) {
            return false;
        }
    }
    match (rules.min_rating, rules.min_play_count) {
        (Some(min_r), Some(min_p)) => {
            let rating_ok = normalized_rating(song.user_rating).is_some_and(|r| r >= min_r);
            let plays_ok = song.play_count.unwrap_or(0) >= min_p;
            let starred_ok = song.starred.is_some();
            if !(starred_ok || rating_ok || plays_ok) {
                return false;
            }
        }
        (Some(min_r), None) => {
            let rating_ok = normalized_rating(song.user_rating).is_some_and(|r| r >= min_r);
            let starred_ok = song.starred.is_some();
            if !(starred_ok || rating_ok) {
                return false;
            }
        }
        (None, Some(min_p)) => {
            let plays_ok = song.play_count.unwrap_or(0) >= min_p;
            let starred_ok = song.starred.is_some();
            if !(starred_ok || plays_ok) {
                return false;
            }
        }
        (None, None) => {}
    }
    if let Some(days) = rules.added_within_days {
        let Some(cutoff) = now.checked_sub_signed(chrono::Duration::days(i64::from(days))) else {
            return false;
        };
        if !parse_rfc3339_utc(song.created.as_deref()).is_some_and(|dt| dt >= cutoff) {
            return false;
        }
    }
    if let Some(max_p) = rules.max_play_count {
        if song.play_count.unwrap_or(0) > max_p {
            return false;
        }
    }
    true
}

/// Sorts owned candidate songs according to `sort_order` with daily deterministic rotation jitter
/// inside coarse tiers/windows so favorites stay near the top while order refreshes each day.
fn sort_owned_candidates<T>(
    items: &mut [(&SubsonicSong, T)],
    sort_order: MixSortOrder,
    user_id: &str,
    mix_slug: &str,
    date_key: &str,
) {
    match sort_order {
        MixSortOrder::RatingThenPlays => {
            items.sort_by_key(|(s, _)| {
                let plays = s.play_count.unwrap_or(0);
                let tier: u8 = match normalized_rating(s.user_rating) {
                    Some(5) => 0,
                    Some(4) => 1,
                    None if s.starred.is_some() => 1,
                    _ if plays >= 5 => 2,
                    _ if plays >= 1 => 3,
                    _ => 4,
                };
                (tier, daily_track_hash(user_id, mix_slug, date_key, &s.id))
            });
        }
        MixSortOrder::OldestPlayedFirst => {
            items.sort_by_key(|(s, _)| {
                let window = parse_rfc3339_utc(s.played.as_deref())
                    .map(|dt| dt.timestamp().div_euclid(30 * 86_400))
                    .unwrap_or(i64::MAX);
                (window, daily_track_hash(user_id, mix_slug, date_key, &s.id))
            });
        }
        MixSortOrder::NewestAddedFirst => {
            items.sort_by_key(|(s, _)| {
                let window = parse_rfc3339_utc(s.created.as_deref())
                    .map(|dt| dt.timestamp().div_euclid(7 * 86_400))
                    .unwrap_or(i64::MIN);
                (
                    std::cmp::Reverse(window),
                    daily_track_hash(user_id, mix_slug, date_key, &s.id),
                )
            });
        }
    }
}

/// Builds the exact playable blended tracklist for `def` from owned `library` songs and staged
/// `pending` discovery tracks using the mix's `owned_ratio`, `theme_tags`, `selection_rules`,
/// daily deterministic jitter, and `MAX_TRACKS_PER_ARTIST` cap. Shared by both
/// `list_active_contextual_mixes_in` (for exact playable `track_count`) and
/// `resolve_contextual_mix_playlist_in` so card counts always match the resolved playlist.
fn build_contextual_mix_tracks(
    user_id: &str,
    def: &ContextualMixDefinition,
    library: Option<&[SubsonicSong]>,
    pending: &[DiscoveryTrack],
    now: chrono::DateTime<Utc>,
) -> Vec<ContextualMixTrack> {
    let target = def.target_tracks as usize;
    let date_key = now.format("%Y-%m-%d").to_string();
    let song_by_id: HashMap<&str, &SubsonicSong> = library
        .map(|songs| songs.iter().map(|s| (s.id.as_str(), s)).collect())
        .unwrap_or_default();

    let mut owned_tracks: Vec<ContextualMixTrack> = Vec::new();
    let mut fresh_tracks: Vec<ContextualMixTrack> = Vec::new();
    let mut seen_keys: HashSet<String> = HashSet::new();
    let mut artist_counts: HashMap<String, usize> = HashMap::new();

    if def.kind == ContextualMixKind::Seasonal {
        // Seasonal mixes prioritize staged `pack_slug` discovery tracks and also match owned
        // holiday/seasonal library tracks strictly (never falling back to unrelated songs),
        // so users with existing holiday tracks get an immediate playable mix.
        for dt in pending
            .iter()
            .filter(|t| t.pack_slug.as_deref() == Some(def.slug.as_str()))
        {
            if fresh_tracks.len() >= target {
                break;
            }
            let Some(song_id) = dt.song_id.as_deref().filter(|s| !s.is_empty()) else {
                continue;
            };
            let nav_song = song_by_id.get(song_id).copied();
            if library.is_some() && nav_song.is_none() {
                continue;
            }
            let key = CandidateSet::key(&dt.artist, &dt.title);
            if !seen_keys.insert(key) {
                continue;
            }
            let artist_key = discovery_name(&dt.artist);
            let count = artist_counts.entry(artist_key).or_insert(0);
            if *count >= MAX_TRACKS_PER_ARTIST {
                continue;
            }
            *count += 1;
            let bucket = dt.culture_bucket.as_deref().and_then(|b| match b {
                "local" => Some(CultureBucket::Local),
                "international" => Some(CultureBucket::International),
                "taste_wildcard" => Some(CultureBucket::TasteWildcard),
                _ => None,
            });
            fresh_tracks.push(ContextualMixTrack {
                song_id: song_id.to_string(),
                discovery_track_id: Some(dt.id.clone()),
                artist: dt.artist.clone(),
                title: dt.title.clone(),
                album: (!dt.album.is_empty()).then(|| dt.album.clone()),
                duration_s: nav_song.and_then(|s| s.duration.filter(|&d| d > 0)),
                owned: false,
                rating: nav_song
                    .and_then(|s| normalized_rating(s.user_rating))
                    .or_else(|| normalized_rating(dt.rating)),
                starred: nav_song.is_some_and(|s| s.starred.is_some()),
                reason: dt.reason.as_deref().cloned().or_else(|| {
                    Some(CandidateReason::InternationalContextual {
                        mix_slug: def.slug.clone(),
                    })
                }),
                culture_bucket: bucket,
            });
        }

        let owned_cap = if def.owned_ratio > 0.0 {
            ((target as f32) * def.owned_ratio).round() as usize
        } else {
            target.saturating_sub(fresh_tracks.len())
        };

        if owned_cap > 0 {
            if let Some(library) = library {
                let pending_song_ids: HashSet<&str> = pending
                    .iter()
                    .filter_map(|t| t.song_id.as_deref().filter(|s| !s.is_empty()))
                    .collect();
                let pending_keys: HashSet<String> = pending
                    .iter()
                    .map(|t| CandidateSet::key(&t.artist, &t.title))
                    .collect();

                let matchers = SeasonalMatchers::for_definition(def);
                let mut matched = Vec::new();
                for song in library {
                    if matches!(song.user_rating, Some(1..=2)) {
                        continue;
                    }
                    if pending_song_ids.contains(song.id.as_str()) {
                        continue;
                    }
                    let artist_ref = song.artist.as_deref().unwrap_or("");
                    if pending_keys.contains(&CandidateSet::key(artist_ref, &song.title)) {
                        continue;
                    }
                    if song.path.as_deref().is_some_and(is_staged_discovery_path) {
                        continue;
                    }
                    if let Some(matched_tag) = matchers.matches(song) {
                        matched.push((song, matched_tag));
                    }
                }

                let sort_order = def
                    .selection_rules
                    .as_ref()
                    .map(|r| r.sort_order)
                    .unwrap_or_default();
                sort_owned_candidates(&mut matched, sort_order, user_id, &def.slug, &date_key);

                for (song, matched_tag) in matched {
                    if owned_tracks.len() >= owned_cap {
                        break;
                    }
                    let artist = song.artist.clone().unwrap_or_default();
                    let key = CandidateSet::key(&artist, &song.title);
                    if !seen_keys.insert(key) {
                        continue;
                    }
                    let artist_key = discovery_name(&artist);
                    let count = artist_counts.entry(artist_key).or_insert(0);
                    if *count >= MAX_TRACKS_PER_ARTIST {
                        continue;
                    }
                    *count += 1;
                    owned_tracks.push(ContextualMixTrack {
                        song_id: song.id.clone(),
                        discovery_track_id: None,
                        artist,
                        title: song.title.clone(),
                        album: song.album.clone(),
                        duration_s: song.duration,
                        owned: true,
                        rating: normalized_rating(song.user_rating),
                        starred: song.starred.is_some(),
                        reason: Some(CandidateReason::LibraryMatch {
                            mix_slug: def.slug.clone(),
                            matched_genre: Some(matched_tag),
                        }),
                        culture_bucket: Some(CultureBucket::OwnedLibrary),
                    });
                }
            }
        }
    } else {
        let owned_target = ((target as f32) * def.owned_ratio).round() as usize;
        let fresh_target = target.saturating_sub(owned_target);
        let strict_match = def.selection_rules.as_ref().is_some_and(|r| r.strict_match);

        if owned_target > 0 {
            if let Some(library) = library {
                let pending_song_ids: HashSet<&str> = pending
                    .iter()
                    .filter_map(|t| t.song_id.as_deref().filter(|s| !s.is_empty()))
                    .collect();
                let pending_keys: HashSet<String> = pending
                    .iter()
                    .map(|t| CandidateSet::key(&t.artist, &t.title))
                    .collect();

                let tags_lower: Vec<String> = def
                    .theme_tags
                    .iter()
                    .map(|t| t.to_ascii_lowercase())
                    .collect();

                // Prefer owned library songs matching selection_rules and theme_tags, excluding
                // disliked tracks and staged Discovery/ or pending discovery_tracks rows.
                let mut matched = Vec::new();
                let mut fallback = Vec::new();
                for song in library {
                    if matches!(song.user_rating, Some(1..=2)) {
                        continue;
                    }
                    if pending_song_ids.contains(song.id.as_str()) {
                        continue;
                    }
                    let artist_ref = song.artist.as_deref().unwrap_or("");
                    if pending_keys.contains(&CandidateSet::key(artist_ref, &song.title)) {
                        continue;
                    }
                    if song.path.as_deref().is_some_and(is_staged_discovery_path) {
                        continue;
                    }
                    if let Some(ref rules) = def.selection_rules {
                        if !matches_selection_rules(song, rules, now) {
                            continue;
                        }
                    }

                    if tags_lower.is_empty() {
                        matched.push((song, None));
                    } else {
                        let hit = song.all_genres().find_map(|g| {
                            let g_lower = g.to_ascii_lowercase();
                            tags_lower
                                .iter()
                                .find(|t| !t.is_empty() && g_lower.contains(t.as_str()))
                                .cloned()
                        });
                        if let Some(matched_genre) = hit {
                            matched.push((song, Some(matched_genre)));
                        } else if !strict_match {
                            fallback.push((song, None));
                        }
                    }
                }

                let sort_order = def
                    .selection_rules
                    .as_ref()
                    .map(|r| r.sort_order)
                    .unwrap_or_default();
                sort_owned_candidates(&mut matched, sort_order, user_id, &def.slug, &date_key);
                sort_owned_candidates(&mut fallback, sort_order, user_id, &def.slug, &date_key);

                let ordered = if strict_match {
                    matched
                } else {
                    matched.into_iter().chain(fallback).collect()
                };

                for (song, matched_genre) in ordered {
                    if owned_tracks.len() >= owned_target {
                        break;
                    }
                    let artist = song.artist.clone().unwrap_or_default();
                    let key = CandidateSet::key(&artist, &song.title);
                    if !seen_keys.insert(key) {
                        continue;
                    }
                    let artist_key = discovery_name(&artist);
                    let count = artist_counts.entry(artist_key).or_insert(0);
                    if *count >= MAX_TRACKS_PER_ARTIST {
                        continue;
                    }
                    *count += 1;
                    owned_tracks.push(ContextualMixTrack {
                        song_id: song.id.clone(),
                        discovery_track_id: None,
                        artist,
                        title: song.title.clone(),
                        album: song.album.clone(),
                        duration_s: song.duration,
                        owned: true,
                        rating: normalized_rating(song.user_rating),
                        starred: song.starred.is_some(),
                        reason: Some(CandidateReason::LibraryMatch {
                            mix_slug: def.slug.clone(),
                            matched_genre,
                        }),
                        culture_bucket: None,
                    });
                }
            }
        }

        // Collect staged fresh tracks: exact pack_slug matches first, then general pending discovery tracks
        // for hybrid moment mixes (never pulling seasonal tracks into non-seasonal mixes, and never
        // pulling unrelated pending tracks into strict_match mixes).
        let staged_iter = pending
            .iter()
            .filter(|t| t.pack_slug.as_deref() == Some(def.slug.as_str()))
            .chain(pending.iter().filter(|t| {
                !strict_match
                    && def.kind == ContextualMixKind::Moment
                    && t.pack_slug.is_none()
                    && !t.exclude_from_core_taste
            }));

        for dt in staged_iter {
            if fresh_tracks.len() >= fresh_target {
                break;
            }
            let Some(song_id) = dt.song_id.as_deref().filter(|s| !s.is_empty()) else {
                continue;
            };
            let nav_song = song_by_id.get(song_id).copied();
            if library.is_some() && nav_song.is_none() {
                continue;
            }
            let key = CandidateSet::key(&dt.artist, &dt.title);
            if !seen_keys.insert(key) {
                continue;
            }
            let artist_key = discovery_name(&dt.artist);
            let count = artist_counts.entry(artist_key).or_insert(0);
            if *count >= MAX_TRACKS_PER_ARTIST {
                continue;
            }
            *count += 1;
            let bucket = dt.culture_bucket.as_deref().and_then(|b| match b {
                "local" => Some(CultureBucket::Local),
                "international" => Some(CultureBucket::International),
                "taste_wildcard" => Some(CultureBucket::TasteWildcard),
                _ => None,
            });
            fresh_tracks.push(ContextualMixTrack {
                song_id: song_id.to_string(),
                discovery_track_id: Some(dt.id.clone()),
                artist: dt.artist.clone(),
                title: dt.title.clone(),
                album: (!dt.album.is_empty()).then(|| dt.album.clone()),
                duration_s: nav_song.and_then(|s| s.duration.filter(|&d| d > 0)),
                owned: false,
                rating: nav_song
                    .and_then(|s| normalized_rating(s.user_rating))
                    .or_else(|| normalized_rating(dt.rating)),
                starred: nav_song.is_some_and(|s| s.starred.is_some()),
                reason: dt.reason.as_deref().cloned().or_else(|| {
                    Some(CandidateReason::MomentMatch {
                        mix_slug: def.slug.clone(),
                        mood_tag: def.primary_tag().map(str::to_string),
                        genre: None,
                    })
                }),
                culture_bucket: bucket,
            });
        }
    }

    // Interleave owned (~3) : fresh (~1) smoothly for hybrid mixes.
    let mut tracks = Vec::with_capacity(owned_tracks.len() + fresh_tracks.len());
    let mut owned_it = owned_tracks.into_iter();
    let mut fresh_it = fresh_tracks.into_iter();
    loop {
        let mut added = false;
        for _ in 0..3 {
            if let Some(t) = owned_it.next() {
                tracks.push(t);
                added = true;
            }
        }
        if let Some(t) = fresh_it.next() {
            tracks.push(t);
            added = true;
        }
        if !added {
            break;
        }
    }
    tracks
}

#[allow(clippy::too_many_arguments)]
fn summarize_definition(
    user_id: &str,
    def: &ContextualMixDefinition,
    culture: &UserCultureProfile,
    state: Option<&ContextualMixStateRow>,
    pending: &[DiscoveryTrack],
    library: Option<&[SubsonicSong]>,
    now: chrono::DateTime<Utc>,
    precomputed_track_count: Option<u32>,
    weather: Option<&WeatherSnapshot>,
) -> ContextualMixSummary {
    let steering = state
        .map(|s| CultureSteering::parse(&s.steering))
        .unwrap_or(CultureSteering::Balanced);
    let effective_local = def
        .effective_base_local_ratio(culture)
        .map(|base_ratio| steering.effective_ratio(base_ratio));
    let local_pct = effective_local.map(|r| (r * 100.0).round().clamp(0.0, 100.0) as u8);
    let owned_pct = (def.owned_ratio * 100.0).round().clamp(0.0, 100.0) as u8;
    let cycle = def
        .calendar_window
        .as_ref()
        .and_then(|w| w.cycle_dates(now.year(), now.month(), now.day()));
    let (active_from, active_until, expires_at) = match cycle {
        Some((from, until, exp)) => (Some(from), Some(until), Some(exp)),
        None => (None, None, state.and_then(|s| s.expires_at.clone())),
    };
    let track_count = precomputed_track_count
        .unwrap_or_else(|| build_contextual_mix_tracks(user_id, def, library, pending, now).len() as u32);
    let weather_badge = if !def.weather_conditions.is_empty() {
        weather
            .filter(|w| def.weather_conditions.contains(&w.condition))
            .cloned()
    } else {
        None
    };

    ContextualMixSummary {
        slug: def.slug.clone(),
        kind: def.kind,
        i18n_key: def.i18n_key.clone(),
        custom_name: def.custom_name.clone(),
        custom_description: def.custom_description.clone(),
        built_in: def.built_in,
        country_code: effective_local
            .and_then(|_| culture.effective_country().map(str::to_string)),
        local_pct,
        owned_pct,
        offline_priority: def.offline_priority,
        exclude_from_core_taste: def.exclude_from_core_taste,
        steering: effective_local.map(|_| steering),
        active_from,
        active_until,
        expires_at,
        navidrome_playlist_id: state.and_then(|s| s.navidrome_playlist_id.clone()),
        weather_badge,
        track_count,
        target_tracks: def.target_tracks,
    }
}

fn rolling_profile_for_slug(slug: &str) -> Option<DiscoveryProfile> {
    match slug {
        "conservative" => Some(DiscoveryProfile::Conservative),
        "balanced" => Some(DiscoveryProfile::Balanced),
        "adventurous" => Some(DiscoveryProfile::Adventurous),
        _ => None,
    }
}

fn build_rolling_mix_tracks(
    profile: DiscoveryProfile,
    library: Option<&[SubsonicSong]>,
    pending: &[DiscoveryTrack],
) -> Vec<ContextualMixTrack> {
    let profile_name = profile.name();
    let song_by_id: HashMap<&str, &SubsonicSong> = library
        .map(|songs| songs.iter().map(|s| (s.id.as_str(), s)).collect())
        .unwrap_or_default();

    let mut seen_keys: HashSet<String> = HashSet::new();
    let mut tracks: Vec<ContextualMixTrack> = Vec::new();
    for dt in pending
        .iter()
        .filter(|t| t.profile.eq_ignore_ascii_case(profile_name) && t.pack_slug.is_none())
    {
        let Some(song_id) = dt.song_id.as_deref().filter(|s| !s.is_empty()) else {
            continue;
        };
        let nav_song = song_by_id.get(song_id).copied();
        if library.is_some() && nav_song.is_none() {
            continue;
        }
        let key = CandidateSet::key(&dt.artist, &dt.title);
        if !seen_keys.insert(key) {
            continue;
        }
        let bucket = dt.culture_bucket.as_deref().and_then(|b| match b {
            "local" => Some(CultureBucket::Local),
            "international" => Some(CultureBucket::International),
            "taste_wildcard" => Some(CultureBucket::TasteWildcard),
            _ => None,
        });
        tracks.push(ContextualMixTrack {
            song_id: song_id.to_string(),
            discovery_track_id: Some(dt.id.clone()),
            artist: dt.artist.clone(),
            title: dt.title.clone(),
            album: (!dt.album.is_empty()).then(|| dt.album.clone()),
            duration_s: nav_song.and_then(|s| s.duration.filter(|&d| d > 0)),
            owned: false,
            rating: nav_song
                .and_then(|s| normalized_rating(s.user_rating))
                .or_else(|| normalized_rating(dt.rating)),
            starred: nav_song.is_some_and(|s| s.starred.is_some()),
            reason: dt.reason.as_deref().cloned(),
            culture_bucket: bucket,
        });
    }
    tracks
}

fn summarize_rolling_profile(
    profile: DiscoveryProfile,
    settings: &UserSettings,
    track_count: u32,
) -> ContextualMixSummary {
    let profile_name = profile.name();
    let slug = profile_name.to_ascii_lowercase();
    let i18n_key = match profile {
        DiscoveryProfile::Conservative => "mix.rolling.conservative",
        DiscoveryProfile::Balanced => "mix.rolling.balanced",
        DiscoveryProfile::Adventurous => "mix.rolling.adventurous",
    };
    let counts = settings.parse_track_counts();
    let target = counts.get(profile_name).copied().unwrap_or(10);
    let navidrome_playlist_id = UserSettings::get_playlist_id_for_profile(
        &settings.discovery_navidrome_playlist_id,
        profile_name,
    );
    ContextualMixSummary {
        slug,
        kind: ContextualMixKind::Rolling,
        i18n_key: i18n_key.to_string(),
        custom_name: None,
        custom_description: None,
        built_in: true,
        country_code: None,
        local_pct: None,
        owned_pct: 0,
        offline_priority: false,
        exclude_from_core_taste: false,
        steering: None,
        active_from: None,
        active_until: None,
        expires_at: None,
        navidrome_playlist_id,
        weather_badge: None,
        track_count,
        target_tracks: target,
    }
}

/// Returns all calendar-active, enabled `ContextualMixKind::Seasonal` definitions for `user_id`
/// at `now`, regardless of whether they currently have tracks staged or matched.
/// Used by seasonal pack generation and the ASAP startup check.
pub(crate) async fn list_calendar_active_seasonal_defs_in(
    db: &SqlitePool,
    user_id: &str,
    now: chrono::DateTime<Utc>,
) -> Result<(UserCultureProfile, Vec<(ContextualMixDefinition, Option<ContextualMixStateRow>)>), String>
{
    let culture = UserCultureProfileRow::get_in(db, user_id)
        .await?
        .unwrap_or_default();
    let defs = ContextualMixDefinitionRow::list_visible_for_user_in(db, user_id).await?;
    let states = ContextualMixStateRow::list_for_user_in(db, user_id).await?;
    let state_by_slug: HashMap<String, ContextualMixStateRow> =
        states.into_iter().map(|s| (s.slug.clone(), s)).collect();

    let effective_country = culture.effective_country().map(str::to_string);
    let mut active = Vec::new();
    for def in defs {
        if def.kind != ContextualMixKind::Seasonal {
            continue;
        }
        let state = state_by_slug.get(&def.slug).cloned();
        let enabled_override = state
            .as_ref()
            .and_then(|s| s.enabled_override)
            .map(|v| v != 0);
        if enabled_override == Some(false) {
            continue;
        }
        let explicitly_enabled = enabled_override == Some(true) || !def.built_in;
        if def.is_active_at(
            now.month(),
            now.day(),
            now.hour(),
            Some(now.weekday().number_from_monday()),
            None,
            effective_country.as_deref(),
            explicitly_enabled,
        ) {
            active.push((def, state));
        }
    }
    Ok((culture, active))
}

/// Lists active contextual mixes for `ctx` as pure structured client-i18n summaries:
/// 1. Active calendar Seasonal Packs that currently have at least one playable track (staged or owned),
///    sorted by narrowest calendar window first so event packs lead broad seasons
/// 2. Active Moment Mixes (always-surfaced + live weather + matching current hour/weekday window,
///    hiding empty strict-rule mixes)
/// 3. Enabled Rolling Discovery profiles (`mix.rolling.conservative` / `balanced` / `adventurous`)
#[allow(clippy::too_many_arguments)]
pub(crate) async fn list_active_contextual_mixes_in(
    db: &SqlitePool,
    navidrome: &dyn NavidromeOf,
    music_path: Option<&Path>,
    ctx: &UserCtx,
    now: chrono::DateTime<Utc>,
    local_hour: Option<u8>,
    local_weekday: Option<u8>,
    weather: Option<WeatherSnapshot>,
) -> Result<Vec<ContextualMixSummary>, Failure> {
    let hour = match local_hour {
        Some(h) if h <= 23 => u32::from(h),
        Some(_) => {
            return Err(Failure::new(
                FailureCode::InvalidInput,
                Stage::Input,
                RetryAdvice::NotRetryable,
                "local_hour must be in 0..=23",
            ));
        }
        None => now.hour(),
    };
    let weekday = match local_weekday {
        Some(d) if (1..=7).contains(&d) => u32::from(d),
        Some(_) => {
            return Err(Failure::new(
                FailureCode::InvalidInput,
                Stage::Input,
                RetryAdvice::NotRetryable,
                "local_weekday must be in 1..=7",
            ));
        }
        None => now.weekday().number_from_monday(),
    };
    let settings = UserSettings::get_in(db, &ctx.user_id)
        .await
        .map_err(|e| database_failure(Stage::Input, e))?;
    let culture = get_culture_profile_in(db, &ctx.user_id).await?;
    let mut pending = DiscoveryTrackRow::pending_for_user_in(db, &ctx.user_id)
        .await
        .map_err(|e| database_failure(Stage::Input, e))?;
    let defs = ContextualMixDefinitionRow::list_visible_for_user_in(db, &ctx.user_id)
        .await
        .map_err(|e| database_failure(Stage::Input, e))?;
    let states = ContextualMixStateRow::list_for_user_in(db, &ctx.user_id)
        .await
        .map_err(|e| database_failure(Stage::Input, e))?;
    let state_by_slug: HashMap<&str, &ContextualMixStateRow> =
        states.iter().map(|s| (s.slug.as_str(), s)).collect();

    let effective_country = culture.effective_country();
    let weather_condition = weather.as_ref().map(|w| w.condition);
    let mut active_defs: Vec<(&ContextualMixDefinition, Option<&ContextualMixStateRow>)> =
        Vec::new();

    for def in &defs {
        let state = state_by_slug.get(def.slug.as_str()).copied();
        let enabled_override = state.and_then(|s| s.enabled_override).map(|v| v != 0);
        if enabled_override == Some(false) {
            continue;
        }
        let explicitly_enabled = enabled_override == Some(true) || !def.built_in;
        if !def.is_active_at(
            now.month(),
            now.day(),
            hour,
            Some(weekday),
            weather_condition,
            effective_country,
            explicitly_enabled,
        ) {
            continue;
        }
        active_defs.push((def, state));
    }

    let has_active_contextual = !active_defs.is_empty();
    let needs_reconcile = pending.iter().any(needs_song_id_reconciliation);

    // Fetch Navidrome library ONCE for the whole listing call, whenever active contextual mixes
    // (Moment or Seasonal owned matching) or pending `song_id` reconciliation need it.
    let library: Option<Arc<Vec<SubsonicSong>>> = if has_active_contextual || needs_reconcile {
        match navidrome.navidrome_of(&ctx.id()).await {
            Ok(navi) => match fetch_library_snapshot_cached(&ctx.user_id, &navi).await {
                Ok(songs) => Some(songs),
                Err(e) => {
                    warn!(
                        "Could not fetch Navidrome songs while listing contextual mixes for user {}: {}",
                        ctx.user_id, e
                    );
                    None
                }
            },
            Err(NavidromeLookupError::NotLinked | NavidromeLookupError::NoServer) => None,
            Err(e) => {
                warn!(
                    "Could not resolve Navidrome client while listing contextual mixes for user {}: {}",
                    ctx.user_id, e
                );
                None
            }
        }
    } else {
        None
    };

    if let Some(ref songs) = library {
        let cleared = clear_stale_discovery_song_ids_in(db, songs, &mut pending).await;
        if needs_reconcile || cleared > 0 {
            reconcile_discovery_song_ids_in(db, songs, music_path, &mut pending).await;
        }
    }

    let mut seasonal: Vec<(u32, ContextualMixSummary)> = Vec::new();
    let mut moments: Vec<((u8, u16), ContextualMixSummary)> = Vec::new();

    for (def, state) in active_defs {
        let summary = summarize_definition(
            &ctx.user_id,
            def,
            &culture,
            state,
            &pending,
            library.as_deref().map(Vec::as_slice),
            now,
            None,
            weather.as_ref(),
        );
        match def.kind {
            ContextualMixKind::Seasonal => {
                // Never surface empty seasonal mixes to clients.
                if summary.track_count > 0 {
                    let span_days = def
                        .calendar_window
                        .as_ref()
                        .map(|w| w.span_days())
                        .unwrap_or(u32::MAX);
                    seasonal.push((span_days, summary));
                }
            }
            ContextualMixKind::Moment => {
                // Never surface empty strict-rule mixes (e.g. 80s Wave with 0 80s tracks).
                let is_strict = def.selection_rules.as_ref().is_some_and(|r| r.strict_match);
                if is_strict && summary.track_count == 0 {
                    continue;
                }
                let sort_key: (u8, u16) = if def.offline_priority {
                    (0, 0)
                } else if !def.weather_conditions.is_empty() {
                    (1, 0)
                } else if def.hour_window.is_some() || !def.days_of_week.is_empty() {
                    (2, 0)
                } else if def.selection_rules.is_none() {
                    (3, 0)
                } else if let Some((start_year, _)) =
                    def.selection_rules.as_ref().and_then(|r| r.year_range)
                {
                    // Keep decade mixes in chronological order (80s Wave < 90s Mix < 2000s Mix).
                    (5, start_year)
                } else {
                    (4, 0)
                };
                moments.push((sort_key, summary));
            }
            ContextualMixKind::Rolling => {}
        }
    }

    // Prioritize narrow holiday/event packs ahead of broad multi-month seasons.
    seasonal.sort_by(|(span_a, sum_a), (span_b, sum_b)| {
        span_a.cmp(span_b).then_with(|| sum_a.slug.cmp(&sum_b.slug))
    });

    // Put Drive / offline_priority moments first, then live weather mixes, then scheduled time/day
    // moments, then mood moments, then library-rule smart mixes, then chronological decade capsules.
    moments.sort_by(|(key_a, sum_a), (key_b, sum_b)| {
        key_a.cmp(key_b).then_with(|| sum_a.slug.cmp(&sum_b.slug))
    });

    let mut mixes = Vec::new();
    mixes.extend(seasonal.into_iter().map(|(_, s)| s));
    mixes.extend(moments.into_iter().map(|(_, s)| s));

    // Rolling Personal Mixes (count ONLY reconciled playable pending tracks matching resolve)
    for profile in parse_profiles(&settings.discovery_profiles) {
        let playable_count =
            build_rolling_mix_tracks(profile, library.as_deref().map(Vec::as_slice), &pending)
                .len() as u32;
        mixes.push(summarize_rolling_profile(profile, &settings, playable_count));
    }

    Ok(mixes)
}

/// Updates the user's live culture steering (`MoreLocal`, `Balanced`, `MoreInternational`) for
/// any seasonal contextual mix slug (supporting legacy `"noel"` / `"holiday"` / `"nye"` aliases).
pub(crate) async fn steer_seasonal_pack_in(
    db: &SqlitePool,
    navidrome: &dyn NavidromeOf,
    music_path: Option<&Path>,
    ctx: &UserCtx,
    slug: &str,
    steering: CultureSteering,
    now: chrono::DateTime<Utc>,
) -> Result<ContextualMixSummary, Failure> {
    let canonical = canonical_slug(slug)?;
    let def = ContextualMixDefinitionRow::get_visible_by_slug_in(db, &ctx.user_id, &canonical)
        .await
        .map_err(|e| database_failure(Stage::Input, e))?
        .ok_or_else(|| {
            Failure::new(
                FailureCode::UnknownHandle,
                Stage::Input,
                RetryAdvice::NotRetryable,
                format!("Unknown contextual mix '{slug}'"),
            )
        })?;
    if def.kind != ContextualMixKind::Seasonal {
        return Err(Failure::new(
            FailureCode::InvalidInput,
            Stage::Input,
            RetryAdvice::NotRetryable,
            format!(
                "Mix '{}' is a {} mix; cultural steering is only supported for seasonal packs",
                def.slug,
                def.kind.as_str()
            ),
        ));
    }
    let culture = get_culture_profile_in(db, &ctx.user_id).await?;
    let base_local = def.effective_base_local_ratio(&culture).unwrap_or(0.0);
    let effective_ratio = steering.effective_ratio(base_local);
    let state = ContextualMixStateRow::upsert_steering_in(
        db,
        &ctx.user_id,
        &def.slug,
        steering,
        effective_ratio,
    )
    .await
    .map_err(|e| database_failure(Stage::Input, e))?;
    let mut pending = DiscoveryTrackRow::pending_for_user_in(db, &ctx.user_id)
        .await
        .map_err(|e| database_failure(Stage::Input, e))?;
    let needs_reconcile = pending.iter().any(needs_song_id_reconciliation);
    let library: Option<Arc<Vec<SubsonicSong>>> = match navidrome.navidrome_of(&ctx.id()).await {
        Ok(navi) => match fetch_library_snapshot_cached(&ctx.user_id, &navi).await {
            Ok(songs) => Some(songs),
            Err(e) => {
                warn!(
                    "Could not fetch Navidrome songs while steering seasonal pack '{}' for user {}: {}",
                    def.slug, ctx.user_id, e
                );
                None
            }
        },
        Err(NavidromeLookupError::NotLinked | NavidromeLookupError::NoServer) => None,
        Err(e) => {
            warn!(
                "Could not resolve Navidrome client while steering seasonal pack '{}' for user {}: {}",
                def.slug, ctx.user_id, e
            );
            None
        }
    };
    if let Some(ref songs) = library {
        let cleared = clear_stale_discovery_song_ids_in(db, songs, &mut pending).await;
        if needs_reconcile || cleared > 0 {
            reconcile_discovery_song_ids_in(db, songs, music_path, &mut pending).await;
        }
    }
    Ok(summarize_definition(
        &ctx.user_id,
        &def,
        &culture,
        Some(&state),
        &pending,
        library.as_deref().map(Vec::as_slice),
        now,
        None,
        None,
    ))
}

/// Reconciles active Seasonal contextual mixes into real Navidrome smart playlists pointing to
/// `Discovery/Seasonal/<slug>/`, and deletes expired seasonal playlists once past `expires_at`
/// with zero pending tracks left. Called directly from `reconcile_discovery_playlists`.
pub(crate) async fn reconcile_seasonal_playlists_in(
    db: &SqlitePool,
    navi: &NavidromeClient,
    user_id: &str,
    discovery_place: &NavidromePlace,
    now: chrono::DateTime<Utc>,
) -> Result<Vec<String>, String> {
    let culture = UserCultureProfileRow::get_in(db, user_id)
        .await?
        .unwrap_or_default();
    let defs = ContextualMixDefinitionRow::list_visible_for_user_in(db, user_id).await?;
    let states = ContextualMixStateRow::list_for_user_in(db, user_id).await?;
    let pending = DiscoveryTrackRow::pending_for_user_in(db, user_id).await?;
    let state_by_slug: HashMap<&str, &ContextualMixStateRow> =
        states.iter().map(|s| (s.slug.as_str(), s)).collect();

    let seasonal_root = discovery_place.child("Seasonal");
    let mut reconciled_slugs = Vec::new();

    for def in &defs {
        if def.kind != ContextualMixKind::Seasonal || !def.sync_to_navidrome {
            continue;
        }
        let state = state_by_slug.get(def.slug.as_str()).copied();
        let enabled_override = state.and_then(|s| s.enabled_override).map(|v| v != 0);
        let explicitly_enabled = enabled_override == Some(true) || !def.built_in;
        let active = enabled_override != Some(false)
            && def.is_active_at(
                now.month(),
                now.day(),
                now.hour(),
                Some(now.weekday().number_from_monday()),
                None,
                culture.effective_country(),
                explicitly_enabled,
            );

        let has_pending = pending
            .iter()
            .any(|t| t.pack_slug.as_deref() == Some(def.slug.as_str()));

        if active && has_pending {
            let place = seasonal_root.child(&def.slug);
            let prefix = place.song_prefix();
            // Navidrome smart playlist name uses custom_name when set, else the canonical mix title
            // so Navidrome, Web, Desktop, Android, and Android Auto always match.
            let playlist_name = canonical_mix_title(
                &def.slug,
                Some(&def.i18n_key),
                def.custom_name.as_deref(),
            );
            let comment = format!("soulbeet:contextual:{}", def.slug);
            let expires_at = def
                .calendar_window
                .as_ref()
                .and_then(|w| w.cycle_dates(now.year(), now.month(), now.day()))
                .map(|(_, _, exp)| exp);

            let stored_id = state.and_then(|s| s.navidrome_playlist_id.as_deref());
            let existing = match stored_id {
                Some(id) => match navi.get_native_playlist(id).await {
                    Ok(pl) => pl,
                    Err(e) => {
                        warn!(
                            "Could not check seasonal Navidrome playlist '{}' ({}) for user {}: {}",
                            def.slug, id, user_id, e
                        );
                        continue;
                    }
                },
                None => None,
            };

            let (playlist_id, created_new) = match existing {
                Some(pl) => {
                    let rules = filepath_rules(place.library_id, &prefix);
                    let change = SmartPlaylistChange {
                        name: (pl.name != playlist_name).then_some(playlist_name.as_str()),
                        rules: (pl.rules.as_ref() != Some(&rules)).then_some(&rules),
                    };
                    if change.name.is_some() || change.rules.is_some() {
                        if let Err(e) = navi.update_smart_playlist(&pl.id, &change).await {
                            warn!(
                                "Could not update seasonal Navidrome playlist '{}' ({}) for user {}: {}",
                                def.slug, pl.id, user_id, e
                            );
                            continue;
                        }
                    }
                    (pl.id, false)
                }
                None => {
                    let created_id = match navi
                        .create_smart_playlist(&playlist_name, &comment, place.library_id, &prefix)
                        .await
                    {
                        Ok(id) => id,
                        Err(e) => {
                            warn!(
                                "Could not create seasonal Navidrome smart playlist '{}' for user {}: {}",
                                playlist_name, user_id, e
                            );
                            continue;
                        }
                    };
                    info!(
                        "Created seasonal Navidrome smart playlist '{}' ({}) for user {}",
                        playlist_name, created_id, user_id
                    );
                    (created_id, true)
                }
            };

            if let Err(e) = ContextualMixStateRow::upsert_playlist_id_in(
                db,
                user_id,
                &def.slug,
                Some(&playlist_id),
                expires_at.as_deref(),
            )
            .await
            {
                if created_new {
                    let undone = match navi.delete_smart_playlist(&playlist_id).await {
                        Ok(()) => "rolled back".to_string(),
                        Err(del_err) => format!("left in Navidrome: {del_err}"),
                    };
                    warn!(
                        "Failed to persist seasonal playlist '{}' ({}) for user {} ({e}); {undone}",
                        def.slug, playlist_id, user_id
                    );
                } else {
                    warn!(
                        "Failed to persist seasonal playlist '{}' ({}) for user {}: {}",
                        def.slug, playlist_id, user_id, e
                    );
                }
                continue;
            }
            reconciled_slugs.push(def.slug.clone());
        } else if let Some(st) = state {
            // Never keep an empty seasonal smart playlist in Navidrome: clean up whenever 0
            // pending staged tracks remain under Discovery/Seasonal/<slug>/ (whether active,
            // disabled before expires_at, or expired).
            if !has_pending {
                if let Some(ref pl_id) = st.navidrome_playlist_id {
                    let gone = match navi.delete_smart_playlist(pl_id).await {
                        Ok(()) => true,
                        Err(soulbeet::NavidromeError::Http { status: 404, .. }) => true,
                        Err(e) => match navi.get_native_playlist(pl_id).await {
                            Ok(None) => true,
                            _ => {
                                warn!(
                                    "Could not clean up empty/expired seasonal playlist '{}' ({}) for user {}: {}",
                                    def.slug, pl_id, user_id, e
                                );
                                false
                            }
                        },
                    };
                    if gone {
                        info!(
                            "Cleaned up empty/expired seasonal Navidrome playlist '{}' ({}) for user {}",
                            def.slug, pl_id, user_id
                        );
                        if let Err(e) = ContextualMixStateRow::upsert_playlist_id_in(
                            db,
                            user_id,
                            &def.slug,
                            None,
                            st.expires_at.as_deref(),
                        )
                        .await
                        {
                            warn!(
                                "Failed to clear seasonal playlist id for '{}' (user {}): {}",
                                def.slug, user_id, e
                            );
                        }
                    }
                }
            }
        }
    }

    // Clean up orphaned states from retired/deleted seasonal mixes (e.g. retired built-ins like
    // `diwali`) once no pending tracks remain for that slug, so Navidrome playlists are never
    // left orphaned.
    let known_slugs: HashSet<&str> = defs.iter().map(|d| d.slug.as_str()).collect();
    for st in &states {
        if known_slugs.contains(st.slug.as_str()) {
            continue;
        }
        let has_pending = pending
            .iter()
            .any(|t| t.pack_slug.as_deref() == Some(st.slug.as_str()));
        if has_pending {
            continue;
        }
        let cleaned = match st.navidrome_playlist_id.as_deref() {
            Some(pl_id) => match navi.delete_smart_playlist(pl_id).await {
                Ok(()) => true,
                Err(soulbeet::NavidromeError::Http { status: 404, .. }) => true,
                Err(e) => match navi.get_native_playlist(pl_id).await {
                    Ok(None) => true,
                    _ => {
                        warn!(
                            "Could not clean up retired seasonal playlist '{}' ({}) for user {}: {}",
                            st.slug, pl_id, user_id, e
                        );
                        false
                    }
                },
            },
            None => true,
        };
        if cleaned {
            if let Err(e) = ContextualMixStateRow::delete_in(db, user_id, &st.slug).await {
                warn!(
                    "Failed to delete retired contextual_mix_states row '{}' for user {}: {}",
                    st.slug, user_id, e
                );
            }
        }
    }

    Ok(reconciled_slugs)
}

/// Resolves a playable blended playlist for any contextual mix slug:
/// - For Moment/hybrid mixes (`owned_ratio > 0`): selects owned Navidrome songs matching the mix's
///   `theme_tags` (excluding disliked songs and staged `Discovery/` / pending discovery rows,
///   capped per artist for diversity) blended with staged fresh Discovery tracks.
/// - For Seasonal mixes (`owned_ratio == 0`): returns staged Discovery tracks for `pack_slug`
///   and NEVER creates/updates static playlists (seasonal mixes use Navidrome smart playlists).
/// - Strictly read-only when `sync_navidrome` is `false`. Only creates/updates a static Navidrome
///   playlist when `sync_navidrome` is `true` on a `Moment` mix with at least one track.
pub(crate) async fn resolve_contextual_mix_playlist_in(
    db: &SqlitePool,
    navidrome: &dyn NavidromeOf,
    music_path: Option<&Path>,
    ctx: &UserCtx,
    slug: &str,
    sync_navidrome: bool,
    now: chrono::DateTime<Utc>,
) -> Result<ContextualMixPlaylist, Failure> {
    let canonical = canonical_slug(slug)?;
    let def_opt = ContextualMixDefinitionRow::get_visible_by_slug_in(db, &ctx.user_id, &canonical)
        .await
        .map_err(|e| database_failure(Stage::Playlist, e))?;

    let def = match def_opt {
        Some(def) => def,
        None => {
            if let Some(profile) = rolling_profile_for_slug(&canonical) {
                let settings = UserSettings::get_in(db, &ctx.user_id)
                    .await
                    .map_err(|e| database_failure(Stage::Playlist, e))?;
                let mut pending = DiscoveryTrackRow::pending_for_user_in(db, &ctx.user_id)
                    .await
                    .map_err(|e| database_failure(Stage::Playlist, e))?;
                let needs_reconcile = pending.iter().any(needs_song_id_reconciliation);
                let has_rolling = pending.iter().any(|t| {
                    t.profile.eq_ignore_ascii_case(profile.name()) && t.pack_slug.is_none()
                });
                let library: Option<Arc<Vec<SubsonicSong>>> = if needs_reconcile || has_rolling {
                    match navidrome.navidrome_of(&ctx.id()).await {
                        Ok(navi) => match fetch_library_snapshot_cached(&ctx.user_id, &navi).await {
                            Ok(songs) => Some(songs),
                            Err(e) => {
                                warn!(
                                    "Could not fetch Navidrome songs for rolling mix '{}' (user {}): {}",
                                    canonical, ctx.user_id, e
                                );
                                None
                            }
                        },
                        Err(NavidromeLookupError::NotLinked | NavidromeLookupError::NoServer) => {
                            None
                        }
                        Err(e) => return Err(e.failure(Stage::Playlist, e.to_string())),
                    }
                } else {
                    None
                };
                if let Some(ref songs) = library {
                    let cleared = clear_stale_discovery_song_ids_in(db, songs, &mut pending).await;
                    if needs_reconcile || cleared > 0 {
                        reconcile_discovery_song_ids_in(db, songs, music_path, &mut pending).await;
                    }
                }
                let tracks = build_rolling_mix_tracks(
                    profile,
                    library.as_deref().map(Vec::as_slice),
                    &pending,
                );
                let summary = summarize_rolling_profile(profile, &settings, tracks.len() as u32);
                return Ok(ContextualMixPlaylist {
                    summary,
                    tracks,
                    prefetch_candidates: Vec::new(),
                });
            }
            return Err(Failure::new(
                FailureCode::UnknownHandle,
                Stage::Playlist,
                RetryAdvice::NotRetryable,
                format!("Contextual mix '{slug}' not found"),
            ));
        }
    };

    let culture = get_culture_profile_in(db, &ctx.user_id).await?;
    let mut state = ContextualMixStateRow::get_in(db, &ctx.user_id, &def.slug)
        .await
        .map_err(|e| database_failure(Stage::Playlist, e))?;
    let mut pending = DiscoveryTrackRow::pending_for_user_in(db, &ctx.user_id)
        .await
        .map_err(|e| database_failure(Stage::Playlist, e))?;

    let target = def.target_tracks as usize;
    let owned_target = ((target as f32) * def.owned_ratio).round() as usize;
    let needs_reconcile = pending.iter().any(needs_song_id_reconciliation);

    let navi_opt = match navidrome.navidrome_of(&ctx.id()).await {
        Ok(client) => Some(client),
        Err(NavidromeLookupError::NotLinked | NavidromeLookupError::NoServer) => None,
        Err(e) => return Err(e.failure(Stage::Playlist, e.to_string())),
    };

    let library: Option<Arc<Vec<SubsonicSong>>> = if owned_target > 0
        || def.kind == ContextualMixKind::Seasonal
        || needs_reconcile
        || !pending.is_empty()
    {
        if let Some(ref navi) = navi_opt {
            match fetch_library_snapshot_cached(&ctx.user_id, navi).await {
                Ok(songs) => Some(songs),
                Err(e) if owned_target > 0 => {
                    return Err(navidrome_failure(Stage::Playlist, &e));
                }
                Err(e) => {
                    warn!(
                        "Could not fetch Navidrome songs for mix '{}' (user {}): {}",
                        def.slug, ctx.user_id, e
                    );
                    None
                }
            }
        } else {
            None
        }
    } else {
        None
    };

    if let Some(ref songs) = library {
        let cleared = clear_stale_discovery_song_ids_in(db, songs, &mut pending).await;
        if needs_reconcile || cleared > 0 {
            reconcile_discovery_song_ids_in(db, songs, music_path, &mut pending).await;
        }
    }

    let tracks = build_contextual_mix_tracks(
        &ctx.user_id,
        &def,
        library.as_deref().map(Vec::as_slice),
        &pending,
        now,
    );

    // Optionally materialize as a real static Navidrome playlist (ONLY for Moment mixes and ONLY
    // when the caller explicitly requested `sync_navidrome = true`; Seasonal mixes use smart
    // playlists reconciled by `reconcile_seasonal_playlists_in`).
    if sync_navidrome && def.kind == ContextualMixKind::Moment && !tracks.is_empty() {
        if let Some(ref navi) = navi_opt {
            let song_ids: Vec<String> = tracks.iter().map(|t| t.song_id.clone()).collect();
            let playlist_name = canonical_mix_title(
                &def.slug,
                Some(&def.i18n_key),
                def.custom_name.as_deref(),
            );
            let stored_id = state
                .as_ref()
                .and_then(|s| s.navidrome_playlist_id.clone());

            let updated_id = match stored_id {
                Some(id) => match navi.replace_playlist_tracks(&id, &song_ids).await {
                    Ok(true) => Some(id),
                    Ok(false) => navi
                        .create_playlist(&playlist_name, &song_ids)
                        .await
                        .ok()
                        .map(|p| p.id),
                    Err(e) => {
                        warn!(
                            "Could not update Navidrome playlist '{}' for mix '{}': {}",
                            id, def.slug, e
                        );
                        None
                    }
                },
                None => navi
                    .create_playlist(&playlist_name, &song_ids)
                    .await
                    .ok()
                    .map(|p| p.id),
            };

            if let Some(pl_id) = updated_id {
                let comment = format!("soulbeet:contextual:{}", def.slug);
                let meta = PlaylistMeta {
                    name: Some(playlist_name.as_str()),
                    comment: Some(comment.as_str()),
                    public: None,
                };
                if let Err(e) = navi.update_playlist_meta(&pl_id, &meta).await {
                    warn!(
                        "Could not update metadata on Navidrome playlist '{}' for mix '{}': {}",
                        pl_id, def.slug, e
                    );
                }
                if let Ok(updated_state) = ContextualMixStateRow::upsert_playlist_id_in(
                    db,
                    &ctx.user_id,
                    &def.slug,
                    Some(&pl_id),
                    None,
                )
                .await
                {
                    state = Some(updated_state);
                }
            }
        }
    }

    let summary = summarize_definition(
        &ctx.user_id,
        &def,
        &culture,
        state.as_ref(),
        &pending,
        library.as_deref().map(Vec::as_slice),
        now,
        Some(tracks.len() as u32),
        None,
    );

    Ok(ContextualMixPlaylist {
        summary,
        tracks,
        prefetch_candidates: Vec::new(),
    })
}

/// Lists pending unrated Discovery tracks awaiting verdict in Triage mode, soonest-to-expire first.
pub(crate) async fn list_triage_queue_in(
    db: &SqlitePool,
    navidrome: &dyn NavidromeOf,
    music_path: Option<&Path>,
    user_id: &str,
) -> Result<Vec<DiscoveryTrack>, Failure> {
    let settings = UserSettings::get_in(db, user_id)
        .await
        .map_err(|e| database_failure(Stage::Input, e))?;
    let mut pending = DiscoveryTrackRow::pending_for_user_in(db, user_id)
        .await
        .map_err(|e| database_failure(Stage::Input, e))?;
    reconcile_and_hydrate_discovery_tracks_in(db, navidrome, music_path, user_id, &mut pending)
        .await;
    let lifetimes = settings.parse_lifetime_days();
    let seasonal = SeasonalExpiryLookup::load_for_tracks_in(db, user_id, &pending).await;
    pending.sort_by_key(|t| {
        let exp = expires_at_with_seasonal(t, &lifetimes, &seasonal);
        (exp.is_none(), exp)
    });
    Ok(pending)
}

/// Starts an on-demand Seed Radio from a song ID, artist name, or artist MBID:
/// 1. Returns matching owned tracks from Navidrome immediately for zero-wait playback.
/// 2. Queries `MusicKnowledge::similar_artists` + `artist_top_tracks` to produce unowned
///    `prefetch_candidates` ready for fast background acquisition.
pub(crate) async fn start_seed_radio_in(
    navidrome: &dyn NavidromeOf,
    knowledge: &dyn MusicKnowledge,
    ctx: &UserCtx,
    req: &SeedRadioRequest,
) -> Result<SeedRadioResponse, Failure> {
    let navi = navidrome_for(navidrome, ctx, Stage::LibraryCheck).await?;
    let limit = req.limit.unwrap_or(20).clamp(5, 50);

    let (mut seed_artist, mut seed_track, mut seed_mbid) = (
        req.seed_artist.clone().filter(|s| !s.trim().is_empty()),
        req.seed_track.clone().filter(|s| !s.trim().is_empty()),
        req.seed_artist_mbid.clone().filter(|s| !s.trim().is_empty()),
    );

    let mut owned_tracks: Vec<SeedRadioTrack> = Vec::new();
    let mut owned_keys: HashSet<String> = HashSet::new();

    if let Some(song_id) = req.seed_song_id.as_deref().filter(|s| !s.trim().is_empty()) {
        if let Some(song) = navi
            .get_song(song_id)
            .await
            .map_err(|e| navidrome_failure(Stage::LibraryCheck, &e))?
        {
            if seed_artist.is_none() {
                seed_artist = song.artist.clone();
            }
            if seed_track.is_none() {
                seed_track = Some(song.title.clone());
            }
            let artist_name = song.artist.clone().unwrap_or_default();
            owned_keys.insert(CandidateSet::key(&artist_name, &song.title));
            owned_tracks.push(SeedRadioTrack {
                song_id: song.id,
                title: song.title,
                artist: artist_name.clone(),
                album: song.album,
                duration: song.duration,
                rating: normalized_rating(song.user_rating),
                starred: song.starred.is_some(),
                reason: CandidateReason::RadioSeed {
                    seed_artist: artist_name,
                },
            });
        }
    }

    let seed_artist = seed_artist.ok_or_else(|| {
        Failure::new(
            FailureCode::InvalidInput,
            Stage::Input,
            RetryAdvice::NotRetryable,
            "Provide seed_song_id or seed_artist to start Seed Radio",
        )
    })?;

    if seed_mbid.is_none() {
        seed_mbid = knowledge.resolve_artist(&seed_artist).await.ok().flatten();
    }

    let all_songs = fetch_library_snapshot_cached(&ctx.user_id, &navi)
        .await
        .map_err(|e| navidrome_failure(Stage::LibraryCheck, &e))?;

    // Track every song in the user's library so already-owned tracks are never returned as
    // unowned `prefetch_candidates`.
    let library_keys: HashSet<String> = all_songs
        .iter()
        .map(|s| CandidateSet::key(s.artist.as_deref().unwrap_or(""), &s.title))
        .collect();

    let seed_artist_key = discovery_name(&seed_artist);
    for song in all_songs.iter() {
        if owned_tracks.len() >= 4 {
            break;
        }
        let artist_name = song.artist.as_deref().unwrap_or("");
        if discovery_name(artist_name) == seed_artist_key {
            let key = CandidateSet::key(artist_name, &song.title);
            if owned_keys.insert(key) {
                owned_tracks.push(SeedRadioTrack {
                    song_id: song.id.clone(),
                    title: song.title.clone(),
                    artist: artist_name.to_string(),
                    album: song.album.clone(),
                    duration: song.duration,
                    rating: normalized_rating(song.user_rating),
                    starred: song.starred.is_some(),
                    reason: CandidateReason::MoreFromArtist {
                        artist: seed_artist.clone(),
                    },
                });
            }
        }
    }

    let mut prefetch_candidates: Vec<Candidate> = Vec::new();
    let mut seen_prefetch: HashSet<String> = HashSet::new();

    if let Some(ref mbid) = seed_mbid {
        if let Ok(neighbours) = knowledge.similar_artists(&seed_artist, mbid, 12).await {
            for neighbour in &neighbours {
                let n_key = discovery_name(&neighbour.artist.name);
                for song in all_songs.iter() {
                    if owned_tracks.len() >= limit {
                        break;
                    }
                    let artist_name = song.artist.as_deref().unwrap_or("");
                    if discovery_name(artist_name) == n_key {
                        let key = CandidateSet::key(artist_name, &song.title);
                        if owned_keys.insert(key) {
                            owned_tracks.push(SeedRadioTrack {
                                song_id: song.id.clone(),
                                title: song.title.clone(),
                                artist: artist_name.to_string(),
                                album: song.album.clone(),
                                duration: song.duration,
                                rating: normalized_rating(song.user_rating),
                                starred: song.starred.is_some(),
                                reason: CandidateReason::SimilarArtist {
                                    seed_artist: seed_artist.clone(),
                                    artist: artist_name.to_string(),
                                },
                            });
                            break;
                        }
                    }
                }

                if prefetch_candidates.len() < 5 {
                    if let Some(ref n_mbid) = neighbour.artist.mbid {
                        if let Ok(top) = knowledge.artist_top_tracks(n_mbid, 3).await {
                            for tr in top {
                                let key = CandidateSet::key(&tr.artist.name, &tr.title);
                                if !library_keys.contains(&key)
                                    && !owned_keys.contains(&key)
                                    && seen_prefetch.insert(key)
                                {
                                    prefetch_candidates.push(Candidate {
                                        artist: tr.artist.name.clone(),
                                        track: tr.title,
                                        album: None,
                                        mbid: tr.mbid,
                                        score: neighbour.score,
                                        signals: vec!["seed_radio_similar".into()],
                                        source: "seed_radio".into(),
                                        artist_listener_count: None,
                                        primary_genre: None,
                                        release_year: None,
                                        artist_mbid: tr.artist.mbid,
                                        reason: Some(CandidateReason::SimilarArtist {
                                            seed_artist: seed_artist.clone(),
                                            artist: tr.artist.name,
                                        }),
                                        culture_bucket: Some(CultureBucket::TasteWildcard),
                                    });
                                    break;
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    let seed_label = match seed_track {
        Some(track) => format!("{seed_artist} — {track}"),
        None => seed_artist,
    };

    Ok(SeedRadioResponse {
        radio_id: Uuid::new_v4().to_string(),
        seed_label,
        owned_tracks,
        prefetch_candidates,
    })
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;

    use super::*;
    use crate::auth::{Role, UserId};
    use crate::core::{folder_of, test_ctx};
    use crate::db::test_db;
    use crate::services::NavidromeLookupError;
    use crate::test_navidrome::{subsonic_ok, NavidromeStub};
    use async_trait::async_trait;
    use shared::recommendation::{CalendarWindow, HourWindow};
    use std::sync::Arc;

    struct StubNavidrome(Arc<NavidromeClient>);

    #[async_trait]
    impl NavidromeOf for StubNavidrome {
        async fn navidrome_of(
            &self,
            _user: &UserId,
        ) -> Result<Arc<NavidromeClient>, NavidromeLookupError> {
            Ok(self.0.clone())
        }
    }

    struct NoNavidrome;

    #[async_trait]
    impl NavidromeOf for NoNavidrome {
        async fn navidrome_of(
            &self,
            _user: &UserId,
        ) -> Result<Arc<NavidromeClient>, NavidromeLookupError> {
            Err(NavidromeLookupError::NotLinked)
        }
    }

    #[tokio::test]
    async fn culture_profile_steering_and_client_i18n_summaries_persist_per_user() {
        let db = test_db::migrated().await;
        folder_of(&db, "alice").await;
        let ctx = test_ctx("alice");

        let default = get_culture_profile_in(&db, "alice").await.unwrap();
        assert_eq!(default.home_country, None);
        assert!((default.local_culture_ratio - 0.45).abs() < 0.001);

        let updated = update_culture_profile_in(
            &db,
            "alice",
            UpdateCultureProfileRequest {
                home_country: Some("jp".into()),
                preferred_languages: Some(vec!["JA".into(), "EN".into()]),
                local_culture_ratio: Some(0.50),
                weather_city: None,
            },
        )
        .await
        .unwrap();
        assert_eq!(updated.home_country.as_deref(), Some("JP"));
        assert_eq!(updated.preferred_languages, vec!["ja", "en"]);

        let dec = Utc.with_ymd_and_hms(2026, 12, 10, 14, 0, 0).unwrap();

        // Steer winter_holiday to MoreLocal -> 0.50 + 0.25 = 0.75 (75%)
        let steered = steer_seasonal_pack_in(
            &db,
            &NoNavidrome,
            None,
            &ctx,
            "winter_holiday",
            CultureSteering::MoreLocal,
            dec,
        )
        .await
        .unwrap();
        assert_eq!(steered.slug, "winter_holiday");
        assert_eq!(steered.i18n_key, "mix.seasonal.winter_holiday");
        assert_eq!(steered.custom_name, None);
        assert_eq!(steered.country_code.as_deref(), Some("JP"));
        assert_eq!(steered.local_pct, Some(75));
        assert_eq!(steered.steering, Some(CultureSteering::MoreLocal));
        assert_eq!(steered.expires_at.as_deref(), Some("2027-01-05"));
        assert!(steered.exclude_from_core_taste);

        // Empty seasonal mixes (0 staged + 0 owned tracks) are hidden from list_active_contextual_mixes_in,
        // while list_calendar_active_seasonal_defs_in still surfaces them for the generator.
        let empty_mixes =
            list_active_contextual_mixes_in(&db, &NoNavidrome, None, &ctx, dec, None, None, None)
                .await
                .unwrap();
        assert!(!empty_mixes.iter().any(|m| m.slug == "winter_holiday"));
        let (_, cal_defs) = list_calendar_active_seasonal_defs_in(&db, "alice", dec)
            .await
            .unwrap();
        assert!(cal_defs.iter().any(|(d, _)| d.slug == "winter_holiday"));

        // Once a seasonal track is staged and reconciled, winter_holiday surfaces with pure i18n keys
        let folder_id = crate::models::folder::Folder::get_all_by_user_in(&db, "alice")
            .await
            .unwrap()[0]
            .id
            .clone();
        sqlx::query(
            "INSERT INTO discovery_tracks \
                (id, song_id, title, artist, album, path, folder_id, profile, status, \
                 exclude_from_core_taste, culture_bucket, pack_slug) \
             VALUES ('dt-dec-1', 'nd-dec-1', 'Snow Song', 'Winter Act', 'Holiday', \
                     '/music/alice/Discovery/Seasonal/winter_holiday/01.flac', ?, \
                     'Seasonal:winter_holiday', 'Pending', 1, 'local', 'winter_holiday')",
        )
        .bind(&folder_id)
        .execute(&db)
        .await
        .unwrap();

        let mixes =
            list_active_contextual_mixes_in(&db, &NoNavidrome, None, &ctx, dec, None, None, None)
                .await
                .unwrap();
        assert_eq!(mixes[0].kind, ContextualMixKind::Seasonal);
        assert_eq!(mixes[0].slug, "winter_holiday");
        assert_eq!(mixes[0].i18n_key, "mix.seasonal.winter_holiday");
        assert_eq!(mixes[0].custom_name, None);
        assert_eq!(mixes[0].local_pct, Some(75));
        assert_eq!(mixes[0].track_count, 1);
        assert!(mixes.iter().any(|m| m.slug == "drive" && m.offline_priority));
        assert!(mixes.iter().any(|m| m.slug == "focus"));

        // local_hour overrides UTC hour for Moment windows (morning = 5..=10, late_night = 22..=4)
        let morning_mixes = list_active_contextual_mixes_in(
            &db,
            &NoNavidrome,
            None,
            &ctx,
            dec,
            Some(8),
            None,
            None,
        )
        .await
        .unwrap();
        assert!(morning_mixes.iter().any(|m| m.slug == "morning"));
        assert!(!morning_mixes.iter().any(|m| m.slug == "focus"));

        // Steering a Moment mix is rejected
        let steer_moment = steer_seasonal_pack_in(
            &db,
            &NoNavidrome,
            None,
            &ctx,
            "drive",
            CultureSteering::MoreLocal,
            dec,
        )
        .await
        .unwrap_err();
        assert_eq!(steer_moment.code, FailureCode::InvalidInput);
    }

    #[tokio::test]
    async fn custom_mix_crud_and_per_user_toggle_extend_catalog_without_recompiling() {
        let db = test_db::migrated().await;
        folder_of(&db, "alice").await;
        let alice = test_ctx("alice");
        let admin = UserCtx {
            user_id: "alice".into(),
            username: "alice".into(),
            role: Role::Admin,
        };

        // Regular user creates a private custom seasonal event (e.g. Midsummer)
        let created = upsert_mix_definition_in(
            &db,
            &alice,
            UpsertContextualMixRequest {
                slug: "midsummer".into(),
                kind: ContextualMixKind::Seasonal,
                custom_name: Some("Midsommar".into()),
                custom_description: Some("Nordic folk and sunlit indie".into()),
                global: false,
                enabled: None,
                theme_tags: vec!["folk".into(), "indie".into(), "scandinavian".into()],
                international_queries: vec!["Midsummer Folk".into()],
                calendar_window: Some(CalendarWindow::new(6, 18, 6, 26, 5)),
                hour_window: None,
                days_of_week: vec![],
                weather_conditions: vec![],
                always_surfaced: false,
                countries: vec![],
                owned_ratio: 0.0,
                default_local_ratio: Some(0.50),
                exclude_from_core_taste: true,
                offline_priority: false,
                target_tracks: 20,
                sync_to_navidrome: true,
                selection_rules: None,
            },
        )
        .await
        .unwrap();
        assert!(!created.built_in);
        assert_eq!(created.owner_user_id.as_deref(), Some("alice"));
        assert_eq!(created.custom_name.as_deref(), Some("Midsommar"));

        // Invalid calendar_window and hour_window are rejected
        let bad_cal = upsert_mix_definition_in(
            &db,
            &alice,
            UpsertContextualMixRequest {
                slug: "bad_cal".into(),
                kind: ContextualMixKind::Seasonal,
                custom_name: None,
                custom_description: None,
                global: false,
                enabled: None,
                theme_tags: vec!["folk".into()],
                international_queries: vec![],
                calendar_window: Some(CalendarWindow::new(2, 31, 3, 1, 5)),
                hour_window: None,
                days_of_week: vec![],
                weather_conditions: vec![],
                always_surfaced: false,
                countries: vec![],
                owned_ratio: 0.0,
                default_local_ratio: None,
                exclude_from_core_taste: true,
                offline_priority: false,
                target_tracks: 20,
                sync_to_navidrome: true,
                selection_rules: None,
            },
        )
        .await
        .unwrap_err();
        assert_eq!(bad_cal.code, FailureCode::InvalidInput);

        let bad_hour = upsert_mix_definition_in(
            &db,
            &alice,
            UpsertContextualMixRequest {
                slug: "bad_hour".into(),
                kind: ContextualMixKind::Moment,
                custom_name: None,
                custom_description: None,
                global: false,
                enabled: None,
                theme_tags: vec!["jazz".into()],
                international_queries: vec![],
                calendar_window: None,
                hour_window: Some(HourWindow::new(25, 4)),
                days_of_week: vec![],
                weather_conditions: vec![],
                always_surfaced: false,
                countries: vec![],
                owned_ratio: 0.5,
                default_local_ratio: None,
                exclude_from_core_taste: false,
                offline_priority: false,
                target_tracks: 20,
                sync_to_navidrome: false,
                selection_rules: None,
            },
        )
        .await
        .unwrap_err();
        assert_eq!(bad_hour.code, FailureCode::InvalidInput);

        // Non-admin cannot create global mixes
        let denied = upsert_mix_definition_in(
            &db,
            &alice,
            UpsertContextualMixRequest {
                slug: "global_fest".into(),
                kind: ContextualMixKind::Moment,
                custom_name: Some("Fest".into()),
                custom_description: None,
                global: true,
                enabled: None,
                theme_tags: vec!["jazz".into()],
                international_queries: vec![],
                calendar_window: None,
                hour_window: Some(HourWindow::new(18, 21)),
                days_of_week: vec![],
                weather_conditions: vec![],
                always_surfaced: false,
                countries: vec![],
                owned_ratio: 0.75,
                default_local_ratio: None,
                exclude_from_core_taste: false,
                offline_priority: false,
                target_tracks: 25,
                sync_to_navidrome: false,
                selection_rules: None,
            },
        )
        .await
        .unwrap_err();
        assert_eq!(denied.code, FailureCode::PermissionNotGranted);

        // Admin promoting a private mix to global updates owner_user_id to NULL and preserves enabled=false
        let disabled_midsummer = upsert_mix_definition_in(
            &db,
            &admin,
            UpsertContextualMixRequest {
                slug: "midsummer".into(),
                kind: ContextualMixKind::Seasonal,
                custom_name: Some("Midsommar".into()),
                custom_description: None,
                global: true,
                enabled: Some(false),
                theme_tags: vec!["folk".into()],
                international_queries: vec![],
                calendar_window: Some(CalendarWindow::new(6, 18, 6, 26, 5)),
                hour_window: None,
                days_of_week: vec![],
                weather_conditions: vec![],
                always_surfaced: false,
                countries: vec![],
                owned_ratio: 0.0,
                default_local_ratio: Some(0.50),
                exclude_from_core_taste: true,
                offline_priority: false,
                target_tracks: 20,
                sync_to_navidrome: true,
                selection_rules: None,
            },
        )
        .await
        .unwrap();
        assert_eq!(disabled_midsummer.owner_user_id, None);
        assert!(!disabled_midsummer.enabled);

        // Editing without `enabled` preserves existing `enabled = false`
        let preserved = upsert_mix_definition_in(
            &db,
            &admin,
            UpsertContextualMixRequest {
                slug: "midsummer".into(),
                kind: ContextualMixKind::Seasonal,
                custom_name: Some("Midsommar Updated".into()),
                custom_description: None,
                global: true,
                enabled: None,
                theme_tags: vec!["folk".into()],
                international_queries: vec![],
                calendar_window: Some(CalendarWindow::new(6, 18, 6, 26, 5)),
                hour_window: None,
                days_of_week: vec![],
                weather_conditions: vec![],
                always_surfaced: false,
                countries: vec![],
                owned_ratio: 0.0,
                default_local_ratio: Some(0.50),
                exclude_from_core_taste: true,
                offline_priority: false,
                target_tracks: 20,
                sync_to_navidrome: true,
                selection_rules: None,
            },
        )
        .await
        .unwrap();
        assert!(!preserved.enabled);
        // Re-enable for user alice via per-user override so it surfaces in June
        set_mix_enabled_in(
            &db,
            &alice,
            SetMixEnabledRequest {
                slug: "midsummer".into(),
                enabled: true,
            },
        )
        .await
        .unwrap();

        // Built-ins cannot be deleted (even via legacy alias "noel"), only disabled per-user
        let cant_del = delete_mix_definition_in(&db, &admin, "noel")
            .await
            .unwrap_err();
        assert_eq!(cant_del.code, FailureCode::InvalidInput);

        // Legacy alias "holiday" works consistently in set_mix_enabled_in
        set_mix_enabled_in(
            &db,
            &alice,
            SetMixEnabledRequest {
                slug: "holiday".into(),
                enabled: false,
            },
        )
        .await
        .unwrap();
        let dec = Utc.with_ymd_and_hms(2026, 12, 10, 14, 0, 0).unwrap();
        let mixes =
            list_active_contextual_mixes_in(&db, &NoNavidrome, None, &alice, dec, None, None, None)
                .await
                .unwrap();
        assert!(!mixes.iter().any(|m| m.slug == "winter_holiday"));

        // Custom seasonal mix is active on calendar in June (found by generator helper), hidden while empty,
        // and surfaces in list_active_contextual_mixes_in once a track exists.
        let june = Utc.with_ymd_and_hms(2026, 6, 21, 14, 0, 0).unwrap();
        let (_, june_cal) = list_calendar_active_seasonal_defs_in(&db, "alice", june)
            .await
            .unwrap();
        assert!(june_cal.iter().any(|(d, _)| d.slug == "midsummer"));
        let june_empty = list_active_contextual_mixes_in(
            &db,
            &NoNavidrome,
            None,
            &alice,
            june,
            None,
            None,
            None,
        )
        .await
        .unwrap();
        assert!(!june_empty.iter().any(|m| m.slug == "midsummer"));

        let folder_id = crate::models::folder::Folder::get_all_by_user_in(&db, "alice")
            .await
            .unwrap()[0]
            .id
            .clone();
        sqlx::query(
            "INSERT INTO discovery_tracks \
                (id, song_id, title, artist, album, path, folder_id, profile, status, \
                 exclude_from_core_taste, culture_bucket, pack_slug) \
             VALUES ('dt-mid-1', 'nd-mid-1', 'Sunlit Folk', 'Nordic Trio', 'Solstice', \
                     '/music/alice/Discovery/Seasonal/midsummer/01.flac', ?, \
                     'Seasonal:midsummer', 'Pending', 1, 'local', 'midsummer')",
        )
        .bind(&folder_id)
        .execute(&db)
        .await
        .unwrap();

        let june_mixes = list_active_contextual_mixes_in(
            &db,
            &NoNavidrome,
            None,
            &alice,
            june,
            None,
            None,
            None,
        )
        .await
        .unwrap();
        assert!(june_mixes.iter().any(|m| m.slug == "midsummer" && m.track_count == 1));

        delete_mix_definition_in(&db, &admin, "midsummer")
            .await
            .unwrap();
        assert!(ContextualMixStateRow::get_in(&db, "alice", "midsummer")
            .await
            .unwrap()
            .is_none());
    }

    #[tokio::test]
    async fn moment_mix_resolves_blended_owned_and_fresh_playlist_and_syncs_to_navidrome() {
        let db = test_db::migrated().await;
        let folder_id = folder_of(&db, "alice").await;
        let ctx = test_ctx("alice");

        // Stage one fresh non-seasonal Discovery track with structured JSON reason
        let reason_json = serde_json::to_string(&CandidateReason::MomentMatch {
            mix_slug: "drive".into(),
            mood_tag: Some("electronic".into()),
            genre: Some("indie rock".into()),
        })
        .unwrap();
        sqlx::query(
            "INSERT INTO discovery_tracks \
                (id, song_id, title, artist, album, path, folder_id, profile, status, \
                 exclude_from_core_taste, reason_json, culture_bucket, pack_slug) \
             VALUES ('dt-fresh-1', 'nd-fresh-1', 'Night Drive', 'Fresh Band', 'EP', \
                     '/music/alice/Discovery/Balanced/01.flac', ?, 'Balanced', 'Pending', \
                     0, ?, 'taste_wildcard', NULL)",
        )
        .bind(&folder_id)
        .bind(&reason_json)
        .execute(&db)
        .await
        .unwrap();

        let stub = NavidromeStub::start();
        stub.answer(
            "/rest/search3",
            &[
                &subsonic_ok(
                    r#""searchResult3":{"song":[
                        {"id":"nd-own-1","title":"Roadsong","artist":"Synth Act","album":"LP","genre":"Electronic","duration":210,"userRating":5,"playCount":12},
                        {"id":"nd-own-2","title":"Highway","artist":"Indie Act","album":"LP2","genre":"Indie Rock","duration":195,"userRating":4,"playCount":8},
                        {"id":"nd-disliked","title":"Bad Tune","artist":"Skip Band","album":"LP","genre":"Electronic","duration":180,"userRating":1,"playCount":1},
                        {"id":"nd-staged-path","title":"Staged Track","artist":"Staged Band","album":"EP","genre":"Electronic","duration":180,"path":"alice/Discovery/Balanced/Staged.flac"},
                        {"id":"nd-fresh-1","title":"Night Drive","artist":"Fresh Band","album":"EP","genre":"Electronic","duration":200},
                        {"id":"nd-xmas-1","title":"Snowfall","artist":"Winter Act","album":"Xmas","genre":"Holiday","duration":210,"path":"alice/Discovery/Seasonal/winter_holiday/01.flac"}
                    ]}"#,
                ),
                &subsonic_ok(
                    r#""searchResult3":{"song":[
                        {"id":"nd-own-1","title":"Roadsong","artist":"Synth Act","album":"LP","genre":"Electronic","duration":210,"userRating":5,"playCount":12},
                        {"id":"nd-own-2","title":"Highway","artist":"Indie Act","album":"LP2","genre":"Indie Rock","duration":195,"userRating":4,"playCount":8},
                        {"id":"nd-disliked","title":"Bad Tune","artist":"Skip Band","album":"LP","genre":"Electronic","duration":180,"userRating":1,"playCount":1},
                        {"id":"nd-staged-path","title":"Staged Track","artist":"Staged Band","album":"EP","genre":"Electronic","duration":180,"path":"alice/Discovery/Balanced/Staged.flac"},
                        {"id":"nd-fresh-1","title":"Night Drive","artist":"Fresh Band","album":"EP","genre":"Electronic","duration":200},
                        {"id":"nd-xmas-1","title":"Snowfall","artist":"Winter Act","album":"Xmas","genre":"Holiday","duration":210,"path":"alice/Discovery/Seasonal/winter_holiday/01.flac"}
                    ]}"#,
                ),
                &subsonic_ok(
                    r#""searchResult3":{"song":[
                        {"id":"nd-own-1","title":"Roadsong","artist":"Synth Act","album":"LP","genre":"Electronic","duration":210,"userRating":5,"playCount":12},
                        {"id":"nd-own-2","title":"Highway","artist":"Indie Act","album":"LP2","genre":"Indie Rock","duration":195,"userRating":4,"playCount":8},
                        {"id":"nd-disliked","title":"Bad Tune","artist":"Skip Band","album":"LP","genre":"Electronic","duration":180,"userRating":1,"playCount":1},
                        {"id":"nd-staged-path","title":"Staged Track","artist":"Staged Band","album":"EP","genre":"Electronic","duration":180,"path":"alice/Discovery/Balanced/Staged.flac"},
                        {"id":"nd-fresh-1","title":"Night Drive","artist":"Fresh Band","album":"EP","genre":"Electronic","duration":200},
                        {"id":"nd-xmas-1","title":"Snowfall","artist":"Winter Act","album":"Xmas","genre":"Holiday","duration":210,"path":"alice/Discovery/Seasonal/winter_holiday/01.flac"}
                    ]}"#,
                ),
            ],
        );
        stub.answer(
            "/rest/createPlaylist",
            &[&subsonic_ok(
                r#""playlist":{"id":"pl-drive-1","name":"Drive Mix","songCount":3,"duration":600,"owner":"alice","created":"2026-06-15T19:00:00Z","changed":"2026-06-15T19:00:00Z"}"#,
            )],
        );
        stub.answer("/rest/updatePlaylist", &[&subsonic_ok("")]);

        let now = Utc.with_ymd_and_hms(2026, 6, 15, 19, 0, 0).unwrap();
        // sync=false must stay strictly read-only (no createPlaylist call)
        let readonly = resolve_contextual_mix_playlist_in(
            &db,
            &StubNavidrome(stub.client()),
            None,
            &ctx,
            "drive",
            false,
            now,
        )
        .await
        .unwrap();
        assert_eq!(readonly.summary.navidrome_playlist_id, None);
        assert!(
            !stub.asked().iter().any(|p| p.contains("createPlaylist")),
            "sync=false must not create a static playlist"
        );
        assert_eq!(readonly.tracks.len(), 3);
        assert!(!readonly.tracks.iter().any(|t| t.song_id == "nd-disliked"));
        assert!(!readonly.tracks.iter().any(|t| t.song_id == "nd-staged-path"));

        // sync=true on a Moment mix syncs static playlist
        let playlist = resolve_contextual_mix_playlist_in(
            &db,
            &StubNavidrome(stub.client()),
            None,
            &ctx,
            "drive",
            true,
            now,
        )
        .await
        .unwrap();

        assert_eq!(playlist.summary.slug, "drive");
        assert_eq!(playlist.summary.i18n_key, "mix.moment.drive");
        assert_eq!(
            playlist.summary.navidrome_playlist_id.as_deref(),
            Some("pl-drive-1")
        );
        let meta_posts = stub.posted("/rest/updatePlaylist");
        assert!(
            meta_posts.iter().any(|body| {
                body.contains("playlistId=pl-drive-1")
                    && (body.contains("name=Drive+Mix") || body.contains("name=Drive%20Mix"))
                    && body.contains("comment=soulbeet%3Acontextual%3Adrive")
            }),
            "sync=true on Moment mix must sync canonical name and contextual comment via updatePlaylist; posted={meta_posts:?}"
        );
        assert_eq!(playlist.tracks.len(), 3);
        assert_eq!(playlist.tracks.iter().filter(|t| t.owned).count(), 2);
        assert_eq!(playlist.tracks.iter().filter(|t| !t.owned).count(), 1);
        assert!(playlist.tracks.iter().all(|t| t.reason.is_some()));

        // Seasonal mixes NEVER call static createPlaylist even when sync=true
        sqlx::query(
            "INSERT INTO discovery_tracks \
                (id, song_id, title, artist, album, path, folder_id, profile, status, \
                 exclude_from_core_taste, culture_bucket, pack_slug) \
             VALUES ('dt-xmas-1', 'nd-xmas-1', 'Snowfall', 'Winter Act', 'Xmas', \
                     '/music/alice/Discovery/Seasonal/winter_holiday/01.flac', ?, 'Balanced', \
                     'Pending', 1, 'local', 'winter_holiday')",
        )
        .bind(&folder_id)
        .execute(&db)
        .await
        .unwrap();
        let before_calls = stub.asked().len();
        let seasonal_pl = resolve_contextual_mix_playlist_in(
            &db,
            &StubNavidrome(stub.client()),
            None,
            &ctx,
            "winter_holiday",
            true,
            now,
        )
        .await
        .unwrap();
        assert_eq!(seasonal_pl.tracks.len(), 1);
        assert!(
            !stub.asked()[before_calls..]
                .iter()
                .any(|p| p.contains("createPlaylist")),
            "seasonal mix must never call static Navidrome playlist sync"
        );
    }

    #[tokio::test]
    async fn seasonal_reconciliation_skips_empty_packs_creates_when_populated_and_cleans_up() {
        let db = test_db::migrated().await;
        let folder_id = folder_of(&db, "alice").await;

        let stub = NavidromeStub::start();
        stub.answer(
            "/api/playlist",
            &[r#"{"id":"sp-winter-1","name":"Christmas Mix","comment":"soulbeet:contextual:winter_holiday","public":false}"#],
        );

        let place = NavidromePlace {
            library_id: 1,
            path: "alice/Discovery".to_string(),
        };

        // In December with 0 pending tracks, NO empty Navidrome smart playlist may be created!
        let dec = Utc.with_ymd_and_hms(2026, 12, 10, 12, 0, 0).unwrap();
        let skipped =
            reconcile_seasonal_playlists_in(&db, &stub.client(), "alice", &place, dec)
                .await
                .unwrap();
        assert!(skipped.is_empty());
        assert!(
            stub.asked().is_empty(),
            "empty seasonal pack must not create a Navidrome smart playlist"
        );

        // Once a pending seasonal track exists, smart playlist IS created and persisted
        sqlx::query(
            "INSERT INTO discovery_tracks \
                (id, song_id, title, artist, album, path, folder_id, profile, status, \
                 exclude_from_core_taste, culture_bucket, pack_slug) \
             VALUES ('dt-sp-1', 'nd-sp-1', 'Winter Lights', 'Nordic Act', 'Xmas', \
                     '/music/alice/Discovery/Seasonal/winter_holiday/01.flac', ?, \
                     'Seasonal:winter_holiday', 'Pending', 1, 'local', 'winter_holiday')",
        )
        .bind(&folder_id)
        .execute(&db)
        .await
        .unwrap();

        let created =
            reconcile_seasonal_playlists_in(&db, &stub.client(), "alice", &place, dec)
                .await
                .unwrap();
        assert!(created.contains(&"winter_holiday".to_string()));

        let state = ContextualMixStateRow::get_in(&db, "alice", "winter_holiday")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(state.navidrome_playlist_id.as_deref(), Some("sp-winter-1"));
        assert_eq!(state.expires_at.as_deref(), Some("2027-01-05"));

        // Remove the pending track -> after expiry (Jan 10, 2027) with no pending tracks,
        // even a 404 (already deleted) clears the stored ID
        sqlx::query("DELETE FROM discovery_tracks WHERE id = 'dt-sp-1'")
            .execute(&db)
            .await
            .unwrap();
        stub.answer_when("/api/playlist/sp-winter-1", "", 404, r#"{"error":"not found"}"#);
        let jan = Utc.with_ymd_and_hms(2027, 1, 10, 12, 0, 0).unwrap();
        reconcile_seasonal_playlists_in(&db, &stub.client(), "alice", &place, jan)
            .await
            .unwrap();

        let after = ContextualMixStateRow::get_in(&db, "alice", "winter_holiday")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(after.navidrome_playlist_id, None);

        // Disabling a seasonal mix before expires_at with 0 pending tracks also cleans up its
        // empty Navidrome smart playlist.
        ContextualMixStateRow::upsert_playlist_id_in(
            &db,
            "alice",
            "winter_holiday",
            Some("sp-winter-2"),
            Some("2027-01-05"),
        )
        .await
        .unwrap();
        ContextualMixStateRow::upsert_enabled_override_in(&db, "alice", "winter_holiday", false)
            .await
            .unwrap();
        stub.answer_when("/api/playlist/sp-winter-2", "", 200, r#"{}"#);
        reconcile_seasonal_playlists_in(&db, &stub.client(), "alice", &place, dec)
            .await
            .unwrap();
        let after_disable = ContextualMixStateRow::get_in(&db, "alice", "winter_holiday")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(after_disable.navidrome_playlist_id, None);
    }

    #[tokio::test]
    async fn start_seed_radio_passes_correct_arg_order_and_excludes_all_owned_tracks_from_prefetch()
    {
        use shared::recommendation::{ArtistRef, Neighbour, RecordingFacts, TrackRef, WeightedTag};
        use soulbeet::knowledge::KnowledgeError;
        use soulbeet::traits::TagOp;
        use std::sync::Mutex;

        struct StubKnowledge {
            calls: Mutex<Vec<(String, String, usize)>>,
        }

        #[async_trait]
        impl MusicKnowledge for StubKnowledge {
            fn name(&self) -> &'static str {
                "stub"
            }

            async fn resolve_artist(&self, name: &str) -> Result<Option<String>, KnowledgeError> {
                Ok((name == "Seed Artist").then(|| "mb-seed".into()))
            }

            async fn similar_artists(
                &self,
                seed_name: &str,
                seed_mbid: &str,
                limit: usize,
            ) -> Result<Vec<Neighbour>, KnowledgeError> {
                self.calls
                    .lock()
                    .unwrap()
                    .push((seed_name.into(), seed_mbid.into(), limit));
                assert_eq!(seed_name, "Seed Artist", "seed_name must be arg 1");
                assert_eq!(seed_mbid, "mb-seed", "seed_mbid must be arg 2");
                Ok(vec![Neighbour {
                    artist: ArtistRef {
                        name: "Neighbour Band".into(),
                        mbid: Some("mb-neighbour".into()),
                    },
                    score: 0.9,
                }])
            }

            async fn artist_top_tracks(
                &self,
                artist_mbid: &str,
                _limit: usize,
            ) -> Result<Vec<TrackRef>, KnowledgeError> {
                assert_eq!(artist_mbid, "mb-neighbour");
                Ok(vec![
                    // Already owned in library (even though NOT selected into owned_tracks!)
                    TrackRef {
                        artist: ArtistRef {
                            name: "Neighbour Band".into(),
                            mbid: Some("mb-neighbour".into()),
                        },
                        title: "Second Owned Track".into(),
                        mbid: Some("rec-owned-2".into()),
                    },
                    // Truly unowned -> must be chosen for prefetch_candidates
                    TrackRef {
                        artist: ArtistRef {
                            name: "Neighbour Band".into(),
                            mbid: Some("mb-neighbour".into()),
                        },
                        title: "Brand New Track".into(),
                        mbid: Some("rec-new".into()),
                    },
                ])
            }

            async fn artist_tags(
                &self,
                _: &[String],
            ) -> Result<HashMap<String, Vec<WeightedTag>>, KnowledgeError> {
                Ok(HashMap::new())
            }

            async fn artist_areas(
                &self,
                _: &[String],
            ) -> Result<HashMap<String, Option<String>>, KnowledgeError> {
                Ok(HashMap::new())
            }

            async fn artist_popularity(
                &self,
                _: &[String],
            ) -> Result<HashMap<String, u64>, KnowledgeError> {
                Ok(HashMap::new())
            }

            async fn popularity_reference(&self) -> Result<u64, KnowledgeError> {
                Ok(1)
            }

            async fn related_tags(&self, _: &str) -> Result<Vec<String>, KnowledgeError> {
                Ok(vec![])
            }

            async fn tag_tracks(
                &self,
                _: &str,
                _: usize,
            ) -> Result<Vec<TrackRef>, KnowledgeError> {
                Ok(vec![])
            }

            async fn multi_tag_tracks(
                &self,
                _: &[&str],
                _: TagOp,
                _: Option<(u8, u8)>,
                _: usize,
            ) -> Result<Vec<TrackRef>, KnowledgeError> {
                Ok(vec![])
            }

            async fn recordings(
                &self,
                _: &[String],
            ) -> Result<HashMap<String, RecordingFacts>, KnowledgeError> {
                Ok(HashMap::new())
            }
        }

        let stub = NavidromeStub::start();
        stub.answer(
            "/rest/getSong",
            &[&subsonic_ok(
                r#""song":{"id":"s-seed","title":"Anchor","artist":"Seed Artist","album":"LP","duration":200}"#,
            )],
        );
        stub.answer(
            "/rest/search3",
            &[&subsonic_ok(
                r#""searchResult3":{"song":[
                    {"id":"s-seed","title":"Anchor","artist":"Seed Artist","album":"LP","duration":200},
                    {"id":"s-seed-2","title":"Anchor B","artist":"Seed Artist","album":"LP","duration":180},
                    {"id":"s-n1","title":"First Owned Track","artist":"Neighbour Band","album":"N1","duration":210},
                    {"id":"s-n2","title":"Second Owned Track","artist":"Neighbour Band","album":"N2","duration":220}
                ]}"#,
            )],
        );

        let kb = StubKnowledge {
            calls: Mutex::new(Vec::new()),
        };
        let ctx = test_ctx("alice");
        let resp = start_seed_radio_in(
            &StubNavidrome(stub.client()),
            &kb,
            &ctx,
            &SeedRadioRequest {
                seed_song_id: Some("s-seed".into()),
                seed_artist: None,
                seed_track: None,
                seed_artist_mbid: None,
                limit: Some(10),
            },
        )
        .await
        .unwrap();

        assert_eq!(
            kb.calls.lock().unwrap().as_slice(),
            &[("Seed Artist".to_string(), "mb-seed".to_string(), 12)]
        );
        assert_eq!(resp.owned_tracks.len(), 3);
        assert_eq!(resp.owned_tracks[0].song_id, "s-seed");
        assert_eq!(resp.owned_tracks[1].song_id, "s-seed-2");
        assert_eq!(resp.owned_tracks[2].song_id, "s-n1");
        assert_eq!(resp.prefetch_candidates.len(), 1);
        assert_eq!(resp.prefetch_candidates[0].track, "Brand New Track");
    }

    #[tokio::test]
    async fn triage_queue_lists_pending_tracks_with_structured_reason_and_anti_pollution_flags() {
        let db = test_db::migrated().await;
        let folder_id = folder_of(&db, "alice").await;
        let reason_json = serde_json::to_string(&CandidateReason::RegionalContextual {
            mix_slug: "winter_holiday".into(),
            country_code: Some("JP".into()),
        })
        .unwrap();
        sqlx::query(
            "INSERT INTO discovery_tracks \
                (id, song_id, title, artist, album, path, folder_id, profile, status, \
                 exclude_from_core_taste, reason_json, culture_bucket, pack_slug) \
             VALUES ('dt-holiday', 's-holiday', 'Winter Song', 'Regional Artist', 'Holiday', \
                     '/music/alice/Discovery/Seasonal/winter_holiday/01.flac', ?, 'Balanced', \
                     'Pending', 1, ?, 'local', 'winter_holiday')",
        )
        .bind(&folder_id)
        .bind(&reason_json)
        .execute(&db)
        .await
        .unwrap();

        let queue = list_triage_queue_in(&db, &NoNavidrome, None, "alice")
            .await
            .unwrap();
        assert_eq!(queue.len(), 1);
        assert!(queue[0].exclude_from_core_taste);
        assert_eq!(
            queue[0].reason.as_deref(),
            Some(&CandidateReason::RegionalContextual {
                mix_slug: "winter_holiday".into(),
                country_code: Some("JP".into()),
            })
        );
        assert_eq!(queue[0].culture_bucket.as_deref(), Some("local"));
        assert_eq!(queue[0].pack_slug.as_deref(), Some("winter_holiday"));
    }

    #[tokio::test]
    async fn discovery_song_id_reconciliation_matches_paths_persists_and_short_circuits_fast_path()
    {
        let db = test_db::migrated().await;
        let folder_id = folder_of(&db, "alice").await;

        // 1. Exact NAVIDROME_MUSIC_PATH mapped match
        sqlx::query(
            "INSERT INTO discovery_tracks (id, song_id, title, artist, album, path, folder_id, profile, status) \
             VALUES ('dt-path', NULL, 'Midnight Sun', 'Path Artist', 'EP', \
                     '/local/music/alice/Discovery/Balanced/01.flac', ?, 'Balanced', 'Pending')",
        )
        .bind(&folder_id)
        .execute(&db)
        .await
        .unwrap();

        // 2. Discovery-scoped (artist, title) match that MUST NOT bind to the owned library copy
        sqlx::query(
            "INSERT INTO discovery_tracks (id, song_id, title, artist, album, path, folder_id, profile, status) \
             VALUES ('dt-scoped', '', 'Echoes', 'Twin Act', 'Single', \
                     '/local/music/alice/Discovery/Balanced/unmatched-name.flac', ?, 'Balanced', 'Pending')",
        )
        .bind(&folder_id)
        .execute(&db)
        .await
        .unwrap();

        let stub = NavidromeStub::start();
        stub.answer(
            "/rest/search3",
            &[&subsonic_ok(
                r#""searchResult3":{"song":[
                    {"id":"nd-owned-echoes","title":"Echoes","artist":"Twin Act","album":"LP","path":"alice/Twin Act/LP/Echoes.flac"},
                    {"id":"nd-staged-echoes","title":"Echoes","artist":"Twin Act","album":"Single","path":"alice/Discovery/Balanced/different-file.flac"},
                    {"id":"nd-path-match","title":"Different Tag","artist":"Path Artist","album":"EP","path":"alice/Discovery/Balanced/01.flac"}
                ]}"#,
            )],
        );

        let queue = list_triage_queue_in(
            &db,
            &StubNavidrome(stub.client()),
            Some(Path::new("/local/music")),
            "alice",
        )
        .await
        .unwrap();
        assert_eq!(queue.len(), 2);
        let by_id: HashMap<&str, Option<&str>> = queue
            .iter()
            .map(|t| (t.id.as_str(), t.song_id.as_deref()))
            .collect();
        assert_eq!(by_id.get("dt-path"), Some(&Some("nd-path-match")));
        assert_eq!(by_id.get("dt-scoped"), Some(&Some("nd-staged-echoes")));
        assert_eq!(
            stub.asked()
                .iter()
                .filter(|p| p.as_str() == "/rest/search3")
                .count(),
            1
        );

        // Persisted in SQLite: `maybe_reconcile_discovery_song_ids_in` hits SQLite ONLY with zero
        // Navidrome requests on already-reconciled rows!
        let persisted = DiscoveryTrackRow::get_by_id_in(&db, "dt-path")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(persisted.song_id.as_deref(), Some("nd-path-match"));

        let mut already_reconciled = DiscoveryTrackRow::pending_for_user_in(&db, "alice")
            .await
            .unwrap();
        let asked_before = stub.asked().len();
        let second_reconciled = maybe_reconcile_discovery_song_ids_in(
            &db,
            &StubNavidrome(stub.client()),
            Some(Path::new("/local/music")),
            "alice",
            &mut already_reconciled,
        )
        .await;
        assert_eq!(second_reconciled, 0);
        assert_eq!(
            stub.asked().len(),
            asked_before,
            "once reconciled, maybe_reconcile_discovery_song_ids_in must not call Navidrome again"
        );
    }

    #[tokio::test]
    async fn reconciliation_falls_back_to_unique_artist_title_only_when_report_real_path_is_disabled()
    {
        let db = test_db::migrated().await;
        let folder_id = folder_of(&db, "alice").await;
        sqlx::query(
            "INSERT INTO discovery_tracks (id, song_id, title, artist, album, path, folder_id, profile, status) \
             VALUES ('dt-uniq', NULL, 'Unique Song', 'Solo Artist', 'LP', \
                     '/music/alice/Discovery/Balanced/01.flac', ?, 'Balanced', 'Pending'), \
                    ('dt-dup', NULL, 'Ambiguous', 'Dup Artist', 'LP', \
                     '/music/alice/Discovery/Balanced/02.flac', ?, 'Balanced', 'Pending')",
        )
        .bind(&folder_id)
        .bind(&folder_id)
        .execute(&db)
        .await
        .unwrap();

        // No song has a Discovery/ path (simulates ReportRealPath=false in Navidrome)
        let library = vec![
            SubsonicSong {
                id: "nd-uniq".into(),
                title: "Unique Song".into(),
                artist: Some("Solo Artist".into()),
                album: Some("LP".into()),
                album_id: None,
                genre: None,
                genres: vec![],
                duration: Some(200),
                user_rating: None,
                play_count: None,
                created: None,
                played: None,
                starred: None,
                music_brainz_id: None,
                suffix: None,
                bit_rate: None,
                size: None,
                year: None,
                track: None,
                path: Some("Solo Artist/LP/Unique Song.flac".into()),
            },
            SubsonicSong {
                id: "nd-dup-1".into(),
                title: "Ambiguous".into(),
                artist: Some("Dup Artist".into()),
                album: Some("LP".into()),
                album_id: None,
                genre: None,
                genres: vec![],
                duration: Some(180),
                user_rating: None,
                play_count: None,
                created: None,
                played: None,
                starred: None,
                music_brainz_id: None,
                suffix: None,
                bit_rate: None,
                size: None,
                year: None,
                track: None,
                path: Some("Dup Artist/LP/Ambiguous.flac".into()),
            },
            SubsonicSong {
                id: "nd-dup-2".into(),
                title: "Ambiguous".into(),
                artist: Some("Dup Artist".into()),
                album: Some("Single".into()),
                album_id: None,
                genre: None,
                genres: vec![],
                duration: Some(180),
                user_rating: None,
                play_count: None,
                created: None,
                played: None,
                starred: None,
                music_brainz_id: None,
                suffix: None,
                bit_rate: None,
                size: None,
                year: None,
                track: None,
                path: Some("Dup Artist/Single/Ambiguous.flac".into()),
            },
        ];

        let mut tracks = DiscoveryTrackRow::pending_for_user_in(&db, "alice")
            .await
            .unwrap();
        let count = reconcile_discovery_song_ids_in(&db, &library, None, &mut tracks).await;
        assert_eq!(count, 1);
        let uniq = tracks.iter().find(|t| t.id == "dt-uniq").unwrap();
        let dup = tracks.iter().find(|t| t.id == "dt-dup").unwrap();
        assert_eq!(uniq.song_id.as_deref(), Some("nd-uniq"));
        assert_eq!(dup.song_id, None, "ambiguous fallback must not bind");
    }

    #[tokio::test]
    async fn moment_mix_summary_track_count_matches_resolved_playlist_and_reconciles_fresh_tracks()
    {
        let db = test_db::migrated().await;
        let folder_id = folder_of(&db, "alice").await;
        let ctx = test_ctx("alice");

        // Fresh pending discovery track with song_id = NULL (pre-reconciliation!)
        sqlx::query(
            "INSERT INTO discovery_tracks \
                (id, song_id, title, artist, album, path, folder_id, profile, status, \
                 exclude_from_core_taste, culture_bucket, pack_slug) \
             VALUES ('dt-fresh-unrec', NULL, 'Sunset Cruise', 'New Wave Act', 'Single', \
                     '/music/alice/Discovery/Balanced/Sunset Cruise.flac', ?, 'Balanced', 'Pending', \
                     0, 'taste_wildcard', NULL)",
        )
        .bind(&folder_id)
        .execute(&db)
        .await
        .unwrap();

        let library_resp = subsonic_ok(
            r#""searchResult3":{"song":[
                {"id":"nd-own-1","title":"Song 1","artist":"Repeated Artist","album":"LP","genre":"Electronic","duration":210,"userRating":5,"playCount":12},
                {"id":"nd-own-2","title":"Song 2","artist":"Repeated Artist","album":"LP","genre":"Electronic","duration":200,"userRating":5,"playCount":11},
                {"id":"nd-own-3","title":"Song 3","artist":"Repeated Artist","album":"LP","genre":"Electronic","duration":190,"userRating":5,"playCount":10},
                {"id":"nd-own-4","title":"Song 4","artist":"Other Artist","album":"LP","genre":"Indie Rock","duration":195,"userRating":4,"playCount":8},
                {"id":"nd-disliked","title":"Skip Me","artist":"Bad Act","album":"LP","genre":"Electronic","duration":180,"userRating":1,"playCount":1},
                {"id":"nd-fresh-reconciled","title":"Sunset Cruise","artist":"New Wave Act","album":"Single","genre":"Electronic","duration":205,"path":"alice/Discovery/Balanced/Sunset Cruise.flac"}
            ]}"#,
        );

        let stub = NavidromeStub::start();
        stub.answer("/rest/search3", &[&library_resp, &library_resp]);

        let now = Utc.with_ymd_and_hms(2026, 6, 15, 19, 0, 0).unwrap();
        let mixes = list_active_contextual_mixes_in(
            &db,
            &StubNavidrome(stub.client()),
            Some(Path::new("/music")),
            &ctx,
            now,
            Some(19),
            None,
            None,
        )
        .await
        .unwrap();

        // Only ONE Navidrome library fetch for the whole list call
        assert_eq!(
            stub.asked()
                .iter()
                .filter(|p| p.as_str() == "/rest/search3")
                .count(),
            1
        );

        let drive_summary = mixes.iter().find(|m| m.slug == "drive").unwrap();
        // Owned: max 2 from "Repeated Artist" (MAX_TRACKS_PER_ARTIST=2) + 1 from "Other Artist" = 3 owned
        // Fresh: 1 freshly reconciled pending track = 1 fresh -> total 4 playable tracks!
        assert_eq!(drive_summary.track_count, 4);

        let resolved = resolve_contextual_mix_playlist_in(
            &db,
            &StubNavidrome(stub.client()),
            Some(Path::new("/music")),
            &ctx,
            "drive",
            false,
            now,
        )
        .await
        .unwrap();

        assert_eq!(
            drive_summary.track_count,
            resolved.tracks.len() as u32,
            "summary track_count must match resolved playlist length"
        );
        assert_eq!(resolved.tracks.iter().filter(|t| t.owned).count(), 3);
        assert_eq!(resolved.tracks.iter().filter(|t| !t.owned).count(), 1);
        assert!(resolved
            .tracks
            .iter()
            .any(|t| !t.owned && t.song_id == "nd-fresh-reconciled" && t.duration_s == Some(205)));
    }

    #[tokio::test]
    async fn reconciliation_rejects_bare_filename_real_owned_paths_and_duplicate_song_ids() {
        let db = test_db::migrated().await;
        let folder_id = folder_of(&db, "alice").await;

        let make_song = |id: &str, title: &str, artist: &str, path: &str| SubsonicSong {
            id: id.into(),
            title: title.into(),
            artist: Some(artist.into()),
            album: Some("LP".into()),
            album_id: None,
            genre: None,
            genres: vec![],
            duration: Some(200),
            user_rating: None,
            play_count: None,
            created: None,
            played: None,
            starred: None,
            music_brainz_id: None,
            suffix: None,
            bit_rate: None,
            size: None,
            year: None,
            track: None,
            path: Some(path.into()),
        };

        // (a) Bare filename `01.flac` in a different Discovery folder must NEVER bind to wrong song
        sqlx::query(
            "INSERT INTO discovery_tracks (id, song_id, title, artist, album, path, folder_id, profile, status) \
             VALUES ('dt-bare', NULL, 'Actual Track', 'Actual Artist', 'EP', \
                     '/music/alice/Discovery/Adventurous/01.flac', ?, 'Adventurous', 'Pending')",
        )
        .bind(&folder_id)
        .execute(&db)
        .await
        .unwrap();

        let library_other_disc = vec![make_song(
            "nd-wrong-01",
            "Totally Different Song",
            "Other Artist",
            "alice/Discovery/Balanced/01.flac",
        )];
        let mut tracks = DiscoveryTrackRow::pending_for_user_in(&db, "alice")
            .await
            .unwrap();
        let count = reconcile_discovery_song_ids_in(
            &db,
            &library_other_disc,
            Some(Path::new("/music")),
            &mut tracks,
        )
        .await;
        assert_eq!(count, 0, "bare filename 01.flac mismatch must not bind");
        assert_eq!(tracks[0].song_id, None);

        // (b) Owned library song with real non-Discovery path must NOT bind when Discovery/ hasn't scanned yet
        let library_real_abs = vec![make_song(
            "nd-owned-abs",
            "Actual Track",
            "Actual Artist",
            "/music/alice/Actual Artist/EP/Actual Track.flac",
        )];
        let count_abs = reconcile_discovery_song_ids_in(
            &db,
            &library_real_abs,
            Some(Path::new("/music")),
            &mut tracks,
        )
        .await;
        assert_eq!(
            count_abs, 0,
            "absolute non-Discovery real path must block owned-song fallback"
        );
        assert_eq!(tracks[0].song_id, None);

        let library_real_rel = vec![make_song(
            "nd-owned-rel",
            "Actual Track",
            "Actual Artist",
            "music/Actual Artist/EP/01.flac",
        )];
        let count_rel =
            reconcile_discovery_song_ids_in(&db, &library_real_rel, None, &mut tracks).await;
        assert_eq!(
            count_rel, 0,
            "relative non-Discovery real path must block owned-song fallback"
        );
        assert_eq!(tracks[0].song_id, None);

        // (c) Duplicate song_id must NEVER be assigned to two pending rows
        sqlx::query(
            "INSERT INTO discovery_tracks (id, song_id, title, artist, album, path, folder_id, profile, status) \
             VALUES ('dt-dup-a', NULL, 'Same File', 'Artist A', 'EP', \
                     '/music/alice/Discovery/Balanced/dup.flac', ?, 'Balanced', 'Pending'), \
                    ('dt-dup-b', NULL, 'Same File', 'Artist A', 'EP', \
                     '/music/alice/Discovery/Balanced/dup.flac', ?, 'Balanced', 'Pending')",
        )
        .bind(&folder_id)
        .bind(&folder_id)
        .execute(&db)
        .await
        .unwrap();

        let library_single_disc = vec![make_song(
            "nd-dup-target",
            "Same File",
            "Artist A",
            "alice/Discovery/Balanced/dup.flac",
        )];
        let mut all_tracks = DiscoveryTrackRow::pending_for_user_in(&db, "alice")
            .await
            .unwrap();
        let matched = reconcile_discovery_song_ids_in(
            &db,
            &library_single_disc,
            Some(Path::new("/music")),
            &mut all_tracks,
        )
        .await;
        assert_eq!(matched, 1, "only one pending row may claim a given song_id");
        let assigned_count = all_tracks
            .iter()
            .filter(|t| t.song_id.as_deref() == Some("nd-dup-target"))
            .count();
        assert_eq!(assigned_count, 1, "song_id must never be reused across rows");
    }

    #[tokio::test]
    async fn seasonal_track_count_matches_resolved_playlist_and_rolling_slug_resolves() {
        let db = test_db::migrated().await;
        let folder_id = folder_of(&db, "alice").await;
        let ctx = test_ctx("alice");

        // Seasonal: 1 playable (song_id set) + 1 unreconciled (song_id NULL)
        sqlx::query(
            "INSERT INTO discovery_tracks \
                (id, song_id, title, artist, album, path, folder_id, profile, status, \
                 exclude_from_core_taste, culture_bucket, pack_slug) \
             VALUES ('dt-seas-ok', 'nd-seas-1', 'Winter Lights', 'Nordic Act', 'Xmas', \
                     '/music/alice/Discovery/Seasonal/winter_holiday/01.flac', ?, 'Balanced', \
                     'Pending', 1, 'local', 'winter_holiday'), \
                    ('dt-seas-null', NULL, 'Unscanned Carol', 'Missing Act', 'Xmas', \
                     '/music/alice/Discovery/Seasonal/winter_holiday/02.flac', ?, 'Balanced', \
                     'Pending', 1, 'local', 'winter_holiday')",
        )
        .bind(&folder_id)
        .bind(&folder_id)
        .execute(&db)
        .await
        .unwrap();

        // Rolling (Balanced): 1 playable + 1 unreconciled
        sqlx::query(
            "INSERT INTO discovery_tracks \
                (id, song_id, title, artist, album, path, folder_id, profile, status, \
                 exclude_from_core_taste, culture_bucket, pack_slug) \
             VALUES ('dt-roll-ok', 'nd-roll-1', 'Balanced Gem', 'Indie Act', 'LP', \
                     '/music/alice/Discovery/Balanced/01.flac', ?, 'Balanced', \
                     'Pending', 0, 'taste_wildcard', NULL), \
                    ('dt-roll-null', NULL, 'Unscanned Gem', 'Ghost Act', 'LP', \
                     '/music/alice/Discovery/Balanced/02.flac', ?, 'Balanced', \
                     'Pending', 0, 'taste_wildcard', NULL)",
        )
        .bind(&folder_id)
        .bind(&folder_id)
        .execute(&db)
        .await
        .unwrap();

        let dec = Utc.with_ymd_and_hms(2026, 12, 10, 14, 0, 0).unwrap();
        let mixes = list_active_contextual_mixes_in(
            &db,
            &NoNavidrome,
            None,
            &ctx,
            dec,
            Some(14),
            None,
            None,
        )
        .await
        .unwrap();

        let winter_summary = mixes.iter().find(|m| m.slug == "winter_holiday").unwrap();
        assert_eq!(
            winter_summary.track_count, 1,
            "seasonal summary must count only playable tracks with song_id"
        );
        let winter_pl = resolve_contextual_mix_playlist_in(
            &db,
            &NoNavidrome,
            None,
            &ctx,
            "winter_holiday",
            false,
            dec,
        )
        .await
        .unwrap();
        assert_eq!(winter_summary.track_count, winter_pl.tracks.len() as u32);
        assert_eq!(winter_pl.tracks[0].song_id, "nd-seas-1");

        let balanced_summary = mixes.iter().find(|m| m.slug == "balanced").unwrap();
        assert_eq!(balanced_summary.kind, ContextualMixKind::Rolling);
        assert_eq!(
            balanced_summary.track_count, 1,
            "rolling summary must count only playable tracks with song_id"
        );
        let balanced_pl = resolve_contextual_mix_playlist_in(
            &db,
            &NoNavidrome,
            None,
            &ctx,
            "balanced",
            false,
            dec,
        )
        .await
        .unwrap();
        assert_eq!(balanced_pl.summary.slug, "balanced");
        assert_eq!(balanced_pl.summary.kind, ContextualMixKind::Rolling);
        assert_eq!(
            balanced_summary.track_count,
            balanced_pl.tracks.len() as u32
        );
        assert_eq!(balanced_pl.tracks.len(), 1);
        assert_eq!(balanced_pl.tracks[0].song_id, "nd-roll-1");
    }

    #[tokio::test]
    async fn owned_holiday_library_tracks_match_winter_holiday_without_false_positives() {
        let db = test_db::migrated().await;
        folder_of(&db, "alice").await;
        let ctx = test_ctx("alice");

        let library_resp = subsonic_ok(
            r#""searchResult3":{"song":[
                {"id":"nd-xmas-1","title":"All I Want for Christmas Is You","artist":"Mariah Carey","album":"Merry Christmas","genre":"Pop","duration":241,"userRating":5,"starred":"2026-12-01T00:00:00Z","playCount":20},
                {"id":"nd-xmas-2","title":"Petit Papa Noël","artist":"Tino Rossi","album":"Chants de Noël","genre":"Chanson","duration":188,"userRating":4,"playCount":10},
                {"id":"nd-xmas-3","title":"Jingle Bell Rock","artist":"Bobby Helms","album":"Holiday Favorites","genre":"Rock","duration":130,"userRating":0,"playCount":5},
                {"id":"nd-xmas-4","title":"Silent Night","artist":"Nat King Cole","album":"The Magic of Christmas","genre":"Jazz","duration":165,"playCount":7},
                {"id":"nd-xmas-5","title":"Winter Wonderland","artist":"Bing Crosby","album":"Classic Winter","genre":"Holiday","duration":150,"playCount":4},
                {"id":"nd-xmas-6","title":"Carol of the Bells","artist":"Trans-Siberian Orchestra","album":"Christmas Eve","genre":"Rock","duration":205,"playCount":6},
                {"id":"nd-xmas-7","title":"Snowy Evening","artist":"Cozy Trio","album":"December Nights","genre":"Holiday","duration":180,"playCount":3},
                {"id":"nd-non-1","title":"Bohemian Rhapsody","artist":"Queen","album":"A Night at the Opera","genre":"Rock","duration":354,"userRating":5,"playCount":50},
                {"id":"nd-non-2","title":"Wonderwall","artist":"Noel Gallagher","album":"Morning Glory","genre":"Britpop","duration":258,"userRating":5,"playCount":30},
                {"id":"nd-non-3","title":"Roadsong","artist":"Synth Act","album":"LP","genre":"Electronic","duration":210,"userRating":5,"playCount":25},
                {"id":"nd-non-4","title":"Strange Fruit","artist":"Billie Holiday","album":"Lady Sings the Blues","genre":"Jazz","duration":185,"userRating":5,"playCount":40},
                {"id":"nd-non-5","title":"Holiday","artist":"Green Day","album":"American Idiot","genre":"Punk Rock","duration":232,"userRating":5,"playCount":35},
                {"id":"nd-non-6","title":"Holiday","artist":"Madonna","album":"Madonna","genre":"Pop","duration":368,"userRating":4,"playCount":20},
                {"id":"nd-non-7","title":"Carol","artist":"Chuck Berry","album":"Berry Is on Top","genre":"Rock","duration":168,"userRating":5,"playCount":18},
                {"id":"nd-non-8","title":"Oh! Carol","artist":"Neil Sedaka","album":"Rock with Sedaka","genre":"Pop","duration":137,"userRating":4,"playCount":15},
                {"id":"nd-non-9","title":"Golden Sky","artist":"The Holidays","album":"Post Paradise","genre":"Indie","duration":215,"userRating":4,"playCount":12}
            ]}"#,
        );

        let stub = NavidromeStub::start();
        stub.answer("/rest/search3", &[&library_resp, &library_resp]);

        let dec = Utc.with_ymd_and_hms(2026, 12, 10, 14, 0, 0).unwrap();
        let mixes = list_active_contextual_mixes_in(
            &db,
            &StubNavidrome(stub.client()),
            None,
            &ctx,
            dec,
            Some(14),
            None,
            None,
        )
        .await
        .unwrap();

        let winter = mixes
            .iter()
            .find(|m| m.slug == "winter_holiday")
            .expect("owned holiday tracks must immediately surface winter_holiday");
        assert_eq!(winter.track_count, 7);

        let pl = resolve_contextual_mix_playlist_in(
            &db,
            &StubNavidrome(stub.client()),
            None,
            &ctx,
            "winter_holiday",
            false,
            dec,
        )
        .await
        .unwrap();
        assert_eq!(pl.tracks.len(), 7);
        assert!(pl.tracks.iter().all(|t| t.owned));
        assert!(!pl.tracks.iter().any(|t| t.song_id.starts_with("nd-non-")));
        assert!(pl.tracks.iter().any(|t| t.song_id == "nd-xmas-6"));
        assert!(pl.tracks.iter().any(|t| t.song_id == "nd-xmas-7"));

        let mariah = pl.tracks.iter().find(|t| t.song_id == "nd-xmas-1").unwrap();
        assert_eq!(mariah.rating, Some(5));
        assert!(mariah.starred);

        let jingle = pl.tracks.iter().find(|t| t.song_id == "nd-xmas-3").unwrap();
        assert_eq!(jingle.rating, None, "rating 0 must normalize to None");
        assert!(!jingle.starred);
    }

    #[tokio::test]
    async fn contextual_mix_triage_and_seed_radio_hydrate_rating_and_starred() {
        use shared::recommendation::{Neighbour, RecordingFacts, TrackRef, WeightedTag};
        use soulbeet::knowledge::KnowledgeError;
        use soulbeet::traits::TagOp;

        struct EmptyKnowledge;

        #[async_trait]
        impl MusicKnowledge for EmptyKnowledge {
            fn name(&self) -> &'static str {
                "empty"
            }
            async fn resolve_artist(&self, _: &str) -> Result<Option<String>, KnowledgeError> {
                Ok(None)
            }
            async fn similar_artists(
                &self,
                _: &str,
                _: &str,
                _: usize,
            ) -> Result<Vec<Neighbour>, KnowledgeError> {
                Ok(vec![])
            }
            async fn artist_top_tracks(
                &self,
                _: &str,
                _: usize,
            ) -> Result<Vec<TrackRef>, KnowledgeError> {
                Ok(vec![])
            }
            async fn artist_tags(
                &self,
                _: &[String],
            ) -> Result<HashMap<String, Vec<WeightedTag>>, KnowledgeError> {
                Ok(HashMap::new())
            }
            async fn artist_areas(
                &self,
                _: &[String],
            ) -> Result<HashMap<String, Option<String>>, KnowledgeError> {
                Ok(HashMap::new())
            }
            async fn artist_popularity(
                &self,
                _: &[String],
            ) -> Result<HashMap<String, u64>, KnowledgeError> {
                Ok(HashMap::new())
            }
            async fn popularity_reference(&self) -> Result<u64, KnowledgeError> {
                Ok(1)
            }
            async fn related_tags(&self, _: &str) -> Result<Vec<String>, KnowledgeError> {
                Ok(vec![])
            }
            async fn tag_tracks(&self, _: &str, _: usize) -> Result<Vec<TrackRef>, KnowledgeError> {
                Ok(vec![])
            }
            async fn multi_tag_tracks(
                &self,
                _: &[&str],
                _: TagOp,
                _: Option<(u8, u8)>,
                _: usize,
            ) -> Result<Vec<TrackRef>, KnowledgeError> {
                Ok(vec![])
            }
            async fn recordings(
                &self,
                _: &[String],
            ) -> Result<HashMap<String, RecordingFacts>, KnowledgeError> {
                Ok(HashMap::new())
            }
        }

        let db = test_db::migrated().await;
        let folder_id = folder_of(&db, "alice").await;
        let ctx = test_ctx("alice");

        // Staged track 1: rated in Navidrome (4) + starred; DB rating NULL
        // Staged track 2: unrated in Navidrome (0); DB rating 3 (fallback must apply)
        sqlx::query(
            "INSERT INTO discovery_tracks \
                (id, song_id, title, artist, album, path, folder_id, profile, rating, status) \
             VALUES ('dt-r1', 'nd-r1', 'Staged Live Rated', 'Artist A', 'EP', \
                     '/music/alice/Discovery/Balanced/01.flac', ?, 'Balanced', NULL, 'Pending'), \
                    ('dt-r2', 'nd-r2', 'Staged DB Fallback', 'Artist B', 'EP', \
                     '/music/alice/Discovery/Balanced/02.flac', ?, 'Balanced', 3, 'Pending')",
        )
        .bind(&folder_id)
        .bind(&folder_id)
        .execute(&db)
        .await
        .unwrap();

        let library_resp = subsonic_ok(
            r#""searchResult3":{"song":[
                {"id":"nd-r1","title":"Staged Live Rated","artist":"Artist A","album":"EP","duration":200,"userRating":4,"starred":"2026-12-01T00:00:00Z","path":"alice/Discovery/Balanced/01.flac"},
                {"id":"nd-r2","title":"Staged DB Fallback","artist":"Artist B","album":"EP","duration":180,"userRating":0,"path":"alice/Discovery/Balanced/02.flac"},
                {"id":"nd-seed","title":"Anchor Song","artist":"Seed Act","album":"LP","duration":210,"userRating":5,"starred":"2026-11-20T00:00:00Z"},
                {"id":"nd-seed-2","title":"Anchor B","artist":"Seed Act","album":"LP","duration":190,"userRating":0}
            ]}"#,
        );

        let stub = NavidromeStub::start();
        stub.answer("/rest/search3", &[&library_resp, &library_resp, &library_resp]);
        stub.answer(
            "/rest/getSong",
            &[&subsonic_ok(
                r#""song":{"id":"nd-seed","title":"Anchor Song","artist":"Seed Act","album":"LP","duration":210,"userRating":5,"starred":"2026-11-20T00:00:00Z"}"#,
            )],
        );

        // 1. Rolling ContextualMixPlaylist hydrates rating + starred + fallback
        let now = Utc.with_ymd_and_hms(2026, 12, 10, 14, 0, 0).unwrap();
        let rolling = resolve_contextual_mix_playlist_in(
            &db,
            &StubNavidrome(stub.client()),
            None,
            &ctx,
            "balanced",
            false,
            now,
        )
        .await
        .unwrap();
        let t1 = rolling.tracks.iter().find(|t| t.song_id == "nd-r1").unwrap();
        let t2 = rolling.tracks.iter().find(|t| t.song_id == "nd-r2").unwrap();
        assert_eq!(t1.rating, Some(4));
        assert!(t1.starred);
        assert_eq!(t2.rating, Some(3), "must fall back to DB rating when Navidrome has 0");
        assert!(!t2.starred);

        // 2. Triage queue hydrates live Navidrome rating with DB fallback
        let triage = list_triage_queue_in(&db, &StubNavidrome(stub.client()), None, "alice")
            .await
            .unwrap();
        let tr1 = triage.iter().find(|t| t.id == "dt-r1").unwrap();
        let tr2 = triage.iter().find(|t| t.id == "dt-r2").unwrap();
        assert_eq!(tr1.rating, Some(4));
        assert_eq!(tr2.rating, Some(3));

        // 3. SeedRadioTrack hydrates rating + starred
        let radio = start_seed_radio_in(
            &StubNavidrome(stub.client()),
            &EmptyKnowledge,
            &ctx,
            &SeedRadioRequest {
                seed_song_id: Some("nd-seed".into()),
                seed_artist: None,
                seed_track: None,
                seed_artist_mbid: None,
                limit: Some(10),
            },
        )
        .await
        .unwrap();
        assert_eq!(radio.owned_tracks[0].song_id, "nd-seed");
        assert_eq!(radio.owned_tracks[0].rating, Some(5));
        assert!(radio.owned_tracks[0].starred);
        assert_eq!(radio.owned_tracks[1].song_id, "nd-seed-2");
        assert_eq!(radio.owned_tracks[1].rating, None);
        assert!(!radio.owned_tracks[1].starred);
    }

    #[tokio::test]
    async fn discovery_track_create_with_context_persists_seasonal_fields() {
        use crate::models::discovery_playlist::DiscoveryTrackContext;

        let db = test_db::migrated().await;
        let folder_id = folder_of(&db, "alice").await;
        let reason = CandidateReason::RegionalContextual {
            mix_slug: "winter_holiday".into(),
            country_code: Some("FR".into()),
        };
        let reason_json = serde_json::to_string(&reason).unwrap();

        let created = DiscoveryTrackRow::create_with_context_in(
            &db,
            Some("nd-ctx-1"),
            "Petit Papa Noel",
            "Tino Rossi",
            "Chants de Noel",
            "/music/alice/Discovery/Seasonal/winter_holiday/01.flac",
            &folder_id,
            "Seasonal:winter_holiday",
            DiscoveryTrackContext {
                exclude_from_core_taste: true,
                reason_json: Some(&reason_json),
                culture_bucket: Some("local"),
                pack_slug: Some("winter_holiday"),
            },
        )
        .await
        .unwrap();

        assert!(created.exclude_from_core_taste);
        assert_eq!(created.pack_slug.as_deref(), Some("winter_holiday"));
        assert_eq!(created.culture_bucket.as_deref(), Some("local"));
        assert_eq!(created.reason.as_deref(), Some(&reason));

        // Seasonal pack query finds it; rolling profile query excludes it
        let pack_tracks = DiscoveryTrackRow::get_pending_by_folder_and_pack_in(
            &db,
            &folder_id,
            "winter_holiday",
        )
        .await
        .unwrap();
        assert_eq!(pack_tracks.len(), 1);
        assert_eq!(pack_tracks[0].id, created.id);

        let balanced_tracks = DiscoveryTrackRow::get_pending_by_folder_and_profile_in(
            &db,
            &folder_id,
            "Balanced",
        )
        .await
        .unwrap();
        assert!(balanced_tracks.is_empty());
    }

    #[test]
    fn canonical_mix_title_returns_human_readable_names() {
        assert_eq!(
            canonical_mix_title("winter_holiday", Some("mix.seasonal.winter_holiday"), None),
            "Christmas Mix"
        );
        assert_eq!(
            canonical_mix_title("new_years_eve", Some("mix.seasonal.new_years_eve"), None),
            "New Year's Eve"
        );
        assert_eq!(
            canonical_mix_title("halloween", Some("mix.seasonal.halloween"), None),
            "Darkwave Halloween"
        );
        assert_eq!(
            canonical_mix_title("summer", Some("mix.seasonal.summer"), None),
            "Summer Anthems"
        );
        assert_eq!(
            canonical_mix_title("winter_holiday", None, Some("Custom Xmas")),
            "Custom Xmas"
        );
        assert_eq!(
            canonical_mix_title("midsummer_night", None, None),
            "Midsummer Night"
        );
    }

    #[tokio::test]
    async fn expansion_catalog_retires_diwali_opens_global_packs_and_seeds_missing_slugs() {
        let db = test_db::migrated().await;
        folder_of(&db, "alice").await;
        let ctx = test_ctx("alice");

        let defs = list_mix_definitions_in(&db, &ctx).await.unwrap();
        assert!(
            !defs.iter().any(|d| d.slug == "diwali"),
            "diwali must be retired from built-ins"
        );
        for slug in ["lunar_new_year", "carnival"] {
            let def = defs
                .iter()
                .find(|d| d.slug == slug)
                .unwrap_or_else(|| panic!("missing {slug}"));
            assert!(
                def.countries.is_empty(),
                "{slug} must be open globally (countries empty)"
            );
        }
        let morning = defs.iter().find(|d| d.slug == "morning").unwrap();
        assert_eq!(morning.hour_window, Some(HourWindow::new(5, 10)));

        // Lunar New Year & Carnival are calendar-active globally even without a home_country set.
        let feb = Utc.with_ymd_and_hms(2026, 2, 10, 12, 0, 0).unwrap();
        let (_, feb_seasonal) = list_calendar_active_seasonal_defs_in(&db, "alice", feb)
            .await
            .unwrap();
        let feb_slugs: HashSet<&str> = feb_seasonal.iter().map(|(d, _)| d.slug.as_str()).collect();
        assert!(feb_slugs.contains("lunar_new_year"));
        assert!(feb_slugs.contains("carnival"));
        assert!(feb_slugs.contains("valentines"));

        // Verify seed_builtins_in slug-set check: delete one built-in and insert a stray built_in=1
        // row so COUNT(1) equals builtins.len(), yet seeding MUST still restore the missing slug.
        sqlx::query("DELETE FROM contextual_mix_definitions WHERE slug = 'friday_night'")
            .execute(&db)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO contextual_mix_definitions \
                (slug, kind, i18n_key, built_in, enabled, theme_tags, international_queries, \
                 days_of_week, always_surfaced, countries, owned_ratio, exclude_from_core_taste, \
                 offline_priority, target_tracks, sync_to_navidrome) \
             VALUES ('legacy_extra', 'moment', 'mix.moment.legacy_extra', 1, 1, '[]', '[]', \
                     '[]', 0, '[]', 0.5, 0, 0, 20, 0)",
        )
        .execute(&db)
        .await
        .unwrap();

        let reseeded = list_mix_definitions_in(&db, &ctx).await.unwrap();
        assert!(
            reseeded.iter().any(|d| d.slug == "friday_night"),
            "slug-set fast path must re-seed missing built-ins even when COUNT matches"
        );
    }

    #[tokio::test]
    async fn seasonal_narrowest_window_hero_sorting_leads_broad_seasons() {
        let db = test_db::migrated().await;
        let folder_id = folder_of(&db, "alice").await;
        let ctx = test_ctx("alice");

        // Stage one track in halloween (18d) and autumn (59d), plus summer_solstice (10d) and summer (92d)
        for (id, song_id, slug) in [
            ("dt-oct-1", "nd-oct-1", "autumn"),
            ("dt-oct-2", "nd-oct-2", "halloween"),
            ("dt-jun-1", "nd-jun-1", "summer"),
            ("dt-jun-2", "nd-jun-2", "summer_solstice"),
        ] {
            sqlx::query(
                "INSERT INTO discovery_tracks \
                    (id, song_id, title, artist, album, path, folder_id, profile, status, \
                     exclude_from_core_taste, culture_bucket, pack_slug) \
                 VALUES (?, ?, ?, 'Seasonal Act', 'Pack', ?, ?, ?, 'Pending', 1, 'local', ?)",
            )
            .bind(id)
            .bind(song_id)
            .bind(format!("Track {slug}"))
            .bind(format!("/music/alice/Discovery/Seasonal/{slug}/01.flac"))
            .bind(&folder_id)
            .bind(format!("Seasonal:{slug}"))
            .bind(slug)
            .execute(&db)
            .await
            .unwrap();
        }

        // In October (Oct 20): halloween (18d) must come before autumn (59d)
        let oct = Utc.with_ymd_and_hms(2026, 10, 20, 14, 0, 0).unwrap();
        let oct_mixes = list_active_contextual_mixes_in(
            &db,
            &NoNavidrome,
            None,
            &ctx,
            oct,
            Some(14),
            None,
            None,
        )
        .await
        .unwrap();
        let oct_seasonal: Vec<&str> = oct_mixes
            .iter()
            .filter(|m| m.kind == ContextualMixKind::Seasonal)
            .map(|m| m.slug.as_str())
            .collect();
        assert_eq!(oct_seasonal, vec!["halloween", "autumn"]);

        // In June (Jun 20): summer_solstice (10d) must come before summer (92d)
        let jun = Utc.with_ymd_and_hms(2026, 6, 20, 14, 0, 0).unwrap();
        let jun_mixes = list_active_contextual_mixes_in(
            &db,
            &NoNavidrome,
            None,
            &ctx,
            jun,
            Some(14),
            None,
            None,
        )
        .await
        .unwrap();
        let jun_seasonal: Vec<&str> = jun_mixes
            .iter()
            .filter(|m| m.kind == ContextualMixKind::Seasonal)
            .map(|m| m.slug.as_str())
            .collect();
        assert_eq!(jun_seasonal, vec!["summer_solstice", "summer"]);
    }

    #[tokio::test]
    async fn day_of_week_moments_activate_on_matching_weekdays_and_validate_on_upsert() {
        let db = test_db::migrated().await;
        folder_of(&db, "alice").await;
        let ctx = test_ctx("alice");

        let now = Utc.with_ymd_and_hms(2026, 6, 15, 12, 0, 0).unwrap();

        // Friday 20:00 (weekday 5): friday_night active
        let fri_eve = list_active_contextual_mixes_in(
            &db,
            &NoNavidrome,
            None,
            &ctx,
            now,
            Some(20),
            Some(5),
            None,
        )
        .await
        .unwrap();
        assert!(fri_eve.iter().any(|m| m.slug == "friday_night"));
        assert!(fri_eve.iter().any(|m| m.slug == "evening_unwind"));
        assert!(fri_eve.iter().any(|m| m.slug == "dinner_groove"));

        // Tuesday 20:00 (weekday 2): friday_night hidden
        let tue_eve = list_active_contextual_mixes_in(
            &db,
            &NoNavidrome,
            None,
            &ctx,
            now,
            Some(20),
            Some(2),
            None,
        )
        .await
        .unwrap();
        assert!(!tue_eve.iter().any(|m| m.slug == "friday_night"));
        assert!(tue_eve.iter().any(|m| m.slug == "evening_unwind"));

        // Sunday 09:00 (weekday 7): sunday_morning active
        let sun_am = list_active_contextual_mixes_in(
            &db,
            &NoNavidrome,
            None,
            &ctx,
            now,
            Some(9),
            Some(7),
            None,
        )
        .await
        .unwrap();
        assert!(sun_am.iter().any(|m| m.slug == "sunday_morning"));
        assert!(sun_am.iter().any(|m| m.slug == "morning"));

        // Monday 09:00 (weekday 1): sunday_morning hidden
        let mon_am = list_active_contextual_mixes_in(
            &db,
            &NoNavidrome,
            None,
            &ctx,
            now,
            Some(9),
            Some(1),
            None,
        )
        .await
        .unwrap();
        assert!(!mon_am.iter().any(|m| m.slug == "sunday_morning"));

        // Upsert validates days_of_week and selection_rules
        let bad_day = upsert_mix_definition_in(
            &db,
            &ctx,
            UpsertContextualMixRequest {
                slug: "bad_day".into(),
                kind: ContextualMixKind::Moment,
                custom_name: None,
                custom_description: None,
                global: false,
                enabled: None,
                theme_tags: vec!["jazz".into()],
                international_queries: vec![],
                calendar_window: None,
                hour_window: None,
                days_of_week: vec![0, 5],
                weather_conditions: vec![],
                always_surfaced: false,
                countries: vec![],
                owned_ratio: 0.8,
                default_local_ratio: None,
                exclude_from_core_taste: false,
                offline_priority: false,
                target_tracks: 20,
                sync_to_navidrome: false,
                selection_rules: None,
            },
        )
        .await
        .unwrap_err();
        assert_eq!(bad_day.code, FailureCode::InvalidInput);

        let bad_year = upsert_mix_definition_in(
            &db,
            &ctx,
            UpsertContextualMixRequest {
                slug: "bad_year".into(),
                kind: ContextualMixKind::Moment,
                custom_name: None,
                custom_description: None,
                global: false,
                enabled: None,
                theme_tags: vec![],
                international_queries: vec![],
                calendar_window: None,
                hour_window: None,
                days_of_week: vec![6, 5, 6],
                weather_conditions: vec![],
                always_surfaced: true,
                countries: vec![],
                owned_ratio: 1.0,
                default_local_ratio: None,
                exclude_from_core_taste: false,
                offline_priority: false,
                target_tracks: 20,
                sync_to_navidrome: false,
                selection_rules: Some(MixSelectionRules {
                    year_range: Some((1999, 1990)),
                    ..Default::default()
                }),
            },
        )
        .await
        .unwrap_err();
        assert_eq!(bad_year.code, FailureCode::InvalidInput);

        let bad_rating = upsert_mix_definition_in(
            &db,
            &ctx,
            UpsertContextualMixRequest {
                slug: "bad_rating".into(),
                kind: ContextualMixKind::Moment,
                custom_name: None,
                custom_description: None,
                global: false,
                enabled: None,
                theme_tags: vec![],
                international_queries: vec![],
                calendar_window: None,
                hour_window: None,
                days_of_week: vec![5],
                weather_conditions: vec![],
                always_surfaced: true,
                countries: vec![],
                owned_ratio: 1.0,
                default_local_ratio: None,
                exclude_from_core_taste: false,
                offline_priority: false,
                target_tracks: 20,
                sync_to_navidrome: false,
                selection_rules: Some(MixSelectionRules {
                    min_rating: Some(6),
                    ..Default::default()
                }),
            },
        )
        .await
        .unwrap_err();
        assert_eq!(bad_rating.code, FailureCode::InvalidInput);

        // Valid custom mix deduplicates and sorts days_of_week and persists selection_rules
        let saved = upsert_mix_definition_in(
            &db,
            &ctx,
            UpsertContextualMixRequest {
                slug: "weekend_70s".into(),
                kind: ContextualMixKind::Moment,
                custom_name: Some("Weekend 70s".into()),
                custom_description: None,
                global: false,
                enabled: None,
                theme_tags: vec![],
                international_queries: vec![],
                calendar_window: None,
                hour_window: None,
                days_of_week: vec![7, 6, 7],
                weather_conditions: vec![],
                always_surfaced: false,
                countries: vec![],
                owned_ratio: 1.0,
                default_local_ratio: None,
                exclude_from_core_taste: false,
                offline_priority: false,
                target_tracks: 20,
                sync_to_navidrome: false,
                selection_rules: Some(MixSelectionRules {
                    year_range: Some((1970, 1979)),
                    min_rating: Some(4),
                    strict_match: true,
                    ..Default::default()
                }),
            },
        )
        .await
        .unwrap();
        assert_eq!(saved.days_of_week, vec![6, 7]);
        assert_eq!(
            saved.selection_rules.as_ref().and_then(|r| r.year_range),
            Some((1970, 1979))
        );
    }

    #[tokio::test]
    async fn smart_library_rule_mixes_filter_sort_and_hide_when_empty() {
        let db = test_db::migrated().await;
        folder_of(&db, "alice").await;
        let ctx = test_ctx("alice");

        let now = Utc.with_ymd_and_hms(2026, 6, 15, 14, 0, 0).unwrap();

        // With no library linked, all strict_match smart mixes are hidden.
        let empty_mixes = list_active_contextual_mixes_in(
            &db,
            &NoNavidrome,
            None,
            &ctx,
            now,
            Some(14),
            Some(1),
            None,
        )
        .await
        .unwrap();
        for slug in [
            "forgotten_gems",
            "fresh_arrivals",
            "decade_80s",
            "decade_90s",
            "decade_2000s",
        ] {
            assert!(
                !empty_mixes.iter().any(|m| m.slug == slug),
                "{slug} must stay hidden when 0 matching tracks exist"
            );
        }

        // Library with:
        // - 80s track (1985), played 400 days ago, rated 5 -> decade_80s + forgotten_gems (oldest)
        // - 80s track (1982), played 220 days ago, starred unrated -> decade_80s + forgotten_gems
        // - 90s track (1994), played 200 days ago, 12 plays unrated -> decade_90s + forgotten_gems
        // - 90s track (1998), played 10 days ago, rated 5 -> decade_90s only (NOT forgotten)
        // - 2024 track created 5 days ago, 0 plays -> fresh_arrivals (newest)
        // - 2024 track created 35 days ago, 1 play -> fresh_arrivals
        // - 2024 track created 10 days ago, 4 plays -> NOT fresh_arrivals (play_count > 1)
        // - 2024 track created 120 days ago, 0 plays -> NOT fresh_arrivals (too old)
        // - ZERO 2000s tracks -> decade_2000s must remain hidden!
        let library_resp = subsonic_ok(
            r#""searchResult3":{"song":[
                {"id":"s-80s-old","title":"Neon Pulse","artist":"Synth 85","album":"LP","genre":"Synthpop","year":1985,"userRating":5,"playCount":3,"created":"2022-01-10T00:00:00Z","played":"2025-05-01T00:00:00Z"},
                {"id":"s-80s-star","title":"Midnight Tape","artist":"Wave 82","album":"LP","genre":"New Wave","year":1982,"playCount":2,"starred":"2023-01-01T00:00:00Z","created":"2022-03-10T00:00:00Z","played":"2025-11-01T00:00:00Z"},
                {"id":"s-90s-plays","title":"Grunge Hymn","artist":"Seattle 94","album":"LP","genre":"Rock","year":1994,"playCount":12,"created":"2023-02-01T00:00:00Z","played":"2025-11-20T00:00:00Z"},
                {"id":"s-90s-recent","title":"Freshly Played 90s","artist":"Britpop 98","album":"LP","genre":"Rock","year":1998,"userRating":5,"playCount":20,"created":"2023-02-01T00:00:00Z","played":"2026-06-05T00:00:00Z"},
                {"id":"s-fresh-new","title":"Brand New LP","artist":"New Act A","album":"LP","genre":"Indie","year":2024,"playCount":0,"created":"2026-06-10T00:00:00Z"},
                {"id":"s-fresh-mid","title":"Recent Import","artist":"New Act B","album":"LP","genre":"Indie","year":2024,"playCount":1,"created":"2026-05-10T00:00:00Z"},
                {"id":"s-fresh-played","title":"Overplayed New","artist":"New Act C","album":"LP","genre":"Indie","year":2024,"playCount":4,"created":"2026-06-05T00:00:00Z"},
                {"id":"s-old-unplayed","title":"Dusty Unplayed","artist":"Old Act","album":"LP","genre":"Indie","year":2024,"playCount":0,"created":"2026-02-01T00:00:00Z"}
            ]}"#,
        );

        let stub = NavidromeStub::start();
        stub.answer(
            "/rest/search3",
            &[
                &library_resp,
                &library_resp,
                &library_resp,
                &library_resp,
                &library_resp,
            ],
        );

        let mixes = list_active_contextual_mixes_in(
            &db,
            &StubNavidrome(stub.client()),
            None,
            &ctx,
            now,
            Some(14),
            Some(1),
            None,
        )
        .await
        .unwrap();

        let by_slug: HashMap<&str, &ContextualMixSummary> =
            mixes.iter().map(|m| (m.slug.as_str(), m)).collect();
        assert_eq!(by_slug.get("forgotten_gems").map(|m| m.track_count), Some(3));
        assert_eq!(by_slug.get("fresh_arrivals").map(|m| m.track_count), Some(2));
        assert_eq!(by_slug.get("decade_80s").map(|m| m.track_count), Some(2));
        assert_eq!(by_slug.get("decade_90s").map(|m| m.track_count), Some(2));
        assert!(
            !by_slug.contains_key("decade_2000s"),
            "decade_2000s must stay hidden when library has zero 2000s tracks"
        );

        let forgotten = resolve_contextual_mix_playlist_in(
            &db,
            &StubNavidrome(stub.client()),
            None,
            &ctx,
            "forgotten_gems",
            false,
            now,
        )
        .await
        .unwrap();
        let forgotten_ids: Vec<&str> = forgotten.tracks.iter().map(|t| t.song_id.as_str()).collect();
        assert_eq!(forgotten_ids.len(), 3);
        assert_eq!(
            forgotten_ids[0], "s-80s-old",
            "OldestPlayedFirst must place the oldest-played 30d bucket first"
        );
        assert!(forgotten_ids.contains(&"s-80s-star"));
        assert!(forgotten_ids.contains(&"s-90s-plays"));
        assert!(!forgotten_ids.contains(&"s-90s-recent"));

        let fresh = resolve_contextual_mix_playlist_in(
            &db,
            &StubNavidrome(stub.client()),
            None,
            &ctx,
            "fresh_arrivals",
            false,
            now,
        )
        .await
        .unwrap();
        let fresh_ids: Vec<&str> = fresh.tracks.iter().map(|t| t.song_id.as_str()).collect();
        assert_eq!(
            fresh_ids,
            vec!["s-fresh-new", "s-fresh-mid"],
            "NewestAddedFirst must order newest 7d bucket first and exclude played/old tracks"
        );

        let eighties = resolve_contextual_mix_playlist_in(
            &db,
            &StubNavidrome(stub.client()),
            None,
            &ctx,
            "decade_80s",
            false,
            now,
        )
        .await
        .unwrap();
        let eighties_ids: HashSet<&str> =
            eighties.tracks.iter().map(|t| t.song_id.as_str()).collect();
        assert_eq!(eighties_ids, HashSet::from(["s-80s-old", "s-80s-star"]));

        let nineties = resolve_contextual_mix_playlist_in(
            &db,
            &StubNavidrome(stub.client()),
            None,
            &ctx,
            "decade_90s",
            false,
            now,
        )
        .await
        .unwrap();
        let nineties_ids: HashSet<&str> =
            nineties.tracks.iter().map(|t| t.song_id.as_str()).collect();
        assert_eq!(nineties_ids, HashSet::from(["s-90s-plays", "s-90s-recent"]));
    }

    #[test]
    fn multi_genre_matching_and_daily_deterministic_rotation_jitter() {
        use soulbeet::navidrome::models::SubsonicNamedGenre;

        let make = |id: &str, artist: &str, primary_genre: &str, extra_genres: &[&str], rating: Option<u8>| {
            SubsonicSong {
                id: id.into(),
                title: format!("Title {id}"),
                artist: Some(artist.into()),
                album: Some("LP".into()),
                album_id: None,
                genre: Some(primary_genre.into()),
                genres: extra_genres
                    .iter()
                    .map(|g| SubsonicNamedGenre {
                        name: (*g).to_string(),
                    })
                    .collect(),
                duration: Some(200),
                user_rating: rating,
                play_count: Some(5),
                created: None,
                played: None,
                starred: None,
                music_brainz_id: None,
                suffix: None,
                bit_rate: None,
                size: None,
                year: Some(2020),
                track: None,
                path: None,
            }
        };

        let drive = shared::recommendation::builtin_contextual_mixes()
            .into_iter()
            .find(|d| d.slug == "drive")
            .unwrap();

        // 6 songs with primary genre "Pop" but secondary OpenSubsonic genre "Synthpop" (matches drive)
        let library: Vec<SubsonicSong> = (0..6)
            .map(|i| {
                make(
                    &format!("song-{i}"),
                    &format!("Artist {i}"),
                    "Pop",
                    &["Synthpop"],
                    Some(if i < 4 { 5 } else { 4 }),
                )
            })
            .collect();

        let day1 = Utc.with_ymd_and_hms(2026, 6, 15, 10, 0, 0).unwrap();
        let day1_later = Utc.with_ymd_and_hms(2026, 6, 15, 22, 30, 0).unwrap();

        let tracks_a = build_contextual_mix_tracks("alice", &drive, Some(&library), &[], day1);
        let order_a: Vec<String> = tracks_a.iter().map(|t| t.song_id.clone()).collect();
        let order_b: Vec<String> =
            build_contextual_mix_tracks("alice", &drive, Some(&library), &[], day1_later)
                .into_iter()
                .map(|t| t.song_id)
                .collect();

        // All 6 matched specifically via OpenSubsonic secondary `genres` tag ("synthpop"),
        // NOT via the untagged library fallback!
        assert_eq!(order_a.len(), 6);
        assert!(
            tracks_a.iter().all(|t| matches!(
                t.reason,
                Some(CandidateReason::LibraryMatch {
                    matched_genre: Some(ref g),
                    ..
                }) if g.eq_ignore_ascii_case("synthpop")
            )),
            "expected every drive track to match secondary genre 'synthpop'; got {:?}",
            tracks_a.iter().map(|t| &t.reason).collect::<Vec<_>>()
        );
        // Same day produces identical deterministic order
        assert_eq!(order_a, order_b);
        // Tier 0 (5-star songs 0..4) stays strictly ahead of Tier 1 (4-star songs 4..6)
        assert!(order_a[..4].iter().all(|id| !matches!(id.as_str(), "song-4" | "song-5")));
        assert!(order_a[4..].iter().all(|id| matches!(id.as_str(), "song-4" | "song-5")));

        // Across consecutive dates, order within the 5-star tier rotates deterministically
        let mut saw_different_day_order = false;
        for day_offset in 1..=7 {
            let next_day = day1 + chrono::Duration::days(day_offset);
            let next_order: Vec<String> =
                build_contextual_mix_tracks("alice", &drive, Some(&library), &[], next_day)
                    .into_iter()
                    .map(|t| t.song_id)
                    .collect();
            assert!(next_order[..4]
                .iter()
                .all(|id| !matches!(id.as_str(), "song-4" | "song-5")));
            if next_order != order_a {
                saw_different_day_order = true;
                break;
            }
        }
        assert!(
            saw_different_day_order,
            "daily rotation jitter must rotate track order across calendar dates"
        );

        // Seasonal multi-genre matcher: primary "Rock" + secondary "Celtic" proves secondary
        // `genres[]` matching; generic "Folk" (Bob Dylan) and "Patrick Watson" must NOT match!
        let celtic = shared::recommendation::builtin_contextual_mixes()
            .into_iter()
            .find(|d| d.slug == "celtic_spring")
            .unwrap();
        let celtic_lib = vec![
            make("c-1", "Dubliners Band", "Rock", &["Celtic"], Some(5)),
            make("c-folk", "Bob Dylan", "Folk", &[], Some(5)),
            make("c-fp", "Patrick Watson", "Indie", &["Chamber Pop"], Some(5)),
        ];
        let celtic_tracks =
            build_contextual_mix_tracks("alice", &celtic, Some(&celtic_lib), &[], day1);
        let celtic_ids: Vec<&str> = celtic_tracks.iter().map(|t| t.song_id.as_str()).collect();
        assert_eq!(celtic_ids, vec!["c-1"]);

        // Valentine's artist false-positive guard: My Bloody Valentine / My Chemical Romance
        // must NOT match by artist name.
        let valentines = shared::recommendation::builtin_contextual_mixes()
            .into_iter()
            .find(|d| d.slug == "valentines")
            .unwrap();
        let val_lib = vec![
            make("v-mbv", "My Bloody Valentine", "Shoegaze", &[], Some(5)),
            make("v-mcr", "My Chemical Romance", "Emo", &[], Some(5)),
            make("v-ok", "Chet Baker", "Jazz", &["Romance"], Some(5)),
        ];
        let val_tracks =
            build_contextual_mix_tracks("alice", &valentines, Some(&val_lib), &[], day1);
        let val_ids: Vec<&str> = val_tracks.iter().map(|t| t.song_id.as_str()).collect();
        assert_eq!(val_ids, vec!["v-ok"]);

        // Overflowing not_played_days in matches_selection_rules must return false, never panic.
        let extreme_rules = MixSelectionRules {
            not_played_days: Some(u32::MAX),
            added_within_days: Some(u32::MAX),
            ..Default::default()
        };
        assert!(!matches_selection_rules(&val_lib[2], &extreme_rules, day1));
    }

    #[tokio::test]
    async fn invalid_local_weekday_and_unbounded_rule_days_return_invalid_input_and_retired_states_clean_up() {
        let db = test_db::migrated().await;
        let _ = folder_of(&db, "alice").await;
        let ctx = test_ctx("alice");
        let dec = Utc.with_ymd_and_hms(2026, 12, 10, 12, 0, 0).unwrap();

        let bad_weekday = list_active_contextual_mixes_in(
            &db,
            &NoNavidrome,
            None,
            &ctx,
            dec,
            Some(12),
            Some(0),
            None,
        )
        .await;
        assert!(bad_weekday.is_err());

        let bad_days = upsert_mix_definition_in(
            &db,
            &ctx,
            UpsertContextualMixRequest {
                slug: "bad_days_mix".into(),
                kind: ContextualMixKind::Moment,
                custom_name: Some("Bad Days".into()),
                custom_description: None,
                global: false,
                enabled: Some(true),
                theme_tags: vec![],
                international_queries: vec![],
                calendar_window: None,
                hour_window: None,
                days_of_week: vec![],
                weather_conditions: vec![],
                always_surfaced: true,
                countries: vec![],
                owned_ratio: 1.0,
                default_local_ratio: None,
                exclude_from_core_taste: false,
                offline_priority: false,
                target_tracks: 20,
                sync_to_navidrome: false,
                selection_rules: Some(MixSelectionRules {
                    not_played_days: Some(100_000_000),
                    ..Default::default()
                }),
            },
        )
        .await;
        assert!(bad_days.is_err());

        // Orphaned state from a retired seasonal pack (e.g. legacy `diwali` with a Navidrome smart
        // playlist id) is deleted from Navidrome and removed from `contextual_mix_states`.
        let stub = NavidromeStub::start();
        stub.answer("/api/playlist/sp-diwali-old", &[""]);
        ContextualMixStateRow::upsert_playlist_id_in(
            &db,
            "alice",
            "diwali",
            Some("sp-diwali-old"),
            Some("2026-11-22"),
        )
        .await
        .unwrap();
        let place = NavidromePlace {
            library_id: 1,
            path: "alice/Discovery".into(),
        };
        reconcile_seasonal_playlists_in(&db, &stub.client(), "alice", &place, dec)
            .await
            .unwrap();
        assert!(
            stub.asked().iter().any(|p| p == "/api/playlist/sp-diwali-old"),
            "retired seasonal playlist should be deleted in Navidrome; asked={:?}",
            stub.asked()
        );
        assert!(
            ContextualMixStateRow::get_in(&db, "alice", "diwali")
                .await
                .unwrap()
                .is_none(),
            "retired contextual_mix_states row should be removed after Navidrome cleanup"
        );
    }

    #[tokio::test]
    async fn weather_gated_mixes_activate_only_on_matching_live_weather_and_attach_badge() {
        let db = test_db::migrated().await;
        folder_of(&db, "alice").await;
        let ctx = test_ctx("alice");
        let now = Utc.with_ymd_and_hms(2026, 6, 15, 14, 0, 0).unwrap();
        let all_weather_slugs = ["rainy_day", "snow_day", "storm_night", "sunny_day"];

        // When weather is None, all 4 weather mixes are hidden.
        let none_mixes = list_active_contextual_mixes_in(
            &db,
            &NoNavidrome,
            None,
            &ctx,
            now,
            Some(14),
            Some(1),
            None,
        )
        .await
        .unwrap();
        for slug in all_weather_slugs {
            assert!(
                !none_mixes.iter().any(|m| m.slug == slug),
                "{slug} must stay hidden when weather is None"
            );
        }
        assert!(none_mixes.iter().all(|m| m.weather_badge.is_none()));

        // Each WeatherCondition activates ONLY its matching mix with weather_badge attached,
        // keeps non-weather mixes badge-free, and sorts the weather mix in bucket 1
        // (right after drive/offline_priority, ahead of scheduled time moments like focus).
        for (cond, expected_slug, temp_c, code, is_day) in [
            (WeatherCondition::Rainy, "rainy_day", 13.5, 61, true),
            (WeatherCondition::Snowy, "snow_day", -2.0, 73, true),
            (WeatherCondition::Stormy, "storm_night", 18.0, 95, false),
            (WeatherCondition::Sunny, "sunny_day", 26.0, 0, true),
        ] {
            let snap = WeatherSnapshot {
                condition: cond,
                temperature_c: temp_c,
                weather_code: code,
                is_day,
                location_label: Some("Lyon, FR".into()),
            };
            let mixes = list_active_contextual_mixes_in(
                &db,
                &NoNavidrome,
                None,
                &ctx,
                now,
                Some(14),
                Some(1),
                Some(snap.clone()),
            )
            .await
            .unwrap();

            for slug in all_weather_slugs {
                let found = mixes.iter().find(|m| m.slug == slug);
                if slug == expected_slug {
                    let m = found.expect("matching weather mix must surface");
                    assert_eq!(m.weather_badge.as_ref(), Some(&snap));
                } else {
                    assert!(
                        found.is_none(),
                        "{slug} must stay hidden when weather is {cond:?}"
                    );
                }
            }

            // Non-weather mixes must NOT carry weather_badge
            let drive = mixes.iter().find(|m| m.slug == "drive").unwrap();
            assert_eq!(drive.weather_badge, None);
            let focus = mixes.iter().find(|m| m.slug == "focus").unwrap();
            assert_eq!(focus.weather_badge, None);

            // Ordering: drive (bucket 0) < weather mix (bucket 1) < focus (bucket 2)
            let pos = |slug: &str| mixes.iter().position(|m| m.slug == slug).unwrap();
            assert!(pos("drive") < pos(expected_slug));
            assert!(pos(expected_slug) < pos("focus"));
        }
    }

    #[tokio::test]
    async fn culture_profile_weather_city_geocodes_preserves_and_clears() {
        let db = test_db::migrated().await;
        folder_of(&db, "alice").await;

        // Setting home_country + weather_city passes effective_country to geocoder and persists coords.
        let saved = update_culture_profile_with_geocoder_in(
            &db,
            "alice",
            UpdateCultureProfileRequest {
                home_country: Some("fr".into()),
                preferred_languages: None,
                local_culture_ratio: None,
                weather_city: Some("Lyon".into()),
            },
            |query, preferred| async move {
                assert_eq!(query, "Lyon");
                assert_eq!(preferred.as_deref(), Some("FR"));
                Ok(crate::core::weather::WeatherLocation {
                    lat: 45.7485,
                    lon: 4.8467,
                    label: Some("Lyon, FR".into()),
                })
            },
        )
        .await
        .unwrap();
        assert_eq!(saved.weather_city.as_deref(), Some("Lyon, FR"));
        assert_eq!(saved.weather_lat, Some(45.7485));
        assert_eq!(saved.weather_lon, Some(4.8467));

        // Passing weather_city: None preserves existing city and coordinates without calling geocoder.
        let preserved = update_culture_profile_with_geocoder_in(
            &db,
            "alice",
            UpdateCultureProfileRequest {
                home_country: None,
                preferred_languages: Some(vec!["fr".into()]),
                local_culture_ratio: None,
                weather_city: None,
            },
            |_, _| async { panic!("geocoder must not be called when weather_city is None") },
        )
        .await
        .unwrap();
        assert_eq!(preserved.weather_city.as_deref(), Some("Lyon, FR"));
        assert_eq!(preserved.weather_lat, Some(45.7485));
        assert_eq!(preserved.weather_lon, Some(4.8467));

        // Passing overly long weather_city fails with InvalidInput before geocoding.
        let too_long = update_culture_profile_with_geocoder_in(
            &db,
            "alice",
            UpdateCultureProfileRequest {
                home_country: None,
                preferred_languages: None,
                local_culture_ratio: None,
                weather_city: Some("a".repeat(101)),
            },
            |_, _| async { panic!("geocoder must not be called for >100 char city") },
        )
        .await
        .unwrap_err();
        assert_eq!(too_long.code, FailureCode::InvalidInput);

        // Passing empty / whitespace-only weather_city clears city and coordinates back to auto IP geo.
        let cleared = update_culture_profile_with_geocoder_in(
            &db,
            "alice",
            UpdateCultureProfileRequest {
                home_country: None,
                preferred_languages: None,
                local_culture_ratio: None,
                weather_city: Some("   ".into()),
            },
            |_, _| async { panic!("geocoder must not be called when clearing weather_city") },
        )
        .await
        .unwrap();
        assert_eq!(cleared.weather_city, None);
        assert_eq!(cleared.weather_lat, None);
        assert_eq!(cleared.weather_lon, None);
    }

    #[tokio::test]
    async fn native_library_snapshot_caches_and_filters_stale_song_ids() {
        let db = test_db::migrated().await;
        let folder_id = folder_of(&db, "alice").await;
        let ctx = test_ctx("alice");

        sqlx::query(
            "INSERT INTO discovery_tracks \
                (id, song_id, title, artist, album, path, folder_id, profile, status, pack_slug) \
             VALUES \
                ('dt-live', 'nd-live-1', 'Live Song', 'Good Act', 'EP', \
                 '/music/alice/Discovery/Seasonal/winter_holiday/live.flac', ?, 'Balanced', 'Pending', 'winter_holiday'), \
                ('dt-stale', 'nd-deleted-99', 'Gone Song', 'Old Act', 'EP', \
                 '/music/alice/Discovery/Seasonal/winter_holiday/gone.flac', ?, 'Balanced', 'Pending', 'winter_holiday')",
        )
        .bind(&folder_id)
        .bind(&folder_id)
        .execute(&db)
        .await
        .unwrap();

        let stub = NavidromeStub::start();
        let native_body = format!(
            "[{}]",
            serde_json::json!({
                "id": "nd-live-1",
                "path": "alice/Discovery/Seasonal/winter_holiday/live.flac",
                "title": "Live Song",
                "size": 1000,
                "libraryId": 1,
                "libraryPath": "/music",
                "missing": false,
                "updatedAt": "2026-12-10T00:00:00Z",
                "artist": "Good Act",
                "album": "EP",
                "albumArtist": "Good Act",
                "rating": 0,
                "duration": 200.0,
                "starred": false,
                "playCount": 0
            })
        );
        stub.answer("/api/song", &[&native_body]);
        stub.header("/api/song", "X-Total-Count", "1");

        let now = Utc.with_ymd_and_hms(2026, 12, 15, 19, 0, 0).unwrap();
        let first = resolve_contextual_mix_playlist_in(
            &db,
            &StubNavidrome(stub.client()),
            Some(Path::new("/music")),
            &ctx,
            "winter_holiday",
            false,
            now,
        )
        .await
        .unwrap();
        assert_eq!(first.tracks.len(), 1);
        assert_eq!(first.tracks[0].song_id, "nd-live-1");

        // Stale song_id was cleared in SQLite so deleted rows cannot inflate counts.
        let stale_row = DiscoveryTrackRow::get_by_id_in(&db, "dt-stale")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(stale_row.song_id, None);

        // Second call within 45s hits the per-user TTL cache (still 1 `/api/song` request).
        let second = resolve_contextual_mix_playlist_in(
            &db,
            &StubNavidrome(stub.client()),
            Some(Path::new("/music")),
            &ctx,
            "winter_holiday",
            false,
            now,
        )
        .await
        .unwrap();
        assert_eq!(second.tracks.len(), 1);
        assert_eq!(
            stub.asked().iter().filter(|p| p.as_str() == "/api/song").count(),
            1
        );

        // Invalidation forces a fresh `/api/song` fetch.
        invalidate_library_snapshot_cache(&ctx.user_id);
        let _third = resolve_contextual_mix_playlist_in(
            &db,
            &StubNavidrome(stub.client()),
            Some(Path::new("/music")),
            &ctx,
            "winter_holiday",
            false,
            now,
        )
        .await
        .unwrap();
        assert_eq!(
            stub.asked().iter().filter(|p| p.as_str() == "/api/song").count(),
            2
        );
    }

    #[tokio::test]
    async fn native_api_song_preserves_genres_and_created_at_without_search3_fallback() {
        let db = test_db::migrated().await;
        folder_of(&db, "alice").await;
        let ctx = test_ctx("alice");

        let stub = NavidromeStub::start();
        let native_body = serde_json::json!([
            {
                "id": "nd-recent-jazz",
                "path": "alice/Bill Evans/Waltz/01.flac",
                "title": "Waltz for Debby",
                "size": 2000,
                "libraryId": 1,
                "libraryPath": "/music",
                "missing": false,
                "createdAt": "2026-12-12T10:00:00Z",
                "updatedAt": "2020-01-01T00:00:00Z",
                "artist": "Bill Evans",
                "album": "Waltz for Debby",
                "albumArtist": "Bill Evans",
                "genre": "Jazz",
                "genres": [
                    {"id": "g1", "name": "Jazz"},
                    {"id": "g2", "name": "Late Night Piano"}
                ],
                "rating": 5,
                "duration": 240.0,
                "starred": true,
                "playCount": 1
            },
            {
                "id": "nd-old-jazz",
                "path": "alice/Miles Davis/Blue/01.flac",
                "title": "So What",
                "size": 2000,
                "libraryId": 1,
                "libraryPath": "/music",
                "missing": false,
                "createdAt": "2024-01-10T10:00:00Z",
                "updatedAt": "2026-12-14T10:00:00Z",
                "artist": "Miles Davis",
                "album": "Kind of Blue",
                "albumArtist": "Miles Davis",
                "genre": "Jazz",
                "genres": [{"id": "g1", "name": "Jazz"}],
                "rating": 5,
                "duration": 300.0,
                "starred": true,
                "playCount": 10
            },
            {
                "id": "nd-recent-metal",
                "path": "alice/Metal Act/Noise/01.flac",
                "title": "Heavy Riff",
                "size": 2000,
                "libraryId": 1,
                "libraryPath": "/music",
                "missing": false,
                "createdAt": "2026-12-14T12:00:00Z",
                "updatedAt": "2026-12-14T12:00:00Z",
                "artist": "Metal Act",
                "album": "Noise",
                "albumArtist": "Metal Act",
                "genre": "Death Metal",
                "genres": [{"id": "g3", "name": "Death Metal"}],
                "rating": 5,
                "duration": 210.0,
                "starred": true,
                "playCount": 0
            }
        ])
        .to_string();
        stub.answer("/api/song", &[&native_body]);
        stub.header("/api/song", "X-Total-Count", "3");

        // Create a custom mix that requires BOTH multi-genre tag matching AND added_within_days
        // with NewestAddedFirst sorting.
        let mix_req = UpsertContextualMixRequest {
            slug: "recent_piano_jazz".into(),
            kind: ContextualMixKind::Moment,
            custom_name: Some("Recent Piano Jazz".into()),
            custom_description: None,
            global: false,
            enabled: Some(true),
            theme_tags: vec!["late night piano".into(), "jazz".into()],
            international_queries: vec![],
            calendar_window: None,
            hour_window: None,
            days_of_week: vec![],
            weather_conditions: vec![],
            always_surfaced: true,
            countries: vec![],
            owned_ratio: 1.0,
            default_local_ratio: None,
            exclude_from_core_taste: false,
            offline_priority: false,
            target_tracks: 10,
            sync_to_navidrome: false,
            selection_rules: Some(MixSelectionRules {
                added_within_days: Some(30),
                sort_order: MixSortOrder::NewestAddedFirst,
                strict_match: true,
                ..MixSelectionRules::default()
            }),
        };
        upsert_mix_definition_in(&db, &ctx, mix_req).await.unwrap();

        let now = Utc.with_ymd_and_hms(2026, 12, 15, 19, 0, 0).unwrap();
        let playlist = resolve_contextual_mix_playlist_in(
            &db,
            &StubNavidrome(stub.client()),
            Some(Path::new("/music")),
            &ctx,
            "recent_piano_jazz",
            true,
            now,
        )
        .await
        .unwrap();

        // Only nd-recent-jazz matches BOTH genre ("Jazz" / "Late Night Piano") AND createdAt within 30 days.
        // nd-old-jazz has recent updatedAt (2026-12-14) but old createdAt (2024-01-10) -> must be excluded.
        // nd-recent-metal has recent createdAt but wrong genre -> must be excluded.
        assert_eq!(playlist.tracks.len(), 1);
        assert_eq!(playlist.tracks[0].song_id, "nd-recent-jazz");

        // Verify /api/song was used directly with zero fallback to Subsonic search3.
        let asked = stub.asked();
        assert!(asked.iter().any(|p| p == "/api/song"), "asked={asked:?}");
        assert!(
            !asked.iter().any(|p| p.contains("search3")),
            "unexpected search3 fallback: {asked:?}"
        );

        // Also verify fetch_all_songs_native directly preserves genre, genres, and createdAt.
        let fetched = fetch_all_songs_native(&stub.client()).await.unwrap();
        let waltz = fetched.iter().find(|s| s.id == "nd-recent-jazz").unwrap();
        assert_eq!(waltz.genre.as_deref(), Some("Jazz"));
        let all_genres: Vec<&str> = waltz.all_genres().collect();
        assert!(all_genres.contains(&"Jazz"));
        assert!(all_genres.contains(&"Late Night Piano"));
        assert_eq!(waltz.created.as_deref(), Some("2026-12-12T10:00:00+00:00"));
    }
}
