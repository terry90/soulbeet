use dioxus::prelude::*;
use shared::client::{
    ClientBootstrap, CurationResult, DeleteGuardPreview, DropTrackRequest, KeepTrackRequest,
    RateTrackRequest, StarTrackRequest, TrackLyrics,
};

#[cfg(feature = "server")]
use std::sync::{Arc, LazyLock};

#[cfg(feature = "server")]
static LRCLIB_FETCHER: LazyLock<LiveExtrasFetcher> = LazyLock::new(LiveExtrasFetcher::new);

#[cfg(feature = "server")]
use shared::client::{NavidromeSessionInfo, ServerCapabilities, TrackLyricLine};
#[cfg(feature = "server")]
use shared::failure::Stage;
#[cfg(feature = "server")]
use soulbeet::navidrome::{SUBSONIC_API_VERSION, SUBSONIC_CLIENT_NAME};

#[cfg(feature = "server")]
use super::{failure_error, server_error};
#[cfg(feature = "server")]
use crate::core::curation::{
    drop_track_in, guard_preview_in, keep_track_in, rate_track_in, star_track_in,
    LiveCurationNavidrome,
};
#[cfg(feature = "server")]
use crate::core::delete_guard::{paths, rules, Guard};
#[cfg(feature = "server")]
use crate::core::discovery::DiscoveryEffects;
#[cfg(feature = "server")]
use crate::core::index::ServerNavidrome;
#[cfg(feature = "server")]
use crate::core::library::{navidrome_failure, navidrome_for};
#[cfg(feature = "server")]
use crate::core::tracks::extras::{atomic_write_file, parse_lrc, ExtrasFetcher, LiveExtrasFetcher};
#[cfg(feature = "server")]
use crate::core::tracks::is_inside_discovery;
#[cfg(feature = "server")]
use crate::folder_policy::PolicyRows;
#[cfg(feature = "server")]
use crate::import_queue::IMPORT_QUEUE;
#[cfg(feature = "server")]
use crate::models::discovery_playlist::DiscoveryTrackRow;
#[cfg(feature = "server")]
use crate::models::user_settings::UserSettings;
#[cfg(feature = "server")]
use crate::server_fns::discovery::LiveDiscovery;
#[cfg(feature = "server")]
use crate::AuthSession;

/// Returns client bootstrap coordinates: scoped OpenSubsonic credentials (salt + MD5 token,
/// never the plaintext password), curation settings, active discovery context, and capabilities.
///
/// Uses `NAVIDROME_PUBLIC_URL` when set (falling back to `NAVIDROME_URL`) so containerized
/// deployments can advertise an externally reachable Navidrome URL to native/browser clients.
#[get("/api/client/bootstrap", auth: AuthSession)]
pub async fn get_client_bootstrap() -> Result<ClientBootstrap, ServerFnError> {
    let settings = UserSettings::get(&auth.0.user_id)
        .await
        .map_err(server_error)?;

    let navidrome = match crate::services::navidrome_client_for_user(&auth.0.user_id).await {
        Ok(client) => {
            let version_check = tokio::time::timeout(
                std::time::Duration::from_secs(3),
                client.server_version(),
            )
            .await;
            match version_check {
                Ok(Err(ref e)) if e.is_wrong_credentials() => {
                    tracing::warn!(
                        "Navidrome credentials rejected during client bootstrap for user {}: {}",
                        auth.0.user_id,
                        e
                    );
                    None
                }
                other => {
                    let server_version = match other {
                        Ok(Ok(version)) => version,
                        Ok(Err(e)) => {
                            tracing::warn!(
                                "Navidrome server_version probe failed during bootstrap for user {}: {}",
                                auth.0.user_id,
                                e
                            );
                            None
                        }
                        Err(_) => {
                            tracing::warn!(
                                "Navidrome server_version probe timed out during bootstrap for user {}",
                                auth.0.user_id
                            );
                            None
                        }
                    };
                    let (subsonic_salt, subsonic_token) = client.subsonic_credentials();
                    let base_url = crate::config::CONFIG
                        .navidrome_public_url()
                        .or_else(|| crate::config::CONFIG.navidrome_url())
                        .map(|u| u.trim_end_matches('/').to_string())
                        .unwrap_or_else(|| {
                            client.base_url().as_str().trim_end_matches('/').to_string()
                        });
                    Some(NavidromeSessionInfo {
                        base_url,
                        username: client.username().to_string(),
                        subsonic_salt,
                        subsonic_token,
                        api_version: SUBSONIC_API_VERSION.to_string(),
                        client_name: SUBSONIC_CLIENT_NAME.to_string(),
                        server_version,
                    })
                }
            }
        }
        Err(_) => None,
    };

    let acquire_and_play = crate::services::is_slskd_configured().await;
    let lyrics_fetch = navidrome.is_some();

    Ok(ClientBootstrap {
        user_id: auth.0.user_id.clone(),
        username: auth.0.username.clone(),
        is_admin: auth.0.role.is_admin(),
        navidrome,
        capabilities: ServerCapabilities {
            acquire_and_play,
            connect_ws: true,
            lyrics_fetch,
            discovery: settings.discovery_enabled,
            multi_listener_guard: true,
        },
        promote_threshold: settings.discovery_promote_threshold,
        auto_delete_enabled: settings.auto_delete_enabled,
        discovery_enabled: settings.discovery_enabled,
        active_culture_profile: None,
        active_seasonal_packs: Vec::new(),
    })
}

