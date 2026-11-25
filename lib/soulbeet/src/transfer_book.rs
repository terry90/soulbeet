//! What a backend records of its own transfers so a restart finds them again: the backend
//! decides what it keeps and when it forgets it.

use async_trait::async_trait;

/// One transfer a backend recorded, under its own `key`.
#[derive(Debug, Clone, PartialEq)]
pub struct BookEntry {
    pub backend: String,
    pub key: String,
    /// The id the transfer is listed under, once the backend has one.
    pub transfer_id: Option<String>,
    /// The id the backend's service gave the file, when it gave one.
    pub file_id: Option<String>,
    pub source: String,
    /// `DownloadableItem::id` of what was sent.
    pub item: String,
    /// The `download` call's request.
    pub request: String,
    /// The batch folder the file lands in.
    pub target: String,
    /// Whatever else the backend keeps for the transfer.
    pub data: serde_json::Value,
    /// Unix seconds.
    pub created_at: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("transfer book: {0}")]
pub struct BookError(pub String);

#[async_trait]
pub trait TransferBook: Send + Sync {
    /// Inserts `entry`, or replaces the one with its backend and key.
    async fn put(&self, entry: &BookEntry) -> Result<(), BookError>;
    /// Sets the transfer id and file id of an entry. A transfer id another entry of the backend
    /// holds is refused.
    async fn bind(
        &self,
        backend: &str,
        key: &str,
        transfer_id: &str,
        file_id: Option<&str>,
    ) -> Result<(), BookError>;
    async fn set_data(
        &self,
        backend: &str,
        key: &str,
        data: &serde_json::Value,
    ) -> Result<(), BookError>;
    /// The backend's entries from any of `sources`.
    async fn by_sources(
        &self,
        backend: &str,
        sources: &[String],
    ) -> Result<Vec<BookEntry>, BookError>;
    async fn by_transfer(
        &self,
        backend: &str,
        transfer_id: &str,
    ) -> Result<Option<BookEntry>, BookError>;
    async fn all(&self, backend: &str) -> Result<Vec<BookEntry>, BookError>;
    async fn forget(&self, backend: &str, key: &str) -> Result<(), BookError>;
}
