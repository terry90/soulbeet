//! Core logic for scanning the Navidrome library on demand, browsing songs/albums with
//! structured filters and sorting, and inspecting an artist with their albums.

use std::collections::HashSet;
use std::path::Path;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use shared::failure::{
    cut_json, Failure, FailureCode, RetryAdvice, Stage, UserAction, UserActionCode, TEXT_MAX_CHARS,
};
use soulbeet::navidrome::models::{NativeBrowseSong, SubsonicBrowseAlbum};
use soulbeet::navidrome::{NavidromeClient, ScanStatus};
use soulbeet::NavidromeError;
use sqlx::SqlitePool;

use crate::auth::{UserCtx, UserId};
use crate::core::folders;
use crate::core::index::{scan_failed, NavidromeOf};
use crate::core::library::{navidrome_failure, AlbumFacts, ArtistFacts, FolderResolver, SongFacts};
use crate::core::scan::{start_scan_with_options_for, ScanRoles, ScanStart, SCAN_ROLES};
use crate::core::time;

/// Longest wait `scan_library` accepts, in seconds.
pub const MAX_SCAN_WAIT_SECONDS: u32 = 30;

/// Interval between `getScanStatus` polls while waiting for a scan to finish.
const SCAN_POLL_INTERVAL: Duration = Duration::from_millis(500);

/// Result of triggering (and optionally waiting on) a Navidrome library scan.
#[derive(Debug, Clone)]
pub struct ScanReport {
    pub started: bool,
    pub full_scan: bool,
    pub completed: bool,
    pub scan: ScanStatus,
}

/// Triggers a Navidrome scan (quick or full) for `user` using their account or another linked
/// admin account, optionally waiting up to 30 seconds for it to finish.
pub(crate) async fn scan_library(
    db: &SqlitePool,
    navidrome: &dyn NavidromeOf,
    user: &UserId,
    full_scan: bool,
    wait_seconds: u32,
) -> Result<ScanReport, Failure> {
    scan_library_in(
        db,
        navidrome,
        &SCAN_ROLES,
        user,
        full_scan,
        wait_seconds,
        SCAN_POLL_INTERVAL,
    )
    .await
}

pub(crate) async fn scan_library_in(
    db: &SqlitePool,
    navidrome: &dyn NavidromeOf,
    roles: &ScanRoles,
    user: &UserId,
    full_scan: bool,
    wait_seconds: u32,
    poll_interval: Duration,
) -> Result<ScanReport, Failure> {
    let own = navidrome
        .navidrome_of(user)
        .await
        .map_err(|e| e.failure(Stage::Index, e.to_string()))?;

    let baseline = own
        .get_scan_status()
        .await
        .map_err(|e| navidrome_failure(Stage::Index, &e))?;
    let baseline_last_scan = baseline.last_scan;
    let baseline_started = baseline.last_start();

    let start = start_scan_with_options_for(db, navidrome, roles, user, &own, full_scan)
        .await
        .map_err(|e| e.failure(Stage::Index, e.to_string()))?;

    if start == ScanStart::NoAccount {
        return Err(no_scan_account_failure());
    }

    let capped_wait = wait_seconds.min(MAX_SCAN_WAIT_SECONDS);
    let mut latest = own
        .get_scan_status()
        .await
        .map_err(|e| navidrome_failure(Stage::Index, &e))?;

    if let Some(err) = check_scan_outcome(&baseline, baseline_started, &latest) {
        return err;
    }
    if latest.ended_after(baseline_last_scan) {
        return Ok(ScanReport {
            started: true,
            full_scan,
            completed: true,
            scan: latest,
        });
    }

    if capped_wait == 0 {
        return Ok(ScanReport {
            started: true,
            full_scan,
            completed: false,
            scan: latest,
        });
    }

    let deadline = tokio::time::Instant::now() + Duration::from_secs(u64::from(capped_wait));
    loop {
        let now = tokio::time::Instant::now();
        if now >= deadline {
            return Ok(ScanReport {
                started: true,
                full_scan,
                completed: false,
                scan: latest,
            });
        }
        let sleep_for = poll_interval.min(deadline - now);
        tokio::time::sleep(sleep_for).await;

        latest = own
            .get_scan_status()
            .await
            .map_err(|e| navidrome_failure(Stage::Index, &e))?;

        if let Some(err) = check_scan_outcome(&baseline, baseline_started, &latest) {
            return err;
        }
        if latest.ended_after(baseline_last_scan) {
            return Ok(ScanReport {
                started: true,
                full_scan,
                completed: true,
                scan: latest,
            });
        }
    }
}

/// Checks whether `latest` reports a scan failure after `baseline`.
fn check_scan_outcome(
    baseline: &ScanStatus,
    baseline_started: Option<chrono::DateTime<chrono::Utc>>,
    latest: &ScanStatus,
) -> Option<Result<ScanReport, Failure>> {
    if latest.scanning || latest.error.is_empty() {
        return None;
    }
    let advanced_start = match (baseline_started, latest.last_start()) {
        (Some(before), Some(after)) => after > before,
        (None, Some(_)) => true,
        _ => false,
    };
    let fresh_error =
        latest.ended_after(baseline.last_scan) || latest.error != baseline.error || advanced_start;
    if fresh_error {
        return Some(Err(scan_failed(&latest.error)));
    }
    None
}

