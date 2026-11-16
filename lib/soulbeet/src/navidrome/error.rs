use shared::failure::{
    Failure, FailureCode, RetryAdvice, Service, Stage, UserAction, UserActionCode,
};
use thiserror::Error;

/// Subsonic error code Navidrome sends for a refused login, in an HTTP 200
/// body (`responses.ErrorAuthenticationFail`).
pub const WRONG_CREDENTIALS: u32 = 40;

/// Subsonic error code Navidrome sends when the account may not do what was
/// asked (`responses.ErrorAuthorizationFail`), as `startScan` answers a
/// non-admin account (server/subsonic/library_scanning.go:43-44).
pub const NOT_AUTHORIZED: u32 = 50;

/// Subsonic error code for an id Navidrome has no record of for this user
/// (`responses.ErrorDataNotFound`, server/subsonic/responses/errors.go).
pub const DATA_NOT_FOUND: u32 = 70;

/// What went wrong talking to Navidrome. None of these carries the request
/// URL: its query holds the Subsonic token and salt, a working credential.
#[derive(Error, Debug)]
pub enum NavidromeError {
    /// The client cannot be built from what it was given.
    #[error("Navidrome client is not configured: {0}")]
    Config(String),

    #[error("Navidrome URL error: {0}")]
    Url(#[from] url::ParseError),

    /// Recent requests failed, so this one was not sent.
    #[error("Navidrome appears unavailable (circuit breaker open)")]
    CircuitOpen,

    /// No HTTP answer: connection refused, name lookup, timeout, TLS.
    #[error("Navidrome request failed: {0}")]
    Transport(String),

    /// An HTTP answer that is not a success, from Navidrome or from a proxy
    /// in front of it. `body` is the answer's text where it explains the failure,
    /// shown in the message. `raw` is the text of a native API answer, kept for
    /// the failure's cause only.
    #[error("{action} failed: HTTP {status}{}", body_suffix(.body))]
    Http {
        action: &'static str,
        status: u16,
        body: Option<String>,
        raw: Option<String>,
    },

    /// Navidrome answered with a Subsonic error.
    #[error("Navidrome error {code}: {message}")]
    Subsonic { code: u32, message: String },

    /// Navidrome's native login refused the username or password: its
    /// `/auth/login` answers 401 for both (server/auth.go:57).
    #[error("Navidrome native login failed: HTTP 401")]
    NativeLoginRefused,

    /// The answer could not be read.
    #[error("Unreadable Navidrome response: {0}")]
    Parse(String),
}

impl NavidromeError {
    /// Navidrome refused the username or password. Its Subsonic `authenticate`
    /// middleware (server/subsonic/middlewares.go) sends code 40 for an
    /// unknown user, a wrong password, token or JWT, and when its own user
    /// lookup fails. Nothing else in an answer says the credentials are wrong.
    pub fn is_wrong_credentials(&self) -> bool {
        matches!(
            self,
            NavidromeError::Subsonic {
                code: WRONG_CREDENTIALS,
                ..
            }
        )
    }

    /// Navidrome has no record of the id for this user: a Subsonic error 70 and nothing else.
    pub fn is_not_found(&self) -> bool {
        matches!(
            self,
            NavidromeError::Subsonic {
                code: DATA_NOT_FOUND,
                ..
            }
        )
    }

