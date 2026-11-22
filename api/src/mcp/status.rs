//! `get_status`: what Soulful can do for the user right now, read at once
//! (D9).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex, PoisonError};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use chrono::{DateTime, NaiveDate, Utc};
use futures::future::{join_all, BoxFuture, Shared};
use futures::FutureExt;
use rmcp::model::{CallToolResult, JsonObject};
use serde_json::{json, Map, Value};
use shared::failure::{cut_json, Failure, Stage, RAW_MAX_CHARS, TEXT_MAX_CHARS};
use shared::system::{HealthLevel, SystemHealth};
use soulbeet::navidrome::ScanStatus;
use sqlx::SqlitePool;

use super::args::Args;
use super::contract::{failed, success, ToolError};
use super::paging::fit_whole;
use super::McpCaller;
use crate::auth::UserCtx;
use crate::connection::ConnectionState;
use crate::core::index::ServerNavidrome;
use crate::core::library::{library_roots_in, listing, navidrome_failure};
use crate::core::scan::{scan_by, ScanBy, SCAN_ROLES};
use crate::core::time;
use crate::core::{database_failure, folders, health::system_health, settings};
use crate::ledger::store::{running_batches, RunningBatch};
use crate::models::folder::Folder;
use crate::oauth::metadata::mcp_url;
use crate::services::{navidrome_client_for_user, DEFAULT_DOWNLOAD_BACKEND};

/// How long a folder's free space is waited for.
const FREE_SPACE_WAIT: Duration = Duration::from_secs(2);

/// What `FreeSpaceReads::read` answers when the read takes longer than
/// `wait`: "no answer within 2 seconds", or in milliseconds for a wait
/// that is not whole seconds.
fn no_answer(wait: Duration) -> String {
    match (wait.as_secs(), wait.subsec_millis()) {
        (1, 0) => "no answer within 1 second".to_string(),
        (secs, 0) => format!("no answer within {secs} seconds"),
        _ => format!("no answer within {} ms", wait.as_millis()),
    }
}

/// What the user's Navidrome account says, each read on its own.
#[derive(Clone)]
pub(crate) struct NavidromeFacts {
    pub admin: Result<bool, Failure>,
    /// Whose account starts the scans after the user's imports (core::scan).
    pub scan_by: Result<ScanBy, Failure>,
    pub scan: Result<ScanStatus, Failure>,
}

/// A folder and the free space of the filesystem it is on.
#[derive(Clone)]
pub(crate) struct FolderFacts {
    pub folder: Folder,
    pub free: Result<u64, String>,
}

/// Everything `get_status` reports, as read.
#[derive(Clone)]
pub(crate) struct StatusFacts {
    pub health: SystemHealth,
    /// None when the user has no Navidrome client (`health.navidrome_failure` says why).
    pub navidrome: Option<NavidromeFacts>,
    pub folders: Vec<FolderFacts>,
    /// Where Navidrome's libraries are on this machine, as far as Soulful
    /// knows (library_roots_in).
    pub library_roots: Vec<PathBuf>,
    pub default_folder_id: Option<String>,
    pub running_batches: Vec<RunningBatch>,
}

/// Where the facts come from, a seam for tests.
#[async_trait]
pub(crate) trait StatusSource: Send + Sync {
    async fn gather(&self, ctx: &UserCtx) -> Result<StatusFacts, Failure>;
}

/// The running instance's backends, and its database `db`.
pub(crate) struct LiveStatus {
    pub db: SqlitePool,
}

