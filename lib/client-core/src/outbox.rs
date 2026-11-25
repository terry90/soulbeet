use std::collections::HashSet;
use std::sync::Arc;

use reqwest::Client;
use serde_json::json;
use sqlx::SqlitePool;
use tokio::sync::RwLock;
use uuid::Uuid;

use crate::db::now_unix;
use crate::error::{CoreError, CoreResult};
use crate::models::{DrainReport, NavidromeCredentials, OutboxMutation, OutboxRow};
use crate::subsonic::SubsonicClient;

/// Atomic, chronological offline mutation outbox with immediate optimistic local SQLite updates
/// and ordered idempotent replay on reconnect.
#[derive(Clone)]
pub struct MutationOutbox {
    pool: SqlitePool,
    http: Client,
    soulbeet_base_url: Option<String>,
    soulbeet_auth_header: Option<String>,
    navidrome: Arc<RwLock<Option<NavidromeCredentials>>>,
}

impl MutationOutbox {
    pub fn new(
        pool: SqlitePool,
        http: Client,
        soulbeet_base_url: Option<String>,
        soulbeet_auth_header: Option<String>,
        navidrome: Arc<RwLock<Option<NavidromeCredentials>>>,
    ) -> Self {
        Self {
            pool,
            http,
            soulbeet_base_url,
            soulbeet_auth_header,
            navidrome,
        }
    }

    /// Atomically enqueues a mutation and applies its optimistic local state change in one SQLite transaction.
    pub async fn enqueue(&self, mutation: OutboxMutation) -> CoreResult<i64> {
        self.enqueue_with_key(mutation, Uuid::new_v4().to_string())
            .await
    }

    /// Enqueues a mutation using a caller-supplied `idempotency_key`.
    /// Re-enqueuing an identical `idempotency_key` is a safe no-op returning the existing `seq`.
    pub async fn enqueue_with_key(
        &self,
        mutation: OutboxMutation,
        idempotency_key: String,
    ) -> CoreResult<i64> {
        let now = now_unix();
        let payload_json = serde_json::to_string(&mutation)?;
        let kind = mutation.kind_name();
        let song_id = mutation.song_id().to_string();

        let mut tx = self.pool.begin().await?;

        if let Some(existing_seq) = sqlx::query_scalar::<_, i64>(
            "SELECT seq FROM mutation_outbox WHERE idempotency_key = ?",
        )
        .bind(&idempotency_key)
        .fetch_optional(&mut *tx)
        .await?
        {
            tx.commit().await?;
            return Ok(existing_seq);
        }

        let res = sqlx::query(
            r#"
            INSERT INTO mutation_outbox (idempotency_key, kind, song_id, payload_json, created_at, attempts, next_retry_at)
            VALUES (?, ?, ?, ?, ?, 0, 0)
            "#,
        )
        .bind(&idempotency_key)
        .bind(kind)
        .bind(&song_id)
        .bind(&payload_json)
        .bind(now)
        .execute(&mut *tx)
        .await?;
        let seq = res.last_insert_rowid();

        // Apply immediate optimistic local update inside the same transaction.
        match &mutation {
            OutboxMutation::Scrobble {
                song_id,
                submission,
                played_at_unix,
            } => {
                if *submission {
                    sqlx::query(
                        r#"
                        UPDATE tracks
                        SET play_count = play_count + 1,
                            last_played_at = MAX(COALESCE(last_played_at, 0), ?),
                            updated_at = ?
                        WHERE id = ?
                        "#,
                    )
                    .bind(*played_at_unix)
                    .bind(now)
                    .bind(song_id)
                    .execute(&mut *tx)
                    .await?;
                }
            }
            OutboxMutation::Keep {
                song_id,
                discovery_track_id,
                ..
            } => {
                sqlx::query(
                    r#"
                    UPDATE tracks
                    SET is_discovery = 0,
                        user_rating = MAX(user_rating, 4),
                        updated_at = ?
                    WHERE id = ?
                    "#,
                )
                .bind(now)
                .bind(song_id)
                .execute(&mut *tx)
                .await?;

                if let Some(disc_id) = discovery_track_id {
                    sqlx::query(
                        "UPDATE discovery_tracks SET status = 'promoted', updated_at = ? WHERE id = ?",
                    )
                    .bind(now)
                    .bind(disc_id)
                    .execute(&mut *tx)
                    .await?;
                } else {
                    sqlx::query(
                        "UPDATE discovery_tracks SET status = 'promoted', updated_at = ? WHERE song_id = ?",
                    )
                    .bind(now)
                    .bind(song_id)
                    .execute(&mut *tx)
                    .await?;
                }
            }
            OutboxMutation::Star {
                song_id,
                starred,
                discovery_track_id,
            } => {
                sqlx::query("UPDATE tracks SET starred = ?, updated_at = ? WHERE id = ?")
                    .bind(*starred)
                    .bind(now)
                    .bind(song_id)
                    .execute(&mut *tx)
                    .await?;

                if *starred {
                    sqlx::query("UPDATE tracks SET is_discovery = 0, updated_at = ? WHERE id = ?")
                        .bind(now)
                        .bind(song_id)
                        .execute(&mut *tx)
                        .await?;
                    if let Some(disc_id) = discovery_track_id {
                        sqlx::query(
                            "UPDATE discovery_tracks SET status = 'promoted', updated_at = ? WHERE id = ?",
                        )
                        .bind(now)
                        .bind(disc_id)
                        .execute(&mut *tx)
                        .await?;
                    } else {
                        sqlx::query(
                            "UPDATE discovery_tracks SET status = 'promoted', updated_at = ? WHERE song_id = ?",
                        )
                        .bind(now)
                        .bind(song_id)
                        .execute(&mut *tx)
                        .await?;
                    }
                }
            }
            OutboxMutation::Rate {
                song_id, rating, ..
            } => {
                let clamped = i64::from((*rating).min(5));
                sqlx::query("UPDATE tracks SET user_rating = ?, updated_at = ? WHERE id = ?")
                    .bind(clamped)
                    .bind(now)
                    .bind(song_id)
                    .execute(&mut *tx)
                    .await?;
            }
            OutboxMutation::Drop {
                song_id,
                discovery_track_id,
                ..
            } => {
                sqlx::query("UPDATE tracks SET is_discovery = 0, updated_at = ? WHERE id = ?")
                    .bind(now)
                    .bind(song_id)
                    .execute(&mut *tx)
                    .await?;
                if let Some(disc_id) = discovery_track_id {
                    sqlx::query(
                        "UPDATE discovery_tracks SET status = 'rejected', updated_at = ? WHERE id = ?",
                    )
                    .bind(now)
                    .bind(disc_id)
                    .execute(&mut *tx)
                    .await?;
                } else {
                    sqlx::query(
                        "UPDATE discovery_tracks SET status = 'rejected', updated_at = ? WHERE song_id = ?",
                    )
                    .bind(now)
                    .bind(song_id)
                    .execute(&mut *tx)
                    .await?;
                }
            }
        }

        tx.commit().await?;
        Ok(seq)
    }

