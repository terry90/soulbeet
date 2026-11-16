//! Ledger states of MCP jobs for tests, each built the way its code path builds it: through
//! `Ledger::{create_batch, record, transition, ..}` only. Every job id is a new uuid.

use super::runner::{Acquire, TestEffects, MCP_DOWNLOADS_PER_USER};
use super::{job_records, views_of, AlbumRef, JobSelect, JobView, Wanted};
use crate::auth::UserId;
use crate::core::index::not_found_after_scan;
use crate::core::library::{Reference, SongFacts};
use crate::core::test_ctx;
use crate::import_queue::test_support::{no_match, track_choices};
use crate::import_queue::ImportJob;
use crate::ledger::store;
use crate::ledger::{
    Admitted, BatchOrigin, DuplicateRule, IndexWait, JobEvent, JobOptions, JobState, NewJob,
    NewJobBatch, NewSearch, Pick, SlotResult, SourceFilters, StoredImport, StoredSpec, Transition,
    Transitioned,
};
use crate::server_fns::download::monitor::cancelled_on_request;
use crate::server_fns::download::process::{import_job, Downloaded};
use crate::server_fns::download::test_support::{group, item, transfer};
use crate::test_navidrome::native_song;
use shared::download::{
    DownloadProgress, DownloadQuery, DownloadState, DownloadableItem, IndexState, JobSlot,
    MatchCandidate, SearchStats,
};
use shared::failure::{Failure, FailureCode, RetryAdvice, Service, Stage};
use soulbeet::{
    DriverChoice, ImportMode, ImportOutcome, ImportResult, ImportedItem, SkipReason, SourceFile,
};
use sqlx::SqlitePool;
use std::path::Path;

/// alice's id, her folder's and her name in the ledger.
const ALICE: &str = "alice";
const ALICE_FOLDER: &str = "alice-folder";

