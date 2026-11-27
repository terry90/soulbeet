use dioxus::prelude::*;
use shared::navidrome::{DiscoveryConfig, DiscoveryTrack};

#[cfg(feature = "server")]
use dioxus::logger::tracing::{debug, info, warn};

#[cfg(feature = "server")]
use std::collections::HashMap;
#[cfg(feature = "server")]
use std::sync::{Arc, LazyLock};
#[cfg(feature = "server")]
use tokio::sync::Mutex;

#[cfg(feature = "server")]
static GENERATION_LOCKS: LazyLock<Mutex<HashMap<String, Arc<Mutex<()>>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

#[cfg(feature = "server")]
async fn update_progress(
    user_id: &str,
    profile: &str,
    phase: shared::navidrome::ProfilePhase,
    current: u32,
    total: u32,
) {
    let mut map = crate::globals::DISCOVERY_PROGRESS.write().await;
    if let Some(progress) = map.get_mut(user_id) {
        if let Some(pp) = progress.profiles.iter_mut().find(|p| p.profile == profile) {
            pp.phase = phase;
            pp.current = current;
            pp.total = total;
        }
    }
}

#[cfg(feature = "server")]
fn seasonal_progress_label(def: &shared::recommendation::ContextualMixDefinition) -> String {
    format!(
        "Seasonal: {}",
        shared::recommendation::canonical_mix_title(
            &def.slug,
            Some(&def.i18n_key),
            def.custom_name.as_deref(),
        )
    )
}

#[cfg(feature = "server")]
async fn upsert_progress(
    user_id: &str,
    profile: &str,
    phase: shared::navidrome::ProfilePhase,
    current: u32,
    total: u32,
) {
    let mut map = crate::globals::DISCOVERY_PROGRESS.write().await;
    if let Some(progress) = map.get_mut(user_id) {
        if let Some(pp) = progress.profiles.iter_mut().find(|p| p.profile == profile) {
            pp.phase = phase;
            pp.current = current;
            pp.total = total;
        } else {
            progress.profiles.push(shared::navidrome::ProfileProgress {
                profile: profile.to_string(),
                phase,
                current,
                total,
            });
        }
    }
}

#[cfg(feature = "server")]
type SeasonalWorkItem = (
    shared::recommendation::ContextualMixDefinition,
    Option<crate::models::contextual::ContextualMixStateRow>,
    usize,
);

#[cfg(feature = "server")]
async fn register_seasonal_progress_profiles(user_id: &str, work: &[SeasonalWorkItem]) {
    if work.is_empty() {
        return;
    }
    let mut map = crate::globals::DISCOVERY_PROGRESS.write().await;
    let Some(progress) = map.get_mut(user_id) else {
        return;
    };
    for (def, _, needed) in work {
        let label = seasonal_progress_label(def);
        if !progress.profiles.iter().any(|p| p.profile == label) {
            progress.profiles.push(shared::navidrome::ProfileProgress {
                profile: label,
                phase: shared::navidrome::ProfilePhase::Waiting,
                current: 0,
                total: *needed as u32,
            });
        }
    }
}

#[cfg(feature = "server")]
async fn collect_seasonal_work(
    user_id: &str,
    folder_id: &str,
    now: chrono::DateTime<chrono::Utc>,
) -> Result<(shared::recommendation::UserCultureProfile, Vec<SeasonalWorkItem>), Failure> {
    let (stored_culture, active_seasonal) =
        crate::core::contextual::list_calendar_active_seasonal_defs_in(
            &crate::db::DB,
            user_id,
            now,
        )
        .await
        .map_err(|e| run_database_failure(Stage::Input, e))?;

    let mut work = Vec::new();
    for (def, state) in active_seasonal {
        let existing = DiscoveryTrackRow::get_pending_by_folder_and_pack_in(
            &crate::db::DB,
            folder_id,
            &def.slug,
        )
        .await
        .map(|v| v.len())
        .unwrap_or(0);
        let needed = (def.target_tracks as usize).saturating_sub(existing);
        if needed > 0 {
            work.push((def, state, needed));
        }
    }
    Ok((stored_culture, work))
}

#[cfg(feature = "server")]
use crate::auth::UserId;
#[cfg(feature = "server")]
use crate::import_queue::{Duplicates, ImportJob, IMPORT_QUEUE};
#[cfg(feature = "server")]
use crate::models::discovery_playlist::DiscoveryTrackRow;
#[cfg(feature = "server")]
use crate::models::folder::Folder;
#[cfg(feature = "server")]
use crate::models::user_settings::UserSettings;
#[cfg(feature = "server")]
use crate::server_fns::download::enqueue::{
    enqueue_claimed, held_failure, not_enqueued_failure, transfer_key,
};
#[cfg(feature = "server")]
use crate::server_fns::download::failover::{self, SourcePool};
#[cfg(feature = "server")]
use crate::server_fns::download::monitor::{
    await_ended, cancel_abandoned, read_pending_retry, remove_ended_transfers, transfer_by_id,
    FAILED_STATE_CONFIRM,
};
#[cfg(feature = "server")]
use crate::transfer_registry::{BatchRef, TransferKey, TransferOwner, TransferRegistry};
#[cfg(feature = "server")]
use crate::AuthSession;

#[cfg(feature = "server")]
use super::{failure_error, server_error};
#[cfg(feature = "server")]
use crate::core::discovery::{
    choose_discovery_folder, claim_run, current_progress, discovery_disabled,
    discovery_folder_required, generation_running, parse_profiles, promote, remove_in, start_in,
    DiscoveryEffects, Replace,
};
#[cfg(feature = "server")]
use crate::core::{database_failure, target_unwritable};
#[cfg(feature = "server")]
use shared::failure::{Failure, FailureCode, RetryAdvice, Stage};
#[cfg(feature = "server")]
use shared::navidrome::DiscoveryProgress;

/// The discovery folder the user's settings name no longer exists.
#[cfg(feature = "server")]
fn discovery_folder_not_found() -> Failure {
    Failure::new(
        FailureCode::FolderRequired,
        Stage::Input,
        RetryAdvice::NeedsUser,
        "Discovery folder not found",
    )
    .with_user_action(choose_discovery_folder())
}

/// A database error of a generation run, shown as the database's own text.
#[cfg(feature = "server")]
fn run_database_failure(stage: Stage, e: String) -> Failure {
    database_failure(stage, &e).with_message(e)
}

/// Get the current user's discovery configuration
#[get("/api/discovery/config", auth: AuthSession)]
pub async fn get_discovery_config() -> Result<DiscoveryConfig, ServerFnError> {
    let settings = UserSettings::get(&auth.0.user_id)
        .await
        .map_err(server_error)?;
    let folder_name = if let Some(ref fid) = settings.discovery_folder_id {
        Folder::get_by_id(fid).await.ok().flatten().map(|f| f.name)
    } else {
        None
    };
    let track_counts = settings.parse_track_counts();
    let lifetime_days = settings.parse_lifetime_days();
    let playlist_names =
        serde_json::from_str(&settings.discovery_playlist_name).unwrap_or_default();
    let latest = crate::models::engine_report::EngineReportRow::latest_per_profile_in(
        &crate::db::DB,
        &auth.0.user_id,
    )
    .await
    .map_err(server_error)?;
    Ok(DiscoveryConfig {
        enabled: settings.discovery_enabled,
        folder_id: settings.discovery_folder_id,
        folder_name,
        track_counts,
        lifetime_days,
        profiles: settings.discovery_profiles,
        playlist_names,
        last_generated_at: settings.discovery_last_generated_at,
        last_run: crate::core::recommend::last_run_of(latest),
    })
}

