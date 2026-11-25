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

use chrono::{Datelike, NaiveDate, Timelike, Utc};
use shared::failure::{Failure, FailureCode, RetryAdvice, Stage};
use shared::navidrome::DiscoveryTrack;
use shared::recommendation::{
    discovery_name, Candidate, CandidateReason, CandidateSet, ContextualMixDefinition,
    ContextualMixKind, ContextualMixPlaylist, ContextualMixSummary, ContextualMixTrack,
    CultureBucket, CultureSteering, DiscoveryProfile, SeedRadioRequest, SeedRadioResponse,
    SeedRadioTrack, SetMixEnabledRequest, UpdateCultureProfileRequest, UpsertContextualMixRequest,
    UserCultureProfile,
};
use soulbeet::engine::contextual::MAX_TRACKS_PER_ARTIST;
use soulbeet::navidrome::{filepath_rules, NavidromeClient, SmartPlaylistChange};
use soulbeet::MusicKnowledge;
use sqlx::SqlitePool;
use tracing::{info, warn};
use uuid::Uuid;

use crate::auth::UserCtx;
use crate::core::database_failure;
use crate::core::discovery::{expires_at, parse_profiles};
use crate::core::index::NavidromeOf;
use crate::core::library::{navidrome_failure, navidrome_for, NavidromePlace};
use crate::models::contextual::{
    ContextualMixDefinitionRow, ContextualMixStateRow, UserCultureProfileRow,
};
use crate::models::discovery_playlist::DiscoveryTrackRow;
use crate::models::user_settings::UserSettings;

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

    let slug = normalize_slug(&req.slug)?;
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
    } else {
        Some(ctx.user_id.clone())
    };

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
        enabled: true,
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
    let slug = normalize_slug(slug)?;
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
    let slug = normalize_slug(&req.slug)?;
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

fn summarize_definition(
    def: &ContextualMixDefinition,
    culture: &UserCultureProfile,
    state: Option<&ContextualMixStateRow>,
    pending: &[DiscoveryTrack],
    now: chrono::DateTime<Utc>,
) -> ContextualMixSummary {
    let steering = state
        .map(|s| CultureSteering::parse(&s.steering))
        .unwrap_or(CultureSteering::Balanced);
    let effective_local = def.default_local_ratio.map(|base| {
        let base_ratio = if base > 0.0 {
            culture.local_culture_ratio
        } else {
            base
        };
        steering.effective_ratio(base_ratio)
    });
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
    let track_count = pending
        .iter()
        .filter(|t| t.pack_slug.as_deref() == Some(def.slug.as_str()))
        .count() as u32;

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

/// Lists active contextual mixes for `ctx` as pure structured client-i18n summaries:
/// 1. Active calendar Seasonal Packs (matching date + user country or explicitly opted in)
/// 2. Active Moment Mixes (always-surfaced + matching current hour window)
/// 3. Enabled Rolling Discovery profiles (`mix.rolling.conservative` / `balanced` / `adventurous`)
pub(crate) async fn list_active_contextual_mixes_in(
    db: &SqlitePool,
    ctx: &UserCtx,
    now: chrono::DateTime<Utc>,
) -> Result<Vec<ContextualMixSummary>, Failure> {
    let settings = UserSettings::get_in(db, &ctx.user_id)
        .await
        .map_err(|e| database_failure(Stage::Input, e))?;
    let culture = get_culture_profile_in(db, &ctx.user_id).await?;
    let pending = DiscoveryTrackRow::pending_for_user_in(db, &ctx.user_id)
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
    let mut seasonal = Vec::new();
    let mut moments = Vec::new();

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
            now.hour(),
            effective_country,
            explicitly_enabled,
        ) {
            continue;
        }
        let summary = summarize_definition(def, &culture, state, &pending, now);
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

    // Rolling Personal Mixes
    let counts = settings.parse_track_counts();
    for profile in parse_profiles(&settings.discovery_profiles) {
        let profile_name = profile.name();
        let slug = profile_name.to_ascii_lowercase();
        let i18n_key = match profile {
            DiscoveryProfile::Conservative => "mix.rolling.conservative",
            DiscoveryProfile::Balanced => "mix.rolling.balanced",
            DiscoveryProfile::Adventurous => "mix.rolling.adventurous",
        };
        let pending_count = pending
            .iter()
            .filter(|t| t.profile == profile_name && t.pack_slug.is_none())
            .count() as u32;
        let target = counts.get(profile_name).copied().unwrap_or(10);
        let navidrome_playlist_id = UserSettings::get_playlist_id_for_profile(
            &settings.discovery_navidrome_playlist_id,
            profile_name,
        );
        mixes.push(ContextualMixSummary {
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
            track_count: pending_count,
            target_tracks: target,
        });
    }

    Ok(mixes)
}

