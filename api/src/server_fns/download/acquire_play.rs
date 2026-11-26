use dioxus::prelude::*;
use shared::download::{
    AcquireAndPlayRequest, AcquireAndPlayResponse, ArchivalUpgradeJob, ArchivalUpgradeOutcome,
    TriggerArchivalUpgradeRequest,
};

#[cfg(feature = "server")]
use crate::{server_fns::failure_error, AuthSession};

/// 1-tap Acquire & Play for an unowned catalog track: returns immediately when already
/// in Navidrome, or starts prioritized backend acquisition and streams live stage events
/// (`Acquiring -> Importing -> Indexing -> Ready`) over `/api/downloads/updates`.
#[post("/api/acquire-and-play", auth: AuthSession)]
pub async fn acquire_and_play(
    req: AcquireAndPlayRequest,
) -> Result<AcquireAndPlayResponse, ServerFnError> {
    crate::core::acquire_play::acquire_and_play(&auth.0, req)
        .await
        .map_err(failure_error)
}

/// Lists the caller's On-Keep background archival FLAC upgrade jobs (newest first).
#[get("/api/archival-upgrades", auth: AuthSession)]
pub async fn list_archival_upgrades() -> Result<Vec<ArchivalUpgradeJob>, ServerFnError> {
    crate::core::archival::list_for_user_in(&crate::db::DB, &auth.0)
        .await
        .map_err(failure_error)
}

/// Evaluates and (when eligible) enqueues a background archival FLAC upgrade for a
/// lossy fast-acquired track that the user explicitly kept, starred, or promoted.
#[post("/api/archival-upgrades/trigger", auth: AuthSession)]
pub async fn trigger_archival_upgrade(
    req: TriggerArchivalUpgradeRequest,
) -> Result<ArchivalUpgradeOutcome, ServerFnError> {
    crate::core::archival::enqueue_on_keep_in(&crate::db::DB, &auth.0, &req)
        .await
        .map_err(failure_error)
}