/// Get discovery tracks for the current user's configured folder
#[get("/api/discovery/tracks", auth: AuthSession)]
pub async fn get_discovery_tracks() -> Result<Vec<DiscoveryTrack>, ServerFnError> {
    let settings = UserSettings::get(&auth.0.user_id)
        .await
        .map_err(server_error)?;
    if let Some(ref folder_id) = settings.discovery_folder_id {
        let mut tracks = DiscoveryTrackRow::get_by_folder(folder_id)
            .await
            .map_err(server_error)?;
        crate::core::contextual::reconcile_and_hydrate_discovery_tracks_in(
            &crate::db::DB,
            &crate::core::index::ServerNavidrome,
            crate::config::CONFIG.navidrome_music_path(),
            &auth.0.user_id,
            &mut tracks,
        )
        .await;
        Ok(tracks)
    } else {
        Ok(vec![])
    }
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct TrackActionRequest {
    pub track_id: String,
}

#[post("/api/discovery/promote", auth: AuthSession)]
pub async fn promote_discovery_track(req: TrackActionRequest) -> Result<(), ServerFnError> {
    let effects: Arc<dyn DiscoveryEffects> = Arc::new(LiveDiscovery);
    let replace = Replace::Guarded(crate::core::delete_guard::Origin::Ui);
    promote(
        &crate::db::DB,
        &effects,
        &IMPORT_QUEUE,
        &auth.0.id(),
        &req.track_id,
        replace,
    )
    .await
    .map(|_| ())
    .map_err(|n| failure_error(n.into_failure()))
}

#[post("/api/discovery/remove", auth: AuthSession)]
pub async fn remove_discovery_track(req: TrackActionRequest) -> Result<(), ServerFnError> {
    remove_in(&crate::db::DB, &IMPORT_QUEUE, &auth.0.id(), &req.track_id)
        .await
        .map(|_| ())
        .map_err(|n| failure_error(n.into_failure()))
}

#[post("/api/discovery/start-generation", auth: AuthSession)]
pub async fn start_discovery_generation() -> Result<(), ServerFnError> {
    start_in(&crate::db::DB, &LiveDiscovery, &auth.0)
        .await
        .map(|_| ())
        .map_err(failure_error)
}

#[get("/api/discovery/progress", auth: AuthSession)]
pub async fn get_discovery_progress(
) -> Result<Option<shared::navidrome::DiscoveryProgress>, ServerFnError> {
    let progress = LiveDiscovery.current(&auth.0.id()).await;
    Ok(progress.and_then(|p| current_progress(&p, chrono::Utc::now())))
}

#[get("/api/discovery/culture-profile", auth: AuthSession)]
pub async fn get_culture_profile(
) -> Result<shared::recommendation::UserCultureProfile, ServerFnError> {
    crate::core::contextual::get_culture_profile_in(&crate::db::DB, &auth.0.user_id)
        .await
        .map_err(failure_error)
}

#[post("/api/discovery/culture-profile", auth: AuthSession)]
pub async fn update_culture_profile(
    req: shared::recommendation::UpdateCultureProfileRequest,
) -> Result<shared::recommendation::UserCultureProfile, ServerFnError> {
    crate::core::contextual::update_culture_profile_in(&crate::db::DB, &auth.0.user_id, req)
        .await
        .map_err(failure_error)
}

#[get("/api/discovery/contextual-mixes?local_hour", auth: AuthSession)]
pub async fn list_active_contextual_mixes(
    local_hour: Option<u8>,
) -> Result<Vec<shared::recommendation::ContextualMixSummary>, ServerFnError> {
    crate::core::contextual::list_active_contextual_mixes_in(
        &crate::db::DB,
        &crate::core::index::ServerNavidrome,
        crate::config::CONFIG.navidrome_music_path(),
        &auth.0,
        chrono::Utc::now(),
        local_hour,
    )
    .await
    .map_err(failure_error)
}

#[get("/api/discovery/mix-definitions", auth: AuthSession)]
pub async fn list_contextual_mix_definitions(
) -> Result<Vec<shared::recommendation::ContextualMixDefinition>, ServerFnError> {
    crate::core::contextual::list_mix_definitions_in(&crate::db::DB, &auth.0)
        .await
        .map_err(failure_error)
}

#[post("/api/discovery/mix-definitions", auth: AuthSession)]
pub async fn upsert_contextual_mix_definition(
    req: shared::recommendation::UpsertContextualMixRequest,
) -> Result<shared::recommendation::ContextualMixDefinition, ServerFnError> {
    crate::core::contextual::upsert_mix_definition_in(&crate::db::DB, &auth.0, req)
        .await
        .map_err(failure_error)
}

#[post("/api/discovery/mix-definitions/delete", auth: AuthSession)]
pub async fn delete_contextual_mix_definition(slug: String) -> Result<(), ServerFnError> {
    crate::core::contextual::delete_mix_definition_in(&crate::db::DB, &auth.0, &slug)
        .await
        .map_err(failure_error)
}

#[post("/api/discovery/mix-definitions/enabled", auth: AuthSession)]
pub async fn set_contextual_mix_enabled(
    req: shared::recommendation::SetMixEnabledRequest,
) -> Result<shared::recommendation::ContextualMixDefinition, ServerFnError> {
    crate::core::contextual::set_mix_enabled_in(&crate::db::DB, &auth.0, req)
        .await
        .map_err(failure_error)
}

#[get("/api/discovery/contextual-mixes/playlist?slug&sync", auth: AuthSession)]
pub async fn get_contextual_mix_playlist(
    slug: String,
    sync: Option<bool>,
) -> Result<shared::recommendation::ContextualMixPlaylist, ServerFnError> {
    crate::core::contextual::resolve_contextual_mix_playlist_in(
        &crate::db::DB,
        &crate::core::index::ServerNavidrome,
        crate::config::CONFIG.navidrome_music_path(),
        &auth.0,
        &slug,
        sync.unwrap_or(false),
        chrono::Utc::now(),
    )
    .await
    .map_err(failure_error)
}

#[post("/api/discovery/seasonal/steer", auth: AuthSession)]
pub async fn steer_seasonal_pack(
    req: shared::recommendation::SteerSeasonalPackRequest,
) -> Result<shared::recommendation::ContextualMixSummary, ServerFnError> {
    crate::core::contextual::steer_seasonal_pack_in(
        &crate::db::DB,
        &crate::core::index::ServerNavidrome,
        crate::config::CONFIG.navidrome_music_path(),
        &auth.0,
        &req.slug,
        req.steering,
        chrono::Utc::now(),
    )
    .await
    .map_err(failure_error)
}

#[get("/api/discovery/triage", auth: AuthSession)]
pub async fn list_triage_queue() -> Result<Vec<DiscoveryTrack>, ServerFnError> {
    crate::core::contextual::list_triage_queue_in(
        &crate::db::DB,
        &crate::core::index::ServerNavidrome,
        crate::config::CONFIG.navidrome_music_path(),
        &auth.0.user_id,
    )
    .await
    .map_err(failure_error)
}

#[post("/api/radio/start", auth: AuthSession)]
pub async fn start_seed_radio(
    req: shared::recommendation::SeedRadioRequest,
) -> Result<shared::recommendation::SeedRadioResponse, ServerFnError> {
    let settings = UserSettings::get(&auth.0.user_id)
        .await
        .map_err(server_error)?;
    let services = crate::core::recommend::DiscoveryServices::live();
    let plan = crate::core::recommend::plan_for(&settings);
    let (lb, _errors) = crate::core::recommend::listenbrainz_for(&services, plan.listenbrainz_token)
        .await
        .map_err(server_error)?;
    let labs =
        soulbeet::listenbrainz::labs::LabsClient::new(&services.hosts.labs, services.metabrainz_pace)
            .map_err(server_error)?;
    let knowledge = soulbeet::knowledge::metabrainz::MetaBrainzKnowledge::new(
        lb,
        labs,
        services.musicbrainz,
        services.cache,
    );
    crate::core::contextual::start_seed_radio_in(
        &crate::core::index::ServerNavidrome,
        &knowledge,
        &auth.0,
        &req,
    )
    .await
    .map_err(failure_error)
}

/// Discovery as the running server does it: runs in `DISCOVERY_PROGRESS`, the generation in
/// its own task, imports through the library's import queue.
#[cfg(feature = "server")]
pub(crate) struct LiveDiscovery;

#[cfg(feature = "server")]
#[async_trait::async_trait]
impl DiscoveryEffects for LiveDiscovery {
    async fn claim(&self, user: &UserId, progress: DiscoveryProgress) -> Result<(), Failure> {
        let mut map = crate::globals::DISCOVERY_PROGRESS.write().await;
        claim_run(&mut map, &user.to_string(), progress)
    }

    async fn current(&self, user: &UserId) -> Option<DiscoveryProgress> {
        let map = crate::globals::DISCOVERY_PROGRESS.read().await;
        map.get(&user.to_string()).cloned()
    }

    fn generate(&self, user: &UserId) {
        use shared::navidrome::{GenerationStatus, ProfilePhase};

        let uid = user.to_string();
        tokio::spawn(async move {
            match generate_discovery_playlist_internal(&uid).await {
                Ok(result) => {
                    let mut map = crate::globals::DISCOVERY_PROGRESS.write().await;
                    if let Some(p) = map.get_mut(&uid) {
                        p.status = GenerationStatus::Complete;
                        p.completed_at = Some(chrono::Utc::now().to_rfc3339());
                        for pp in &mut p.profiles {
                            if pp.phase != ProfilePhase::Skipped {
                                pp.phase = ProfilePhase::Done;
                                pp.current = pp.total;
                            }
                        }
                        p.result = Some(result);
                    }
                }
                Err(e) => {
                    let mut map = crate::globals::DISCOVERY_PROGRESS.write().await;
                    if let Some(p) = map.get_mut(&uid) {
                        p.status = GenerationStatus::Error;
                        p.completed_at = Some(chrono::Utc::now().to_rfc3339());
                        p.failure = Some(e);
                    }
                }
            }
        });
    }

    async fn import(
        &self,
        src: &std::path::Path,
        target: &std::path::Path,
        actor: &UserId,
        replace: Replace,
    ) -> Result<(), Failure> {
        let duplicates = match replace {
            Replace::Guarded(origin) => Duplicates::Guarded {
                actor: actor.clone(),
                origin,
            },
            Replace::Never => Duplicates::Skip,
        };
        crate::import_queue::import_to_library(src, target, duplicates).await
    }
}

/// A track discovery has enqueued and is waiting on, plus the peers it can
/// still fall back to.
#[cfg(feature = "server")]
struct QueuedTrack {
    artist: String,
    track: String,
    album: Option<String>,
    /// slskd's id for the transfer this track is waiting on. slskd keeps
    /// older records of the same peer and file, so only the id tells this
    /// transfer apart.
    transfer_id: String,
    /// The peer and remote filename of that transfer, claimed by `batch`.
    /// Moves to the new peer together with `transfer_id` on every retry.
    key: TransferKey,
    /// This track's own batch: the folder under slskd's download directory
    /// every attempt is enqueued into, and the owner of their registry keys.
    batch: BatchRef,
    /// When this track's transfer was first seen failed, cleared the moment
    /// it is seen in any other state.
    failing_since: Option<tokio::time::Instant>,
    /// When this track's transfer was first seen as a failed attempt slskd
    /// will retry (`read_pending_retry`).
    retry_pending_since: Option<tokio::time::Instant>,
    /// Alternate peers for this track, and how many have been tried.
    pool: SourcePool,
    /// The `(peer, transfer id)` of each attempt this track failed over
    /// from. Each was cancelled at its failover, and `stop_waiting` removes
    /// their records.
    abandoned: Vec<(String, String)>,
    /// Optional serialized `CandidateReason` for contextual/seasonal tracks.
    reason_json: Option<String>,
    /// Optional `CultureBucket` label for contextual/seasonal tracks.
    culture_bucket: Option<String>,
}

/// The track's import is done or abandoned once it is dropped, so whoever
/// asks for its files next may download them.
#[cfg(feature = "server")]
impl Drop for QueuedTrack {
    fn drop(&mut self) {
        TransferRegistry::global().release_batch(&self.batch.owner);
    }
}

/// What a discovery download did, as observed in the phase 2 poll loop.
#[cfg(feature = "server")]
enum Outcome {
    Succeeded,
    /// Transfer error: worth another peer.
    Failed,
    /// Cancelled by a user or by monitor cleanup: never retried.
    Cancelled,
}

/// Whether a discovery transfer in `state` delivered its file.
#[cfg(feature = "server")]
fn succeeded(state: &shared::download::DownloadState) -> bool {
    use shared::download::DownloadState as DS;
    matches!(state, DS::Completed | DS::Imported | DS::ImportSkipped)
}

/// Settle one poll of slskd's transfer list against the tracks still pending.
///
/// `pending` and `failed` hold transfer ids. Finished transfers leave
/// `pending`; a failed one retries from the track's next-best peer, moving
/// the track's key and transfer id to it, and only lands in `failed` once
/// the track is out of peers. A cancellation is a decision rather than a
/// peer problem, so it fails without a retry: one slskd shows, and one its
/// user asked for through the transfer registry, which is cancelled here
/// unless it already finished. A track that settles without a file gives up
/// its key.
///
/// A failed transfer is cancelled when its track moves on or runs out of
/// peers (`cancel_abandoned`): one read as failed past `RETRY_PENDING_FOR`
/// may still be in slskd's own retry, and a file it fetched late would land
/// in the batch folder with nobody to import it.
///
/// A failure counts only once it has persisted for `FAILED_STATE_CONFIRM`,
/// the same window `DownloadMonitor` applies. slskd 0.26 retries failed
/// transfers itself and reports the failure in between attempts, so acting on
/// the first sighting would put a second peer on a track slskd is still
/// delivering, and the winner would be whichever finished last. At a 5s poll
/// interval this costs one extra poll.
#[cfg(feature = "server")]
async fn settle_poll(
    queued: &mut [QueuedTrack],
    downloads: &[shared::download::DownloadProgress],
    pending: &mut std::collections::HashSet<String>,
    failed: &mut std::collections::HashSet<String>,
    backend: &Arc<dyn soulbeet::DownloadBackend>,
) {
    let registry = TransferRegistry::global();
    let mut newly_done: Vec<(usize, String, Outcome)> = Vec::new();
    for (idx, qt) in queued.iter_mut().enumerate() {
        if !pending.contains(&qt.transfer_id) {
            continue;
        }
        let listed = transfer_by_id(downloads, &qt.key.peer, &qt.transfer_id);
        if registry.is_cancelled(&qt.key, &qt.batch.owner)
            && !listed.is_some_and(|dl| succeeded(&dl.state))
        {
            info!(
                "Discovery: '{}' - {} cancelled on request",
                qt.artist, qt.track
            );
            if let Err(e) = backend
                .cancel_download(&qt.key.peer, &qt.transfer_id, false)
                .await
            {
                warn!(
                    "Failed to cancel transfer {} in slskd: {}",
                    qt.transfer_id, e
                );
            }
            newly_done.push((idx, qt.transfer_id.clone(), Outcome::Cancelled));
            continue;
        }
        let Some(dl) = listed else {
            continue;
        };
        let dl = read_pending_retry(dl, &mut qt.retry_pending_since);
        if !matches!(dl.state, shared::download::DownloadState::Failed(_)) {
            qt.failing_since = None;
        }
        match &dl.state {
            state if succeeded(state) => {
                newly_done.push((idx, qt.transfer_id.clone(), Outcome::Succeeded));
            }
            shared::download::DownloadState::Failed(_) => {
                let failing_since = qt
                    .failing_since
                    .get_or_insert_with(tokio::time::Instant::now);
                if failing_since.elapsed() >= FAILED_STATE_CONFIRM {
                    newly_done.push((idx, qt.transfer_id.clone(), Outcome::Failed));
                }
            }
            shared::download::DownloadState::Cancelled => {
                newly_done.push((idx, qt.transfer_id.clone(), Outcome::Cancelled));
            }
            _ => {}
        }
    }

    for (idx, id, outcome) in newly_done {
        pending.remove(&id);
        let qt = &mut queued[idx];
        match outcome {
            // Its key stays claimed until the import is done.
            Outcome::Succeeded => {}
            Outcome::Cancelled => {
                registry.release(&qt.key, &qt.batch.owner);
                failed.insert(id);
            }
            Outcome::Failed => {
                let next = qt.pool.enqueue_next(0, backend, &qt.batch).await;
                for refused in &next.refused {
                    warn!(
                        "Discovery: '{}' - {} failover source {} refused: {}: {}",
                        qt.artist,
                        qt.track,
                        refused.candidate.source,
                        refused.failure.code.as_str(),
                        refused.failure.message
                    );
                }
                match next.queued {
                    Some((_, next)) => {
                        info!(
                            "Discovery: '{}' - {} failed from {}, retrying from {}",
                            qt.artist, qt.track, qt.key.peer, next.source
                        );
                        // The new peer serves this track under its own remote
                        // path and transfer id, so the whole identity moves at
                        // once, a cancel asked for meanwhile with it, and the
                        // fresh transfer starts with no failure recorded.
                        let dead = std::mem::replace(
                            &mut qt.key,
                            transfer_key(backend.id(), &next.source, &next.item),
                        );
                        registry.hand_over(&dead, &qt.key, &qt.batch.owner);
                        cancel_abandoned(backend, &dead.peer, &id).await;
                        qt.abandoned.push((dead.peer.clone(), id));
                        qt.transfer_id = next.id.clone();
                        qt.failing_since = None;
                        qt.retry_pending_since = None;
                        pending.insert(next.id);
                    }
                    None => {
                        cancel_abandoned(backend, &qt.key.peer, &id).await;
                        registry.release(&qt.key, &qt.batch.owner);
                        failed.insert(id);
                    }
                }
            }
        }
    }
}

/// End the wait on `queued` the way `DownloadMonitor` ends a batch: cancel
/// the transfers still in `pending`, then remove the records of this run's
/// transfers, current or failed over from, that ended without a file. Left
/// running, a late transfer would land in its batch folder after its track
/// was given up on, and nobody would import or delete it.
///
/// The removal waits for slskd to end the cancelled transfers
/// (`await_ended`). A transfer that finished before its cancel reached
/// slskd delivered its file, so it leaves `pending` and is imported like
/// the rest.
#[cfg(feature = "server")]
async fn stop_waiting(
    queued: &[QueuedTrack],
    pending: &mut std::collections::HashSet<String>,
    backend: &Arc<dyn soulbeet::DownloadBackend>,
) {
    for qt in queued.iter().filter(|qt| pending.contains(&qt.transfer_id)) {
        cancel_abandoned(backend, &qt.key.peer, &qt.transfer_id).await;
    }
    let transfers: Vec<(String, String)> = (queued.iter())
        .flat_map(|qt| {
            std::iter::once((qt.key.peer.clone(), qt.transfer_id.clone()))
                .chain(qt.abandoned.iter().cloned())
        })
        .collect();
    await_ended(backend, &transfers).await;
    let Some(listed) = remove_ended_transfers(backend, &transfers).await else {
        return;
    };
    for qt in queued {
        if transfer_by_id(&listed, &qt.key.peer, &qt.transfer_id)
            .is_some_and(|dl| succeeded(&dl.state))
        {
            pending.remove(&qt.transfer_id);
        }
    }
}

/// What a discovery run's source search for one candidate track came to.
#[cfg(feature = "server")]
#[derive(Debug)]
enum CandidateSearch {
    /// The items found, best first.
    Found(Vec<shared::download::DownloadableItem>),
    /// Nothing was found, or nothing that scored well enough.
    Missed(Failure),
    /// The search failed, which says nothing of the track.
    Failed(Failure),
}

/// Searches `searched` for `track` by `artist`, from `album`, for a discovery run of `user_id`,
/// counting in `db` the runs in a row whose search for it found nothing
/// (`discovery_search_miss`). Err is the database's text.
#[cfg(feature = "server")]
async fn search_candidate(
    db: &sqlx::SqlitePool,
    searched: &crate::core::sources::Searched,
    user_id: &str,
    artist: &str,
    track: &str,
    album: Option<&str>,
) -> Result<CandidateSearch, String> {
    use crate::models::discovery_search_miss::{clear_in, record_in};
    let wanted = [shared::metadata::Track {
        id: String::new(),
        title: track.to_string(),
        artist: artist.to_string(),
        album_id: None,
        album_title: album.map(str::to_string),
        release_date: None,
        duration: None,
        mbid: None,
        release_mbid: None,
    }];
    let found = crate::core::sources::search_tier_with_priority(
        std::slice::from_ref(searched),
        None,
        &wanted,
        shared::slskd::SearchPriority::Background,
    )
    .await;
    if !found.groups.is_empty() {
        clear_in(db, user_id, artist, track).await?;
        return Ok(CandidateSearch::Found(
            found.groups.into_iter().flat_map(|g| g.items).collect(),
        ));
    }
    let failure = found.failure();
    match failure.code {
        FailureCode::SearchNoResults | FailureCode::SearchAllFiltered => {
            record_in(db, user_id, artist, track).await?;
            Ok(CandidateSearch::Missed(failure))
        }
        _ => Ok(CandidateSearch::Failed(failure)),
    }
}

#[cfg(feature = "server")]
pub async fn generate_discovery_playlist_internal(
    user_id: &str,
) -> Result<shared::navidrome::GenerationResult, Failure> {
    use crate::core::sources::Searched;
    use crate::models::deletion_review::DeletionReviewRow;
    use crate::models::discovery_candidate::DiscoveryCandidateRow;
    use crate::models::discovery_history::{seen_key, DiscoveryHistoryRow};
    use crate::models::discovery_search_miss;
    use crate::services::{download_backend, DEFAULT_DOWNLOAD_BACKEND};
    use shared::navidrome::{GenerationResult, ProfileGenerationStats};

    // Acquire per-user lock to prevent concurrent generation
    let user_lock = {
        let mut locks = GENERATION_LOCKS.lock().await;
        locks
            .entry(user_id.to_string())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    };
    let Ok(_guard) = user_lock.try_lock() else {
        return Err(generation_running());
    };

    let settings = UserSettings::get(user_id)
        .await
        .map_err(|e| run_database_failure(Stage::Input, e))?;
    if !settings.discovery_enabled {
        return Err(discovery_disabled());
    }
    let folder_id = settings
        .discovery_folder_id
        .as_ref()
        .ok_or_else(discovery_folder_required)?;
    let folder = Folder::get_by_id(folder_id)
        .await
        .map_err(|e| run_database_failure(Stage::Input, e))?
        .ok_or_else(discovery_folder_not_found)?;

    // Shown as the run's error, so the user sees why nothing was imported.
    if let Some(failure) = crate::importer_health::blocking_failure().await {
        return Err(failure);
    }

    let selected_profiles = parse_profiles(&settings.discovery_profiles);
    let track_counts = settings.parse_track_counts();

    let backend = download_backend(None)
        .await
        .map_err(|e| e.failure(Stage::SourceSearch, e.to_string()))?;
    let searched: Searched = (DEFAULT_DOWNLOAD_BACKEND.to_string(), backend.clone());

    // Create per-profile subdirectories under Discovery/
    for profile in &selected_profiles {
        let profile_dir = folder.discovery_profile_path(&profile.to_string());
        tokio::fs::create_dir_all(&profile_dir).await.map_err(|e| {
            target_unwritable(
                &e,
                format!("Failed to create Discovery/{} dir: {}", profile, e),
            )
        })?;
    }

    // Build the "already seen" set for deduplication
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();

    // Past discovery suggestions
    let history_keys = DiscoveryHistoryRow::get_suggested_keys(user_id)
        .await
        .map_err(|e| run_database_failure(Stage::Input, e))?;
    info!(
        "Loaded {} tracks from discovery history",
        history_keys.len()
    );
    seen.extend(history_keys);

    // Past deletions (user doesn't want these back)
    let disliked = DeletionReviewRow::disliked_keys(user_id)
        .await
        .map_err(|e| run_database_failure(Stage::Input, e))?;
    info!("Added {} deleted tracks to exclusion set", disliked.len());
    seen.extend(disliked);

    // Explicit discovery rejections (removed staged tracks)
    let rejected = crate::models::contextual::DiscoveryRejectionRow::keys_for_user_in(
        &crate::db::DB,
        user_id,
    )
    .await
    .map_err(|e| run_database_failure(Stage::Input, e))?;
    info!("Added {} rejected discovery tracks to exclusion set", rejected.len());
    seen.extend(rejected);

    // Tracks whose search kept finding nothing, which the engine would suggest every run
    let kept_out = discovery_search_miss::kept_out_keys(user_id)
        .await
        .map_err(|e| run_database_failure(Stage::Input, e))?;
    info!(
        "Added {} tracks never found on the network to exclusion set",
        kept_out.len()
    );
    seen.extend(kept_out);

    // Pre-register any calendar-active seasonal packs that need tracks in DISCOVERY_PROGRESS so
    // the progress bar reflects remaining seasonal work even if rolling profiles are skipped.
    if let Ok((_, seasonal_work)) =
        collect_seasonal_work(user_id, folder_id, chrono::Utc::now()).await
    {
        register_seasonal_progress_profiles(user_id, &seasonal_work).await;
    }

    let mut total_downloads = 0u32;
    let mut all_profile_stats: Vec<ProfileGenerationStats> = Vec::new();
    // Set when the importer broke during the run and stopped it.
    let mut importer_issue: Option<Failure> = None;
    // Set when the engine failed. Every profile is built from the same library read, so it
    // is not run again for later profiles, which still use the candidates they hold.
    let mut engine_issue: Option<Failure> = None;

    for profile in &selected_profiles {
        let profile_name = profile.to_string();
        let target = track_counts.get(&profile_name).copied().unwrap_or(10) as usize;

        // Account for tracks already in the playlist so regeneration fills gaps
        let existing =
            DiscoveryTrackRow::get_pending_by_folder_and_profile(folder_id, &profile_name)
                .await
                .map(|v| v.len())
                .unwrap_or(0);
        let tracks_per_profile = target.saturating_sub(existing);
        if tracks_per_profile == 0 {
            update_progress(
                user_id,
                &profile_name,
                shared::navidrome::ProfilePhase::Skipped,
                0,
                0,
            )
            .await;
            info!(
                "{}: already at target ({} pending tracks)",
                profile_name, existing
            );
            continue;
        }
        update_progress(
            user_id,
            &profile_name,
            shared::navidrome::ProfilePhase::PullingCandidates,
            0,
            0,
        )
        .await;
        info!(
            "{}: need {} more tracks ({} existing, {} target)",
            profile_name, tracks_per_profile, existing, target
        );

        let mut profile_downloads = 0u32;
        let mut stats = ProfileGenerationStats {
            profile: profile_name.clone(),
            target: tracks_per_profile as u32,
            ..Default::default()
        };

        // Retry loop: keep pulling new candidates until target is met or pool is dry.
        // Each attempt searches Soulseek, downloads, and imports. Tracks that fail
        // at any stage are added to `seen` so the next attempt skips them.
        for _attempt in 0..3u32 {
            stats.attempts += 1;
            let remaining = tracks_per_profile.saturating_sub(profile_downloads as usize);
            if remaining == 0 {
                break;
            }

            // Over-fetch candidates (3x) to survive search/download/import attrition
            let fetch_count = (remaining as f64 * 3.0).ceil() as u32;
            let mut candidates =
                DiscoveryCandidateRow::get_unused(user_id, &profile_name, fetch_count)
                    .await
                    .map_err(|e| run_database_failure(Stage::Input, e))?;

            if candidates.is_empty() {
                if _attempt == 0 && engine_issue.is_none() {
                    update_progress(
                        user_id,
                        &profile_name,
                        shared::navidrome::ProfilePhase::GeneratingRecommendations,
                        0,
                        0,
                    )
                    .await;
                    info!("No candidates for {}, running engine", profile_name);
                    match generate_recommendations_for_user(user_id, *profile).await {
                        Ok(count) => {
                            info!("Generated {} candidates for {}", count, profile_name)
                        }
                        Err(f) => {
                            warn!(
                                "{} engine failed: {}",
                                profile_name,
                                crate::core::logged(&f)
                            );
                            engine_issue = Some(f);
                            break;
                        }
                    }
                    candidates =
                        DiscoveryCandidateRow::get_unused(user_id, &profile_name, fetch_count)
                            .await
                            .map_err(|e| run_database_failure(Stage::Input, e))?;
                }
                if candidates.is_empty() {
                    info!("{}: no more candidates available", profile_name);
                    break;
                }
            }

            if let Some(failure) = crate::importer_health::blocking_failure().await {
                warn!("Discovery for {user_id} skipped: {}", failure.message);
                importer_issue = Some(failure);
                break;
            }

            // Phase 1: Search and queue downloads
            let mut queued: Vec<QueuedTrack> = Vec::new();

            // Over-queue by 50% to absorb import failures without another retry round
            let queue_target = remaining + remaining.div_ceil(2);

            for candidate in &candidates {
                if queued.len() >= queue_target {
                    break;
                }

                let key = seen_key(&candidate.artist, &candidate.track);
                if seen.contains(&key) {
                    stats.candidates_skipped_seen += 1;
                    // Used, so the next attempt draws other candidates in its place.
                    DiscoveryCandidateRow::mark_used(
                        user_id,
                        &profile_name,
                        &candidate.artist,
                        &candidate.track,
                    )
                    .await
                    .map_err(|e| run_database_failure(Stage::Input, e))?;
                    continue;
                }
                stats.candidates_tried += 1;
                update_progress(
                    user_id,
                    &profile_name,
                    shared::navidrome::ProfilePhase::SearchingSoulseek,
                    stats.candidates_tried,
                    candidates.len() as u32,
                )
                .await;

                let searched_for = search_candidate(
                    &crate::db::DB,
                    &searched,
                    user_id,
                    &candidate.artist,
                    &candidate.track,
                    candidate.album.as_deref(),
                )
                .await
                .map_err(|e| run_database_failure(Stage::SourceSearch, e))?;
                let mut ranked_items = match searched_for {
                    CandidateSearch::Found(items) => items,
                    CandidateSearch::Missed(failure) => {
                        info!(
                            "No results for '{}' - {}, skipping: {}",
                            candidate.artist, candidate.track, failure.message
                        );
                        stats.search_misses += 1;
                        Vec::new()
                    }
                    CandidateSearch::Failed(failure) => {
                        warn!(
                            "Search failed for '{}' - {}: {}",
                            candidate.artist,
                            candidate.track,
                            crate::core::logged(&failure)
                        );
                        stats.search_errors += 1;
                        Vec::new()
                    }
                };
                if ranked_items.is_empty() {
                    seen.insert(key.clone());
                    DiscoveryCandidateRow::mark_used(
                        user_id,
                        &profile_name,
                        &candidate.artist,
                        &candidate.track,
                    )
                    .await
                    .map_err(|e| run_database_failure(Stage::SourceSearch, e))?;
                    continue;
                }
                stats.search_hits += 1;

                // Keep the sources this track is allowed to try: the one
                // enqueued here plus the alternates phase 2 fails over to.
                ranked_items.truncate(failover::MAX_SOURCES_PER_TRACK);
                let track_batch = BatchRef::new(
                    TransferOwner {
                        user_id: UserId::new(user_id),
                        batch_id: uuid::Uuid::new_v4().to_string(),
                    },
                    format!("{} - {}", candidate.artist, candidate.track),
                    std::path::PathBuf::from(folder.discovery_profile_path(&profile_name)),
                    soulbeet::ImportMode::Singleton,
                    searched.0.clone(),
                );
                // Discovery watches its own transfers, so nothing may join it.
                track_batch.inbox.close();
                let mut downloaded = false;
                for (attempt_idx, item) in ranked_items.iter().enumerate() {
                    let transfer = transfer_key(backend.id(), &item.source, &item.id);
                    let claim = TransferRegistry::global()
                        .claim(std::slice::from_ref(&transfer), &track_batch);
                    if claim.claimed.is_empty() {
                        info!(
                            "Download source {} skipped for '{}' - {}: {}",
                            attempt_idx + 1,
                            candidate.artist,
                            candidate.track,
                            held_failure(&claim, &track_batch).message
                        );
                        stats.downloads_failed += 1;
                        continue;
                    }
                    // Discovery keeps no ledger, so nothing reads its request back.
                    let request = backend.new_request();
                    let download_results =
                        match enqueue_claimed(&backend, vec![item.clone()], &track_batch, &request)
                            .await
                        {
                            Ok(r) => r,
                            Err(e) => {
                                if attempt_idx + 1 < ranked_items.len() {
                                    info!(
                                        "Download source {} failed for '{}' - {}, trying next: {}",
                                        attempt_idx + 1,
                                        candidate.artist,
                                        candidate.track,
                                        e
                                    );
                                } else {
                                    warn!(
                                        "Download failed for '{}' - {} (all sources exhausted): {}",
                                        candidate.artist, candidate.track, e
                                    );
                                }
                                stats.downloads_failed += 1;
                                continue;
                            }
                        };

                    if let Some(dl) = download_results
                        .iter()
                        .find(|dl| dl.status == shared::download::EnqueueStatus::Enqueued)
                    {
                        stats.downloads_queued += 1;
                        seen.insert(key.clone());
                        queued.push(QueuedTrack {
                            artist: candidate.artist.clone(),
                            track: candidate.track.clone(),
                            album: candidate.album.clone(),
                            transfer_id: dl.id.clone(),
                            key: transfer,
                            batch: track_batch.clone(),
                            failing_since: None,
                            retry_pending_since: None,
                            // Start the pool at the source that actually
                            // enqueued: earlier entries were already tried and
                            // rejected in this loop, and must not be retried.
                            pool: SourcePool::single(ranked_items[attempt_idx..].to_vec()),
                            abandoned: Vec::new(),
                            reason_json: None,
                            culture_bucket: None,
                        });
                        DiscoveryCandidateRow::mark_used(
                            user_id,
                            &profile_name,
                            &candidate.artist,
                            &candidate.track,
                        )
                        .await
                        .map_err(|e| run_database_failure(Stage::Enqueue, e))?;
                        downloaded = true;
                        break;
                    }
                    // This source refused the file in its response
                    stats.downloads_failed += 1;
                    if attempt_idx + 1 < ranked_items.len() {
                        info!(
                            "Download source {} refused '{}' - {} ({}), trying next",
                            attempt_idx + 1,
                            candidate.artist,
                            candidate.track,
                            not_enqueued_failure(&download_results).message
                        );
                    }
                }
                if !downloaded {
                    seen.insert(key.clone());
                    DiscoveryCandidateRow::mark_used(
                        user_id,
                        &profile_name,
                        &candidate.artist,
                        &candidate.track,
                    )
                    .await
                    .map_err(|e| run_database_failure(Stage::Enqueue, e))?;
                }
            }

            if queued.is_empty() {
                break;
            }

            // Phase 2: Wait for downloads to complete, then move to Discovery/
            info!(
                "{}: {} downloads queued, waiting for completion (attempt {})...",
                profile_name,
                queued.len(),
                _attempt + 1,
            );
            let download_base = crate::config::CONFIG.download_path().clone();

            // Poll slskd until all queued downloads are complete (or timeout after 10 min).
            // Note: This 10-minute timeout starts from enqueue, not from transfer start.
            // A slow peer (3 min queue wait + 8 min transfer) could exceed it. This is
            // acceptable because: (1) discovery tracks are single songs, not albums,
            // (2) most Soulseek transfers start quickly, and (3) timed-out tracks are
            // recorded in stats and their transfers cancelled (`stop_waiting`).
            let wait_start = tokio::time::Instant::now();
            let max_wait = tokio::time::Duration::from_secs(600);
            let mut pending_ids: std::collections::HashSet<String> =
                queued.iter().map(|q| q.transfer_id.clone()).collect();
            let mut failed_ids: std::collections::HashSet<String> =
                std::collections::HashSet::new();

            while !pending_ids.is_empty() && wait_start.elapsed() < max_wait {
                tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;
                let completed = (queued.len() as u32)
                    .saturating_sub(pending_ids.len() as u32 + failed_ids.len() as u32);
                update_progress(
                    user_id,
                    &profile_name,
                    shared::navidrome::ProfilePhase::Downloading,
                    completed,
                    queued.len() as u32,
                )
                .await;

                // Only the peers this batch is waiting on: the full list
                // carries slskd's entire retained history (#78). Rebuilt each
                // pass so a failover's new peer is included.
                let mut sources: Vec<String> = Vec::new();
                for track in &queued {
                    if !sources.contains(&track.key.peer) {
                        sources.push(track.key.peer.clone());
                    }
                }

                let downloads = match backend.get_downloads_for(&sources).await {
                    Ok(d) => d,
                    Err(_) => continue,
                };

                settle_poll(
                    &mut queued,
                    &downloads,
                    &mut pending_ids,
                    &mut failed_ids,
                    &backend,
                )
                .await;
            }
            stop_waiting(&queued, &mut pending_ids, &backend).await;

            stats.downloads_timed_out += pending_ids.len() as u32;
            stats.downloads_failed += failed_ids.len() as u32;
            stats.downloads_completed +=
                (queued.len() - pending_ids.len() - failed_ids.len()) as u32;
            if !pending_ids.is_empty() {
                warn!(
                    "{}: {} downloads didn't complete within timeout",
                    profile_name,
                    pending_ids.len()
                );
            }
            if !failed_ids.is_empty() {
                warn!(
                    "{}: {} downloads failed in slskd",
                    profile_name,
                    failed_ids.len()
                );
            }

            // Phase 3: Import completed files into Discovery/{profile}/ via beets and record in DB
            let profile_path = folder.discovery_profile_path(&profile_name);
            let discovery_target = std::path::PathBuf::from(&profile_path);

            // Skip both timed-out and slskd-failed downloads
            let skip_ids: std::collections::HashSet<&String> =
                pending_ids.iter().chain(failed_ids.iter()).collect();

            let importable_count = queued
                .iter()
                .filter(|q| !skip_ids.contains(&q.transfer_id))
                .count() as u32;
            let mut import_idx = 0u32;
            for qt in &queued {
                if skip_ids.contains(&qt.transfer_id) {
                    continue;
                }
                import_idx += 1;
                update_progress(
                    user_id,
                    &profile_name,
                    shared::navidrome::ProfilePhase::Importing,
                    import_idx,
                    importable_count,
                )
                .await;

                let resolved = crate::server_fns::download::process::resolve_on_disk(
                    &qt.key.filename,
                    &download_base,
                    Some(&qt.batch.destination),
                    &std::sync::Mutex::new(std::collections::HashSet::new()),
                )
                .await;
                let src = match resolved {
                    Ok(p) => p,
                    Err(_) => {
                        warn!(
                            "Could not find downloaded file for '{}' - {} (slskd: {})",
                            qt.artist, qt.track, qt.key.filename
                        );
                        TransferRegistry::global().release(&qt.key, &qt.batch.owner);
                        stats.imports_file_missing += 1;
                        continue;
                    }
                };

                let verify_path = src.clone();
                let verified = tokio::task::spawn_blocking(move || {
                    soulbeet::verify_downloaded_audio(
                        &verify_path,
                        soulbeet::VerifyPolicy::Standard,
                        None,
                    )
                })
                .await;
                match verified {
                    Ok(Ok(_)) => {}
                    Ok(Err(failure)) => {
                        warn!(
                            "Post-download verification failed for '{}' - {} ({}): {}",
                            qt.artist,
                            qt.track,
                            failure.code.as_str(),
                            failure.message
                        );
                        discard_download(&src, &download_base).await;
                        TransferRegistry::global().release(&qt.key, &qt.batch.owner);
                        stats.imports_failed += 1;
                        continue;
                    }
                    Err(join_err) => {
                        warn!(
                            "Post-download verification task failed for '{}' - {}: {}",
                            qt.artist, qt.track, join_err
                        );
                        discard_download(&src, &download_base).await;
                        TransferRegistry::global().release(&qt.key, &qt.batch.owner);
                        stats.imports_failed += 1;
                        continue;
                    }
                }

                let outcome = IMPORT_QUEUE
                    .submit(ImportJob {
                        sources: vec![src.clone()],
                        files: vec![src.clone()],
                        target: discovery_target.clone(),
                        mode: soulbeet::ImportMode::Singleton,
                        owner: None,
                        snapshot_target: true,
                        cleanup_root: Some(download_base.clone()),
                        choice: soulbeet::DriverChoice::Auto,
                        choices: None,
                        // Discovery's staging keeps its own rules (section 13).
                        duplicates: Duplicates::AsConfigured,
                        last_run: None,
                        lock_retries: 0,
                    })
                    .await
                    .wait()
                    .await;
                TransferRegistry::global().release(&qt.key, &qt.batch.owner);
                let result = &outcome.result;
                match classify_discovery(&result.outcome) {
                    DiscoveryImport::Imported => {
                        info!(
                            "Imported '{}' - {} into Discovery/{}",
                            qt.artist, qt.track, profile_name
                        );
                    }
                    DiscoveryImport::Permanent => {
                        warn!("Beets skipped '{}' - {}: {result}", qt.artist, qt.track);
                        stats.imports_skipped += 1;
                        discard_download(&src, &download_base).await;
                        // A duplicate is a permanent condition: the library
                        // already holds this track, so suggesting it again
                        // would download it again. Only genuine failures
                        // stay eligible for rediscovery.
                        DiscoveryHistoryRow::record(user_id, &qt.artist, &qt.track, &profile_name)
                            .await
                            .map_err(|e| run_database_failure(Stage::Import, e))?;
                        continue;
                    }
                    DiscoveryImport::Failed => {
                        warn!(
                            "Beets import failed for '{}' - {}: {result}",
                            qt.artist, qt.track
                        );
                        stats.imports_failed += 1;
                        discard_download(&src, &download_base).await;
                        continue;
                    }
                }

                let new_files = &outcome.new_files;
                if new_files.is_empty() {
                    warn!(
                        "Could not find imported file for '{}' - {} in {}",
                        qt.artist, qt.track, profile_path
                    );
                    stats.imports_file_missing += 1;
                    continue;
                }
                for rel in new_files {
                    DiscoveryTrackRow::create(
                        None,
                        &qt.track,
                        &qt.artist,
                        qt.album.as_deref().unwrap_or(""),
                        &discovery_target.join(rel).to_string_lossy(),
                        folder_id,
                        &profile_name,
                    )
                    .await
                    .map_err(|e| run_database_failure(Stage::Import, e))?;
                }
                // Recorded here, not at enqueue: discovery history is the
                // permanent exclusion set, and a track killed by a bad peer
                // must stay available to suggest again.
                DiscoveryHistoryRow::record(user_id, &qt.artist, &qt.track, &profile_name)
                    .await
                    .map_err(|e| run_database_failure(Stage::Import, e))?;
                stats.imports_succeeded += 1;
                profile_downloads += 1;
            }

            if (profile_downloads as usize) >= tracks_per_profile {
                break;
            }
            info!(
                "{}: {}/{} tracks after attempt {}, retrying with more candidates...",
                profile_name,
                profile_downloads,
                tracks_per_profile,
                _attempt + 1
            );
        } // end retry loop

        info!(
            "{}: {} tracks imported to Discovery/",
            profile_name, profile_downloads
        );

        total_downloads += profile_downloads;
        all_profile_stats.push(stats);
    }

    // Also build/top-up any calendar-active seasonal packs (e.g. Winter Holiday / Christmas)
    // as part of the same discovery run before scanning and reconciling playlists.
    if importer_issue.is_none() {
        match generate_seasonal_packs_unlocked(user_id, &settings, &folder, &backend, &searched)
            .await
        {
            Ok(seasonal_imported) => {
                if seasonal_imported > 0 {
                    info!(
                        "Discovery: imported {} seasonal tracks for user {}",
                        seasonal_imported, user_id
                    );
                    total_downloads += seasonal_imported;
                }
            }
            Err(f) if f.code == FailureCode::ImporterBroken => {
                importer_issue = Some(f);
            }
            Err(f) => {
                warn!(
                    "Seasonal pack generation failed during discovery run for user {}: {}",
                    user_id,
                    crate::core::logged(&f)
                );
            }
        }
    }

    UserSettings::update_discovery_last_generated(user_id)
        .await
        .map_err(|e| run_database_failure(Stage::Import, e))?;

    // Trigger a Navidrome scan then wait for it to finish so playlists can be created.
    if total_downloads > 0 {
        scan_reconcile_after_imports(user_id, total_downloads).await;
    }

    run_outcome(
        GenerationResult {
            total_imported: total_downloads,
            profiles: all_profile_stats,
        },
        importer_issue,
        engine_issue,
    )
}

/// Triggers a Navidrome scan after `imported` discovery tracks are staged, reconciles their
/// Navidrome `song_id`s in SQLite, and reconciles rolling + seasonal Navidrome smart playlists.
#[cfg(feature = "server")]
async fn scan_reconcile_after_imports(user_id: &str, imported: u32) {
    if imported == 0 {
        return;
    }
    if let Ok(navi) = crate::services::navidrome_client_for_user(user_id).await {
        let navidrome = crate::core::index::ServerNavidrome;
        let user = UserId::new(user_id);
        if scan_imports(&crate::db::DB, &navidrome, &user, &navi, imported).await {
            match DiscoveryTrackRow::pending_for_user_in(&crate::db::DB, user_id).await {
                Ok(mut pending)
                    if pending
                        .iter()
                        .any(crate::core::contextual::needs_song_id_reconciliation) =>
                {
                    match navi.get_all_songs_with_ratings().await {
                        Ok(library) => {
                            let reconciled =
                                crate::core::contextual::reconcile_discovery_song_ids_in(
                                    &crate::db::DB,
                                    &library,
                                    crate::config::CONFIG.navidrome_music_path(),
                                    &mut pending,
                                )
                                .await;
                            if reconciled > 0 {
                                info!(
                                    "Reconciled {} discovery track song_ids after scan for user {}",
                                    reconciled, user_id
                                );
                            }
                        }
                        Err(e) => warn!(
                            "Failed to fetch Navidrome library to reconcile discovery song_ids for user {}: {}",
                            user_id, e
                        ),
                    }
                }
                Ok(_) => {}
                Err(e) => warn!(
                    "Failed to load pending discovery tracks for song_id reconciliation (user {}): {}",
                    user_id, e
                ),
            }
            if let Err(e) = reconcile_discovery_playlists(user_id).await {
                warn!("Playlist reconciliation after generation failed: {}", e);
            }
        }
    }
}

/// Acquires `user_id`'s discovery generation lock, claims `DISCOVERY_PROGRESS` cleanly when
/// seasonal work is needed, builds/tops up any calendar-active seasonal mixes (staging under
/// `Discovery/Seasonal/<slug>/`), and triggers a Navidrome scan and seasonal smart playlist
/// reconciliation if any tracks were imported.
#[cfg(feature = "server")]
pub async fn generate_seasonal_packs_for_user(user_id: &str) -> Result<u32, Failure> {
    use crate::core::sources::Searched;
    use crate::services::{download_backend, DEFAULT_DOWNLOAD_BACKEND};
    use shared::navidrome::{GenerationResult, GenerationStatus, ProfilePhase, ProfileProgress};

    let user_lock = {
        let mut locks = GENERATION_LOCKS.lock().await;
        locks
            .entry(user_id.to_string())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    };
    let Ok(_guard) = user_lock.try_lock() else {
        return Err(generation_running());
    };

    let settings = UserSettings::get(user_id)
        .await
        .map_err(|e| run_database_failure(Stage::Input, e))?;
    if !settings.discovery_enabled {
        return Err(discovery_disabled());
    }
    let folder_id = settings
        .discovery_folder_id
        .as_ref()
        .ok_or_else(discovery_folder_required)?;
    let folder = Folder::get_by_id(folder_id)
        .await
        .map_err(|e| run_database_failure(Stage::Input, e))?
        .ok_or_else(discovery_folder_not_found)?;

    if let Some(failure) = crate::importer_health::blocking_failure().await {
        return Err(failure);
    }

    let now = chrono::Utc::now();
    let (stored_culture, work) = collect_seasonal_work(user_id, &folder.id, now).await?;
    if work.is_empty() {
        return Ok(0);
    }

    let backend = download_backend(None)
        .await
        .map_err(|e| e.failure(Stage::SourceSearch, e.to_string()))?;
    let searched: Searched = (DEFAULT_DOWNLOAD_BACKEND.to_string(), backend.clone());

    let initial_profiles: Vec<ProfileProgress> = work
        .iter()
        .map(|(def, _, needed)| ProfileProgress {
            profile: seasonal_progress_label(def),
            phase: ProfilePhase::Waiting,
            current: 0,
            total: *needed as u32,
        })
        .collect();
    let progress = DiscoveryProgress {
        status: GenerationStatus::Running,
        profiles: initial_profiles,
        started_at: Some(chrono::Utc::now().to_rfc3339()),
        ..Default::default()
    };
    {
        let mut map = crate::globals::DISCOVERY_PROGRESS.write().await;
        claim_run(&mut map, user_id, progress)?;
    }

    let outcome = generate_seasonal_packs_with_work(
        user_id,
        &settings,
        &folder,
        &backend,
        &searched,
        now,
        stored_culture,
        work,
    )
    .await;

    match outcome {
        Ok(imported) => {
            if imported > 0 {
                scan_reconcile_after_imports(user_id, imported).await;
            }
            let mut map = crate::globals::DISCOVERY_PROGRESS.write().await;
            if let Some(p) = map.get_mut(user_id) {
                p.status = GenerationStatus::Complete;
                p.completed_at = Some(chrono::Utc::now().to_rfc3339());
                for pp in &mut p.profiles {
                    if pp.phase != ProfilePhase::Skipped {
                        pp.phase = ProfilePhase::Done;
                        pp.current = pp.total;
                    }
                }
                p.result = Some(GenerationResult {
                    total_imported: imported,
                    profiles: Vec::new(),
                });
            }
            Ok(imported)
        }
        Err(e) => {
            let mut map = crate::globals::DISCOVERY_PROGRESS.write().await;
            if let Some(p) = map.get_mut(user_id) {
                p.status = GenerationStatus::Error;
                p.completed_at = Some(chrono::Utc::now().to_rfc3339());
                p.failure = Some(e.clone());
            }
            Err(e)
        }
    }
}

/// Builds and downloads candidates for every calendar-active seasonal mix whose pending
/// `pack_slug` count is below `target_tracks`. Caller must hold `GENERATION_LOCKS` for `user_id`.
#[cfg(feature = "server")]
async fn generate_seasonal_packs_unlocked(
    user_id: &str,
    settings: &UserSettings,
    folder: &Folder,
    backend: &Arc<dyn soulbeet::DownloadBackend>,
    searched: &crate::core::sources::Searched,
) -> Result<u32, Failure> {
    let now = chrono::Utc::now();
    let (stored_culture, work) = collect_seasonal_work(user_id, &folder.id, now).await?;
    if work.is_empty() {
        return Ok(0);
    }
    generate_seasonal_packs_with_work(
        user_id,
        settings,
        folder,
        backend,
        searched,
        now,
        stored_culture,
        work,
    )
    .await
}

#[cfg(feature = "server")]
#[allow(clippy::too_many_arguments)]
async fn generate_seasonal_packs_with_work(
    user_id: &str,
    settings: &UserSettings,
    folder: &Folder,
    backend: &Arc<dyn soulbeet::DownloadBackend>,
    searched: &crate::core::sources::Searched,
    now: chrono::DateTime<chrono::Utc>,
    stored_culture: shared::recommendation::UserCultureProfile,
    work: Vec<SeasonalWorkItem>,
) -> Result<u32, Failure> {
    use chrono::Datelike;
    use shared::recommendation::{
        ArtistRef, CandidateSet, CultureSteering, DiscoveryProfile, ProfileConfig, TrackRef,
        UserCultureProfile, UserMusicProfile,
    };
    use soulbeet::engine::contextual::{
        infer_culture_profile, ContextSpec, ContextualCandidateGenerator, ListenBrainzPlaylistPool,
    };
    use soulbeet::knowledge::metabrainz::MetaBrainzKnowledge;
    use soulbeet::listenbrainz::labs::LabsClient;
    use soulbeet::musicbrainz::MusicBrainzProvider;
    use soulbeet::CandidateGenerator;

    use crate::models::contextual::{
        ContextualMixStateRow, DiscoveryRejectionRow, UserCultureProfileRow,
    };
    use crate::models::deletion_review::DeletionReviewRow;
    use crate::models::discovery_history::{seen_key, DiscoveryHistoryRow};
    use crate::models::discovery_playlist::DiscoveryTrackContext;
    use crate::models::discovery_search_miss;

    register_seasonal_progress_profiles(user_id, &work).await;

    // Set up MetaBrainz knowledge + ListenBrainz community playlist pool + MusicBrainz fallback.
    let services = crate::core::recommend::DiscoveryServices::live();
    let plan = crate::core::recommend::plan_for(settings);
    let (listenbrainz, _) =
        crate::core::recommend::listenbrainz_for(&services, plan.listenbrainz_token)
            .await
            .map_err(|e| {
                Failure::new(
                    FailureCode::MetadataUnavailable,
                    Stage::Input,
                    RetryAdvice::SameSourceLater,
                    e.to_string(),
                )
            })?;
    let labs = LabsClient::new(&services.hosts.labs, services.metabrainz_pace).map_err(|e| {
        Failure::new(
            FailureCode::MetadataUnavailable,
            Stage::Input,
            RetryAdvice::NotRetryable,
            e.to_string(),
        )
    })?;
    let knowledge = Arc::new(MetaBrainzKnowledge::new(
        listenbrainz.clone(),
        labs,
        services.musicbrainz.clone(),
        services.cache.clone(),
    ));
    let pool = Arc::new(ListenBrainzPlaylistPool::new(listenbrainz));
    let metadata: Arc<dyn soulbeet::MetadataProvider> = Arc::new(MusicBrainzProvider::new());

    // Build user's taste profile + known tracks from Navidrome (best-effort so seasonal packs
    // still generate even for a fresh library with few or no plays).
    let mut profile: UserMusicProfile = match crate::services::navidrome_client_for_user(user_id)
        .await
    {
        Ok(navi) => {
            let excluded_tracks =
                DiscoveryTrackRow::excluded_from_core_taste_for_user_in(&crate::db::DB, user_id)
                    .await
                    .unwrap_or_default();
            let excluded_song_ids: std::collections::HashSet<String> = excluded_tracks
                .iter()
                .filter_map(|t| t.song_id.clone().filter(|s| !s.is_empty()))
                .collect();
            let excluded_keys: std::collections::HashSet<String> = excluded_tracks
                .iter()
                .map(|t| CandidateSet::key(&t.artist, &t.title))
                .collect();

            match soulbeet::history::navidrome::read(&navi).await {
                Ok(lib) => {
                    let filtered = lib.exclude_from_core_taste(move |song| {
                        let lower_path = song.path.replace('\\', "/").to_ascii_lowercase();
                        lower_path.contains("/discovery/seasonal/")
                            || lower_path.starts_with("discovery/seasonal/")
                            || excluded_song_ids.contains(&song.id)
                            || excluded_keys.contains(&CandidateSet::key(&song.artist, &song.title))
                    });
                    let histories: Vec<Arc<dyn soulbeet::ListeningHistory>> =
                        vec![Arc::new(filtered)];
                    soulbeet::engine::build_profile(&histories, knowledge.as_ref())
                        .await
                        .profile
                }
                Err(e) => {
                    warn!(
                        "Could not read Navidrome history for seasonal generation (user {}): {}",
                        user_id, e
                    );
                    UserMusicProfile::default()
                }
            }
        }
        Err(_) => UserMusicProfile::default(),
    };

    // Never re-download tracks already pending in discovery.
    if let Ok(pending_all) = DiscoveryTrackRow::pending_for_user_in(&crate::db::DB, user_id).await {
        for dt in pending_all {
            profile.known.add_track(&TrackRef {
                artist: ArtistRef {
                    name: dt.artist,
                    mbid: None,
                },
                title: dt.title,
                mbid: None,
            });
        }
    }

    // Auto-infer and persist culture profile when the user has no saved row yet.
    let culture: UserCultureProfile = match UserCultureProfileRow::get_in(&crate::db::DB, user_id)
        .await
    {
        Ok(Some(existing)) => existing,
        Ok(None) => {
            let inferred = infer_culture_profile(&profile, knowledge.as_ref(), None, Vec::new(), 0.45)
                .await;
            match UserCultureProfileRow::upsert_in(&crate::db::DB, user_id, &inferred).await {
                Ok(saved) => saved,
                Err(e) => {
                    warn!(
                        "Could not persist inferred culture profile for user {}: {}",
                        user_id, e
                    );
                    inferred
                }
            }
        }
        Err(e) => {
            warn!(
                "Could not load culture profile for user {}: {}",
                user_id, e
            );
            stored_culture
        }
    };

    // Exclusions: explicit rejections, deleted tracks, and search-miss kept-out tracks.
    let mut rejected_keys = DiscoveryRejectionRow::keys_for_user_in(&crate::db::DB, user_id)
        .await
        .map_err(|e| run_database_failure(Stage::Input, e))?;
    let mut tried_keys: std::collections::HashSet<String> = std::collections::HashSet::new();
    if let Ok(disliked) = DeletionReviewRow::disliked_keys(user_id).await {
        for k in disliked {
            tried_keys.insert(k);
        }
    }
    if let Ok(kept_out) = discovery_search_miss::kept_out_keys(user_id).await {
        for k in kept_out {
            tried_keys.insert(k);
        }
    }

    let download_base = crate::config::CONFIG.download_path().clone();
    let mut total_seasonal_imported = 0u32;

    for (def, state, needed) in work {
        if let Some(failure) = crate::importer_health::blocking_failure().await {
            warn!(
                "Seasonal pack '{}' for {} skipped: {}",
                def.slug, user_id, failure.message
            );
            return Err(failure);
        }

        let seasonal_dir = std::path::Path::new(&folder.discovery_path())
            .join("Seasonal")
            .join(&def.slug);
        tokio::fs::create_dir_all(&seasonal_dir).await.map_err(|e| {
            target_unwritable(
                &e,
                format!("Failed to create Discovery/Seasonal/{} dir: {}", def.slug, e),
            )
        })?;

        let steering = state
            .as_ref()
            .map(|s| CultureSteering::parse(&s.steering))
            .unwrap_or_default();
        let seasonal_expires_at = def
            .calendar_window
            .as_ref()
            .and_then(|w| w.cycle_dates(now.year(), now.month(), now.day()))
            .map(|(_, _, exp)| exp);

        let progress_label = seasonal_progress_label(&def);
        upsert_progress(
            user_id,
            &progress_label,
            shared::navidrome::ProfilePhase::GeneratingRecommendations,
            0,
            needed as u32,
        )
        .await;

        let overfetch_target = needed.saturating_mul(3).max(def.target_tracks as usize);
        let generator = ContextualCandidateGenerator::new(
            ContextSpec {
                definition: def.clone(),
                culture: culture.clone(),
                steering,
                target_count: overfetch_target,
            },
            knowledge.clone(),
            Some(pool.clone()),
            rejected_keys.clone(),
        )
        .with_metadata(metadata.clone());

        let output = generator
            .generate(&profile, &ProfileConfig::from_profile(DiscoveryProfile::Balanced))
            .await;
        let mut candidates = output.candidates.into_vec();
        candidates.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.artist.cmp(&b.artist))
                .then_with(|| a.track.cmp(&b.track))
        });

        if candidates.is_empty() {
            info!(
                "Seasonal pack '{}': generator returned 0 candidates for user {}",
                def.slug, user_id
            );
            upsert_progress(
                user_id,
                &progress_label,
                shared::navidrome::ProfilePhase::Done,
                0,
                needed as u32,
            )
            .await;
            continue;
        }

        info!(
            "Seasonal pack '{}': {} candidates ready, need {} tracks for user {}",
            def.slug,
            candidates.len(),
            needed,
            user_id
        );

        let profile_label = format!("Seasonal:{}", def.slug);
        let mut pack_imported = 0usize;
        let mut cand_cursor = 0usize;
        let mut searched_count = 0u32;

        for round in 0..2u32 {
            let remaining = needed.saturating_sub(pack_imported);
            if remaining == 0 || cand_cursor >= candidates.len() {
                break;
            }
            if let Some(failure) = crate::importer_health::blocking_failure().await {
                return Err(failure);
            }

            let queue_target = remaining + remaining.div_ceil(2);
            let mut queued: Vec<QueuedTrack> = Vec::new();

            while cand_cursor < candidates.len() && queued.len() < queue_target {
                let candidate = &candidates[cand_cursor];
                cand_cursor += 1;

                let s_key = seen_key(&candidate.artist, &candidate.track);
                let c_key = CandidateSet::key(&candidate.artist, &candidate.track);
                if tried_keys.contains(&s_key) || rejected_keys.contains(&c_key) {
                    continue;
                }

                searched_count += 1;
                upsert_progress(
                    user_id,
                    &progress_label,
                    shared::navidrome::ProfilePhase::SearchingSoulseek,
                    searched_count,
                    candidates.len() as u32,
                )
                .await;

                let searched_for = search_candidate(
                    &crate::db::DB,
                    searched,
                    user_id,
                    &candidate.artist,
                    &candidate.track,
                    candidate.album.as_deref(),
                )
                .await
                .map_err(|e| run_database_failure(Stage::SourceSearch, e))?;

                let mut ranked_items = match searched_for {
                    CandidateSearch::Found(items) => items,
                    CandidateSearch::Missed(_) | CandidateSearch::Failed(_) => {
                        tried_keys.insert(s_key);
                        continue;
                    }
                };
                if ranked_items.is_empty() {
                    tried_keys.insert(s_key);
                    continue;
                }

                ranked_items.truncate(failover::MAX_SOURCES_PER_TRACK);
                let track_batch = BatchRef::new(
                    TransferOwner {
                        user_id: UserId::new(user_id),
                        batch_id: uuid::Uuid::new_v4().to_string(),
                    },
                    format!("{} - {}", candidate.artist, candidate.track),
                    seasonal_dir.clone(),
                    soulbeet::ImportMode::Singleton,
                    searched.0.clone(),
                );
                track_batch.inbox.close();

                let reason_json = candidate
                    .reason
                    .as_ref()
                    .and_then(|r| serde_json::to_string(r).ok());
                let culture_bucket = candidate.culture_bucket.map(|b| b.as_str().to_string());

                let mut enqueued = false;
                for item in &ranked_items {
                    let transfer = transfer_key(backend.id(), &item.source, &item.id);
                    let claim = TransferRegistry::global()
                        .claim(std::slice::from_ref(&transfer), &track_batch);
                    if claim.claimed.is_empty() {
                        continue;
                    }
                    let request = backend.new_request();
                    let download_results =
                        match enqueue_claimed(backend, vec![item.clone()], &track_batch, &request)
                            .await
                        {
                            Ok(r) => r,
                            Err(_) => continue,
                        };
                    if let Some(dl) = download_results
                        .iter()
                        .find(|dl| dl.status == shared::download::EnqueueStatus::Enqueued)
                    {
                        tried_keys.insert(s_key.clone());
                        let attempt_idx = ranked_items
                            .iter()
                            .position(|i| i.source == item.source && i.id == item.id)
                            .unwrap_or(0);
                        queued.push(QueuedTrack {
                            artist: candidate.artist.clone(),
                            track: candidate.track.clone(),
                            album: candidate.album.clone(),
                            transfer_id: dl.id.clone(),
                            key: transfer,
                            batch: track_batch.clone(),
                            failing_since: None,
                            retry_pending_since: None,
                            pool: SourcePool::single(ranked_items[attempt_idx..].to_vec()),
                            abandoned: Vec::new(),
                            reason_json: reason_json.clone(),
                            culture_bucket: culture_bucket.clone(),
                        });
                        enqueued = true;
                        break;
                    }
                }
                if !enqueued {
                    tried_keys.insert(s_key);
                }
            }

            if queued.is_empty() {
                break;
            }

            info!(
                "Seasonal pack '{}': {} downloads queued (round {}), waiting for completion...",
                def.slug,
                queued.len(),
                round + 1
            );

            let wait_start = tokio::time::Instant::now();
            let max_wait = tokio::time::Duration::from_secs(600);
            let mut pending_ids: std::collections::HashSet<String> =
                queued.iter().map(|q| q.transfer_id.clone()).collect();
            let mut failed_ids: std::collections::HashSet<String> =
                std::collections::HashSet::new();

            upsert_progress(
                user_id,
                &progress_label,
                shared::navidrome::ProfilePhase::Downloading,
                0,
                queued.len() as u32,
            )
            .await;

            while !pending_ids.is_empty() && wait_start.elapsed() < max_wait {
                tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;
                let completed = (queued.len() as u32)
                    .saturating_sub(pending_ids.len() as u32 + failed_ids.len() as u32);
                upsert_progress(
                    user_id,
                    &progress_label,
                    shared::navidrome::ProfilePhase::Downloading,
                    completed,
                    queued.len() as u32,
                )
                .await;
                let mut sources: Vec<String> = Vec::new();
                for track in &queued {
                    if !sources.contains(&track.key.peer) {
                        sources.push(track.key.peer.clone());
                    }
                }
                let downloads = match backend.get_downloads_for(&sources).await {
                    Ok(d) => d,
                    Err(_) => continue,
                };
                settle_poll(
                    &mut queued,
                    &downloads,
                    &mut pending_ids,
                    &mut failed_ids,
                    backend,
                )
                .await;
            }
            stop_waiting(&queued, &mut pending_ids, backend).await;

            let skip_ids: std::collections::HashSet<&String> =
                pending_ids.iter().chain(failed_ids.iter()).collect();
            let importable_count = queued
                .iter()
                .filter(|q| !skip_ids.contains(&q.transfer_id))
                .count()
                .min(needed.saturating_sub(pack_imported)) as u32;
            let mut import_idx = 0u32;

            for qt in &queued {
                if skip_ids.contains(&qt.transfer_id) {
                    continue;
                }
                if pack_imported >= needed {
                    if let Ok(src) = crate::server_fns::download::process::resolve_on_disk(
                        &qt.key.filename,
                        &download_base,
                        Some(&qt.batch.destination),
                        &std::sync::Mutex::new(std::collections::HashSet::new()),
                    )
                    .await
                    {
                        discard_download(&src, &download_base).await;
                    }
                    TransferRegistry::global().release(&qt.key, &qt.batch.owner);
                    continue;
                }
                import_idx += 1;
                upsert_progress(
                    user_id,
                    &progress_label,
                    shared::navidrome::ProfilePhase::Importing,
                    import_idx,
                    importable_count,
                )
                .await;
                let resolved = crate::server_fns::download::process::resolve_on_disk(
                    &qt.key.filename,
                    &download_base,
                    Some(&qt.batch.destination),
                    &std::sync::Mutex::new(std::collections::HashSet::new()),
                )
                .await;
                let src = match resolved {
                    Ok(p) => p,
                    Err(_) => {
                        TransferRegistry::global().release(&qt.key, &qt.batch.owner);
                        continue;
                    }
                };

                let verify_path = src.clone();
                let verified = tokio::task::spawn_blocking(move || {
                    soulbeet::verify_downloaded_audio(
                        &verify_path,
                        soulbeet::VerifyPolicy::Standard,
                        None,
                    )
                })
                .await;
                match verified {
                    Ok(Ok(_)) => {}
                    Ok(Err(failure)) => {
                        warn!(
                            "Seasonal post-download verification failed for '{}' - {} ({}): {}",
                            qt.artist,
                            qt.track,
                            failure.code.as_str(),
                            failure.message
                        );
                        discard_download(&src, &download_base).await;
                        TransferRegistry::global().release(&qt.key, &qt.batch.owner);
                        continue;
                    }
                    Err(join_err) => {
                        warn!(
                            "Seasonal post-download verification task failed for '{}' - {}: {}",
                            qt.artist, qt.track, join_err
                        );
                        discard_download(&src, &download_base).await;
                        TransferRegistry::global().release(&qt.key, &qt.batch.owner);
                        continue;
                    }
                }

                let outcome = IMPORT_QUEUE
                    .submit(ImportJob {
                        sources: vec![src.clone()],
                        files: vec![src.clone()],
                        target: seasonal_dir.clone(),
                        mode: soulbeet::ImportMode::Singleton,
                        owner: None,
                        snapshot_target: true,
                        cleanup_root: Some(download_base.clone()),
                        choice: soulbeet::DriverChoice::Auto,
                        choices: None,
                        duplicates: Duplicates::AsConfigured,
                        last_run: None,
                        lock_retries: 0,
                    })
                    .await
                    .wait()
                    .await;
                TransferRegistry::global().release(&qt.key, &qt.batch.owner);

                match classify_discovery(&outcome.result.outcome) {
                    DiscoveryImport::Imported => {
                        info!(
                            "Imported seasonal '{}' - {} into Discovery/Seasonal/{}",
                            qt.artist, qt.track, def.slug
                        );
                    }
                    DiscoveryImport::Permanent | DiscoveryImport::Failed => {
                        warn!(
                            "Seasonal beets import skipped/failed for '{}' - {}: {}",
                            qt.artist, qt.track, outcome.result
                        );
                        discard_download(&src, &download_base).await;
                        continue;
                    }
                }

                let new_files = &outcome.new_files;
                if new_files.is_empty() {
                    continue;
                }
                for rel in new_files {
                    DiscoveryTrackRow::create_with_context(
                        None,
                        &qt.track,
                        &qt.artist,
                        qt.album.as_deref().unwrap_or(""),
                        &seasonal_dir.join(rel).to_string_lossy(),
                        &folder.id,
                        &profile_label,
                        DiscoveryTrackContext {
                            exclude_from_core_taste: def.exclude_from_core_taste,
                            reason_json: qt.reason_json.as_deref(),
                            culture_bucket: qt.culture_bucket.as_deref(),
                            pack_slug: Some(def.slug.as_str()),
                        },
                    )
                    .await
                    .map_err(|e| run_database_failure(Stage::Import, e))?;
                }
                let _ =
                    DiscoveryHistoryRow::record(user_id, &qt.artist, &qt.track, &profile_label)
                        .await;
                rejected_keys.insert(CandidateSet::key(&qt.artist, &qt.track));
                pack_imported += 1;
                total_seasonal_imported += 1;
            }
        }

        upsert_progress(
            user_id,
            &progress_label,
            shared::navidrome::ProfilePhase::Done,
            pack_imported as u32,
            needed as u32,
        )
        .await;

        if pack_imported > 0 {
            if let Err(e) = ContextualMixStateRow::upsert_playlist_id_in(
                &crate::db::DB,
                user_id,
                &def.slug,
                state.as_ref().and_then(|s| s.navidrome_playlist_id.as_deref()),
                seasonal_expires_at.as_deref(),
            )
            .await
            {
                warn!(
                    "Could not persist seasonal expiry metadata for '{}' (user {}): {}",
                    def.slug, user_id, e
                );
            }
        }
    }

    Ok(total_seasonal_imported)
}

