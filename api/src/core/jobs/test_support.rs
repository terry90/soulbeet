//! Ledger states of MCP jobs for tests, each built the way its code path builds it: through
//! `Ledger::{create_batch, record, transition, ..}` only. Every job id is a new uuid.

use super::runner::{Acquire, MCP_DOWNLOADS_PER_USER};
use super::{jobs_view, AlbumRef, JobSelect, JobView, Wanted};
use crate::auth::UserId;
use crate::core::library::Reference;
use crate::core::test_ctx;
use crate::ledger::store;
use crate::ledger::{
    Admitted, BatchOrigin, DuplicateRule, JobEvent, JobOptions, JobState, NewJob, NewJobBatch,
    NewSearch, Pick, SlotResult, SourceFilters, StoredImport, StoredSpec, Transition, Transitioned,
};
use crate::server_fns::download::test_support::{group, item, transfer};
use shared::download::{DownloadProgress, DownloadState, JobSlot, SearchStats};
use shared::failure::{Failure, FailureCode, RetryAdvice, Service, Stage};
use soulbeet::{DriverChoice, ImportMode, ImportOutcome, ImportedItem, SkipReason};
use sqlx::SqlitePool;
use std::path::Path;

/// alice's id, her folder's and her name in the ledger.
const ALICE: &str = "alice";
const ALICE_FOLDER: &str = "alice-folder";

/// A migrated database with the user alice and her one folder. The folder is the runner's
/// temporary directory, which exists already: preparing it as an import target writes nothing.
pub(crate) async fn test_db_with_alice() -> SqlitePool {
    let db = crate::db::test_db::migrated().await;
    sqlx::query("INSERT INTO users (id, username, password_hash) VALUES (?, ?, 'x')")
        .bind(ALICE)
        .bind(ALICE)
        .execute(&db)
        .await
        .unwrap();
    sqlx::query("INSERT INTO folders (id, user_id, name, path) VALUES (?, ?, 'Music', ?)")
        .bind(ALICE_FOLDER)
        .bind(ALICE)
        .bind(std::env::temp_dir().to_str().unwrap())
        .execute(&db)
        .await
        .unwrap();
    db
}

/// An automatic pick at 0.7, no filters, 4 sources per slot.
pub(crate) fn auto_options() -> JobOptions {
    JobOptions {
        pick: Pick::Auto,
        min_score: 0.7,
        filters: SourceFilters::default(),
        max_sources: 4,
    }
}

fn alice() -> UserId {
    UserId::new(ALICE)
}

/// alice's MCP job for `wanted` in a batch of its own, searching under a new search id, as
/// `ensure` makes one.
async fn mcp_job(acq: &Acquire, wanted: Wanted, options: JobOptions) -> String {
    mcp_job_of(acq, ALICE, wanted, options).await
}

/// `user`'s MCP job for `wanted` into their folder `{user}-folder` (`core::folder_of`'s), in a
/// batch of its own, searching under a new search id.
async fn mcp_job_of(acq: &Acquire, user: &str, wanted: Wanted, options: JobOptions) -> String {
    let label = wanted.label();
    let (_, mut jobs) = mcp_batch(acq, user, &label, vec![wanted], options).await;
    jobs.remove(0)
}

/// `user`'s MCP batch `label` with a job for each of `wanted`, in order, each searching under a
/// new search id: the batch's id and its jobs' ids.
async fn mcp_batch(
    acq: &Acquire,
    user: &str,
    label: &str,
    wanted: Vec<Wanted>,
    options: JobOptions,
) -> (String, Vec<String>) {
    let batch = NewJobBatch {
        id: uuid::Uuid::new_v4().to_string(),
        user: UserId::new(user),
        label: Some(label.to_string()),
        origin: BatchOrigin::Mcp,
    };
    let batch_id = batch.id.clone();
    let jobs: Vec<NewJob> = wanted
        .into_iter()
        .map(|wanted| NewJob {
            id: uuid::Uuid::new_v4().to_string(),
            user: UserId::new(user),
            folder_id: format!("{user}-folder"),
            wanted: wanted.query(),
            import_mode: match wanted.kind() {
                crate::ledger::JobKind::Track => ImportMode::Singleton,
                crate::ledger::JobKind::Album => ImportMode::Album,
            },
            state: JobState::Searching,
            search_id: Some(uuid::Uuid::new_v4().to_string()),
            picked: vec![],
            kind: wanted.kind(),
            options: Some(options.clone()),
        })
        .collect();
    let ids = jobs.iter().map(|job| job.id.clone()).collect::<Vec<_>>();
    let admitted = acq.ledger.create_batch(batch, jobs).await.unwrap();
    assert_eq!(admitted, vec![Admitted::Created; ids.len()]);
    (batch_id, ids)
}

