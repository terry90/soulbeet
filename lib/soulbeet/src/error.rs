use crate::backend_error::{BackendFault, PeerAsk};
use shared::failure::{
    Failure, FailureCode, RetryAdvice, Service, Stage, UserAction, UserActionCode,
};
use shared::slskd::says_not_logged_in;
use thiserror::Error;

#[derive(Error, Debug)]
pub enum SoulseekError {
    #[error("{0}")]
    NotConfigured(MissingSetting),

    /// A request that got no answer, or whose answer could not be read; never with its URL
    /// (`From<reqwest::Error>`).
    #[error("Request error: {0}")]
    Request(reqwest::Error),

    #[error("URL parsing error: {0}")]
    UrlParse(#[from] url::ParseError),

    #[error("API error: {status} - {message}")]
    Api { status: u16, message: String },

    /// The batch enqueue route answered 404: it only exists since slskd 0.26.
    #[error(
        "API error: 404 - transfers/downloads/batches not found; slskd 0.26 or newer is required ({body})"
    )]
    SlskdTooOld { body: String },

    /// slskd answered `status` with `body`, `User {username} appears to be
    /// offline`, for a peer that is offline or that it blacklists.
    #[error("User '{username}' is offline")]
    UserOffline {
        username: String,
        status: u16,
        body: String,
    },

    /// slskd refused a search's request with 400: an `ArgumentException`
    /// about the query, such as `Search query must contain at least one
    /// non-exclusion term (Parameter 'query')`, or a request field it could
    /// not validate.
    #[error("API error: 400 - {body}")]
    SearchQueryRejected { body: String },

    /// slskd answered a search's request with 400 `An active search with
    /// token {token} is already in progress` (Soulseek.NET's
    /// `DuplicateTokenException`).
    #[error("API error: 400 - {body}")]
    SearchAlreadyRunning { body: String },

    /// slskd answered 404 to a poll, reading the search's status or responses
    /// or cancelling it: it has no search with that id any more
    /// (`SearchesController.GetById`, `GetResponsesById`, `Cancel`), for
    /// example because it was deleted from slskd.
    #[error("API error: 404 - {body}")]
    SearchGone { body: String },

    /// A poll for a search this client is not running: it never started it,
    /// the search already ended, or the client was rebuilt since.
    #[error("Search {search_id} is not known to this client")]
    UnknownSearch { search_id: String },

    /// slskd completed a search and counted responses, but its responses
    /// stayed empty: it leaves the record that way when its final update
    /// fails (`SearchService.cs:370-374`).
    #[error("slskd counted {response_count} responses but returned none")]
    SearchResponsesLost { response_count: u32 },

    /// A search slskd was told to cancel at the deadline was still running
    /// after the cancel grace.
    #[error("Search {search_id} did not end after it was cancelled")]
    SearchNotEnding { search_id: String },

    /// slskd ended a search `Completed, Errored` with no response: Soulseek.NET threw while the
    /// search ran (`SearchService.cs:331-345`), so no peer may have been asked.
    #[error("slskd ended search {search_id} with an error")]
    SearchErrored { search_id: String },

    #[error("Unparseable slskd response: {0}")]
    InvalidResponse(String),

    #[error("Peer name '{username}' cannot be sent in a URL path")]
    UnaddressablePeer { username: String },

    /// A search without an album names its target by exactly one track.
    #[error("A search without an album takes exactly one track, not {tracks}")]
    NoSearchTarget { tracks: usize },
}

/// A setting the slskd client cannot run without, as Soulful stores it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MissingSetting {
    Url,
    ApiKey,
    /// The URL or the API key is saved empty.
    Empty,
}

impl std::fmt::Display for MissingSetting {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            MissingSetting::Url => "slskd URL not configured",
            MissingSetting::ApiKey => "slskd API key not configured",
            MissingSetting::Empty => "slskd not configured",
        })
    }
}

/// reqwest's text for an error of `send` ends with the request's URL (reqwest 0.12.28
/// `src/error.rs:266-268`), which names the slskd host and the peer or file asked about. A
/// failure never holds a request URL (design 10), so every request error is kept without it.
impl From<reqwest::Error> for SoulseekError {
    fn from(e: reqwest::Error) -> Self {
        SoulseekError::Request(e.without_url())
    }
}

