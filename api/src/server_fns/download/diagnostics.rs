//! Authenticated on-demand job diagnostics endpoint backed by existing ledger tables
//! (`jobs`, `job_tracks`, `job_attempts`, `job_events`, `searches`).

use dioxus::prelude::*;
use shared::download::JobDiagnostics;

#[cfg(feature = "server")]
use crate::{
    auth::UserCtx,
    ledger::store::{self, AttemptRecord},
    server_fns::server_error,
    AuthSession,
};
#[cfg(feature = "server")]
use shared::download::{DiagnosticAttempt, DiagnosticEvent, DownloadState};
#[cfg(feature = "server")]
use sqlx::SqlitePool;

#[cfg(feature = "server")]
fn not_found_error() -> ServerFnError {
    ServerFnError::ServerError {
        message: "Job not found".to_string(),
        code: 404,
        details: None,
    }
}

#[cfg(feature = "server")]
fn attempt_outcome(attempt: &AttemptRecord) -> String {
    if attempt.failure.is_some() {
        return "failed".to_string();
    }
    if let Some(ref row) = attempt.last_row {
        return match row.state {
            DownloadState::Imported => "imported".to_string(),
            DownloadState::Completed => "downloaded".to_string(),
            DownloadState::Importing | DownloadState::ImportQueued(_) => "importing".to_string(),
            DownloadState::ImportSkipped => "skipped".to_string(),
            DownloadState::Failed(_) => "failed".to_string(),
            DownloadState::Cancelled => "cancelled".to_string(),
            DownloadState::InProgress => "downloading".to_string(),
            DownloadState::Queued => "queued".to_string(),
        };
    }
    if attempt.ended_at.is_some() {
        "completed".to_string()
    } else if attempt.started_at.is_some() {
        "in_progress".to_string()
    } else {
        "queued".to_string()
    }
}

/// Loads full decision audit trail and diagnostics for `job_id`, enforcing strict ownership:
/// only the job's owner or an admin may inspect it. Returns 404 otherwise so another user's
/// job ids and attempt history are never leaked.
#[cfg(feature = "server")]
pub(crate) async fn load_job_diagnostics(
    db: &SqlitePool,
    actor: &UserCtx,
    job_id: &str,
) -> Result<JobDiagnostics, ServerFnError> {
    let job = if actor.role.is_admin() {
        store::job_record_by_id(db, job_id)
            .await
            .map_err(server_error)?
    } else {
        store::job_record(db, job_id, &actor.id())
            .await
            .map_err(server_error)?
    };
    let Some(job) = job else {
        return Err(not_found_error());
    };

    let tracks = store::tracks_of(db, &job.id).await.map_err(server_error)?;
    let attempts_by_slot = store::attempts_of(db, &job.id)
        .await
        .map_err(server_error)?;
    let timeline = store::timeline_of(db, &job.id)
        .await
        .map_err(server_error)?;

    let search_stats = match &job.search_id {
        Some(search_id) => store::search_of(db, search_id, &job.ctx.id())
            .await
            .map_err(server_error)?
            .and_then(|s| s.stats),
        None => None,
    };

    // Job-level failure covers pre-slot failures (search / tracklist); otherwise inspect slot rows.
    let failure = job.failure.clone().or_else(|| {
        tracks
            .iter()
            .find_map(|t| t.row.as_ref().and_then(|r| r.failure.clone()))
    });

    let mut attempts = Vec::new();
    for (slot, records) in attempts_by_slot {
        for record in records {
            let backend = record
                .candidate
                .as_ref()
                .map(|c| c.backend.clone())
                .or_else(|| record.last_row.as_ref().and_then(|r| r.backend.clone()));
            let item = record
                .candidate
                .as_ref()
                .map(|c| c.id.clone())
                .or_else(|| record.last_row.as_ref().map(|r| r.item.clone()));
            let source = record
                .source
                .clone()
                .or_else(|| record.candidate.as_ref().map(|c| c.source.clone()))
                .or_else(|| record.last_row.as_ref().map(|r| r.source.clone()));
            let outcome = attempt_outcome(&record);
            attempts.push(DiagnosticAttempt {
                slot,
                attempt: record.n,
                source,
                backend,
                item,
                started_at: record.started_at,
                ended_at: record.ended_at,
                outcome,
                failure: record.failure,
            });
        }
    }

    let timeline = timeline
        .into_iter()
        .map(|entry| DiagnosticEvent {
            seq: entry.seq,
            at: entry.at,
            stage: entry.stage,
            event: entry.event,
            facts: entry.facts,
        })
        .collect();

    Ok(JobDiagnostics {
        job_id: job.id,
        state: job.state.as_str().to_string(),
        failure,
        search_stats,
        attempts,
        timeline,
    })
}

