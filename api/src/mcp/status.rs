//! `get_status`: what Soulful can do for the user right now, read at once
//! (D9).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, NaiveDate, Utc};
use futures::future::{join_all, BoxFuture, Shared};
use futures::FutureExt;
use rmcp::model::{CallToolResult, JsonObject};
use serde_json::{json, Map, Value};
use shared::failure::{Failure, Stage};
use shared::system::{HealthLevel, SystemHealth};
use soulbeet::navidrome::ScanStatus;

use super::contract::{failed, invalid_input, success, NextCall, ToolError};
use super::McpCaller;
use crate::auth::UserCtx;
use crate::connection::ConnectionState;
use crate::core::{folders, health::system_health, settings};
use crate::models::folder::Folder;
use crate::oauth::metadata::mcp_url;
use crate::services::{navidrome_client_for_user, DEFAULT_DOWNLOAD_BACKEND};
use crate::transfer_registry::{RunningBatch, TransferRegistry};

/// How long a folder's free space is waited for.
const FREE_SPACE_WAIT: Duration = Duration::from_secs(2);

/// What `FreeSpaceReads::read` answers when the read takes longer than its
/// wait.
const NO_ANSWER: &str = "no answer within 2 seconds";

/// What the user's Navidrome account says, each read on its own.
#[derive(Clone)]
pub(crate) struct NavidromeFacts {
    pub admin: Result<bool, Failure>,
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
    pub default_folder_id: Option<String>,
    pub running_batches: Vec<RunningBatch>,
}

/// Where the facts come from, a seam for tests.
#[async_trait]
pub(crate) trait StatusSource: Send + Sync {
    async fn gather(&self, ctx: &UserCtx) -> Result<StatusFacts, Failure>;
}

/// The running instance's backends, database and registry.
pub(crate) struct LiveStatus;

#[async_trait]
impl StatusSource for LiveStatus {
    /// Reads every fact at once; fails only when the folders or settings
    /// cannot be read.
    async fn gather(&self, ctx: &UserCtx) -> Result<StatusFacts, Failure> {
        let (health, navidrome, folders, settings) = tokio::join!(
            system_health(ctx),
            navidrome_facts(ctx),
            folder_facts(ctx),
            settings::get(ctx),
        );
        Ok(StatusFacts {
            health,
            navidrome,
            folders: folders?,
            default_folder_id: chosen_folder(settings?.default_download_folder_id),
            running_batches: TransferRegistry::global().running_batches(&ctx.id()),
        })
    }
}

/// The folder a settings choice names: an empty id is a cleared choice
/// (core::settings::check_folder_choices), so no folder.
fn chosen_folder(id: Option<String>) -> Option<String> {
    id.filter(|id| !id.is_empty())
}

async fn navidrome_facts(ctx: &UserCtx) -> Option<NavidromeFacts> {
    let client = navidrome_client_for_user(&ctx.user_id).await.ok()?;
    let (user, scan) = tokio::join!(client.get_own_user(), client.get_scan_status());
    let failure = |e: soulbeet::NavidromeError| e.failure(Stage::LibraryCheck, e.to_string());
    Some(NavidromeFacts {
        admin: user.map(|u| u.admin_role).map_err(failure),
        scan: scan.map_err(failure),
    })
}

async fn folder_facts(ctx: &UserCtx) -> Result<Vec<FolderFacts>, Failure> {
    let folders = folders::list(ctx).await?;
    Ok(join_all(folders.into_iter().map(|folder| async move {
        let free = FREE_SPACE.read(PathBuf::from(&folder.path)).await;
        FolderFacts { folder, free }
    }))
    .await)
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

    /// The free space at `path`, or `NO_ANSWER` when the read, started now
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
            .unwrap_or_else(|_| Err(NO_ANSWER.into()))
    }

    /// A blocking read of `path` that takes itself out of `running` once it
    /// returns. The caller inserts it while it holds the lock, and the
    /// read's thread takes the same lock to remove it, so the entry it
    /// removes is its own.
    fn start(self: &Arc<Self>, path: PathBuf) -> FreeSpaceRead {
        let reads = self.clone();
        tokio::task::spawn_blocking(move || {
            let free = (reads.reader)(&path);
            reads
                .running
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .remove(&path);
            free
        })
        .map(|joined| joined.unwrap_or_else(|join| Err(join.to_string())))
        .boxed()
        .shared()
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
    if let Some(field) = args.iter().flat_map(|args| args.keys()).min() {
        return failed(invalid_input(
            "get_status",
            field,
            "it takes no arguments",
            vec![NextCall {
                tool: "get_status",
                args: json!({}),
            }],
        ));
    }
    match state.status.gather(&caller.ctx).await {
        Ok(facts) => success(status_fields(&facts, caller)),
        Err(failure) => failed(ToolError {
            failure,
            facts: Map::new(),
            next: vec![],
        }),
    }
}