fn no_scan_account_failure() -> Failure {
    Failure::new(
        FailureCode::IndexNoScanPossible,
        Stage::Index,
        RetryAdvice::NeedsUser,
        "No Navidrome account linked to Soulbeet may start a scan",
    )
    .with_user_action(UserAction::new(
        UserActionCode::EnableIndexing,
        "Sign in to Soulbeet once as a Navidrome admin, enable Navidrome's file watcher, or set a scan schedule",
        "Soulbeet sign-in or Navidrome",
    ))
}

/// One artist with all of their albums as Navidrome lists them for the caller.
pub async fn get_artist(
    navi: &NavidromeClient,
    artist_id: &str,
) -> Result<(ArtistFacts, Vec<AlbumFacts>), Failure> {
    let artist = navi
        .get_artist(artist_id)
        .await
        .map_err(|e| navidrome_failure(Stage::LibraryCheck, &e))?
        .ok_or_else(|| {
            Failure::new(
                FailureCode::UnknownHandle,
                Stage::LibraryCheck,
                RetryAdvice::NotRetryable,
                "No artist with this id",
            )
        })?;
    let albums = artist.album.iter().map(AlbumFacts::from).collect();
    Ok((ArtistFacts::from(&artist.artist), albums))
}

/// What `browse_library` lists.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BrowseKind {
    #[default]
    Songs,
    Albums,
}

impl BrowseKind {
    pub fn as_str(self) -> &'static str {
        match self {
            BrowseKind::Songs => "songs",
            BrowseKind::Albums => "albums",
        }
    }
}

/// How `browse_library` orders matching items.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BrowseSort {
    #[default]
    RecentlyAdded,
    RecentlyPlayed,
    MostPlayed,
    HighestRated,
    Title,
    Artist,
    Year,
    Random,
}

impl BrowseSort {
    pub fn as_str(self) -> &'static str {
        match self {
            BrowseSort::RecentlyAdded => "recently_added",
            BrowseSort::RecentlyPlayed => "recently_played",
            BrowseSort::MostPlayed => "most_played",
            BrowseSort::HighestRated => "highest_rated",
            BrowseSort::Title => "title",
            BrowseSort::Artist => "artist",
            BrowseSort::Year => "year",
            BrowseSort::Random => "random",
        }
    }
}

/// Structured filter and sort parameters for `browse_library`.
#[derive(Debug, Clone, Default)]
pub struct BrowseQuery {
    pub kind: BrowseKind,
    pub folder_id: Option<String>,
    pub genre: Option<String>,
    pub min_rating: Option<u8>,
    pub max_rating: Option<u8>,
    pub unrated: Option<bool>,
    pub starred: Option<bool>,
    pub year_from: Option<u32>,
    pub year_to: Option<u32>,
    pub format: Option<String>,
    pub max_bitrate_kbps: Option<u32>,
    pub missing: Option<bool>,
    pub sort: BrowseSort,
    /// Deterministic seed used when `sort == BrowseSort::Random` so pagination stays stable.
    pub random_seed: u64,
}

/// A song returned by `browse_library`: `SongFacts` plus genre, timestamps and missing marker.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct BrowseSong {
    #[serde(flatten)]
    pub song: SongFacts,
    pub genre: Option<String>,
    pub play_date: Option<String>,
    pub created_at: Option<String>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub missing: bool,
}

/// Filtered, sorted items from `browse_library`.
#[derive(Debug, Clone, PartialEq)]
pub enum BrowseItems {
    Songs(Vec<BrowseSong>),
    Albums(Vec<AlbumFacts>),
}

/// Full result of `browse_library` before pagination.
#[derive(Debug, Clone, PartialEq)]
pub struct BrowseResult {
    pub total: usize,
    pub items: BrowseItems,
}

/// Browses the caller's Navidrome library with structured filters and deterministic sorting.
pub async fn browse_library(
    db: &SqlitePool,
    navi: &NavidromeClient,
    ctx: &UserCtx,
    music_path: Option<&Path>,
    query: BrowseQuery,
) -> Result<BrowseResult, Failure> {
    if let Some(ref folder_id) = query.folder_id {
        folders::owned_in(db, ctx, folder_id).await?;
    }

    match query.kind {
        BrowseKind::Songs => browse_songs(db, navi, ctx, music_path, &query).await,
        BrowseKind::Albums => browse_albums(db, navi, ctx, music_path, &query).await,
    }
}

