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

use chrono::{Datelike, NaiveDate, Timelike, Utc};
use shared::failure::{Failure, FailureCode, RetryAdvice, Stage};
use shared::navidrome::{DiscoveryStatus, DiscoveryTrack};
use shared::recommendation::{
    discovery_name, Candidate, CandidateReason, CandidateSet, ContextualMixDefinition,
    ContextualMixKind, ContextualMixPlaylist, ContextualMixSummary, ContextualMixTrack,
    CultureBucket, CultureSteering, DiscoveryProfile, SeedRadioRequest, SeedRadioResponse,
    SeedRadioTrack, SetMixEnabledRequest, UpdateCultureProfileRequest, UpsertContextualMixRequest,
    UserCultureProfile,
};
use soulbeet::engine::contextual::MAX_TRACKS_PER_ARTIST;
use soulbeet::navidrome::models::SubsonicSong;
use soulbeet::navidrome::{filepath_rules, NavidromeClient, SmartPlaylistChange};
use soulbeet::MusicKnowledge;
use sqlx::SqlitePool;
use tracing::{info, warn};
use uuid::Uuid;

use crate::auth::{UserCtx, UserId};
use crate::core::database_failure;
use crate::core::discovery::{expires_at, parse_profiles};
use crate::core::index::NavidromeOf;
use crate::core::library::{navidrome_failure, navidrome_for, NavidromePlace};
use crate::models::contextual::{
    ContextualMixDefinitionRow, ContextualMixStateRow, UserCultureProfileRow,
};
use crate::models::discovery_playlist::DiscoveryTrackRow;
use crate::models::navidrome_library::NavidromeLibraryRow;
use crate::models::user_settings::UserSettings;
use crate::services::NavidromeLookupError;

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