/// What a run returns once its imports are scanned. A broken importer fails it even after
/// imports, since the user must fix it. A failed engine fails it only when nothing was
/// imported, so a run that imported tracks still reports them.
#[cfg(feature = "server")]
fn run_outcome(
    result: shared::navidrome::GenerationResult,
    importer_issue: Option<Failure>,
    engine_issue: Option<Failure>,
) -> Result<shared::navidrome::GenerationResult, Failure> {
    if let Some(issue) = importer_issue {
        return Err(issue);
    }
    match engine_issue {
        Some(issue) if result.total_imported == 0 => Err(issue),
        _ => Ok(result),
    }
}

/// Starts a Navidrome scan of `imported` new tracks for `user`, whose client
/// is `navi`, with scan::start_scan_for and the linked accounts `navidrome`
/// gives, and waits up to two minutes for it to end. Returns whether the scan
/// was started.
///
/// The scan runs in a goroutine and startScan answers at once, so "not
/// scanning" alone can be read before it begins: the wait is for a scan that
/// ended after the one recorded before it. The scan status is the server's,
/// so `navi` reads the end of a scan another account started. When that
/// cannot be read the scan is still started, only not waited for. With no
/// linked account that may start one, the tracks wait for Navidrome's own
/// scan.
#[cfg(feature = "server")]
async fn scan_imports(
    db: &sqlx::SqlitePool,
    navidrome: &dyn crate::core::index::NavidromeOf,
    user: &UserId,
    navi: &Arc<soulbeet::navidrome::NavidromeClient>,
    imported: u32,
) -> bool {
    use crate::core::scan::{start_scan_for, ScanStart, SCAN_ROLES};

    let before = match navi.get_scan_status().await {
        Ok(status) => Some(status.last_scan),
        Err(e) => {
            warn!("Failed to read Navidrome scan status, not waiting for the scan: {e}");
            None
        }
    };
    match start_scan_for(db, navidrome, &SCAN_ROLES, user, navi).await {
        Ok(ScanStart::Started) => {}
        Ok(ScanStart::NoAccount) => {
            debug!(
                "No Navidrome account linked to Soulbeet may start a scan: {imported} discovery imports appear at Navidrome's next own scan"
            );
            return false;
        }
        Err(e) => {
            warn!("Failed to trigger Navidrome scan: {}", e);
            return false;
        }
    }
    info!(
        "Triggered Navidrome library scan after {} discovery imports",
        imported
    );
    let Some(before) = before else {
        return true;
    };
    // Poll every 3s, at most 2 min
    for _ in 0..40u32 {
        tokio::time::sleep(tokio::time::Duration::from_secs(3)).await;
        match navi.get_scan_status().await {
            Ok(status) if status.ended_after(before) => {
                if !status.error.is_empty() {
                    warn!("Navidrome scan reported an error: {}", status.error);
                }
                info!("Navidrome scan ended after {} items", status.count);
                break;
            }
            Ok(_) => continue,
            Err(_) => break,
        }
    }
    true
}