/// Updates the user's live culture steering (`MoreLocal`, `Balanced`, `MoreInternational`) for
/// any contextual mix slug (supporting legacy `"noel"` / `"holiday"` aliases for `"winter_holiday"`).
pub(crate) async fn steer_seasonal_pack_in(
    db: &SqlitePool,
    ctx: &UserCtx,
    slug: &str,
    steering: CultureSteering,
    now: chrono::DateTime<Utc>,
) -> Result<ContextualMixSummary, Failure> {
    let canonical = match slug.trim().to_ascii_lowercase().as_str() {
        "noel" | "holiday" => "winter_holiday".to_string(),
        "nye" => "new_years_eve".to_string(),
        other => normalize_slug(other)?,
    };
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
    let culture = get_culture_profile_in(db, &ctx.user_id).await?;
    let effective_ratio = steering.effective_ratio(culture.local_culture_ratio);
    let state = ContextualMixStateRow::upsert_steering_in(
        db,
        &ctx.user_id,
        &def.slug,
        steering,
        effective_ratio,
    )
    .await
    .map_err(|e| database_failure(Stage::Input, e))?;
    let pending = DiscoveryTrackRow::pending_for_user_in(db, &ctx.user_id)
        .await
        .map_err(|e| database_failure(Stage::Input, e))?;
    Ok(summarize_definition(&def, &culture, Some(&state), &pending, now))
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
            // Navidrome smart playlist name uses custom_name when set, else a stable canonical slug tag
            // (clients map `navidrome_playlist_id` -> `ContextualMixSummary.i18n_key` for localized display).
            let playlist_name = def
                .custom_name
                .clone()
                .unwrap_or_else(|| format!("Soulbeet: {}", def.slug));
            let comment = format!("soulbeet:contextual:{}", def.slug);
            let expires_at = def
                .calendar_window
                .as_ref()
                .and_then(|w| w.cycle_dates(now.year(), now.month(), now.day()))
                .map(|(_, _, exp)| exp);

            let stored_id = state.and_then(|s| s.navidrome_playlist_id.as_deref());
            let existing = match stored_id {
                Some(id) => navi.get_native_playlist(id).await.map_err(|e| e.to_string())?,
                None => None,
            };

            let playlist_id = match existing {
                Some(pl) => {
                    let rules = filepath_rules(place.library_id, &prefix);
                    let change = SmartPlaylistChange {
                        name: (pl.name != playlist_name).then_some(playlist_name.as_str()),
                        rules: (pl.rules.as_ref() != Some(&rules)).then_some(&rules),
                    };
                    if change.name.is_some() || change.rules.is_some() {
                        navi.update_smart_playlist(&pl.id, &change)
                            .await
                            .map_err(|e| e.to_string())?;
                    }
                    pl.id
                }
                None => {
                    let created_id = navi
                        .create_smart_playlist(&playlist_name, &comment, place.library_id, &prefix)
                        .await
                        .map_err(|e| e.to_string())?;
                    info!(
                        "Created seasonal Navidrome smart playlist '{}' ({}) for user {}",
                        playlist_name, created_id, user_id
                    );
                    created_id
                }
            };

            ContextualMixStateRow::upsert_playlist_id_in(
                db,
                user_id,
                &def.slug,
                Some(&playlist_id),
                expires_at.as_deref(),
            )
            .await?;
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
                    if let Err(e) = navi.delete_smart_playlist(pl_id).await {
                        warn!(
                            "Could not clean up expired seasonal playlist '{}' ({}) for user {}: {}",
                            def.slug, pl_id, user_id, e
                        );
                    } else {
                        info!(
                            "Cleaned up expired seasonal Navidrome playlist '{}' ({}) for user {}",
                            def.slug, pl_id, user_id
                        );
                        ContextualMixStateRow::upsert_playlist_id_in(
                            db,
                            user_id,
                            &def.slug,
                            None,
                            st.expires_at.as_deref(),
                        )
                        .await?;
                    }
                }
            }
        }
    }

    Ok(reconciled_slugs)
}

