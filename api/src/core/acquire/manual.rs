//! The hand-picked download.

use crate::auth::UserCtx;
use crate::config::CONFIG;
use crate::core::sources;
use crate::core::{backend_unavailable, database_failure};
use crate::ledger::store::StoredSearch;
use crate::ledger::{JobState, Ledger, NewJob, LEDGER};
use crate::models::folder::Folder;
use crate::services::{download_backend, BackendLookupError};
use shared::download::{DownloadableItem, QueuedDownload};
use shared::failure::{Failure, Stage};
use soulbeet::DownloadBackend;
use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;

use super::{
    alternates_from, manual_batch_label, numbered, prepare_target, queue_and_watch,
    request_stopped, Acquisition, NewBatch,
};

/// `items` with each file from each source once, in their order: a search
/// can list a file twice, and one file is one slot.
fn distinct(items: Vec<DownloadableItem>) -> Vec<DownloadableItem> {
    let mut seen = std::collections::HashSet::new();
    items
        .into_iter()
        .filter(|item| seen.insert((item.source.clone(), item.id.clone())))
        .collect()
}

/// A hand-picked download of `items` into `folder` from `ctx`'s search
/// `search_id`, from the named backend or the default one. It continues the
/// job waiting for a source from that search, else starts one, and the
/// search's other candidates stand by as alternates.
pub async fn download(
    ctx: &UserCtx,
    folder: &Folder,
    items: Vec<DownloadableItem>,
    backend: Option<&str>,
    search_id: &str,
) -> Result<Vec<QueuedDownload>, Failure> {
    download_with(
        &LEDGER,
        ctx,
        folder,
        items,
        crate::importer_health::blocking_failure(),
        sources::owned_search(&crate::db::DB, ctx, search_id),
        download_backend(backend),
    )
    .await
}

/// `download`, given the ledger, the importer's gate, the search lookup and
/// the backend lookup. Each is awaited only once the steps before it passed:
/// the gate, then the search, then the target folder, then the backend. A
/// file named twice is one slot, and a request naming none makes no job.
/// From the job on it runs as its own task, so a client that disconnects
/// cannot leave a job its enqueue never ran for.
async fn download_with(
    ledger: &Ledger,
    ctx: &UserCtx,
    folder: &Folder,
    items: Vec<DownloadableItem>,
    gate: impl Future<Output = Option<Failure>>,
    search: impl Future<Output = Result<StoredSearch, Failure>>,
    backend: impl Future<Output = Result<Arc<dyn DownloadBackend>, BackendLookupError>>,
) -> Result<Vec<QueuedDownload>, Failure> {
    if let Some(failure) = gate.await {
        return Err(failure);
    }
    let search = search.await?;
    let target = prepare_target(folder).await?;
    let backend = backend
        .await
        .map_err(|e| backend_unavailable(Stage::Enqueue, &e))?;
    let items = distinct(items);
    if items.is_empty() {
        return Ok(Vec::new());
    }
    tokio::spawn(start_manual(
        ledger.clone(),
        ctx.clone(),
        folder.id.clone(),
        search,
        target,
        backend,
        items,
    ))
    .await
    .map_err(|e| request_stopped(&e))?
}

/// The job of a hand-picked download of `items` into `folder_id`, stored
/// with its slots: the one waiting for a source from `search`, else a new
/// one. Then its files are enqueued and watched.
async fn start_manual(
    ledger: Ledger,
    ctx: UserCtx,
    folder_id: String,
    search: StoredSearch,
    target: PathBuf,
    backend: Arc<dyn DownloadBackend>,
    items: Vec<DownloadableItem>,
) -> Result<Vec<QueuedDownload>, Failure> {
    let picked = numbered(items.clone());
    let claimed = ledger
        .claim_source_choice(&ctx.id(), &search.id, &folder_id, picked.clone())
        .await
        .map_err(|e| database_failure(Stage::Enqueue, e))?;
    let (job, import_mode) = match claimed {
        Some(claimed) => (claimed.id, claimed.import_mode),
        None => {
            let (id, import_mode) = (uuid::Uuid::new_v4().to_string(), CONFIG.import_mode());
            ledger
                .create(
                    NewJob {
                        id: id.clone(),
                        user: ctx.id(),
                        folder_id,
                        wanted: search.query,
                        import_mode,
                        state: JobState::Queued,
                        search_id: Some(search.id),
                        picked: picked.clone(),
                    },
                    None,
                )
                .await
                .map_err(|e| database_failure(Stage::Enqueue, e))?;
            (id, import_mode)
        }
    };

    let alternates = alternates_from(search.candidates.as_deref().unwrap_or_default(), &items);
    let fresh = NewBatch {
        batch_id: uuid::Uuid::new_v4().to_string(),
        label: manual_batch_label(&items),
        target,
        import_mode,
    };
    let acquisition = Acquisition {
        job,
        items: picked,
        alternates,
    };
    queue_and_watch(ledger, ctx, backend, fresh, acquisition).await
}