impl SoulseekError {
    /// Whether this error is worth retrying. Returns false for definitive
    /// failures like user offline where retrying would be wasteful.
    pub fn is_retryable(&self) -> bool {
        match self {
            SoulseekError::UserOffline { .. } => false,
            SoulseekError::NotConfigured(_) => false,
            SoulseekError::UnaddressablePeer { .. } => false,
            SoulseekError::SlskdTooOld { .. } => false,
            SoulseekError::SearchQueryRejected { .. } => false,
            SoulseekError::SearchAlreadyRunning { .. } => false,
            SoulseekError::SearchGone { .. } => false,
            SoulseekError::UnknownSearch { .. } => false,
            SoulseekError::SearchResponsesLost { .. } => false,
            SoulseekError::SearchNotEnding { .. } => false,
            SoulseekError::SearchErrored { .. } => false,
            SoulseekError::NoSearchTarget { .. } => false,
            // 400 and 404 are definitive: a rejected request body or a
            // missing record will not heal on retry.
            SoulseekError::Api {
                status: 400 | 401 | 403 | 404,
                ..
            } => false,
            _ => true,
        }
    }

    /// This error as the failure of `stage`, shown as `message`. The cause
    /// is slskd's status and body when slskd answered, else the Display.
    pub fn failure(&self, stage: Stage, message: impl Into<String>) -> Failure {
        use FailureCode::*;
        use RetryAdvice::*;
        let by_stage = match stage {
            Stage::Transfer => TransferListUnreadable,
            Stage::Enqueue => EnqueueFailed,
            _ => BackendUnreachable,
        };
        let (code, retry) = match self {
            SoulseekError::UserOffline { .. } => (PeerOffline, OtherSource),
            SoulseekError::SlskdTooOld { .. } => (FailureCode::SlskdTooOld, NeedsUser),
            // A URL that does not parse is the configured one: every
            // endpoint is joined onto it.
            SoulseekError::NotConfigured(_)
            | SoulseekError::UrlParse(_)
            | SoulseekError::Api {
                status: 401 | 403, ..
            } => (BackendNotConfigured, NeedsUser),
            SoulseekError::SearchQueryRejected { .. } => {
                (FailureCode::SearchQueryRejected, OtherQuery)
            }
            SoulseekError::SearchAlreadyRunning { .. } => {
                (FailureCode::SearchAlreadyRunning, SameSourceLater)
            }
            // slskd or this client no longer has the search: running it
            // again is the remedy.
            SoulseekError::SearchGone { .. }
            | SoulseekError::UnknownSearch { .. }
            | SoulseekError::SearchResponsesLost { .. } => (SearchLost, SameSourceLater),
            SoulseekError::SearchNotEnding { .. } => (SearchTimeout, SameSourceLater),
            // slskd's Soulseek client failed during the search: the same search later.
            SoulseekError::SearchErrored { .. } => (BackendUnreachable, SameSourceLater),
            SoulseekError::Api { status: 429, .. } if stage == Stage::SourceSearch => {
                (SearchRateLimited, SameSourceLater)
            }
            SoulseekError::Api { status: 429, .. } => (EnqueueFailed, SameSourceLater),
            SoulseekError::Api {
                status: 408 | 503, ..
            }
            | SoulseekError::Request(_) => (BackendUnreachable, SameSourceLater),
            SoulseekError::UnaddressablePeer { .. } => (EnqueueFailed, OtherSource),
            SoulseekError::NoSearchTarget { .. } => (InvalidInput, NotRetryable),
            // A request slskd refused as malformed: the same files fail
            // again.
            SoulseekError::Api { status: 400, .. } if stage == Stage::Enqueue => {
                (EnqueueFailed, NotRetryable)
            }
            // slskd is not logged in to Soulseek: every peer fails the same
            // way until it logs back in.
            SoulseekError::Api {
                status: 500,
                message,
            } if says_not_logged_in(message) => (BackendUnreachable, SameSourceLater),
            // slskd could not reach the peer.
            SoulseekError::Api { status: 500, .. } if stage == Stage::Enqueue => {
                (EnqueueFailed, OtherSource)
            }
            // An answer Soulful cannot parse will not parse later either.
            // At transfer it is the downloads list, which has its own code.
            SoulseekError::InvalidResponse(_) if stage == Stage::Transfer => {
                (TransferListUnreadable, NotRetryable)
            }
            SoulseekError::InvalidResponse(_) => (BackendResponseUnreadable, NotRetryable),
            SoulseekError::Api { .. } => (by_stage, SameSourceLater),
        };
        let (status, raw) = match self {
            SoulseekError::Api { status, message } => (Some(*status), message.clone()),
            SoulseekError::SlskdTooOld { body } | SoulseekError::SearchGone { body } => {
                (Some(404), body.clone())
            }
            SoulseekError::UserOffline { status, body, .. } => (Some(*status), body.clone()),
            SoulseekError::SearchQueryRejected { body }
            | SoulseekError::SearchAlreadyRunning { body } => (Some(400), body.clone()),
            other => (None, other.to_string()),
        };
        let failure = Failure::new(code, stage, retry, message).with_cause(
            Service::Slskd,
            status.map(i32::from),
            raw,
        );
        match self {
            SoulseekError::InvalidResponse(_) => {
                failure.with_user_action(update_soulful_or_slskd())
            }
            _ if code == BackendNotConfigured => failure.with_user_action(configure_slskd()),
            SoulseekError::SlskdTooOld { .. } => failure.with_user_action(upgrade_slskd()),
            _ => failure,
        }
    }
}

