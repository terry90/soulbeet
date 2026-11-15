//! `get_status`: what Soulful can do for the user right now, read at once
//! (D9).

use std::path::{Path, PathBuf};
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, NaiveDate, Utc};
use futures::future::join_all;
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
            // An empty id is a cleared choice (core::settings::check_folder_choices).
            default_folder_id: settings?
                .default_download_folder_id
                .filter(|id| !id.is_empty()),
            running_batches: TransferRegistry::global().running_batches(&ctx.id()),
        })
    }
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
        let free = bounded_free_space(PathBuf::from(&folder.path)).await;
        FolderFacts { folder, free }
    }))
    .await)
}

/// `free_space` off the async threads, given up after `FREE_SPACE_WAIT`: a
/// hung network mount blocks `statvfs`.
async fn bounded_free_space(path: PathBuf) -> Result<u64, String> {
    let read = tokio::task::spawn_blocking(move || free_space(&path));
    match tokio::time::timeout(FREE_SPACE_WAIT, read).await {
        Err(_) => Err("no answer within 2 seconds".into()),
        Ok(Err(join)) => Err(join.to_string()),
        Ok(Ok(free)) => free,
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

fn scan_document(scan: &ScanStatus) -> Value {
    let last_scan = (scan.last_scan != go_zero_time()).then(|| scan.last_scan.to_rfc3339());
    let error = (!scan.error.is_empty()).then_some(scan.error.as_str());
    json!({"scanning": scan.scanning, "count": scan.count, "last_scan": last_scan, "error": error})
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
                              "scan": {"scanning": false, "count": 1342, "last_scan": "2026-11-14T21:03:11.519322+00:00", "error": null},
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
            json!({"scanning": false, "count": 0, "last_scan": null, "error": null})
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
    fn free_space_reads_the_filesystem_and_names_the_os_error() {
        let dir = tempfile::tempdir().unwrap();
        assert!(free_space(dir.path()).unwrap() > 0);
        assert_eq!(
            free_space(&dir.path().join("missing")),
            Err("No such file or directory (os error 2)".to_string())
        );
    }
}
