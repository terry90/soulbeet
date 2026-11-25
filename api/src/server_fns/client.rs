use dioxus::prelude::*;
use shared::client::{
    ClientBootstrap, CurationResult, DeleteGuardPreview, DropTrackRequest, KeepTrackRequest,
    RateTrackRequest, StarTrackRequest, TrackLyrics,
};

#[cfg(feature = "server")]
use std::sync::Arc;

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
use crate::core::delete_guard::Guard;
#[cfg(feature = "server")]
use crate::core::discovery::DiscoveryEffects;
#[cfg(feature = "server")]
use crate::core::index::ServerNavidrome;
#[cfg(feature = "server")]
use crate::core::library::{navidrome_failure, navidrome_for};
#[cfg(feature = "server")]
use crate::import_queue::IMPORT_QUEUE;
#[cfg(feature = "server")]
use crate::models::user_settings::UserSettings;
#[cfg(feature = "server")]
use crate::server_fns::discovery::LiveDiscovery;
#[cfg(feature = "server")]
use crate::AuthSession;

/// Returns client bootstrap coordinates: scoped OpenSubsonic credentials (salt + MD5 token,
/// never the plaintext password), curation settings, active discovery context, and capabilities.
#[get("/api/client/bootstrap", auth: AuthSession)]
pub async fn get_client_bootstrap() -> Result<ClientBootstrap, ServerFnError> {
    let settings = UserSettings::get(&auth.0.user_id)
        .await
        .map_err(server_error)?;

    let navidrome = match crate::services::navidrome_client_for_user(&auth.0.user_id).await {
        Ok(client) => {
            let (subsonic_salt, subsonic_token) = client.subsonic_credentials();
            let server_version = client.server_version().await.ok().flatten();
            let base_url = crate::config::CONFIG
                .navidrome_url()
                .map(|u| u.trim_end_matches('/').to_string())
                .unwrap_or_else(|| client.base_url().as_str().trim_end_matches('/').to_string());
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
        Err(_) => None,
    };

    let acquire_and_play = crate::services::is_slskd_configured().await;

    Ok(ClientBootstrap {
        user_id: auth.0.user_id.clone(),
        username: auth.0.username.clone(),
        is_admin: auth.0.role.is_admin(),
        navidrome,
        capabilities: ServerCapabilities {
            acquire_and_play,
            connect_ws: true,
            lyrics_fetch: true,
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

/// Fetches structured (synced) lyrics for `song_id` via OpenSubsonic `getLyricsBySongId`,
/// falling back gracefully to legacy `getLyrics` when `artist` and `title` are provided.
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

    Ok(lyrics
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
        .collect())
}

/// Promotes a Pending Discovery track to the permanent library (raising rating >= promote
/// threshold), or affirms a permanent library track.
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