#[async_trait]
impl StatusSource for LiveStatus {
    /// Reads every fact at once; fails only when the folders, the library
    /// roots, the settings or the ledger's batches cannot be read.
    async fn gather(&self, ctx: &UserCtx) -> Result<StatusFacts, Failure> {
        let user = ctx.id();
        let (health, navidrome, folders, library_roots, settings, batches) = tokio::join!(
            system_health(ctx),
            navidrome_facts(&self.db, ctx),
            async { Ok::<_, Failure>(with_free_space(folders::list(ctx).await?).await) },
            library_roots_in(&self.db, crate::config::CONFIG.navidrome_music_path()),
            settings::get(ctx),
            running_batches(&self.db, &user),
        );
        Ok(StatusFacts {
            health,
            navidrome,
            folders: folders?,
            library_roots: library_roots?,
            default_folder_id: chosen_folder(settings?.default_download_folder_id),
            running_batches: batches.map_err(|e| database_failure(Stage::Input, e))?,
        })
    }
}

/// The folder a settings choice names: an empty id is a cleared choice
/// (core::settings::check_folder_choices), so no folder.
pub(crate) fn chosen_folder(id: Option<String>) -> Option<String> {
    id.filter(|id| !id.is_empty())
}

/// The user's own admin role, read here, is also what scan_by knows of their account.
async fn navidrome_facts(db: &SqlitePool, ctx: &UserCtx) -> Option<NavidromeFacts> {
    let client = navidrome_client_for_user(&ctx.user_id).await.ok()?;
    let (user, scan) = tokio::join!(client.get_own_user(), client.get_scan_status());
    let failure = |e: soulbeet::NavidromeError| navidrome_failure(Stage::LibraryCheck, &e);
    let admin = user.map(|u| u.admin_role).map_err(failure);
    let scanner = match &admin {
        Ok(admin) => {
            let id = ctx.id();
            SCAN_ROLES.learn(&id, &client, *admin, Instant::now());
            scan_by(db, &ServerNavidrome, &SCAN_ROLES, &id, &client)
                .await
                .map_err(|e| e.failure(Stage::LibraryCheck, e.to_string()))
        }
        Err(f) => Err(f.clone()),
    };
    Some(NavidromeFacts {
        admin,
        scan_by: scanner,
        scan: scan.map_err(failure),
    })
}

/// Each folder with the free space of the filesystem it is on, all read at
/// once.
pub(crate) async fn with_free_space(folders: Vec<Folder>) -> Vec<FolderFacts> {
    join_all(folders.into_iter().map(|folder| async move {
        let free = FREE_SPACE.read(PathBuf::from(&folder.path)).await;
        FolderFacts { folder, free }
    }))
    .await
}

/// One folder as `get_status` and `list_folders` report it, `in_navidrome`
/// read against `library_roots`. Its name is the user's own text, cut to
/// TEXT_MAX_CHARS of JSON; its path is canonical, so the kernel bounds it at
/// PATH_MAX.
pub(crate) fn folder_fields(
    f: &FolderFacts,
    default_folder_id: Option<&str>,
    library_roots: &[PathBuf],
) -> Value {
    let (free_bytes, free_space_failure) = match &f.free {
        Ok(bytes) => (json!(bytes), Value::Null),
        Err(e) => (Value::Null, json!(e)),
    };
    json!({
        "folder_id": f.folder.id,
        "name": cut_json(f.folder.name.clone(), TEXT_MAX_CHARS),
        "path": f.folder.path,
        "shared": f.folder.shared,
        "default": Some(f.folder.id.as_str()) == default_folder_id,
        "free_bytes": free_bytes,
        "free_space_failure": free_space_failure,
        "in_navidrome": listing(Path::new(&f.folder.path), library_roots),
    })
}

/// The running instance's free-space reads.
static FREE_SPACE: LazyLock<Arc<FreeSpaceReads>> =
    LazyLock::new(|| FreeSpaceReads::new(free_space, FREE_SPACE_WAIT));

type FreeSpaceRead = Shared<BoxFuture<'static, Result<u64, String>>>;

