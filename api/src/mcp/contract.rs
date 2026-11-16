//! The documents every tool answers (design 10.1, 10.2): one text block
//! holding one JSON object, `{"ok": true, ...}` on success and
//! `{"ok": false, "error": {...}}` on failure.

use std::time::Duration;

use rmcp::model::{CallToolResult, ContentBlock};
use serde::Serialize;
use serde_json::{json, Map, Value};
use shared::failure::{Failure, FailureCode, RetryAdvice, Stage};

use crate::oauth::limits::{retry_after_seconds, try_again_in};

/// A call that can still be tried, with its arguments.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub(crate) struct NextCall {
    pub tool: &'static str,
    pub args: Value,
    /// The deciding facts next to the call (design 10.2), flattened into it.
    #[serde(flatten)]
    pub facts: Map<String, Value>,
}

impl NextCall {
    pub(crate) fn new(tool: &'static str, args: Value) -> NextCall {
        NextCall {
            tool,
            args,
            facts: Map::new(),
        }
    }

    /// The call with the fact `key` next to it.
    pub(crate) fn with(mut self, key: &str, value: Value) -> NextCall {
        self.facts.insert(key.to_string(), value);
        self
    }
}

/// A tool's failure: what went wrong, the facts it is about and what to call
/// next.
#[derive(Debug)]
pub(crate) struct ToolError {
    pub failure: Failure,
    pub facts: Map<String, Value>,
    pub next: Vec<NextCall>,
}

/// `{"ok": true, ...fields}` as one text block.
pub(crate) fn success(fields: Map<String, Value>) -> CallToolResult {
    let mut document = Map::new();
    document.insert("ok".into(), true.into());
    document.extend(fields);
    CallToolResult::success(vec![text(document)])
}

/// A tool's answer: its fields as a success, or its failure.
pub(crate) fn answer(result: Result<Map<String, Value>, ToolError>) -> CallToolResult {
    match result {
        Ok(fields) => success(fields),
        Err(e) => failed(e),
    }
}

/// `{"ok": false, "error": {...}}` as one text block, the error being
/// `error_object`; marked as an error, with no structured content.
pub(crate) fn failed(error: ToolError) -> CallToolResult {
    let mut document = Map::new();
    document.insert("ok".into(), false.into());
    document.insert("error".into(), error_object(&error));
    CallToolResult::error(vec![text(document)])
}

/// The error a failed result carries: the failure's own fields with
/// `facts` and `next`.
pub(crate) fn error_object(error: &ToolError) -> Value {
    let Value::Object(mut fields) = json!(error.failure) else {
        unreachable!("a Failure serializes as a JSON object");
    };
    fields.insert("facts".into(), Value::Object(error.facts.clone()));
    fields.insert("next".into(), json!(error.next));
    Value::Object(fields)
}

fn text(document: Map<String, Value>) -> ContentBlock {
    ContentBlock::text(Value::Object(document).to_string())
}

/// `tool` was given an argument it does not take.
pub(crate) fn invalid_input(
    tool: &'static str,
    field: &str,
    expected: &str,
    next: Vec<NextCall>,
) -> ToolError {
    refused_input(
        format!("{tool} takes no argument named '{field}': {expected}"),
        field,
        expected,
        next,
    )
}

/// `tool` was given a value it cannot use for `field`; only the caller
/// knows the right one, so no call is offered (D1).
pub(crate) fn invalid_value(tool: &'static str, field: &str, expected: &str) -> ToolError {
    refused_input(
        format!("{tool}: {field} must be {expected}"),
        field,
        expected,
        vec![],
    )
}

fn refused_input(message: String, field: &str, expected: &str, next: Vec<NextCall>) -> ToolError {
    let failure = Failure::new(
        FailureCode::InvalidInput,
        Stage::Input,
        RetryAdvice::NotRetryable,
        message,
    );
    let Value::Object(facts) = json!({"field": field, "expected": expected}) else {
        unreachable!("an object literal is an object");
    };
    ToolError {
        failure,
        facts,
        next,
    }
}

/// What a failure's message adds when the call it would offer again is too
/// large to repeat in `next`.
const TOO_LARGE_TO_REPEAT: &str = "The call is too large to offer again in next.";

