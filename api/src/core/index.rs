//! The index wait: after an import, whether Navidrome lists the imported files (design 11.7).

use super::delete_guard::paths;
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
use soulbeet::{ImportedItem, NavidromeError};
use sqlx::SqlitePool;
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use tracing::{debug, warn};

const INDEX_POLL: Duration = Duration::from_secs(5);
const INDEX_GIVE_UP_SECS: i64 = 10 * 60;
const LOOKUP_PAGE: u32 = 100;
/// How many pages of the songs Navidrome added last a file no text finds is looked for in.
const NEWEST_PAGES: u32 = 3;

pub(crate) enum Step {
    Listed,
    Wait(IndexWait),
    StartScan(IndexWait),
    Fail(Failure),
}

/// Whether a wait held since `since` gave up at `now`.
fn given_up(since: i64, now: i64) -> bool {
    now - since >= INDEX_GIVE_UP_SECS
}

/// What to do with an import, given the scan status just read, whether its items were found and
/// whether the wait gave up (D8): scanning: Wait unchanged; found: Listed; no baseline: StartScan
/// with the baseline set from status (last_scan, last_start); ended_after(baseline.last_scan):
/// Fail(not_found_after_scan); last_start later than baseline.started, and error set or
/// last_start later than last_scan: Fail(scan_failed(&error)); gave up: Fail(no_scan_possible);
/// else Wait unchanged.
pub(crate) fn next_step(wait: &IndexWait, status: &ScanStatus, found: bool, gave_up: bool) -> Step {
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
        // A scan begun after the baseline, Navidrome idle and lastScan unchanged, failed. A
        // failed phase stores its error and leaves lastScan as it was; failing to prepare the
        // libraries stores no error (scanner/scanner.go:130-135), and then the start is later
        // than lastScan: a completed scan sets lastScan after its start (scanner/scanner.go:
        // 292-326, persistence/library_repository.go:169-174), and a status read while one
        // completes still counts elapsedTime from its start (scanner/controller.go:126-147).
        // An error from before the baseline, with no scan begun since, is stale.
        Some(baseline)
            if started > baseline.started
                && (!status.error.is_empty() || started > status.last_scan) =>
        {
            Step::Fail(scan_failed(&status.error))
        }
        _ if gave_up => Step::Fail(no_scan_possible()),
        _ => Step::Wait(wait.clone()),
    }
}

/// Whether `song` is the imported file at `item` (relative to its library) of `size` bytes:
/// the shorter of the two paths, by components, ends the longer, and the sizes are equal.
pub(crate) fn is_imported_file(song: &NativeSong, item: &Path, size: u64) -> bool {
    let listed = Path::new(&song.path);
    // Every path ends with the empty one.
    let named = |path: &Path| path.components().next().is_some();
    named(listed)
        && named(item)
        && song.size == size
        && (listed.ends_with(item) || item.ends_with(listed))
}

/// What Navidrome's text sanitizer drops besides white space (utils/str/sanitize_strings.go:14-28):
/// slashes become spaces, the typographic quotes Clear maps to `'` and `"` (utils/str/str.go:9-12)
/// and the quotes, brackets, braces, parentheses and commas of ignoredCharsRegex go. Removing
/// accents (sanitize.Accents) only ever writes letters, and the words are split on white space.
const NAVIDROME_DROPS: [char; 28] = [
    '\\', '/', '\'', '"', '[', ']', '(', ')', '{', '}', ',', '\u{2018}', '\u{2019}', '\u{201b}',
    '\u{2032}', '\u{201c}', '\u{201d}', '\u{ff02}', '\u{3003}', '\u{2ee}', '\u{5f2}', '\u{1cd3}',
    '\u{2033}', '\u{2036}', '\u{2f6}', '\u{2ba}', '\u{2dd}', '\u{201f}',
];

/// Whether Navidrome's full-text filter keeps a word of `text`. A text it keeps nothing of is no
/// filter (persistence/sql_search.go:62-66): Navidrome drops it and lists every song
/// (persistence/sql_restful.go:26-35). Any other symbol is kept, so the band `!!!` is a word.
fn navidrome_keeps(text: &str) -> bool {
    text.chars()
        .any(|c| !c.is_whitespace() && !NAVIDROME_DROPS.contains(&c))
}