async fn browse_songs(
    db: &SqlitePool,
    navi: &NavidromeClient,
    ctx: &UserCtx,
    music_path: Option<&Path>,
    query: &BrowseQuery,
) -> Result<BrowseResult, Failure> {
    let want_missing = query.missing.unwrap_or(false);
    let page = navi
        .all_browse_songs_filtered(Some(want_missing))
        .await
        .map_err(|e| navidrome_failure(Stage::LibraryCheck, &e))?;

    let resolver = FolderResolver::load_in(db, &ctx.user_id, music_path).await;
    let normalized_format = query
        .format
        .as_deref()
        .map(|f| f.trim().trim_start_matches('.'));

    let mut matched: Vec<(NativeBrowseSong, SongFacts)> = Vec::new();
    for s in page.songs {
        if s.song.missing != want_missing {
            continue;
        }
        if !song_passes_metadata_filters(&s, query, normalized_format) {
            continue;
        }
        let facts = resolver.facts(&s.song);
        if let Some(ref want_folder) = query.folder_id {
            if facts
                .folder
                .as_ref()
                .is_none_or(|f| f.folder_id != *want_folder)
            {
                continue;
            }
        }
        matched.push((s, facts));
    }

    sort_songs(&mut matched, query.sort, query.random_seed);

    let songs: Vec<BrowseSong> = matched
        .into_iter()
        .map(|(raw, facts)| BrowseSong {
            song: facts,
            genre: browse_song_genre(&raw),
            play_date: raw.song.play_date.map(time::at),
            created_at: raw.created_at.map(time::at),
            missing: raw.song.missing,
        })
        .collect();

    Ok(BrowseResult {
        total: songs.len(),
        items: BrowseItems::Songs(songs),
    })
}

async fn browse_albums(
    db: &SqlitePool,
    navi: &NavidromeClient,
    ctx: &UserCtx,
    music_path: Option<&Path>,
    query: &BrowseQuery,
) -> Result<BrowseResult, Failure> {
    let failure = |e: NavidromeError| navidrome_failure(Stage::LibraryCheck, &e);
    let albums = navi.all_browse_albums().await.map_err(failure)?;

    let needs_song_filter = query.folder_id.is_some()
        || query.format.is_some()
        || query.max_bitrate_kbps.is_some()
        || query.missing == Some(true);

    let allowed_album_ids: Option<HashSet<String>> = if needs_song_filter {
        let want_missing = query.missing.unwrap_or(false);
        let song_page = navi
            .all_browse_songs_filtered(Some(want_missing))
            .await
            .map_err(failure)?;
        let resolver = if query.folder_id.is_some() {
            Some(FolderResolver::load_in(db, &ctx.user_id, music_path).await)
        } else {
            None
        };
        let normalized_format = query
            .format
            .as_deref()
            .map(|f| f.trim().trim_start_matches('.'));
        let mut ids = HashSet::new();
        for s in &song_page.songs {
            if s.song.missing != want_missing || s.song.album_id.is_empty() {
                continue;
            }
            if let Some(want_fmt) = normalized_format {
                if !s.song.suffix.eq_ignore_ascii_case(want_fmt) {
                    continue;
                }
            }
            if let Some(max_kbps) = query.max_bitrate_kbps {
                if s.song.bit_rate == 0 || s.song.bit_rate > max_kbps {
                    continue;
                }
            }
            if let (Some(ref want_folder), Some(ref resolver)) = (&query.folder_id, &resolver) {
                if resolver
                    .resolve_native(&s.song)
                    .is_none_or(|f| f.folder_id != *want_folder)
                {
                    continue;
                }
            }
            ids.insert(s.song.album_id.clone());
        }
        Some(ids)
    } else {
        None
    };

    let mut matched: Vec<SubsonicBrowseAlbum> = albums
        .into_iter()
        .filter(|a| {
            if let Some(ref allowed) = allowed_album_ids {
                if !allowed.contains(&a.album.id) {
                    return false;
                }
            }
            album_passes_filters(a, query)
        })
        .collect();

    sort_albums(&mut matched, query.sort, query.random_seed);

    let facts: Vec<AlbumFacts> = matched.iter().map(|a| AlbumFacts::from(&a.album)).collect();
    Ok(BrowseResult {
        total: facts.len(),
        items: BrowseItems::Albums(facts),
    })
}

fn browse_song_genre(raw: &NativeBrowseSong) -> Option<String> {
    if !raw.genre.is_empty() {
        return Some(cut_json(raw.genre.clone(), TEXT_MAX_CHARS));
    }
    raw.genres
        .iter()
        .find(|g| !g.name.is_empty())
        .map(|g| cut_json(g.name.clone(), TEXT_MAX_CHARS))
}

fn song_matches_genre(song: &NativeBrowseSong, want: &str) -> bool {
    song.genre.eq_ignore_ascii_case(want)
        || song
            .genres
            .iter()
            .any(|g| g.name.eq_ignore_ascii_case(want))
}

fn album_matches_genre(album: &SubsonicBrowseAlbum, want: &str) -> bool {
    album
        .album
        .genre
        .as_deref()
        .is_some_and(|g| g.eq_ignore_ascii_case(want))
        || album
            .genres
            .iter()
            .any(|g| g.name.eq_ignore_ascii_case(want))
}