/// Create or update smart playlists in Navidrome for each discovery profile.
///
/// Uses Navidrome's native API to create smart playlists of the songs in this
/// user's Discovery/{profile}/ directory, in its library. The playlist
/// auto-updates as Navidrome scans new files. Owned by the authenticated user.
#[cfg(feature = "server")]
pub async fn reconcile_discovery_playlists(user_id: &str) -> Result<(), String> {
    let user = UserId::new(user_id);
    let music_path = crate::config::CONFIG.navidrome_music_path();
    reconcile_in(
        &crate::db::DB,
        &crate::core::index::ServerNavidrome,
        music_path,
        &user,
    )
    .await?;

    // Also reconcile active seasonal packs as Navidrome smart playlists (`Discovery/Seasonal/<slug>/`)
    // and clean up expired seasonal playlists.
    if let Ok(settings) = UserSettings::get_in(&crate::db::DB, user_id).await {
        if settings.discovery_enabled {
            if let Some(ref folder_id) = settings.discovery_folder_id {
                if let Ok(Some(folder)) = Folder::get_by_id_in(&crate::db::DB, folder_id).await {
                    if let Ok(navi) = crate::services::navidrome_client_for_user(user_id).await {
                        if let Ok(Some(discovery)) = crate::core::discovery::discovery_place_in(
                            &crate::db::DB,
                            &navi,
                            music_path,
                            &folder,
                        )
                        .await
                        {
                            if let Err(e) = crate::core::contextual::reconcile_seasonal_playlists_in(
                                &crate::db::DB,
                                &navi,
                                user_id,
                                &discovery,
                                chrono::Utc::now(),
                            )
                            .await
                            {
                                warn!(
                                    "Failed to reconcile seasonal playlists for user {}: {}",
                                    user_id, e
                                );
                            }
                        }
                    }
                }
            }
        }
    }

    Ok(())
}

