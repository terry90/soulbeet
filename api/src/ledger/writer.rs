//! The writer task: the one place the ledger is written, applying each write
//! in the order it was sent.

use super::{
    ClaimedJob, JobState, LedgerError, NewJob, NewSearch, Pruned, JOB_KEPT_SECS, SEARCH_KEPT_SECS,
    TERMINAL,
};
use crate::auth::UserId;
use shared::download::DownloadableGroup;
use soulbeet::ImportMode;
use sqlx::{Sqlite, SqlitePool};
use tokio::sync::{mpsc, oneshot};
use tracing::warn;

pub(super) type Reply<T> = oneshot::Sender<Result<T, LedgerError>>;

pub(super) enum Write {
    Create {
        job: Box<NewJob>,
        search: Option<Box<NewSearch>>,
        reply: Reply<()>,
    },
    RecordSearch {
        search: Box<NewSearch>,
        reply: Reply<()>,
    },
    StoreCandidates {
        search_id: String,
        groups: Vec<DownloadableGroup>,
        reply: Reply<()>,
    },
    ClaimSourceChoice {
        user: UserId,
        search_id: String,
        folder_id: String,
        reply: Reply<Option<ClaimedJob>>,
    },
    Prune {
        now: i64,
        reply: Reply<Pruned>,
    },
    #[cfg(test)]
    Settled {
        reply: Reply<()>,
    },
}

impl From<sqlx::Error> for LedgerError {
    fn from(e: sqlx::Error) -> Self {
        LedgerError(e.to_string())
    }
}

impl From<serde_json::Error> for LedgerError {
    fn from(e: serde_json::Error) -> Self {
        LedgerError(e.to_string())
    }
}

pub(super) fn spawn(db: SqlitePool, mut inbox: mpsc::UnboundedReceiver<Write>) {
    tokio::spawn(async move {
        while let Some(write) = inbox.recv().await {
            apply(&db, write).await;
        }
    });
}

async fn apply(db: &SqlitePool, write: Write) {
    match write {
        Write::Create { job, search, reply } => answer(
            reply,
            "record a job",
            create(db, *job, search.map(|s| *s)).await,
        ),
        Write::RecordSearch { search, reply } => {
            answer(reply, "record a search", insert_search(db, *search).await)
        }
        Write::StoreCandidates {
            search_id,
            groups,
            reply,
        } => answer(
            reply,
            "store a search's candidates",
            store_candidates(db, &search_id, &groups).await,
        ),
        Write::ClaimSourceChoice {
            user,
            search_id,
            folder_id,
            reply,
        } => answer(
            reply,
            "claim a job waiting for a source",
            claim_source_choice(db, &user, &search_id, &folder_id).await,
        ),
        Write::Prune { now, reply } => answer(reply, "remove expired rows", prune(db, now).await),
        #[cfg(test)]
        Write::Settled { reply } => {
            let _ = reply.send(Ok(()));
        }
    }
}

/// Logs a failed write and answers its sender, who may have stopped waiting.
fn answer<T>(reply: Reply<T>, what: &str, result: Result<T, LedgerError>) {
    if let Err(e) = &result {
        warn!("Could not {what} in the job ledger: {e}");
    }
    let _ = reply.send(result);
}

fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

fn import_mode_name(mode: ImportMode) -> &'static str {
    match mode {
        ImportMode::Album => "album",
        ImportMode::Singleton => "singleton",
    }
}

fn import_mode_named(name: &str) -> Result<ImportMode, LedgerError> {
    match name {
        "album" => Ok(ImportMode::Album),
        "singleton" => Ok(ImportMode::Singleton),
        other => Err(LedgerError(format!("unknown import mode {other:?}"))),
    }
}