#[cfg(test)]
mod tests {
    use super::super::test_support::own_ctx;
    use super::*;
    use crate::auth::UserId;
    use crate::core::test_ctx;
    use crate::ledger::NewSearch;
    use crate::server_fns::download::test_support::{
        group, item, key, ScriptedOutcome, StubBackend,
    };
    use crate::transfer_registry::{BatchRef, TransferOwner, TransferRegistry};
    use shared::download::{DownloadQuery, JobSlot};
    use shared::failure::{FailureCode, RetryAdvice};
    use soulbeet::ImportMode;

    /// A lookup the code under test must never reach.
    async fn unreached_backend() -> Result<Arc<dyn DownloadBackend>, BackendLookupError> {
        panic!("the backend was looked up")
    }

    fn folder_at(path: &std::path::Path) -> Folder {
        Folder {
            id: "f1".into(),
            user_id: "user-a".into(),
            name: "Music".into(),
            path: path.to_string_lossy().into_owned(),
            shared: false,
        }
    }

    #[tokio::test]
    async fn a_download_checks_the_importer_then_the_search_then_the_folder_then_the_backend() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("music");
        std::fs::write(&file, b"").unwrap();
        let unwritable = folder_at(&file.join("Come Over"));
        let items = || vec![item("peer_a", "One", "01.flac")];
        let importer = Failure::new(
            FailureCode::ImporterBroken,
            Stage::Import,
            RetryAdvice::NeedsUser,
            "No metadata source plugin loaded",
        );
        let stored = || StoredSearch {
            id: "s1".into(),
            backend: Some("slskd".into()),
            query: DownloadQuery::new(vec![]),
            candidates: None,
            expires_at: i64::MAX,
        };
        let ledger = Ledger::recording().0;

        let gated = download_with(
            &ledger,
            &own_ctx(),
            &unwritable,
            items(),
            std::future::ready(Some(importer.clone())),
            std::future::ready(Err(sources::search_not_found())),
            unreached_backend(),
        )
        .await
        .unwrap_err();
        assert_eq!(gated, importer);

        let no_search = download_with(
            &ledger,
            &own_ctx(),
            &unwritable,
            items(),
            std::future::ready(None),
            std::future::ready(Err(sources::search_not_found())),
            unreached_backend(),
        )
        .await
        .unwrap_err();
        assert_eq!(no_search, sources::search_not_found());

        let unwritable_target = download_with(
            &ledger,
            &own_ctx(),
            &unwritable,
            items(),
            std::future::ready(None),
            std::future::ready(Ok(stored())),
            unreached_backend(),
        )
        .await
        .unwrap_err();
        assert_eq!(unwritable_target.code, FailureCode::ImportTargetUnwritable);

