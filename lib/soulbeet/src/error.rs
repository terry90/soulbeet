use shared::failure::{Failure, FailureCode, RetryAdvice, Service, Stage, UserAction};
use thiserror::Error;

#[derive(Error, Debug)]
pub enum SoulseekError {
    #[error("Client is not configured. Base URL is missing.")]
    NotConfigured,

    #[error("Request error: {0}")]
    Request(#[from] reqwest::Error),

    #[error("URL parsing error: {0}")]
    UrlParse(#[from] url::ParseError),

    #[error("API error: {status} - {message}")]
    Api { status: u16, message: String },

    /// The batch enqueue route answered 404: it only exists since slskd 0.26.
    #[error(
        "API error: 404 - transfers/downloads/batches not found; slskd 0.26 or newer is required ({body})"
    )]
    SlskdTooOld { body: String },

    #[error("User '{username}' is offline")]
    UserOffline { username: String },

    #[error("Unparseable slskd response: {0}")]
    InvalidResponse(String),

    #[error("Peer name '{username}' cannot be sent in a URL path")]
    UnaddressablePeer { username: String },
}

impl SoulseekError {
    /// Whether this error is worth retrying. Returns false for definitive
    /// failures like user offline where retrying would be wasteful.
    pub fn is_retryable(&self) -> bool {
        match self {
            SoulseekError::UserOffline { .. } => false,
            SoulseekError::NotConfigured => false,
            SoulseekError::UnaddressablePeer { .. } => false,
            SoulseekError::SlskdTooOld { .. } => false,
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
    /// is slskd's status and body for an `Api` error, else the Display.
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
            SoulseekError::NotConfigured
            | SoulseekError::Api {
                status: 401 | 403, ..
            } => (BackendNotConfigured, NeedsUser),
            SoulseekError::Api { status: 429, .. } if stage == Stage::SourceSearch => {
                (SearchRateLimited, SameSourceLater)
            }
            SoulseekError::Api { status: 429, .. } => (EnqueueFailed, SameSourceLater),
            SoulseekError::Api {
                status: 408 | 503, ..
            }
            | SoulseekError::Request(_) => (BackendUnreachable, SameSourceLater),
            SoulseekError::UnaddressablePeer { .. } => (EnqueueFailed, OtherSource),
            // A request slskd refused as malformed: the same files fail
            // again. At search it is the query that slskd cannot use.
            SoulseekError::Api { status: 400, .. } if stage == Stage::Enqueue => {
                (EnqueueFailed, NotRetryable)
            }
            SoulseekError::Api { status: 400, .. } if stage == Stage::SourceSearch => {
                (BackendUnreachable, OtherQuery)
            }
            // slskd could not reach the peer, or is not logged in to Soulseek.
            SoulseekError::Api { status: 500, .. } if stage == Stage::Enqueue => {
                (EnqueueFailed, OtherSource)
            }
            SoulseekError::Api { .. }
            | SoulseekError::InvalidResponse(_)
            | SoulseekError::UrlParse(_) => (by_stage, SameSourceLater),
        };
        let (status, raw) = match self {
            SoulseekError::Api { status, message } => (Some(i32::from(*status)), message.clone()),
            SoulseekError::SlskdTooOld { body } => (Some(404), body.clone()),
            other => (None, other.to_string()),
        };
        let failure =
            Failure::new(code, stage, retry, message).with_cause(Service::Slskd, status, raw);
        match code {
            BackendNotConfigured => failure.with_user_action(UserAction::configure_backend()),
            FailureCode::SlskdTooOld => failure.with_user_action(UserAction::upgrade_backend()),
            _ => failure,
        }
    }
}

pub type Result<T> = std::result::Result<T, SoulseekError>;

#[cfg(test)]
mod tests {
    use super::*;
    use shared::failure::UserActionCode;

    #[test]
    fn soulseek_errors_map_to_failure_codes() {
        let api = |status| SoulseekError::Api {
            status,
            message: "body".into(),
        };
        let cases = [
            (
                SoulseekError::UserOffline {
                    username: "p".into(),
                },
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
                RetryAdvice::SameSourceLater,
            ),
            (
                SoulseekError::InvalidResponse("x".into()),
                Stage::Enqueue,
                FailureCode::EnqueueFailed,
                RetryAdvice::SameSourceLater,
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
                    400,
                    "Search query must contain at least one non-exclusion term (Parameter 'query')",
                ),
                Stage::SourceSearch,
                FailureCode::BackendUnreachable,
                RetryAdvice::OtherQuery,
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
                SoulseekError::NotConfigured,
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
                FailureCode::BackendUnreachable,
                RetryAdvice::SameSourceLater,
            ),
        ];
        for (e, stage, code, retry) in cases {
            let f = e.failure(stage, e.to_string());
            assert_eq!((f.code, f.retry), (code, retry), "{e:?} at {stage:?}");
            let raw = match &e {
                SoulseekError::Api { message, .. } => message.clone(),
                other => other.to_string(),
            };
            assert_eq!(f.cause.unwrap().raw, raw, "{e:?}");
        }
        let unconfigured = SoulseekError::NotConfigured.failure(Stage::Enqueue, "m");
        assert_eq!(
            unconfigured.user_action.unwrap().code,
            UserActionCode::ConfigureBackend
        );
        let offline = SoulseekError::UserOffline {
            username: "p".into(),
        }
        .failure(Stage::Enqueue, "m");
        assert_eq!(offline.cause.unwrap().status, None);
        assert_eq!(offline.user_action, None);
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