/// `message` followed by the sentence saying the call is too large to offer
/// again, as a sentence of its own.
pub(crate) fn too_large_to_repeat(message: &str) -> String {
    let stop = if message.ends_with('.') { " " } else { ". " };
    format!("{message}{stop}{TOO_LARGE_TO_REPEAT}")
}

/// A service's failure (D13): `retry`, the call that failed, is offered
/// when the failure says the same source can answer later, and when that
/// call is too large to repeat (None) the message says so; any other
/// failure offers nothing and relies on its `retry` and `user_action`.
pub(crate) fn upstream_error(
    mut failure: Failure,
    facts: Map<String, Value>,
    retry: Option<NextCall>,
) -> ToolError {
    let mut next = vec![];
    if failure.retry == RetryAdvice::SameSourceLater {
        match retry {
            Some(call) => next.push(call),
            None => failure.message = too_large_to_repeat(&failure.message),
        }
    }
    ToolError {
        failure,
        facts,
        next,
    }
}

/// What came of one item of a call acting on several.
pub(crate) enum ItemResult {
    /// The item's fields.
    Done(Map<String, Value>),
    Failed(ToolError),
}

/// The results of a call acting on several items (D2), in input order.
pub(crate) struct Items {
    /// The stage a `partial` failure is reported at.
    pub stage: Stage,
    /// What the items are called in the `partial` message, plural.
    pub noun: &'static str,
    /// The input position of the first result: where the cursor started.
    pub start: usize,
    pub results: Vec<ItemResult>,
    /// Items after the results left for a later call.
    pub pending: usize,
    /// Where that later call starts, when items are pending.
    pub cursor: Option<String>,
}

/// D2: the document `items` sends, as text, so a caller measures exactly
/// what goes out (D4).
pub(crate) fn items_text(items: &Items) -> String {
    let mut failed_indexes = Vec::new();
    let results: Vec<Value> = items
        .results
        .iter()
        .enumerate()
        .map(|(i, result)| {
            let index = items.start + i;
            let mut entry = Map::new();
            entry.insert("index".into(), index.into());
            match result {
                ItemResult::Done(fields) => {
                    entry.insert("ok".into(), true.into());
                    entry.extend(fields.clone());
                }
                ItemResult::Failed(error) => {
                    failed_indexes.push(index);
                    entry.insert("ok".into(), false.into());
                    entry.insert("error".into(), error_object(error));
                }
            }
            Value::Object(entry)
        })
        .collect();
    let requested = items.results.len() + items.pending;
    let failed = failed_indexes.len();
    let mut document = Map::new();
    document.insert("ok".into(), (failed == 0).into());
    document.insert(
        "summary".into(),
        json!({"requested": requested, "succeeded": items.results.len() - failed,
            "failed": failed, "pending": items.pending}),
    );
    document.insert("results".into(), Value::Array(results));
    document.insert("cursor".into(), json!(items.cursor));
    if failed > 0 {
        let partial = ToolError {
            failure: Failure::new(
                FailureCode::Partial,
                items.stage,
                RetryAdvice::PerItem,
                format!(
                    "{failed} of {requested} {} failed; each failed entry has its own error",
                    items.noun
                ),
            ),
            facts: Map::from_iter([("failed_indexes".to_string(), json!(failed_indexes))]),
            next: vec![],
        };
        document.insert("error".into(), error_object(&partial));
    }
    Value::Object(document).to_string()
}

/// D2: one text block of `items_text`, marked as an error when any result
/// failed. It sends every result: a caller keeps the document under the
/// cap with `items_text` first (D4).
pub(crate) fn items(items: Items) -> CallToolResult {
    let any_failed = items
        .results
        .iter()
        .any(|result| matches!(result, ItemResult::Failed(_)));
    let block = ContentBlock::text(items_text(&items));
    if any_failed {
        CallToolResult::error(vec![block])
    } else {
        CallToolResult::success(vec![block])
    }
}

/// This app made more than `per_minute` tool calls in the current minute;
/// the window ends in `wait`.
pub(crate) fn rate_limited(per_minute: u32, wait: Duration) -> ToolError {
    let n = retry_after_seconds(wait);
    let failure = Failure::new(
        FailureCode::RateLimited,
        Stage::Input,
        RetryAdvice::SameSourceLater,
        format!(
            "Too many tool calls from this app: at most {per_minute} a minute. {}",
            try_again_in(n)
        ),
    )
    .with_retry_after(n);
    ToolError {
        failure,
        facts: Map::new(),
        next: vec![],
    }
}

