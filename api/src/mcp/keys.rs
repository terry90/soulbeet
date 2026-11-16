//! Request keys (design 10.3): a write call repeated with the same `request_key` answers the
//! first call's result instead of acting again.

use std::future::Future;
use std::sync::LazyLock;

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
        next: vec![],
    }
}

/// Runs `work` once per `key` of the caller's app for `tool` (design 10.3); without a key,
/// always. `call` is the call's arguments as the tool read them, `request_key` left out: a key
/// repeated with other arguments is refused. The work runs in its own task, so a client that
/// leaves does not stop it, and its result is stored for the key, failures included. A repeat
/// answers the stored result while the first call has ended, `key_running` while it runs.
pub(crate) async fn keyed<Fut>(
    acquire: &Acquire,
    caller: &McpCaller,
    tool: &'static str,
    key: Option<String>,
    call: &Value,
    work: Fut,
) -> CallToolResult
where
    Fut: Future<Output = CallToolResult> + Send + 'static,
{
    let Some(key) = key else {
        return run(tokio::spawn(work)).await;
    };
    let key = RequestKey {
        user: caller.ctx.id(),
        grant_id: caller.grant_id.clone(),
        tool: tool.to_string(),
        key,
    };
    let now = chrono::Utc::now().timestamp();
    let reserved = acquire
        .ledger
        .reserve_key(&key, &hash(&call.to_string()), &INSTANCE, now)
        .await;
    match reserved {
        Err(e) => failed(ToolError {
            failure: database_failure(Stage::Input, e),
            facts: Map::new(),
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
                let result = work.await;
                let document = result
                    .content
                    .first()
                    .and_then(|block| block.as_text())
                    .map(|text| text.text.clone())
                    .unwrap_or_default();
                // The writer logs a result it could not store; the caller still gets it.
                let _ = ledger
                    .finish_key(&key, document, result.is_error == Some(true))
                    .await;
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
    async fn never() -> CallToolResult {
        unreachable!("a repeated key never runs its call")
    }

    fn answer(text: &str) -> CallToolResult {
        success(Map::from_iter([("answer".to_string(), json!(text))]))
    }

    #[tokio::test]
    async fn a_repeat_with_the_same_key_gets_the_first_document() {
        let db = crate::db::test_db::migrated().await;
        add_user(&db, "u1", "alice").await;
        let caller = McpCaller {
            grant_id: grant_id_of(&db, "u1", RESOURCE).await,
            ..alice_caller(false)
        };
        let (acquire, _) = Acquire::for_test(db);
        let call = json!({"reference": {"kind": "track", "artist": "Kowloon", "title": "Wake Up"}});
        let (entered, release) = (Arc::new(Notify::new()), Arc::new(Notify::new()));
        let first = tokio::spawn({
            let (acquire, caller, call) = (acquire.clone(), caller.clone(), call.clone());
            let (entered, release) = (entered.clone(), release.clone());
            async move {
                let work = async move {
                    entered.notify_one();
                    release.notified().await;
                    answer("first")
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
            answer("second")
        })
        .await;
        assert_eq!(text_of(&unkeyed), text_of(&answer("second")));
    }
}
