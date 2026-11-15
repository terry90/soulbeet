//! The index wait: after an import, whether Navidrome lists the imported files (design 11.7).

use crate::auth::UserId;
use crate::globals::send_user_event;
use crate::ledger::store::{self, IndexingSlot};
use crate::ledger::{IndexWait, JobEvent, JobState, Ledger, ScanBaseline};
use crate::services::NavidromeLookupError;
use async_trait::async_trait;
use shared::download::{DownloadEvent, DownloadProgress, IndexState, JobSlot};
use shared::failure::{
    Failure, FailureCode, RetryAdvice, Service, Stage, UserAction, UserActionCode,
};
use soulbeet::navidrome::error::NOT_AUTHORIZED;
use soulbeet::navidrome::models::NativeSong;
use soulbeet::navidrome::{NavidromeClient, ScanStatus};
use soulbeet::NavidromeError;
use sqlx::SqlitePool;
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use tracing::{debug, warn};

const INDEX_POLL: Duration = Duration::from_secs(5);
const INDEX_GIVE_UP_SECS: i64 = 10 * 60;
const LOOKUP_PAGE: u32 = 100;

pub(crate) enum Step {
    Listed,
    Wait(IndexWait),
    StartScan(IndexWait),
    Fail(Failure),
}

/// What to do with an import, given the scan status just read and whether its items were found (D8):
/// scanning: Wait unchanged; found: Listed; no baseline: StartScan with the baseline set from
/// status (last_scan, last_start); ended_after(baseline.last_scan): Fail(not_found_after_scan);
/// last_start later than baseline.started and error set: Fail(scan_failed(&error));
/// INDEX_GIVE_UP_SECS since `since`: Fail(no_scan_possible); else Wait unchanged.
pub(crate) fn next_step(wait: &IndexWait, status: &ScanStatus, found: bool, now: i64) -> Step {
    // None while Navidrome scans: a scan already running may have walked past the file.
    let Some(started) = status.last_start() else {
        return Step::Wait(wait.clone());
    };
    if found {
        return Step::Listed;
    }
    match wait.baseline {
        None => Step::StartScan(IndexWait {
            baseline: Some(ScanBaseline {
                last_scan: status.last_scan,
                started,
            }),
            ..wait.clone()
        }),
        Some(baseline) if status.ended_after(baseline.last_scan) => {
            Step::Fail(not_found_after_scan())
        }
        // A failed scan stores its error and leaves lastScan as it was; an error from before
        // the baseline, with no scan begun since, is stale.
        Some(baseline) if started > baseline.started && !status.error.is_empty() => {
            Step::Fail(scan_failed(&status.error))
        }
        _ if now - wait.since >= INDEX_GIVE_UP_SECS => Step::Fail(no_scan_possible()),
        _ => Step::Wait(wait.clone()),
    }
}

/// Whether `song` is the imported file at `item` (relative to its library) of `size` bytes:
/// the shorter of the two paths, by components, ends the longer, and the sizes are equal.
pub(crate) fn is_imported_file(song: &NativeSong, item: &Path, size: u64) -> bool {
    let listed = Path::new(&song.path);
    song.size == size && (listed.ends_with(item) || item.ends_with(listed))
}

