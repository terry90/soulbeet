//! Request keys (design 10.3): a write call repeated with the same `request_key` answers the
//! first call's result instead of acting again.

use std::collections::HashSet;
use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::sync::{LazyLock, Mutex};

use futures::FutureExt;

use rmcp::model::{CallToolResult, ContentBlock};
use serde_json::{Map, Value};
use shared::failure::{Failure, FailureCode, RetryAdvice, Stage};

use super::args::Args;
use super::contract::{failed, invalid_value, ToolError};
use super::McpCaller;
use crate::core::database_failure;
use crate::core::jobs::runner::Acquire;
use crate::ledger::{KeyReservation, RequestKey};
use crate::oauth::secrets::hash;

/// This process, in the reservations it makes: a reservation of another process was cut off
/// when that process stopped, and its key is free again.
static INSTANCE: LazyLock<String> = LazyLock::new(|| uuid::Uuid::new_v4().to_string());

/// Keys whose call ended in this process while the database refused both to store its result
/// and to let the key go: their rows still read as running, which they are not.
static UNRELEASED: LazyLock<Mutex<HashSet<RequestKey>>> = LazyLock::new(Default::default);

/// Lets `key` go after its call ended without a stored result, or remembers that it could not.
async fn release(ledger: &crate::ledger::Ledger, key: &RequestKey) {
    if ledger.release_key(key, &INSTANCE).await.is_err() {
        // The writer logged why.
        unreleased().insert(key.clone());
    }
}

fn unreleased() -> std::sync::MutexGuard<'static, HashSet<RequestKey>> {
    UNRELEASED.lock().unwrap_or_else(|e| e.into_inner())
}

/// The schema's bound on a request key.
const KEY_MAX_CHARS: usize = 200;

/// The `request_key` argument, trimmed.
pub(crate) fn request_key(args: &Args) -> Result<Option<String>, ToolError> {
    let refused = || {
        invalid_value(
            args.tool(),
            "request_key",
            "a non-empty string of at most 200 characters",
        )
    };
    let key = args.opt_string("request_key").map_err(|_| refused())?;
    match key {
        Some(key) if key.chars().count() > KEY_MAX_CHARS => Err(refused()),
        key => Ok(key),
    }
}

/// The first call with the key has not ended yet.
pub(crate) fn key_running() -> ToolError {
    ToolError {
        failure: Failure::new(
            FailureCode::AlreadyRunning,
            Stage::Input,
            RetryAdvice::SameSourceLater,
            "A call with this request_key is still running",
        )
        .with_retry_after(5),
        facts: Map::new(),
        tried: vec![],
        next: vec![],
    }
}

/// What a keyed call's work answered, and whether its key keeps the answer.
pub(crate) enum Keyed {
    /// The work made or joined a job, batch or search, or its answer is final: a repeat with
    /// the key answers it.
    Kept(CallToolResult),
    /// A refusal made before the work wrote anything: the key is let go, and a repeat with it
    /// runs the work again.
    Released(CallToolResult),
}