/// reconcile_discovery_playlists on `db` and `navidrome`, `music_path` the root of Navidrome's
/// default library here (NAVIDROME_MUSIC_PATH). Each playlist is changed in place, so its id
/// stays the one clients hold. The error names every profile that failed.
#[cfg(feature = "server")]
async fn reconcile_in(
    db: &sqlx::SqlitePool,
    navidrome: &dyn crate::core::index::NavidromeOf,
    music_path: Option<&std::path::Path>,
    user: &UserId,
) -> Result<(), String> {
    use crate::core::playlists::{discovery_key, lock};

    let user_id = user.to_string();
    // Taken before the settings are read, so a run waiting for it reads the ids the other stored.
    let _reconciling = lock(&discovery_key(&user_id)).await;
    let settings = UserSettings::get_in(db, &user_id).await?;
    if !settings.discovery_enabled {
        return Ok(());
    }
    let folder_id = match settings.discovery_folder_id.as_ref() {
        Some(id) => id.clone(),
        None => return Ok(()),
    };
    let folder = Folder::get_by_id_in(db, &folder_id)
        .await?
        .ok_or("Discovery folder not found")?;

    let navi = match navidrome.navidrome_of(user).await {
        Ok(c) => c,
        Err(e) => {
            info!(
                "Playlist reconciliation skipped (no Navidrome client): {}",
                e
            );
            return Ok(());
        }
    };

    // Found before any playlist is touched, so an unknown place leaves them as they are.
    let discovery = crate::core::discovery::discovery_place_in(db, &navi, music_path, &folder)
        .await
        .map_err(|f| crate::core::logged(&f))?;
    let Some(discovery) = discovery else {
        info!(
            "Playlist reconciliation skipped: no Navidrome library Soulbeet knows holds {}",
            folder.discovery_path()
        );
        return Ok(());
    };

    let selected_profiles = parse_profiles(&settings.discovery_profiles);
    let mut failed = Vec::new();

    for profile in &selected_profiles {
        let profile_name = profile.to_string();
        let stored_id = UserSettings::get_playlist_id_for_profile(
            &settings.discovery_navidrome_playlist_id,
            &profile_name,
        );

        let playlist_name = UserSettings::get_playlist_name_for_profile(
            &settings.discovery_playlist_name,
            &profile_name,
        );

        let place = discovery.child(&profile_name);
        let comment = format!("Soulbeet discovery ({})", profile_name);

        let reconciled = reconcile_playlist(
            &navi,
            stored_id.as_deref(),
            &playlist_name,
            &comment,
            &place,
        )
        .await;
        match reconciled {
            Ok(Reconciled::Created(playlist_id)) => {
                if let Err(e) = UserSettings::update_discovery_playlist_id_in(
                    db,
                    &user_id,
                    &profile_name,
                    &playlist_id,
                )
                .await
                {
                    // An id not stored makes the next run create the playlist again.
                    let undone = match navi.delete_smart_playlist(&playlist_id).await {
                        Ok(()) => "deleted".to_string(),
                        Err(d) => format!("left in Navidrome: {d}"),
                    };
                    failed.push(format!(
                        "{profile_name}: id {playlist_id} not saved ({e}), playlist {undone}"
                    ));
                    continue;
                }
                info!(
                    "Created smart playlist '{}' (library {}, path filter: {}) for user {}",
                    playlist_name,
                    place.library_id,
                    place.song_prefix(),
                    user_id
                );
            }
            Ok(Reconciled::Updated) => info!(
                "Updated smart playlist '{}' (library {}, path filter: {}) for user {}",
                playlist_name,
                place.library_id,
                place.song_prefix(),
                user_id
            ),
            Ok(Reconciled::Unchanged) => {}
            Err(e) => {
                warn!(
                    "Failed to reconcile smart playlist '{}': {}",
                    playlist_name, e
                );
                failed.push(format!("{profile_name}: {e}"));
            }
        }
    }

    if failed.is_empty() {
        Ok(())
    } else {
        Err(failed.join("; "))
    }
}

