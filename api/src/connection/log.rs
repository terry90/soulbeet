//! The latest requests the connection router answered, kept in memory for
//! "Remote access" in Settings.

use std::collections::VecDeque;
use std::sync::{Arc, LazyLock, Mutex, MutexGuard};

use axum::http::HeaderMap;

use crate::base_url::{BaseUrl, BaseUrlError};
use crate::connections::LoggedRequest;

/// How many requests the log keeps of each kind: refused or failed ones,
/// and answered ones. Kept apart, so a day of routine calls does not push
/// the last refusals out.
pub(crate) const LOG_CAPACITY: usize = 50;

/// How many characters of a path, an address problem or a header value an
/// entry keeps.
const FIELD_CHARS: usize = 200;

#[derive(Default)]
struct Entries {
    /// Numbers the entries in the order they were recorded.
    next: u64,
    refused: VecDeque<(u64, LoggedRequest)>,
    answered: VecDeque<(u64, LoggedRequest)>,
}

/// The newest `LOG_CAPACITY` refused or unanswered requests and the newest
/// `LOG_CAPACITY` answered ones.
#[derive(Default)]
pub(crate) struct RequestLog {
    entries: Mutex<Entries>,
}

/// A request is kept with the refusals when it got no answer, an error
/// status or a connection layer refusal.
fn is_refused(entry: &LoggedRequest) -> bool {
    entry.refusal.is_some() || entry.status.is_none_or(|status| status >= 400)
}

impl RequestLog {
    /// Adds `entry`, dropping the oldest one of its kind when that kind is
    /// full.
    pub(crate) fn record(&self, entry: LoggedRequest) {
        let mut entries = self.lock();
        let seq = entries.next;
        entries.next += 1;
        let kind = if is_refused(&entry) {
            &mut entries.refused
        } else {
            &mut entries.answered
        };
        if kind.len() == LOG_CAPACITY {
            kind.pop_back();
        }
        kind.push_front((seq, entry));
    }

    /// The entries of both kinds, newest first.
    pub(crate) fn recent(&self) -> Vec<LoggedRequest> {
        let entries = self.lock();
        let mut all: Vec<&(u64, LoggedRequest)> =
            entries.refused.iter().chain(&entries.answered).collect();
        all.sort_unstable_by_key(|(seq, _)| std::cmp::Reverse(*seq));
        all.into_iter().map(|(_, entry)| entry.clone()).collect()
    }

    fn lock(&self) -> MutexGuard<'_, Entries> {
        // An entry is pushed or popped whole, so a panic elsewhere leaves
        // the entries usable.
        self.entries.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// The running instance's log.
pub(crate) static LIVE_LOG: LazyLock<Arc<RequestLog>> = LazyLock::new(Default::default);

/// `text` cut to `FIELD_CHARS` characters.
pub(crate) fn cut(text: &str) -> String {
    text.chars().take(FIELD_CHARS).collect()
}

/// The value of the header `name`, cut to `FIELD_CHARS` characters.
pub(crate) fn header_value(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .map(|value| cut(&String::from_utf8_lossy(value.as_bytes())))
}

/// The derived address, or why there is none, as text cut to `FIELD_CHARS`
/// characters.
pub(crate) fn base_and_problem(
    result: &Result<BaseUrl, BaseUrlError>,
) -> (Option<String>, Option<String>) {
    match result {
        Ok(base) => (Some(base.to_string()), None),
        Err(e) => (None, Some(cut(&e.to_string()))),
    }
}

/// Records its entry when it is dropped: with the status `answered` gave
/// it, or with none when the request's future was dropped first because
/// the client left before the answer.
pub(crate) struct Recording {
    log: Arc<RequestLog>,
    entry: Option<LoggedRequest>,
}

impl Recording {
    /// Starts the entry of a request that has no answer yet.
    pub(crate) fn start(log: Arc<RequestLog>, entry: LoggedRequest) -> Recording {
        Recording {
            log,
            entry: Some(entry),
        }
    }

    /// Records the request with its answer's status and refusal code.
    pub(crate) fn answered(mut self, status: u16, refusal: Option<String>) {
        if let Some(entry) = self.entry.as_mut() {
            entry.status = Some(status);
            entry.refusal = refusal;
        }
    }
}

impl Drop for Recording {
    fn drop(&mut self) {
        if let Some(entry) = self.entry.take() {
            self.log.record(entry);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(at: i64, status: Option<u16>, refusal: Option<&str>) -> LoggedRequest {
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
            status,
            refusal: refusal.map(Into::into),
        }
    }

    #[test]
    fn the_log_keeps_the_newest_50_of_each_kind() {
        let log = RequestLog::default();
        for at in 0..=50 {
            log.record(entry(at, Some(200), None));
        }
        let recent = log.recent();
        assert_eq!(
            (recent.len(), recent[0].at, recent[49].at),
            (LOG_CAPACITY, 50, 1)
        );
    }

    #[test]
    fn routine_answers_do_not_push_out_the_last_refusals() {
        let log = RequestLog::default();
        log.record(entry(1, Some(403), Some("origin_not_allowed")));
        log.record(entry(2, Some(401), None));
        log.record(entry(3, None, None));
        // A day of tool calls.
        for at in 10..10_000 {
            log.record(entry(at, Some(200), None));
        }
        let recent = log.recent();
        assert_eq!(recent.len(), LOG_CAPACITY + 3);
        assert_eq!(recent[0].at, 9_999);
        assert_eq!(
            recent[LOG_CAPACITY..]
                .iter()
                .rev()
                .take(3)
                .map(|e| (e.at, e.status))
                .collect::<Vec<_>>(),
            [(1, Some(403)), (2, Some(401)), (3, None)]
        );
    }

    #[test]
    fn entries_come_newest_first_across_both_kinds() {
        let log = RequestLog::default();
        log.record(entry(1, Some(200), None));
        log.record(entry(1, Some(404), Some("not_found")));
        log.record(entry(1, Some(303), None));
        let statuses: Vec<_> = log.recent().into_iter().map(|e| e.status).collect();
        assert_eq!(statuses, [Some(303), Some(404), Some(200)]);
    }

    #[test]
    fn a_request_dropped_before_its_answer_is_recorded_without_a_status() {
        let log = Arc::new(RequestLog::default());
        drop(Recording::start(log.clone(), entry(1, None, None)));
        Recording::start(log.clone(), entry(2, None, None)).answered(200, None);
        let recent = log.recent();
        assert_eq!(
            recent.iter().map(|e| (e.at, e.status)).collect::<Vec<_>>(),
            [(2, Some(200)), (1, None)]
        );
    }

    #[test]
    fn long_fields_are_cut_to_200_characters() {
        assert_eq!(cut(&"é".repeat(300)).chars().count(), 200);
        assert_eq!(cut("short"), "short");
    }
}