/// Runs `work` once per `key` of the caller's app for `tool` (design 10.3); without a key,
/// always. `call` is the call's arguments as the tool read them, `request_key` left out: a key
/// repeated with other arguments is refused. The work runs in its own task, so a client that
/// leaves does not stop it. A kept answer is stored for the key, failures included; a released
/// one, one the ledger could not store, or work that panicked lets the key go. A repeat answers
/// the stored result while the first call has ended, `key_running` while it runs.
pub(crate) async fn keyed<Fut>(
    acquire: &Acquire,
    caller: &McpCaller,
    tool: &'static str,
    key: Option<String>,
    call: &Value,
    work: Fut,
) -> CallToolResult
where
    Fut: Future<Output = Keyed> + Send + 'static,
{
    let Some(key) = key else {
        return run(tokio::spawn(async move {
            match work.await {
                Keyed::Kept(result) | Keyed::Released(result) => result,
            }
        }))
        .await;
    };
    let key = RequestKey {
        user: caller.ctx.id(),
        grant_id: caller.grant_id.clone(),
        tool: tool.to_string(),
        key,
    };
    let now = chrono::Utc::now().timestamp();
    let call_hash = hash(&call.to_string());
    let mut reserved = acquire
        .ledger
        .reserve_key(&key, &call_hash, &INSTANCE, now)
        .await;
    // A reservation of this process whose call ended: let go now, then taken afresh. Only the
    // call that takes the key out of UNRELEASED lets it go: another call that found it running
    // at the same time must not delete the reservation this one takes afresh.
    if matches!(reserved, Ok(KeyReservation::Running)) && unreleased().remove(&key) {
        reserved = match acquire.ledger.release_key(&key, &INSTANCE).await {
            Ok(()) => {
                (acquire.ledger)
                    .reserve_key(&key, &call_hash, &INSTANCE, now)
                    .await
            }
            Err(e) => {
                unreleased().insert(key.clone());
                Err(e)
            }
        };
    }
    match reserved {
        Err(e) => failed(ToolError {
            failure: database_failure(Stage::Input, e),
            facts: Map::new(),
            tried: vec![],
            next: vec![],
        }),
        Ok(KeyReservation::Done { document, is_error }) => {
            let block = vec![ContentBlock::text(document)];
            if is_error {
                CallToolResult::error(block)
            } else {
                CallToolResult::success(block)
            }
        }
        Ok(KeyReservation::Running) => failed(key_running()),
        Ok(KeyReservation::OtherCall) => failed(invalid_value(
            tool,
            "request_key",
            &format!("a key not used for another call of {tool}"),
        )),
        Ok(KeyReservation::Fresh) => {
            let ledger = acquire.ledger.clone();
            run(tokio::spawn(async move {
                let answered = match AssertUnwindSafe(work).catch_unwind().await {
                    Ok(answered) => answered,
                    Err(panic) => {
                        // Nothing is stored for the key: it is let go, and the panic goes on.
                        release(&ledger, &key).await;
                        std::panic::resume_unwind(panic);
                    }
                };
                let (result, keep) = match answered {
                    Keyed::Kept(result) => (result, true),
                    Keyed::Released(result) => (result, false),
                };
                let stored = keep && {
                    let document = result
                        .content
                        .first()
                        .and_then(|block| block.as_text())
                        .map(|text| text.text.clone())
                        .unwrap_or_default();
                    let is_error = result.is_error == Some(true);
                    ledger.finish_key(&key, document, is_error).await.is_ok()
                };
                // The writer logs a write it could not make; the caller gets the result either
                // way.
                if !stored {
                    release(&ledger, &key).await;
                }
                result
            }))
            .await
        }
    }
}

