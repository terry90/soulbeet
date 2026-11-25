//! Discovery 2.0 core logic: UserCultureProfile persistence, active Seasonal & Moment Mix
//! summaries, live seasonal steering, Discovery Triage queue, and On-Demand Seed Radio.

use std::collections::HashSet;

use chrono::{Datelike, Timelike, Utc};
use shared::failure::{Failure, FailureCode, RetryAdvice, Stage};
use shared::navidrome::DiscoveryTrack;
use shared::recommendation::{
    Candidate, CandidateSet, ContextualMixKind, ContextualMixSummary, CultureBucket,
    CultureSteering, DiscoveryProfile, MomentKind, SeasonalKind, SeasonalPackSpec,
    SeedRadioRequest, SeedRadioResponse, SeedRadioTrack, UpdateCultureProfileRequest,
    UserCultureProfile,
};
use soulbeet::navidrome::models::LibraryKind;
use soulbeet::MusicKnowledge;
use sqlx::SqlitePool;
use uuid::Uuid;

use crate::auth::UserCtx;
use crate::core::database_failure;
use crate::core::discovery::{expires_at, parse_profiles};
use crate::core::index::NavidromeOf;
use crate::core::library::{navidrome_failure, navidrome_for};
use crate::models::contextual::{SeasonalPackRow, UserCultureProfileRow};
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

/// Lists active contextual mixes for `ctx`:
/// 1. Active calendar Seasonal Pack (if date falls inside a seasonal window)
/// 2. Drive Mix + active time-of-day Moment Mix
/// 3. User's enabled Rolling Discovery profiles (`Comfort Mix`, `Weekly Radar`, `Deep Cuts`)
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

    let mut mixes = Vec::new();

    // 1. Active calendar Seasonal Pack
    if let Some(kind) = SeasonalKind::active_for(now.month(), now.day()) {
        let stored = SeasonalPackRow::get_by_slug_in(db, &ctx.user_id, kind.slug())
            .await
            .map_err(|e| database_failure(Stage::Input, e))?;
        let steering = stored
            .as_ref()
            .map(|r| CultureSteering::parse(&r.steering))
            .unwrap_or(CultureSteering::Balanced);
        let spec = SeasonalPackSpec::for_kind(kind, &culture, steering);
        let count = pending
            .iter()
            .filter(|t| t.pack_slug.as_deref() == Some(kind.slug()))
            .count() as u32;
        let local_pct = (spec.local_ratio * 100.0).round() as u32;
        let country_tag = culture.effective_country().unwrap_or("LOCAL");
        mixes.push(ContextualMixSummary {
            slug: spec.slug,
            kind: ContextualMixKind::Seasonal,
            title: spec.title,
            subtitle: spec.subtitle,
            badge: format!("{local_pct}% {country_tag} · {}% INTL", 100u32.saturating_sub(local_pct)),
            exclude_from_core_taste: true,
            steering: Some(spec.steering),
            local_ratio: Some(spec.local_ratio),
            expires_label: Some(spec.expires_label),
            track_count: count,
        });
    }

    // 2. Drive Mix (always surfaced for Android Auto / mobile quick launch) + time-of-day Moment Mix
    let active_moment = MomentKind::active_for_hour(now.hour());
    let mut moments = vec![MomentKind::Drive];
    if active_moment != MomentKind::Drive {
        moments.push(active_moment);
    }
    for moment in moments {
        mixes.push(ContextualMixSummary {
            slug: moment.slug().to_string(),
            kind: ContextualMixKind::Moment,
            title: moment.title().to_string(),
            subtitle: moment.subtitle().to_string(),
            badge: if moment == MomentKind::Drive {
                "OFFLINE FIRST · 75% LIBRARY".to_string()
            } else {
                "75% LIBRARY · 25% FRESH".to_string()
            },
            exclude_from_core_taste: false,
            steering: None,
            local_ratio: None,
            expires_label: None,
            track_count: 25,
        });
    }

    // 3. Rolling Personal Mixes
    let counts = settings.parse_track_counts();
    for profile in parse_profiles(&settings.discovery_profiles) {
        let profile_name = profile.name();
        let display = match profile {
            DiscoveryProfile::Conservative => "Comfort Mix",
            DiscoveryProfile::Balanced => "Weekly Radar",
            DiscoveryProfile::Adventurous => "Deep Cuts",
        };
        let subtitle = match profile {
            DiscoveryProfile::Conservative => "Familiar orbit, adjacent artists, high hit rate",
            DiscoveryProfile::Balanced => "Flagship weekly blend across your favorite signals",
            DiscoveryProfile::Adventurous => "Obscure gems and cross-genre leaps",
        };
        let pending_count = pending
            .iter()
            .filter(|t| t.profile == profile_name && t.pack_slug.is_none())
            .count() as u32;
        let target = counts.get(profile_name).copied().unwrap_or(10);
        mixes.push(ContextualMixSummary {
            slug: profile_name.to_ascii_lowercase(),
            kind: ContextualMixKind::Rolling,
            title: display.to_string(),
            subtitle: subtitle.to_string(),
            badge: format!("{} / {} READY", pending_count, target),
            exclude_from_core_taste: false,
            steering: None,
            local_ratio: None,
            expires_label: None,
            track_count: pending_count,
        });
    }

    Ok(mixes)
}

