//! Download source search: one backend's search started and polled for a
//! user, and the every-backend ranked search an auto-download runs.

use crate::auth::UserCtx;
use crate::core::{backend_unavailable, database_failure, metadata};
use crate::db::DB;
use crate::ledger::store::{self, StoredSearch};
use crate::ledger::{NewSearch, LEDGER};
use crate::services::{
    available_download_backends, download_backend, BackendLookupError, DEFAULT_DOWNLOAD_BACKEND,
};
use dioxus::logger::tracing::warn;
use shared::download::{
    DownloadQuery, DownloadableGroup, SearchResult, SearchSpec, SearchState, SearchStats,
};
use shared::failure::{Failure, FailureCode, RetryAdvice, Stage};
use shared::metadata::{Album, Track};
use soulbeet::DownloadBackend;
use sqlx::SqlitePool;
use std::sync::Arc;
use std::time::Duration;

/// Maximum time to wait for search results from a single backend. A safety
/// net above the slskd client's own deadline, which cancels a search at 120
/// seconds and gives it 15 more to end.
const SEARCH_TIMEOUT: Duration = Duration::from_secs(180);

/// Poll interval when waiting for search results.
const SEARCH_POLL_INTERVAL: Duration = Duration::from_secs(2);

/// A backend an auto-download searches, by its id in the settings.
pub type Searched = (String, Arc<dyn DownloadBackend>);

/// The search does not exist or is another user's.
pub fn search_not_found() -> Failure {
    Failure::new(
        FailureCode::UnknownHandle,
        Stage::Input,
        RetryAdvice::NotRetryable,
        "Search not found",
    )
}

/// The search was found, and is older than SEARCH_KEPT_SECS.
pub fn search_expired() -> Failure {
    Failure::new(
        FailureCode::ExpiredHandle,
        Stage::Input,
        RetryAdvice::NotRetryable,
        "Search expired",
    )
}

/// `stored` when it exists and has not expired at `now`.
pub(crate) fn live_search(stored: Option<StoredSearch>, now: i64) -> Result<StoredSearch, Failure> {
    let stored = stored.ok_or_else(search_not_found)?;
    if stored.expires_at <= now {
        return Err(search_expired());
    }
    Ok(stored)
}

/// `ctx`'s search `id`, while it has not expired. Another user's search
/// reads like a missing one.
pub(crate) async fn owned_search(
    db: &SqlitePool,
    ctx: &UserCtx,
    id: &str,
) -> Result<StoredSearch, Failure> {
    let stored = store::search_of(db, id, &ctx.id())
        .await
        .map_err(|e| database_failure(Stage::SourceSearch, e))?;
    live_search(stored, chrono::Utc::now().timestamp())
}

/// Starts a search on the named backend, or the default one, for `query`
/// with its album's tracklist, and records it in the ledger as `ctx`'s.
pub async fn start(ctx: &UserCtx, query: DownloadQuery) -> Result<String, Failure> {
    let key = metadata::lastfm_key(ctx).await?;
    let mut query = query;
    metadata::hydrate_album_tracks(&mut query, key.as_deref()).await?;

    // Stored by the id the backend is looked up by, so a poll finds it again.
    let backend_id = query
        .backend
        .clone()
        .unwrap_or_else(|| DEFAULT_DOWNLOAD_BACKEND.to_string());
    let backend = download_backend(Some(&backend_id))
        .await
        .map_err(|e| backend_unavailable(Stage::SourceSearch, &e))?;

    let search_id = backend
        .start_search(&SearchSpec::of(query.album.clone(), query.tracks.clone()))
        .await
        .map_err(|e| e.failure(Stage::SourceSearch, e.to_string()))?;
    LEDGER
        .record_search(NewSearch {
            id: search_id.clone(),
            user: ctx.id(),
            backend: Some(backend_id),
            query,
        })
        .await
        .map_err(|e| database_failure(Stage::SourceSearch, e))?;
    Ok(search_id)
}

