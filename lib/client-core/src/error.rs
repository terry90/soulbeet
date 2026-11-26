use thiserror::Error;

pub type CoreResult<T> = Result<T, CoreError>;

#[derive(Debug, Error)]
pub enum CoreError {
    #[error("database error: {0}")]
    Db(#[from] sqlx::Error),

    #[error("migration error: {0}")]
    Migrate(#[from] sqlx::migrate::MigrateError),

    #[error("http error: {0}")]
    Http(#[from] reqwest::Error),

    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("url parse error: {0}")]
    Url(#[from] url::ParseError),

    #[error("websocket error: {0}")]
    WebSocket(String),

    #[error("not configured: {0}")]
    NotConfigured(&'static str),

    #[error("offline mode active and resource not cached: {0}")]
    Offline(String),

    #[error("subsonic error {code}: {message}")]
    Subsonic { code: i64, message: String },

    #[error("server returned HTTP {status}: {body}")]
    ServerHttp { status: u16, body: String },

    #[error("invalid input: {0}")]
    InvalidInput(String),
}