/// Updates the user's live culture steering (`MoreLocal`, `Balanced`, `MoreInternational`) for
/// seasonal pack `slug` and returns the updated `SeasonalPackSpec`.
pub(crate) async fn steer_seasonal_pack_in(
    db: &SqlitePool,
    user_id: &str,
    slug: &str,
    steering: CultureSteering,
) -> Result<SeasonalPackSpec, Failure> {
    let kind = SeasonalKind::from_slug(slug).ok_or_else(|| {
        Failure::new(
            FailureCode::InvalidInput,
            Stage::Input,
            RetryAdvice::NotRetryable,
            format!("Unknown seasonal pack '{slug}'"),
        )
    })?;
    let culture = get_culture_profile_in(db, user_id).await?;
    let spec = SeasonalPackSpec::for_kind(kind, &culture, steering);
    let stored = SeasonalPackRow::upsert_steering_in(db, user_id, &spec)
        .await
        .map_err(|e| database_failure(Stage::Input, e))?;
    Ok(stored.into_spec(kind))
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

    // Resolve seed artist & title from song_id if supplied.
    let (mut seed_artist, mut seed_track, mut seed_mbid) = (
        req.seed_artist.clone().filter(|s| !s.trim().is_empty()),
        req.seed_track.clone().filter(|s| !s.trim().is_empty()),
        req.seed_artist_mbid.clone().filter(|s| !s.trim().is_empty()),
    );

    let mut owned_tracks: Vec<SeedRadioTrack> = Vec::new();
    let mut owned_keys: HashSet<String> = HashSet::new();

    if let Some(ref song_id) = req.seed_song_id.as_deref().filter(|s| !s.trim().is_empty()) {
        if let Some(song) = navi
            .get_song(song_id)
            .await
            .map_err(|e| navidrome_failure(Stage::LibraryCheck, &e))?
        {
            let artist_name = song.artist.clone().unwrap_or_default();
            if seed_artist.is_none() && !artist_name.is_empty() {
                seed_artist = Some(artist_name.clone());
            }
            if seed_track.is_none() {
                seed_track = Some(song.title.clone());
            }
            owned_keys.insert(CandidateSet::key(&artist_name, &song.title));
            owned_tracks.push(SeedRadioTrack {
                song_id: song.id,
                title: song.title,
                artist: artist_name,
                album: song.album,
                duration: song.duration,
                reason_label: "RADIO SEED".to_string(),
            });
        }
    }

    let seed_artist = seed_artist.ok_or_else(|| {
        Failure::new(
            FailureCode::InvalidInput,
            Stage::Input,
            RetryAdvice::NotRetryable,
            "Provide seed_song_id or seed_artist to start radio",
        )
    })?;

    let seed_label = match &seed_track {
        Some(t) => format!("{seed_artist} — {t} Radio"),
        None => format!("{seed_artist} Radio"),
    };

    // Pull owned matches for the seed artist immediately.
    if let Ok(found) = navi.search3(&seed_artist, LibraryKind::Songs, 0, 12).await {
        for song in found.song {
            let artist_name = song.artist.clone().unwrap_or_default();
            let key = CandidateSet::key(&artist_name, &song.title);
            if owned_keys.insert(key) {
                owned_tracks.push(SeedRadioTrack {
                    song_id: song.id,
                    title: song.title,
                    artist: artist_name,
                    album: song.album,
                    duration: song.duration,
                    reason_label: format!("MORE FROM {}", seed_artist.to_ascii_uppercase()),
                });
            }
        }
    }

    // Resolve artist MBID and fetch similar artists + top tracks.
    if seed_mbid.is_none() {
        seed_mbid = knowledge.resolve_artist(&seed_artist).await.ok().flatten();
    }

    let mut prefetch_candidates: Vec<Candidate> = Vec::new();
    let mut seen_prefetch: HashSet<String> = HashSet::new();

    if let Some(ref mbid) = seed_mbid {
        if let Ok(neighbours) = knowledge.similar_artists(&seed_artist, mbid, 8).await {
            for neighbour in neighbours {
                // Check if user already owns songs by this similar artist -> play immediately!
                if let Ok(found) = navi
                    .search3(&neighbour.artist.name, LibraryKind::Songs, 0, 3)
                    .await
                {
                    for song in found.song {
                        let artist_name = song.artist.clone().unwrap_or_default();
                        let key = CandidateSet::key(&artist_name, &song.title);
                        if owned_keys.insert(key) && owned_tracks.len() < limit {
                            owned_tracks.push(SeedRadioTrack {
                                song_id: song.id,
                                title: song.title,
                                artist: artist_name,
                                album: song.album,
                                duration: song.duration,
                                reason_label: format!(
                                    "SIMILAR TO {}",
                                    seed_artist.to_ascii_uppercase()
                                ),
                            });
                        }
                    }
                }

                // Queue top unowned tracks for background prefetch.
                if let Some(ref n_mbid) = neighbour.artist.mbid {
                    if let Ok(top) = knowledge.artist_top_tracks(n_mbid, 3).await {
                        for tr in top {
                            let key = CandidateSet::key(&tr.artist.name, &tr.title);
                            if owned_keys.contains(&key) || !seen_prefetch.insert(key) {
                                continue;
                            }
                            prefetch_candidates.push(Candidate {
                                artist: tr.artist.name,
                                track: tr.title,
                                album: None,
                                mbid: tr.mbid,
                                score: neighbour.score,
                                signals: vec!["seed_radio_similar".to_string()],
                                source: "seed_radio".to_string(),
                                artist_listener_count: None,
                                primary_genre: None,
                                release_year: None,
                                artist_mbid: tr.artist.mbid,
                                reason_label: Some(format!(
                                    "SIMILAR TO {}",
                                    seed_artist.to_ascii_uppercase()
                                )),
                                culture_bucket: Some(CultureBucket::TasteWildcard),
                            });
                            if prefetch_candidates.len() >= 10 {
                                break;
                            }
                        }
                    }
                }
                if prefetch_candidates.len() >= 10 {
                    break;
                }
            }
        }
    }

    owned_tracks.truncate(limit);

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
    use crate::core::{folder_of, test_ctx};
    use crate::db::test_db;

    #[tokio::test]
    async fn culture_profile_and_seasonal_steering_persist_per_user() {
        let db = test_db::migrated().await;
        folder_of(&db, "alice").await;

        let default = get_culture_profile_in(&db, "alice").await.unwrap();
        assert_eq!(default.home_country, None);
        assert!((default.local_culture_ratio - 0.45).abs() < 0.001);

        let updated = update_culture_profile_in(
            &db,
            "alice",
            UpdateCultureProfileRequest {
                home_country: Some("fr".into()),
                preferred_languages: Some(vec!["FR".into(), "EN".into()]),
                local_culture_ratio: Some(0.50),
            },
        )
        .await
        .unwrap();
        assert_eq!(updated.home_country.as_deref(), Some("FR"));
        assert_eq!(updated.preferred_languages, vec!["fr", "en"]);
        assert!((updated.local_culture_ratio - 0.50).abs() < 0.001);

        // Steer Noël to MoreLocal -> 0.50 + 0.25 = 0.75
        let steered =
            steer_seasonal_pack_in(&db, "alice", "noel", CultureSteering::MoreLocal)
                .await
                .unwrap();
        assert_eq!(steered.title, "Noël à la Française");
        assert_eq!(steered.steering, CultureSteering::MoreLocal);
        assert!((steered.local_ratio - 0.75).abs() < 0.001);
        assert!(steered.exclude_from_core_taste);

        // Active mixes in December surface Noël à la Française with persisted MoreLocal steering
        let dec = Utc.with_ymd_and_hms(2026, 12, 10, 14, 0, 0).unwrap();
        let mixes = list_active_contextual_mixes_in(&db, &test_ctx("alice"), dec)
            .await
            .unwrap();
        assert_eq!(mixes[0].kind, ContextualMixKind::Seasonal);
        assert_eq!(mixes[0].slug, "noel");
        assert_eq!(mixes[0].title, "Noël à la Française");
        assert_eq!(mixes[0].steering, Some(CultureSteering::MoreLocal));
        assert!(mixes[0].exclude_from_core_taste);
        assert!(mixes.iter().any(|m| m.slug == "drive"));
        assert!(mixes.iter().any(|m| m.slug == "focus"));
    }

    #[tokio::test]
    async fn triage_queue_lists_pending_tracks_with_reason_and_anti_pollution_flags() {
        let db = test_db::migrated().await;
        let folder_id = folder_of(&db, "alice").await;
        sqlx::query(
            "INSERT INTO discovery_tracks \
                (id, song_id, title, artist, album, path, folder_id, profile, status, \
                 exclude_from_core_taste, reason_label, culture_bucket, pack_slug) \
             VALUES ('dt-noel', 's-noel', 'Petit Papa Noël', 'Tino Rossi', 'Noël', \
                     '/music/alice/Discovery/Seasonal/noel/01.flac', ?, 'Balanced', 'Pending', \
                     1, 'FRENCH NOËL PICK', 'local', 'noel')",
        )
        .bind(&folder_id)
        .execute(&db)
        .await
        .unwrap();

        let queue = list_triage_queue_in(&db, "alice").await.unwrap();
        assert_eq!(queue.len(), 1);
        assert!(queue[0].exclude_from_core_taste);
        assert_eq!(queue[0].reason_label.as_deref(), Some("FRENCH NOËL PICK"));
        assert_eq!(queue[0].culture_bucket.as_deref(), Some("local"));
        assert_eq!(queue[0].pack_slug.as_deref(), Some("noel"));
    }
}