#[cfg(test)]
mod tests {
    use serde_json::from_str;

    use super::*;
    use crate::connection::test_util::text_of;
    use crate::mcp::args::tests::obj;
    use crate::mcp::args::Args;

    #[test]
    fn a_failure_result_has_the_contract_shape() {
        let f = soulbeet::NavidromeError::CircuitOpen.failure(
            Stage::LibraryCheck,
            soulbeet::NavidromeError::CircuitOpen.to_string(),
        );
        let r = failed(ToolError {
            failure: f.clone(),
            facts: json!({"folder_id": null}).as_object().unwrap().clone(),
            next: vec![NextCall::new("get_status", json!({}))],
        });
        assert_eq!(r.is_error, Some(true));
        assert!(r.structured_content.is_none() && r.content.len() == 1);
        let doc: Value = from_str(text_of(&r)).unwrap();
        let mut want = serde_json::to_value(&f).unwrap();
        want["facts"] = json!({"folder_id": null});
        want["next"] = json!([{"tool": "get_status", "args": {}}]);
        assert_eq!(doc, json!({"ok": false, "error": want}));
    }

    #[test]
    fn a_success_is_one_text_block_with_ok_true() {
        let r = success(
            json!({"base_url": "https://music.example.com"})
                .as_object()
                .unwrap()
                .clone(),
        );
        assert_eq!(
            (r.is_error, r.content.len(), r.structured_content.is_none()),
            (Some(false), 1, true)
        );
        assert_eq!(
            from_str::<Value>(text_of(&r)).unwrap(),
            json!({"ok": true, "base_url": "https://music.example.com"})
        );
    }

    #[test]
    fn a_rate_limit_says_when_to_come_back() {
        let e = rate_limited(120, Duration::from_millis(12_300));
        assert_eq!(
            (
                e.failure.code,
                e.failure.retry,
                e.failure.retry_after_seconds
            ),
            (
                FailureCode::RateLimited,
                RetryAdvice::SameSourceLater,
                Some(13)
            )
        );
        assert_eq!(
            e.failure.message,
            "Too many tool calls from this app: at most 120 a minute. Try again in 13 seconds."
        );
        assert!(e.next.is_empty() && e.facts.is_empty());
        assert_eq!(
            rate_limited(120, Duration::from_millis(400))
                .failure
                .message,
            "Too many tool calls from this app: at most 120 a minute. Try again in 1 second."
        );
    }

    fn circuit() -> ToolError {
        let e = soulbeet::NavidromeError::CircuitOpen;
        ToolError {
            failure: e.failure(Stage::LibraryCheck, e.to_string()),
            facts: Map::new(),
            next: vec![],
        }
    }

    #[test]
    fn several_items_report_each_and_the_summary() {
        let r = items(Items {
            stage: Stage::LibraryCheck,
            noun: "references",
            start: 0,
            pending: 0,
            cursor: None,
            results: vec![
                ItemResult::Done(json!({"matches": []}).as_object().unwrap().clone()),
                ItemResult::Done(json!({"matches": []}).as_object().unwrap().clone()),
            ],
        });
        assert_eq!(r.is_error, Some(false));
        let doc: Value = from_str(text_of(&r)).unwrap();
        assert_eq!(
            doc,
            json!({"ok": true, "summary": {"requested": 2, "succeeded": 2, "failed": 0, "pending": 0},
            "results": [{"index": 0, "ok": true, "matches": []}, {"index": 1, "ok": true, "matches": []}], "cursor": null})
        );
    }