/// What reconcile did to one profile's smart playlist.
#[cfg(feature = "server")]
enum Reconciled {
    /// Navidrome had none by the stored id, or none was stored: made with this id.
    Created(String),
    Updated,
    Unchanged,
}

/// Brings the smart playlist stored as `stored_id` to `name` and the rules of the songs in
/// `place`, in place, so its id, comment and public flag stay. A playlist is made only when Navidrome says it
/// has none by that id: after a failed lookup the old one may still be there and would be left
/// beside the new one.
#[cfg(feature = "server")]
async fn reconcile_playlist(
    navi: &soulbeet::navidrome::NavidromeClient,
    stored_id: Option<&str>,
    name: &str,
    comment: &str,
    place: &crate::core::library::NavidromePlace,
) -> Result<Reconciled, soulbeet::NavidromeError> {
    use soulbeet::navidrome::{filepath_rules, SmartPlaylistChange};

    let prefix = place.song_prefix();
    let existing = match stored_id {
        Some(id) => navi.get_native_playlist(id).await?,
        None => None,
    };
    let Some(playlist) = existing else {
        let id = navi
            .create_smart_playlist(name, comment, place.library_id, &prefix)
            .await?;
        return Ok(Reconciled::Created(id));
    };
    let rules = filepath_rules(place.library_id, &prefix);
    let change = SmartPlaylistChange {
        name: (playlist.name != name).then_some(name),
        rules: (playlist.rules.as_ref() != Some(&rules)).then_some(&rules),
    };
    if change.name.is_none() && change.rules.is_none() {
        return Ok(Reconciled::Unchanged);
    }
    navi.update_smart_playlist(&playlist.id, &change).await?;
    Ok(Reconciled::Updated)
}

#[post("/api/discovery/generate-recommendations", auth: AuthSession)]
pub async fn generate_recommendations() -> Result<u32, ServerFnError> {
    generate_recommendations_internal(&auth.0.user_id)
        .await
        .map_err(failure_error)
}

/// How a discovery import counts. Config and runtime failures must not land
/// in the history, or the track could never be suggested again.
#[cfg(feature = "server")]
#[derive(Debug, PartialEq)]
enum DiscoveryImport {
    Imported,
    /// The library cannot take this track: recorded so it is not suggested again.
    Permanent,
    Failed,
}

#[cfg(feature = "server")]
fn classify_discovery(outcome: &soulbeet::ImportOutcome) -> DiscoveryImport {
    use soulbeet::{ImportOutcome, SkipReason};
    match outcome {
        ImportOutcome::Imported { .. } => DiscoveryImport::Imported,
        ImportOutcome::Skipped(
            SkipReason::NoMatch { .. } | SkipReason::AlreadyInLibrary { .. },
        ) => DiscoveryImport::Permanent,
        ImportOutcome::Skipped(SkipReason::NothingToImport)
        | ImportOutcome::LookupFailed { .. }
        | ImportOutcome::Failed { .. }
        | ImportOutcome::TimedOut { .. }
        | ImportOutcome::Refused { .. } => DiscoveryImport::Failed,
    }
}

/// Deletes a download discovery could not import (it has no retry) and
/// prunes the folder it leaves empty, logging what it could not delete.
#[cfg(feature = "server")]
async fn discard_download(src: &std::path::Path, download_base: &std::path::Path) {
    let files = [src.to_path_buf()];
    for (path, e) in crate::core::acquire::discard_files(&files, download_base).await {
        tracing::warn!(
            "Could not delete the discovery download {}: {e}",
            path.display()
        );
    }
}

/// Every discovery profile's candidates for the user, from one profile build.
#[cfg(feature = "server")]
pub async fn generate_recommendations_internal(user_id: &str) -> Result<u32, Failure> {
    use crate::core::index::ServerNavidrome;
    use crate::core::recommend::{generate_in, DiscoveryServices};
    use shared::recommendation::DiscoveryProfile;

    generate_in(
        &crate::db::DB,
        &ServerNavidrome,
        &DiscoveryServices::live(),
        &UserId::new(user_id),
        &DiscoveryProfile::ALL,
    )
    .await
}

#[cfg(feature = "server")]
pub async fn generate_recommendations_for_user(
    user_id: &str,
    discovery_profile: shared::recommendation::DiscoveryProfile,
) -> Result<u32, Failure> {
    use crate::core::index::ServerNavidrome;
    use crate::core::recommend::{generate_in, DiscoveryServices};

    generate_in(
        &crate::db::DB,
        &ServerNavidrome,
        &DiscoveryServices::live(),
        &UserId::new(user_id),
        &[discovery_profile],
    )
    .await
}

