//! The documents every tool answers (design 10.1, 10.2): one text block
//! holding one JSON object, `{"ok": true, ...}` on success and
//! `{"ok": false, "error": {...}}` on failure.

use std::time::Duration;

use rmcp::model::{CallToolResult, ContentBlock};
use serde::Serialize;
use serde_json::{json, Map, Value};
use shared::failure::{Failure, FailureCode, RetryAdvice, Stage};

use crate::oauth::limits::retry_after_seconds;

/// A call that can still be tried, with its arguments.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub(crate) struct NextCall {
    pub tool: &'static str,
    pub args: Value,
}

/// A tool's failure: what went wrong, the facts it is about and what to call
/// next.
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

/// `{"ok": false, "error": {...}}` as one text block, the error being the
/// failure's own fields with `facts` and `next`; marked as an error, with no
/// structured content.
pub(crate) fn failed(error: ToolError) -> CallToolResult {
    let Value::Object(mut fields) = json!(error.failure) else {
        unreachable!("a Failure serializes as a JSON object");
    };
    fields.insert("facts".into(), Value::Object(error.facts));
    fields.insert("next".into(), json!(error.next));
    let mut document = Map::new();
    document.insert("ok".into(), false.into());
    document.insert("error".into(), Value::Object(fields));
    CallToolResult::error(vec![text(document)])
}

fn text(document: Map<String, Value>) -> ContentBlock {
    ContentBlock::text(Value::Object(document).to_string())
}

/// `tool` was given an argument it does not take, or a value it cannot use.
pub(crate) fn invalid_input(
    tool: &'static str,
    field: &str,
    expected: &str,
    next: Vec<NextCall>,
) -> ToolError {
    let failure = Failure::new(
        FailureCode::InvalidInput,
        Stage::Input,
        RetryAdvice::NotRetryable,
        format!("{tool} takes no argument named '{field}': {expected}"),
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

/// This app made more than `per_minute` tool calls in the current minute;
/// the window ends in `wait`.
pub(crate) fn rate_limited(per_minute: u32, wait: Duration) -> ToolError {
    let n = retry_after_seconds(wait);
    let failure = Failure::new(
        FailureCode::RateLimited,
        Stage::Input,
        RetryAdvice::SameSourceLater,
        format!("Too many tool calls from this app: at most {per_minute} a minute. Try again in {n} seconds."),
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

    #[test]
    fn a_failure_result_has_the_contract_shape() {
        let f = soulbeet::NavidromeError::CircuitOpen.failure(
            Stage::LibraryCheck,
            soulbeet::NavidromeError::CircuitOpen.to_string(),
        );
        let r = failed(ToolError {
            failure: f.clone(),
            facts: json!({"folder_id": null}).as_object().unwrap().clone(),
            next: vec![NextCall {
                tool: "get_status",
                args: json!({}),
            }],
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
    }

    #[test]
    fn a_bad_argument_names_the_field_and_what_was_expected() {
        let e = invalid_input(
            "get_status",
            "verbose",
            "it takes no arguments",
            vec![NextCall {
                tool: "get_status",
                args: json!({}),
            }],
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