/// The user alice and her one folder, at `library`, which exists already: preparing it as an
/// import target writes nothing.
pub(crate) async fn add_alice(db: &SqlitePool, library: &Path) {
    sqlx::query("INSERT INTO users (id, username, password_hash) VALUES (?, ?, 'x')")
        .bind(ALICE)
        .bind(ALICE)
        .execute(db)
        .await
        .unwrap();
    sqlx::query("INSERT INTO folders (id, user_id, name, path) VALUES (?, ?, 'Music', ?)")
        .bind(ALICE_FOLDER)
        .bind(ALICE)
        .bind(library.to_str().unwrap())
        .execute(db)
        .await
        .unwrap();
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
    let label = crate::ledger::items_label(&[&wanted.query()]).expect("one job");
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
    let ctx = test_ctx(ALICE);
    let records = job_records(&acq.db, &ctx, &select).await.unwrap();
    let mut views = views_of(&acq.db, &acq.ledger, &ctx, records).await.unwrap();
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

/// The search alice's job `job` searches with.
pub(crate) async fn search_of(acq: &Acquire, job: &str) -> String {
    let record = store::job_record(&acq.db, job, &alice()).await.unwrap();
    record
        .and_then(|job| job.search_id)
        .expect("a searching job has its search")
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
        let search_id = search_of(acq, &job).await;
        applied(acq, &job, Transition::AwaitChoice { search_id }).await;
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
    let search_id = search_of(acq, &job).await;
    applied(acq, &job, Transition::Pick { items, search_id }).await;
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
    let picked = search_found(acq, job, title).await;
    let search_id = search_of(acq, job).await;
    applied(
        acq,
        job,
        Transition::Pick {
            items: vec![(0, picked)],
            search_id,
        },
    )
    .await;
    downloading_from(acq, job, &item("peer_a", title, &file_of("a", title))).await;
}

/// alice's queued job `job` takes a download place, is enqueued and its file `from` sends an
/// InProgress row, as its runner and monitor record them.
pub(crate) async fn downloading_from(acq: &Acquire, job: &str, from: &DownloadableItem) {
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
    let row = transfer(
        &from.source,
        &from.id,
        &format!("t-{}", from.id),
        DownloadState::InProgress,
    )
    .with_job(slot_0(job));
    acq.ledger.record(JobEvent::Slots {
        state: JobState::Downloading,
        rows: vec![row],
    });
    acq.ledger.settled().await;
}

/// The search of alice's searching track job `job` for Marrow Choir - `title` ends with a peer_a
/// and a peer_b group, best first: peer_a's file.
async fn search_found(acq: &Acquire, job: &str, title: &str) -> DownloadableItem {
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
        .search_ended(&search_id, Ok((groups, Some(SearchStats::default()))))
        .await
        .unwrap();
    picked
}

/// alice's track job for Marrow Choir - "O Come All Ye Faithful" waiting for a source choice:
/// its search found a peer_a and a peer_b group.
pub(crate) async fn choosing_track_job(acq: &Acquire) -> String {
    let title = "O Come All Ye Faithful";
    let job = mcp_track_job(acq, "Marrow Choir", title, auto_options()).await;
    search_found(acq, &job, title).await;
    let search_id = search_of(acq, &job).await;
    applied(acq, &job, Transition::AwaitChoice { search_id }).await;
    job
}

/// `downloading_track_job_with` `title`, whose transfer from peer_a failed with `failure` and no
/// source was left.
pub(crate) async fn failed_track_job_with(acq: &Acquire, title: &str, failure: Failure) -> String {
    let job = downloading_track_job_with(acq, title, auto_options()).await;
    fail_slot(acq, &job, failure).await;
    job
}

/// A peer's refusal of a file it does not share, as slskd words it (Soulseek.NET
/// src/SoulseekClient.cs:3310, slskd src/slskd/Application.cs:580): retry another source.
fn not_shared() -> Failure {
    let text = "Transfer rejected: File not shared.";
    Failure::new(
        FailureCode::TransferRejectedNotShared,
        Stage::Transfer,
        RetryAdvice::OtherSource,
        text,
    )
    .with_cause(Service::Slskd, None, text)
}

/// Job `job`'s next source `from` was tried and failed with `failure`, no source left after it.
pub(crate) async fn failed_from(
    acq: &Acquire,
    job: &str,
    from: &DownloadableItem,
    failure: Failure,
) {
    acq.ledger.record(JobEvent::Attempt {
        slot: slot_0(job),
        candidate: Some(Box::new(from.clone())),
        request: Some(uuid::Uuid::new_v4().to_string()),
    });
    let row = transfer(
        &from.source,
        &from.id,
        &format!("t-{}", from.id),
        DownloadState::Queued,
    )
    .failed_with(failure)
    .with_job(slot_0(job));
    acq.ledger.record(JobEvent::Slots {
        state: JobState::Failed,
        rows: vec![row],
    });
    acq.ledger.settled().await;
}

/// A failed track job "Ding Dong Merrily" that tried both sources its search found.
pub(crate) async fn exhausted_track_job(acq: &Acquire) -> String {
    let title = "Ding Dong Merrily";
    let job = downloading_track_job_with(acq, title, auto_options()).await;
    acq.ledger.record(JobEvent::Slots {
        state: JobState::Downloading,
        rows: vec![in_progress(&job, title).failed_with(not_shared())],
    });
    let next = item("peer_b", title, &file_of("b", title));
    failed_from(acq, &job, &next, not_shared()).await;
    job
}

/// `downloading_track_job` "The First Noel", cancelled, its transfer stopped by its monitor.
pub(crate) async fn cancelled_track_job(acq: &Acquire) -> String {
    let job = downloading_track_job_with(acq, "The First Noel", auto_options()).await;
    applied(acq, &job, Transition::Cancel).await;
    stop_slots(acq, &job).await;
    job
}

/// The monitor stopped every slot of alice's job `job` that has a row: their rows, Cancelled.
pub(crate) async fn stop_slots(acq: &Acquire, job: &str) {
    let rows = (view_of(acq, job).await.slots.into_iter())
        .filter_map(|slot| slot.row)
        .map(|row| {
            row.with_state(DownloadState::Cancelled)
                .with_failure(cancelled_on_request())
        })
        .collect();
    acq.ledger.record(JobEvent::Slots {
        state: JobState::Cancelled,
        rows,
    });
    acq.ledger.settled().await;
}

/// What beets applied for Marrow Choir - `title`.
fn applied_item(title: &str) -> ImportedItem {
    ImportedItem {
        path: format!("Marrow Choir/Winter Songs/03 - {title}.flac").into(),
        recording_mbid: None,
        artist: "Marrow Choir".into(),
        title: title.into(),
        album: "Winter Songs".into(),
        beets_id: Some(412),
        release_mbid: None,
        match_per_mille: Some(980),
    }
}

/// `importing_track_job` "Joy to the World", imported from peer_a: done, as a user without
/// Navidrome's import ends.
pub(crate) async fn done_track_job(acq: &Acquire) -> String {
    let title = "Joy to the World";
    let source = Path::new("/downloads/Come Over [1f2e3d4c]/03 - Joy to the World.flac");
    let job = importing_track_job(acq, title, source).await;
    let row = in_progress(&job, title).with_state(DownloadState::Imported);
    acq.ledger
        .record(JobEvent::ImportDone(Box::new(crate::ledger::ImportDone {
            import: import_of(&job, title, source),
            rows: vec![row],
            next: JobState::Done,
            index: None,
            results: vec![(
                slot_0(&job),
                SlotResult::Imported {
                    applied: vec![applied_item(title)],
                    untagged: false,
                    listed: None,
                },
            )],
        })));
    acq.ledger.settled().await;
    job
}

/// alice's downloading track job `job` (one slot) imported, then listed by Navidrome as
/// `song_id`: done, as the import queue and the index runner record it.
pub(crate) async fn settle_done(acq: &Acquire, job: &str, song_id: &str) {
    let view = view_of(acq, job).await;
    let title = view.job.wanted.tracks[0].title.clone();
    let row = (view.slots[0].row.clone()).expect("a downloading slot has a row");
    let source = Path::new("/downloads/Come Over [1f2e3d4c]").join(format!("03 - {title}.flac"));
    let import = import_of(job, &title, &source);
    acq.ledger
        .record(JobEvent::ImportQueued(Box::new(import.clone())));
    let applied = vec![applied_item(&title)];
    let imported = |listed| SlotResult::Imported {
        applied: applied.clone(),
        untagged: false,
        listed,
    };
    let row = row.with_state(DownloadState::Imported);
    let wait = IndexWait {
        import: source.display().to_string(),
        library: std::env::temp_dir(),
        items: applied.clone(),
        since: 0,
        baseline: None,
        checked: None,
    };
    acq.ledger
        .record(JobEvent::ImportDone(Box::new(crate::ledger::ImportDone {
            import,
            rows: vec![row.clone()],
            next: JobState::Indexing,
            index: Some(wait),
            results: vec![(slot_0(job), imported(None))],
        })));
    let native = native_song(song_id, "Marrow Choir", &title, 263.4, None);
    let song: soulbeet::navidrome::models::NativeSong = serde_json::from_str(&native).unwrap();
    acq.ledger.record(JobEvent::Settled {
        state: JobState::Done,
        rows: vec![DownloadProgress {
            index: Some(IndexState::Listed),
            ..row
        }],
        results: vec![(slot_0(job), imported(Some(vec![SongFacts::from(&song)])))],
    });
    acq.ledger.settled().await;
}

/// `importing_track_job` "Away in a Manger", imported, whose wait for Navidrome failed: Navidrome
/// scanned and does not list the file, as the index runner records it.
pub(crate) async fn index_failed_track_job(acq: &Acquire) -> String {
    let title = "Away in a Manger";
    let source = Path::new("/downloads/Come Over [1f2e3d4c]/03 - Away in a Manger.flac");
    let job = importing_track_job(acq, title, source).await;
    let row = in_progress(&job, title).with_state(DownloadState::Imported);
    let wait = IndexWait {
        import: source.display().to_string(),
        library: std::env::temp_dir(),
        items: vec![applied_item(title)],
        since: 0,
        baseline: None,
        checked: None,
    };
    acq.ledger
        .record(JobEvent::ImportDone(Box::new(crate::ledger::ImportDone {
            import: import_of(&job, title, source),
            rows: vec![row.clone()],
            next: JobState::Indexing,
            index: Some(wait),
            results: vec![(
                slot_0(&job),
                SlotResult::Imported {
                    applied: vec![applied_item(title)],
                    untagged: false,
                    listed: None,
                },
            )],
        })));
    let lost = DownloadProgress {
        index: Some(IndexState::Failed(not_found_after_scan())),
        ..row
    };
    acq.ledger.record(JobEvent::Slots {
        state: JobState::Failed,
        rows: vec![lost],
    });
    acq.ledger.settled().await;
    job
}

/// `downloading_track_job`, whose transfer from peer_a the peer rejected for a file it does not
/// share, and whose next source, carol_shares, was offline: the slot failed after two attempts.
pub(crate) async fn failed_track_job(acq: &Acquire) -> String {
    let job = downloading_track_job(acq).await;
    let title = "O Holy Night";
    acq.ledger.record(JobEvent::Slots {
        state: JobState::Downloading,
        rows: vec![in_progress(&job, title).failed_with(not_shared())],
    });
    let next = item("carol_shares", title, &file_of("c", title));
    // Soulseek.NET's text for a peer that is not logged in (src/SoulseekClient.cs:3790).
    let not_online = "User carol_shares appears to be offline";
    let offline = Failure::new(
        FailureCode::PeerOffline,
        Stage::Transfer,
        RetryAdvice::OtherSource,
        not_online,
    )
    .with_cause(Service::Slskd, None, not_online);
    failed_from(acq, &job, &next, offline).await;
    job
}

/// alice's web UI job for Marrow Choir - O Holy Night, which picked peer_a's file and failed.
pub(crate) async fn failed_web_ui_job(acq: &Acquire) -> String {
    let job = uuid::Uuid::new_v4().to_string();
    let wanted = DownloadQuery::new(vec![crate::server_fns::download::test_support::track(
        "Marrow Choir",
        "O Holy Night",
    )]);
    acq.ledger
        .create(
            NewJob {
                id: job.clone(),
                user: alice(),
                folder_id: ALICE_FOLDER.into(),
                kind: crate::ledger::JobKind::of(&wanted),
                wanted,
                import_mode: ImportMode::Singleton,
                state: JobState::Queued,
                search_id: None,
                picked: vec![(0, item("peer_a", "O Holy Night", "a/03.flac"))],
                options: None,
            },
            None,
        )
        .await
        .unwrap();
    acq.ledger.record(JobEvent::Ended {
        job: job.clone(),
        state: JobState::Failed,
        failure: None,
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
        files: vec![source.to_path_buf()],
        target: std::env::temp_dir(),
        album: false,
        cleanup_root: None,
        choices: None,
        choice: DriverChoice::Auto,
        rows: vec![(slot_0(job), row)],
        duplicates: DuplicateRule::Skip,
        last_run: None,
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
/// for a decision, in `acq`'s queue and the ledger, as the import queue's finish keeps one.
/// `source` holds audio.
pub(crate) async fn kept_track_job(acq: &Acquire, title: &str, source: &Path) -> String {
    let job = importing_track_job(acq, title, source).await;
    keep(acq, import_of(&job, title, source), true).await;
    job
}

/// A run that skipped Song Number 1 for want of a close match after weighing `candidates`
/// MusicBrainz recordings (the best 0.612 alike, each next one 0.07 less), having read one file
/// tagged Zzqx Artist - Song Number 1.
pub(crate) fn no_match_with_files(candidates: u16) -> ImportResult {
    let weighed = (0..candidates)
        .map(|i| MatchCandidate {
            data_source: "MusicBrainz".into(),
            id: format!("6c1e4d9a-2b7f-4e83-9a51-0d3f8c7b2e{i:02}"),
            artist: "Zzqx Artist".into(),
            title: "Song Number 1".into(),
            album: Some("Qzz Album".into()),
            year: Some(2019),
            country: Some("XW".into()),
            disambig: None,
            per_mille: 612 - 70 * i,
            // beets' penalty names (beets/autotag/distance.py).
            differs: vec!["album".into(), "track_length".into()],
            length_secs: Some(214),
            tracks: None,
        })
        .collect();
    let read = SourceFile {
        path: "01 - Song Number 1.flac".into(),
        artist: "Zzqx Artist".into(),
        title: "Song Number 1".into(),
        album: "Qzz Album".into(),
        track: Some(1),
        length_secs: Some(214),
    };
    ImportResult {
        choices: Some(track_choices(weighed)),
        files: vec![read],
        ..no_match(candidates.into())
    }
}

/// alice's MCP job for Marrow Choir - Song Number 1 whose download, "01 - Song Number 1.flac" in
/// "Come Over [1f2e3d4c]" under `acq`'s download folder, the import queue ran with `result` (its
/// files read from that download) and kept for a decision, as the queue's finish records it.
/// `effects` are `acq`'s.
pub(crate) async fn kept_track_job_with(
    acq: &Acquire,
    effects: &TestEffects,
    mut result: ImportResult,
) -> String {
    let title = "Song Number 1";
    let job = downloading_track_job_with(acq, title, auto_options()).await;
    let source = (acq.download_base).join("Come Over [1f2e3d4c]/01 - Song Number 1.flac");
    std::fs::create_dir_all(source.parent().unwrap()).unwrap();
    std::fs::write(&source, b"flac").unwrap();
    for file in &mut result.files {
        file.path = source.clone();
    }
    effects.importer().script(result);
    let row = in_progress(&job, title).with_state(DownloadState::Completed);
    let import = import_job(
        Downloaded::File(source),
        vec![row],
        &acq.download_base.join("library"),
        &alice(),
        DuplicateRule::Skip,
        &acq.download_base,
    );
    acq.queue.submit(import).await.wait().await;
    acq.ledger.settled().await;
    job
}

/// The downloaded file of `kept_track_job_with` made one the OS refuses to delete: its path now
/// holds a folder, which unlink refuses (EISDIR) whoever runs the tests, where a read-only folder
/// would not stop root. Its path relative to `acq`'s download folder.
pub(crate) fn undeletable_kept_file(acq: &Acquire) -> &'static str {
    let file = "Come Over [1f2e3d4c]/01 - Song Number 1.flac";
    let path = acq.download_base.join(file);
    std::fs::remove_file(&path).unwrap();
    std::fs::create_dir(&path).unwrap();
    file
}

/// `import` ended with nothing to pick: kept for a decision in the ledger, and in `acq`'s queue
/// when `queued` (one a restart could not restore is in the ledger alone).
async fn keep(acq: &Acquire, import: StoredImport, queued: bool) {
    let rows = (import.rows.iter())
        .map(|(_, row)| row.clone().with_state(DownloadState::ImportSkipped))
        .collect();
    if queued {
        acq.queue
            .restore(ImportJob::from_stored(import.clone(), alice()))
            .await
            .expect("its sources hold audio");
    }
    acq.ledger
        .record(JobEvent::ImportDone(Box::new(crate::ledger::ImportDone {
            import,
            rows,
            next: JobState::NeedsDecision,
            index: None,
            results: vec![],
        })));
    acq.ledger.settled().await;
}

/// The release of alice's album jobs for Kowloon - Come Over.
const COME_OVER: &str = "6a09041b-0f79-3278-88d9-0552dd28d37f";

/// alice's album job for Kowloon - Come Over whose search read the tracklist `tracklist` and
/// found peer_a's folder of `picked` (titles, one file each), which were picked, took a download
/// place and finished downloading: the job and the slots' rows, Completed, slot by slot.
async fn downloaded_album_job(
    acq: &Acquire,
    tracklist: &[String],
    picked: &[String],
) -> (String, Vec<DownloadProgress>) {
    let job = mcp_album_job(acq, "Kowloon", "Come Over", COME_OVER).await;
    let record = store::job_record(&acq.db, &job, &alice())
        .await
        .unwrap()
        .unwrap();
    let search_id = record.search_id.unwrap();
    // The runner reads the tracklist into the query before the search runs.
    let mut query = record.wanted;
    query.tracks = (tracklist.iter())
        .map(|title| crate::server_fns::download::test_support::track("Kowloon", title))
        .collect();
    let spec = StoredSpec {
        filters: record.options.unwrap().filters,
        ..StoredSpec::default()
    };
    acq.ledger
        .record_search(NewSearch {
            id: search_id.clone(),
            user: alice(),
            backend: Some("slskd".into()),
            query,
            spec: Some(spec),
        })
        .await
        .unwrap();
    let files: Vec<DownloadableItem> = (picked.iter().enumerate())
        .map(|(i, title)| item("peer_a", title, &format!("a/{:02} - {title}.flac", i + 1)))
        .collect();
    let groups = vec![group("peer_a", files.clone(), 0.9)];
    acq.ledger
        .search_ended(&search_id, Ok((groups, Some(SearchStats::default()))))
        .await
        .unwrap();
    let items = (0..).zip(files.clone()).collect();
    applied(acq, &job, Transition::Pick { items, search_id }).await;
    assert!(acq
        .ledger
        .claim_download(&job, MCP_DOWNLOADS_PER_USER)
        .await
        .unwrap());
    let rows: Vec<DownloadProgress> = (0..)
        .zip(&files)
        .map(|(slot, file)| {
            let row = transfer(
                "peer_a",
                &file.id,
                &format!("t-{}", file.id),
                DownloadState::Completed,
            );
            row.with_job(JobSlot {
                job: job.clone(),
                slot,
            })
        })
        .collect();
    acq.ledger.record(JobEvent::Slots {
        state: JobState::Downloading,
        rows: rows.clone(),
    });
    acq.ledger.settled().await;
    (job, rows)
}

/// The import of the downloaded `rows` (each slot's file under /downloads) into alice's folder
/// as their batch queues it, skipping what the library has: an album whole when `album`.
fn import_of_rows(rows: &[DownloadProgress], album: bool) -> StoredImport {
    let files: Vec<std::path::PathBuf> = (rows.iter())
        .map(|row| Path::new("/downloads/Come Over").join(&row.item))
        .collect();
    let sources = match album {
        true => vec![Path::new("/downloads/Come Over").to_path_buf()],
        false => files.clone(),
    };
    StoredImport {
        sources,
        files,
        target: std::env::temp_dir(),
        album,
        cleanup_root: None,
        choices: None,
        choice: DriverChoice::Auto,
        rows: (rows.iter())
            .map(|row| {
                let queued = row.clone().with_state(DownloadState::ImportQueued(1));
                (row.job.clone().unwrap(), queued)
            })
            .collect(),
        duplicates: DuplicateRule::Skip,
        last_run: None,
    }
}

/// What beets applied or found for Kowloon - `title` of Come Over, as library item `beets_id`.
fn album_item(title: &str, beets_id: u64) -> ImportedItem {
    ImportedItem {
        path: format!("Kowloon/Come Over/{title}.flac").into(),
        recording_mbid: None,
        artist: "Kowloon".into(),
        title: title.into(),
        album: "Come Over".into(),
        beets_id: Some(beets_id),
        release_mbid: Some(COME_OVER.into()),
        match_per_mille: Some(960),
    }
}

/// alice's album job for Kowloon - Come Over, whose tracklist has Wake Up, Come Over and Ready to
/// Go; peer_a's files of the first two were downloaded and imported whole as the release, as a
/// user without Navidrome's import ends: done, Ready to Go taken by no slot.
pub(crate) async fn imported_album_job(acq: &Acquire) -> String {
    let tracklist = ["Wake Up", "Come Over", "Ready to Go"].map(String::from);
    let picked = &tracklist[..2];
    let (job, rows) = downloaded_album_job(acq, &tracklist, picked).await;
    let import = import_of_rows(&rows, true);
    acq.ledger
        .record(JobEvent::ImportQueued(Box::new(import.clone())));
    let applied = (picked.iter().zip(1..))
        .map(|(title, id)| album_item(title, id))
        .collect();
    let result = SlotResult::Imported {
        applied,
        untagged: false,
        listed: None,
    };
    acq.ledger
        .record(JobEvent::ImportDone(Box::new(crate::ledger::ImportDone {
            results: (rows.iter())
                .map(|row| (row.job.clone().unwrap(), result.clone()))
                .collect(),
            rows: (rows.into_iter())
                .map(|row| row.with_state(DownloadState::Imported))
                .collect(),
            import,
            next: JobState::Done,
            index: None,
        })));
    acq.ledger.settled().await;
    job
}

/// alice's album job for Kowloon - Come Over of `n` tracks ("Track 1" and on), each downloaded
/// from peer_a and imported on its own, as the web page imports an album track by track; beets
/// skipped each because the library has its own copy of it: every slot failed with its own
/// already_in_library result.
pub(crate) async fn duplicate_album_job(acq: &Acquire, n: usize) -> String {
    let tracklist: Vec<String> = (1..=n).map(|i| format!("Track {i}")).collect();
    let (job, rows) = downloaded_album_job(acq, &tracklist, &tracklist).await;
    for (row, (title, id)) in rows.into_iter().zip(tracklist.iter().zip(1..)) {
        let import = import_of_rows(std::slice::from_ref(&row), false);
        acq.ledger
            .record(JobEvent::ImportQueued(Box::new(import.clone())));
        let existing = vec![album_item(title, id)];
        let skipped = ImportOutcome::Skipped(SkipReason::AlreadyInLibrary {
            existing: existing.clone(),
        });
        let failure = Failure::new(
            FailureCode::ImportAlreadyInLibrary,
            Stage::Import,
            RetryAdvice::NotRetryable,
            skipped.to_string(),
        );
        let slot = row.job.clone().unwrap();
        acq.ledger
            .record(JobEvent::ImportDone(Box::new(crate::ledger::ImportDone {
                import,
                rows: vec![row
                    .with_state(DownloadState::ImportSkipped)
                    .with_failure(failure)],
                next: JobState::Failed,
                index: None,
                results: vec![(slot, SlotResult::AlreadyInLibrary { existing })],
            })));
    }
    acq.ledger.settled().await;
    job
}

/// alice's album job for Kowloon - Come Over whose two files, from peer_a, were imported apart,
/// each import kept for a decision: the job, and each slot's import, in `acq`'s queue only when
/// `queued` says so (one a restart could not restore is kept in the ledger alone). `sources`
/// hold audio.
pub(crate) async fn kept_album_job(
    acq: &Acquire,
    sources: [&Path; 2],
    queued: [bool; 2],
) -> String {
    let job = mcp_album_job(
        acq,
        "Kowloon",
        "Come Over",
        "6a09041b-0f79-3278-88d9-0552dd28d37f",
    )
    .await;
    let files = [
        item("peer_a", "Wake Up", "a/07.flac"),
        item("peer_a", "Come Over", "a/08.flac"),
    ];
    let search_id = search_of(acq, &job).await;
    applied(
        acq,
        &job,
        Transition::Pick {
            items: (0..).zip(files.clone()).collect(),
            search_id,
        },
    )
    .await;
    assert!(acq
        .ledger
        .claim_download(&job, MCP_DOWNLOADS_PER_USER)
        .await
        .unwrap());
    let rows: Vec<DownloadProgress> = (0..)
        .zip(&files)
        .map(|(slot, file)| {
            transfer(
                "peer_a",
                &file.id,
                &format!("t-{}", file.id),
                DownloadState::Completed,
            )
            .with_job(JobSlot {
                job: job.clone(),
                slot,
            })
        })
        .collect();
    acq.ledger.record(JobEvent::Slots {
        state: JobState::Downloading,
        rows: rows.clone(),
    });
    for ((row, source), queued) in rows.into_iter().zip(sources).zip(queued) {
        let row = row.with_state(DownloadState::ImportQueued(1));
        let slot = row.job.clone().unwrap();
        let import = StoredImport {
            sources: vec![source.to_path_buf()],
            files: vec![source.to_path_buf()],
            target: std::env::temp_dir(),
            album: true,
            cleanup_root: None,
            choices: None,
            choice: DriverChoice::Auto,
            rows: vec![(slot, row)],
            duplicates: DuplicateRule::Skip,
            last_run: None,
        };
        acq.ledger
            .record(JobEvent::ImportQueued(Box::new(import.clone())));
        keep(acq, import, queued).await;
    }
    job
}
