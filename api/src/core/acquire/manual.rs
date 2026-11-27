//! The hand-picked download.

use crate::auth::UserCtx;
use crate::config::CONFIG;
use crate::core::sources;
use crate::core::{backend_unavailable, database_failure};
use crate::ledger::store::StoredSearch;
use crate::ledger::{DuplicateRule, JobKind, JobState, Ledger, NewJob, LEDGER};
use crate::models::folder::Folder;
use crate::server_fns::download::failover::MAX_SOURCES_PER_TRACK;
use crate::services::{Backends, Registry};
use shared::backends::delivery;
use shared::download::{DownloadableItem, QueuedDownload};
use shared::failure::{Failure, Stage};
use soulbeet::DownloadBackend;
use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;

use super::{
    alternates_from, backend_of, manual_batch_label, numbered, prepare_target, queue_and_watch,
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
/// `search_id`, from the backend the items name. It continues the job
/// waiting for a source from that search, else starts one, and the search's
/// other candidates stand by as alternates.
pub async fn download(
    ctx: &UserCtx,
    folder: &Folder,
    items: Vec<DownloadableItem>,
    search_id: &str,
) -> Result<Vec<QueuedDownload>, Failure> {
    download_with(
        &LEDGER,
        ctx,
        folder,
        items,
        crate::importer_health::blocking_failure(),
        sources::owned_search(&crate::db::DB, ctx, search_id),
        &Registry,
    )
    .await
}

/// `download`, given the ledger, the importer's gate, the search lookup and
/// where backends are looked up. Each step runs only once the steps before it
/// passed: the gate, then the search, then the target folder, then the
/// backend the items name. A file named twice is one slot, a request naming
/// none makes no job, and files of more than one backend are refused. From
/// the job on it runs as its own task, so a client that disconnects cannot
/// leave a job its enqueue never ran for.
async fn download_with(
    ledger: &Ledger,
    ctx: &UserCtx,
    folder: &Folder,
    items: Vec<DownloadableItem>,
    gate: impl Future<Output = Option<Failure>>,
    search: impl Future<Output = Result<StoredSearch, Failure>>,
    backends: &dyn Backends,
) -> Result<Vec<QueuedDownload>, Failure> {
    if let Some(failure) = gate.await {
        return Err(failure);
    }
    let search = search.await?;
    let target = prepare_target(folder).await?;
    let items = distinct(items);
    if items.is_empty() {
        return Ok(Vec::new());
    }
    let routed = backend_of(&items)?.to_string();
    let backend = backends
        .get(&routed)
        .await
        .map_err(|e| backend_unavailable(Stage::Enqueue, &e))?;
    tokio::spawn(start_manual(
        ledger.clone(),
        ctx.clone(),
        folder.id.clone(),
        search,
        target,
        (routed, backend),
        items,
    ))
    .await
    .map_err(|e| request_stopped(&e))?
}

/// The job of a hand-picked download of `items` into `folder_id`, stored
/// with its slots: the one waiting for a source from `search`, else a new
/// one. Then its files are enqueued and watched on the backend `routed`
/// gives with the id it is registered under.
async fn start_manual(
    ledger: Ledger,
    ctx: UserCtx,
    folder_id: String,
    search: StoredSearch,
    target: PathBuf,
    routed: (String, Arc<dyn DownloadBackend>),
    items: Vec<DownloadableItem>,
) -> Result<Vec<QueuedDownload>, Failure> {
    let (routed, backend) = routed;
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
                        kind: JobKind::of(&search.query),
                        wanted: search.query,
                        import_mode,
                        state: JobState::Queued,
                        search_id: Some(search.id),
                        picked: picked.clone(),
                        options: None,
                    },
                    None,
                )
                .await
                .map_err(|e| database_failure(Stage::Enqueue, e))?;
            (id, import_mode)
        }
    };

    let alternates = alternates_from(search.candidates.as_deref().unwrap_or_default(), &items);
    let duplicates = DuplicateRule::Guarded.for_delivery(delivery(&routed));
    let fresh = NewBatch {
        batch_id: uuid::Uuid::new_v4().to_string(),
        label: manual_batch_label(&items),
        backend: routed,
        target,
        import_mode,
        duplicates,
        verify_policy: soulbeet::VerifyPolicy::Standard,
    };
    let acquisition = Acquisition {
        job,
        items: picked,
        alternates,
        max_sources: MAX_SOURCES_PER_TRACK,
    };
    queue_and_watch(ledger, ctx, backend, fresh.into(), acquisition).await
}