#[get("/api/discovery/report", auth: AuthSession)]
pub async fn get_engine_report() -> Result<String, ServerFnError> {
    #[cfg(feature = "server")]
    {
        use crate::models::engine_report::EngineReportRow;
        // Try new history table first, fall back to legacy user_profiles.last_report
        let rows = EngineReportRow::get_history(&auth.0.user_id, 1)
            .await
            .map_err(server_error)?;
        if let Some(row) = rows.into_iter().next() {
            let report: shared::recommendation::EngineReport =
                serde_json::from_str(&row.report_json).map_err(|e| server_error(e.to_string()))?;
            return Ok(report.to_text());
        }
        // Legacy fallback
        use crate::models::user_profile::UserProfileRow;
        let report_json = UserProfileRow::get_report(&auth.0.user_id)
            .await
            .map_err(server_error)?;
        match report_json {
            Some(json) => {
                let report: shared::recommendation::EngineReport =
                    serde_json::from_str(&json).map_err(|e| server_error(e.to_string()))?;
                Ok(report.to_text())
            }
            None => Ok("No engine report available. Run discovery generation first.".to_string()),
        }
    }
    #[cfg(not(feature = "server"))]
    Ok(String::new())
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct ReportEntry {
    pub profile: String,
    pub candidate_count: u32,
    pub created_at: String,
    pub text: String,
}

#[get("/api/discovery/reports", auth: AuthSession)]
pub async fn get_engine_reports() -> Result<Vec<ReportEntry>, ServerFnError> {
    #[cfg(feature = "server")]
    {
        use crate::models::engine_report::EngineReportRow;
        let rows = EngineReportRow::get_history(&auth.0.user_id, 30)
            .await
            .map_err(server_error)?;
        let mut entries = Vec::new();
        for row in rows {
            let text = match serde_json::from_str::<shared::recommendation::EngineReport>(
                &row.report_json,
            ) {
                Ok(report) => report.to_text(),
                Err(_) => "Failed to parse report".to_string(),
            };
            entries.push(ReportEntry {
                profile: row.profile,
                candidate_count: row.candidate_count as u32,
                created_at: row.created_at,
                text,
            });
        }
        Ok(entries)
    }
    #[cfg(not(feature = "server"))]
    Ok(Vec::new())
}

#[cfg(all(test, feature = "server"))]
mod tests {
    use super::{
        discovery_folder_not_found, run_database_failure, run_outcome, scan_imports,
        search_candidate, settle_poll, stop_waiting, CandidateSearch, QueuedTrack,
        FAILED_STATE_CONFIRM,
    };
    use crate::core::discovery::{
        discovery_disabled, discovery_folder_required, generation_running,
    };
    use crate::core::sources::Searched;
    use crate::models::discovery_history::seen_key;
    use crate::models::discovery_search_miss::{
        kept_out_keys_in, record_in, SEARCH_MISSES_KEPT_OUT,
    };
    use crate::security::SEEDED_ADMIN_ID;
    use crate::server_fns::download::enqueue::transfer_key;
    use crate::server_fns::download::failover::SourcePool;
    use crate::server_fns::download::monitor::{CANCEL_SETTLE, RETRY_PENDING_FOR};
    use crate::server_fns::download::test_support::{
        batch, group, item, search_result, transfer, Cancel, ScriptedOutcome, StubBackend,
    };
    use crate::transfer_registry::TransferRegistry;
    use shared::download::{DownloadProgress, DownloadState, DownloadableItem, SearchState};
    use shared::failure::{Failure, FailureCode, RetryAdvice, Stage, UserActionCode};
    use soulbeet::error::SoulseekError;
    use soulbeet::DownloadBackend;
    use std::collections::HashSet;
    use std::sync::Arc;

    fn searched(stub: &Arc<StubBackend>) -> Searched {
        (stub.id().to_string(), stub.clone())
    }

    async fn kept_out(db: &sqlx::SqlitePool) -> HashSet<String> {
        kept_out_keys_in(db, SEEDED_ADMIN_ID).await.unwrap()
    }

    /// The Soulseek server drops every search for MGMT's Kids, wildcards included.
    #[tokio::test]
    async fn a_track_whose_search_keeps_finding_nothing_is_kept_out_but_not_for_a_failed_one() {
        let db = crate::db::test_db::migrated().await;
        let stub = StubBackend::new(vec![]);
        stub.script_polls(vec![Err(SoulseekError::SearchGone {
            body: String::new(),
        })]);
        let failed = search_candidate(&db, &searched(&stub), SEEDED_ADMIN_ID, "MGMT", "Kids", None)
            .await
            .unwrap();
        assert!(
            matches!(&failed, CandidateSearch::Failed(f) if f.code == FailureCode::SearchLost),
            "{failed:?}"
        );
        for _ in 1..SEARCH_MISSES_KEPT_OUT {
            record_in(&db, SEEDED_ADMIN_ID, "MGMT", "Kids")
                .await
                .unwrap();
        }
        assert!(kept_out(&db).await.is_empty(), "the failed search counted");

        stub.script_polls(vec![Ok(search_result(
            "search",
            SearchState::Completed,
            vec![],
        ))]);
        let missed = search_candidate(&db, &searched(&stub), SEEDED_ADMIN_ID, "MGMT", "Kids", None)
            .await
            .unwrap();
        assert!(
            matches!(&missed, CandidateSearch::Missed(f) if f.code == FailureCode::SearchNoResults),
            "{missed:?}"
        );
        assert_eq!(
            kept_out(&db).await,
            HashSet::from([seen_key("MGMT", "Kids")])
        );
    }

    #[tokio::test]
    async fn a_search_that_finds_the_track_answers_its_items_and_starts_its_count_again() {
        let db = crate::db::test_db::migrated().await;
        for _ in 1..SEARCH_MISSES_KEPT_OUT {
            record_in(&db, SEEDED_ADMIN_ID, "MGMT", "Kids")
                .await
                .unwrap();
        }
        let stub = StubBackend::new(vec![]);
        let kids = item(stub.id(), "Kids", "kids-1");
        stub.script_polls(vec![Ok(search_result(
            "search",
            SearchState::Completed,
            vec![group(stub.id(), vec![kids.clone()], 0.9)],
        ))]);
        let found = search_candidate(
            &db,
            &searched(&stub),
            SEEDED_ADMIN_ID,
            "MGMT",
            "Kids",
            Some("Oracular Spectacular"),
        )
        .await
        .unwrap();
        assert!(
            matches!(&found, CandidateSearch::Found(items) if *items == [kids.clone()]),
            "{found:?}"
        );
        let specs = stub.specs();
        let wanted = &specs[0].tracks[0];
        assert_eq!(
            (
                wanted.artist.as_str(),
                wanted.title.as_str(),
                wanted.album_title.as_deref()
            ),
            ("MGMT", "Kids", Some("Oracular Spectacular"))
        );
        record_in(&db, SEEDED_ADMIN_ID, "MGMT", "Kids")
            .await
            .unwrap();
        assert!(
            kept_out(&db).await.is_empty(),
            "the misses before the find still count"
        );
    }

    #[test]
    fn discovery_refusals_keep_their_text() {
        use FailureCode::*;
        use RetryAdvice::*;
        for (f, code, retry, message) in [
            (
                generation_running(),
                AlreadyRunning,
                NotRetryable,
                "Discovery generation already in progress",
            ),
            (
                discovery_disabled(),
                InvalidInput,
                NotRetryable,
                "Discovery is not enabled",
            ),
            (
                discovery_folder_required(),
                FolderRequired,
                NeedsUser,
                "No download folder configured for discovery",
            ),
            (
                discovery_folder_not_found(),
                FolderRequired,
                NeedsUser,
                "Discovery folder not found",
            ),
        ] {
            assert_eq!(
                (f.code, f.stage, f.retry, f.message.as_str()),
                (code, Stage::Input, retry, message)
            );
        }
        for f in [discovery_folder_required(), discovery_folder_not_found()] {
            let action = f.user_action.unwrap();
            assert_eq!(
                (action.code, action.what.as_str(), action.where_.as_str()),
                (
                    UserActionCode::AddFolder,
                    "Choose a discovery folder",
                    "Settings > Library > Discovery"
                )
            );
        }
    }

    #[test]
    fn an_engine_failure_fails_the_run_only_when_nothing_was_imported() {
        use crate::core::recommend::no_listening_data;
        use shared::navidrome::{GenerationResult, ProfileGenerationStats};

        let imported = GenerationResult {
            total_imported: 10,
            profiles: vec![ProfileGenerationStats {
                profile: "Conservative".into(),
                imports_succeeded: 10,
                ..Default::default()
            }],
        };
        let importer = Failure::new(
            FailureCode::ImporterBroken,
            Stage::Import,
            RetryAdvice::NeedsUser,
            "beets cannot start",
        );
        assert_eq!(
            run_outcome(imported.clone(), None, Some(no_listening_data())),
            Ok(imported.clone())
        );
        assert_eq!(
            run_outcome(GenerationResult::default(), None, Some(no_listening_data())),
            Err(no_listening_data())
        );
        assert_eq!(
            run_outcome(
                imported.clone(),
                Some(importer.clone()),
                Some(no_listening_data())
            ),
            Err(importer)
        );
        assert_eq!(run_outcome(imported.clone(), None, None), Ok(imported));
    }

    #[test]
    fn a_run_database_failure_keeps_the_database_text_at_its_stage() {
        use shared::failure::{Cause, Service};
        let raw = "error returned from database: (code: 5) database is locked";
        let f = run_database_failure(Stage::SourceSearch, raw.to_string());
        assert_eq!(
            (f.code, f.stage, f.retry, f.message.as_str()),
            (
                FailureCode::SettingsUnreadable,
                Stage::SourceSearch,
                RetryAdvice::SameSourceLater,
                raw
            )
        );
        assert_eq!(f.cause, Some(Cause::new(Service::Database, None, raw)));
    }

    #[test]
    fn classify_discovery_keeps_config_failures_out_of_history() {
        use super::{classify_discovery, DiscoveryImport};
        use soulbeet::{ImportOutcome, SkipReason, TimeoutKind};
        use std::time::Duration;

        let cases = [
            (
                ImportOutcome::Imported {
                    untagged: false,
                    warnings: vec![],
                    items: vec![],
                    failure: None,
                },
                DiscoveryImport::Imported,
            ),
            (
                ImportOutcome::Skipped(SkipReason::NoMatch { candidates: None }),
                DiscoveryImport::Permanent,
            ),
            (
                ImportOutcome::Skipped(SkipReason::AlreadyInLibrary { existing: vec![] }),
                DiscoveryImport::Permanent,
            ),
            (
                ImportOutcome::Skipped(SkipReason::NothingToImport),
                DiscoveryImport::Failed,
            ),
            (
                ImportOutcome::LookupFailed {
                    source: "MusicBrainz".into(),
                    message: "timed out".into(),
                },
                DiscoveryImport::Failed,
            ),
            (
                ImportOutcome::Failed {
                    kind: soulbeet::FailKind::Beets,
                    message: "beets exited with code 1".into(),
                },
                DiscoveryImport::Failed,
            ),
            (
                ImportOutcome::TimedOut {
                    limit: TimeoutKind::Idle(Duration::from_secs(180)),
                    last_line: None,
                },
                DiscoveryImport::Failed,
            ),
            (
                ImportOutcome::Refused {
                    issue: "No metadata source plugin loaded".into(),
                },
                DiscoveryImport::Failed,
            ),
        ];
        for (outcome, expected) in cases {
            assert_eq!(classify_discovery(&outcome), expected, "{outcome:?}");
        }
    }

    /// A track waiting on `items[0]`, with the rest as its alternates. Its
    /// transfer id is the one `StubBackend` would hand out, `t-<file>`.
    fn queued_track(
        items: Vec<DownloadableItem>,
        backend: &Arc<dyn DownloadBackend>,
    ) -> QueuedTrack {
        let batch = batch("Kowloon - Wake Up");
        let key = transfer_key(backend.id(), &items[0].source, &items[0].id);
        TransferRegistry::global().claim(std::slice::from_ref(&key), &batch);
        QueuedTrack {
            artist: "Kowloon".to_string(),
            track: "Wake Up".to_string(),
            album: Some("Come Over".to_string()),
            transfer_id: format!("t-{}", items[0].id),
            key,
            batch,
            failing_since: None,
            retry_pending_since: None,
            pool: SourcePool::single(items),
            abandoned: Vec::new(),
            reason_json: None,
            culture_bucket: None,
        }
    }

    /// Backdate a track's failure so it has already outlived the confirmation
    /// window, letting a single poll act on it without sleeping through it.
    fn already_confirmed_failing(qt: &mut QueuedTrack) {
        qt.failing_since = Some(tokio::time::Instant::from_std(
            std::time::Instant::now()
                .checked_sub(FAILED_STATE_CONFIRM + std::time::Duration::from_secs(1))
                .expect("monotonic clock younger than the confirmation window"),
        ));
    }

    fn failed(source: &str, path: &str) -> DownloadProgress {
        transfer(
            source,
            path,
            &format!("t-{path}"),
            DownloadState::Failed("Transfer rejected".to_string()),
        )
    }

    fn live(source: &str, path: &str) -> DownloadProgress {
        transfer(source, path, &format!("t-{path}"), DownloadState::Queued)
    }

    fn completed(source: &str, path: &str) -> DownloadProgress {
        transfer(source, path, &format!("t-{path}"), DownloadState::Completed)
    }

    fn pending_of(queued: &[QueuedTrack]) -> HashSet<String> {
        queued.iter().map(|q| q.transfer_id.clone()).collect()
    }

    fn stub() -> Arc<dyn DownloadBackend> {
        StubBackend::new(vec![ScriptedOutcome::Enqueued])
    }

    #[tokio::test]
    async fn a_retry_moves_the_whole_identity_to_the_new_peer() {
        let backend = stub();
        let mut queued = vec![queued_track(
            vec![
                item("peer_a", "Wake Up", "a/07 - Wake Up.flac"),
                item("peer_b", "Wake Up", "b/07 - Wake Up.flac"),
            ],
            &backend,
        )];
        let dead = queued[0].key.clone();
        let mut pending = pending_of(&queued);
        let mut failed_ids = HashSet::new();
        already_confirmed_failing(&mut queued[0]);

        settle_poll(
            &mut queued,
            &[failed("peer_a", "a/07 - Wake Up.flac")],
            &mut pending,
            &mut failed_ids,
            &backend,
        )
        .await;

        // Peer, path and transfer id identify the transfer together: leaving
        // any of them on the dead peer would lose sight of the retry.
        assert_eq!(
            queued[0].key,
            transfer_key(backend.id(), "peer_b", "b/07 - Wake Up.flac")
        );
        assert_eq!(queued[0].transfer_id, "t-b/07 - Wake Up.flac");
        assert_eq!(pending, pending_of(&queued));
        assert!(failed_ids.is_empty());
        let other = batch("Y");
        assert!(
            TransferRegistry::global().may_adopt(&dead, &other.owner),
            "the dead peer's key is free again"
        );
        assert!(
            !TransferRegistry::global().may_adopt(&queued[0].key, &other.owner),
            "the retry's key is the track's"
        );
    }

    #[tokio::test]
    async fn a_dead_transfer_listed_first_does_not_shadow_the_live_one() {
        // One peer holding the track in two directories is two ranked items,
        // so an alternate can be the peer that just failed, and both its
        // transfers are listed for that peer.
        let backend = stub();
        let mut queued = vec![queued_track(
            vec![
                item("peer_a", "Wake Up", "b/07 - Wake Up.flac"),
                item("peer_a", "Wake Up", "c/07 - Wake Up.flac"),
            ],
            &backend,
        )];
        let mut pending = pending_of(&queued);
        let mut failed_ids = HashSet::new();
        already_confirmed_failing(&mut queued[0]);
        let downloads = vec![
            failed("peer_a", "a/07 - Wake Up.flac"),
            live("peer_a", "b/07 - Wake Up.flac"),
        ];

        settle_poll(
            &mut queued,
            &downloads,
            &mut pending,
            &mut failed_ids,
            &backend,
        )
        .await;

        assert_eq!(queued[0].transfer_id, "t-b/07 - Wake Up.flac");
        assert_eq!(pending, pending_of(&queued));
        assert!(failed_ids.is_empty());
        assert_eq!(
            queued[0].pool.attempts_used(0),
            1,
            "the live transfer must not cost the track a source"
        );
    }

    #[tokio::test]
    async fn a_completed_retry_is_read_past_the_dead_transfer_it_replaced() {
        let backend = stub();
        let mut queued = vec![queued_track(
            vec![
                item("peer_a", "Wake Up", "b/07 - Wake Up.flac"),
                item("peer_a", "Wake Up", "c/07 - Wake Up.flac"),
            ],
            &backend,
        )];
        let mut pending = pending_of(&queued);
        let mut failed_ids = HashSet::new();
        already_confirmed_failing(&mut queued[0]);
        let downloads = vec![
            failed("peer_a", "a/07 - Wake Up.flac"),
            completed("peer_a", "b/07 - Wake Up.flac"),
        ];

        settle_poll(
            &mut queued,
            &downloads,
            &mut pending,
            &mut failed_ids,
            &backend,
        )
        .await;

        assert!(pending.is_empty(), "the track settled as done");
        assert!(failed_ids.is_empty());
        assert_eq!(queued[0].transfer_id, "t-b/07 - Wake Up.flac");
        assert_eq!(queued[0].pool.attempts_used(0), 1);
    }

    #[tokio::test]
    async fn settle_poll_ignores_a_same_name_transfer_with_another_id() {
        // slskd keeps records of earlier downloads of the same file from the
        // same peer, say an earlier discovery run's. Neither settles this
        // track: a finished one would send the import looking for a file this
        // transfer never wrote, and a failed one would cost it a peer.
        let backend = stub();
        let mut queued = vec![queued_track(
            vec![
                item("peer_a", "Wake Up", "a/07 - Wake Up.flac"),
                item("peer_b", "Wake Up", "b/07 - Wake Up.flac"),
            ],
            &backend,
        )];
        let mut pending = pending_of(&queued);
        let mut failed_ids = HashSet::new();
        already_confirmed_failing(&mut queued[0]);
        let downloads = vec![
            transfer(
                "peer_a",
                "a/07 - Wake Up.flac",
                "t-earlier",
                DownloadState::Completed,
            ),
            transfer(
                "peer_a",
                "a/07 - Wake Up.flac",
                "t-older",
                DownloadState::Failed("Transfer rejected".to_string()),
            ),
        ];

        settle_poll(
            &mut queued,
            &downloads,
            &mut pending,
            &mut failed_ids,
            &backend,
        )
        .await;

        assert_eq!(pending, pending_of(&queued), "still waiting on its own id");
        assert!(failed_ids.is_empty());
        assert_eq!(queued[0].transfer_id, "t-a/07 - Wake Up.flac");
        assert_eq!(queued[0].pool.attempts_used(0), 1);
    }

    #[tokio::test]
    async fn a_track_that_runs_out_of_peers_is_failed_exactly_once() {
        let backend = stub();
        let mut queued = vec![queued_track(
            vec![
                item("peer_a", "Wake Up", "a/07 - Wake Up.flac"),
                item("peer_b", "Wake Up", "b/07 - Wake Up.flac"),
            ],
            &backend,
        )];
        let mut pending = pending_of(&queued);
        let mut failed_ids = HashSet::new();
        already_confirmed_failing(&mut queued[0]);

        // The first peer dies and the track moves to its only alternate.
        settle_poll(
            &mut queued,
            &[failed("peer_a", "a/07 - Wake Up.flac")],
            &mut pending,
            &mut failed_ids,
            &backend,
        )
        .await;
        assert!(failed_ids.is_empty());

        // The alternate dies too, and there is nothing left to try. The dead
        // first peer is still in slskd's list and must not be counted as a
        // second loss: stats.downloads_failed adds the failed set's size.
        already_confirmed_failing(&mut queued[0]);
        let downloads = vec![
            failed("peer_a", "a/07 - Wake Up.flac"),
            failed("peer_b", "b/07 - Wake Up.flac"),
        ];
        settle_poll(
            &mut queued,
            &downloads,
            &mut pending,
            &mut failed_ids,
            &backend,
        )
        .await;

        assert!(pending.is_empty());
        assert_eq!(failed_ids.len(), 1);
        assert!(failed_ids.contains("t-b/07 - Wake Up.flac"));
    }

    #[tokio::test]
    async fn a_failure_that_clears_on_the_next_poll_never_reaches_another_peer() {
        // slskd 0.26 retries a failed transfer itself, reporting the failure
        // only between its own attempts. Enqueueing a second peer on that
        // sighting would leave two transfers running, and whichever landed
        // second would decide which file the track ends up pointing at.
        let backend = stub();
        let mut queued = vec![queued_track(
            vec![
                item("peer_a", "Wake Up", "a/07 - Wake Up.flac"),
                item("peer_b", "Wake Up", "b/07 - Wake Up.flac"),
            ],
            &backend,
        )];
        let mut pending = pending_of(&queued);
        let mut failed_ids = HashSet::new();

        settle_poll(
            &mut queued,
            &[failed("peer_a", "a/07 - Wake Up.flac")],
            &mut pending,
            &mut failed_ids,
            &backend,
        )
        .await;

        assert_eq!(queued[0].key.peer, "peer_a", "the first sighting waits");
        assert_eq!(queued[0].pool.attempts_used(0), 1);
        assert!(queued[0].failing_since.is_some());
        assert_eq!(pending, pending_of(&queued));

        // slskd's own retry re-queued the transfer, so the failure is gone and
        // the clock must go with it.
        settle_poll(
            &mut queued,
            &[live("peer_a", "a/07 - Wake Up.flac")],
            &mut pending,
            &mut failed_ids,
            &backend,
        )
        .await;

        assert_eq!(queued[0].key.peer, "peer_a");
        assert_eq!(queued[0].pool.attempts_used(0), 1);
        assert!(queued[0].failing_since.is_none());
        assert_eq!(pending, pending_of(&queued));
        assert!(failed_ids.is_empty());
    }

    #[tokio::test]
    async fn a_pending_retry_held_past_its_bound_moves_to_the_next_peer() {
        let backend = stub();
        let mut queued = vec![queued_track(
            vec![
                item("peer_a", "Wake Up", "a/07 - Wake Up.flac"),
                item("peer_b", "Wake Up", "b/07 - Wake Up.flac"),
            ],
            &backend,
        )];
        let mut pending = pending_of(&queued);
        let mut failed_ids = HashSet::new();
        already_confirmed_failing(&mut queued[0]);
        queued[0].retry_pending_since = Some(tokio::time::Instant::from_std(
            std::time::Instant::now()
                .checked_sub(RETRY_PENDING_FOR)
                .expect("monotonic clock younger than the retry window"),
        ));
        let row = DownloadProgress {
            pending_retry: Some(Failure::new(
                FailureCode::TransferConnectionFailed,
                Stage::Transfer,
                RetryAdvice::OtherSource,
                "Connection reset by peer",
            )),
            ..live("peer_a", "a/07 - Wake Up.flac")
        };

        settle_poll(&mut queued, &[row], &mut pending, &mut failed_ids, &backend).await;

        assert_eq!(queued[0].pool.attempts_used(0), 2);
    }

    #[tokio::test]
    async fn a_track_its_user_cancels_is_cancelled_and_not_retried() {
        let stub = StubBackend::new(vec![ScriptedOutcome::Enqueued]);
        let backend: Arc<dyn DownloadBackend> = stub.clone();
        let mut queued = vec![queued_track(
            vec![
                item("peer_a", "Wake Up", "a/07 - Wake Up.flac"),
                item("peer_b", "Wake Up", "b/07 - Wake Up.flac"),
            ],
            &backend,
        )];
        let registry = TransferRegistry::global();
        let own = queued[0].key.clone();
        assert!(registry.request_cancel(&own, &queued[0].batch.owner.user_id));
        let mut pending = pending_of(&queued);
        let mut failed_ids = HashSet::new();

        settle_poll(
            &mut queued,
            &[live("peer_a", "a/07 - Wake Up.flac")],
            &mut pending,
            &mut failed_ids,
            &backend,
        )
        .await;

        assert_eq!(
            stub.cancelled(),
            vec![Cancel::only("peer_a", "t-a/07 - Wake Up.flac")]
        );
        assert!(pending.is_empty());
        assert!(failed_ids.contains("t-a/07 - Wake Up.flac"));
        assert_eq!(queued[0].pool.attempts_used(0), 1, "no other peer");
        assert!(registry.may_adopt(&own, &batch("Y").owner));
    }

    #[tokio::test]
    async fn a_track_cancelled_after_its_transfer_finished_is_still_imported() {
        let stub = StubBackend::new(vec![]);
        let backend: Arc<dyn DownloadBackend> = stub.clone();
        let mut queued = vec![queued_track(
            vec![item("peer_a", "Wake Up", "a/07 - Wake Up.flac")],
            &backend,
        )];
        assert!(TransferRegistry::global()
            .request_cancel(&queued[0].key, &queued[0].batch.owner.user_id));
        let mut pending = pending_of(&queued);
        let mut failed_ids = HashSet::new();

        settle_poll(
            &mut queued,
            &[completed("peer_a", "a/07 - Wake Up.flac")],
            &mut pending,
            &mut failed_ids,
            &backend,
        )
        .await;

        assert!(pending.is_empty());
        assert!(failed_ids.is_empty(), "it settled as done");
        assert!(stub.cancelled().is_empty());
    }

    #[tokio::test]
    async fn a_cancel_asked_for_during_a_tracks_failover_follows_it() {
        let stub = StubBackend::new(vec![ScriptedOutcome::Enqueued]);
        let backend: Arc<dyn DownloadBackend> = stub.clone();
        let mut queued = vec![queued_track(
            vec![
                item("peer_a", "Wake Up", "a/07 - Wake Up.flac"),
                item("peer_b", "Wake Up", "b/07 - Wake Up.flac"),
            ],
            &backend,
        )];
        let registry = TransferRegistry::global();
        let first = queued[0].key.clone();
        let user = queued[0].batch.owner.user_id.clone();
        let mut pending = pending_of(&queued);
        let mut failed_ids = HashSet::new();
        already_confirmed_failing(&mut queued[0]);
        let (reached, release) = stub.hold_downloads();
        let cancel = async {
            reached.notified().await; // the failover's enqueue is in flight
            assert!(registry.request_cancel(&first, &user));
            release.notify_one();
        };

        let dead = [failed("peer_a", "a/07 - Wake Up.flac")];
        tokio::join!(
            settle_poll(&mut queued, &dead, &mut pending, &mut failed_ids, &backend,),
            cancel
        );
        assert_eq!(queued[0].key.peer, "peer_b");
        settle_poll(
            &mut queued,
            &[live("peer_b", "b/07 - Wake Up.flac")],
            &mut pending,
            &mut failed_ids,
            &backend,
        )
        .await;

        assert!(stub
            .cancelled()
            .contains(&Cancel::only("peer_b", "t-b/07 - Wake Up.flac")));
        assert!(pending.is_empty());
        assert!(failed_ids.contains("t-b/07 - Wake Up.flac"));
    }

    // Paused: stop_waiting polls slskd with tokio::time::sleep until the
    // cancel has ended the transfer.
    #[tokio::test(start_paused = true)]
    async fn a_transfer_still_pending_at_the_deadline_is_cancelled_and_its_record_removed() {
        let stub = StubBackend::new(vec![]);
        let backend: Arc<dyn DownloadBackend> = stub.clone();
        let queued = vec![queued_track(
            vec![item("peer_a", "Wake Up", "a/07 - Wake Up.flac")],
            &backend,
        )];
        let mut pending = pending_of(&queued);
        stub.set_listing(vec![transfer(
            "peer_a",
            "a/07 - Wake Up.flac",
            "t-a/07 - Wake Up.flac",
            DownloadState::InProgress,
        )]);
        // slskd lists the transfer Cancelled only once its download task
        // has caught the cancel.
        let slskd = stub.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_secs(3)).await;
            slskd.set_listing(vec![transfer(
                "peer_a",
                "a/07 - Wake Up.flac",
                "t-a/07 - Wake Up.flac",
                DownloadState::Cancelled,
            )]);
        });

        stop_waiting(&queued, &mut pending, &backend).await;

        assert_eq!(
            stub.cancelled(),
            vec![
                Cancel::only("peer_a", "t-a/07 - Wake Up.flac"),
                Cancel::removing("peer_a", "t-a/07 - Wake Up.flac"),
            ]
        );
        assert_eq!(pending, pending_of(&queued), "it is not imported");
    }

    #[tokio::test(start_paused = true)]
    async fn a_cancel_slskd_never_ends_stops_the_wait_after_its_bound() {
        let stub = StubBackend::new(vec![]);
        let backend: Arc<dyn DownloadBackend> = stub.clone();
        let queued = vec![queued_track(
            vec![item("peer_a", "Wake Up", "a/07 - Wake Up.flac")],
            &backend,
        )];
        let mut pending = pending_of(&queued);
        stub.set_listing(vec![transfer(
            "peer_a",
            "a/07 - Wake Up.flac",
            "t-a/07 - Wake Up.flac",
            DownloadState::InProgress,
        )]);
        let start = tokio::time::Instant::now();

        stop_waiting(&queued, &mut pending, &backend).await;

        assert!(start.elapsed() >= CANCEL_SETTLE);
        assert!(start.elapsed() < CANCEL_SETTLE + std::time::Duration::from_secs(2));
        assert_eq!(
            stub.cancelled(),
            vec![Cancel::only("peer_a", "t-a/07 - Wake Up.flac")],
            "slskd removes only a record that ended"
        );
        assert_eq!(pending, pending_of(&queued), "it is not imported");
    }

    #[tokio::test]
    async fn a_transfer_failed_over_from_is_cancelled_then_and_its_record_removed_at_the_end() {
        let stub = StubBackend::new(vec![ScriptedOutcome::Enqueued]);
        let backend: Arc<dyn DownloadBackend> = stub.clone();
        let mut queued = vec![queued_track(
            vec![
                item("peer_a", "Wake Up", "a/07 - Wake Up.flac"),
                item("peer_b", "Wake Up", "b/07 - Wake Up.flac"),
            ],
            &backend,
        )];
        let mut pending = pending_of(&queued);
        let mut failed_ids = HashSet::new();
        already_confirmed_failing(&mut queued[0]);

        settle_poll(
            &mut queued,
            &[failed("peer_a", "a/07 - Wake Up.flac")],
            &mut pending,
            &mut failed_ids,
            &backend,
        )
        .await;

        // slskd may still be retrying it, into this track's folder.
        assert_eq!(
            stub.cancelled(),
            vec![Cancel::only("peer_a", "t-a/07 - Wake Up.flac")]
        );

        stub.set_listing(vec![
            failed("peer_a", "a/07 - Wake Up.flac"),
            completed("peer_b", "b/07 - Wake Up.flac"),
        ]);
        stop_waiting(&queued, &mut pending, &backend).await;

        assert_eq!(
            stub.cancelled(),
            vec![
                Cancel::only("peer_a", "t-a/07 - Wake Up.flac"),
                Cancel::only("peer_b", "t-b/07 - Wake Up.flac"),
                Cancel::removing("peer_a", "t-a/07 - Wake Up.flac"),
            ]
        );
        assert!(pending.is_empty(), "the retry's file is imported");
    }

    #[tokio::test]
    async fn a_transfer_that_finished_before_its_cancel_is_imported_and_keeps_its_record() {
        let stub = StubBackend::new(vec![]);
        let backend: Arc<dyn DownloadBackend> = stub.clone();
        let queued = vec![queued_track(
            vec![item("peer_a", "Wake Up", "a/07 - Wake Up.flac")],
            &backend,
        )];
        let mut pending = pending_of(&queued);
        stub.set_listing(vec![completed("peer_a", "a/07 - Wake Up.flac")]);

        stop_waiting(&queued, &mut pending, &backend).await;

        assert_eq!(
            stub.cancelled(),
            vec![Cancel::only("peer_a", "t-a/07 - Wake Up.flac")],
            "its file may still be on its way out of the incomplete folder"
        );
        assert!(pending.is_empty(), "phase 3 imports it");
    }

    #[tokio::test]
    async fn the_end_of_the_wait_removes_only_this_runs_records_that_ended_without_a_file() {
        let stub = StubBackend::new(vec![]);
        let backend: Arc<dyn DownloadBackend> = stub.clone();
        let queued = vec![
            queued_track(
                vec![item("peer_a", "Wake Up", "a/07 - Wake Up.flac")],
                &backend,
            ),
            queued_track(
                vec![item("peer_b", "Wake Up", "b/07 - Wake Up.flac")],
                &backend,
            ),
        ];
        // The first failed out of peers, the second finished in time.
        let mut pending = HashSet::new();
        stub.set_listing(vec![
            failed("peer_a", "a/07 - Wake Up.flac"),
            completed("peer_b", "b/07 - Wake Up.flac"),
            // An earlier run's record of the same file.
            transfer(
                "peer_a",
                "a/07 - Wake Up.flac",
                "t-earlier",
                DownloadState::Cancelled,
            ),
        ]);

        stop_waiting(&queued, &mut pending, &backend).await;

        assert_eq!(
            stub.cancelled(),
            vec![Cancel::removing("peer_a", "t-a/07 - Wake Up.flac")]
        );
        assert!(pending.is_empty());
    }

    #[tokio::test]
    async fn a_scan_status_that_cannot_be_read_still_starts_the_scan() {
        use crate::test_navidrome::{user_body, NavidromeFor, NavidromeStub};

        // getLastScanTime's `Internal Error` (server/subsonic/library_scanning.go:16-21); the scan
        // is accepted.
        let navi = NavidromeStub::start();
        navi.answer(
            "/rest/getScanStatus",
            &[r#"{"subsonic-response":{"status":"failed","version":"1.16.1","error":{"code":0,"message":"Internal Error"}}}"#],
        );
        navi.answer("/rest/getUser", &[&user_body(true)]);
        let db = crate::db::test_db::migrated().await;
        let client = navi.client();
        let navidrome = NavidromeFor(Ok(client.clone()));
        let user = crate::auth::UserId::new(&format!("alice-{}", uuid::Uuid::new_v4()));
        assert!(scan_imports(&db, &navidrome, &user, &client, 3).await);
        assert_eq!(
            navi.asked(),
            vec!["/rest/getScanStatus", "/rest/getUser", "/rest/startScan"]
        );
    }

    #[tokio::test]
    async fn with_no_linked_account_that_may_scan_none_is_asked_for() {
        use crate::test_navidrome::{subsonic_ok, user_body, NavidromeFor, NavidromeStub};

        let navi = NavidromeStub::start();
        navi.answer(
            "/rest/getScanStatus",
            &[&subsonic_ok(
                r#""scanStatus":{"scanning":false,"count":1342,"folderCount":120,"lastScan":"2026-11-15T10:02:00Z","scanType":"quick","elapsedTime":4000000000}"#,
            )],
        );
        navi.answer("/rest/getUser", &[&user_body(false)]);
        let db = crate::db::test_db::migrated().await;
        let client = navi.client();
        let navidrome = NavidromeFor(Ok(client.clone()));
        let user = crate::auth::UserId::new(&format!("alice-{}", uuid::Uuid::new_v4()));
        let logs = crate::test_log::LogCapture::start();
        assert!(!scan_imports(&db, &navidrome, &user, &client, 3).await);
        assert_eq!(navi.asked(), vec!["/rest/getScanStatus", "/rest/getUser"]);
        // A normal state, not a failure: nothing is logged as one.
        let lines = logs.lines();
        assert!(
            lines.contains(&"No Navidrome account linked to Soulbeet may start a scan: 3 discovery imports appear at Navidrome's next own scan".to_string())
                && !lines.iter().any(|line| line.starts_with("Failed")),
            "{lines:?}"
        );
    }

    mod reconcile {
        use super::super::reconcile_in;
        use crate::auth::UserId;
        use crate::models::user_settings::{UpdateUserSettings, UserSettings};
        use crate::test_navidrome::{
            native_playlist, NavidromeFor, NavidromeStub, ADVENT, BAD_GATEWAY, DISCOVERY_RULES,
        };
        use serde_json::{json, Value};
        use sqlx::SqlitePool;
        use std::collections::HashMap;
        use std::path::Path;

        /// The rules a discovery playlist had before it was scoped to the user's folder in its
        /// library: the profile's folder anywhere in a song's path.
        const UNSCOPED: &str = r#"{"all":[{"contains":{"filepath":"Discovery/Balanced"}}],"sort":"-dateAdded","order":"desc"}"#;

        /// Where Soulbeet sees the root of Navidrome's default library, the parent of alice's
        /// folder (core::folder_of), so her Balanced songs start with alice/Discovery/Balanced/.
        const MUSIC: &str = "/music";

        /// alice with discovery on for Balanced in her folder, `ids` stored as her playlists.
        async fn discovering(ids: &str) -> SqlitePool {
            let db = crate::db::test_db::migrated().await;
            let folder = crate::core::folder_of(&db, "alice").await;
            let update = UpdateUserSettings {
                discovery_enabled: Some(true),
                discovery_folder_id: Some(folder),
                discovery_profiles: Some("Balanced".into()),
                ..Default::default()
            };
            UserSettings::upsert_in(&db, "alice", update).await.unwrap();
            sqlx::query("UPDATE user_settings SET discovery_navidrome_playlist_id = ?")
                .bind(ids)
                .execute(&db)
                .await
                .unwrap();
            db
        }

        /// reconcile_in for alice, with MUSIC as NAVIDROME_MUSIC_PATH.
        async fn reconcile(db: &SqlitePool, of: &NavidromeFor) -> Result<(), String> {
            reconcile_in(db, of, Some(Path::new(MUSIC)), &UserId::new("alice")).await
        }

        async fn stored_ids(db: &SqlitePool) -> HashMap<String, String> {
            let s = UserSettings::get_in(db, "alice").await.unwrap();
            serde_json::from_str(&s.discovery_navidrome_playlist_id.unwrap()).unwrap()
        }

        fn only_advent() -> HashMap<String, String> {
            HashMap::from([("Balanced".to_string(), ADVENT.to_string())])
        }

        /// The bodies sent to `path`, Null for a read.
        fn sent(navi: &NavidromeStub, path: &str) -> Vec<Value> {
            (navi.posted(path).iter())
                .map(|body| match body.is_empty() {
                    true => Value::Null,
                    false => serde_json::from_str(body).unwrap(),
                })
                .collect()
        }

        #[tokio::test]
        async fn a_discovery_playlist_that_matches_is_left_alone() {
            let db = discovering(&format!(r#"{{"Balanced":"{ADVENT}"}}"#)).await;
            let navi = NavidromeStub::start();
            let path = format!("/api/playlist/{ADVENT}");
            let playlist = native_playlist(ADVENT, "Fresh Picks", 2, false, Some(DISCOVERY_RULES));
            navi.answer(&path, &[&playlist]);
            let of = NavidromeFor(Ok(navi.client()));
            reconcile(&db, &of).await.unwrap();
            assert_eq!(navi.asked(), ["/auth/login", path.as_str()]);
            assert_eq!(sent(&navi, &path), [Value::Null]);
            assert_eq!(stored_ids(&db).await, only_advent());
        }

        #[tokio::test]
        async fn a_discovery_playlist_with_another_name_or_rule_is_changed_in_place() {
            // The unscoped rule is the one every playlist made before had, so it is brought
            // to the scoped one in place.
            let db = discovering(&format!(r#"{{"Balanced":"{ADVENT}"}}"#)).await;
            let navi = NavidromeStub::start();
            let path = format!("/api/playlist/{ADVENT}");
            let renamed = native_playlist(ADVENT, "My mix", 2, false, Some(DISCOVERY_RULES));
            let moved = native_playlist(ADVENT, "Fresh Picks", 2, false, Some(UNSCOPED));
            // Each read, then its update, which Navidrome answers with the playlist.
            navi.answer(&path, &[&renamed, &renamed, &moved, &moved]);
            let of = NavidromeFor(Ok(navi.client()));
            reconcile(&db, &of).await.unwrap();
            reconcile(&db, &of).await.unwrap();
            let rules: Value = serde_json::from_str(DISCOVERY_RULES).unwrap();
            assert_eq!(
                sent(&navi, &path),
                [
                    Value::Null,
                    json!({ "name": "Fresh Picks" }),
                    Value::Null,
                    json!({ "rules": rules }),
                ]
            );
            assert!(
                !navi.asked().contains(&"/api/playlist".to_string()),
                "no playlist is made"
            );
            assert_eq!(stored_ids(&db).await, only_advent());
        }

        #[tokio::test]
        async fn a_discovery_playlist_navidrome_no_longer_has_is_made_again() {
            let db =
                discovering(r#"{"Adventurous":"2kD8sP4vX7zQ1mN5bT3wYa","Balanced":"gone"}"#).await;
            let navi = NavidromeStub::start();
            // deluan/rest controller.go Get, as get_native_playlist reads it.
            navi.answer_with_status(
                "/api/playlist/gone",
                &[(404, r#"{"error":"playlist(id:gone) not found"}"#)],
            );
            let made = native_playlist(ADVENT, "Fresh Picks", 0, false, Some(DISCOVERY_RULES));
            navi.answer("/api/playlist", &[&made]);
            let of = NavidromeFor(Ok(navi.client()));
            reconcile(&db, &of).await.unwrap();
            assert_eq!(
                navi.asked(),
                ["/auth/login", "/api/playlist/gone", "/api/playlist"]
            );
            let created = sent(&navi, "/api/playlist").remove(0);
            let rules: Value = serde_json::from_str(DISCOVERY_RULES).unwrap();
            assert_eq!(
                (&created["name"], &created["rules"]),
                (&json!("Fresh Picks"), &rules)
            );
            assert_eq!(
                stored_ids(&db).await,
                HashMap::from([
                    (
                        "Adventurous".to_string(),
                        "2kD8sP4vX7zQ1mN5bT3wYa".to_string()
                    ),
                    ("Balanced".to_string(), ADVENT.to_string()),
                ])
            );
        }

        #[tokio::test]
        async fn a_failed_read_or_update_is_an_error_and_makes_no_second_playlist() {
            let db = discovering(&format!(r#"{{"Balanced":"{ADVENT}"}}"#)).await;
            let navi = NavidromeStub::start();
            let path = format!("/api/playlist/{ADVENT}");
            let renamed = native_playlist(ADVENT, "My mix", 2, false, Some(DISCOVERY_RULES));
            navi.answer_with_status(
                &path,
                &[
                    (502, BAD_GATEWAY),
                    (200, renamed.as_str()),
                    (500, r#"{"error":"database is locked"}"#),
                ],
            );
            let of = NavidromeFor(Ok(navi.client()));
            let read = reconcile(&db, &of).await.unwrap_err();
            assert!(
                read.starts_with("Balanced: Get playlist failed: HTTP 502"),
                "{read}"
            );
            let update = reconcile(&db, &of).await.unwrap_err();
            assert!(
                update.starts_with("Balanced: Update smart playlist failed: HTTP 500"),
                "{update}"
            );
            assert!(
                !navi.asked().contains(&"/api/playlist".to_string()),
                "no playlist is made"
            );
            assert_eq!(stored_ids(&db).await, only_advent());
        }

        #[tokio::test]
        async fn a_made_playlist_whose_id_cannot_be_saved_is_deleted_and_reported() {
            let db = discovering("{}").await;
            sqlx::query(
                "CREATE TRIGGER no_ids BEFORE UPDATE OF discovery_navidrome_playlist_id
                 ON user_settings BEGIN SELECT RAISE(ABORT, 'disk full'); END",
            )
            .execute(&db)
            .await
            .unwrap();
            let navi = NavidromeStub::start();
            let made = native_playlist(ADVENT, "Fresh Picks", 0, false, Some(DISCOVERY_RULES));
            navi.answer("/api/playlist", &[&made]);
            let of = NavidromeFor(Ok(navi.client()));
            let err = reconcile(&db, &of).await.unwrap_err();
            assert!(err.contains(&format!("id {ADVENT} not saved")), "{err}");
            assert!(err.contains("playlist deleted"), "{err}");
            let path = format!("/api/playlist/{ADVENT}");
            assert_eq!(
                navi.asked(),
                ["/auth/login", "/api/playlist", path.as_str()]
            );
            assert!(stored_ids(&db).await.is_empty());
        }

        #[tokio::test]
        async fn a_reconcile_waits_for_the_one_running_for_the_same_user() {
            use crate::core::playlists::{discovery_key, lock};

            let db = discovering(&format!(r#"{{"Balanced":"{ADVENT}"}}"#)).await;
            let navi = NavidromeStub::start();
            let playlist = native_playlist(ADVENT, "Fresh Picks", 2, false, Some(DISCOVERY_RULES));
            navi.answer(&format!("/api/playlist/{ADVENT}"), &[&playlist]);
            let of = NavidromeFor(Ok(navi.client()));
            let running = lock(&discovery_key("alice")).await;
            let mut second = tokio::spawn(async move { reconcile(&db, &of).await });
            let short = std::time::Duration::from_millis(50);
            assert!(
                tokio::time::timeout(short, &mut second).await.is_err(),
                "the second waits while the first runs"
            );
            assert!(navi.asked().is_empty());
            drop(running);
            second.await.unwrap().unwrap();
            assert_eq!(navi.asked().len(), 2);
        }

        #[tokio::test]
        async fn a_discovery_folder_with_no_known_place_leaves_its_playlists_alone() {
            let db = discovering(&format!(r#"{{"Balanced":"{ADVENT}"}}"#)).await;
            let navi = NavidromeStub::start();
            let of = NavidromeFor(Ok(navi.client()));
            // No NAVIDROME_MUSIC_PATH, no learned root and no pending track to learn one from.
            reconcile_in(&db, &of, None, &UserId::new("alice"))
                .await
                .unwrap();
            assert!(navi.asked().is_empty(), "{:?}", navi.asked());
            assert_eq!(stored_ids(&db).await, only_advent());
        }
    }

    #[tokio::test]
    async fn seasonal_progress_registers_readable_labels_and_updates_phases() {
        use super::{register_seasonal_progress_profiles, seasonal_progress_label, upsert_progress};
        use shared::navidrome::{DiscoveryProgress, GenerationStatus, ProfilePhase};

        let def = shared::recommendation::builtin_contextual_mixes()
            .into_iter()
            .find(|d| d.slug == "winter_holiday")
            .unwrap();
        assert_eq!(seasonal_progress_label(&def), "Seasonal: Christmas Mix");

        let user_id = format!("test-seasonal-prog-{}", uuid::Uuid::new_v4());
        {
            let mut map = crate::globals::DISCOVERY_PROGRESS.write().await;
            map.insert(
                user_id.clone(),
                DiscoveryProgress {
                    status: GenerationStatus::Running,
                    ..Default::default()
                },
            );
        }

        register_seasonal_progress_profiles(&user_id, &[(def.clone(), None, 10)]).await;
        upsert_progress(
            &user_id,
            "Seasonal: Christmas Mix",
            ProfilePhase::Downloading,
            4,
            10,
        )
        .await;

        let snapshot = {
            let mut map = crate::globals::DISCOVERY_PROGRESS.write().await;
            map.remove(&user_id).unwrap()
        };
        assert_eq!(snapshot.profiles.len(), 1);
        assert_eq!(snapshot.profiles[0].profile, "Seasonal: Christmas Mix");
        assert_eq!(snapshot.profiles[0].phase, ProfilePhase::Downloading);
        assert_eq!(snapshot.profiles[0].current, 4);
        assert_eq!(snapshot.profiles[0].total, 10);
    }
}
