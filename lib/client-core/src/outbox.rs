use std::collections::HashSet;
use std::sync::Arc;

use reqwest::Client;
use serde_json::json;
use sqlx::SqlitePool;
use tokio::sync::RwLock;
use uuid::Uuid;

use crate::db::{get_cursor, now_unix};
use crate::error::{CoreError, CoreResult};
use crate::models::{
    DrainReport, NavidromeCredentials, OutboxMutation, OutboxRow, OutboxSummary, SoulbeetAuth,
};
use crate::subsonic::SubsonicClient;
use crate::sync::CURSOR_PROMOTE_THRESHOLD;

enum DispatchError {
    Retryable {
        err: CoreError,
        retry_after_secs: Option<i64>,
    },
    Permanent(CoreError),
}

/// Atomic, chronological offline mutation outbox with immediate optimistic local SQLite updates
/// and ordered idempotent replay on reconnect.
#[derive(Clone)]
pub struct MutationOutbox {
    pool: SqlitePool,
    http: Client,
    soulbeet_base_url: Option<String>,
    soulbeet_auth: Arc<RwLock<Option<SoulbeetAuth>>>,
    navidrome: Arc<RwLock<Option<NavidromeCredentials>>>,
}

impl MutationOutbox {
    pub fn new(
        pool: SqlitePool,
        http: Client,
        soulbeet_base_url: Option<String>,
        soulbeet_auth: Arc<RwLock<Option<SoulbeetAuth>>>,
        navidrome: Arc<RwLock<Option<NavidromeCredentials>>>,
    ) -> Self {
        Self {
            pool,
            http,
            soulbeet_base_url,
            soulbeet_auth,
            navidrome,
        }
    }

    /// Atomically enqueues a mutation and applies its optimistic local state change in one SQLite transaction.
    pub async fn enqueue(&self, mutation: OutboxMutation) -> CoreResult<i64> {
        self.enqueue_with_key(mutation, Uuid::new_v4().to_string())
            .await
    }