/// Free-space reads off the async threads, at most one running per path. A
/// hung network mount blocks `statvfs` for good: a call gives up after its
/// wait, and a later call for the same path waits on the read still
/// running instead of starting one more blocking thread, so a hung mount
/// holds one of tokio's blocking threads, not one per call.
pub(crate) struct FreeSpaceReads {
    reader: fn(&Path) -> Result<u64, String>,
    wait: Duration,
    running: Mutex<HashMap<PathBuf, FreeSpaceRead>>,
}

impl FreeSpaceReads {
    pub(crate) fn new(reader: fn(&Path) -> Result<u64, String>, wait: Duration) -> Arc<Self> {
        Arc::new(FreeSpaceReads {
            reader,
            wait,
            running: Mutex::new(HashMap::new()),
        })
    }

    /// The free space at `path`, or `no_answer` when the read, started now
    /// or by an earlier call, gives none within the wait.
    pub(crate) async fn read(self: &Arc<Self>, path: PathBuf) -> Result<u64, String> {
        let read = {
            let mut running = self.running.lock().unwrap_or_else(PoisonError::into_inner);
            match running.get(&path) {
                Some(read) => read.clone(),
                None => {
                    let read = self.start(path.clone());
                    running.insert(path, read.clone());
                    read
                }
            }
        };
        tokio::time::timeout(self.wait, read)
            .await
            .unwrap_or_else(|_| Err(no_answer(self.wait)))
    }

    /// A blocking read of `path` that takes itself out of `running` once it
    /// returns or panics. The caller inserts it while it holds the lock, and
    /// the read's thread takes the same lock to remove it, so the entry it
    /// removes is its own. The guard is made on the read's thread: a
    /// runtime that is shutting down drops the closure unrun on the caller's
    /// thread, under the caller's lock, and an unrun closure holds no guard.
    fn start(self: &Arc<Self>, path: PathBuf) -> FreeSpaceRead {
        let reads = self.clone();
        tokio::task::spawn_blocking(move || {
            let leaving = Leaving { reads, path };
            (leaving.reads.reader)(&leaving.path)
        })
        .map(|joined| joined.unwrap_or_else(|join| Err(join.to_string())))
        .boxed()
        .shared()
    }
}

/// Takes a read's entry out of `running` when the read's thread lets go of
/// it, after the read returned or while a panic in it unwinds, so a later
/// call starts a new read instead of waiting on one that ended.
struct Leaving {
    reads: Arc<FreeSpaceReads>,
    path: PathBuf,
}

impl Drop for Leaving {
    fn drop(&mut self) {
        self.reads
            .running
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&self.path);
    }
}

/// The bytes an unprivileged user can still write on the filesystem holding
/// `path`; Err is the OS error's text.
pub(crate) fn free_space(path: &Path) -> Result<u64, String> {
    rustix::fs::statvfs(path)
        .map(|s| s.f_bavail * s.f_frsize)
        .map_err(|errno| std::io::Error::from(errno).to_string())
}

/// `get_status`, which takes no arguments.
pub(crate) async fn call(
    state: &ConnectionState,
    caller: &McpCaller,
    args: Option<JsonObject>,
) -> CallToolResult {
    if let Err(e) = Args::new("get_status", args, &[]).finish() {
        return failed(e);
    }
    match state.status.gather(&caller.ctx).await {
        Ok(facts) => success(status_fields(&facts, caller)),
        Err(failure) => failed(ToolError {
            failure,
            facts: Map::new(),
            tried: vec![],
            next: vec![],
        }),
    }
}

