//! The transfer book in Soulful's database (`backend_transfers`).

use async_trait::async_trait;
use soulbeet::{BookEntry, BookError, TransferBook};
use sqlx::SqlitePool;

pub struct SqliteTransferBook {
    db: SqlitePool,
}

impl SqliteTransferBook {
    pub fn new(db: SqlitePool) -> Self {
        Self { db }
    }
}

type Row = (
    String,
    String,
    Option<String>,
    Option<String>,
    String,
    String,
    String,
    String,
    String,
    i64,
);

const COLUMNS: &str =
    "backend, key, transfer_id, file_id, source, item, request, target, data, created_at";

fn entry(row: Row) -> Result<BookEntry, BookError> {
    let (backend, key, transfer_id, file_id, source, item, request, target, data, created_at) = row;
    let data = serde_json::from_str(&data)
        .map_err(|e| BookError(format!("entry {backend}/{key} holds unreadable data: {e}")))?;
    Ok(BookEntry {
        backend,
        key,
        transfer_id,
        file_id,
        source,
        item,
        request,
        target,
        data,
        created_at,
    })
}

fn failed(e: sqlx::Error) -> BookError {
    BookError(e.to_string())
}

/// `rows` read as entries, the first unreadable one failing them all.
fn entries(rows: Vec<Row>) -> Result<Vec<BookEntry>, BookError> {
    rows.into_iter().map(entry).collect()
}

/// Fails an update that changed no entry: the backend has none under `key`.
fn changed(
    done: sqlx::sqlite::SqliteQueryResult,
    backend: &str,
    key: &str,
) -> Result<(), BookError> {
    match done.rows_affected() {
        0 => Err(BookError(format!("no entry {backend}/{key}"))),
        _ => Ok(()),
    }
}

#[async_trait]
impl TransferBook for SqliteTransferBook {
    async fn put(&self, e: &BookEntry) -> Result<(), BookError> {
        sqlx::query(&format!(
            "INSERT INTO backend_transfers ({COLUMNS}) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?) \
             ON CONFLICT (backend, key) DO UPDATE SET transfer_id = excluded.transfer_id, \
             file_id = excluded.file_id, source = excluded.source, item = excluded.item, \
             request = excluded.request, target = excluded.target, data = excluded.data, \
             created_at = excluded.created_at"
        ))
        .bind(&e.backend)
        .bind(&e.key)
        .bind(&e.transfer_id)
        .bind(&e.file_id)
        .bind(&e.source)
        .bind(&e.item)
        .bind(&e.request)
        .bind(&e.target)
        .bind(e.data.to_string())
        .bind(e.created_at)
        .execute(&self.db)
        .await
        .map(|_| ())
        .map_err(failed)
    }

    async fn bind(
        &self,
        backend: &str,
        key: &str,
        transfer_id: &str,
        file_id: Option<&str>,
    ) -> Result<(), BookError> {
        let done = sqlx::query(
            "UPDATE backend_transfers SET transfer_id = ?, file_id = ? WHERE backend = ? AND key = ?",
        )
        .bind(transfer_id)
        .bind(file_id)
        .bind(backend)
        .bind(key)
        .execute(&self.db)
        .await
        .map_err(failed)?;
        changed(done, backend, key)
    }

    async fn set_data(
        &self,
        backend: &str,
        key: &str,
        data: &serde_json::Value,
    ) -> Result<(), BookError> {
        let done =
            sqlx::query("UPDATE backend_transfers SET data = ? WHERE backend = ? AND key = ?")
                .bind(data.to_string())
                .bind(backend)
                .bind(key)
                .execute(&self.db)
                .await
                .map_err(failed)?;
        changed(done, backend, key)
    }

    async fn by_sources(
        &self,
        backend: &str,
        sources: &[String],
    ) -> Result<Vec<BookEntry>, BookError> {
        let sources = serde_json::to_string(sources).map_err(|e| BookError(e.to_string()))?;
        let rows: Vec<Row> = sqlx::query_as(&format!(
            "SELECT {COLUMNS} FROM backend_transfers WHERE backend = ? \
             AND source IN (SELECT value FROM json_each(?)) ORDER BY created_at, key"
        ))
        .bind(backend)
        .bind(sources)
        .fetch_all(&self.db)
        .await
        .map_err(failed)?;
        entries(rows)
    }

    async fn by_transfer(
        &self,
        backend: &str,
        transfer_id: &str,
    ) -> Result<Option<BookEntry>, BookError> {
        let row: Option<Row> = sqlx::query_as(&format!(
            "SELECT {COLUMNS} FROM backend_transfers WHERE backend = ? AND transfer_id = ?"
        ))
        .bind(backend)
        .bind(transfer_id)
        .fetch_optional(&self.db)
        .await
        .map_err(failed)?;
        row.map(entry).transpose()
    }