    /// Enqueues a mutation using a caller-supplied `idempotency_key`.
    /// Re-enqueuing an identical `idempotency_key` is a race-free no-op returning the existing `seq`.
    pub async fn enqueue_with_key(
        &self,
        mutation: OutboxMutation,
        idempotency_key: String,
    ) -> CoreResult<i64> {
        if let OutboxMutation::Rate { rating, .. } = &mutation {
            if *rating > 5 {
                return Err(CoreError::InvalidInput(format!(
                    "rating must be between 0 and 5 (got {rating})"
                )));
            }
        }
        let now = now_unix();
        let payload_json = serde_json::to_string(&mutation)?;
        let kind = mutation.kind_name();
        let song_id = mutation.song_id().to_string();
        let promote_threshold: i64 = get_cursor(&self.pool, CURSOR_PROMOTE_THRESHOLD)
            .await?
            .and_then(|v| v.parse::<i64>().ok())
            .map(|v| v.clamp(1, 5))
            .unwrap_or(4);

        let mut tx = self.pool.begin().await?;

        let res = sqlx::query(
            r#"
            INSERT OR IGNORE INTO mutation_outbox (
                idempotency_key, kind, song_id, payload_json, created_at, attempts, next_retry_at, dead_letter
            )
            VALUES (?, ?, ?, ?, ?, 0, 0, 0)
            "#,
        )
        .bind(&idempotency_key)
        .bind(kind)
        .bind(&song_id)
        .bind(&payload_json)
        .bind(now)
        .execute(&mut *tx)
        .await?;

        if res.rows_affected() == 0 {
            let existing_seq: i64 = sqlx::query_scalar(
                "SELECT seq FROM mutation_outbox WHERE idempotency_key = ?",
            )
            .bind(&idempotency_key)
            .fetch_one(&mut *tx)
            .await?;
            tx.commit().await?;
            return Ok(existing_seq);
        }

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
                        user_rating = MAX(user_rating, ?),
                        updated_at = ?
                    WHERE id = ?
                    "#,
                )
                .bind(promote_threshold)
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
                        "UPDATE discovery_tracks SET status = 'removed', updated_at = ? WHERE id = ?",
                    )
                    .bind(now)
                    .bind(disc_id)
                    .execute(&mut *tx)
                    .await?;
                } else {
                    sqlx::query(
                        "UPDATE discovery_tracks SET status = 'removed', updated_at = ? WHERE song_id = ?",
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
            "SELECT * FROM mutation_outbox WHERE dead_letter = 0 ORDER BY seq ASC",
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    pub async fn dead_lettered(&self) -> CoreResult<Vec<OutboxRow>> {
        let rows = sqlx::query_as::<_, OutboxRow>(
            "SELECT * FROM mutation_outbox WHERE dead_letter = 1 ORDER BY seq ASC",
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    pub async fn pending_count(&self) -> CoreResult<usize> {
        let count: i64 = sqlx::query_scalar(
            "SELECT COUNT(1) FROM mutation_outbox WHERE dead_letter = 0",
        )
        .fetch_one(&self.pool)
        .await?;
        Ok(count.max(0) as usize)
    }

    pub async fn summary(&self) -> CoreResult<OutboxSummary> {
        let (pending_count, dead_letter_count, oldest_pending_at): (i64, i64, Option<i64>) =
            sqlx::query_as(
                r#"
                SELECT
                    COALESCE(SUM(CASE WHEN dead_letter = 0 THEN 1 ELSE 0 END), 0),
                    COALESCE(SUM(CASE WHEN dead_letter = 1 THEN 1 ELSE 0 END), 0),
                    MIN(CASE WHEN dead_letter = 0 THEN created_at ELSE NULL END)
                FROM mutation_outbox
                "#,
            )
            .fetch_one(&self.pool)
            .await?;

        let last_error: Option<String> = sqlx::query_scalar(
            "SELECT last_error FROM mutation_outbox WHERE last_error IS NOT NULL ORDER BY seq DESC LIMIT 1",
        )
        .fetch_optional(&self.pool)
        .await?;

        Ok(OutboxSummary {
            pending_count: pending_count.max(0) as usize,
            dead_letter_count: dead_letter_count.max(0) as usize,
            oldest_pending_at,
            last_error,
        })
    }

    pub async fn retry_dead_letters(&self) -> CoreResult<usize> {
        let res = sqlx::query(
            "UPDATE mutation_outbox SET dead_letter = 0, attempts = 0, last_error = NULL, next_retry_at = 0 WHERE dead_letter = 1",
        )
        .execute(&self.pool)
        .await?;
        Ok(res.rows_affected() as usize)
    }

    /// Replays due outbox mutations strictly in chronological order (`seq ASC`).
    /// If a mutation fails or is in retry backoff for a given `song_id`, subsequent mutations for
    /// that same `song_id` are deferred so later edits never overtake earlier ones.
    /// Permanent (non-retryable) failures and corrupt JSON payloads are dead-lettered without
    /// blocking unrelated songs or aborting the drain.
    pub async fn drain(&self) -> CoreResult<DrainReport> {
        let now = now_unix();
        let rows = sqlx::query_as::<_, OutboxRow>(
            "SELECT * FROM mutation_outbox WHERE dead_letter = 0 ORDER BY seq ASC",
        )
        .fetch_all(&self.pool)
        .await?;

        let mut report = DrainReport::default();
        let mut blocked_songs: HashSet<String> = HashSet::new();

        for row in rows {
            if blocked_songs.contains(&row.song_id) {
                report.remaining += 1;
                continue;
            }
            if row.next_retry_at > now {
                blocked_songs.insert(row.song_id.clone());
                report.remaining += 1;
                continue;
            }

            let mutation: OutboxMutation = match serde_json::from_str(&row.payload_json) {
                Ok(m) => m,
                Err(err) => {
                    tracing::warn!(
                        seq = row.seq,
                        song_id = %row.song_id,
                        "dead-lettering corrupt outbox payload: {err}"
                    );
                    sqlx::query(
                        r#"
                        UPDATE mutation_outbox
                        SET dead_letter = 1, attempts = ?, last_error = ?, next_retry_at = 0
                        WHERE seq = ?
                        "#,
                    )
                    .bind(row.attempts + 1)
                    .bind(format!("corrupt payload JSON: {err}"))
                    .bind(row.seq)
                    .execute(&self.pool)
                    .await?;
                    report.failed += 1;
                    continue;
                }
            };

            match self.dispatch(&row.idempotency_key, &mutation).await {
                Ok(()) => {
                    sqlx::query("DELETE FROM mutation_outbox WHERE seq = ?")
                        .bind(row.seq)
                        .execute(&self.pool)
                        .await?;
                    report.replayed += 1;
                }
                Err(DispatchError::Retryable {
                    err,
                    retry_after_secs,
                }) => {
                    let attempts = row.attempts + 1;
                    let exp_backoff = (5i64 * (1i64 << attempts.min(6) as u32)).min(300);
                    let backoff_secs = retry_after_secs
                        .map(|s| s.max(1))
                        .unwrap_or(exp_backoff)
                        .max(exp_backoff);
                    let next_retry_at = now.saturating_add(backoff_secs);
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
                Err(DispatchError::Permanent(err)) => {
                    tracing::warn!(
                        seq = row.seq,
                        song_id = %row.song_id,
                        kind = %row.kind,
                        "dead-lettering permanent outbox mutation failure: {err}"
                    );
                    sqlx::query(
                        r#"
                        UPDATE mutation_outbox
                        SET dead_letter = 1, attempts = ?, last_error = ?, next_retry_at = 0
                        WHERE seq = ?
                        "#,
                    )
                    .bind(row.attempts + 1)
                    .bind(err.to_string())
                    .bind(row.seq)
                    .execute(&self.pool)
                    .await?;
                    report.failed += 1;
                }
            }
        }

        Ok(report)
    }

    async fn dispatch(
        &self,
        idempotency_key: &str,
        mutation: &OutboxMutation,
    ) -> Result<(), DispatchError> {
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
                    .ok_or_else(|| DispatchError::Retryable {
                        err: CoreError::NotConfigured("navidrome credentials"),
                        retry_after_secs: None,
                    })?;
                let subsonic = SubsonicClient::new(self.http.clone(), creds);
                subsonic
                    .scrobble(song_id, *submission, Some(played_at_unix.saturating_mul(1000)))
                    .await
                    .map_err(classify_subsonic_error)
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
                        "req": {
                            "song_id": song_id,
                            "discovery_track_id": discovery_track_id,
                            "target_folder_id": target_folder_id,
                        }
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
                            "req": {
                                "song_id": song_id,
                                "starred": starred,
                                "discovery_track_id": discovery_track_id,
                            }
                        }),
                    )
                    .await
                } else {
                    let creds = self
                        .navidrome
                        .read()
                        .await
                        .clone()
                        .ok_or_else(|| DispatchError::Retryable {
                            err: CoreError::NotConfigured("navidrome credentials"),
                            retry_after_secs: None,
                        })?;
                    SubsonicClient::new(self.http.clone(), creds)
                        .star(song_id, *starred)
                        .await
                        .map_err(classify_subsonic_error)
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
                            "req": {
                                "song_id": song_id,
                                "rating": rating,
                                "discovery_track_id": discovery_track_id,
                            }
                        }),
                    )
                    .await
                } else {
                    let creds = self
                        .navidrome
                        .read()
                        .await
                        .clone()
                        .ok_or_else(|| DispatchError::Retryable {
                            err: CoreError::NotConfigured("navidrome credentials"),
                            retry_after_secs: None,
                        })?;
                    SubsonicClient::new(self.http.clone(), creds)
                        .set_rating(song_id, *rating)
                        .await
                        .map_err(classify_subsonic_error)
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
                        "req": {
                            "song_id": song_id,
                            "discovery_track_id": discovery_track_id,
                            "confirm_delete": confirm_delete,
                        }
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
    ) -> Result<(), DispatchError> {
        let base = self
            .soulbeet_base_url
            .as_deref()
            .ok_or_else(|| DispatchError::Retryable {
                err: CoreError::NotConfigured("soulbeet_base_url"),
                retry_after_secs: None,
            })?;
        let url = format!("{}{}", base.trim_end_matches('/'), path);
        let mut req = self
            .http
            .post(&url)
            .header("X-Idempotency-Key", idempotency_key)
            .json(body);
        if let Some(auth) = self.soulbeet_auth.read().await.as_ref() {
            let (name, val) = auth.header_name_and_value();
            req = req.header(name, val);
        }
        let resp = req.send().await.map_err(|e| DispatchError::Retryable {
            err: CoreError::Http(e),
            retry_after_secs: None,
        })?;
        let status = resp.status();
        if status.is_success() {
            return Ok(());
        }
        let status_code = status.as_u16();
        let text = resp.text().await.unwrap_or_default();
        Err(classify_http_failure(status_code, text))
    }
}