/// The status document's fields; a fact that could not be read is `null`,
/// next to its failure.
pub(crate) fn status_fields(facts: &StatusFacts, caller: &McpCaller) -> Map<String, Value> {
    let health = &facts.health;
    let navidrome = facts.navidrome.as_ref();
    let (admin, admin_failure) = read(navidrome.map(|n| &n.admin), |admin| json!(admin));
    let (scan, scan_failure) = read(navidrome.map(|n| &n.scan), scan_document);
    let issues: Vec<Value> = health
        .importer
        .issues
        .iter()
        .map(|issue| {
            json!({
                "severity": level_name(issue.severity),
                "title": issue.title,
                "detail": issue.detail,
            })
        })
        .collect();
    let default = facts.default_folder_id.as_deref();
    let folders: Vec<Value> = facts
        .folders
        .iter()
        .map(|f| {
            let (free_bytes, free_space_failure) = match &f.free {
                Ok(bytes) => (json!(bytes), Value::Null),
                Err(e) => (Value::Null, json!(e)),
            };
            json!({
                "folder_id": f.folder.id,
                "name": f.folder.name,
                "path": f.folder.path,
                "default": Some(f.folder.id.as_str()) == default,
                "free_bytes": free_bytes,
                "free_space_failure": free_space_failure,
            })
        })
        .collect();
    let running_batches: Vec<Value> = facts
        .running_batches
        .iter()
        .map(|b| json!({"batch_id": b.batch_id, "label": b.label, "transfers": b.transfers}))
        .collect();
    let Value::Object(fields) = json!({
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
            // startScan is admin only (navidrome/server/subsonic/library_scanning.go:43-45).
            "can_trigger_scan": admin.clone(),
            "admin": admin,
            "admin_failure": admin_failure,
            "scan": scan,
            "scan_failure": scan_failure,
        },
        "folders": folders,
        "default_folder_id": facts.default_folder_id,
        "running_batches": running_batches,
        "deletion_enabled": caller.allow_delete,
    }) else {
        unreachable!("an object literal is an object");
    };
    fields
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
    let last_scan = (scan.last_scan != go_zero_time()).then(|| scan.last_scan.to_rfc3339());
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
                              "scan": {"scanning": false, "songs": 1342, "files_scanned": null,
                                       "last_scan": "2026-11-14T21:03:11.519322+00:00", "error": null},
                              "scan_failure": null},
                "folders": [{"folder_id": "f1", "name": "Music", "path": "/music/alice", "default": true, "free_bytes": 1000,
                             "free_space_failure": null}],
                "default_folder_id": "f1",
                "running_batches": [{"batch_id": "b1", "label": "Come Over", "transfers": 2}],
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
                "admin_failure": failure(&circuit()), "can_trigger_scan": null, "scan": null,
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
            "scan",
            "scan_failure",
        ] {
            assert_eq!(doc["navidrome"][key], Value::Null, "{key}");
        }
    }

    #[test]
    fn a_never_run_scan_has_no_last_scan_time() {
        let mut facts = sample_facts();
        facts.navidrome = Some(NavidromeFacts {
            admin: Ok(true),
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
        for _ in 0..5 {
            assert_eq!(reads.read(path.clone()).await, Err(NO_ANSWER.to_string()));
        }
        // Calls at once share the read too.
        let at_once = join_all((0..5).map(|_| reads.read(path.clone()))).await;
        assert!(at_once.iter().all(|r| r == &Err(NO_ANSWER.to_string())));
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
    fn free_space_reads_the_filesystem_and_names_the_os_error() {
        let dir = tempfile::tempdir().unwrap();
        assert!(free_space(dir.path()).unwrap() > 0);
        assert_eq!(
            free_space(&dir.path().join("missing")),
            Err("No such file or directory (os error 2)".to_string())
        );
    }
}