    #[test]
    fn a_failed_item_makes_the_whole_result_partial() {
        let r = items(Items {
            stage: Stage::LibraryCheck,
            noun: "references",
            start: 3,
            pending: 0,
            cursor: None,
            results: vec![
                ItemResult::Done(json!({"matches": []}).as_object().unwrap().clone()),
                ItemResult::Failed(circuit()),
            ],
        });
        assert_eq!(r.is_error, Some(true));
        let doc: Value = from_str(text_of(&r)).unwrap();
        assert_eq!(doc["ok"], false);
        assert_eq!(
            doc["summary"],
            json!({"requested": 2, "succeeded": 1, "failed": 1, "pending": 0})
        );
        assert_eq!(
            doc["results"][1],
            json!({"index": 4, "ok": false, "error": error_object(&circuit())})
        );
        let e = &doc["error"];
        assert_eq!(
            (e["code"].as_str(), e["stage"].as_str(), e["retry"].as_str()),
            (Some("partial"), Some("library_check"), Some("per_item"))
        );
        assert_eq!(
            e["message"],
            "1 of 2 references failed; each failed entry has its own error"
        );
        assert_eq!(
            (&e["facts"], &e["next"]),
            (&json!({"failed_indexes": [4]}), &json!([]))
        );
    }

    #[test]
    fn items_left_for_a_later_call_are_pending_with_a_cursor() {
        let pending = Items {
            stage: Stage::LibraryCheck,
            noun: "references",
            start: 0,
            pending: 5,
            cursor: Some("c".into()),
            results: vec![ItemResult::Done(Map::new())],
        };
        let text = items_text(&pending);
        let r = items(pending);
        assert_eq!(text_of(&r), text);
        let doc: Value = from_str(&text).unwrap();
        assert_eq!(
            (
                doc["ok"].as_bool(),
                &doc["summary"]["pending"],
                &doc["summary"]["requested"],
                doc["cursor"].as_str()
            ),
            (Some(true), &json!(5), &json!(6), Some("c"))
        );
    }

    #[test]
    fn an_upstream_failure_offers_the_call_again_only_when_it_can_succeed_later() {
        let a = Args::new(
            "list_playlists",
            obj(json!({"limit": 10})),
            &["limit", "cursor"],
        );
        assert_eq!(
            upstream_error(circuit().failure, Map::new(), a.same_call()).next,
            vec![NextCall::new("list_playlists", json!({"limit": 10}))]
        );
        // getStarred2 on a database error: getStarredItems returns it and mapToSubsonicError writes
        // "Internal Server Error: {err}" (server/subsonic/album_lists.go:190-194, api.go:290-291)
        let refused = soulbeet::NavidromeError::Subsonic {
            code: 0,
            message: "Internal Server Error: database is locked".into(),
        };
        assert!(upstream_error(
            refused.failure(Stage::LibraryCheck, refused.to_string()),
            Map::new(),
            a.same_call()
        )
        .next
        .is_empty());
    }

    #[test]
    fn a_next_call_carries_its_deciding_facts() {
        let call = NextCall::new("get_sources", json!({"search_id": "s1"}))
            .with("needs", json!(["query_text"]));
        assert_eq!(
            json!(call),
            json!({"tool": "get_sources", "args": {"search_id": "s1"}, "needs": ["query_text"]})
        );
    }

    #[test]
    fn a_bad_value_names_the_field_and_what_was_expected() {
        let e = invalid_value("list_playlists", "limit", "an integer from 1 to 100");
        assert_eq!(
            (e.failure.code, e.failure.stage, e.failure.retry),
            (
                FailureCode::InvalidInput,
                Stage::Input,
                RetryAdvice::NotRetryable
            )
        );
        assert_eq!(
            e.failure.message,
            "list_playlists: limit must be an integer from 1 to 100"
        );
        assert_eq!(
            Value::Object(e.facts),
            json!({"field": "limit", "expected": "an integer from 1 to 100"})
        );
        assert!(e.next.is_empty());
    }

    #[test]
    fn a_bad_argument_names_the_field_and_what_was_expected() {
        let e = invalid_input(
            "get_status",
            "verbose",
            "it takes no arguments",
            vec![NextCall::new("get_status", json!({}))],
        );
        assert_eq!(
            (e.failure.code, e.failure.stage, e.failure.retry),
            (
                FailureCode::InvalidInput,
                Stage::Input,
                RetryAdvice::NotRetryable
            )
        );
        assert_eq!(
            e.failure.message,
            "get_status takes no argument named 'verbose': it takes no arguments"
        );
        assert_eq!(
            Value::Object(e.facts),
            json!({"field": "verbose", "expected": "it takes no arguments"})
        );
    }
}