/// Whether every item of `wait` is listed: each looked up by its recording MBID, then by
/// "{artist} {title}" (D7), paging LOOKUP_PAGE at a time until a short page or a match, each
/// candidate checked with is_imported_file against the size of wait.library.join(item.path) on
/// disk. An item whose file cannot be read is not listed.
pub(crate) async fn listed(
    navi: &NavidromeClient,
    wait: &IndexWait,
) -> Result<bool, NavidromeError> {
    for item in &wait.items {
        let Ok(file) = tokio::fs::metadata(wait.library.join(&item.path)).await else {
            return Ok(false);
        };
        let texts = item
            .recording_mbid
            .iter()
            .cloned()
            .chain([format!("{} {}", item.artist, item.title)]);
        let mut found = false;
        for text in texts {
            if lists(navi, &text, &item.path, file.len()).await? {
                found = true;
                break;
            }
        }
        if !found {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Whether a song the native `title` filter matches for `text` is the file at `item`.
async fn lists(
    navi: &NavidromeClient,
    text: &str,
    item: &Path,
    size: u64,
) -> Result<bool, NavidromeError> {
    let mut start = 0;
    loop {
        let page = navi.find_songs(text, start, start + LOOKUP_PAGE).await?;
        if page.iter().any(|song| is_imported_file(song, item, size)) {
            return Ok(true);
        }
        if page.len() < LOOKUP_PAGE as usize {
            return Ok(false);
        }
        start += LOOKUP_PAGE;
    }
}

pub(crate) fn not_found_after_scan() -> Failure {
    Failure::new(
        FailureCode::IndexNotFoundAfterScan,
        Stage::Index,
        RetryAdvice::NotRetryable,
        "Navidrome scanned the library after this import and does not list the file",
    )
}

pub(crate) fn no_scan_possible() -> Failure {
    Failure::new(
        FailureCode::IndexNoScanPossible,
        Stage::Index,
        RetryAdvice::NeedsUser,
        "Navidrome has not scanned for 10 minutes since this import and this account cannot start a scan",
    )
    .with_user_action(UserAction::new(
        UserActionCode::EnableIndexing,
        "Make this Navidrome account an admin, enable Navidrome's file watcher, or set a scan schedule",
        "Navidrome",
    ))
}

pub(crate) fn scan_failed(error: &str) -> Failure {
    Failure::new(
        FailureCode::ScanFailed,
        Stage::Index,
        RetryAdvice::SameSourceLater,
        format!("Navidrome's scan failed: {error}"),
    )
    .with_cause(Service::Navidrome, None, error)
}

/// Where a round gets a user's Navidrome. The server's is navidrome_client_for_user.
#[async_trait]
pub(crate) trait NavidromeOf: Send + Sync {
    async fn navidrome_of(
        &self,
        user: &UserId,
    ) -> Result<Arc<NavidromeClient>, NavidromeLookupError>;
}

struct ServerNavidrome;

#[async_trait]
impl NavidromeOf for ServerNavidrome {
    async fn navidrome_of(
        &self,
        user: &UserId,
    ) -> Result<Arc<NavidromeClient>, NavidromeLookupError> {
        crate::services::navidrome_client_for_user(&user.to_string()).await
    }
}

/// What a runner holds between rounds, by slot: the wait as it last persisted it, or None once
/// the slot ended (Done or Failed), so a round never acts on an older copy its own write has not
/// replaced in the database yet. A round drops the slots indexing_slots no longer returns.
#[derive(Default)]
pub(crate) struct IndexRunner {
    waits: HashMap<JobSlot, Option<IndexWait>>,
}

/// The slots of one import, waiting together, and the wait as the runner knows it.
struct Import {
    wait: IndexWait,
    rows: Vec<DownloadProgress>,
}

impl Import {
    fn slots(&self) -> Vec<JobSlot> {
        self.rows.iter().filter_map(|row| row.job.clone()).collect()
    }
}

/// One round at `now`: reads indexing_slots, groups them by user and import, and for each user asks
/// `navidrome` for a client and reads the scan status once.
/// NotLinked/NoServer: the slots are Done, rows unchanged (D9).
/// Another lookup or status error: Wait, and Fail with e.failure(Stage::Index, e.to_string())
/// once INDEX_GIVE_UP_SECS have passed.
/// A slot the runner holds no wait for gets its row sent and recorded (Slots { Indexing }) with
/// index Waiting. listed() runs when Navidrome is idle and its lastScan differs from `checked`
/// (which it then becomes). StartScan calls start_scan; a refusal (Subsonic 50, `StartScan`'s
/// ErrorAuthorizationFail, server/subsonic/library_scanning.go:43-44) leaves the wait to the watcher
/// or a schedule. Another start_scan error keeps the wait without the new baseline, so the next
/// idle round asks again, and fails it as a status error does once given up. Listed: rows with index Listed, Slots { Done }. Fail(f): rows with index
/// Failed(f), Slots { Failed }. A changed wait: Indexing. Rows go out with send_user_event.
pub(crate) async fn index_round(
    db: &SqlitePool,
    ledger: &Ledger,
    navidrome: &dyn NavidromeOf,
    runner: &mut IndexRunner,
    now: i64,
) {
    let slots = match store::indexing_slots(db).await {
        Ok(slots) => slots,
        Err(e) => {
            warn!("Could not read the imports waiting for Navidrome: {e}");
            return;
        }
    };
    runner
        .waits
        .retain(|held, _| slots.iter().any(|s| &s.slot == held));
    for (user, imports, new) in group(runner, slots) {
        let navi = match navidrome.navidrome_of(&user).await {
            Err(NavidromeLookupError::NotLinked | NavidromeLookupError::NoServer) => {
                for import in imports {
                    end(ledger, runner, &user, import, JobState::Done, None).await;
                }
                continue;
            }
            navi => navi,
        };
        if !new.is_empty() {
            let rows = new
                .into_iter()
                .map(|row| DownloadProgress {
                    index: Some(IndexState::Waiting),
                    ..row
                })
                .collect::<Vec<_>>();
            ledger.record(JobEvent::Slots {
                state: JobState::Indexing,
                rows: rows.clone(),
            });
            send_user_event(&user, DownloadEvent::Progress(rows)).await;
        }
        for import in &imports {
            for slot in import.slots() {
                runner.waits.insert(slot, Some(import.wait.clone()));
            }
        }
        let read = match navi {
            Ok(navi) => match navi.get_scan_status().await {
                Ok(status) => Ok((navi, status)),
                Err(e) => Err(e.failure(Stage::Index, e.to_string())),
            },
            Err(e) => Err(e.failure(Stage::Index, e.to_string())),
        };
        match read {
            Ok((navi, status)) => {
                for import in imports {
                    advance(ledger, runner, &user, &navi, &status, import, now).await;
                }
            }
            Err(failure) => {
                for import in imports {
                    let held = import.wait.clone();
                    give_up_after(ledger, runner, &user, import, held, &failure, now).await;
                }
            }
        }
    }
}

/// The slots of a round by user, each user's by import, and the rows of the slots the runner
/// held nothing for. A slot that ended is left out.
fn group(
    runner: &IndexRunner,
    slots: Vec<IndexingSlot>,
) -> Vec<(UserId, Vec<Import>, Vec<DownloadProgress>)> {
    let mut users: Vec<(UserId, Vec<Import>, Vec<DownloadProgress>)> = Vec::new();
    for IndexingSlot {
        slot,
        user,
        row,
        wait,
    } in slots
    {
        let wait = match runner.waits.get(&slot) {
            Some(None) => continue,
            Some(Some(held)) => held.clone(),
            None => wait,
        };
        let at = match users.iter().position(|(u, ..)| *u == user) {
            Some(at) => at,
            None => {
                users.push((user, Vec::new(), Vec::new()));
                users.len() - 1
            }
        };
        let (_, imports, new) = &mut users[at];
        if !runner.waits.contains_key(&slot) {
            new.push(row.clone());
        }
        match imports.iter_mut().find(|i| i.wait.import == wait.import) {
            Some(import) => import.rows.push(row),
            None => imports.push(Import {
                wait,
                rows: vec![row],
            }),
        }
    }
    users
}

/// Looks the import up when Navidrome scanned since it last did, and takes the next step.
async fn advance(
    ledger: &Ledger,
    runner: &mut IndexRunner,
    user: &UserId,
    navi: &NavidromeClient,
    status: &ScanStatus,
    import: Import,
    now: i64,
) {
    let mut wait = import.wait.clone();
    let mut found = false;
    if !status.scanning && wait.checked != Some(status.last_scan) {
        match listed(navi, &wait).await {
            Ok(listed) => {
                found = listed;
                wait.checked = Some(status.last_scan);
            }
            Err(e) => {
                let failure = e.failure(Stage::Index, e.to_string());
                let held = import.wait.clone();
                return give_up_after(ledger, runner, user, import, held, &failure, now).await;
            }
        }
    }
    match next_step(&wait, status, found, now) {
        Step::Listed => {
            end(
                ledger,
                runner,
                user,
                import,
                JobState::Done,
                Some(IndexState::Listed),
            )
            .await
        }
        Step::Fail(f) => {
            end(
                ledger,
                runner,
                user,
                import,
                JobState::Failed,
                Some(IndexState::Failed(f)),
            )
            .await
        }
        Step::StartScan(started) => match navi.start_scan().await {
            // A non-admin account is refused (Subsonic 50, server/subsonic/library_scanning.go:43-44):
            // Navidrome's watcher or its schedule may still scan.
            Ok(()) => keep(ledger, runner, import, started),
            Err(NavidromeError::Subsonic {
                code: NOT_AUTHORIZED,
                ..
            }) => {
                debug!("Navidrome does not let {user} start a scan");
                keep(ledger, runner, import, started);
            }
            // Any other error says nothing of the account: without the baseline the next idle
            // round asks again.
            Err(e) => {
                warn!("Could not ask Navidrome to scan for {user}: {e}");
                let failure = e.failure(Stage::Index, e.to_string());
                give_up_after(ledger, runner, user, import, wait, &failure, now).await;
            }
        },
        Step::Wait(wait) => keep(ledger, runner, import, wait),
    }
}

/// Fails the import with `failure` once the wait gave up; until then it keeps `wait`.
async fn give_up_after(
    ledger: &Ledger,
    runner: &mut IndexRunner,
    user: &UserId,
    import: Import,
    wait: IndexWait,
    failure: &Failure,
    now: i64,
) {
    if now - import.wait.since >= INDEX_GIVE_UP_SECS {
        let index = Some(IndexState::Failed(failure.clone()));
        end(ledger, runner, user, import, JobState::Failed, index).await;
    } else {
        keep(ledger, runner, import, wait);
    }
}

/// Records `wait` for the import's slots when it changed.
fn keep(ledger: &Ledger, runner: &mut IndexRunner, import: Import, wait: IndexWait) {
    if wait == import.wait {
        return;
    }
    let slots = import.slots();
    for slot in &slots {
        runner.waits.insert(slot.clone(), Some(wait.clone()));
    }
    ledger.record(JobEvent::Indexing {
        slots,
        wait: Box::new(wait),
    });
}

/// Ends the import's slots in `state`; rows with `index` set go to the user, others stay as stored.
async fn end(
    ledger: &Ledger,
    runner: &mut IndexRunner,
    user: &UserId,
    import: Import,
    state: JobState,
    index: Option<IndexState>,
) {
    for slot in import.slots() {
        runner.waits.insert(slot, None);
    }
    let rows = match &index {
        Some(index) => import
            .rows
            .into_iter()
            .map(|row| DownloadProgress {
                index: Some(index.clone()),
                ..row
            })
            .collect(),
        None => import.rows,
    };
    ledger.record(JobEvent::Slots {
        state,
        rows: rows.clone(),
    });
    if index.is_some() {
        send_user_event(user, DownloadEvent::Progress(rows)).await;
    }
}

/// Spawns the loop: index_round(&DB, &LEDGER, the server's NavidromeOf, ..) every INDEX_POLL.
pub fn start() {
    tokio::spawn(async move {
        let mut runner = IndexRunner::default();
        let mut every = tokio::time::interval(INDEX_POLL);
        loop {
            every.tick().await;
            index_round(
                &crate::db::DB,
                &crate::ledger::LEDGER,
                &ServerNavidrome,
                &mut runner,
                chrono::Utc::now().timestamp(),
            )
            .await;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::UserId;
    use crate::ledger::{store, IndexWait, JobEvent, JobState, Ledger, NewJob, ScanBaseline};
    use crate::server_fns::download::test_support::item;
    use crate::services::NavidromeLookupError;
    use crate::test_navidrome::NavidromeStub;
    use async_trait::async_trait;
    use shared::download::{
        DownloadEvent, DownloadProgress, DownloadQuery, DownloadState, IndexState, JobSlot,
    };
    use shared::failure::{FailureCode, Stage, UserActionCode};
    use soulbeet::navidrome::models::NativeSong;
    use soulbeet::navidrome::{NavidromeClient, ScanStatus};
    use soulbeet::{ImportMode, ImportedItem};
    use sqlx::SqlitePool;
    use std::path::Path;
    use std::sync::Arc;
    use tokio::sync::broadcast;

    fn at(text: &str) -> chrono::DateTime<chrono::Utc> {
        text.parse().unwrap()
    }

    /// An idle status's elapsedTime is lastScan less the stored start (D8); a negative one means a
    /// scan began after the last completed one.
    fn status(scanning: bool, last: &str, elapsed_secs: i64, error: &str) -> ScanStatus {
        ScanStatus {
            scanning,
            count: 1342,
            last_scan: at(last),
            error: error.into(),
            elapsed_time: elapsed_secs * 1_000_000_000,
        }
    }

    const LOCKED: &str = "running GC: purging empty albums: database is locked"; // Task 2's sources

    fn waiting(since: i64) -> IndexWait {
        IndexWait {
            import: "/downloads/Come Over [1f2e3d4c]/07.flac".into(),
            library: "/music".into(),
            items: vec![ImportedItem {
                path: "Kowloon/Come Over/07 Wake Up.flac".into(),
                recording_mbid: None,
                artist: "Kowloon".into(),
                title: "Wake Up".into(),
            }],
            since,
            baseline: None,
            checked: None,
        }
    }

    #[test]
    fn a_scan_already_running_at_the_first_look_is_waited_out_and_not_believed() {
        let first = waiting(1_000);
        assert!(
            matches!(next_step(&first, &status(true, "2026-11-15T10:00:00Z", 9, ""), false, 1_005), Step::Wait(w) if w == first)
        );
        let Step::StartScan(started) = next_step(
            &first,
            &status(false, "2026-11-15T10:02:00Z", 4, ""),
            false,
            1_010,
        ) else {
            panic!("the first idle look starts a scan")
        };
        assert_eq!(
            started.baseline,
            Some(ScanBaseline {
                last_scan: at("2026-11-15T10:02:00Z"),
                started: at("2026-11-15T10:01:56Z")
            })
        );
        assert!(matches!(
            next_step(
                &started,
                &status(false, "2026-11-15T10:02:00Z", 4, ""),
                false,
                1_015
            ),
            Step::Wait(_)
        ));
        assert_eq!(
            match next_step(
                &started,
                &status(false, "2026-11-15T10:03:00Z", 3, ""),
                false,
                1_080
            ) {
                Step::Fail(f) => f,
                _ => panic!("a later scan without the file"),
            },
            not_found_after_scan()
        );
    }

    #[test]
    fn a_scan_that_fails_after_the_import_fails_the_wait_and_an_older_error_does_not() {
        // The scan before the import failed: its error is there at the first look.
        let Step::StartScan(started) = next_step(
            &waiting(1_000),
            &status(false, "2026-11-15T10:02:00Z", -20, LOCKED),
            false,
            1_005,
        ) else {
            panic!()
        };
        assert!(matches!(
            next_step(
                &started,
                &status(false, "2026-11-15T10:02:00Z", -20, LOCKED),
                false,
                1_010
            ),
            Step::Wait(_)
        ));
        // The scan Soulful asked for began at 10:02:30 and failed: lastScan unchanged, a later start.
        let Step::Fail(f) = next_step(
            &started,
            &status(false, "2026-11-15T10:02:00Z", -30, LOCKED),
            false,
            1_015,
        ) else {
            panic!("a scan that began after the import failed")
        };
        assert_eq!(f, scan_failed(LOCKED));
        assert_eq!(
            (f.code, f.message.as_str()),
            (
                FailureCode::ScanFailed,
                "Navidrome's scan failed: running GC: purging empty albums: database is locked"
            )
        );
    }

    #[test]
    fn the_wait_ends_listed_or_given_up() {
        let started = IndexWait {
            baseline: Some(ScanBaseline {
                last_scan: at("2026-11-15T10:02:00Z"),
                started: at("2026-11-15T10:01:56Z"),
            }),
            ..waiting(1_000)
        };
        assert!(matches!(
            next_step(
                &started,
                &status(false, "2026-11-15T10:02:00Z", 4, ""),
                true,
                1_010
            ),
            Step::Listed
        ));
        assert!(matches!(
            next_step(
                &started,
                &status(false, "2026-11-15T10:02:00Z", 4, ""),
                false,
                1_000 + INDEX_GIVE_UP_SECS - 1
            ),
            Step::Wait(_)
        ));
        let Step::Fail(f) = next_step(
            &started,
            &status(false, "2026-11-15T10:02:00Z", 4, ""),
            false,
            1_000 + INDEX_GIVE_UP_SECS,
        ) else {
            panic!()
        };
        assert_eq!(f, no_scan_possible());
        assert_eq!(
            f.user_action.map(|a| a.code),
            Some(UserActionCode::EnableIndexing)
        );
    }

    #[test]
    fn only_the_imported_file_counts_as_listed() {
        let song = |path: &str, size| NativeSong {
            id: "f2c4d1a8b0e94c37a6d5e8b1c2a3f405".into(),
            path: path.into(),
            title: "Song Number 1".into(),
            size,
        };
        let item = Path::new("Zzqx Artist/Qzz Album/01 Song Number 1.mp3");
        assert!(is_imported_file(
            &song("Zzqx Artist/Qzz Album/01 Song Number 1.mp3", 32032),
            item,
            32032
        ));
        assert!(is_imported_file(
            &song("alice/Zzqx Artist/Qzz Album/01 Song Number 1.mp3", 32032),
            item,
            32032
        ));
        assert!(is_imported_file(
            &song("Qzz Album/01 Song Number 1.mp3", 32032),
            item,
            32032
        ));
        assert!(!is_imported_file(
            &song("Zzqx Artist/Qzz Album/01 Song Number 1.mp3", 41000),
            item,
            32032
        ));
        assert!(!is_imported_file(
            &song("Zzqx Artist/Best Of/01 Song Number 1.mp3", 32032),
            item,
            32032
        ));
        assert!(!is_imported_file(
            &song("Zzqx Artist/Qzz Album/x01 Song Number 1.mp3", 32032),
            item,
            32032
        ));
    }

    /// A native song row (model/mediafile.go json tags, as Task 2's fixture).
    fn song_json(path: &str, size: u64) -> String {
        format!(
            r#"[{{"id":"f2c4d1a8b0e94c37a6d5e8b1c2a3f405","libraryId":1,"libraryPath":"/music","path":"{path}",
        "title":"Song Number 1","album":"Qzz Album","artist":"Zzqx Artist","size":{size},"suffix":"mp3","missing":false}}]"#
        )
    }

    /// `library` holding the imported file (4 bytes) and a wait for it, imported with a recording MBID.
    fn imported_in(library: &Path) -> IndexWait {
        let path = Path::new("Zzqx Artist/Qzz Album/01 Song Number 1.mp3");
        std::fs::create_dir_all(library.join(path).parent().unwrap()).unwrap();
        std::fs::write(library.join(path), b"mp3!").unwrap();
        IndexWait {
            library: library.to_path_buf(),
            items: vec![ImportedItem {
                path: path.into(),
                recording_mbid: Some("dfeb96a7-f78a-4b04-93ce-c39e28c9a173".into()),
                artist: "Zzqx Artist".into(),
                title: "Song Number 1".into(),
            }],
            ..waiting(1_000)
        }
    }

    #[tokio::test]
    async fn another_copy_of_the_recording_is_not_the_imported_file() {
        let library = tempfile::tempdir().unwrap();
        let wait = imported_in(library.path());
        let navi = NavidromeStub::start();
        // The MBID finds a copy of another size; the text finds one in another album.
        navi.answer(
            "/api/song",
            &[
                &song_json("Zzqx Artist/Qzz Album/01 Song Number 1.mp3", 41000),
                &song_json("Zzqx Artist/Best Of/01 Song Number 1.mp3", 4),
            ],
        );
        assert!(!listed(&navi.client(), &wait).await.unwrap());
        navi.answer(
            "/api/song",
            &[&song_json(
                "alice/Zzqx Artist/Qzz Album/01 Song Number 1.mp3",
                4,
            )],
        );
        assert!(listed(&navi.client(), &wait).await.unwrap());
    }

    const IDLE: &str = r#"{"subsonic-response":{"status":"ok","version":"1.16.1","type":"navidrome","serverVersion":"0.59.0","openSubsonic":true,"scanStatus":{"scanning":false,"count":1342,"folderCount":120,"lastScan":"2026-11-15T10:02:00Z","scanType":"quick","elapsedTime":4000000000}}}"#;

    struct Navidrome(Result<Arc<NavidromeClient>, fn() -> NavidromeLookupError>);

    #[async_trait]
    impl NavidromeOf for Navidrome {
        async fn navidrome_of(
            &self,
            _: &UserId,
        ) -> Result<Arc<NavidromeClient>, NavidromeLookupError> {
            self.0.clone().map_err(|e| e())
        }
    }

    /// A user of its own (the user channels are process-wide) with job j1 whose slot 0 is
    /// imported and waiting as `wait`, its row as finish() sends it.
    async fn indexing(
        wait: IndexWait,
    ) -> (
        Ledger,
        SqlitePool,
        broadcast::Receiver<DownloadEvent>,
        UserId,
    ) {
        let db = crate::db::test_db::migrated().await;
        let user = format!("alice-{}", uuid::Uuid::new_v4());
        let folder = crate::core::folder_of(&db, &user).await;
        let ledger = Ledger::start(db.clone());
        let user = UserId::new(&user);
        let rx = crate::globals::get_or_create_user_channel(&user)
            .await
            .subscribe();
        ledger
            .create(
                NewJob {
                    id: "j1".into(),
                    user: user.clone(),
                    folder_id: folder,
                    wanted: DownloadQuery::new(vec![]),
                    import_mode: ImportMode::Singleton,
                    state: JobState::Queued,
                    search_id: None,
                },
                None,
            )
            .await
            .unwrap();
        let slot = JobSlot {
            job: "j1".into(),
            slot: 0,
        };
        ledger.record(JobEvent::Picked {
            job: "j1".into(),
            items: vec![(0, item("peer_a", "Song Number 1", "01.mp3"))],
        });
        let row = DownloadProgress::queued("t-01".into(), "peer_a".into(), "01.mp3".into(), 4)
            .with_state(DownloadState::Imported)
            .with_job(slot.clone());
        ledger.record(JobEvent::Slots {
            state: JobState::Indexing,
            rows: vec![row],
        });
        ledger.record(JobEvent::Indexing {
            slots: vec![slot],
            wait: Box::new(wait),
        });
        ledger.settled().await;
        (ledger, db, rx, user)
    }

    fn indexes(rx: &mut broadcast::Receiver<DownloadEvent>) -> Vec<Option<IndexState>> {
        std::iter::from_fn(|| rx.try_recv().ok())
            .flat_map(|e| match e {
                DownloadEvent::Progress(rows) => rows.into_iter().map(|r| r.index).collect(),
                _ => vec![],
            })
            .collect()
    }

    #[tokio::test]
    async fn an_import_navidrome_lists_ends_done_and_its_row_says_so() {
        let library = tempfile::tempdir().unwrap();
        let (ledger, db, mut rx, _) = indexing(imported_in(library.path())).await;
        let navi = NavidromeStub::start();
        navi.answer("/rest/getScanStatus", &[IDLE]);
        navi.answer(
            "/api/song",
            &[&song_json(
                "alice/Zzqx Artist/Qzz Album/01 Song Number 1.mp3",
                4,
            )],
        );
        index_round(
            &db,
            &ledger,
            &Navidrome(Ok(navi.client())),
            &mut IndexRunner::default(),
            1_005,
        )
        .await;
        ledger.settled().await;
        assert_eq!(store::job_state(&db, "j1").await, Some(JobState::Done));
        assert_eq!(
            indexes(&mut rx),
            vec![Some(IndexState::Waiting), Some(IndexState::Listed)]
        );
        assert!(!navi.asked().contains(&"/rest/startScan".to_string()));
    }

    #[tokio::test]
    async fn an_import_not_yet_listed_asks_for_a_scan_and_waits() {
        let library = tempfile::tempdir().unwrap();
        let (ledger, db, mut rx, _) = indexing(imported_in(library.path())).await;
        let navi = NavidromeStub::start();
        navi.answer("/rest/getScanStatus", &[IDLE]);
        navi.answer("/api/song", &["[]"]);
        index_round(
            &db,
            &ledger,
            &Navidrome(Ok(navi.client())),
            &mut IndexRunner::default(),
            1_005,
        )
        .await;
        ledger.settled().await;
        assert_eq!(store::job_state(&db, "j1").await, Some(JobState::Indexing));
        assert!(navi.asked().contains(&"/rest/startScan".to_string()));
        let [slot] = &store::indexing_slots(&db).await.unwrap()[..] else {
            panic!()
        };
        assert_eq!(
            slot.wait.baseline.map(|b| b.started),
            Some("2026-11-15T10:01:56Z".parse().unwrap())
        );
        assert_eq!(indexes(&mut rx), vec![Some(IndexState::Waiting)]);
    }

    fn scans_asked(navi: &NavidromeStub) -> usize {
        navi.asked()
            .iter()
            .filter(|path| *path == "/rest/startScan")
            .count()
    }

    #[tokio::test]
    async fn an_account_refused_a_scan_is_not_asked_again() {
        let library = tempfile::tempdir().unwrap();
        let (ledger, db, _rx, _) = indexing(imported_in(library.path())).await;
        let navi = NavidromeStub::start();
        navi.answer("/rest/getScanStatus", &[IDLE]);
        navi.answer("/api/song", &["[]"]);
        // StartScan's ErrorAuthorizationFail (server/subsonic/library_scanning.go:43-44, responses/errors.go:20).
        navi.answer(
            "/rest/startScan",
            &[r#"{"subsonic-response":{"status":"failed","version":"1.16.1","type":"navidrome","serverVersion":"0.59.0","openSubsonic":true,"error":{"code":50,"message":"User is not authorized for the given operation"}}}"#],
        );
        let mut runner = IndexRunner::default();
        for now in [1_005, 1_010] {
            index_round(
                &db,
                &ledger,
                &Navidrome(Ok(navi.client())),
                &mut runner,
                now,
            )
            .await;
            ledger.settled().await;
        }
        assert_eq!(scans_asked(&navi), 1);
        let [slot] = &store::indexing_slots(&db).await.unwrap()[..] else {
            panic!()
        };
        assert!(slot.wait.baseline.is_some());
    }

    #[tokio::test]
    async fn a_scan_request_that_fails_otherwise_is_asked_again_until_the_wait_gives_up() {
        let library = tempfile::tempdir().unwrap();
        let (ledger, db, mut rx, _) = indexing(imported_in(library.path())).await;
        let navi = NavidromeStub::start();
        navi.answer("/rest/getScanStatus", &[IDLE]);
        navi.answer("/api/song", &["[]"]);
        // StartScan's ErrorGeneric (server/subsonic/library_scanning.go:40).
        navi.answer(
            "/rest/startScan",
            &[r#"{"subsonic-response":{"status":"failed","version":"1.16.1","type":"navidrome","serverVersion":"0.59.0","openSubsonic":true,"error":{"code":0,"message":"Internal error"}}}"#],
        );
        let mut runner = IndexRunner::default();
        index_round(
            &db,
            &ledger,
            &Navidrome(Ok(navi.client())),
            &mut runner,
            1_005,
        )
        .await;
        ledger.settled().await;
        let [slot] = &store::indexing_slots(&db).await.unwrap()[..] else {
            panic!()
        };
        assert_eq!(
            (slot.wait.baseline, slot.wait.checked),
            (None, Some(at("2026-11-15T10:02:00Z")))
        );
        index_round(
            &db,
            &ledger,
            &Navidrome(Ok(navi.client())),
            &mut runner,
            1_010,
        )
        .await;
        ledger.settled().await;
        assert_eq!(scans_asked(&navi), 2);
        assert_eq!(store::job_state(&db, "j1").await, Some(JobState::Indexing));
        index_round(
            &db,
            &ledger,
            &Navidrome(Ok(navi.client())),
            &mut runner,
            1_000 + INDEX_GIVE_UP_SECS,
        )
        .await;
        ledger.settled().await;
        assert_eq!(store::job_state(&db, "j1").await, Some(JobState::Failed));
        let failed = indexes(&mut rx).into_iter().last().flatten();
        assert!(
            matches!(failed, Some(IndexState::Failed(ref f)) if f.stage == Stage::Index && f.message.contains("Internal error")),
            "{failed:?}"
        );
    }

    #[tokio::test]
    async fn a_user_without_navidrome_has_the_import_done_without_a_note() {
        let library = tempfile::tempdir().unwrap();
        let (ledger, db, mut rx, _) = indexing(imported_in(library.path())).await;
        index_round(
            &db,
            &ledger,
            &Navidrome(Err(|| NavidromeLookupError::NotLinked)),
            &mut IndexRunner::default(),
            1_005,
        )
        .await;
        ledger.settled().await;
        assert_eq!(store::job_state(&db, "j1").await, Some(JobState::Done));
        assert!(indexes(&mut rx).is_empty());
    }

    #[tokio::test]
    async fn a_navidrome_that_cannot_be_read_fails_the_wait_once_it_gives_up() {
        let library = tempfile::tempdir().unwrap();
        let (ledger, db, mut rx, _) = indexing(imported_in(library.path())).await;
        let navi = NavidromeStub::start();
        // getLastScanTime's error, as discovery's test stub answers it (server/subsonic/library_scanning.go:16-21).
        let unreadable = r#"{"subsonic-response":{"status":"failed","version":"1.16.1","error":{"code":0,"message":"Internal Error"}}}"#;
        navi.answer("/rest/getScanStatus", &[unreadable]);
        let mut runner = IndexRunner::default();
        index_round(
            &db,
            &ledger,
            &Navidrome(Ok(navi.client())),
            &mut runner,
            1_005,
        )
        .await;
        ledger.settled().await;
        assert_eq!(store::job_state(&db, "j1").await, Some(JobState::Indexing));
        index_round(
            &db,
            &ledger,
            &Navidrome(Ok(navi.client())),
            &mut runner,
            1_000 + INDEX_GIVE_UP_SECS,
        )
        .await;
        ledger.settled().await;
        assert_eq!(store::job_state(&db, "j1").await, Some(JobState::Failed));
        let failed = indexes(&mut rx).into_iter().last().flatten();
        assert!(
            matches!(failed, Some(IndexState::Failed(ref f)) if f.stage == Stage::Index && f.message.contains("Internal Error")),
            "{failed:?}"
        );
    }
}