fn song_passes_metadata_filters(
    s: &NativeBrowseSong,
    query: &BrowseQuery,
    normalized_format: Option<&str>,
) -> bool {
    if let Some(ref genre) = query.genre {
        if !song_matches_genre(s, genre) {
            return false;
        }
    }
    if let Some(unrated) = query.unrated {
        if unrated && s.song.rating != 0 {
            return false;
        }
        if !unrated && s.song.rating == 0 {
            return false;
        }
    }
    if let Some(min) = query.min_rating {
        if s.song.rating < min {
            return false;
        }
    }
    if let Some(max) = query.max_rating {
        if s.song.rating > max {
            return false;
        }
    }
    if let Some(starred) = query.starred {
        if s.song.starred != starred {
            return false;
        }
    }
    if let Some(from) = query.year_from {
        if s.song.year < from {
            return false;
        }
    }
    if let Some(to) = query.year_to {
        if s.song.year == 0 || s.song.year > to {
            return false;
        }
    }
    if let Some(want_fmt) = normalized_format {
        if !s.song.suffix.eq_ignore_ascii_case(want_fmt) {
            return false;
        }
    }
    if let Some(max_kbps) = query.max_bitrate_kbps {
        if s.song.bit_rate == 0 || s.song.bit_rate > max_kbps {
            return false;
        }
    }
    true
}

fn album_passes_filters(a: &SubsonicBrowseAlbum, query: &BrowseQuery) -> bool {
    if let Some(ref genre) = query.genre {
        if !album_matches_genre(a, genre) {
            return false;
        }
    }
    let rating = a.album.user_rating.unwrap_or(0);
    if let Some(unrated) = query.unrated {
        if unrated && rating != 0 {
            return false;
        }
        if !unrated && rating == 0 {
            return false;
        }
    }
    if let Some(min) = query.min_rating {
        if rating < min {
            return false;
        }
    }
    if let Some(max) = query.max_rating {
        if rating > max {
            return false;
        }
    }
    if let Some(starred) = query.starred {
        if a.album.starred.is_some() != starred {
            return false;
        }
    }
    let year = a.album.year.unwrap_or(0);
    if let Some(from) = query.year_from {
        if year < from {
            return false;
        }
    }
    if let Some(to) = query.year_to {
        if year == 0 || year > to {
            return false;
        }
    }
    true
}

fn sort_songs(items: &mut [(NativeBrowseSong, SongFacts)], sort: BrowseSort, random_seed: u64) {
    match sort {
        BrowseSort::RecentlyAdded => items.sort_by(|(a, _), (b, _)| {
            let ta = a.created_at.unwrap_or(a.song.updated_at);
            let tb = b.created_at.unwrap_or(b.song.updated_at);
            tb.cmp(&ta).then_with(|| a.song.id.cmp(&b.song.id))
        }),
        BrowseSort::RecentlyPlayed => items.sort_by(|(a, _), (b, _)| {
            b.song
                .play_date
                .cmp(&a.song.play_date)
                .then_with(|| a.song.id.cmp(&b.song.id))
        }),
        BrowseSort::MostPlayed => items.sort_by(|(a, _), (b, _)| {
            b.song
                .play_count
                .cmp(&a.song.play_count)
                .then_with(|| a.song.id.cmp(&b.song.id))
        }),
        BrowseSort::HighestRated => items.sort_by(|(a, _), (b, _)| {
            b.song
                .rating
                .cmp(&a.song.rating)
                .then_with(|| b.song.play_count.cmp(&a.song.play_count))
                .then_with(|| a.song.id.cmp(&b.song.id))
        }),
        BrowseSort::Title => items.sort_by(|(a, _), (b, _)| {
            a.song
                .title
                .to_lowercase()
                .cmp(&b.song.title.to_lowercase())
                .then_with(|| {
                    a.song
                        .artist
                        .to_lowercase()
                        .cmp(&b.song.artist.to_lowercase())
                })
                .then_with(|| a.song.id.cmp(&b.song.id))
        }),
        BrowseSort::Artist => items.sort_by(|(a, _), (b, _)| {
            a.song
                .artist
                .to_lowercase()
                .cmp(&b.song.artist.to_lowercase())
                .then_with(|| {
                    a.song
                        .album
                        .to_lowercase()
                        .cmp(&b.song.album.to_lowercase())
                })
                .then_with(|| a.song.track_number.cmp(&b.song.track_number))
                .then_with(|| {
                    a.song
                        .title
                        .to_lowercase()
                        .cmp(&b.song.title.to_lowercase())
                })
                .then_with(|| a.song.id.cmp(&b.song.id))
        }),
        BrowseSort::Year => items.sort_by(|(a, _), (b, _)| {
            b.song
                .year
                .cmp(&a.song.year)
                .then_with(|| {
                    a.song
                        .artist
                        .to_lowercase()
                        .cmp(&b.song.artist.to_lowercase())
                })
                .then_with(|| {
                    a.song
                        .album
                        .to_lowercase()
                        .cmp(&b.song.album.to_lowercase())
                })
                .then_with(|| a.song.track_number.cmp(&b.song.track_number))
                .then_with(|| a.song.id.cmp(&b.song.id))
        }),
        BrowseSort::Random => items.sort_by(|(a, _), (b, _)| {
            random_key(random_seed, &a.song.id)
                .cmp(&random_key(random_seed, &b.song.id))
                .then_with(|| a.song.id.cmp(&b.song.id))
        }),
    }
}