/// The words Navidrome's full-text filter finds `item` by, as Navidrome's full_text holds the
/// title, album and artist names it shows (persistence/mediafile_repository.go:59-62), each word a
/// word prefix there (persistence/sql_search.go:62-76): "{artist} {title}" when Navidrome keeps a
/// word of it, else the album when it keeps one of that. None when it keeps nothing of either.
fn lookup_text(item: &ImportedItem) -> Option<String> {
    [
        format!("{} {}", item.artist, item.title),
        item.album.clone(),
    ]
    .into_iter()
    .find(|text| navidrome_keeps(text))
    .map(|text| text.trim().to_string())
}

/// Whether every item of `wait` is listed: each looked up by its recording MBID, then by
/// lookup_text (D7), paging LOOKUP_PAGE at a time until a short page or a match, or, with no text
/// Navidrome keeps, among the NEWEST_PAGES pages of the songs it added last, where a file just
/// imported is. Each candidate is checked with is_imported_file against the size of
/// wait.library.join(item.path) on disk. An item whose file cannot be read is not listed. The
/// song found for an item teaches where its library is on this machine (paths::learn_in).
pub(crate) async fn listed(
    db: &SqlitePool,
    navi: &NavidromeClient,
    wait: &IndexWait,
) -> Result<bool, NavidromeError> {
    for item in &wait.items {
        let local = wait.library.join(&item.path);
        let Ok(file) = tokio::fs::metadata(&local).await else {
            return Ok(false);
        };
        let size = file.len();
        let mut found = match &item.recording_mbid {
            Some(mbid) => lists(navi, mbid, &item.path, size).await?,
            None => None,
        };
        if found.is_none() {
            found = match lookup_text(item) {
                Some(text) => lists(navi, &text, &item.path, size).await?,
                None => among_newest(navi, &item.path, size).await?,
            };
        }
        let Some(song) = found else {
            return Ok(false);
        };
        if let Err(e) = paths::learn_in(db, &song, &local).await {
            warn!(
                "Could not learn where Navidrome library {} is on this machine from {}: {e}",
                song.library_id,
                local.display()
            );
        }
    }
    Ok(true)
}

/// The song among the NEWEST_PAGES pages of LOOKUP_PAGE songs Navidrome added last that is the
/// file at `item` of `size` bytes.
async fn among_newest(
    navi: &NavidromeClient,
    item: &Path,
    size: u64,
) -> Result<Option<NativeSong>, NavidromeError> {
    for page in 0..NEWEST_PAGES {
        let start = page * LOOKUP_PAGE;
        let songs = navi.newest_songs(start, start + LOOKUP_PAGE).await?;
        let short = songs.len() < LOOKUP_PAGE as usize;
        if let Some(song) = songs
            .into_iter()
            .find(|song| is_imported_file(song, item, size))
        {
            return Ok(Some(song));
        }
        if short {
            return Ok(None);
        }
    }
    Ok(None)
}

/// The song the native `title` filter matches for `text` that is the file at `item`.
async fn lists(
    navi: &NavidromeClient,
    text: &str,
    item: &Path,
    size: u64,
) -> Result<Option<NativeSong>, NavidromeError> {
    page_through(navi, text, |page| {
        page.into_iter()
            .find(|song| is_imported_file(song, item, size))
    })
    .await
}

/// Every present song the user of `navi` sees that may be `item`: each page of the native
/// `title` filter for its recording MBID, then for lookup_text, each song once. None when
/// Navidrome's filter keeps nothing of either, since no search can then show every song that
/// may be the item.
pub(crate) async fn candidates(
    navi: &NavidromeClient,
    item: &ImportedItem,
) -> Result<Option<Vec<NativeSong>>, NavidromeError> {
    let texts: Vec<String> = item
        .recording_mbid
        .iter()
        .filter(|mbid| navidrome_keeps(mbid))
        .cloned()
        .chain(lookup_text(item))
        .collect();
    if texts.is_empty() {
        return Ok(None);
    }
    let (mut songs, mut seen) = (Vec::new(), HashSet::new());
    for text in &texts {
        page_through(navi, text, |page| {
            songs.extend(page.into_iter().filter(|song| seen.insert(song.id.clone())));
            None::<()>
        })
        .await?;
    }
    Ok(Some(songs))
}