/// Fetches the decision audit trail (failure summary, search rejection diagnostics, tried
/// sources, and stage timeline) for a single ledger job on demand.
#[post("/api/downloads/diagnostics", auth: AuthSession)]
pub async fn get_job_diagnostics(job_id: String) -> Result<JobDiagnostics, ServerFnError> {
    load_job_diagnostics(&crate::db::DB, &auth.0, &job_id).await
}

#[cfg(all(test, feature = "server"))]
mod tests {
    use super::*;
    use crate::auth::{Role, UserId};
    use crate::core::test_ctx;
    use crate::ledger::{JobEvent, JobKind, JobState, Ledger, NewJob, NewSearch};
    use crate::server_fns::download::test_support::item;
    use shared::download::{DownloadProgress, DownloadQuery, JobSlot, SearchStats};
    use shared::failure::{Failure, FailureCode, RetryAdvice, Stage};
    use soulbeet::ImportMode;

    #[tokio::test]
    async fn ownership_guard_rejects_non_owners_and_allows_owner_and_admin() {
        let db = crate::db::test_db::migrated().await;
        let folder_id = crate::core::folder_of(&db, "alice").await;
        let _bob_folder = crate::core::folder_of(&db, "bob").await;
        let ledger = Ledger::start(db.clone());

        let picked_item = item("peer_a", "O Holy Night", "a/01.flac");
        ledger
            .create(
                NewJob {
                    id: "job-alice-1".into(),
                    user: UserId::new("alice"),
                    folder_id,
                    wanted: DownloadQuery::new(vec![]),
                    import_mode: ImportMode::Singleton,
                    state: JobState::Searching,
                    search_id: Some("search-alice-1".into()),
                    picked: vec![],
                    kind: JobKind::Track,
                    options: None,
                },
                Some(NewSearch {
                    id: "search-alice-1".into(),
                    user: UserId::new("alice"),
                    backend: Some("slskd".into()),
                    query: DownloadQuery::new(vec![]),
                    spec: None,
                }),
            )
            .await
            .unwrap();

        let mut stats = SearchStats {
            query_text: "Marrow Choir O Holy Night".into(),
            response_count: 3,
            file_count: 5,
            locked_file_count: 1,
            not_audio: 0,
            below_floor: 2,
            ..Default::default()
        };
        stats.record_rejection(
            "peer_bad",
            "01 - O Holy Night (Live).flac",
            Some(0.55),
            "version_veto",
        );
        ledger
            .search_ended("search-alice-1", Ok((vec![], Some(stats.clone()))))
            .await
            .unwrap();

        ledger.record(JobEvent::Picked {
            job: "job-alice-1".into(),
            items: vec![(0, picked_item.clone())],
        });
        let failure = Failure::new(
            FailureCode::PeerOffline,
            Stage::Transfer,
            RetryAdvice::OtherSource,
            "Peer disconnected",
        );
        let failed_row = DownloadProgress::failed(
            "t-1".into(),
            "peer_a".into(),
            "a/01.flac".into(),
            failure.clone(),
        )
        .with_backend("slskd")
        .with_job(JobSlot {
            job: "job-alice-1".into(),
            slot: 0,
        });
        ledger.record(JobEvent::Slots {
            state: JobState::Failed,
            rows: vec![failed_row],
        });
        ledger.settled().await;

        // Non-owner (bob) is rejected with 404 and sees nothing.
        let bob = test_ctx("bob");
        let err = load_job_diagnostics(&db, &bob, "job-alice-1")
            .await
            .unwrap_err();
        match err {
            ServerFnError::ServerError { code, message, .. } => {
                assert_eq!(code, 404);
                assert_eq!(message, "Job not found");
            }
            other => panic!("expected 404 ServerError, got {other:?}"),
        }

        // Owner (alice) gets complete diagnostics.
        let alice = test_ctx("alice");
        let diag = load_job_diagnostics(&db, &alice, "job-alice-1")
            .await
            .unwrap();
        assert_eq!(diag.job_id, "job-alice-1");
        assert_eq!(diag.state, "failed");
        assert_eq!(diag.failure.as_ref().map(|f| f.code), Some(FailureCode::PeerOffline));
        let diag_stats = diag.search_stats.expect("search_stats persisted");
        assert_eq!(diag_stats.rejection_counts.get("version_veto"), Some(&1));
        assert_eq!(diag_stats.top_rejected.len(), 1);
        assert_eq!(diag.attempts.len(), 1);
        assert_eq!(diag.attempts[0].source.as_deref(), Some("peer_a"));
        assert_eq!(diag.attempts[0].outcome, "failed");
        assert!(diag
            .timeline
            .iter()
            .any(|e| e.stage == Stage::SourceSearch && e.event == "search_diagnostics"));

        // Admin user can inspect alice's job.
        let admin = UserCtx {
            user_id: "admin-1".into(),
            username: "admin".into(),
            role: Role::Admin,
        };
        let admin_diag = load_job_diagnostics(&db, &admin, "job-alice-1")
            .await
            .unwrap();
        assert_eq!(admin_diag.job_id, "job-alice-1");
    }
}
