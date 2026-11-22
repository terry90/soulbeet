//! Tracks whose source search found nothing in discovery runs. The engine suggests the same
//! tracks again run after run, and a search the Soulseek server drops finds nothing every time,
//! wildcards included, so a track that found nothing in SEARCH_MISSES_KEPT_OUT runs is not
//! searched again.

use std::collections::HashSet;

use sqlx::SqlitePool;

use crate::db::DB;
use crate::models::discovery_history::seen_key;

/// Runs whose search for a track found nothing before discovery stops searching for it. More
/// than one, so a run during an outage of the network does not keep every track it tried out.
pub const SEARCH_MISSES_KEPT_OUT: u32 = 3;

/// Counts one more run whose search for `artist` - `track` found nothing for `user_id`.
pub async fn record(user_id: &str, artist: &str, track: &str) -> Result<(), String> {
    record_in(&DB, user_id, artist, track).await
}

pub(crate) async fn record_in(
    db: &SqlitePool,
    user_id: &str,
    artist: &str,
    track: &str,
) -> Result<(), String> {
    sqlx::query(
        "INSERT INTO discovery_search_misses (user_id, track_key, misses) VALUES (?, ?, 1)
         ON CONFLICT(user_id, track_key) DO UPDATE SET misses = misses + 1",
    )
    .bind(user_id)
    .bind(seen_key(artist, track))
    .execute(db)
    .await
    .map_err(|e| e.to_string())?;
    Ok(())
}

/// The `seen_key`s of the tracks whose search found nothing in SEARCH_MISSES_KEPT_OUT runs.
pub async fn kept_out_keys(user_id: &str) -> Result<HashSet<String>, String> {
    kept_out_keys_in(&DB, user_id).await
}

pub(crate) async fn kept_out_keys_in(
    db: &SqlitePool,
    user_id: &str,
) -> Result<HashSet<String>, String> {
    let keys: Vec<String> = sqlx::query_scalar(
        "SELECT track_key FROM discovery_search_misses WHERE user_id = ? AND misses >= ?",
    )
    .bind(user_id)
    .bind(SEARCH_MISSES_KEPT_OUT)
    .fetch_all(db)
    .await
    .map_err(|e| e.to_string())?;
    Ok(keys.into_iter().collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::test_db;
    use crate::security::SEEDED_ADMIN_ID;

    #[tokio::test]
    async fn a_track_is_kept_out_once_its_search_found_nothing_in_three_runs() {
        let db = test_db::migrated().await;
        for _ in 1..SEARCH_MISSES_KEPT_OUT {
            record_in(&db, SEEDED_ADMIN_ID, "MGMT", "Kids")
                .await
                .unwrap();
        }
        assert!(kept_out_keys_in(&db, SEEDED_ADMIN_ID)
            .await
            .unwrap()
            .is_empty());
        // The engine may name it in another case.
        record_in(&db, SEEDED_ADMIN_ID, "mgmt", "KIDS")
            .await
            .unwrap();
        record_in(&db, SEEDED_ADMIN_ID, "Kanye West", "Heartless")
            .await
            .unwrap();
        assert_eq!(
            kept_out_keys_in(&db, SEEDED_ADMIN_ID).await.unwrap(),
            HashSet::from([seen_key("MGMT", "Kids")])
        );
        assert!(kept_out_keys_in(&db, "someone-else")
            .await
            .unwrap()
            .is_empty());
    }
}