/// The status document's fields; a fact that could not be read is `null`,
/// next to its failure. Batch labels and importer issues are outside text
/// (an album title, beets' own output), cut as `cut_json` does: a label
/// and an issue title to TEXT_MAX_CHARS, an issue detail, which can hold
/// a Python error, to RAW_MAX_CHARS like a cause. The running batches and
/// then the folders are listed while the document stays within
/// RESULT_MAX_CHARS; `running_batches_left_out` and `folders_left_out`
/// count the rest (list_folders pages through the folders). The list of
/// importer issues is not fitted, only each issue's texts are cut: beside
/// a few fixed ones the issues come from the beets probe's report, one
/// line the probe usually holds under 8,000 characters by cutting each
/// plugin error to 500, 200, 80 or 0 characters
/// (lib/soulbeet/src/beets/probe.py, `MAX_LINE`), or from the probe's
/// output tail.
pub(crate) fn status_fields(facts: &StatusFacts, caller: &McpCaller) -> Map<String, Value> {
    let health = &facts.health;
    let navidrome = facts.navidrome.as_ref();
    let (admin, admin_failure) = read(navidrome.map(|n| &n.admin), |admin| json!(admin));
    let (can_trigger_scan, can_trigger_scan_failure) =
        read(navidrome.map(|n| &n.scan_by), |by| json!(by.can_scan()));
    let scan_note = navidrome.and_then(|n| n.scan_by.as_ref().ok().map(|by| by.note()));
    let (scan, scan_failure) = read(navidrome.map(|n| &n.scan), scan_document);
    let issues: Vec<Value> = health
        .importer
        .issues
        .iter()
        .map(|issue| {
            json!({
                "severity": level_name(issue.severity),
                "title": cut_json(issue.title.clone(), TEXT_MAX_CHARS),
                "detail": cut_json(issue.detail.clone(), RAW_MAX_CHARS),
            })
        })
        .collect();
    let default = facts.default_folder_id.as_deref();
    let folders: Vec<Value> = facts
        .folders
        .iter()
        .map(|f| folder_fields(f, default, &facts.library_roots))
        .collect();
    let running_batches: Vec<Value> = facts
        .running_batches
        .iter()
        .map(|b| {
            json!({"batch_id": b.batch_id,
                "label": b.label.clone().map(|label| cut_json(label, TEXT_MAX_CHARS)),
                "unfinished_jobs": b.unfinished_jobs})
        })
        .collect();
    let Value::Object(mut fields) = json!({
        "base_url": caller.base.to_string(),
        "mcp_url": mcp_url(&caller.base),
        "user": {"username": caller.ctx.username, "role": caller.ctx.role},
        "download_backend": {
            "backend": DEFAULT_DOWNLOAD_BACKEND,
            "online": health.downloader_online,
            "failure": health.downloader_failure,
        },
        "importer": {"level": level_name(health.importer.level), "issues": issues},
        "navidrome": {
            "connected": health.navidrome_online,
            "failure": health.navidrome_failure,
            // startScan is admin only (navidrome/server/subsonic/library_scanning.go:43-45), so
            // another linked account may start the user's (core::scan).
            "can_trigger_scan": can_trigger_scan,
            "can_trigger_scan_failure": can_trigger_scan_failure,
            "scan_note": scan_note,
            "admin": admin,
            "admin_failure": admin_failure,
            "scan": scan,
            "scan_failure": scan_failure,
        },
        "default_folder_id": facts.default_folder_id,
        "deletion_enabled": caller.allow_delete,
    }) else {
        unreachable!("an object literal is an object");
    };
    // The batches are fitted with room kept for the folders' empty list and
    // their count at its longest, so fitting the folders after them in what
    // is left keeps the whole within the cap.
    fields.insert("folders".into(), json!([]));
    fields.insert("folders_left_out".into(), json!(folders.len()));
    let fields = fit_whole(
        fields,
        "running_batches",
        running_batches,
        "running_batches_left_out",
    );
    fit_whole(fields, "folders", folders, "folders_left_out")
}

/// A fact as `(value, failure)`: one of them is `null`, both when the fact
/// was not read at all.
fn read<T>(fact: Option<&Result<T, Failure>>, value: impl Fn(&T) -> Value) -> (Value, Value) {
    match fact {
        None => (Value::Null, Value::Null),
        Some(Ok(fact)) => (value(fact), Value::Null),
        Some(Err(failure)) => (Value::Null, json!(failure)),
    }
}