/// The result of the task running a call; a panic in it goes on in the caller.
async fn run(task: tokio::task::JoinHandle<CallToolResult>) -> CallToolResult {
    match task.await {
        Ok(result) => result,
        Err(e) => std::panic::resume_unwind(e.into_panic()),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use serde_json::json;
    use tokio::sync::Notify;

    use super::*;
    use crate::connection::test_util::{add_user, alice_caller, grant_id_of, text_of, RESOURCE};
    use crate::mcp::contract::success;

    /// A call a repeated key must never run.
    async fn never() -> Keyed {
        unreachable!("a repeated key never runs its call")
    }

    fn answer(text: &str) -> CallToolResult {
        success(Map::from_iter([("answer".to_string(), json!(text))]))
    }

    /// alice's caller through her grant, and the acquisition over `db`.
    async fn alice_on(db: sqlx::SqlitePool) -> (McpCaller, Arc<Acquire>) {
        add_user(&db, "u1", "alice").await;
        let caller = McpCaller {
            grant_id: grant_id_of(&db, "u1", RESOURCE).await,
            ..alice_caller(false)
        };
        (caller, Acquire::for_test(db).0)
    }

    /// `keyed` for find_sources with the key "k1" on one fixed call.
    async fn call_k1(
        acquire: &Acquire,
        caller: &McpCaller,
        work: impl Future<Output = Keyed> + Send + 'static,
    ) -> CallToolResult {
        let call = json!({"reference": {"kind": "track", "artist": "Kowloon", "title": "Wake Up"}});
        keyed(
            acquire,
            caller,
            "find_sources",
            Some("k1".into()),
            &call,
            work,
        )
        .await
    }

    #[tokio::test]
    async fn a_repeat_with_the_same_key_gets_the_first_document() {
        let (caller, acquire) = alice_on(crate::db::test_db::migrated().await).await;
        let call = json!({"reference": {"kind": "track", "artist": "Kowloon", "title": "Wake Up"}});
        let (entered, release) = (Arc::new(Notify::new()), Arc::new(Notify::new()));
        let first = tokio::spawn({
            let (acquire, caller, call) = (acquire.clone(), caller.clone(), call.clone());
            let (entered, release) = (entered.clone(), release.clone());
            async move {
                let work = async move {
                    entered.notify_one();
                    release.notified().await;
                    Keyed::Kept(answer("first"))
                };
                keyed(
                    &acquire,
                    &caller,
                    "find_sources",
                    Some("k1".into()),
                    &call,
                    work,
                )
                .await
            }
        });
        entered.notified().await;
        let running = keyed(
            &acquire,
            &caller,
            "find_sources",
            Some("k1".into()),
            &call,
            never(),
        )
        .await;
        let doc: Value = serde_json::from_str(text_of(&running)).unwrap();
        assert_eq!(
            (
                running.is_error,
                doc["error"]["code"].as_str(),
                doc["error"]["retry_after_seconds"].as_u64()
            ),
            (Some(true), Some("already_running"), Some(5))
        );
        release.notify_one();
        let first = first.await.unwrap();
        assert_eq!(text_of(&first), text_of(&answer("first")));
        let again = keyed(
            &acquire,
            &caller,
            "find_sources",
            Some("k1".into()),
            &call,
            never(),
        )
        .await;
        assert_eq!(
            (text_of(&again), again.is_error),
            (text_of(&first), Some(false))
        );
        // Without a key, every call runs.
        let unkeyed = keyed(&acquire, &caller, "find_sources", None, &call, async {
            Keyed::Kept(answer("second"))
        })
        .await;
        assert_eq!(text_of(&unkeyed), text_of(&answer("second")));
    }

    #[tokio::test]
    async fn a_call_refused_before_it_acted_leaves_its_key_to_the_next_call() {
        let (caller, acquire) = alice_on(crate::db::test_db::migrated().await).await;
        let limited = || {
            failed(ToolError {
                failure: Failure::new(
                    FailureCode::RateLimited,
                    Stage::SourceSearch,
                    RetryAdvice::SameSourceLater,
                    "This user already has 3 source searches waiting for their turn; read them with get_sources first",
                ),
                facts: Map::new(),
                tried: vec![],
                next: vec![],
            })
        };
        let refused = call_k1(&acquire, &caller, async move { Keyed::Released(limited()) }).await;
        assert_eq!(
            (text_of(&refused), refused.is_error),
            (text_of(&limited()), Some(true))
        );
        // The same call with the same key runs again, and its answer is the key's.
        let ran = call_k1(&acquire, &caller, async { Keyed::Kept(answer("second")) }).await;
        assert_eq!(text_of(&ran), text_of(&answer("second")));
        let again = call_k1(&acquire, &caller, never()).await;
        assert_eq!(text_of(&again), text_of(&answer("second")));
    }

    #[tokio::test]
    async fn a_call_that_panicked_leaves_its_key_to_the_next_call() {
        let (caller, acquire) = alice_on(crate::db::test_db::migrated().await).await;
        let panicked = tokio::spawn({
            let (acquire, caller) = (acquire.clone(), caller.clone());
            async move {
                call_k1(&acquire, &caller, async {
                    panic!("a bug in the call");
                })
                .await
            }
        });
        assert!(panicked.await.unwrap_err().is_panic());
        // Not left running: the same call with the same key runs again.
        let ran = call_k1(&acquire, &caller, async { Keyed::Kept(answer("second")) }).await;
        assert_eq!(text_of(&ran), text_of(&answer("second")));
    }

    #[tokio::test]
    async fn a_result_that_cannot_be_stored_leaves_its_key_to_the_next_call() {
        let db = crate::db::test_db::migrated().await;
        let (caller, acquire) = alice_on(db.clone()).await;
        // The database refuses to store any key's result.
        sqlx::query(
            "CREATE TRIGGER refuse_results BEFORE UPDATE OF document ON request_keys \
             BEGIN SELECT RAISE(ABORT, 'disk I/O error'); END",
        )
        .execute(&db)
        .await
        .unwrap();
        let first = call_k1(&acquire, &caller, async { Keyed::Kept(answer("first")) }).await;
        assert_eq!(text_of(&first), text_of(&answer("first")));
        sqlx::query("DROP TRIGGER refuse_results")
            .execute(&db)
            .await
            .unwrap();
        // Not left running: the same call with the same key runs again.
        let ran = call_k1(&acquire, &caller, async { Keyed::Kept(answer("second")) }).await;
        assert_eq!(text_of(&ran), text_of(&answer("second")));
    }

    #[tokio::test]
    async fn a_key_the_database_would_not_let_go_is_free_once_it_can() {
        let db = crate::db::test_db::migrated().await;
        let (caller, acquire) = alice_on(db.clone()).await;
        // The database refuses to store the result and to let the key go.
        for (name, event) in [
            ("refuse_results", "UPDATE OF document"),
            ("refuse_release", "DELETE"),
        ] {
            sqlx::query(&format!(
                "CREATE TRIGGER {name} BEFORE {event} ON request_keys \
                 BEGIN SELECT RAISE(ABORT, 'disk I/O error'); END"
            ))
            .execute(&db)
            .await
            .unwrap();
        }
        let first = call_k1(&acquire, &caller, async { Keyed::Kept(answer("first")) }).await;
        assert_eq!(text_of(&first), text_of(&answer("first")));
        for name in ["refuse_results", "refuse_release"] {
            sqlx::query(&format!("DROP TRIGGER {name}"))
                .execute(&db)
                .await
                .unwrap();
        }
        // The call ended: the same call with the same key runs again, not "still running".
        let ran = call_k1(&acquire, &caller, async { Keyed::Kept(answer("second")) }).await;
        assert_eq!(text_of(&ran), text_of(&answer("second")));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn calls_that_meet_a_key_the_database_would_not_let_go_run_it_once() {
        let db = crate::db::test_db::migrated().await;
        let (caller, acquire) = alice_on(db.clone()).await;
        let refuse = [
            ("refuse_results", "UPDATE OF document"),
            ("refuse_release", "DELETE"),
        ];
        let runs = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        for round in 0..20 {
            // The first call's key stays reserved and is remembered as not let go.
            for (name, event) in refuse {
                sqlx::query(&format!(
                    "CREATE TRIGGER {name} BEFORE {event} ON request_keys \
                     BEGIN SELECT RAISE(ABORT, 'disk I/O error'); END"
                ))
                .execute(&db)
                .await
                .unwrap();
            }
            call_k1(&acquire, &caller, async { Keyed::Kept(answer("first")) }).await;
            for (name, _) in refuse {
                sqlx::query(&format!("DROP TRIGGER {name}"))
                    .execute(&db)
                    .await
                    .unwrap();
            }
            // Calls that all find the key running at once: one of them lets it go and runs.
            runs.store(0, std::sync::atomic::Ordering::SeqCst);
            let calls: Vec<_> = (0..8)
                .map(|_| {
                    let (acquire, caller, runs) = (acquire.clone(), caller.clone(), runs.clone());
                    tokio::spawn(async move {
                        call_k1(&acquire, &caller, async move {
                            runs.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                            Keyed::Kept(answer("ran"))
                        })
                        .await
                    })
                })
                .collect();
            for call in calls {
                let answered = call.await.unwrap();
                let doc: Value = serde_json::from_str(text_of(&answered)).unwrap();
                assert!(
                    text_of(&answered) == text_of(&answer("ran"))
                        || doc["error"]["code"] == "already_running",
                    "round {round}: {doc}"
                );
            }
            assert_eq!(
                runs.load(std::sync::atomic::Ordering::SeqCst),
                1,
                "round {round}"
            );
            sqlx::query("DELETE FROM request_keys")
                .execute(&db)
                .await
                .unwrap();
        }
    }
}