    /// This error as the failure of `stage`, shown as `message`.
    pub fn failure(&self, stage: Stage, message: impl Into<String>) -> Failure {
        use FailureCode::*;
        use RetryAdvice::*;
        let (code, retry) = match self {
            NavidromeError::CircuitOpen => (NavidromeCircuitOpen, SameSourceLater),
            // Subsonic errors come with HTTP 200 (server/subsonic/api.go:298-305),
            // so another status is a proxy or a wrong URL, and an answer that
            // does not parse is not Navidrome's API.
            NavidromeError::Transport(_)
            | NavidromeError::Http { .. }
            | NavidromeError::Parse(_) => (NavidromeUnreachable, SameSourceLater),
            NavidromeError::Subsonic {
                code: WRONG_CREDENTIALS,
                ..
            }
            | NavidromeError::NativeLoginRefused => (NavidromeAuthFailed, NeedsUser),
            NavidromeError::Subsonic { .. } => (NavidromeRefused, NotRetryable),
            NavidromeError::Config(_) | NavidromeError::Url(_) => (NavidromeNotLinked, NeedsUser),
        };
        let failure = Failure::new(code, stage, retry, message);
        let failure = match self {
            NavidromeError::Http {
                status, body, raw, ..
            } => failure.with_cause(
                Service::Navidrome,
                Some(i32::from(*status)),
                raw.as_ref()
                    .or(body.as_ref())
                    .cloned()
                    .unwrap_or_else(|| self.to_string()),
            ),
            NavidromeError::Subsonic { code, message } => {
                failure.with_cause(Service::Navidrome, Some(*code as i32), message.clone())
            }
            NavidromeError::NativeLoginRefused => {
                failure.with_cause(Service::Navidrome, Some(401), self.to_string())
            }
            _ => failure.with_cause(Service::Navidrome, None, self.to_string()),
        };
        match self {
            NavidromeError::Subsonic {
                code: WRONG_CREDENTIALS,
                ..
            }
            | NavidromeError::NativeLoginRefused => failure.with_user_action(relink_navidrome()),
            NavidromeError::Config(_) | NavidromeError::Url(_) => {
                failure.with_user_action(configure_navidrome())
            }
            _ => failure,
        }
    }
}

/// The user's Navidrome password is refused, missing or unreadable: signing
/// in to Soulful again stores the current one.
pub fn relink_navidrome() -> UserAction {
    UserAction::new(
        UserActionCode::RelinkNavidrome,
        "Sign in to Soulful again with your current Navidrome password",
        "Soulful sign-in",
    )
}

pub fn configure_navidrome() -> UserAction {
    UserAction::new(
        UserActionCode::ConfigureNavidrome,
        "Set NAVIDROME_URL to the address of your Navidrome server",
        "Soulful's environment",
    )
}

fn body_suffix(body: &Option<String>) -> String {
    body.as_deref()
        .map(|b| format!(": {b}"))
        .unwrap_or_default()
}

pub type Result<T> = std::result::Result<T, NavidromeError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn navidrome_errors_map_to_failure_codes() {
        use FailureCode::*;
        use RetryAdvice::*;
        let cases = [
            (
                NavidromeError::CircuitOpen,
                NavidromeCircuitOpen,
                SameSourceLater,
                None,
            ),
            (
                NavidromeError::Transport("error sending request".into()),
                NavidromeUnreachable,
                SameSourceLater,
                None,
            ),
            (
                NavidromeError::Http {
                    action: "Navidrome request",
                    status: 502,
                    body: None,
                    raw: None,
                },
                NavidromeUnreachable,
                SameSourceLater,
                Some(502),
            ),
            (
                NavidromeError::Parse("expected value at line 1 column 1".into()),
                NavidromeUnreachable,
                SameSourceLater,
                None,
            ),
            (
                NavidromeError::Subsonic {
                    code: 40,
                    message: "Wrong username or password".into(),
                },
                NavidromeAuthFailed,
                NeedsUser,
                Some(40),
            ),
            (
                NavidromeError::NativeLoginRefused,
                NavidromeAuthFailed,
                NeedsUser,
                Some(401),
            ),
            (
                NavidromeError::Subsonic {
                    code: 50,
                    message: "User is not authorized for the given operation".into(),
                },
                NavidromeRefused,
                NotRetryable,
                Some(50),
            ),
            (
                NavidromeError::Config("base URL missing".into()),
                NavidromeNotLinked,
                NeedsUser,
                None,
            ),
        ];
        for (e, code, retry, status) in cases {
            let f = e.failure(Stage::LibraryCheck, e.to_string());
            assert_eq!(
                (f.code, f.retry, f.stage),
                (code, retry, Stage::LibraryCheck),
                "{e:?}"
            );
            let cause = f.cause.unwrap();
            assert_eq!(
                (cause.service, cause.status),
                (Service::Navidrome, status),
                "{e:?}"
            );
        }
        let refused = NavidromeError::Subsonic {
            code: 40,
            message: "Wrong username or password".into(),
        }
        .failure(Stage::LibraryCheck, "m");
        assert_eq!(refused.cause.unwrap().raw, "Wrong username or password");
        assert_eq!(refused.user_action.unwrap(), relink_navidrome());
        assert_eq!(
            NavidromeError::NativeLoginRefused.to_string(),
            "Navidrome native login failed: HTTP 401"
        );
    }

    #[test]
    fn only_a_subsonic_70_is_not_found() {
        assert!(NavidromeError::Subsonic {
            code: 70,
            message: "data not found".into()
        }
        .is_not_found());
        for code in [0, 40, 50] {
            let e = NavidromeError::Subsonic {
                code,
                message: "x".into(),
            };
            assert!(!e.is_not_found(), "{code}");
        }
        assert!(!NavidromeError::CircuitOpen.is_not_found());
        assert!(!NavidromeError::Http {
            action: "Get playlist",
            status: 404,
            body: None,
            raw: None
        }
        .is_not_found());
    }
}
