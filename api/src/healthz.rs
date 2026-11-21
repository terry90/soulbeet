//! `GET /healthz`, for the health checks of Docker, Kubernetes and the like.
//!
//! Anyone may call it: the answer says only whether this process serves
//! requests and its database answers. It asks no backend (slskd, Navidrome),
//! whose health each user sees in the app (`core::health`).

use std::time::Duration;

use axum::{
    extract::State,
    http::{header::CACHE_CONTROL, StatusCode},
    response::IntoResponse,
    routing::get,
    Router,
};
use sqlx::SqlitePool;

const PATH: &str = "/healthz";

/// How long the database gets to answer, waiting for a free connection
/// included. A probe gets a 503 past it rather than waiting on.
const DATABASE_WAIT: Duration = Duration::from_secs(2);

/// The route, to merge into the app router.
pub fn router() -> Router {
    routes((*crate::db::DB).clone(), DATABASE_WAIT)
}

#[derive(Clone)]
struct Probe {
    db: SqlitePool,
    wait: Duration,
}

fn routes(db: SqlitePool, wait: Duration) -> Router {
    Router::new()
        .route(PATH, get(healthz))
        .with_state(Probe { db, wait })
}

/// 200 when the database runs `SELECT 1` within the wait, 503 otherwise.
/// The body names no cause: the log does.
async fn healthz(State(probe): State<Probe>) -> impl IntoResponse {
    let select = sqlx::query("SELECT 1").execute(&probe.db);
    let (status, body) = match tokio::time::timeout(probe.wait, select).await {
        Ok(Ok(_)) => (StatusCode::OK, "ok"),
        Ok(Err(e)) => {
            tracing::warn!("Health check: the database failed: {e}");
            (StatusCode::SERVICE_UNAVAILABLE, "unavailable")
        }
        Err(_) => {
            tracing::warn!(
                "Health check: the database did not answer within {:?}",
                probe.wait
            );
            (StatusCode::SERVICE_UNAVAILABLE, "unavailable")
        }
    };
    (status, [(CACHE_CONTROL, "no-store")], body)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::test_db;
    use axum::{body::Body, http::Request};
    use tower::ServiceExt;

    /// The status, `Cache-Control` and body of a `GET /healthz` sent without
    /// a session.
    async fn check(db: SqlitePool, wait: Duration) -> (StatusCode, String, String) {
        let response = routes(db, wait)
            .oneshot(Request::get(PATH).body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let cache = response.headers()[CACHE_CONTROL].to_str().unwrap().into();
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        (status, cache, String::from_utf8(body.to_vec()).unwrap())
    }

    #[tokio::test]
    async fn a_database_that_answers_is_healthy() {
        let (status, cache, body) = check(test_db::empty().await, DATABASE_WAIT).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(cache, "no-store");
        assert_eq!(body, "ok");
    }

    #[tokio::test]
    async fn a_closed_database_is_unavailable_and_the_answer_names_no_cause() {
        let db = test_db::empty().await;
        db.close().await;
        let (status, cache, body) = check(db, DATABASE_WAIT).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(cache, "no-store");
        assert_eq!(body, "unavailable");
    }

    #[tokio::test]
    async fn a_database_with_no_free_connection_is_unavailable_once_the_wait_is_over() {
        // `empty` has one connection: holding it leaves the check waiting.
        let db = test_db::empty().await;
        let _held = db.acquire().await.unwrap();
        let wait = Duration::from_millis(50);
        let started = std::time::Instant::now();
        // The pool's own acquire timeout is 30 s, so an answer within 5 s
        // comes from the check's wait.
        let (status, _, body) =
            tokio::time::timeout(Duration::from_secs(5), check(db.clone(), wait))
                .await
                .expect("the check answers once its wait is over");
        assert!(started.elapsed() >= wait, "it answered before the wait");
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body, "unavailable");
    }
}