/// alice's MCP batch `label` with one searching job, for Marrow Choir - `label`: the batch's id.
pub(crate) async fn named_batch(acq: &Acquire, label: &str) -> String {
    let wanted = vec![track("Marrow Choir", label)];
    mcp_batch(acq, ALICE, label, wanted, auto_options()).await.0
}

/// alice's MCP batch "Winter Songs" of `n` track jobs ("Song Number 1" and on), each
/// downloading as `start_downloading` leaves it: the batch's id and its jobs' ids.
pub(crate) async fn batch_of_downloads(acq: &Acquire, n: usize) -> (String, Vec<String>) {
    let titles: Vec<String> = (1..=n).map(|i| format!("Song Number {i}")).collect();
    let wanted = titles.iter().map(|t| track("Marrow Choir", t)).collect();
    let (batch, jobs) = mcp_batch(acq, ALICE, "Winter Songs", wanted, auto_options()).await;
    for (job, title) in jobs.iter().zip(&titles) {
        start_downloading(acq, job, title).await;
    }
    (batch, jobs)
}

/// alice's MCP batch "Winter Songs" of `n` searching track jobs ("Song Number 1" and on): the
/// batch's id.
pub(crate) async fn batch_of_searches(acq: &Acquire, n: usize) -> String {
    let wanted = (1..=n)
        .map(|i| track("Marrow Choir", &format!("Song Number {i}")))
        .collect();
    mcp_batch(acq, ALICE, "Winter Songs", wanted, auto_options())
        .await
        .0
}

/// The transfer of alice's downloading `job` failed with `failure` and no source is left: its
/// slot's row, failed, as the monitor reports it.
pub(crate) async fn fail_slot(acq: &Acquire, job: &str, failure: Failure) {
    let view = view_of(acq, job).await;
    let row = view.slots[0]
        .row
        .clone()
        .expect("a downloading slot has a row");
    acq.ledger.record(JobEvent::Slots {
        state: JobState::Failed,
        rows: vec![row.failed_with(failure)],
    });
    acq.ledger.settled().await;
}

fn track(artist: &str, title: &str) -> Wanted {
    Wanted::Track(Reference {
        artist: artist.into(),
        title: title.into(),
        album: None,
        recording_mbid: None,
        duration_s: None,
    })
}

/// alice's MCP job for the track `artist` - `title`, searching.
pub(crate) async fn mcp_track_job(
    acq: &Acquire,
    artist: &str,
    title: &str,
    options: JobOptions,
) -> String {
    mcp_job(acq, track(artist, title), options).await
}

/// `user`'s MCP job for Marrow Choir - O Holy Night with `auto_options`, searching; `user` and
/// their folder made with `core::folder_of`.
pub(crate) async fn track_job_of(acq: &Acquire, user: &str) -> String {
    let wanted = track("Marrow Choir", "O Holy Night");
    mcp_job_of(acq, user, wanted, auto_options()).await
}

/// alice's job `job` as the job tools read it.
pub(crate) async fn view_of(acq: &Acquire, job: &str) -> JobView {
    let select = JobSelect::Ids(vec![job.to_string()]);
    let mut views = jobs_view(&acq.db, &acq.ledger, &test_ctx(ALICE), &select)
        .await
        .unwrap();
    assert_eq!(views.len(), 1, "alice has job {job}");
    views.remove(0)
}

/// alice's MCP job for the album `artist` - `title`, release `release_mbid`, searching with
/// `auto_options`.
pub(crate) async fn mcp_album_job(
    acq: &Acquire,
    artist: &str,
    title: &str,
    release_mbid: &str,
) -> String {
    let wanted = Wanted::Album(AlbumRef {
        artist: artist.into(),
        title: title.into(),
        release_mbid: release_mbid.into(),
    });
    mcp_job(acq, wanted, auto_options()).await
}

async fn applied(acq: &Acquire, job: &str, change: Transition) {
    let done = acq.ledger.transition(&alice(), job, change).await.unwrap();
    assert!(matches!(done, Transitioned::Applied { .. }), "{done:?}");
}

/// `n` MCP track jobs of alice ("Song Number 1" and on), each picked by Claude: waiting for a
/// source choice, then queued with one file of peer_a, none holding a download place.
pub(crate) async fn queued_track_jobs(acq: &Acquire, n: usize) -> Vec<String> {
    let mut jobs = Vec::with_capacity(n);
    for i in 1..=n {
        let title = format!("Song Number {i}");
        let job = mcp_track_job(acq, "Marrow Choir", &title, auto_options()).await;
        applied(acq, &job, Transition::AwaitChoice).await;
        let file = format!("a/{i:02}.flac");
        let queue = Transition::Queue {
            items: vec![(0, item("peer_a", &title, &file))],
            search_id: None,
        };
        applied(acq, &job, queue).await;
        jobs.push(job);
    }
    jobs
}