        let unknown = BackendLookupError::Unknown("nicotine".into());
        let no_backend = download_with(
            &ledger,
            &own_ctx(),
            &folder_at(&dir.path().join("library")),
            items(),
            std::future::ready(None),
            std::future::ready(Ok(stored())),
            std::future::ready(Err(BackendLookupError::Unknown("nicotine".into()))),
        )
        .await
        .unwrap_err();
        assert_eq!(no_backend, backend_unavailable(Stage::Enqueue, &unknown));
    }

    /// `alice`'s folder `folder_id` at a new temporary directory.
    fn alices_folder(folder_id: String, dir: &tempfile::TempDir) -> Folder {
        Folder {
            id: folder_id,
            user_id: "alice".into(),
            name: "f".into(),
            path: dir.path().to_string_lossy().into_owned(),
            shared: false,
        }
    }

    #[tokio::test]
    async fn a_pick_from_a_search_that_is_gone_is_refused_before_anything_downloads() {
        let db = crate::db::test_db::migrated().await;
        let folder_id = crate::core::folder_of(&db, "alice").await;
        let dir = tempfile::tempdir().unwrap();
        let folder = alices_folder(folder_id, &dir);
        let ledger = Ledger::start(db.clone());
        let f = download_with(
            &ledger,
            &test_ctx("alice"),
            &folder,
            vec![item("peer_a", "One", "01.flac")],
            std::future::ready(None),
            sources::owned_search(&db, &test_ctx("alice"), "no-such-search"),
            unreached_backend(),
        )
        .await
        .unwrap_err();
        assert_eq!(f, sources::search_not_found());
    }

    #[tokio::test]
    async fn a_pick_from_the_fallback_continues_its_job_with_the_searchs_other_sources() {
        let db = crate::db::test_db::migrated().await;
        let folder_id = crate::core::folder_of(&db, "alice").await;
        let dir = tempfile::tempdir().unwrap();
        let folder = alices_folder(folder_id.clone(), &dir);
        let ledger = Ledger::start(db.clone());
        let candidates = vec![
            group(
                "peer_a",
                vec![
                    item("peer_a", "One", "01.flac"),
                    item("peer_a", "Two", "02.flac"),
                ],
                0.5,
            ),
            group("peer_b", vec![item("peer_b", "Two", "b/02.flac")], 0.4),
        ];
        ledger
            .create(
                NewJob {
                    id: "j1".into(),
                    user: UserId::new("alice"),
                    folder_id,
                    wanted: DownloadQuery::new(vec![]),
                    import_mode: ImportMode::Singleton,
                    state: JobState::NeedsSourceChoice,
                    search_id: Some("s1".into()),
                    picked: vec![],
                },
                Some(NewSearch {
                    id: "s1".into(),
                    user: UserId::new("alice"),
                    backend: None,
                    query: DownloadQuery::new(vec![]),
                }),
            )
            .await
            .unwrap();
        ledger.store_candidates("s1", candidates).await.unwrap();
        // Alice already downloads One into this folder, so Two joins that batch.
        let stub = StubBackend::new(vec![ScriptedOutcome::Enqueued]);
        let backend: Arc<dyn DownloadBackend> = stub.clone();
        let running = BatchRef::new(
            TransferOwner {
                user_id: UserId::new("alice"),
                batch_id: uuid::Uuid::new_v4().to_string(),
            },
            "Come Over".into(),
            dir.path().to_path_buf(),
            ImportMode::Singleton,
        );
        TransferRegistry::global().claim(&[key(&backend, "peer_a", "01.flac")], &running);

        let ctx = test_ctx("alice");
        download_with(
            &ledger,
            &ctx,
            &folder,
            vec![
                item("peer_a", "One", "01.flac"),
                item("peer_a", "Two", "02.flac"),
            ],
            std::future::ready(None),
            sources::owned_search(&db, &ctx, "s1"),
            std::future::ready(Ok(backend)),
        )
        .await
        .unwrap();
        ledger.settled().await;

        assert_eq!(
            crate::ledger::store::job_state(&db, "j1").await,
            Some(JobState::Downloading)
        );
        let joined = running.inbox.take();
        assert_eq!(
            joined
                .iter()
                .map(|s| (s.job.clone(), s.alternates.clone()))
                .collect::<Vec<_>>(),
            vec![(
                JobSlot {
                    job: "j1".into(),
                    slot: 1
                },
                vec![item("peer_b", "Two", "b/02.flac")]
            )]
        );
    }

    #[tokio::test]
    async fn a_file_named_twice_is_one_slot_and_a_request_naming_none_makes_no_job() {
        let db = crate::db::test_db::migrated().await;
        let folder_id = crate::core::folder_of(&db, "alice").await;
        let dir = tempfile::tempdir().unwrap();
        let folder = alices_folder(folder_id, &dir);
        let ledger = Ledger::start(db.clone());
        ledger
            .record_search(NewSearch {
                id: "s1".into(),
                user: UserId::new("alice"),
                backend: None,
                query: DownloadQuery::new(vec![]),
            })
            .await
            .unwrap();
        // Refused, so no monitor starts.
        let stub = StubBackend::new(vec![ScriptedOutcome::Rejected]);
        let backend: Arc<dyn DownloadBackend> = stub.clone();
        let ctx = test_ctx("alice");
        let pick = |items: Vec<DownloadableItem>| {
            download_with(
                &ledger,
                &ctx,
                &folder,
                items,
                std::future::ready(None),
                sources::owned_search(&db, &ctx, "s1"),
                std::future::ready(Ok(backend.clone())),
            )
        };

        assert!(pick(vec![]).await.unwrap().is_empty());
        ledger.settled().await;
        let jobs: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM jobs")
            .fetch_one(&db)
            .await
            .unwrap();
        assert_eq!(jobs, 0);

        let one = item("peer_a", "One", "01.flac");
        let _ = pick(vec![one.clone(), one]).await;
        ledger.settled().await;
        let slots: Vec<u32> = sqlx::query_scalar("SELECT slot FROM job_tracks")
            .fetch_all(&db)
            .await
            .unwrap();
        assert_eq!(slots, vec![0]);
        assert_eq!(stub.destinations().len(), 1);
    }
}
