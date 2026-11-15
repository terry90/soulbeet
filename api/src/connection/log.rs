//! The latest requests the connection router answered, kept in memory for
//! "Remote access" in Settings.

use std::collections::VecDeque;
use std::sync::{Arc, LazyLock, Mutex, MutexGuard};

use axum::http::HeaderMap;

use crate::base_url::{BaseUrl, BaseUrlError};
use crate::connections::LoggedRequest;

/// How many requests the log keeps.
pub(crate) const LOG_CAPACITY: usize = 50;

/// How many characters of a header value an entry keeps.
const HEADER_CHARS: usize = 200;

/// The newest `LOG_CAPACITY` requests.
#[derive(Default)]
pub(crate) struct RequestLog {
    entries: Mutex<VecDeque<LoggedRequest>>,
}

impl RequestLog {
    /// Adds `entry`, dropping the oldest one when the log is full.
    pub(crate) fn record(&self, entry: LoggedRequest) {
        let mut entries = self.lock();
        if entries.len() == LOG_CAPACITY {
            entries.pop_back();
        }
        entries.push_front(entry);
    }

    /// The entries, newest first.
    pub(crate) fn recent(&self) -> Vec<LoggedRequest> {
        self.lock().iter().cloned().collect()
    }

    fn lock(&self) -> MutexGuard<'_, VecDeque<LoggedRequest>> {
        // An entry is pushed or popped whole, so a panic elsewhere leaves
        // the entries usable.
        self.entries.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// The running instance's log.
pub(crate) static LIVE_LOG: LazyLock<Arc<RequestLog>> = LazyLock::new(Default::default);

/// The value of the header `name`, cut to `HEADER_CHARS` characters.
pub(crate) fn header_value(headers: &HeaderMap, name: &str) -> Option<String> {
    headers.get(name).map(|value| {
        String::from_utf8_lossy(value.as_bytes())
            .chars()
            .take(HEADER_CHARS)
            .collect()
    })
}

/// The derived address, or why there is none, as text.
pub(crate) fn base_and_problem(
    result: &Result<BaseUrl, BaseUrlError>,
) -> (Option<String>, Option<String>) {
    match result {
        Ok(base) => (Some(base.to_string()), None),
        Err(e) => (None, Some(e.to_string())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(at: i64) -> LoggedRequest {
        LoggedRequest {
            at,
            method: "GET".into(),
            path: "/mcp".into(),
            base: None,
            base_problem: None,
            host_from: "host".into(),
            scheme_from: "named_host_is_https".into(),
            host_header: None,
            x_forwarded_host: None,
            x_forwarded_proto: None,
            forwarded: None,
            status: 200,
            refusal: None,
        }
    }

    #[test]
    fn the_log_keeps_the_newest_50() {
        let log = RequestLog::default();
        for at in 0..=50 {
            log.record(entry(at));
        }
        let recent = log.recent();
        assert_eq!(
            (recent.len(), recent[0].at, recent[49].at),
            (LOG_CAPACITY, 50, 1)
        );
    }
}
