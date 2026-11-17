//! Times as every tool answers and takes them: RFC 3339 in UTC to the second,
//! `2026-11-15T10:00:00Z`, whatever form Soulful keeps them in.

use chrono::{DateTime, NaiveDateTime, SecondsFormat, Utc};
use serde_json::Value;

/// `at` as a tool answers it.
pub(crate) fn at(at: DateTime<Utc>) -> String {
    at.to_rfc3339_opts(SecondsFormat::Secs, true)
}

/// `secs`, Unix seconds, as a tool answers it; null for a time chrono cannot place.
pub(crate) fn unix(secs: i64) -> Value {
    DateTime::from_timestamp(secs, 0).map_or(Value::Null, |t| Value::String(at(t)))
}

/// A time Soulful stored as text, as a tool answers it: RFC 3339 with any offset, or SQLite's
/// `datetime('now')`, `YYYY-MM-DD HH:MM:SS` in UTC. Null for text in neither form.
pub(crate) fn text(stored: &str) -> Value {
    let parsed = DateTime::parse_from_rfc3339(stored)
        .map(|t| t.with_timezone(&Utc))
        .or_else(|_| {
            NaiveDateTime::parse_from_str(stored, "%Y-%m-%d %H:%M:%S").map(|t| t.and_utc())
        });
    match parsed {
        Ok(t) => Value::String(at(t)),
        Err(e) => {
            tracing::warn!("A stored time '{stored}' is not one Soulbeet writes: {e}");
            Value::Null
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn every_stored_form_answers_the_same_way() {
        assert_eq!(unix(1_763_200_800), json!("2025-11-15T10:00:00Z"));
        assert_eq!(text("2025-11-15 10:00:00"), json!("2025-11-15T10:00:00Z"));
        assert_eq!(text("2025-11-15T11:00:00+01:00"), json!("2025-11-15T10:00:00Z"));
        assert_eq!(text("2025-11-15T10:00:00.250Z"), json!("2025-11-15T10:00:00Z"));
        assert_eq!(text("yesterday"), Value::Null);
        assert_eq!(unix(i64::MAX), Value::Null);
    }
}