/// Hands `take` each page of LOOKUP_PAGE songs the native `title` filter matches for `text`,
/// in order, until it gives Some or a page is short.
async fn page_through<T>(
    navi: &NavidromeClient,
    text: &str,
    mut take: impl FnMut(Vec<NativeSong>) -> Option<T>,
) -> Result<Option<T>, NavidromeError> {
    let mut start = 0;
    loop {
        let page = navi.find_songs(text, start, start + LOOKUP_PAGE).await?;
        let short = page.len() < LOOKUP_PAGE as usize;
        if let Some(found) = take(page) {
            return Ok(Some(found));
        }
        if short {
            return Ok(None);
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

/// Navidrome's last scan failed with `error`, empty when Navidrome stored none.
pub(crate) fn scan_failed(error: &str) -> Failure {
    if error.is_empty() {
        return Failure::new(
            FailureCode::ScanFailed,
            Stage::Index,
            RetryAdvice::SameSourceLater,
            "Navidrome's scan failed without giving a reason",
        );
    }
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
/// replaced in the database yet; when the runner first held it; and whether a failed scan request
/// for it was logged. A round drops the slots indexing_slots no longer returns.
#[derive(Default)]
pub(crate) struct IndexRunner {
    waits: HashMap<JobSlot, Option<IndexWait>>,
    /// On Soulful's clock. A wait a restart interrupted gets its give-up time from here.
    held_since: HashMap<JobSlot, i64>,
    scan_warned: HashSet<JobSlot>,
}

/// The slots of one import, waiting together, and the wait as the runner knows it.
struct Import {
    wait: IndexWait,
    rows: Vec<DownloadProgress>,
    /// The later of when the import ended and when the runner first held it: a downtime does not
    /// count against the wait.
    since: i64,
}

impl Import {
    fn slots(&self) -> Vec<JobSlot> {
        self.rows.iter().filter_map(|row| row.job.clone()).collect()
    }
}

/// One round at `now`: reads indexing_slots, groups them by user and import, and for each user
/// asks `navidrome` for a client and reads the scan status once.
///
/// - A slot whose row or wait cannot be read ends failed (Unreadable), logged once.
/// - NotLinked/NoServer: the slots are Done, rows unchanged (D9).
/// - Another lookup or status error: Wait, and Fail with e.failure(Stage::Index, e.to_string())
///   once INDEX_GIVE_UP_SECS have passed since the import's `since`, counted from no earlier
///   than when the runner first held it.
/// - A slot the runner holds no wait for gets its row sent and recorded (Slots { Indexing })
///   with index Waiting.
/// - listed() runs when Navidrome is idle and its lastScan differs from `checked` (which it
///   then becomes).
/// - StartScan: start_scan is called once for every import of the user that asks for it. A
///   refusal (Subsonic 50, `StartScan`'s ErrorAuthorizationFail,
///   server/subsonic/library_scanning.go:43-44) leaves the wait to the watcher or a schedule.
///   Another start_scan error keeps the wait without the new baseline, so the next idle round
///   asks again, and fails it as a status error does once given up; it is logged at warn once
///   per wait, then at debug.
/// - Listed: rows with index Listed, Slots { Done }. Fail(f): rows with index Failed(f),
///   Slots { Failed }. A changed wait: Indexing. Rows go out with send_user_event.
pub(crate) async fn index_round(
    db: &SqlitePool,
    ledger: &Ledger,
    navidrome: &dyn NavidromeOf,
    runner: &mut IndexRunner,
    now: i64,
) {
    let store::Indexing { slots, unreadable } = match store::indexing_slots(db).await {
        Ok(indexing) => indexing,
        Err(e) => {
            warn!("Could not read the imports waiting for Navidrome: {e}");
            return;
        }
    };
    if !unreadable.is_empty() {
        for (slot, e) in &unreadable {
            warn!("Ending the wait for Navidrome of {slot:?}, which cannot be read: {e}");
        }
        ledger.record(JobEvent::Unreadable(
            unreadable.into_iter().map(|(slot, _)| slot).collect(),
        ));
    }
    let present = |held: &JobSlot| slots.iter().any(|s| &s.slot == held);
    runner.waits.retain(|held, _| present(held));
    runner.held_since.retain(|held, _| present(held));
    runner.scan_warned.retain(present);
    for slot in &slots {
        runner.held_since.entry(slot.slot.clone()).or_insert(now);
    }
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
                let read = Reading {
                    user: &user,
                    navi: &navi,
                    status: &status,
                };
                let mut scan = Vec::new();
                for import in imports {
                    let next = advance(db, ledger, runner, &read, import, now).await;
                    if let Some(asks) = next {
                        scan.push(asks);
                    }
                }
                if !scan.is_empty() {
                    start_scan(ledger, runner, &user, &navi, scan, now).await;
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
        let since = wait
            .since
            .max(runner.held_since.get(&slot).copied().unwrap_or(wait.since));
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
            Some(import) => {
                import.since = import.since.max(since);
                import.rows.push(row);
            }
            None => imports.push(Import {
                wait,
                rows: vec![row],
                since,
            }),
        }
    }
    users
}

/// An import that asks for a scan: the wait it keeps once the scan is asked for, and the one it
/// keeps when asking fails.
struct ScanAsk {
    import: Import,
    started: IndexWait,
    unasked: IndexWait,
}

/// A user's Navidrome as one round read it: the user, the client and its scan status.
struct Reading<'a> {
    user: &'a UserId,
    navi: &'a NavidromeClient,
    status: &'a ScanStatus,
}

/// Looks the import up when Navidrome scanned since it last did, and takes the next step. An
/// import that asks for a scan is given back for start_scan.
async fn advance(
    db: &SqlitePool,
    ledger: &Ledger,
    runner: &mut IndexRunner,
    read: &Reading<'_>,
    import: Import,
    now: i64,
) -> Option<ScanAsk> {
    let Reading { user, navi, status } = *read;
    let mut wait = import.wait.clone();
    let mut found = false;
    if !status.scanning && wait.checked != Some(status.last_scan) {
        match listed(db, navi, &wait).await {
            Ok(listed) => {
                found = listed;
                wait.checked = Some(status.last_scan);
            }
            Err(e) => {
                let failure = e.failure(Stage::Index, e.to_string());
                let held = import.wait.clone();
                give_up_after(ledger, runner, user, import, held, &failure, now).await;
                return None;
            }
        }
    }
    match next_step(&wait, status, found, given_up(import.since, now)) {
        Step::Listed => {
            let listed = Some(IndexState::Listed);
            end(ledger, runner, user, import, JobState::Done, listed).await;
        }
        Step::Fail(f) => {
            let failed = Some(IndexState::Failed(f));
            end(ledger, runner, user, import, JobState::Failed, failed).await;
        }
        Step::StartScan(started) => {
            return Some(ScanAsk {
                import,
                started,
                unasked: wait,
            })
        }
        Step::Wait(wait) => keep(ledger, runner, import, wait),
    }
    None
}

/// Asks Navidrome for one scan for every import of `user` in `asks`, and keeps or gives up each
/// wait by the answer.
async fn start_scan(
    ledger: &Ledger,
    runner: &mut IndexRunner,
    user: &UserId,
    navi: &NavidromeClient,
    asks: Vec<ScanAsk>,
    now: i64,
) {
    match navi.start_scan().await {
        Ok(()) => {
            for ask in asks {
                keep(ledger, runner, ask.import, ask.started);
            }
        }
        // A non-admin account is refused (Subsonic 50, server/subsonic/library_scanning.go:43-44):
        // Navidrome's watcher or its schedule may still scan.
        Err(NavidromeError::Subsonic {
            code: NOT_AUTHORIZED,
            ..
        }) => {
            debug!("Navidrome does not let {user} start a scan");
            for ask in asks {
                keep(ledger, runner, ask.import, ask.started);
            }
        }
        // Any other error says nothing of the account: without the baseline the next idle round
        // asks again.
        Err(e) => {
            let slots: Vec<JobSlot> = asks.iter().flat_map(|ask| ask.import.slots()).collect();
            if slots.iter().any(|slot| !runner.scan_warned.contains(slot)) {
                warn!("Could not ask Navidrome to scan for {user}: {e}");
            } else {
                debug!("Could not ask Navidrome to scan for {user}: {e}");
            }
            runner.scan_warned.extend(slots);
            let failure = e.failure(Stage::Index, e.to_string());
            for ask in asks {
                give_up_after(ledger, runner, user, ask.import, ask.unasked, &failure, now).await;
            }
        }
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
    if given_up(import.since, now) {
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
        every.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
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
    use crate::models::navidrome_library::NavidromeLibraryRow;
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
                album: "Come Over".into(),
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
            matches!(next_step(&first, &status(true, "2026-11-15T10:00:00Z", 9, ""), false, given_up(1_000, 1_005)), Step::Wait(w) if w == first)
        );
        let Step::StartScan(started) = next_step(
            &first,
            &status(false, "2026-11-15T10:02:00Z", 4, ""),
            false,
            given_up(1_000, 1_010),
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
                given_up(1_000, 1_015)
            ),
            Step::Wait(_)
        ));
        assert_eq!(
            match next_step(
                &started,
                &status(false, "2026-11-15T10:03:00Z", 3, ""),
                false,
                given_up(1_000, 1_080)
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
            given_up(1_000, 1_005),
        ) else {
            panic!()
        };
        assert!(matches!(
            next_step(
                &started,
                &status(false, "2026-11-15T10:02:00Z", -20, LOCKED),
                false,
                given_up(1_000, 1_010)
            ),
            Step::Wait(_)
        ));
        // The scan Soulful asked for began at 10:02:30 and failed: lastScan unchanged, a later start.
        let Step::Fail(f) = next_step(
            &started,
            &status(false, "2026-11-15T10:02:00Z", -30, LOCKED),
            false,
            given_up(1_000, 1_015),
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
    fn a_scan_that_fails_without_an_error_fails_the_wait_and_one_that_completes_does_not() {
        let Step::StartScan(started) = next_step(
            &waiting(1_000),
            &status(false, "2026-11-15T10:02:00Z", 4, ""),
            false,
            given_up(1_000, 1_005),
        ) else {
            panic!()
        };
        // Read while the scan begun at 10:02:30 completed: lastScan not yet moved, elapsedTime
        // counted from that start.
        assert!(matches!(
            next_step(
                &started,
                &status(false, "2026-11-15T10:02:00Z", 1, ""),
                false,
                given_up(1_000, 1_010)
            ),
            Step::Wait(_)
        ));
        // The scan begun at 10:02:30 failed preparing the libraries: no error stored.
        let Step::Fail(f) = next_step(
            &started,
            &status(false, "2026-11-15T10:02:00Z", -30, ""),
            false,
            given_up(1_000, 1_015),
        ) else {
            panic!("a scan that began after the import failed")
        };
        assert_eq!(
            (f.code, f.retry, f.message.as_str(), f.cause),
            (
                FailureCode::ScanFailed,
                RetryAdvice::SameSourceLater,
                "Navidrome's scan failed without giving a reason",
                None
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
                given_up(1_000, 1_010)
            ),
            Step::Listed
        ));
        assert!(matches!(
            next_step(
                &started,
                &status(false, "2026-11-15T10:02:00Z", 4, ""),
                false,
                given_up(1_000, 1_000 + INDEX_GIVE_UP_SECS - 1)
            ),
            Step::Wait(_)
        ));
        let Step::Fail(f) = next_step(
            &started,
            &status(false, "2026-11-15T10:02:00Z", 4, ""),
            false,
            given_up(1_000, 1_000 + INDEX_GIVE_UP_SECS),
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
            library_id: 1,
            library_path: "/music".into(),
            missing: false,
            artist: "Zzqx Artist".into(),
            album: "Qzz Album".into(),
            album_artist: "Zzqx Artist".into(),
            rating: 0,
            average_rating: None,
            mbz_recording_id: None,
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
        "title":"Song Number 1","album":"Qzz Album","artist":"Zzqx Artist","albumArtist":"Zzqx Artist","size":{size},"suffix":"mp3","missing":false}}]"#
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
                album: "Qzz Album".into(),
            }],
            ..waiting(1_000)
        }
    }

    #[tokio::test]
    async fn another_copy_of_the_recording_is_not_the_imported_file() {
        let library = tempfile::tempdir().unwrap();
        let wait = imported_in(library.path());
        let db = crate::db::test_db::migrated().await;
        let navi = NavidromeStub::start();
        // The MBID finds a copy of another size; the text finds one in another album.
        navi.answer(
            "/api/song",
            &[
                &song_json("Zzqx Artist/Qzz Album/01 Song Number 1.mp3", 41000),
                &song_json("Zzqx Artist/Best Of/01 Song Number 1.mp3", 4),
            ],
        );
        assert!(!listed(&db, &navi.client(), &wait).await.unwrap());
        navi.answer(
            "/api/song",
            &[&song_json(
                "alice/Zzqx Artist/Qzz Album/01 Song Number 1.mp3",
                4,
            )],
        );
        assert!(listed(&db, &navi.client(), &wait).await.unwrap());
    }

    #[tokio::test]
    async fn an_item_is_looked_up_by_recording_then_by_artist_and_title() {
        let navi = NavidromeStub::start();
        let record = |id: &str, path: &str| {
            format!(
                r#"{{"id":"{id}","libraryId":1,"libraryPath":"/music","path":"{path}","title":"O Holy Night",
                "album":"Winter Songs","artist":"Marrow Choir","albumArtist":"Marrow Choir","size":5,"suffix":"flac","missing":false}}"#
            )
        };
        let first = record(
            "3sG8kPq0WmZ1xYbN4vTcDe",
            "Marrow Choir/Winter Songs/03.flac",
        );
        let second = record("7hJ2mNq4RtX9vBc1LkWsAe", "Marrow Choir/Best Of/11.flac");
        navi.answer(
            "/api/song",
            &[&format!("[{first}]"), &format!("[{first},{second}]")],
        );
        let item = ImportedItem {
            path: "Marrow Choir/Winter Songs/03.flac".into(),
            recording_mbid: Some("0a2f7e4b-6c1d-4e8a-9b3f-5d7c1e2a4b6c".into()),
            artist: "Marrow Choir".into(),
            title: "O Holy Night".into(),
            album: "Winter Songs".into(),
        };
        let found = candidates(&navi.client(), &item).await.unwrap().unwrap();
        assert_eq!(
            found.iter().map(|s| s.id.as_str()).collect::<Vec<_>>(),
            vec!["3sG8kPq0WmZ1xYbN4vTcDe", "7hJ2mNq4RtX9vBc1LkWsAe"]
        );
        let asked: Vec<String> = navi
            .asked_with_queries()
            .into_iter()
            .filter(|target| target.starts_with("/api/song?"))
            .collect();
        assert_eq!(asked.len(), 2, "{asked:?}");
        for (target, title) in asked.iter().zip([
            "title=0a2f7e4b-6c1d-4e8a-9b3f-5d7c1e2a4b6c",
            "title=Marrow+Choir+O+Holy+Night",
        ]) {
            assert!(
                target.split(['?', '&']).any(|p| p == title)
                    && target.split(['?', '&']).any(|p| p == "missing=false"),
                "{target}"
            );
        }
        // Nothing Navidrome's filter keeps: no search can show that no other song has the file.
        let bare = ImportedItem {
            recording_mbid: None,
            artist: "(".into(),
            title: ")".into(),
            album: "[]".into(),
            ..item
        };
        assert!(candidates(&navi.client(), &bare).await.unwrap().is_none());
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
                    picked: vec![],
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
    async fn a_listed_file_teaches_where_its_navidrome_library_is() {
        let library = tempfile::tempdir().unwrap();
        let (ledger, db, _rx, _) = indexing(imported_in(library.path())).await;
        let navi = NavidromeStub::start();
        navi.answer("/rest/getScanStatus", &[IDLE]);
        navi.answer(
            "/api/song",
            &[&song_json("Zzqx Artist/Qzz Album/01 Song Number 1.mp3", 4)],
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
            NavidromeLibraryRow::get_in(&db, 1).await.unwrap(),
            Some(NavidromeLibraryRow {
                library_id: 1,
                navidrome_path: "/music".into(),
                local_path: library.path().to_str().unwrap().into(),
            })
        );
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
        let [slot] = &store::indexing_slots(&db).await.unwrap().slots[..] else {
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
        let [slot] = &store::indexing_slots(&db).await.unwrap().slots[..] else {
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
        let [slot] = &store::indexing_slots(&db).await.unwrap().slots[..] else {
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
            1_005 + INDEX_GIVE_UP_SECS,
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
            1_005 + INDEX_GIVE_UP_SECS,
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

    #[test]
    fn an_empty_path_is_never_the_imported_file() {
        let song = |path: &str| NativeSong {
            id: "f2c4d1a8b0e94c37a6d5e8b1c2a3f405".into(),
            path: path.into(),
            title: "Song Number 1".into(),
            size: 4,
            library_id: 1,
            library_path: "/music".into(),
            missing: false,
            artist: "Zzqx Artist".into(),
            album: "Qzz Album".into(),
            album_artist: "Zzqx Artist".into(),
            rating: 0,
            average_rating: None,
            mbz_recording_id: None,
        };
        let item = Path::new("Zzqx Artist/Qzz Album/01 Song Number 1.mp3");
        assert!(!is_imported_file(&song(""), item, 4));
        assert!(!is_imported_file(
            &song("Zzqx Artist/Qzz Album/01 Song Number 1.mp3"),
            Path::new(""),
            4
        ));
    }

    /// `wait` with its one item tagged `artist`, `title` and `album`, without an MBID.
    fn tagged(mut wait: IndexWait, artist: &str, title: &str, album: &str) -> IndexWait {
        wait.items[0] = ImportedItem {
            recording_mbid: None,
            artist: artist.into(),
            title: title.into(),
            album: album.into(),
            ..wait.items[0].clone()
        };
        wait
    }

    /// The query of each native song lookup `navi` was asked.
    fn song_lookups(navi: &NavidromeStub) -> Vec<String> {
        navi.asked_with_queries()
            .into_iter()
            .filter(|target| target.starts_with("/api/song?"))
            .collect()
    }

    #[tokio::test]
    async fn an_artist_of_symbols_is_looked_up_by_its_name_and_title() {
        let library = tempfile::tempdir().unwrap();
        let wait = tagged(imported_in(library.path()), "!!!", "?!", "");
        let (ledger, db, mut rx, _) = indexing(wait).await;
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
        let lookups = song_lookups(&navi);
        assert_eq!(lookups.len(), 1);
        assert!(
            lookups[0].contains("&title=%21%21%21+%3F%21&"),
            "{}",
            lookups[0]
        );
    }

    /// An item none of whose text Navidrome's sanitizer keeps.
    fn wordless(wait: IndexWait) -> IndexWait {
        tagged(wait, "\u{201c}\u{201d}", "(/)", "[ , ]")
    }

    #[tokio::test]
    async fn an_item_navidrome_keeps_no_word_of_is_found_among_the_newest_songs() {
        let library = tempfile::tempdir().unwrap();
        let (ledger, db, mut rx, _) = indexing(wordless(imported_in(library.path()))).await;
        let navi = NavidromeStub::start();
        navi.answer("/rest/getScanStatus", &[IDLE]);
        // A full page of songs added since, then the imported one.
        let others: Vec<String> = (0..LOOKUP_PAGE)
            .map(|n| {
                let song = song_json(&format!("Other/{n:03}.mp3"), 4);
                song[1..song.len() - 1].to_string()
            })
            .collect();
        let newer = format!("[{}]", others.join(","));
        navi.answer(
            "/api/song",
            &[
                &newer,
                &song_json("alice/Zzqx Artist/Qzz Album/01 Song Number 1.mp3", 4),
            ],
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
        assert_eq!(
            song_lookups(&navi),
            vec![
                "/api/song?_start=0&_end=100&_sort=created_at&_order=DESC&missing=false",
                "/api/song?_start=100&_end=200&_sort=created_at&_order=DESC&missing=false",
            ]
        );
    }

    #[tokio::test]
    async fn an_item_navidrome_keeps_no_word_of_and_does_not_list_ends_not_found_after_scan() {
        let library = tempfile::tempdir().unwrap();
        // A scan was asked for at 10:01, and Navidrome has completed one since (IDLE).
        let wait = IndexWait {
            baseline: Some(ScanBaseline {
                last_scan: at("2026-11-15T10:00:00Z"),
                started: at("2026-11-15T09:59:50Z"),
            }),
            ..wordless(imported_in(library.path()))
        };
        let (ledger, db, mut rx, _) = indexing(wait).await;
        let navi = NavidromeStub::start();
        navi.answer("/rest/getScanStatus", &[IDLE]);
        navi.answer("/api/song", &[&song_json("Other/001.mp3", 4)]);
        index_round(
            &db,
            &ledger,
            &Navidrome(Ok(navi.client())),
            &mut IndexRunner::default(),
            1_005,
        )
        .await;
        ledger.settled().await;
        assert_eq!(store::job_state(&db, "j1").await, Some(JobState::Failed));
        assert_eq!(
            indexes(&mut rx),
            vec![
                Some(IndexState::Waiting),
                Some(IndexState::Failed(not_found_after_scan()))
            ]
        );
        assert_eq!(
            song_lookups(&navi),
            vec!["/api/song?_start=0&_end=100&_sort=created_at&_order=DESC&missing=false"]
        );
    }

    #[tokio::test]
    async fn a_wait_a_restart_interrupted_has_its_whole_time_again() {
        let library = tempfile::tempdir().unwrap();
        // The import ended long before this process started.
        let (ledger, db, _rx, _) = indexing(IndexWait {
            since: 0,
            ..imported_in(library.path())
        })
        .await;
        let navi = NavidromeStub::start();
        let unreadable = r#"{"subsonic-response":{"status":"failed","version":"1.16.1","error":{"code":0,"message":"Internal Error"}}}"#;
        navi.answer("/rest/getScanStatus", &[unreadable]);
        let mut runner = IndexRunner::default();
        for now in [50_000, 50_000 + INDEX_GIVE_UP_SECS - 1] {
            index_round(
                &db,
                &ledger,
                &Navidrome(Ok(navi.client())),
                &mut runner,
                now,
            )
            .await;
            ledger.settled().await;
            assert_eq!(
                store::job_state(&db, "j1").await,
                Some(JobState::Indexing),
                "{now}"
            );
        }
        index_round(
            &db,
            &ledger,
            &Navidrome(Ok(navi.client())),
            &mut runner,
            50_000 + INDEX_GIVE_UP_SECS,
        )
        .await;
        ledger.settled().await;
        assert_eq!(store::job_state(&db, "j1").await, Some(JobState::Failed));
    }

    #[tokio::test]
    async fn one_scan_is_asked_for_every_import_of_a_user() {
        let library = tempfile::tempdir().unwrap();
        let (ledger, db, _rx, _) = indexing(imported_in(library.path())).await;
        // A second import of the same job's user, waiting on its own.
        let slot = JobSlot {
            job: "j1".into(),
            slot: 1,
        };
        ledger.record(JobEvent::Picked {
            job: "j1".into(),
            items: vec![(1, item("peer_a", "Song Number 2", "02.mp3"))],
        });
        ledger.record(JobEvent::Slots {
            state: JobState::Indexing,
            rows: vec![DownloadProgress::queued(
                "t-02".into(),
                "peer_a".into(),
                "02.mp3".into(),
                4,
            )
            .with_state(DownloadState::Imported)
            .with_job(slot.clone())],
        });
        ledger.record(JobEvent::Indexing {
            slots: vec![slot],
            wait: Box::new(IndexWait {
                import: "/downloads/Come Over [1f2e3d4c]/02.flac".into(),
                ..imported_in(library.path())
            }),
        });
        ledger.settled().await;
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
        assert_eq!(scans_asked(&navi), 1);
        let waits = store::indexing_slots(&db).await.unwrap().slots;
        assert_eq!(waits.len(), 2);
        assert!(waits.iter().all(|s| s.wait.baseline.is_some()));
    }

    #[tokio::test]
    async fn a_wait_that_cannot_be_read_ends_and_the_others_go_on() {
        let library = tempfile::tempdir().unwrap();
        let (ledger, db, _rx, _) = indexing(imported_in(library.path())).await;
        sqlx::query("UPDATE job_tracks SET index_wait = '{' WHERE job_id = 'j1'")
            .execute(&db)
            .await
            .unwrap();
        let indexing = store::indexing_slots(&db).await.unwrap();
        assert!(indexing.slots.is_empty());
        assert_eq!(indexing.unreadable.len(), 1);
        index_round(
            &db,
            &ledger,
            &Navidrome(Err(|| NavidromeLookupError::NotLinked)),
            &mut IndexRunner::default(),
            1_005,
        )
        .await;
        ledger.settled().await;
        assert_eq!(store::job_state(&db, "j1").await, Some(JobState::Failed));
    }
}