/// The state and groups of `ctx`'s search `search_id`, on the backend it
/// was started on. A search that ended stores its groups as its candidates.
pub async fn poll(ctx: &UserCtx, search_id: &str) -> Result<SearchResult, Failure> {
    let stored = owned_search(&DB, ctx, search_id).await?;
    let backend = download_backend(stored.backend.as_deref())
        .await
        .map_err(|e| backend_unavailable(Stage::SourceSearch, &e))?;

    let result = backend
        .poll_search(search_id)
        .await
        .map_err(|e| e.failure(Stage::SourceSearch, e.to_string()))?;
    if result.state != SearchState::InProgress {
        LEDGER
            .store_candidates(search_id, result.groups.clone())
            .await
            .map_err(|e| database_failure(Stage::SourceSearch, e))?;
    }
    Ok(result)
}

/// How one backend's search ended.
enum SearchStop {
    /// The backend finished the search, having read what `stats` says.
    Finished { stats: SearchStats },
    /// SEARCH_TIMEOUT passed before the backend finished.
    TimedOut,
    /// Starting or polling the search failed.
    Failed(Failure),
}

/// One backend's search for `spec`, `id` naming it in the logs: the groups it
/// found and how it ended.
async fn search_backend(
    id: &str,
    backend: &Arc<dyn DownloadBackend>,
    spec: &SearchSpec,
) -> (Vec<DownloadableGroup>, SearchStop) {
    let search_id = match backend.start_search(spec).await {
        Ok(sid) => sid,
        Err(e) => {
            warn!("Backend {} search start failed: {}", id, e);
            let stop = SearchStop::Failed(e.failure(Stage::SourceSearch, e.to_string()));
            return (Vec::new(), stop);
        }
    };

    // Poll until results are ready or timeout. A backend may send
    // groups while the search is in progress, so keep the latest
    // non-empty batch. slskd sends its groups only in the completing
    // poll.
    let deadline = tokio::time::Instant::now() + SEARCH_TIMEOUT;
    let mut latest_groups = Vec::<DownloadableGroup>::new();
    loop {
        if tokio::time::Instant::now() >= deadline {
            warn!("Backend {} search timed out", id);
            return (latest_groups, SearchStop::TimedOut);
        }
        tokio::time::sleep(SEARCH_POLL_INTERVAL).await;

        match backend.poll_search(&search_id).await {
            Ok(result) => match result.state {
                SearchState::Completed | SearchState::TimedOut => {
                    if !result.groups.is_empty() {
                        latest_groups = result.groups;
                    }
                    let stop = SearchStop::Finished {
                        stats: result.stats,
                    };
                    return (latest_groups, stop);
                }
                SearchState::InProgress => {
                    if !result.groups.is_empty() {
                        latest_groups = result.groups;
                    }
                }
            },
            Err(e) => {
                warn!("Backend {} poll error: {}", id, e);
                let stop = SearchStop::Failed(e.failure(Stage::SourceSearch, e.to_string()));
                return (latest_groups, stop);
            }
        }
    }
}

/// Why a search of `first` and `rest` found nothing: when any backend
/// finished, an empty search, or one whose files were all scored out when its
/// responses held files; else a timeout when any timed out, else the first
/// backend's own failure.
fn no_results_failure(first: &SearchStop, rest: &[SearchStop]) -> Failure {
    const MESSAGE: &str = "No results found";
    let rank = |stop: &SearchStop| match stop {
        SearchStop::Finished { .. } => 2,
        SearchStop::TimedOut => 1,
        SearchStop::Failed(_) => 0,
    };
    // The earliest of the highest rank.
    let stop = rest
        .iter()
        .fold(first, |best, s| if rank(s) > rank(best) { s } else { best });
    match stop {
        SearchStop::Finished {
            stats: SearchStats {
                file_count: 1.., ..
            },
        } => Failure::new(
            FailureCode::SearchAllFiltered,
            Stage::SourceSearch,
            RetryAdvice::OtherQuery,
            MESSAGE,
        ),
        SearchStop::Finished {
            stats: SearchStats { file_count: 0, .. },
        } => Failure::new(
            FailureCode::SearchNoResults,
            Stage::SourceSearch,
            RetryAdvice::OtherQuery,
            MESSAGE,
        ),
        SearchStop::TimedOut => Failure::new(
            FailureCode::SearchTimeout,
            Stage::SourceSearch,
            RetryAdvice::SameSourceLater,
            MESSAGE,
        ),
        SearchStop::Failed(f) => f.clone().with_message(MESSAGE),
    }
}