/// Fetches structured (synced) lyrics for `song_id` via OpenSubsonic `getLyricsBySongId`
/// (with legacy `getLyrics` fallback), and falls back to LRCLIB when Navidrome returns no lyrics.
/// When LRCLIB returns lyrics and the audio file resolves inside one of the caller's library
/// folders without an existing `.lrc` sidecar, saves the sidecar best-effort so Navidrome serves
/// it natively on future plays.
#[get("/api/client/lyrics?song_id&artist&title", auth: AuthSession)]
pub async fn get_track_lyrics(
    song_id: String,
    artist: Option<String>,
    title: Option<String>,
) -> Result<Vec<TrackLyrics>, ServerFnError> {
    let navi = navidrome_for(&ServerNavidrome, &auth.0, Stage::LibraryCheck)
        .await
        .map_err(failure_error)?;
    let lyrics = navi
        .get_song_lyrics(&song_id, artist.as_deref(), title.as_deref())
        .await
        .map_err(|e| failure_error(navidrome_failure(Stage::LibraryCheck, &e)))?;

    let mapped: Vec<TrackLyrics> = lyrics
        .into_iter()
        .map(|entry| TrackLyrics {
            display_artist: entry.display_artist,
            display_title: entry.display_title,
            lang: entry.lang,
            synced: entry.synced,
            offset_ms: entry.offset,
            lines: entry
                .line
                .into_iter()
                .map(|l| TrackLyricLine {
                    start_ms: l.start,
                    text: l.value,
                })
                .collect(),
        })
        .filter(|entry| !entry.lines.is_empty())
        .collect();

    if !mapped.is_empty() {
        return Ok(mapped);
    }

    // Fallback to LRCLIB using native Navidrome track metadata (or query params).
    let native_song = navi.get_native_song(&song_id).await.ok().flatten();
    let eff_artist = native_song
        .as_ref()
        .map(|s| s.artist.trim().to_string())
        .filter(|s| !s.is_empty())
        .or_else(|| artist.map(|s| s.trim().to_string()).filter(|s| !s.is_empty()));
    let eff_title = native_song
        .as_ref()
        .map(|s| s.title.trim().to_string())
        .filter(|s| !s.is_empty())
        .or_else(|| title.map(|s| s.trim().to_string()).filter(|s| !s.is_empty()));
    let eff_album = native_song
        .as_ref()
        .map(|s| s.album.trim().to_string())
        .filter(|s| !s.is_empty());
    let eff_duration_s = native_song
        .as_ref()
        .filter(|s| s.duration > 0.0)
        .map(|s| s.duration.round() as u32);

    let (Some(eff_artist), Some(eff_title)) = (eff_artist, eff_title) else {
        return Ok(vec![]);
    };

    let fetched = match LRCLIB_FETCHER
        .fetch_lyrics(
            &eff_artist,
            &eff_title,
            eff_album.as_deref(),
            eff_duration_s,
        )
        .await
    {
        Ok(Some(f)) => f,
        Ok(None) => return Ok(vec![]),
        Err(e) => {
            tracing::warn!(
                "LRCLIB fallback failed for song {} ({} - {}): {}",
                song_id,
                eff_artist,
                eff_title,
                e
            );
            return Ok(vec![]);
        }
    };

    let parsed = parse_lrc(
        &fetched.content,
        fetched.synced,
        Some(eff_artist),
        Some(eff_title),
    );
    if parsed.lines.is_empty() {
        return Ok(vec![]);
    }

    // Best-effort: cache the .lrc sidecar next to the audio file when safely inside the user's
    // own library folder and not already present. Never fail the HTTP response.
    if let Some(native) = native_song {
        cache_lrc_sidecar_best_effort(&auth.0, &native, fetched.content).await;
    }

    Ok(vec![parsed])
}

