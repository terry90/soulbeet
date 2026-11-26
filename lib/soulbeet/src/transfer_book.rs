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

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Mutex;

    /// A transfer book in memory, as the SQLite one behaves. While `.1` is set it refuses
    /// every write but `put`, as a database that cannot be written.
    #[derive(Default)]
    pub(crate) struct MemoryBook(pub Mutex<Vec<BookEntry>>, pub AtomicBool);

    impl MemoryBook {
        fn writable(&self) -> Result<(), BookError> {
            match self.1.load(Ordering::SeqCst) {
                true => Err(BookError("the database is locked".into())),
                false => Ok(()),
            }
        }
    }

    #[async_trait]
    impl TransferBook for MemoryBook {
        async fn put(&self, entry: &BookEntry) -> Result<(), BookError> {
            let mut entries = self.0.lock().unwrap();
            entries.retain(|e| !(e.backend == entry.backend && e.key == entry.key));
            entries.push(entry.clone());
            Ok(())
        }
        async fn bind(
            &self,
            backend: &str,
            key: &str,
            transfer_id: &str,
            file_id: Option<&str>,
        ) -> Result<(), BookError> {
            self.writable()?;
            let mut entries = self.0.lock().unwrap();
            // As the unique index on (backend, transfer_id) refuses it.
            if (entries.iter()).any(|e| {
                e.backend == backend
                    && e.key != key
                    && e.transfer_id.as_deref() == Some(transfer_id)
            }) {
                return Err(BookError(format!(
                    "transfer {backend}/{transfer_id} is bound to another entry"
                )));
            }
            let entry = (entries.iter_mut())
                .find(|e| e.backend == backend && e.key == key)
                .ok_or_else(|| BookError(format!("no entry {backend}/{key}")))?;
            entry.transfer_id = Some(transfer_id.to_string());
            entry.file_id = file_id.map(str::to_string);
            Ok(())
        }
        async fn set_data(
            &self,
            backend: &str,
            key: &str,
            data: &serde_json::Value,
        ) -> Result<(), BookError> {
            self.writable()?;
            let mut entries = self.0.lock().unwrap();
            let entry = (entries.iter_mut())
                .find(|e| e.backend == backend && e.key == key)
                .ok_or_else(|| BookError(format!("no entry {backend}/{key}")))?;
            entry.data = data.clone();
            Ok(())
        }
        async fn by_sources(
            &self,
            backend: &str,
            sources: &[String],
        ) -> Result<Vec<BookEntry>, BookError> {
            Ok(self
                .all(backend)
                .await?
                .into_iter()
                .filter(|e| sources.contains(&e.source))
                .collect())
        }
        async fn by_transfer(
            &self,
            backend: &str,
            transfer_id: &str,
        ) -> Result<Option<BookEntry>, BookError> {
            Ok(self
                .all(backend)
                .await?
                .into_iter()
                .find(|e| e.transfer_id.as_deref() == Some(transfer_id)))
        }
        async fn all(&self, backend: &str) -> Result<Vec<BookEntry>, BookError> {
            let mut entries: Vec<BookEntry> = (self.0.lock().unwrap().iter())
                .filter(|e| e.backend == backend)
                .cloned()
                .collect();
            entries.sort_by(|a, b| (a.created_at, &a.key).cmp(&(b.created_at, &b.key)));
            Ok(entries)
        }
        async fn forget(&self, backend: &str, key: &str) -> Result<(), BookError> {
            self.writable()?;
            (self.0.lock().unwrap()).retain(|e| !(e.backend == backend && e.key == key));
            Ok(())
        }
    }

    impl MemoryBook {
        pub(crate) fn states(&self) -> Vec<(String, String)> {
            (self.0.lock().unwrap().iter())
                .map(|e| {
                    (
                        e.key.clone(),
                        e.data["state"].as_str().unwrap_or("").to_string(),
                    )
                })
                .collect()
        }
    }
}
