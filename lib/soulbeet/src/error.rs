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

    #[error("User '{username}' is offline")]
    UserOffline { username: String },

    #[error("Failed to acquire lock for rate limiting")]
    LockError,

    #[error("Search timed out")]
    SearchTimeout,

    #[error("Could not find a username for the given download ID")]
    UsernameNotFound,
}

impl SoulseekError {
    /// Whether this error is worth retrying. Returns false for definitive
    /// failures like user offline where retrying would be wasteful.
    pub fn is_retryable(&self) -> bool {
        match self {
            SoulseekError::UserOffline { .. } => false,
            SoulseekError::NotConfigured => false,
            SoulseekError::Api { status, .. } if *status == 401 || *status == 403 => false,
            _ => true,
        }
    }
}

pub type Result<T> = std::result::Result<T, SoulseekError>;