async fn insert_search(
    db: impl sqlx::Executor<'_, Database = Sqlite>,
    search: NewSearch,
) -> Result<(), LedgerError> {
    sqlx::query(
        "INSERT INTO searches (id, user_id, backend, expires_at, query) VALUES (?, ?, ?, ?, ?)",
    )
    .bind(search.id)
    .bind(search.user.to_string())
    .bind(search.backend)
    .bind(now() + SEARCH_KEPT_SECS)
    .bind(serde_json::to_string(&search.query)?)
    .execute(db)
    .await?;
    Ok(())
}

async fn create(
    db: &SqlitePool,
    job: NewJob,
    search: Option<NewSearch>,
) -> Result<(), LedgerError> {
    let mut tx = db.begin().await?;
    if let Some(search) = search {
        insert_search(&mut *tx, search).await?;
    }
    let at = now();
    sqlx::query(
        "INSERT INTO jobs (id, user_id, folder_id, created_at, updated_at, wanted, import_mode, \
         state, search_id) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(job.id)
    .bind(job.user.to_string())
    .bind(job.folder_id)
    .bind(at)
    .bind(at)
    .bind(serde_json::to_string(&job.wanted)?)
    .bind(import_mode_name(job.import_mode))
    .bind(job.state.as_str())
    .bind(job.search_id)
    .execute(&mut *tx)
    .await?;
    Ok(tx.commit().await?)
}

async fn store_candidates(
    db: &SqlitePool,
    search_id: &str,
    groups: &[DownloadableGroup],
) -> Result<(), LedgerError> {
    sqlx::query("UPDATE searches SET candidates = ? WHERE id = ?")
        .bind(serde_json::to_string(groups)?)
        .bind(search_id)
        .execute(db)
        .await?;
    Ok(())
}

async fn claim_source_choice(
    db: &SqlitePool,
    user: &UserId,
    search_id: &str,
    folder_id: &str,
) -> Result<Option<ClaimedJob>, LedgerError> {
    let claimed: Option<(String, String)> = sqlx::query_as(
        "UPDATE jobs SET state = ?, updated_at = ? WHERE id = (\
             SELECT id FROM jobs \
             WHERE user_id = ? AND search_id = ? AND folder_id = ? AND state = ? LIMIT 1\
         ) RETURNING id, import_mode",
    )
    .bind(JobState::Queued.as_str())
    .bind(now())
    .bind(user.to_string())
    .bind(search_id)
    .bind(folder_id)
    .bind(JobState::NeedsSourceChoice.as_str())
    .fetch_optional(db)
    .await?;
    claimed
        .map(|(id, mode)| {
            Ok(ClaimedJob {
                id,
                import_mode: import_mode_named(&mode)?,
            })
        })
        .transpose()
}

async fn prune(db: &SqlitePool, now: i64) -> Result<Pruned, LedgerError> {
    let mut tx = db.begin().await?;
    let searches = sqlx::query("DELETE FROM searches WHERE expires_at <= ?")
        .bind(now)
        .execute(&mut *tx)
        .await?
        .rows_affected();
    // Every search left is unexpired, so a waiting job without one has
    // nothing left to pick from.
    let dismissed = sqlx::query(
        "UPDATE jobs SET state = ?, updated_at = ? \
         WHERE state = ? AND NOT EXISTS (SELECT 1 FROM searches s WHERE s.id = jobs.search_id)",
    )
    .bind(JobState::Dismissed.as_str())
    .bind(now)
    .bind(JobState::NeedsSourceChoice.as_str())
    .execute(&mut *tx)
    .await?
    .rows_affected();
    let terminal = vec!["?"; TERMINAL.len()].join(", ");
    let old = format!("DELETE FROM jobs WHERE state IN ({terminal}) AND updated_at < ?");
    let jobs = TERMINAL
        .iter()
        .fold(sqlx::query(&old), |query, state| query.bind(state.as_str()))
        .bind(now - JOB_KEPT_SECS)
        .execute(&mut *tx)
        .await?
        .rows_affected();
    tx.commit().await?;
    Ok(Pruned {
        searches,
        dismissed,
        jobs,
    })
}