    pub async fn pending(&self) -> CoreResult<Vec<OutboxRow>> {
        let rows = sqlx::query_as::<_, OutboxRow>(
            "SELECT * FROM mutation_outbox ORDER BY seq ASC",
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    pub async fn pending_count(&self) -> CoreResult<usize> {
        let count: i64 = sqlx::query_scalar("SELECT COUNT(1) FROM mutation_outbox")
            .fetch_one(&self.pool)
            .await?;
        Ok(count.max(0) as usize)
    }

    /// Replays due outbox mutations strictly in chronological order (`seq ASC`).
    /// If a mutation fails for a given `song_id`, subsequent mutations for that same `song_id`
    /// are deferred so later edits never overtake earlier ones.
    pub async fn drain(&self) -> CoreResult<DrainReport> {
        let now = now_unix();
        let rows = sqlx::query_as::<_, OutboxRow>(
            "SELECT * FROM mutation_outbox ORDER BY seq ASC",
        )
        .fetch_all(&self.pool)
        .await?;

        let mut report = DrainReport::default();
        let mut blocked_songs: HashSet<String> = HashSet::new();

        for row in rows {
            if blocked_songs.contains(&row.song_id) || row.next_retry_at > now {
                report.remaining += 1;
                continue;
            }

            let mutation: OutboxMutation = serde_json::from_str(&row.payload_json)?;
            match self.dispatch(&row.idempotency_key, &mutation).await {
                Ok(()) => {
                    sqlx::query("DELETE FROM mutation_outbox WHERE seq = ?")
                        .bind(row.seq)
                        .execute(&self.pool)
                        .await?;
                    report.replayed += 1;
                }
                Err(err) => {
                    let attempts = row.attempts + 1;
                    let backoff_secs = (5i64 * (1i64 << attempts.min(6) as u32)).min(300);
                    let next_retry_at = now + backoff_secs;
                    sqlx::query(
                        r#"
                        UPDATE mutation_outbox
                        SET attempts = ?, last_error = ?, next_retry_at = ?
                        WHERE seq = ?
                        "#,
                    )
                    .bind(attempts)
                    .bind(err.to_string())
                    .bind(next_retry_at)
                    .bind(row.seq)
                    .execute(&self.pool)
                    .await?;
                    blocked_songs.insert(row.song_id.clone());
                    report.failed += 1;
                    report.remaining += 1;
                }
            }
        }

        Ok(report)
    }

    async fn dispatch(&self, idempotency_key: &str, mutation: &OutboxMutation) -> CoreResult<()> {
        match mutation {
            OutboxMutation::Scrobble {
                song_id,
                submission,
                played_at_unix,
            } => {
                let creds = self
                    .navidrome
                    .read()
                    .await
                    .clone()
                    .ok_or(CoreError::NotConfigured("navidrome credentials"))?;
                let subsonic = SubsonicClient::new(self.http.clone(), creds);
                subsonic
                    .scrobble(song_id, *submission, Some(played_at_unix * 1000))
                    .await
            }
            OutboxMutation::Keep {
                song_id,
                discovery_track_id,
                target_folder_id,
            } => {
                self.post_soulbeet(
                    "/api/curation/keep",
                    idempotency_key,
                    &json!({
                        "song_id": song_id,
                        "discovery_track_id": discovery_track_id,
                        "target_folder_id": target_folder_id,
                    }),
                )
                .await
            }
            OutboxMutation::Star {
                song_id,
                starred,
                discovery_track_id,
            } => {
                if self.soulbeet_base_url.is_some() {
                    self.post_soulbeet(
                        "/api/curation/star",
                        idempotency_key,
                        &json!({
                            "song_id": song_id,
                            "starred": starred,
                            "discovery_track_id": discovery_track_id,
                        }),
                    )
                    .await
                } else {
                    let creds = self
                        .navidrome
                        .read()
                        .await
                        .clone()
                        .ok_or(CoreError::NotConfigured("navidrome credentials"))?;
                    SubsonicClient::new(self.http.clone(), creds)
                        .star(song_id, *starred)
                        .await
                }
            }
            OutboxMutation::Rate {
                song_id,
                rating,
                discovery_track_id,
            } => {
                if self.soulbeet_base_url.is_some() {
                    self.post_soulbeet(
                        "/api/curation/rate",
                        idempotency_key,
                        &json!({
                            "song_id": song_id,
                            "rating": rating,
                            "discovery_track_id": discovery_track_id,
                        }),
                    )
                    .await
                } else {
                    let creds = self
                        .navidrome
                        .read()
                        .await
                        .clone()
                        .ok_or(CoreError::NotConfigured("navidrome credentials"))?;
                    SubsonicClient::new(self.http.clone(), creds)
                        .set_rating(song_id, *rating)
                        .await
                }
            }
            OutboxMutation::Drop {
                song_id,
                discovery_track_id,
                confirm_delete,
            } => {
                self.post_soulbeet(
                    "/api/curation/drop",
                    idempotency_key,
                    &json!({
                        "song_id": song_id,
                        "discovery_track_id": discovery_track_id,
                        "confirm_delete": confirm_delete,
                    }),
                )
                .await
            }
        }
    }

    async fn post_soulbeet(
        &self,
        path: &str,
        idempotency_key: &str,
        body: &serde_json::Value,
    ) -> CoreResult<()> {
        let base = self
            .soulbeet_base_url
            .as_deref()
            .ok_or(CoreError::NotConfigured("soulbeet_base_url"))?;
        let url = format!("{}{}", base.trim_end_matches('/'), path);
        let mut req = self
            .http
            .post(&url)
            .header("X-Idempotency-Key", idempotency_key)
            .json(body);
        if let Some(auth) = &self.soulbeet_auth_header {
            req = req.header(reqwest::header::AUTHORIZATION, auth);
        }
        let resp = req.send().await?;
        let status = resp.status();
        if status.is_success() || status.as_u16() == 409 {
            return Ok(());
        }
        let text = resp.text().await.unwrap_or_default();
        Err(CoreError::ServerHttp {
            status: status.as_u16(),
            body: text,
        })
    }
}