#[cfg(test)]
mod tests {
    use super::super::test_support::own_ctx;
    use super::*;
    use crate::auth::UserId;
    use crate::core::test_ctx;
    use crate::ledger::NewSearch;
    use crate::server_fns::download::test_support::{
        group, item, item_on, key, ScriptedOutcome, StubBackend, StubBackends,
    };
    use crate::services::{BackendLookupError, DEFAULT_DOWNLOAD_BACKEND};
    use crate::transfer_registry::{BatchRef, TransferOwner, TransferRegistry};
    use shared::download::{DownloadQuery, JobSlot};
    use shared::failure::{FailureCode, RetryAdvice};
    use soulbeet::ImportMode;

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
            spec: None,
            stats: None,
            failure: None,
        };
        let ledger = Ledger::recording().0;
        let unreached = StubBackends::default();

        let gated = download_with(
            &ledger,
            &own_ctx(),
            &unwritable,
            items(),
            std::future::ready(Some(importer.clone())),
            std::future::ready(Err(sources::search_not_found())),
            &unreached,
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
            &unreached,
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
            &unreached,
        )
        .await
        .unwrap_err();
        assert_eq!(unwritable_target.code, FailureCode::ImportTargetUnwritable);
        assert!(unreached.asked().is_empty());

        let unknown = BackendLookupError::Unknown("nicotine".into());
        let missing = StubBackends::default();
        missing.fail(DEFAULT_DOWNLOAD_BACKEND, unknown.clone());
        let no_backend = download_with(
            &ledger,
            &own_ctx(),
            &folder_at(&dir.path().join("library")),
            items(),
            std::future::ready(None),
            std::future::ready(Ok(stored())),
            &missing,
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
        let unreached = StubBackends::default();
        let f = download_with(
            &ledger,
            &test_ctx("alice"),
            &folder,
            vec![item("peer_a", "One", "01.flac")],
            std::future::ready(None),
            sources::owned_search(&db, &test_ctx("alice"), "no-such-search"),
            &unreached,
        )
        .await
        .unwrap_err();
        assert_eq!(f, sources::search_not_found());
        assert!(unreached.asked().is_empty());
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
                    kind: JobKind::Track,
                    options: None,
                },
                Some(NewSearch {
                    id: "s1".into(),
                    user: UserId::new("alice"),
                    backend: None,
                    query: DownloadQuery::new(vec![]),
                    spec: None,
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
            DEFAULT_DOWNLOAD_BACKEND.to_string(),
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
            &StubBackends::default().with(DEFAULT_DOWNLOAD_BACKEND, stub),
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

    /// Alice's search `s1` and her folder, at `dir`.
    async fn alices_search(
        db: &sqlx::SqlitePool,
        ledger: &Ledger,
        dir: &tempfile::TempDir,
    ) -> Folder {
        let folder = alices_folder(crate::core::folder_of(db, "alice").await, dir);
        ledger
            .record_search(NewSearch {
                id: "s1".into(),
                user: UserId::new("alice"),
                backend: None,
                query: DownloadQuery::new(vec![]),
                spec: None,
            })
            .await
            .unwrap();
        folder
    }

    #[tokio::test]
    async fn a_file_named_twice_is_one_slot_and_a_request_naming_none_makes_no_job() {
        let db = crate::db::test_db::migrated().await;
        let dir = tempfile::tempdir().unwrap();
        let ledger = Ledger::start(db.clone());
        let folder = alices_search(&db, &ledger, &dir).await;
        // Refused, so no monitor starts.
        let stub = StubBackend::new(vec![ScriptedOutcome::Rejected]);
        let backends = StubBackends::default().with(DEFAULT_DOWNLOAD_BACKEND, stub.clone());
        let ctx = test_ctx("alice");
        let pick = |items: Vec<DownloadableItem>| {
            download_with(
                &ledger,
                &ctx,
                &folder,
                items,
                std::future::ready(None),
                sources::owned_search(&db, &ctx, "s1"),
                &backends,
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

    #[tokio::test]
    async fn files_on_one_backend_are_sent_only_to_that_backend() {
        let db = crate::db::test_db::migrated().await;
        let dir = tempfile::tempdir().unwrap();
        let ledger = Ledger::start(db.clone());
        let folder = alices_search(&db, &ledger, &dir).await;
        // Refused, so no monitor starts.
        let (a, b) = (
            StubBackend::new(vec![]),
            StubBackend::new(vec![ScriptedOutcome::Rejected]),
        );
        let backends = StubBackends::default()
            .with("a", a.clone())
            .with("b", b.clone());
        let ctx = test_ctx("alice");

        download_with(
            &ledger,
            &ctx,
            &folder,
            vec![item_on("b", "peer_b", "One", "b/01.flac")],
            std::future::ready(None),
            sources::owned_search(&db, &ctx, "s1"),
            &backends,
        )
        .await
        .unwrap();

        assert_eq!(backends.asked(), vec!["b".to_string()]);
        assert_eq!(b.requests().len(), 1);
        assert!(a.requests().is_empty());
    }

    #[tokio::test]
    async fn files_of_two_backends_are_refused_before_any_job_or_lookup() {
        let db = crate::db::test_db::migrated().await;
        let dir = tempfile::tempdir().unwrap();
        let ledger = Ledger::start(db.clone());
        let folder = alices_search(&db, &ledger, &dir).await;
        let (a, b) = (StubBackend::new(vec![]), StubBackend::new(vec![]));
        let backends = StubBackends::default()
            .with("a", a.clone())
            .with("b", b.clone());
        let ctx = test_ctx("alice");

        let f = download_with(
            &ledger,
            &ctx,
            &folder,
            vec![
                item_on("a", "peer_a", "One", "a/01.flac"),
                item_on("b", "peer_b", "Two", "b/02.flac"),
            ],
            std::future::ready(None),
            sources::owned_search(&db, &ctx, "s1"),
            &backends,
        )
        .await
        .unwrap_err();
        ledger.settled().await;

        assert_eq!(
            (f.code, f.stage, f.retry),
            (
                FailureCode::InvalidInput,
                Stage::Input,
                RetryAdvice::NotRetryable
            )
        );
        assert!(backends.asked().is_empty());
        let jobs: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM jobs")
            .fetch_one(&db)
            .await
            .unwrap();
        assert_eq!(jobs, 0);
        assert!(a.requests().is_empty() && b.requests().is_empty());
    }
}