/// No backend was available, `e` saying why the default one was not.
fn no_backends(e: &BackendLookupError) -> Failure {
    e.failure(Stage::SourceSearch, "No download backends available")
}

/// The backends to search, the default one first when it is available, or
/// the default one's lookup error as `no_backends` when none is.
pub async fn usable_backends() -> Result<(Searched, Vec<Searched>), Failure> {
    let mut others = Vec::new();
    for (id, _) in available_download_backends() {
        if id == DEFAULT_DOWNLOAD_BACKEND {
            continue;
        }
        match download_backend(Some(id)).await {
            Ok(b) => others.push((id.to_string(), b)),
            Err(e) => warn!("Backend {} unavailable for auto-download: {}", id, e),
        }
    }
    match download_backend(None).await {
        Ok(b) => Ok(((DEFAULT_DOWNLOAD_BACKEND.to_string(), b), others)),
        Err(e) => {
            warn!(
                "Backend {} unavailable for auto-download: {}",
                DEFAULT_DOWNLOAD_BACKEND, e
            );
            let mut others = others.into_iter();
            match others.next() {
                Some(first) => Ok((first, others.collect())),
                None => Err(no_backends(&e)),
            }
        }
    }
}

/// Searches every backend at once: all groups found, best score first, or
/// the failure of the stops when none was found.
pub async fn search_ranked(
    first: &Searched,
    rest: &[Searched],
    album: Option<&Album>,
    tracks: &[Track],
) -> Result<Vec<DownloadableGroup>, Failure> {
    let spec = SearchSpec::of(album.cloned(), tracks.to_vec());
    let ((first_groups, first_stop), results) = futures::future::join(
        search_backend(&first.0, &first.1, &spec),
        futures::future::join_all(
            rest.iter()
                .map(|(id, backend)| search_backend(id, backend, &spec)),
        ),
    )
    .await;

    let (groups, stops): (Vec<_>, Vec<_>) = results.into_iter().unzip();
    let mut all_groups: Vec<DownloadableGroup> = first_groups
        .into_iter()
        .chain(groups.into_iter().flatten())
        .collect();
    if all_groups.is_empty() {
        return Err(no_results_failure(&first_stop, &stops));
    }
    all_groups.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    Ok(all_groups)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::UserId;
    use crate::core::test_ctx;
    use crate::ledger::{Ledger, NewSearch};
    use crate::server_fns::download::test_support::{group, item, search_result, StubBackend};
    use shared::failure::UserActionCode;
    use soulbeet::error::{MissingSetting, SoulseekError};

    /// The groups `stub`'s search found and the failure an empty one gives.
    async fn searched(stub: Arc<StubBackend>) -> (Vec<DownloadableGroup>, Failure) {
        let backend: Arc<dyn DownloadBackend> = stub;
        let (groups, stop) = search_backend("stub", &backend, &SearchSpec::default()).await;
        (groups, no_results_failure(&stop, &[]))
    }

    /// A search the backend finished with no file in its responses.
    fn finished_empty() -> SearchStop {
        SearchStop::Finished {
            stats: SearchStats::default(),
        }
    }

    fn advice(f: &Failure) -> (FailureCode, Stage, RetryAdvice, &str) {
        (f.code, f.stage, f.retry, f.message.as_str())
    }

    // Paused: every poll waits SEARCH_POLL_INTERVAL first.
    #[tokio::test(start_paused = true)]
    async fn a_search_slskd_refuses_to_start_is_not_an_empty_search() {
        let stub = StubBackend::new(vec![]);
        let refused = "The server connection must be connected and logged in to perform a search (currently: Disconnected)";
        stub.fail_search_start(SoulseekError::Api {
            status: 409,
            message: refused.into(),
        });

        let (groups, f) = searched(stub).await;

        assert!(groups.is_empty());
        assert_eq!(
            advice(&f),
            (
                FailureCode::BackendUnreachable,
                Stage::SourceSearch,
                RetryAdvice::SameSourceLater,
                "No results found"
            )
        );
        let cause = f.cause.expect("slskd's answer");
        assert_eq!((cause.status, cause.raw.as_str()), (Some(409), refused));
    }

    #[tokio::test(start_paused = true)]
    async fn a_search_whose_poll_fails_is_not_an_empty_search() {
        let stub = StubBackend::new(vec![]);
        stub.script_polls(vec![Err(SoulseekError::Api {
            status: 503,
            message: "Circuit breaker is open - slskd appears to be unavailable".into(),
        })]);

        let (_, f) = searched(stub).await;

        assert_eq!(
            advice(&f),
            (
                FailureCode::BackendUnreachable,
                Stage::SourceSearch,
                RetryAdvice::SameSourceLater,
                "No results found"
            )
        );
        assert_eq!(f.cause.and_then(|c| c.status), Some(503));
    }

    #[tokio::test(start_paused = true)]
    async fn a_search_that_finishes_empty_asks_for_another_query() {
        let stub = StubBackend::new(vec![]);
        stub.script_polls(vec![Ok(search_result(
            "search",
            SearchState::Completed,
            Vec::new(),
        ))]);

        let (groups, f) = searched(stub).await;

        assert!(groups.is_empty());
        assert_eq!(
            advice(&f),
            (
                FailureCode::SearchNoResults,
                Stage::SourceSearch,
                RetryAdvice::OtherQuery,
                "No results found"
            )
        );
        assert_eq!(f.cause, None);
    }

    #[tokio::test(start_paused = true)]
    async fn a_search_still_running_at_the_deadline_is_a_timeout() {
        let stub = StubBackend::new(vec![]);

        let (_, f) = searched(stub).await;

        assert_eq!(
            advice(&f),
            (
                FailureCode::SearchTimeout,
                Stage::SourceSearch,
                RetryAdvice::SameSourceLater,
                "No results found"
            )
        );
    }

    /// A backend may send groups while the search runs: those an earlier
    /// poll found are the result when the completing poll is empty.
    #[tokio::test(start_paused = true)]
    async fn a_search_keeps_the_groups_found_before_it_completed() {
        let stub = StubBackend::new(vec![]);
        let found = vec![group(
            "peer_a",
            vec![item("peer_a", "Wake Up", "a.flac")],
            0.9,
        )];
        stub.script_polls(vec![
            Ok(search_result(
                "search",
                SearchState::InProgress,
                found.clone(),
            )),
            Ok(search_result("search", SearchState::Completed, Vec::new())),
        ]);
        let backend: Arc<dyn DownloadBackend> = stub;

        let (groups, stop) = search_backend("stub", &backend, &SearchSpec::default()).await;

        assert_eq!(groups, found);
        assert!(matches!(stop, SearchStop::Finished { .. }));
    }

    #[test]
    fn a_search_whose_files_were_all_scored_out_is_all_filtered() {
        let f = no_results_failure(
            &SearchStop::Finished {
                stats: SearchStats {
                    file_count: 7,
                    ..Default::default()
                },
            },
            &[SearchStop::TimedOut],
        );
        assert_eq!(
            (f.code, f.stage, f.retry, f.message.as_str()),
            (
                FailureCode::SearchAllFiltered,
                Stage::SourceSearch,
                RetryAdvice::OtherQuery,
                "No results found"
            )
        );
        assert_eq!(
            no_results_failure(&finished_empty(), &[]).code,
            FailureCode::SearchNoResults
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_finished_search_reports_the_files_it_saw() {
        let stub = StubBackend::new(vec![]);
        stub.script_polls(vec![Ok(SearchResult {
            stats: SearchStats {
                file_count: 7,
                ..Default::default()
            },
            ..search_result("search", SearchState::Completed, Vec::new())
        })]);
        let backend: Arc<dyn DownloadBackend> = stub;
        let (_, stop) = search_backend("stub", &backend, &SearchSpec::default()).await;
        assert!(matches!(
            stop,
            SearchStop::Finished {
                stats: SearchStats { file_count: 7, .. }
            }
        ));
    }

    #[test]
    fn no_results_failure_tells_an_empty_search_from_a_dead_backend() {
        let dead = |message: &str| {
            SearchStop::Failed(
                SoulseekError::Api {
                    status: 503,
                    message: message.into(),
                }
                .failure(Stage::SourceSearch, "x"),
            )
        };
        let advice = |first: SearchStop, rest: Vec<SearchStop>| {
            let f = no_results_failure(&first, &rest);
            (f.code, f.retry)
        };
        use FailureCode::*;
        use RetryAdvice::*;
        assert_eq!(
            advice(finished_empty(), vec![]),
            (SearchNoResults, OtherQuery)
        );
        assert_eq!(
            advice(SearchStop::TimedOut, vec![]),
            (SearchTimeout, SameSourceLater)
        );
        assert_eq!(
            advice(dead("down"), vec![]),
            (BackendUnreachable, SameSourceLater)
        );
        assert_eq!(
            advice(dead("down"), vec![finished_empty()]),
            (SearchNoResults, OtherQuery)
        );
        // A timeout says more than a dead backend, an empty search more than both.
        assert_eq!(
            advice(dead("down"), vec![SearchStop::TimedOut]),
            (SearchTimeout, SameSourceLater)
        );
        assert_eq!(
            advice(SearchStop::TimedOut, vec![finished_empty()]),
            (SearchNoResults, OtherQuery)
        );
        let f = no_results_failure(&dead("first"), &[dead("second")]);
        assert_eq!(f.message, "No results found");
        assert_eq!(f.stage, Stage::SourceSearch);
        assert_eq!(f.cause.unwrap().raw, "first");
    }

    /// slskd lost the search: its 404 on the responses is a failed search
    /// to run again, not an empty one.
    #[tokio::test(start_paused = true)]
    async fn a_search_slskd_no_longer_has_is_not_an_empty_search() {
        let stub = StubBackend::new(vec![]);
        stub.script_polls(vec![Err(SoulseekError::SearchGone {
            body: String::new(),
        })]);

        let (_, f) = searched(stub).await;

        assert_eq!(
            advice(&f),
            (
                FailureCode::SearchLost,
                Stage::SourceSearch,
                RetryAdvice::SameSourceLater,
                "No results found"
            )
        );
        assert_eq!(f.cause.and_then(|c| c.status), Some(404));
    }

    /// slskd refused the query itself: another query, not a wait.
    #[tokio::test(start_paused = true)]
    async fn a_query_slskd_refuses_asks_for_another_query() {
        let stub = StubBackend::new(vec![]);
        let body = "Search query must contain at least one non-exclusion term (Parameter 'query')";
        stub.fail_search_start(SoulseekError::SearchQueryRejected { body: body.into() });

        let (_, f) = searched(stub).await;

        assert_eq!(
            advice(&f),
            (
                FailureCode::SearchQueryRejected,
                Stage::SourceSearch,
                RetryAdvice::OtherQuery,
                "No results found"
            )
        );
        let cause = f.cause.unwrap();
        assert_eq!((cause.status, cause.raw.as_str()), (Some(400), body));
    }

    #[test]
    fn a_search_is_readable_until_it_expires() {
        let stored = |expires_at| {
            Some(StoredSearch {
                id: "s1".into(),
                backend: Some("slskd".into()),
                query: DownloadQuery::new(vec![]),
                candidates: None,
                expires_at,
            })
        };
        assert_eq!(live_search(None, 1_000).unwrap_err(), search_not_found());
        assert_eq!(
            live_search(stored(1_000), 1_000).unwrap_err(),
            search_expired()
        );
        assert_eq!(
            live_search(stored(1_001), 1_000).map(|s| s.expires_at),
            Ok(1_001)
        );
        let f = search_expired();
        assert_eq!(
            (f.code, f.stage, f.retry, f.message.as_str()),
            (
                FailureCode::ExpiredHandle,
                Stage::Input,
                RetryAdvice::NotRetryable,
                "Search expired"
            )
        );
    }

    #[tokio::test]
    async fn another_users_search_reads_like_a_missing_one() {
        let db = crate::db::test_db::migrated().await;
        crate::core::folder_of(&db, "alice").await;
        crate::core::folder_of(&db, "bob").await;
        let ledger = Ledger::start(db.clone());
        ledger
            .record_search(NewSearch {
                id: "s1".into(),
                user: UserId::new("alice"),
                backend: Some("slskd".into()),
                query: DownloadQuery::new(vec![]),
            })
            .await
            .unwrap();
        assert_eq!(
            owned_search(&db, &test_ctx("bob"), "s1").await.unwrap_err(),
            search_not_found()
        );
        assert_eq!(
            owned_search(&db, &test_ctx("alice"), "s1")
                .await
                .map(|s| s.backend),
            Ok(Some("slskd".into()))
        );
    }

    // Paused: every poll waits SEARCH_POLL_INTERVAL first.
    #[tokio::test(start_paused = true)]
    async fn search_ranked_puts_the_best_group_of_every_backend_first() {
        let found =
            |peer: &str, score| vec![group(peer, vec![item(peer, "Wake Up", "a.flac")], score)];
        let (a, b) = (StubBackend::new(vec![]), StubBackend::new(vec![]));
        a.script_polls(vec![Ok(search_result(
            "search",
            SearchState::Completed,
            found("peer_a", 0.5),
        ))]);
        b.script_polls(vec![Ok(search_result(
            "search",
            SearchState::Completed,
            found("peer_b", 0.9),
        ))]);
        let first: Searched = ("a".into(), a as Arc<dyn DownloadBackend>);
        let rest: Vec<Searched> = vec![("b".into(), b as Arc<dyn DownloadBackend>)];
        let groups = search_ranked(&first, &rest, None, &[]).await.unwrap();
        let ranked: Vec<(&str, f64)> = groups
            .iter()
            .map(|g| (g.source.as_str(), g.score))
            .collect();
        assert_eq!(ranked, vec![("peer_b", 0.9), ("peer_a", 0.5)]);
    }

    #[tokio::test(start_paused = true)]
    async fn search_ranked_with_nothing_found_fails_as_no_results() {
        let a = StubBackend::new(vec![]);
        a.script_polls(vec![Ok(search_result(
            "search",
            SearchState::Completed,
            Vec::new(),
        ))]);
        let first: Searched = ("a".into(), a as Arc<dyn DownloadBackend>);
        let f = search_ranked(&first, &[], None, &[]).await.unwrap_err();
        assert_eq!(
            (f.code, f.message.as_str()),
            (FailureCode::SearchNoResults, "No results found")
        );
    }

    #[test]
    fn an_unknown_search_is_not_found() {
        let f = search_not_found();
        assert_eq!(
            (f.code, f.stage, f.retry, f.message.as_str()),
            (
                FailureCode::UnknownHandle,
                Stage::Input,
                RetryAdvice::NotRetryable,
                "Search not found"
            )
        );
    }

    #[test]
    fn no_backends_keeps_why_the_default_backend_was_unavailable() {
        let unset = BackendLookupError::Backend(SoulseekError::NotConfigured(MissingSetting::Url));
        let f = no_backends(&unset);
        assert_eq!(
            (f.code, f.stage, f.retry, f.message.as_str()),
            (
                FailureCode::BackendNotConfigured,
                Stage::SourceSearch,
                RetryAdvice::NeedsUser,
                "No download backends available"
            )
        );
        assert_eq!(
            f.user_action.unwrap().code,
            UserActionCode::ConfigureBackend
        );
        assert_eq!(f.cause.unwrap().raw, "slskd URL not configured");
    }
}