fn sort_albums(items: &mut [SubsonicBrowseAlbum], sort: BrowseSort, random_seed: u64) {
    match sort {
        BrowseSort::RecentlyAdded => items.sort_by(|a, b| {
            b.created
                .cmp(&a.created)
                .then_with(|| a.album.id.cmp(&b.album.id))
        }),
        BrowseSort::RecentlyPlayed => items.sort_by(|a, b| {
            b.played
                .cmp(&a.played)
                .then_with(|| a.album.id.cmp(&b.album.id))
        }),
        BrowseSort::MostPlayed => items.sort_by(|a, b| {
            b.album
                .play_count
                .unwrap_or(0)
                .cmp(&a.album.play_count.unwrap_or(0))
                .then_with(|| a.album.id.cmp(&b.album.id))
        }),
        BrowseSort::HighestRated => items.sort_by(|a, b| {
            b.album
                .user_rating
                .unwrap_or(0)
                .cmp(&a.album.user_rating.unwrap_or(0))
                .then_with(|| {
                    b.album
                        .play_count
                        .unwrap_or(0)
                        .cmp(&a.album.play_count.unwrap_or(0))
                })
                .then_with(|| a.album.id.cmp(&b.album.id))
        }),
        BrowseSort::Title => items.sort_by(|a, b| {
            a.album
                .display_name()
                .to_lowercase()
                .cmp(&b.album.display_name().to_lowercase())
                .then_with(|| a.album.id.cmp(&b.album.id))
        }),
        BrowseSort::Artist => items.sort_by(|a, b| {
            a.album
                .artist
                .as_deref()
                .unwrap_or("")
                .to_lowercase()
                .cmp(&b.album.artist.as_deref().unwrap_or("").to_lowercase())
                .then_with(|| {
                    a.album
                        .display_name()
                        .to_lowercase()
                        .cmp(&b.album.display_name().to_lowercase())
                })
                .then_with(|| a.album.id.cmp(&b.album.id))
        }),
        BrowseSort::Year => items.sort_by(|a, b| {
            b.album
                .year
                .unwrap_or(0)
                .cmp(&a.album.year.unwrap_or(0))
                .then_with(|| {
                    a.album
                        .display_name()
                        .to_lowercase()
                        .cmp(&b.album.display_name().to_lowercase())
                })
                .then_with(|| a.album.id.cmp(&b.album.id))
        }),
        BrowseSort::Random => items.sort_by(|a, b| {
            random_key(random_seed, &a.album.id)
                .cmp(&random_key(random_seed, &b.album.id))
                .then_with(|| a.album.id.cmp(&b.album.id))
        }),
    }
}