/// Classifies Soulbeet HTTP error responses, including HTTP 409 `Failure` envelopes whose
/// `details.retry_advice` / `details.retry` dictates retryability.
fn classify_http_failure(status: u16, body: String) -> DispatchError {
    if status == 409 {
        if let Ok(val) = serde_json::from_str::<serde_json::Value>(&body) {
            let advice = val
                .pointer("/details/retry_advice")
                .or_else(|| val.pointer("/details/retry"))
                .or_else(|| val.get("retry_advice"))
                .or_else(|| val.get("retry"))
                .and_then(|v| v.as_str())
                .map(|s| s.to_ascii_lowercase());
            let retry_after_secs = val
                .pointer("/details/retry_after_seconds")
                .or_else(|| val.get("retry_after_seconds"))
                .and_then(|v| v.as_i64());

            if matches!(advice.as_deref(), Some("retryable" | "same_source_later")) {
                return DispatchError::Retryable {
                    err: CoreError::ServerHttp { status, body },
                    retry_after_secs,
                };
            }
        }
        return DispatchError::Permanent(CoreError::ServerHttp { status, body });
    }

    if matches!(status, 401 | 403 | 408 | 429) || status >= 500 {
        return DispatchError::Retryable {
            err: CoreError::ServerHttp { status, body },
            retry_after_secs: None,
        };
    }

    DispatchError::Permanent(CoreError::ServerHttp { status, body })
}

fn classify_subsonic_error(err: CoreError) -> DispatchError {
    match &err {
        CoreError::Subsonic { code: 70, .. } => DispatchError::Permanent(err),
        CoreError::ServerHttp { status, body } => classify_http_failure(*status, body.clone()),
        _ => DispatchError::Retryable {
            err,
            retry_after_secs: None,
        },
    }
}