/// `n` finished attempts of an earlier job of alice, each from `source` and failed by the peer
/// (the transfer stage, advice other_source): what D5 counts.
pub(crate) async fn record_failures(acq: &Acquire, source: &str, n: u32) {
    let job = mcp_track_job(acq, "Marrow Choir", "Earlier Song", auto_options()).await;
    let items = (0..n)
        .map(|slot| {
            (
                slot,
                item(source, "Earlier Song", &format!("e/{slot:02}.flac")),
            )
        })
        .collect();
    applied(acq, &job, Transition::Pick { items }).await;
    // slskd's text for a peer's daily failure limit (slskd Application.cs:808).
    let limit = Failure::new(
        FailureCode::TransferRejectedFailureLimit,
        Stage::Transfer,
        RetryAdvice::OtherSource,
        "Transfer rejected: Too many failed transfers today",
    );
    let rows = (0..n)
        .map(|slot| {
            let file = format!("e/{slot:02}.flac");
            DownloadProgress::failed(format!("t-{slot}"), source.into(), file, limit.clone())
                .with_job(JobSlot {
                    job: job.clone(),
                    slot,
                })
        })
        .collect();
    acq.ledger.record(JobEvent::Slots {
        state: JobState::Failed,
        rows,
    });
    acq.ledger.settled().await;
}

/// `downloading_track_job_with` "O Holy Night" and the `auto_options`.
pub(crate) async fn downloading_track_job(acq: &Acquire) -> String {
    downloading_track_job_with(acq, "O Holy Night", auto_options()).await
}

/// alice's MCP job for Marrow Choir - `title` with `options`, downloading as `start_downloading`
/// leaves it.
pub(crate) async fn downloading_track_job_with(
    acq: &Acquire,
    title: &str,
    options: JobOptions,
) -> String {
    let job = mcp_track_job(acq, "Marrow Choir", title, options).await;
    start_downloading(acq, &job, title).await;
    job
}

/// alice's searching track job `job` for Marrow Choir - `title`, downloading: its search found a
/// peer_a and a peer_b group, peer_a was picked, took a download place and was enqueued, and
/// one InProgress row came for it.
async fn start_downloading(acq: &Acquire, job: &str, title: &str) {
    let record = store::job_record(&acq.db, job, &alice())
        .await
        .unwrap()
        .unwrap();
    let search_id = record.search_id.unwrap();
    let spec = StoredSpec {
        filters: record.options.unwrap().filters,
        ..StoredSpec::default()
    };
    acq.ledger
        .record_search(NewSearch {
            id: search_id.clone(),
            user: alice(),
            backend: Some("slskd".into()),
            query: record.wanted,
            spec: Some(spec),
        })
        .await
        .unwrap();
    let picked = item("peer_a", title, &file_of("a", title));
    let groups = vec![
        group("peer_a", vec![picked.clone()], 0.9),
        group(
            "peer_b",
            vec![item("peer_b", title, &file_of("b", title))],
            0.8,
        ),
    ];
    acq.ledger
        .search_ended(&search_id, Ok((groups, SearchStats::default())))
        .await
        .unwrap();
    applied(
        acq,
        job,
        Transition::Pick {
            items: vec![(0, picked)],
        },
    )
    .await;
    assert!(acq
        .ledger
        .claim_download(job, MCP_DOWNLOADS_PER_USER)
        .await
        .unwrap());
    acq.ledger.record(JobEvent::Enqueued {
        job: job.to_string(),
        monitor_batch_id: uuid::Uuid::new_v4().to_string(),
        batch_label: "Come Over".into(),
        request: uuid::Uuid::new_v4().to_string(),
    });
    acq.ledger.record(JobEvent::Slots {
        state: JobState::Downloading,
        rows: vec![in_progress(job, title)],
    });
    acq.ledger.settled().await;
}