/// Deterministic pseudo-random key for `(seed, id)` via FNV-1a + splitmix64 avalanche, so
/// `BrowseSort::Random` produces a uniform order that stays stable across paginated requests.
pub(crate) fn random_key(seed: u64, id: &str) -> u64 {
    let mut h = 0xcbf29ce484222325u64 ^ seed;
    for b in id.as_bytes() {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x100000001b3);
    }
    h = (h ^ (h >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
    h = (h ^ (h >> 27)).wrapping_mul(0x94d049bb133111eb);
    h ^ (h >> 31)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::test_ctx;
    use crate::db::test_db;
    use crate::test_navidrome::{subsonic_failed, subsonic_ok, user_body, NavidromeEach, NavidromeStub};
    use serde_json::json;

    async fn add_linked_user(db: &SqlitePool, id: &str) -> UserId {
        sqlx::query(
            "INSERT INTO users (id, username, password_hash, navidrome_token) VALUES (?, ?, 'x', 'cipher')",
        )
        .bind(id)
        .bind(id)
        .execute(db)
        .await
        .unwrap();
        UserId::new(id)
    }

    fn scan_status_body(scanning: bool, count: i64, last_scan: &str, error: &str) -> String {
        subsonic_ok(&format!(
            r#""scanStatus":{{"scanning":{scanning},"count":{count},"folderCount":1,"lastScan":"{last_scan}","error":"{error}","elapsedTime":1000000000}}"#
        ))
    }

    #[tokio::test]
    async fn scan_library_starts_quick_and_full_scans_and_waits_for_completion() {
        let db = test_db::migrated().await;
        let alice = add_linked_user(&db, "alice").await;
        let own = NavidromeStub::start();
        own.answer("/rest/getUser", &[&user_body(true)]);
        own.answer(
            "/rest/getScanStatus",
            &[
                &scan_status_body(false, 10, "2026-11-15T10:00:00Z", ""),
                &scan_status_body(true, 11, "2026-11-15T10:00:00Z", ""),
                &scan_status_body(false, 12, "2026-11-15T10:01:00Z", ""),
            ],
        );
        let navidrome = NavidromeEach::of(&[(&alice, &own)]);
        let roles = ScanRoles::default();

        let report = scan_library_in(
            &db,
            &navidrome,
            &roles,
            &alice,
            true,
            5,
            Duration::from_millis(5),
        )
        .await
        .unwrap();

        assert!(report.started);
        assert!(report.full_scan);
        assert!(report.completed);
        assert!(!report.scan.scanning);
        assert_eq!(report.scan.count, 12);
        assert_eq!(own.posted("/rest/startScan"), vec!["fullScan=true"]);

        // Quick scan without waiting returns immediately with completed=false while still scanning.
        let own_quick = NavidromeStub::start();
        own_quick.answer("/rest/getUser", &[&user_body(true)]);
        own_quick.answer(
            "/rest/getScanStatus",
            &[
                &scan_status_body(false, 12, "2026-11-15T10:01:00Z", ""),
                &scan_status_body(true, 3, "2026-11-15T10:01:00Z", ""),
            ],
        );
        let navidrome_quick = NavidromeEach::of(&[(&alice, &own_quick)]);
        let report_quick = scan_library_in(
            &db,
            &navidrome_quick,
            &ScanRoles::default(),
            &alice,
            false,
            0,
            Duration::from_millis(5),
        )
        .await
        .unwrap();
        assert!(report_quick.started);
        assert!(!report_quick.full_scan);
        assert!(!report_quick.completed);
        assert!(report_quick.scan.scanning);
        assert_eq!(own_quick.posted("/rest/startScan"), vec![""]);
    }

    #[tokio::test]
    async fn scan_library_fails_with_index_no_scan_possible_when_no_admin_is_linked() {
        let db = test_db::migrated().await;
        let alice = add_linked_user(&db, "alice").await;
        let own = NavidromeStub::start();
        own.answer("/rest/getUser", &[&user_body(false)]);
        own.answer(
            "/rest/getScanStatus",
            &[&scan_status_body(false, 10, "2026-11-15T10:00:00Z", "")],
        );
        let navidrome = NavidromeEach::of(&[(&alice, &own)]);

        let err = scan_library_in(
            &db,
            &navidrome,
            &ScanRoles::default(),
            &alice,
            false,
            0,
            Duration::from_millis(5),
        )
        .await
        .unwrap_err();
        assert_eq!(err.code, FailureCode::IndexNoScanPossible);
        assert_eq!(err.stage, Stage::Index);
        assert_eq!(err.retry, RetryAdvice::NeedsUser);
        assert_eq!(
            err.user_action.as_ref().map(|a| a.code),
            Some(UserActionCode::EnableIndexing)
        );
    }

    #[tokio::test]
    async fn scan_library_reports_scan_failed_when_navidrome_records_an_error() {
        let db = test_db::migrated().await;
        let alice = add_linked_user(&db, "alice").await;
        let own = NavidromeStub::start();
        own.answer("/rest/getUser", &[&user_body(true)]);
        own.answer(
            "/rest/getScanStatus",
            &[
                &scan_status_body(false, 10, "2026-11-15T10:00:00Z", ""),
                &scan_status_body(false, 10, "2026-11-15T10:00:00Z", "database is locked"),
            ],
        );
        let navidrome = NavidromeEach::of(&[(&alice, &own)]);

        let err = scan_library_in(
            &db,
            &navidrome,
            &ScanRoles::default(),
            &alice,
            false,
            5,
            Duration::from_millis(5),
        )
        .await
        .unwrap_err();
        assert_eq!(err.code, FailureCode::ScanFailed);
        assert_eq!(err.stage, Stage::Index);
        assert!(err.message.contains("database is locked"), "{}", err.message);
    }

    #[tokio::test]
    async fn get_artist_returns_artist_and_albums_or_unknown_handle() {
        let navi = NavidromeStub::start();
        navi.answer(
            "/rest/getArtist",
            &[
                &subsonic_ok(
                    r#""artist":{"id":"ar1","name":"Marrow Choir","albumCount":2,"userRating":5,"starred":"2026-11-02T18:04:51Z","musicBrainzId":"4d5e6f7a-8b9c-4d0e-9f1a-2b3c4d5e6f7a","album":[{"id":"al1","name":"Winter Songs","artist":"Marrow Choir","year":2019,"songCount":10,"duration":2400,"genre":"Choral","userRating":4,"playCount":5},{"id":"al2","name":"Spring Hymns","artist":"Marrow Choir","year":2021,"songCount":8,"duration":1900,"genre":"Folk","userRating":5,"playCount":12}]}"#,
                ),
                &subsonic_failed(70, "Artist not found"),
            ],
        );
        let client = navi.client();

        let (artist, albums) = get_artist(&client, "ar1").await.unwrap();
        assert_eq!(artist.artist_id, "ar1");
        assert_eq!(artist.name, "Marrow Choir");
        assert_eq!(artist.album_count, Some(2));
        assert_eq!(artist.rating, 5);
        assert!(artist.starred);
        assert_eq!(albums.len(), 2);
        assert_eq!(albums[0].album_id, "al1");
        assert_eq!(albums[1].name, "Spring Hymns");

        let err = get_artist(&client, "missing").await.unwrap_err();
        assert_eq!(err.code, FailureCode::UnknownHandle);
        assert_eq!(err.stage, Stage::LibraryCheck);
        assert_eq!(err.message, "No artist with this id");
    }

    #[allow(clippy::too_many_arguments)]
    fn browse_song_json(
        id: &str,
        path: &str,
        title: &str,
        artist: &str,
        album_id: &str,
        genre: &str,
        rating: u8,
        starred: bool,
        year: u32,
        suffix: &str,
        bit_rate: u32,
        play_count: u64,
        play_date: Option<&str>,
        created_at: &str,
        missing: bool,
    ) -> serde_json::Value {
        let mut v = json!({
            "id": id,
            "libraryId": 1,
            "libraryPath": "/music",
            "path": path,
            "title": title,
            "album": format!("Album {album_id}"),
            "albumId": album_id,
            "artist": artist,
            "albumArtist": artist,
            "size": 1000,
            "suffix": suffix,
            "duration": 200.0,
            "bitRate": bit_rate,
            "trackNumber": 1,
            "year": year,
            "rating": rating,
            "starred": starred,
            "playCount": play_count,
            "missing": missing,
            "updatedAt": created_at,
            "createdAt": created_at,
            "genre": genre,
            "genres": [{"name": genre}],
        });
        if let Some(pd) = play_date {
            v["playDate"] = json!(pd);
        }
        v
    }

    #[tokio::test]
    async fn browse_songs_filters_by_folder_genre_rating_format_bitrate_missing_and_sorts() {
        let db = test_db::migrated().await;
        let dir = tempfile::tempdir().unwrap();
        let music_root = dir.path().join("music");
        let alice_dir = music_root.join("alice");
        let shared_dir = music_root.join("shared");
        std::fs::create_dir_all(&alice_dir).unwrap();
        std::fs::create_dir_all(&shared_dir).unwrap();

        sqlx::query("INSERT INTO users (id, username, password_hash) VALUES ('alice', 'alice', 'x'), ('bob', 'bob', 'x')")
            .execute(&db)
            .await
            .unwrap();
        sqlx::query("INSERT INTO folders (id, user_id, name, path, shared) VALUES ('f-alice', 'alice', 'Alice', ?, 0), ('f-shared', 'alice', 'Shared', ?, 1), ('f-bob', 'bob', 'Bob', '/music/bob', 0)")
            .bind(alice_dir.to_string_lossy().as_ref())
            .bind(shared_dir.to_string_lossy().as_ref())
            .execute(&db)
            .await
            .unwrap();

        let songs = json!([
            browse_song_json("s1", "alice/t1.flac", "Beta", "Zeta", "al1", "Ambient", 5, true, 2020, "flac", 900, 10, Some("2026-11-10T00:00:00Z"), "2026-11-01T00:00:00Z", false),
            browse_song_json("s2", "alice/t2.mp3", "Alpha", "Alpha", "al2", "Folk", 0, false, 1995, "mp3", 128, 25, Some("2026-11-14T00:00:00Z"), "2026-11-05T00:00:00Z", false),
            browse_song_json("s3", "shared/t3.mp3", "Gamma", "Beta", "al3", "ambient", 3, false, 2010, "mp3", 320, 2, None, "2026-11-03T00:00:00Z", false),
            browse_song_json("s4", "alice/gone.flac", "Delta", "Gamma", "al4", "Ambient", 4, false, 2018, "flac", 800, 1, None, "2026-11-02T00:00:00Z", true),
        ]);

        let navi = NavidromeStub::start();
        navi.header("/api/song", "X-Total-Count", "4");
        navi.answer("/api/song", &[&songs.to_string()]);
        let client = navi.client();
        let alice = test_ctx("alice");

        // Another user's folder returns unknown_handle before calling Navidrome.
        let bad_folder = browse_library(
            &db,
            &client,
            &alice,
            Some(&music_root),
            BrowseQuery {
                folder_id: Some("f-bob".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap_err();
        assert_eq!(bad_folder.code, FailureCode::UnknownHandle);

        // Filter by folder + genre (case-insensitive) + default missing=false.
        let res = browse_library(
            &db,
            &client,
            &alice,
            Some(&music_root),
            BrowseQuery {
                folder_id: Some("f-alice".into()),
                genre: Some("AMBIENT".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let BrowseItems::Songs(list) = res.items else {
            panic!("expected songs");
        };
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].song.song_id, "s1");
        assert_eq!(
            list[0].song.folder.as_ref().map(|f| f.folder_id.as_str()),
            Some("f-alice")
        );
        assert!(!list[0].missing);

        // Filter by format + max_bitrate_kbps + unrated.
        let res = browse_library(
            &db,
            &client,
            &alice,
            Some(&music_root),
            BrowseQuery {
                format: Some("MP3".into()),
                max_bitrate_kbps: Some(192),
                unrated: Some(true),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let BrowseItems::Songs(list) = res.items else {
            panic!("expected songs");
        };
        assert_eq!(
            list.iter().map(|s| s.song.song_id.as_str()).collect::<Vec<_>>(),
            vec!["s2"]
        );

        // Filter by missing=true returns only missing songs with missing: true.
        let res = browse_library(
            &db,
            &client,
            &alice,
            Some(&music_root),
            BrowseQuery {
                missing: Some(true),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let BrowseItems::Songs(list) = res.items else {
            panic!("expected songs");
        };
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].song.song_id, "s4");
        assert!(list[0].missing);

        // Sort checks: MostPlayed, RecentlyPlayed, HighestRated, Title, Year, RecentlyAdded, Random.
        let ids_for = |sort: BrowseSort| {
            let db = db.clone();
            let client = client.clone();
            let alice = alice.clone();
            let root = music_root.clone();
            async move {
                let res = browse_library(
                    &db,
                    &client,
                    &alice,
                    Some(&root),
                    BrowseQuery {
                        sort,
                        random_seed: 42,
                        ..Default::default()
                    },
                )
                .await
                .unwrap();
                let BrowseItems::Songs(list) = res.items else {
                    panic!("expected songs");
                };
                list.into_iter().map(|s| s.song.song_id).collect::<Vec<_>>()
            }
        };

        assert_eq!(ids_for(BrowseSort::RecentlyAdded).await, vec!["s2", "s3", "s1"]);
        assert_eq!(ids_for(BrowseSort::RecentlyPlayed).await, vec!["s2", "s1", "s3"]);
        assert_eq!(ids_for(BrowseSort::MostPlayed).await, vec!["s2", "s1", "s3"]);
        assert_eq!(ids_for(BrowseSort::HighestRated).await, vec!["s1", "s3", "s2"]);
        assert_eq!(ids_for(BrowseSort::Title).await, vec!["s2", "s1", "s3"]);
        assert_eq!(ids_for(BrowseSort::Artist).await, vec!["s2", "s3", "s1"]);
        assert_eq!(ids_for(BrowseSort::Year).await, vec!["s1", "s3", "s2"]);
        // Random is deterministic for a fixed seed.
        let rand1 = ids_for(BrowseSort::Random).await;
        let rand2 = ids_for(BrowseSort::Random).await;
        assert_eq!(rand1, rand2);
        assert_eq!(rand1.len(), 3);
    }

    #[tokio::test]
    async fn browse_albums_filters_by_metadata_and_folder_via_songs() {
        let db = test_db::migrated().await;
        let dir = tempfile::tempdir().unwrap();
        let music_root = dir.path().join("music");
        let alice_dir = music_root.join("alice");
        std::fs::create_dir_all(&alice_dir).unwrap();

        sqlx::query("INSERT INTO users (id, username, password_hash) VALUES ('alice', 'alice', 'x')")
            .execute(&db)
            .await
            .unwrap();
        sqlx::query("INSERT INTO folders (id, user_id, name, path, shared) VALUES ('f-alice', 'alice', 'Alice', ?, 0)")
            .bind(alice_dir.to_string_lossy().as_ref())
            .execute(&db)
            .await
            .unwrap();

        let albums = subsonic_ok(
            r#""albumList2":{"album":[
                {"id":"al1","name":"Winter Songs","artist":"Marrow Choir","year":2019,"genre":"Choral","userRating":5,"starred":"2026-11-01T00:00:00Z","playCount":40,"created":"2026-11-01T00:00:00Z","played":"2026-11-10T00:00:00Z"},
                {"id":"al2","name":"Autumn Tape","artist":"Vela Norte","year":2015,"genre":"Ambient","userRating":3,"playCount":5,"created":"2026-11-05T00:00:00Z","played":"2026-11-12T00:00:00Z"}
            ]}"#,
        );
        let songs = json!([
            browse_song_json("s1", "alice/t1.flac", "T1", "Marrow Choir", "al1", "Choral", 5, true, 2019, "flac", 900, 10, None, "2026-11-01T00:00:00Z", false),
            browse_song_json("s2", "other/t2.mp3", "T2", "Vela Norte", "al2", "Ambient", 3, false, 2015, "mp3", 192, 5, None, "2026-11-05T00:00:00Z", false),
        ]);

        let navi = NavidromeStub::start();
        navi.answer("/rest/getAlbumList2", &[&albums]);
        navi.header("/api/song", "X-Total-Count", "2");
        navi.answer("/api/song", &[&songs.to_string()]);
        let client = navi.client();
        let alice = test_ctx("alice");

        // Without song-level filters, returns both albums sorted by RecentlyAdded.
        let res = browse_library(
            &db,
            &client,
            &alice,
            Some(&music_root),
            BrowseQuery {
                kind: BrowseKind::Albums,
                sort: BrowseSort::RecentlyAdded,
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let BrowseItems::Albums(list) = res.items else {
            panic!("expected albums");
        };
        assert_eq!(
            list.iter().map(|a| a.album_id.as_str()).collect::<Vec<_>>(),
            vec!["al2", "al1"]
        );

        // With folder_id filter, derives matching album_ids from filtered songs -> only al1.
        let res = browse_library(
            &db,
            &client,
            &alice,
            Some(&music_root),
            BrowseQuery {
                kind: BrowseKind::Albums,
                folder_id: Some("f-alice".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let BrowseItems::Albums(list) = res.items else {
            panic!("expected albums");
        };
        assert_eq!(
            list.iter().map(|a| a.album_id.as_str()).collect::<Vec<_>>(),
            vec!["al1"]
        );
    }
}
