use thiserror::Error;

/// Subsonic error code Navidrome sends for a refused login, in an HTTP 200
/// body (`responses.ErrorAuthenticationFail`).
pub const WRONG_CREDENTIALS: u32 = 40;

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
    /// in front of it. `body` is the answer's text where it explains the failure.
    #[error("{action} failed: HTTP {status}{}", body_suffix(.body))]
    Http {
        action: &'static str,
        status: u16,
        body: Option<String>,
    },

    /// Navidrome answered with a Subsonic error.
    #[error("Navidrome error {code}: {message}")]
    Subsonic { code: u32, message: String },

    /// The answer could not be read.
    #[error("Unreadable Navidrome response: {0}")]
    Parse(String),

    /// The answer lacked what the request asked for.
    #[error("{0}")]
    Missing(String),
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
}

fn body_suffix(body: &Option<String>) -> String {
    body.as_deref()
        .map(|b| format!(": {b}"))
        .unwrap_or_default()
}

pub type Result<T> = std::result::Result<T, NavidromeError>;