/// Resolves a playable blended playlist for any contextual mix slug:
/// - For Moment/hybrid mixes (`owned_ratio > 0`): selects owned Navidrome songs matching the mix's
///   `theme_tags` (capped per artist for diversity) blended with staged fresh Discovery tracks.
/// - For Seasonal mixes (`owned_ratio == 0`): returns staged Discovery tracks for `pack_slug`.
/// - When `sync_navidrome` is `true` (or `def.sync_to_navidrome` is `true` on a Moment mix) and at
///   least one playable `song_id` exists, creates or updates a real static Navidrome playlist and
///   persists its `navidrome_playlist_id` in `contextual_mix_states`.
pub(crate) async fn resolve_contextual_mix_playlist_in(
    db: &SqlitePool,
    navidrome: &dyn NavidromeOf,
    ctx: &UserCtx,
    slug: &str,
    sync_navidrome: bool,
    now: chrono::DateTime<Utc>,
) -> Result<ContextualMixPlaylist, Failure> {
    let canonical = match slug.trim().to_ascii_lowercase().as_str() {
        "noel" | "holiday" => "winter_holiday".to_string(),
        "nye" => "new_years_eve".to_string(),
        other => normalize_slug(other)?,
    };
    let def = ContextualMixDefinitionRow::get_visible_by_slug_in(db, &ctx.user_id, &canonical)
        .await
        .map_err(|e| database_failure(Stage::Playlist, e))?
        .ok_or_else(|| {
            Failure::new(
                FailureCode::UnknownHandle,
                Stage::Playlist,
                RetryAdvice::NotRetryable,
                format!("Contextual mix '{slug}' not found"),
            )
        })?;

    let culture = get_culture_profile_in(db, &ctx.user_id).await?;
    let mut state = ContextualMixStateRow::get_in(db, &ctx.user_id, &def.slug)
        .await
        .map_err(|e| database_failure(Stage::Playlist, e))?;
    let pending = DiscoveryTrackRow::pending_for_user_in(db, &ctx.user_id)
        .await
        .map_err(|e| database_failure(Stage::Playlist, e))?;

    let target = def.target_tracks as usize;
    let owned_target = ((target as f32) * def.owned_ratio).round() as usize;
    let fresh_target = target.saturating_sub(owned_target);

    let mut owned_tracks: Vec<ContextualMixTrack> = Vec::new();
    let mut seen_keys: HashSet<String> = HashSet::new();
    let mut artist_counts: HashMap<String, usize> = HashMap::new();

    let navi_opt = navidrome_for(navidrome, ctx, Stage::Playlist).await.ok();

    if owned_target > 0 {
        if let Some(ref navi) = navi_opt {
            let library = navi
                .get_all_songs_with_ratings()
                .await
                .map_err(|e| navidrome_failure(Stage::Playlist, &e))?;

            let tags_lower: Vec<String> = def
                .theme_tags
                .iter()
                .map(|t| t.to_ascii_lowercase())
                .collect();

            // Prefer songs whose genre matches any of the mix's theme_tags, then fill from top-rated/played library.
            let mut matched = Vec::new();
            let mut fallback = Vec::new();
            for song in library {
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
            let sort_key = |s: &soulbeet::navidrome::models::SubsonicSong| {
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
                    song_id: song.id,
                    discovery_track_id: None,
                    artist,
                    title: song.title,
                    album: song.album,
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

    for dt in staged_iter {
        if fresh_tracks.len() >= fresh_target.max(if def.kind == ContextualMixKind::Seasonal {
            target
        } else {
            0
        }) {
            break;
        }
        let Some(ref song_id) = dt.song_id else {
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
            song_id: song_id.clone(),
            discovery_track_id: Some(dt.id.clone()),
            artist: dt.artist.clone(),
            title: dt.title.clone(),
            album: (!dt.album.is_empty()).then(|| dt.album.clone()),
            duration_s: None,
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

    // Optionally materialize as a real static Navidrome playlist (for Moment / custom mixes).
    if (sync_navidrome || (def.kind == ContextualMixKind::Moment && def.sync_to_navidrome))
        && !tracks.is_empty()
    {
        if let Some(ref navi) = navi_opt {
            let song_ids: Vec<String> = tracks.iter().map(|t| t.song_id.clone()).collect();
            let playlist_name = def
                .custom_name
                .clone()
                .unwrap_or_else(|| format!("Soulbeet: {}", def.slug));
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

    let mut summary = summarize_definition(&def, &culture, state.as_ref(), &pending, now);
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
    user_id: &str,
) -> Result<Vec<DiscoveryTrack>, Failure> {
    let settings = UserSettings::get_in(db, user_id)
        .await
        .map_err(|e| database_failure(Stage::Input, e))?;
    let mut pending = DiscoveryTrackRow::pending_for_user_in(db, user_id)
        .await
        .map_err(|e| database_failure(Stage::Input, e))?;
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
        if let Ok(neighbours) = knowledge.similar_artists(mbid, &seed_artist, 12).await {
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
                                if !owned_keys.contains(&key) && seen_prefetch.insert(key) {
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
        let steered =
            steer_seasonal_pack_in(&db, &ctx, "winter_holiday", CultureSteering::MoreLocal, dec)
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
        let mixes = list_active_contextual_mixes_in(&db, &ctx, dec).await.unwrap();
        assert_eq!(mixes[0].kind, ContextualMixKind::Seasonal);
        assert_eq!(mixes[0].slug, "winter_holiday");
        assert_eq!(mixes[0].i18n_key, "mix.seasonal.winter_holiday");
        assert_eq!(mixes[0].custom_name, None);
        assert_eq!(mixes[0].local_pct, Some(75));
        assert!(mixes.iter().any(|m| m.slug == "drive" && m.offline_priority));
        assert!(mixes.iter().any(|m| m.slug == "focus"));
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

        // Built-ins cannot be deleted, only disabled per-user
        let cant_del = delete_mix_definition_in(&db, &admin, "winter_holiday")
            .await
            .unwrap_err();
        assert_eq!(cant_del.code, FailureCode::InvalidInput);

        set_mix_enabled_in(
            &db,
            &alice,
            SetMixEnabledRequest {
                slug: "winter_holiday".into(),
                enabled: false,
            },
        )
        .await
        .unwrap();
        let dec = Utc.with_ymd_and_hms(2026, 12, 10, 14, 0, 0).unwrap();
        let mixes = list_active_contextual_mixes_in(&db, &alice, dec)
            .await
            .unwrap();
        assert!(!mixes.iter().any(|m| m.slug == "winter_holiday"));

        // Custom mix surfaces during its calendar window and can be deleted
        let june = Utc.with_ymd_and_hms(2026, 6, 21, 14, 0, 0).unwrap();
        let june_mixes = list_active_contextual_mixes_in(&db, &alice, june)
            .await
            .unwrap();
        assert!(june_mixes.iter().any(|m| m.slug == "midsummer"));

        delete_mix_definition_in(&db, &alice, "midsummer")
            .await
            .unwrap();
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
            &[&subsonic_ok(
                r#""searchResult3":{"song":[
                    {"id":"nd-own-1","title":"Roadsong","artist":"Synth Act","album":"LP","genre":"Electronic","duration":210,"userRating":5,"playCount":12},
                    {"id":"nd-own-2","title":"Highway","artist":"Indie Act","album":"LP2","genre":"Indie Rock","duration":195,"userRating":4,"playCount":8}
                ]}"#,
            )],
        );
        stub.answer(
            "/rest/createPlaylist",
            &[&subsonic_ok(
                r#""playlist":{"id":"pl-drive-1","name":"Soulbeet: drive","songCount":3,"duration":600,"owner":"alice","created":"2026-06-15T19:00:00Z","changed":"2026-06-15T19:00:00Z"}"#,
            )],
        );

        let now = Utc.with_ymd_and_hms(2026, 6, 15, 19, 0, 0).unwrap();
        let playlist = resolve_contextual_mix_playlist_in(
            &db,
            &StubNavidrome(stub.client()),
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
    }

    #[tokio::test]
    async fn seasonal_reconciliation_creates_smart_playlist_and_cleans_up_expired_ones() {
        let db = test_db::migrated().await;
        folder_of(&db, "alice").await;

        let stub = NavidromeStub::start();
        stub.answer(
            "/api/playlist",
            &[r#"{"id":"sp-winter-1","name":"Soulbeet: winter_holiday","comment":"soulbeet:contextual:winter_holiday","public":false}"#],
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

        // After expiry (Jan 10, 2027) with no pending tracks, the stale playlist is deleted
        stub.answer("/api/playlist/sp-winter-1", &[r#"{}"#]);
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

        let queue = list_triage_queue_in(&db, "alice").await.unwrap();
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
}