/// Navidrome's `count` is the library's song count when idle and the files
/// read so far while a scan runs (navidrome scanner/controller.go,
/// `Status`), so each is its own field, `null` in the other state.
fn scan_document(scan: &ScanStatus) -> Value {
    let last_scan = (scan.last_scan != go_zero_time()).then(|| time::at(scan.last_scan));
    let error = (!scan.error.is_empty()).then_some(scan.error.as_str());
    let (songs, files_scanned) = match scan.scanning {
        false => (Some(scan.count), None),
        true => (None, Some(scan.count)),
    };
    json!({"scanning": scan.scanning, "songs": songs, "files_scanned": files_scanned,
        "last_scan": last_scan, "error": error})
}

/// `0001-01-01T00:00:00Z`, Go's zero time, which Navidrome sends as
/// `lastScan` before the first scan.
fn go_zero_time() -> DateTime<Utc> {
    NaiveDate::from_ymd_opt(1, 1, 1)
        .and_then(|day| day.and_hms_opt(0, 0, 0))
        .expect("0001-01-01 00:00:00 is a valid time")
        .and_utc()
}

/// The browser reads `HealthLevel`'s own serde names, so the document names
/// the levels here.
fn level_name(level: HealthLevel) -> &'static str {
    match level {
        HealthLevel::Checking => "checking",
        HealthLevel::Ready => "ready",
        HealthLevel::Degraded => "degraded",
        HealthLevel::Broken => "broken",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connection::test_util::{alice_caller, sample_facts};
    use crate::mcp::paging::RESULT_MAX_CHARS;
    use crate::services::BackendLookupError;

    #[test]
    fn the_status_document_holds_every_fact() {
        assert_eq!(
            Value::Object(status_fields(&sample_facts(), &alice_caller(false))),
            json!({
                "base_url": "https://music.example.com",
                "mcp_url": "https://music.example.com/mcp",
                "user": {"username": "alice", "role": "user"},
                "download_backend": {"backend": "slskd", "online": true, "failure": null},
                "importer": {"level": "degraded", "issues": [{"severity": "degraded", "title": "beets config check timed out",
                             "detail": "A plugin may be waiting on the network or for input."}]},
                "navidrome": {"connected": true, "failure": null, "admin": false, "admin_failure": null, "can_trigger_scan": false,
                              "can_trigger_scan_failure": null, "scan_note": ScanBy::Nobody.note(),
                              "scan": {"scanning": false, "songs": 1342, "files_scanned": null,
                                       "last_scan": "2026-11-14T21:03:11Z", "error": null},
                              "scan_failure": null},
                "folders": [{"folder_id": "f1", "name": "Music", "path": "/music/alice", "shared": false, "default": true, "free_bytes": 1000,
                             "free_space_failure": null, "in_navidrome": "listed"}],
                "folders_left_out": 0,
                "default_folder_id": "f1",
                "running_batches": [{"batch_id": "b1", "label": "Come Over", "unfinished_jobs": 2}],
                "running_batches_left_out": 0,
                "deletion_enabled": false
            })
        );
        assert_eq!(
            status_fields(&sample_facts(), &alice_caller(true))["deletion_enabled"],
            true
        );
    }

    #[test]
    fn unread_facts_are_null_with_their_failure() {
        let circuit = || {
            soulbeet::NavidromeError::CircuitOpen.failure(
                Stage::LibraryCheck,
                soulbeet::NavidromeError::CircuitOpen.to_string(),
            )
        };
        // The lookup failure system_health records when the backend's
        // settings cannot be read (services.rs, BackendLookupError::failure).
        let lookup = BackendLookupError::Settings(
            "error returned from database: (code: 5) database is locked".into(),
        );
        let mut facts = sample_facts();
        facts.health.downloader_online = false;
        facts.health.downloader_failure =
            Some(lookup.failure(Stage::SourceSearch, lookup.to_string()));
        facts.health.navidrome_online = false;
        facts.health.navidrome_failure = Some(circuit());
        facts.navidrome = Some(NavidromeFacts {
            admin: Err(circuit()),
            scan_by: Err(circuit()),
            scan: Err(circuit()),
        });
        facts.folders[0].free = Err("no answer within 2 seconds".into());
        let doc = Value::Object(status_fields(&facts, &alice_caller(false)));
        let failure = |f: &Failure| serde_json::to_value(f).unwrap();
        assert_eq!(
            doc["download_backend"],
            json!({"backend": "slskd", "online": false,
                "failure": failure(facts.health.downloader_failure.as_ref().unwrap())})
        );
        assert_eq!(
            doc["navidrome"],
            json!({"connected": false, "failure": failure(&circuit()), "admin": null,
                "admin_failure": failure(&circuit()), "can_trigger_scan": null,
                "can_trigger_scan_failure": failure(&circuit()), "scan_note": null, "scan": null,
                "scan_failure": failure(&circuit())})
        );
        assert_eq!(
            (
                &doc["folders"][0]["free_bytes"],
                &doc["folders"][0]["free_space_failure"]
            ),
            (&Value::Null, &json!("no answer within 2 seconds"))
        );
        facts.navidrome = None;
        let doc = Value::Object(status_fields(&facts, &alice_caller(false)));
        for key in [
            "admin",
            "admin_failure",
            "can_trigger_scan",
            "can_trigger_scan_failure",
            "scan_note",
            "scan",
            "scan_failure",
        ] {
            assert_eq!(doc["navidrome"][key], Value::Null, "{key}");
        }
    }

    #[test]
    fn a_folder_no_known_library_root_holds_is_unknown_in_navidrome() {
        let mut facts = sample_facts();
        let in_navidrome = |facts: &StatusFacts| {
            status_fields(facts, &alice_caller(false))["folders"][0]["in_navidrome"].clone()
        };
        assert_eq!(in_navidrome(&facts), "listed");
        facts.library_roots = vec![PathBuf::from("/music/bob")];
        assert_eq!(in_navidrome(&facts), "unknown");
        facts.library_roots.clear();
        assert_eq!(in_navidrome(&facts), "unknown");
    }

    #[test]
    fn a_never_run_scan_has_no_last_scan_time() {
        let mut facts = sample_facts();
        facts.navidrome = Some(NavidromeFacts {
            admin: Ok(true),
            scan_by: Ok(ScanBy::Own),
            scan: Ok(ScanStatus {
                scanning: false,
                count: 0,
                last_scan: "0001-01-01T00:00:00Z".parse().unwrap(),
                error: String::new(),
                elapsed_time: 0,
            }),
        });
        let doc = Value::Object(status_fields(&facts, &alice_caller(false)));
        assert_eq!(
            doc["navidrome"]["scan"],
            json!({"scanning": false, "songs": 0, "files_scanned": null, "last_scan": null, "error": null})
        );
        assert_eq!(
            (
                doc["navidrome"]["admin"].as_bool(),
                doc["navidrome"]["can_trigger_scan"].as_bool()
            ),
            (Some(true), Some(true))
        );
    }

    #[test]
    fn a_user_who_is_no_admin_can_have_a_scan_started_by_another_account() {
        let mut facts = sample_facts();
        if let Some(navidrome) = facts.navidrome.as_mut() {
            navidrome.scan_by = Ok(ScanBy::Another);
        }
        let doc = Value::Object(status_fields(&facts, &alice_caller(false)));
        assert_eq!(
            (
                &doc["navidrome"]["admin"],
                &doc["navidrome"]["can_trigger_scan"],
                &doc["navidrome"]["scan_note"]
            ),
            (&json!(false), &json!(true), &json!(ScanBy::Another.note()))
        );
        let nobody = Value::Object(status_fields(&sample_facts(), &alice_caller(false)));
        let note = nobody["navidrome"]["scan_note"].as_str().unwrap();
        assert!(note.contains("Navidrome's next own scan"), "{note}");
    }

    #[test]
    fn a_running_scan_counts_files_scanned_not_songs() {
        let mut facts = sample_facts();
        if let Some(NavidromeFacts { scan: Ok(scan), .. }) = facts.navidrome.as_mut() {
            scan.scanning = true;
            scan.count = 412;
        }
        let doc = Value::Object(status_fields(&facts, &alice_caller(false)));
        assert_eq!(
            (
                &doc["navidrome"]["scan"]["songs"],
                &doc["navidrome"]["scan"]["files_scanned"]
            ),
            (&Value::Null, &json!(412))
        );
    }

    #[test]
    fn an_empty_default_folder_id_is_no_default() {
        assert_eq!(chosen_folder(Some("f1".into())), Some("f1".to_string()));
        assert_eq!(chosen_folder(Some(String::new())), None);
        assert_eq!(chosen_folder(None), None);
    }

    /// Reads that block until the test lets them return, counting how many
    /// started.
    static STARTED: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    static RELEASE: LazyLock<(Mutex<bool>, std::sync::Condvar)> =
        LazyLock::new(|| (Mutex::new(false), std::sync::Condvar::new()));

    fn hung_read(_: &Path) -> Result<u64, String> {
        STARTED.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let (released, wake) = &*RELEASE;
        let mut released = released.lock().unwrap();
        while !*released {
            released = wake.wait(released).unwrap();
        }
        Ok(7)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_hung_read_holds_one_blocking_thread_however_often_it_is_asked() {
        let reads = FreeSpaceReads::new(hung_read, Duration::from_millis(50));
        let path = PathBuf::from("/mnt/hung");
        let no_answer = Err("no answer within 50 ms".to_string());
        for _ in 0..5 {
            assert_eq!(reads.read(path.clone()).await, no_answer);
        }
        // Calls at once share the read too.
        let at_once = join_all((0..5).map(|_| reads.read(path.clone()))).await;
        assert!(at_once.iter().all(|r| r == &no_answer));
        assert_eq!(STARTED.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(reads.running.lock().unwrap().len(), 1);
        {
            let (released, wake) = &*RELEASE;
            *released.lock().unwrap() = true;
            wake.notify_all();
        }
        // The read returns and takes itself out; the next call starts anew.
        for _ in 0..100 {
            if reads.running.lock().unwrap().is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(reads.running.lock().unwrap().is_empty());
        assert_eq!(reads.read(path).await, Ok(7));
        assert_eq!(STARTED.load(std::sync::atomic::Ordering::SeqCst), 2);
    }

    #[test]
    fn the_timeout_text_names_the_wait() {
        assert_eq!(no_answer(FREE_SPACE_WAIT), "no answer within 2 seconds");
        assert_eq!(
            no_answer(Duration::from_secs(1)),
            "no answer within 1 second"
        );
        assert_eq!(
            no_answer(Duration::from_millis(1500)),
            "no answer within 1500 ms"
        );
    }

    fn panicking_read(_: &Path) -> Result<u64, String> {
        panic!("statvfs wrapper panicked");
    }

    #[tokio::test]
    async fn a_read_that_panics_leaves_no_entry_behind() {
        let reads = FreeSpaceReads::new(panicking_read, Duration::from_secs(5));
        let path = PathBuf::from("/mnt/broken");
        let first = reads.read(path.clone()).await.unwrap_err();
        assert!(first.contains("panicked"), "{first}");
        assert!(reads.running.lock().unwrap().is_empty());
    }

    /// A runtime that is shutting down drops a blocking closure without
    /// running it, inside spawn_blocking (tokio 1.48.0
    /// src/runtime/blocking/pool.rs:393-404), while `read` holds the lock on
    /// `running`. Dropping the unrun read must not take that lock again.
    #[test]
    fn a_read_the_runtime_drops_unrun_does_not_take_the_lock_its_caller_holds() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let handle = runtime.handle().clone();
        drop(runtime);
        let (done, finished) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _entered = handle.enter();
            let reads = FreeSpaceReads::new(free_space, FREE_SPACE_WAIT);
            let running = reads.running.lock().unwrap();
            let read = reads.start(PathBuf::from("/mnt/any"));
            drop(running);
            drop(read);
            done.send(()).unwrap();
        });
        assert!(
            finished.recv_timeout(Duration::from_secs(5)).is_ok(),
            "starting a read the runtime refused deadlocked on `running`"
        );
    }

    #[test]
    fn folders_that_would_pass_the_cap_are_left_out_and_counted() {
        let mut facts = sample_facts();
        // A canonical path of 4,000 control characters, about 24,000 characters once escaped:
        // three such folders do not fit in 60,000.
        let path = format!("/music/{}", "\u{1}".repeat(4_000));
        let folder = facts.folders[0].clone();
        facts.folders = (0..3)
            .map(|i| FolderFacts {
                folder: Folder {
                    id: format!("f{i}"),
                    name: "\u{1}".repeat(10_000),
                    path: path.clone(),
                    ..folder.folder.clone()
                },
                free: Ok(1000),
            })
            .collect();
        let mut doc = status_fields(&facts, &alice_caller(false));
        assert_eq!(
            (
                doc["folders"].as_array().unwrap().len(),
                &doc["folders_left_out"]
            ),
            (2, &json!(1))
        );
        let name = doc["folders"][0]["name"].as_str().unwrap();
        assert!(shared::failure::json_chars(name) <= TEXT_MAX_CHARS);
        doc.insert("ok".into(), json!(true));
        assert!(Value::Object(doc).to_string().chars().count() <= RESULT_MAX_CHARS);
    }

    #[test]
    fn batch_labels_and_importer_issues_are_cut_and_batches_past_the_cap_counted() {
        let mut facts = sample_facts();
        let quotes = "\"".repeat(5_000);
        facts.health.importer.issues[0].title = quotes.clone();
        facts.health.importer.issues[0].detail = quotes.clone();
        facts.running_batches = (0..200)
            .map(|i| RunningBatch {
                batch_id: format!("b{i}"),
                label: Some(quotes.clone()),
                unfinished_jobs: 1,
            })
            .collect();
        let mut doc = status_fields(&facts, &alice_caller(false));
        let issue = &doc["importer"]["issues"][0];
        assert_eq!(
            (
                issue["title"].as_str().unwrap(),
                issue["detail"].as_str().unwrap()
            ),
            (
                cut_json(quotes.clone(), TEXT_MAX_CHARS).as_str(),
                cut_json(quotes.clone(), RAW_MAX_CHARS).as_str()
            )
        );
        let batches = doc["running_batches"].as_array().unwrap();
        assert_eq!(
            batches[0]["label"],
            json!(cut_json(quotes.clone(), TEXT_MAX_CHARS))
        );
        assert!(
            batches.len() > 1 && batches.len() < 200,
            "{}",
            batches.len()
        );
        assert_eq!(doc["running_batches_left_out"], json!(200 - batches.len()));
        doc.insert("ok".into(), json!(true));
        assert!(Value::Object(doc).to_string().chars().count() <= RESULT_MAX_CHARS);
    }

    #[test]
    fn free_space_reads_the_filesystem_and_names_the_os_error() {
        let dir = tempfile::tempdir().unwrap();
        assert!(free_space(dir.path()).unwrap() > 0);
        assert_eq!(
            free_space(&dir.path().join("missing")),
            Err("No such file or directory (os error 2)".to_string())
        );
    }
}