/// Soulful's own words for the ask that failed: slskd's text, which names peers and remote files,
/// stays in the failure's cause. slskd asks the peer for a place in its queue
/// (`GET transfers/downloads/{username}/{id}/position`) and the Soulseek server for a presence
/// (`GET users/{username}/status`).
fn ask_message(ask: PeerAsk) -> &'static str {
    match ask {
        PeerAsk::QueuePosition => {
            "slskd could not ask the source for this transfer's place in its queue"
        }
        PeerAsk::Presence => "slskd could not say whether the source is online",
    }
}

impl SoulseekError {
    /// This error of `ask` as a failure at `stage`, with the ask's own message. slskd answers
    /// 500 with the exception's text when it is not logged in (backend_unreachable), when the
    /// peer is offline (peer_offline, read as `UserOffline`), and when the one it asked did not
    /// answer in time or the transfer is no longer active (`ask.unanswered()`: Soulseek.NET
    /// `Waiter.cs:161`, `SoulseekClient.cs:1643-1646,3866`). Each says to ask again later.
    pub fn ask_failure(&self, ask: PeerAsk, stage: Stage) -> Failure {
        match self {
            SoulseekError::Api {
                status: 500,
                message,
            } if !says_not_logged_in(message) => Failure::new(
                ask.unanswered(),
                stage,
                RetryAdvice::SameSourceLater,
                ask_message(ask),
            )
            .with_cause(Service::Slskd, Some(500), message.clone()),
            // Not the downloads list, whose own code transfer_list_unreadable is.
            SoulseekError::InvalidResponse(_) => Failure::new(
                FailureCode::BackendResponseUnreadable,
                stage,
                RetryAdvice::NotRetryable,
                ask_message(ask),
            )
            .with_cause(Service::Slskd, None, self.to_string())
            .with_user_action(update_soulful_or_slskd()),
            other => other.failure(stage, ask_message(ask)),
        }
    }
}

fn configure_slskd() -> UserAction {
    UserAction::new(
        UserActionCode::ConfigureBackend,
        "Set the slskd URL and API key",
        "Settings > Config",
    )
}

fn upgrade_slskd() -> UserAction {
    UserAction::new(
        UserActionCode::UpgradeBackend,
        "Upgrade slskd to 0.26 or newer",
        "slskd",
    )
}

fn update_soulful_or_slskd() -> UserAction {
    UserAction::new(
        UserActionCode::UpdateSoulfulOrBackend,
        "Update Soulbeet or slskd so that Soulbeet can read slskd's answers again",
        "Soulbeet and slskd",
    )
}

impl BackendFault for SoulseekError {
    fn failure(&self, stage: Stage, message: String) -> Failure {
        SoulseekError::failure(self, stage, message)
    }

    fn ask_failure(&self, ask: PeerAsk, stage: Stage) -> Failure {
        SoulseekError::ask_failure(self, ask, stage)
    }

    fn answer_unreadable(&self) -> bool {
        matches!(self, SoulseekError::InvalidResponse(_))
    }
}