/// Human-readable title-cased fallback name for Navidrome playlists when `custom_name` is unset
/// (e.g. `"winter_holiday"` -> `"Winter Holiday"`).
fn title_case_slug(slug: &str) -> String {
    slug.split(['_', '-'])
        .filter(|w| !w.is_empty())
        .map(|w| {
            let mut chars = w.chars();
            match chars.next() {
                Some(first) => format!("{}{}", first.to_uppercase(), chars.as_str()),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// True when `path` sits inside a staged `Discovery/` directory.
fn is_staged_discovery_path(path: &str) -> bool {
    path.split(['/', '\\'])
        .any(|seg| seg.eq_ignore_ascii_case("discovery"))
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
    let library = match navi.get_all_songs_with_ratings().await {
        Ok(songs) => songs,
        Err(e) => {
            warn!(
                "Could not fetch Navidrome songs to reconcile discovery song_ids for user {}: {}",
                user_id, e
            );
            return 0;
        }
    };
    reconcile_discovery_song_ids_in(db, &library, music_path, tracks).await
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
                format!("Mix '{slug}' is managed globally or belongs to another user"),
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

/// Builds the exact playable blended tracklist for `def` from owned `library` songs and staged
/// `pending` discovery tracks using the mix's `owned_ratio`, `theme_tags`, and `MAX_TRACKS_PER_ARTIST`
/// cap. Shared by both `list_active_contextual_mixes_in` (for exact playable `track_count`) and
/// `resolve_contextual_mix_playlist_in` so card counts always match the resolved playlist.
fn build_contextual_mix_tracks(
    def: &ContextualMixDefinition,
    library: Option<&[SubsonicSong]>,
    pending: &[DiscoveryTrack],
) -> Vec<ContextualMixTrack> {
    let target = def.target_tracks as usize;
    let owned_target = ((target as f32) * def.owned_ratio).round() as usize;
    let fresh_target = target.saturating_sub(owned_target);

    let mut owned_tracks: Vec<ContextualMixTrack> = Vec::new();
    let mut seen_keys: HashSet<String> = HashSet::new();
    let mut artist_counts: HashMap<String, usize> = HashMap::new();

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

            // Prefer owned library songs matching theme_tags, excluding disliked tracks and
            // staged Discovery/ or pending discovery_tracks rows.
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
                let genre = song.genre.as_deref().unwrap_or("").to_ascii_lowercase();
                let hit = tags_lower
                    .iter()
                    .find(|t| !t.is_empty() && genre.contains(t.as_str()))
                    .cloned();
                if let Some(matched_genre) = hit {
                    matched.push((song, Some(matched_genre)));
                } else {
                    fallback.push((song, None));
                }
            }

            // Sort both buckets by user rating desc, play count desc for high hit-rate mixes.
            let sort_key = |s: &SubsonicSong| {
                (
                    std::cmp::Reverse(s.user_rating.unwrap_or(0)),
                    std::cmp::Reverse(s.play_count.unwrap_or(0)),
                )
            };
            matched.sort_by_key(|(s, _)| sort_key(s));
            fallback.sort_by_key(|(s, _)| sort_key(s));

            for (song, matched_genre) in matched.into_iter().chain(fallback) {
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
                    reason: Some(CandidateReason::LibraryMatch {
                        mix_slug: def.slug.clone(),
                        matched_genre,
                    }),
                    culture_bucket: None,
                });
            }
        }
    }

    let duration_by_song_id: HashMap<&str, u32> = library
        .map(|songs| {
            songs
                .iter()
                .filter_map(|s| s.duration.filter(|&d| d > 0).map(|d| (s.id.as_str(), d)))
                .collect()
        })
        .unwrap_or_default();

    // Collect staged fresh tracks: exact pack_slug matches first, then general pending discovery tracks
    // for hybrid moment mixes (never pulling seasonal tracks into non-seasonal mixes!).
    let mut fresh_tracks: Vec<ContextualMixTrack> = Vec::new();
    let staged_iter = pending
        .iter()
        .filter(|t| t.pack_slug.as_deref() == Some(def.slug.as_str()))
        .chain(pending.iter().filter(|t| {
            def.kind == ContextualMixKind::Moment
                && t.pack_slug.is_none()
                && !t.exclude_from_core_taste
        }));

    let fresh_limit = fresh_target.max(if def.kind == ContextualMixKind::Seasonal {
        target
    } else {
        0
    });

    for dt in staged_iter {
        if fresh_tracks.len() >= fresh_limit {
            break;
        }
        let Some(song_id) = dt.song_id.as_deref().filter(|s| !s.is_empty()) else {
            continue;
        };
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
            duration_s: duration_by_song_id.get(song_id).copied(),
            owned: false,
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

fn summarize_definition(
    def: &ContextualMixDefinition,
    culture: &UserCultureProfile,
    state: Option<&ContextualMixStateRow>,
    pending: &[DiscoveryTrack],
    library: Option<&[SubsonicSong]>,
    now: chrono::DateTime<Utc>,
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
    let track_count = build_contextual_mix_tracks(def, library, pending).len() as u32;

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
    let duration_by_song_id: HashMap<&str, u32> = library
        .map(|songs| {
            songs
                .iter()
                .filter_map(|s| s.duration.filter(|&d| d > 0).map(|d| (s.id.as_str(), d)))
                .collect()
        })
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
            duration_s: duration_by_song_id.get(song_id).copied(),
            owned: false,
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
        track_count,
        target_tracks: target,
    }
}

/// Lists active contextual mixes for `ctx` as pure structured client-i18n summaries:
/// 1. Active calendar Seasonal Packs (matching date + user country or explicitly opted in)
/// 2. Active Moment Mixes (always-surfaced + matching current hour window, using `local_hour` when supplied)
/// 3. Enabled Rolling Discovery profiles (`mix.rolling.conservative` / `balanced` / `adventurous`)
pub(crate) async fn list_active_contextual_mixes_in(
    db: &SqlitePool,
    navidrome: &dyn NavidromeOf,
    music_path: Option<&Path>,
    ctx: &UserCtx,
    now: chrono::DateTime<Utc>,
    local_hour: Option<u8>,
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
            effective_country,
            explicitly_enabled,
        ) {
            continue;
        }
        active_defs.push((def, state));
    }

    let has_active_moment = active_defs
        .iter()
        .any(|(def, _)| def.kind == ContextualMixKind::Moment || def.owned_ratio > 0.0);
    let needs_reconcile = pending.iter().any(needs_song_id_reconciliation);

    // Fetch Navidrome library ONCE for the whole listing call, only when needed to count Moment
    // mix owned tracks or reconcile missing pending `song_id`s.
    let library: Option<Vec<SubsonicSong>> = if has_active_moment || needs_reconcile {
        match navidrome.navidrome_of(&ctx.id()).await {
            Ok(navi) => match navi.get_all_songs_with_ratings().await {
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
        if needs_reconcile {
            reconcile_discovery_song_ids_in(db, songs, music_path, &mut pending).await;
        }
    }

    let mut seasonal = Vec::new();
    let mut moments = Vec::new();

    for (def, state) in active_defs {
        let summary =
            summarize_definition(def, &culture, state, &pending, library.as_deref(), now);
        match def.kind {
            ContextualMixKind::Seasonal => seasonal.push(summary),
            ContextualMixKind::Moment => moments.push(summary),
            ContextualMixKind::Rolling => {}
        }
    }

    // Put Drive / offline_priority moments first so Android Auto / mobile quick-launch sees it immediately.
    moments.sort_by_key(|m| (!m.offline_priority, m.slug.clone()));

    let mut mixes = Vec::new();
    mixes.extend(seasonal);
    mixes.extend(moments);

    // Rolling Personal Mixes (count ONLY reconciled playable pending tracks matching resolve)
    for profile in parse_profiles(&settings.discovery_profiles) {
        let playable_count =
            build_rolling_mix_tracks(profile, library.as_deref(), &pending).len() as u32;
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
    let library: Option<Vec<SubsonicSong>> = if def.owned_ratio > 0.0 || needs_reconcile {
        match navidrome.navidrome_of(&ctx.id()).await {
            Ok(navi) => match navi.get_all_songs_with_ratings().await {
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
        }
    } else {
        None
    };
    if let Some(ref songs) = library {
        if needs_reconcile {
            reconcile_discovery_song_ids_in(db, songs, music_path, &mut pending).await;
        }
    }
    Ok(summarize_definition(
        &def,
        &culture,
        Some(&state),
        &pending,
        library.as_deref(),
        now,
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

    let today = now.date_naive();
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
                culture.effective_country(),
                explicitly_enabled,
            );

        if active {
            let place = seasonal_root.child(&def.slug);
            let prefix = place.song_prefix();
            // Navidrome smart playlist name uses custom_name when set, else a readable title-cased
            // fallback (clients map `navidrome_playlist_id` -> `ContextualMixSummary.i18n_key`).
            let playlist_name = def
                .custom_name
                .clone()
                .unwrap_or_else(|| format!("Soulbeet: {}", title_case_slug(&def.slug)));
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
            // Auto-clean expired seasonal smart playlists once past expires_at and no pending tracks remain.
            let is_expired = st
                .expires_at
                .as_deref()
                .and_then(|s| NaiveDate::parse_from_str(s, "%Y-%m-%d").ok())
                .is_some_and(|exp| today > exp);
            let has_pending = pending
                .iter()
                .any(|t| t.pack_slug.as_deref() == Some(def.slug.as_str()));
            if is_expired && !has_pending {
                if let Some(ref pl_id) = st.navidrome_playlist_id {
                    let gone = match navi.delete_smart_playlist(pl_id).await {
                        Ok(()) => true,
                        Err(soulbeet::NavidromeError::Http { status: 404, .. }) => true,
                        Err(e) => match navi.get_native_playlist(pl_id).await {
                            Ok(None) => true,
                            _ => {
                                warn!(
                                    "Could not clean up expired seasonal playlist '{}' ({}) for user {}: {}",
                                    def.slug, pl_id, user_id, e
                                );
                                false
                            }
                        },
                    };
                    if gone {
                        info!(
                            "Cleaned up expired seasonal Navidrome playlist '{}' ({}) for user {}",
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
                                "Failed to clear expired seasonal playlist id for '{}' (user {}): {}",
                                def.slug, user_id, e
                            );
                        }
                    }
                }
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
                let library: Option<Vec<SubsonicSong>> = if needs_reconcile {
                    match navidrome.navidrome_of(&ctx.id()).await {
                        Ok(navi) => match navi.get_all_songs_with_ratings().await {
                            Ok(songs) => Some(songs),
                            Err(e) => {
                                warn!(
                                    "Could not fetch Navidrome songs to reconcile discovery song_ids for rolling mix '{}' (user {}): {}",
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
                    if needs_reconcile {
                        reconcile_discovery_song_ids_in(db, songs, music_path, &mut pending).await;
                    }
                }
                let tracks = build_rolling_mix_tracks(profile, library.as_deref(), &pending);
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

    let library: Option<Vec<SubsonicSong>> = if owned_target > 0 || needs_reconcile {
        if let Some(ref navi) = navi_opt {
            match navi.get_all_songs_with_ratings().await {
                Ok(songs) => Some(songs),
                Err(e) if owned_target > 0 => {
                    return Err(navidrome_failure(Stage::Playlist, &e));
                }
                Err(e) => {
                    warn!(
                        "Could not fetch Navidrome songs to reconcile discovery song_ids for mix '{}' (user {}): {}",
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
        if needs_reconcile {
            reconcile_discovery_song_ids_in(db, songs, music_path, &mut pending).await;
        }
    }

    let tracks = build_contextual_mix_tracks(&def, library.as_deref(), &pending);

    // Optionally materialize as a real static Navidrome playlist (ONLY for Moment mixes and ONLY
    // when the caller explicitly requested `sync_navidrome = true`; Seasonal mixes use smart
    // playlists reconciled by `reconcile_seasonal_playlists_in`).
    if sync_navidrome && def.kind == ContextualMixKind::Moment && !tracks.is_empty() {
        if let Some(ref navi) = navi_opt {
            let song_ids: Vec<String> = tracks.iter().map(|t| t.song_id.clone()).collect();
            let playlist_name = def
                .custom_name
                .clone()
                .unwrap_or_else(|| format!("Soulbeet: {}", title_case_slug(&def.slug)));
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

    let mut summary = summarize_definition(
        &def,
        &culture,
        state.as_ref(),
        &pending,
        library.as_deref(),
        now,
    );
    summary.track_count = tracks.len() as u32;

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
    maybe_reconcile_discovery_song_ids_in(db, navidrome, music_path, user_id, &mut pending).await;
    let lifetimes = settings.parse_lifetime_days();
    pending.sort_by_key(|t| {
        let exp = expires_at(t, &lifetimes);
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

    let all_songs = navi
        .get_all_songs_with_ratings()
        .await
        .map_err(|e| navidrome_failure(Stage::LibraryCheck, &e))?;

    // Track every song in the user's library so already-owned tracks are never returned as
    // unowned `prefetch_candidates`.
    let library_keys: HashSet<String> = all_songs
        .iter()
        .map(|s| CandidateSet::key(s.artist.as_deref().unwrap_or(""), &s.title))
        .collect();

    let seed_artist_key = discovery_name(&seed_artist);
    for song in &all_songs {
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
                for song in &all_songs {
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

        // Active mixes in December carry pure i18n keys + ISO dates, zero hardcoded English titles
        let mixes = list_active_contextual_mixes_in(&db, &NoNavidrome, None, &ctx, dec, None)
            .await
            .unwrap();
        assert_eq!(mixes[0].kind, ContextualMixKind::Seasonal);
        assert_eq!(mixes[0].slug, "winter_holiday");
        assert_eq!(mixes[0].i18n_key, "mix.seasonal.winter_holiday");
        assert_eq!(mixes[0].custom_name, None);
        assert_eq!(mixes[0].local_pct, Some(75));
        assert!(mixes.iter().any(|m| m.slug == "drive" && m.offline_priority));
        assert!(mixes.iter().any(|m| m.slug == "focus"));

        // local_hour overrides UTC hour for Moment windows (morning = 6..=10, late_night = 22..=4)
        let morning_mixes =
            list_active_contextual_mixes_in(&db, &NoNavidrome, None, &ctx, dec, Some(8))
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
                always_surfaced: false,
                countries: vec![],
                owned_ratio: 0.0,
                default_local_ratio: Some(0.50),
                exclude_from_core_taste: true,
                offline_priority: false,
                target_tracks: 20,
                sync_to_navidrome: true,
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
                always_surfaced: false,
                countries: vec![],
                owned_ratio: 0.0,
                default_local_ratio: None,
                exclude_from_core_taste: true,
                offline_priority: false,
                target_tracks: 20,
                sync_to_navidrome: true,
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
                always_surfaced: false,
                countries: vec![],
                owned_ratio: 0.5,
                default_local_ratio: None,
                exclude_from_core_taste: false,
                offline_priority: false,
                target_tracks: 20,
                sync_to_navidrome: false,
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
                always_surfaced: false,
                countries: vec![],
                owned_ratio: 0.75,
                default_local_ratio: None,
                exclude_from_core_taste: false,
                offline_priority: false,
                target_tracks: 25,
                sync_to_navidrome: false,
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
                always_surfaced: false,
                countries: vec![],
                owned_ratio: 0.0,
                default_local_ratio: Some(0.50),
                exclude_from_core_taste: true,
                offline_priority: false,
                target_tracks: 20,
                sync_to_navidrome: true,
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
                always_surfaced: false,
                countries: vec![],
                owned_ratio: 0.0,
                default_local_ratio: Some(0.50),
                exclude_from_core_taste: true,
                offline_priority: false,
                target_tracks: 20,
                sync_to_navidrome: true,
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
        let mixes = list_active_contextual_mixes_in(&db, &NoNavidrome, None, &alice, dec, None)
            .await
            .unwrap();
        assert!(!mixes.iter().any(|m| m.slug == "winter_holiday"));

        // Custom mix surfaces during its calendar window and deleting it cleans up contextual_mix_states
        let june = Utc.with_ymd_and_hms(2026, 6, 21, 14, 0, 0).unwrap();
        let june_mixes =
            list_active_contextual_mixes_in(&db, &NoNavidrome, None, &alice, june, None)
                .await
                .unwrap();
        assert!(june_mixes.iter().any(|m| m.slug == "midsummer"));

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
                        {"id":"nd-fresh-1","title":"Night Drive","artist":"Fresh Band","album":"EP","genre":"Electronic","duration":200}
                    ]}"#,
                ),
                &subsonic_ok(
                    r#""searchResult3":{"song":[
                        {"id":"nd-own-1","title":"Roadsong","artist":"Synth Act","album":"LP","genre":"Electronic","duration":210,"userRating":5,"playCount":12},
                        {"id":"nd-own-2","title":"Highway","artist":"Indie Act","album":"LP2","genre":"Indie Rock","duration":195,"userRating":4,"playCount":8},
                        {"id":"nd-disliked","title":"Bad Tune","artist":"Skip Band","album":"LP","genre":"Electronic","duration":180,"userRating":1,"playCount":1},
                        {"id":"nd-staged-path","title":"Staged Track","artist":"Staged Band","album":"EP","genre":"Electronic","duration":180,"path":"alice/Discovery/Balanced/Staged.flac"},
                        {"id":"nd-fresh-1","title":"Night Drive","artist":"Fresh Band","album":"EP","genre":"Electronic","duration":200}
                    ]}"#,
                ),
            ],
        );
        stub.answer(
            "/rest/createPlaylist",
            &[&subsonic_ok(
                r#""playlist":{"id":"pl-drive-1","name":"Soulbeet: Drive","songCount":3,"duration":600,"owner":"alice","created":"2026-06-15T19:00:00Z","changed":"2026-06-15T19:00:00Z"}"#,
            )],
        );

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
        assert_eq!(
            stub.asked().len(),
            before_calls,
            "seasonal mix must never call static Navidrome playlist sync"
        );
    }

    #[tokio::test]
    async fn seasonal_reconciliation_creates_smart_playlist_and_cleans_up_expired_ones() {
        let db = test_db::migrated().await;
        folder_of(&db, "alice").await;

        let stub = NavidromeStub::start();
        stub.answer(
            "/api/playlist",
            &[r#"{"id":"sp-winter-1","name":"Soulbeet: Winter Holiday","comment":"soulbeet:contextual:winter_holiday","public":false}"#],
        );

        let place = NavidromePlace {
            library_id: 1,
            path: "alice/Discovery".to_string(),
        };

        // In December, winter_holiday smart playlist is created and persisted
        let dec = Utc.with_ymd_and_hms(2026, 12, 10, 12, 0, 0).unwrap();
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

        // After expiry (Jan 10, 2027) with no pending tracks, even a 404 (already deleted) clears the stored ID
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
        assert_eq!(stub.asked().len(), 1);

        // Persisted in SQLite: a second call hits SQLite ONLY with zero Navidrome requests!
        let persisted = DiscoveryTrackRow::get_by_id_in(&db, "dt-path")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(persisted.song_id.as_deref(), Some("nd-path-match"));

        let second = list_triage_queue_in(
            &db,
            &StubNavidrome(stub.client()),
            Some(Path::new("/local/music")),
            "alice",
        )
        .await
        .unwrap();
        assert_eq!(second.len(), 2);
        assert_eq!(
            stub.asked().len(),
            1,
            "once reconciled, triage queue must not call Navidrome again"
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
                duration: Some(200),
                user_rating: None,
                play_count: None,
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
                duration: Some(180),
                user_rating: None,
                play_count: None,
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
                duration: Some(180),
                user_rating: None,
                play_count: None,
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
        )
        .await
        .unwrap();

        // Only ONE Navidrome fetch for the whole list call
        assert_eq!(stub.asked().len(), 1);

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
            duration: Some(200),
            user_rating: None,
            play_count: None,
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
        let mixes = list_active_contextual_mixes_in(&db, &NoNavidrome, None, &ctx, dec, Some(14))
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
}