#[cfg(feature = "server")]
async fn cache_lrc_sidecar_best_effort(
    ctx: &crate::auth::UserCtx,
    native: &soulbeet::navidrome::models::NativeSong,
    content: String,
) {
    let user = ctx.id();
    let Ok((audio_path, _)) = paths::local_file_learning(
        &crate::db::DB,
        native,
        crate::config::CONFIG.navidrome_music_path(),
        &user,
    )
    .await
    else {
        return;
    };
    if rules::audio_file(&audio_path).is_err() {
        return;
    }
    let Ok(rows) = PolicyRows::load_in(&crate::db::DB).await else {
        return;
    };
    let (policy_folders, _) = rows.inputs();
    if rules::owning_folders(&audio_path, &policy_folders, &user).is_err() {
        return;
    }
    if is_inside_discovery(&audio_path, &policy_folders) {
        return;
    }
    let pending = DiscoveryTrackRow::pending_for_user_in(&crate::db::DB, &ctx.user_id)
        .await
        .unwrap_or_default();
    for track in &pending {
        if track.song_id.as_deref() == Some(native.id.as_str()) {
            return;
        }
        if tokio::fs::canonicalize(&track.path)
            .await
            .is_ok_and(|p| p == audio_path)
        {
            return;
        }
    }
    let lrc_path = audio_path.with_extension("lrc");
    // Use symlink_metadata (does not follow symlinks) so even a dangling symlink is never
    // overwritten or followed.
    if tokio::fs::symlink_metadata(&lrc_path).await.is_ok() {
        return;
    }
    let write_path = lrc_path.clone();
    let bytes = content.into_bytes();
    match tokio::task::spawn_blocking(move || atomic_write_file(&write_path, &bytes)).await {
        Ok(Ok(())) => {
            tracing::info!("Cached fetched LRCLIB sidecar at {}", lrc_path.display());
        }
        Ok(Err(e)) => {
            tracing::debug!(
                "Skipped caching LRCLIB sidecar at {}: {}",
                lrc_path.display(),
                e
            );
        }
        Err(e) => {
            tracing::debug!("LRCLIB sidecar write task failed: {}", e);
        }
    }
}

/// Promotes a Pending Discovery track to the permanent library (raising rating >= promote
/// threshold), or affirms a permanent library track.
///
/// Note: `/api/curation/*` endpoints are Dioxus `#[post]` server functions taking a single `req`
/// parameter, so external HTTP clients must send JSON wrapped as `{"req": { ... }}`.
#[post("/api/curation/keep", auth: AuthSession)]
pub async fn curation_keep(req: KeepTrackRequest) -> Result<CurationResult, ServerFnError> {
    let guard = Guard::live();
    let effects: Arc<dyn DiscoveryEffects> = Arc::new(LiveDiscovery);
    keep_track_in(&guard, &LiveCurationNavidrome, &effects, &IMPORT_QUEUE, &auth.0, &req)
        .await
        .map_err(failure_error)
}

/// Stars or unstars a track. Starring a Pending Discovery track automatically promotes it into
/// the permanent library.
#[post("/api/curation/star", auth: AuthSession)]
pub async fn curation_star(req: StarTrackRequest) -> Result<CurationResult, ServerFnError> {
    let guard = Guard::live();
    let effects: Arc<dyn DiscoveryEffects> = Arc::new(LiveDiscovery);
    star_track_in(&guard, &LiveCurationNavidrome, &effects, &IMPORT_QUEUE, &auth.0, &req)
        .await
        .map_err(failure_error)
}

/// Sets a 0–5★ rating on a track, promoting Pending Discovery tracks at or above the promote
/// threshold and removing them at 1★.
#[post("/api/curation/rate", auth: AuthSession)]
pub async fn curation_rate(req: RateTrackRequest) -> Result<CurationResult, ServerFnError> {
    let guard = Guard::live();
    let effects: Arc<dyn DiscoveryEffects> = Arc::new(LiveDiscovery);
    rate_track_in(&guard, &LiveCurationNavidrome, &effects, &IMPORT_QUEUE, &auth.0, &req)
        .await
        .map_err(failure_error)
}

/// Drops a Pending Discovery track or removes/protects a Permanent Library track through
/// Soulbeet's Multi-Listener Delete Guard.
#[post("/api/curation/drop", auth: AuthSession)]
pub async fn curation_drop(req: DropTrackRequest) -> Result<CurationResult, ServerFnError> {
    let guard = Guard::live();
    drop_track_in(&guard, &LiveCurationNavidrome, &IMPORT_QUEUE, &auth.0, &req)
        .await
        .map_err(failure_error)
}

/// Previews Soulbeet's Multi-Listener Delete Guard verdict for `song_id` without mutating state.
#[get("/api/curation/guard-preview/{song_id}", auth: AuthSession)]
pub async fn curation_guard_preview(song_id: String) -> Result<DeleteGuardPreview, ServerFnError> {
    let guard = Guard::live();
    guard_preview_in(&guard, &auth.0, &song_id)
        .await
        .map_err(failure_error)
}