pub type Result<T> = std::result::Result<T, SoulseekError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn soulseek_errors_map_to_failure_codes() {
        let api = |status| SoulseekError::Api {
            status,
            message: "body".into(),
        };
        let cases = [
            (
                offline(),
                Stage::Enqueue,
                FailureCode::PeerOffline,
                RetryAdvice::OtherSource,
            ),
            (
                SoulseekError::SlskdTooOld { body: "nf".into() },
                Stage::Enqueue,
                FailureCode::SlskdTooOld,
                RetryAdvice::NeedsUser,
            ),
            (
                api(403),
                Stage::Enqueue,
                FailureCode::BackendNotConfigured,
                RetryAdvice::NeedsUser,
            ),
            (
                api(503),
                Stage::Enqueue,
                FailureCode::BackendUnreachable,
                RetryAdvice::SameSourceLater,
            ),
            (
                api(429),
                Stage::SourceSearch,
                FailureCode::SearchRateLimited,
                RetryAdvice::SameSourceLater,
            ),
            (
                api(429),
                Stage::Enqueue,
                FailureCode::EnqueueFailed,
                RetryAdvice::SameSourceLater,
            ),
            (
                SoulseekError::InvalidResponse("x".into()),
                Stage::Transfer,
                FailureCode::TransferListUnreadable,
                RetryAdvice::NotRetryable,
            ),
            (
                SoulseekError::InvalidResponse("x".into()),
                Stage::Enqueue,
                FailureCode::BackendResponseUnreadable,
                RetryAdvice::NotRetryable,
            ),
        ];
        for (e, stage, code, retry) in cases {
            let f = e.failure(stage, e.to_string());
            assert_eq!((f.code, f.retry, f.stage), (code, retry, stage), "{e:?}");
            assert_eq!(f.message, e.to_string());
        }
        let f = api(503).failure(Stage::Enqueue, "m");
        let cause = f.cause.unwrap();
        assert_eq!(
            (cause.service, cause.status, cause.raw.as_str()),
            (Service::Slskd, Some(503), "body")
        );
        let too_old = SoulseekError::SlskdTooOld { body: "nf".into() }.failure(Stage::Enqueue, "m");
        assert_eq!(
            too_old.user_action.unwrap().code,
            UserActionCode::UpgradeBackend
        );
    }

    /// slskd's own bodies for each status (`TransfersController.cs`,
    /// `SearchesController.cs`, slskd 0.26).
    #[test]
    fn slskd_answers_get_the_advice_their_cause_calls_for() {
        let api = |status, message: &str| SoulseekError::Api {
            status,
            message: message.into(),
        };
        let cases = [
            (
                api(400, "Two or more files in the request are repeated"),
                Stage::Enqueue,
                FailureCode::EnqueueFailed,
                RetryAdvice::NotRetryable,
            ),
            (
                api(
                    500,
                    "Failed to establish a direct or indirect message connection to peer (10.0.0.2)",
                ),
                Stage::Enqueue,
                FailureCode::EnqueueFailed,
                RetryAdvice::OtherSource,
            ),
            (
                api(
                    500,
                    "The server connection must be connected and logged in to fetch user endpoint (currently: Disconnected)",
                ),
                Stage::Enqueue,
                FailureCode::BackendUnreachable,
                RetryAdvice::SameSourceLater,
            ),
            (
                SoulseekError::SearchQueryRejected {
                    body: "Search query must contain at least one non-exclusion term (Parameter 'query')".into(),
                },
                Stage::SourceSearch,
                FailureCode::SearchQueryRejected,
                RetryAdvice::OtherQuery,
            ),
            (
                SoulseekError::SearchAlreadyRunning {
                    body: "An active search with token 17 is already in progress".into(),
                },
                Stage::SourceSearch,
                FailureCode::SearchAlreadyRunning,
                RetryAdvice::SameSourceLater,
            ),
            (
                api(
                    409,
                    "The server connection must be connected and logged in to perform a search (currently: Disconnected)",
                ),
                Stage::SourceSearch,
                FailureCode::BackendUnreachable,
                RetryAdvice::SameSourceLater,
            ),
            (
                api(401, ""),
                Stage::Transfer,
                FailureCode::BackendNotConfigured,
                RetryAdvice::NeedsUser,
            ),
            (
                SoulseekError::NotConfigured(MissingSetting::Url),
                Stage::SourceSearch,
                FailureCode::BackendNotConfigured,
                RetryAdvice::NeedsUser,
            ),
            (
                SoulseekError::UnaddressablePeer {
                    username: "..".into(),
                },
                Stage::Enqueue,
                FailureCode::EnqueueFailed,
                RetryAdvice::OtherSource,
            ),
            (
                SoulseekError::InvalidResponse("x".into()),
                Stage::SourceSearch,
                FailureCode::BackendResponseUnreadable,
                RetryAdvice::NotRetryable,
            ),
            (
                SoulseekError::UrlParse(url::ParseError::RelativeUrlWithoutBase),
                Stage::Transfer,
                FailureCode::BackendNotConfigured,
                RetryAdvice::NeedsUser,
            ),
            (
                SoulseekError::SearchGone {
                    body: String::new(),
                },
                Stage::SourceSearch,
                FailureCode::SearchLost,
                RetryAdvice::SameSourceLater,
            ),
            (
                SoulseekError::UnknownSearch {
                    search_id: "3f2b".into(),
                },
                Stage::SourceSearch,
                FailureCode::SearchLost,
                RetryAdvice::SameSourceLater,
            ),
            (
                SoulseekError::SearchResponsesLost { response_count: 2 },
                Stage::SourceSearch,
                FailureCode::SearchLost,
                RetryAdvice::SameSourceLater,
            ),
            (
                SoulseekError::SearchNotEnding {
                    search_id: "3f2b".into(),
                },
                Stage::SourceSearch,
                FailureCode::SearchTimeout,
                RetryAdvice::SameSourceLater,
            ),
            (
                SoulseekError::SearchErrored {
                    search_id: "3f2b".into(),
                },
                Stage::SourceSearch,
                FailureCode::BackendUnreachable,
                RetryAdvice::SameSourceLater,
            ),
        ];
        for (e, stage, code, retry) in cases {
            let f = e.failure(stage, e.to_string());
            assert_eq!((f.code, f.retry), (code, retry), "{e:?} at {stage:?}");
            let raw = match &e {
                SoulseekError::Api { message, .. } => message.clone(),
                SoulseekError::SearchQueryRejected { body }
                | SoulseekError::SearchAlreadyRunning { body }
                | SoulseekError::SearchGone { body } => body.clone(),
                other => other.to_string(),
            };
            assert_eq!(f.cause.unwrap().raw, raw, "{e:?}");
        }
        let unconfigured =
            SoulseekError::NotConfigured(MissingSetting::ApiKey).failure(Stage::Enqueue, "m");
        assert_eq!(
            unconfigured.user_action.unwrap().code,
            UserActionCode::ConfigureBackend
        );
        let rejected = SoulseekError::SearchQueryRejected { body: "b".into() }
            .failure(Stage::SourceSearch, "m");
        assert_eq!(rejected.cause.unwrap().status, Some(400));
        let gone = SoulseekError::SearchGone {
            body: String::new(),
        };
        assert_eq!(gone.to_string(), "API error: 404 - ");
        assert_eq!(
            gone.failure(Stage::SourceSearch, "m").cause.unwrap().status,
            Some(404)
        );
    }

    /// slskd's 500 bodies for a failed ask, unwrapped from their JSON string (Program.cs:1051-1053),
    /// each with Soulful's message and slskd's text kept in the cause.
    #[test]
    fn a_failed_ask_about_a_peer_says_who_did_not_answer() {
        let api = |message: &str| SoulseekError::Api {
            status: 500,
            message: message.into(),
        };
        let position = PeerAsk::QueuePosition;
        let cases = [
            // Soulseek.NET SoulseekClient.cs:1640.
            (
                api("The server connection must be Connected and LoggedIn to check download queue position (currently: Disconnected)"),
                position,
                FailureCode::BackendUnreachable,
                RetryAdvice::SameSourceLater,
            ),
            // Waiter.cs:161: the peer did not answer.
            (
                api("The wait timed out after 5000 milliseconds"),
                position,
                FailureCode::TransferConnectionFailed,
                RetryAdvice::SameSourceLater,
            ),
            // SoulseekClient.cs:1643-1646: the transfer ended between two reads.
            (
                api("A download of @@kqzbm\\Music\\03 - O Holy Night.flac from user carol_shares is not active"),
                position,
                FailureCode::TransferConnectionFailed,
                RetryAdvice::SameSourceLater,
            ),
            // SoulseekClient.cs:3790, as the client reads it.
            (
                SoulseekError::UserOffline {
                    username: "carol_shares".into(),
                    status: 500,
                    body: "User carol_shares appears to be offline".into(),
                },
                position,
                FailureCode::PeerOffline,
                RetryAdvice::OtherSource,
            ),
            // SoulseekClient.cs:3866: the Soulseek server did not answer GetStatus in time.
            (
                api("The wait timed out after 5000 milliseconds"),
                PeerAsk::Presence,
                FailureCode::BackendUnreachable,
                RetryAdvice::SameSourceLater,
            ),
            (
                api("The server connection must be connected and logged in to fetch user status (currently: Disconnected)"),
                PeerAsk::Presence,
                FailureCode::BackendUnreachable,
                RetryAdvice::SameSourceLater,
            ),
            (
                SoulseekError::InvalidResponse("expected i64 at line 1 column 1".into()),
                position,
                FailureCode::BackendResponseUnreadable,
                RetryAdvice::NotRetryable,
            ),
        ];
        for (e, ask, code, retry) in cases {
            let f = e.ask_failure(ask, Stage::Transfer);
            assert_eq!((f.code, f.retry), (code, retry), "{e:?}");
            assert_eq!(f.message, ask_message(ask));
            let raw = match &e {
                SoulseekError::Api { message, .. } => message.clone(),
                SoulseekError::UserOffline { body, .. } => body.clone(),
                other => other.to_string(),
            };
            assert_eq!(f.cause.unwrap().raw, raw, "{e:?}");
        }
    }

    fn offline() -> SoulseekError {
        SoulseekError::UserOffline {
            username: "bob".into(),
            status: 404,
            body: "User bob appears to be offline".into(),
        }
    }

    /// slskd's own answer is the cause, not Soulful's text.
    #[test]
    fn an_offline_peer_keeps_slskds_answer_as_its_cause() {
        let f = offline().failure(Stage::Enqueue, offline().to_string());
        assert_eq!(f.message, "User 'bob' is offline");
        let cause = f.cause.unwrap();
        assert_eq!(
            (cause.status, cause.raw.as_str()),
            (Some(404), "User bob appears to be offline")
        );
        assert_eq!(f.user_action, None);
    }

    /// A downloads list or answer that no longer parses waits for a new
    /// Soulful or slskd, not for time.
    #[test]
    fn an_unparseable_answer_asks_for_matching_versions() {
        let drift = SoulseekError::InvalidResponse("JSON parse error: missing field `id`".into());
        let f = drift.failure(Stage::Transfer, "Could not read slskd downloads list");
        assert_eq!(
            (f.code, f.retry),
            (
                FailureCode::TransferListUnreadable,
                RetryAdvice::NotRetryable
            )
        );
        assert_eq!(f.cause.unwrap().raw, drift.to_string());
        assert_eq!(f.user_action, Some(update_soulful_or_slskd()));
    }

    #[test]
    fn slskd_user_actions_carry_their_texts() {
        let action = |code, what: &str, where_: &str| UserAction::new(code, what, where_);
        assert_eq!(
            configure_slskd(),
            action(
                UserActionCode::ConfigureBackend,
                "Set the slskd URL and API key",
                "Settings > Config"
            )
        );
        assert_eq!(
            upgrade_slskd(),
            action(
                UserActionCode::UpgradeBackend,
                "Upgrade slskd to 0.26 or newer",
                "slskd"
            )
        );
        assert_eq!(
            update_soulful_or_slskd(),
            action(
                UserActionCode::UpdateSoulfulOrBackend,
                "Update Soulbeet or slskd so that Soulbeet can read slskd's answers again",
                "Soulbeet and slskd"
            )
        );
    }

    #[test]
    fn missing_settings_read_as_before() {
        for (missing, text) in [
            (MissingSetting::Url, "slskd URL not configured"),
            (MissingSetting::ApiKey, "slskd API key not configured"),
            (MissingSetting::Empty, "slskd not configured"),
        ] {
            assert_eq!(SoulseekError::NotConfigured(missing).to_string(), text);
        }
    }

    #[test]
    fn slskd_too_old_reads_as_before() {
        assert_eq!(
            SoulseekError::SlskdTooOld { body: "nf".into() }.to_string(),
            "API error: 404 - transfers/downloads/batches not found; slskd 0.26 or newer is required (nf)"
        );
        assert!(!SoulseekError::SlskdTooOld {
            body: String::new()
        }
        .is_retryable());
    }
}