    async fn all(&self, backend: &str) -> Result<Vec<BookEntry>, BookError> {
        let rows: Vec<Row> = sqlx::query_as(&format!(
            "SELECT {COLUMNS} FROM backend_transfers WHERE backend = ? ORDER BY created_at, key"
        ))
        .bind(backend)
        .fetch_all(&self.db)
        .await
        .map_err(failed)?;
        entries(rows)
    }

    async fn forget(&self, backend: &str, key: &str) -> Result<(), BookError> {
        sqlx::query("DELETE FROM backend_transfers WHERE backend = ? AND key = ?")
            .bind(backend)
            .bind(key)
            .execute(&self.db)
            .await
            .map(|_| ())
            .map_err(failed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::test_db;
    use serde_json::json;

    fn recorded(backend: &str, key: &str, source: &str, at: i64) -> BookEntry {
        BookEntry {
            backend: backend.into(),
            key: key.into(),
            transfer_id: None,
            file_id: None,
            source: source.into(),
            item: format!("{key}.flac"),
            request: "r1".into(),
            target: "Come Over [12345678]".into(),
            data: json!({}),
            created_at: at,
        }
    }

    async fn book() -> SqliteTransferBook {
        SqliteTransferBook::new(test_db::migrated().await)
    }

    #[tokio::test]
    async fn an_entry_is_put_bound_and_found_by_its_transfer() {
        let book = book().await;
        let first = recorded("archive", "k1", "the_slip", 10);
        book.put(&first).await.unwrap();
        assert_eq!(book.all("archive").await.unwrap(), vec![first.clone()]);

        book.bind("archive", "k1", "t1", Some("f1")).await.unwrap();
        let bound = BookEntry {
            transfer_id: Some("t1".into()),
            file_id: Some("f1".into()),
            ..first.clone()
        };
        assert_eq!(
            book.by_transfer("archive", "t1").await.unwrap(),
            Some(bound.clone())
        );
        assert_eq!(book.by_transfer("airdcpp", "t1").await.unwrap(), None);

        let data = json!({"size": 27213250, "md5": "0f343b0931126a20f133d67c2b018a3b"});
        book.set_data("archive", "k1", &data).await.unwrap();
        assert_eq!(book.all("archive").await.unwrap()[0].data, data);

        // Put again: replaced, not added.
        let replaced = BookEntry {
            target: "Other [87654321]".into(),
            ..bound
        };
        book.put(&replaced).await.unwrap();
        assert_eq!(book.all("archive").await.unwrap(), vec![replaced]);

        assert!(book.bind("archive", "missing", "t9", None).await.is_err());
        assert!(book.set_data("archive", "missing", &data).await.is_err());
    }

    #[tokio::test]
    async fn a_transfer_id_belongs_to_one_entry_of_a_backend() {
        let book = book().await;
        for e in [
            recorded("airdcpp", "k1", "peer_a", 1),
            recorded("airdcpp", "k2", "peer_a", 2),
            recorded("archive", "k1", "the_slip", 3),
        ] {
            book.put(&e).await.unwrap();
        }
        book.bind("airdcpp", "k1", "b1", None).await.unwrap();
        assert!(book.bind("airdcpp", "k2", "b1", None).await.is_err());
        // Another backend's ids are its own.
        book.bind("archive", "k1", "b1", None).await.unwrap();
        let taken = BookEntry {
            transfer_id: Some("b1".into()),
            ..recorded("airdcpp", "k3", "peer_b", 4)
        };
        assert!(book.put(&taken).await.is_err());
    }

    #[tokio::test]
    async fn entries_are_found_by_source_and_forgotten() {
        let book = book().await;
        for e in [
            recorded("airdcpp", "k1", "peer_a", 3),
            recorded("airdcpp", "k2", "peer_b", 1),
            recorded("airdcpp", "k3", "peer_c", 2),
            recorded("archive", "k4", "peer_a", 0),
        ] {
            book.put(&e).await.unwrap();
        }
        let keys = |found: Vec<BookEntry>| found.into_iter().map(|e| e.key).collect::<Vec<_>>();
        let sources = vec!["peer_a".to_string(), "peer_b".to_string()];
        assert_eq!(
            keys(book.by_sources("airdcpp", &sources).await.unwrap()),
            vec!["k2", "k1"]
        );
        assert!(book.by_sources("airdcpp", &[]).await.unwrap().is_empty());

        book.forget("airdcpp", "k1").await.unwrap();
        book.forget("airdcpp", "missing").await.unwrap();
        assert_eq!(keys(book.all("airdcpp").await.unwrap()), vec!["k2", "k3"]);
        assert_eq!(keys(book.all("archive").await.unwrap()), vec!["k4"]);
    }
}