/// `downloading_track_job`, whose transfer from peer_a the peer rejected for a file it does not
/// share, and whose next source, carol_shares, was offline: the slot failed after two attempts.
pub(crate) async fn failed_track_job(acq: &Acquire) -> String {
    let job = downloading_track_job(acq).await;
    let title = "O Holy Night";
    // Soulseek.NET's text for a refused upload (src/SoulseekClient.cs:3310) with slskd's reason
    // for a file it does not share (src/slskd/Application.cs:580).
    let not_shared = "Transfer rejected: File not shared.";
    let rejected = Failure::new(
        FailureCode::TransferRejectedNotShared,
        Stage::Transfer,
        RetryAdvice::OtherSource,
        not_shared,
    )
    .with_cause(Service::Slskd, None, not_shared);
    acq.ledger.record(JobEvent::Slots {
        state: JobState::Downloading,
        rows: vec![in_progress(&job, title).failed_with(rejected)],
    });
    let next = item("carol_shares", title, &file_of("c", title));
    acq.ledger.record(JobEvent::Attempt {
        slot: slot_0(&job),
        candidate: Some(Box::new(next.clone())),
        request: Some(uuid::Uuid::new_v4().to_string()),
    });
    // Soulseek.NET's text for a peer that is not logged in (src/SoulseekClient.cs:3790).
    let not_online = "User carol_shares appears to be offline";
    let offline = Failure::new(
        FailureCode::PeerOffline,
        Stage::Transfer,
        RetryAdvice::OtherSource,
        not_online,
    )
    .with_cause(Service::Slskd, None, not_online);
    let row = transfer(
        "carol_shares",
        &next.id,
        &format!("t-{}", next.id),
        DownloadState::Queued,
    )
    .failed_with(offline)
    .with_job(slot_0(&job));
    acq.ledger.record(JobEvent::Slots {
        state: JobState::Failed,
        rows: vec![row],
    });
    acq.ledger.settled().await;
    job
}

/// A peer's file of `title`, in its folder `dir`.
pub(crate) fn file_of(dir: &str, title: &str) -> String {
    format!("{dir}/03 - {title}.flac")
}

/// The row of `job`'s transfer of `title` from peer_a, InProgress.
pub(crate) fn in_progress(job: &str, title: &str) -> DownloadProgress {
    let file = file_of("a", title);
    transfer(
        "peer_a",
        &file,
        &format!("t-{file}"),
        DownloadState::InProgress,
    )
    .with_job(slot_0(job))
}

fn slot_0(job: &str) -> JobSlot {
    JobSlot {
        job: job.to_string(),
        slot: 0,
    }
}

/// The import of `job`'s downloaded `source` into alice's folder, as its batch queues it:
/// skipping what the library has.
fn import_of(job: &str, title: &str, source: &Path) -> StoredImport {
    let row = in_progress(job, title).with_state(DownloadState::ImportQueued(1));
    StoredImport {
        sources: vec![source.to_path_buf()],
        target: std::env::temp_dir(),
        album: false,
        cleanup_root: None,
        choices: None,
        choice: DriverChoice::Auto,
        rows: vec![(slot_0(job), row)],
        duplicates: DuplicateRule::Skip,
    }
}

/// `downloading_track_job_with` `title` whose file `source` finished and is queued for import.
pub(crate) async fn importing_track_job(acq: &Acquire, title: &str, source: &Path) -> String {
    let job = downloading_track_job_with(acq, title, auto_options()).await;
    let import = import_of(&job, title, source);
    acq.ledger.record(JobEvent::ImportQueued(Box::new(import)));
    acq.ledger.settled().await;
    job
}

/// `importing_track_job` whose import beets skipped because the library has `existing`: the slot
/// fails with them as its result, as the import queue's finish records a skipped duplicate.
pub(crate) async fn duplicate_track_job(
    acq: &Acquire,
    title: &str,
    source: &Path,
    existing: Vec<ImportedItem>,
) -> String {
    let job = importing_track_job(acq, title, source).await;
    let import = import_of(&job, title, source);
    let skipped = ImportOutcome::Skipped(SkipReason::AlreadyInLibrary {
        existing: existing.clone(),
    });
    let failure = Failure::new(
        FailureCode::ImportAlreadyInLibrary,
        Stage::Import,
        RetryAdvice::NotRetryable,
        skipped.to_string(),
    );
    let row = in_progress(&job, title)
        .with_state(DownloadState::ImportSkipped)
        .with_failure(failure);
    acq.ledger
        .record(JobEvent::ImportDone(Box::new(crate::ledger::ImportDone {
            import,
            rows: vec![row],
            next: JobState::Failed,
            index: None,
            results: vec![(slot_0(&job), SlotResult::AlreadyInLibrary { existing })],
        })));
    acq.ledger.settled().await;
    job
}

/// `importing_track_job` whose import beets skipped with nothing to pick: the import is kept
/// for a decision, as the import queue's finish records one.
pub(crate) async fn kept_track_job(acq: &Acquire, title: &str, source: &Path) -> String {
    let job = importing_track_job(acq, title, source).await;
    let import = import_of(&job, title, source);
    let row = in_progress(&job, title).with_state(DownloadState::ImportSkipped);
    acq.ledger
        .record(JobEvent::ImportDone(Box::new(crate::ledger::ImportDone {
            import,
            rows: vec![row],
            next: JobState::NeedsDecision,
            index: None,
            results: vec![],
        })));
    acq.ledger.settled().await;
    job
}
