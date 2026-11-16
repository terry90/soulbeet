//! D11: the calls a job's `next` offers, built in this one place from the job's state, its
//! failure, its kind and its untried candidates, by the predicate the job tools refuse with.

use serde_json::{json, Value};
use shared::failure::{Failure, FailureCode, RetryAdvice, Stage};

use super::contract::NextCall;
use super::facts::candidate_document;
use crate::core::deletion::SONGS_PER_TOKEN;
use crate::core::jobs::decide::kept_imports;
use crate::core::jobs::{allowed, job_failure, slot_failure, JobView, RetryMode, Untried};
use crate::ledger::{BatchOrigin, JobState, SlotResult, Transition};

/// How many untried candidates a job waiting for a source choice offers as download_source calls.
const CHOICES_OFFERED: usize = 3;

/// The calls that can be made for the job now. A running job: a wait for its next change, unless
/// it is stranded until the next start, when no change comes before it. A job
/// waiting for a source choice: download_source for each of its first untried candidates, with
/// that candidate's facts. Each retry_job mode `allowed` accepts, with its deciding facts, unless
/// the job's failure needs a person (its user_action says what; retry_job still accepts a mode
/// once they acted). A job waiting for an import decision: get_import_choices and resolve_import's
/// retry for each import it keeps, naming its slot when it keeps several. dismiss_job and
/// cancel_job where those transitions apply. A web UI job is offered only the wait, the import
/// decision and dismiss_job: its own pipeline runs it. With `deletion_enabled` (D14) an app's job
/// follows the retry entries with previews, never a deletion, of what it left behind
/// (leftover_previews).
pub(crate) fn next_calls(
    view: &JobView,
    untried: &Untried,
    deletion_enabled: bool,
) -> Vec<NextCall> {
    use JobState::*;
    let job = &view.job;
    let id = &job.id;
    let mcp = job.origin == BatchOrigin::Mcp;
    let mut next = Vec::new();
    if !view.stranded
        && matches!(
            job.state,
            Searching | Queued | Downloading | Importing | Indexing
        )
    {
        next.push(NextCall::new(
            "get_jobs",
            json!({"job_ids": [id], "detail": "full", "wait_seconds": 60,
                "wait_for": "any_change"}),
        ));
    }
    // The search's query holds an album's tracklist.
    let wanted = view.search.as_ref().map_or(&job.wanted, |s| &s.query);
    if mcp && job.state == NeedsSourceChoice {
        for candidate in untried.candidates.iter().take(CHOICES_OFFERED) {
            let Value::Object(facts) = candidate_document(candidate, wanted) else {
                unreachable!("a candidate document is an object");
            };
            next.push(NextCall {
                tool: "download_source",
                args: json!({"job_id": id, "candidate_id": candidate.id}),
                facts,
            });
        }
    }
    if job.state == NeedsDecision {
        let kept = kept_imports(view);
        for import in &kept {
            let mut args = json!({"job_id": id});
            if kept.len() > 1 {
                args["slot"] = json!(import.slots[0]);
            }
            next.push(NextCall::new("get_import_choices", args.clone()));
            args["choice"] = json!("retry");
            next.push(NextCall::new("resolve_import", args));
        }
    }
    let failure = job_failure(view);
    let needs_user = (failure.as_ref()).is_some_and(|f| f.retry == RetryAdvice::NeedsUser);
    if !needs_user {
        let modes = [
            RetryMode::IndexOnly,
            RetryMode::SameSource,
            RetryMode::NextSource,
            RetryMode::NewSearch,
            RetryMode::RejectResult,
        ];
        for mode in modes {
            if allowed(view, untried, mode) {
                next.push(retry_call(view, untried, mode, failure.as_ref()));
            }
        }
    }
    if mcp && deletion_enabled {
        next.extend(leftover_previews(view));
    }
    let job_call = |tool| NextCall::new(tool, json!({"job_id": id}));
    if Transition::Dismiss.allowed_from().contains(&job.state) {
        next.push(job_call("dismiss_job"));
    }
    if mcp && Transition::Cancel.allowed_from().contains(&job.state) {
        next.push(job_call("cancel_job"));
    }
    next
}

/// preview_delete_tracks, reason replaced, of what the job left behind (D14): for a slot failed
/// with result_rejected, the songs its result lists (the wrong copy); for a done slot, the copies
/// a replacing import did not replace. Each song once, in slot order, SONGS_PER_TOKEN a call.
fn leftover_previews(view: &JobView) -> Vec<NextCall> {
    let mut songs: Vec<&str> = Vec::new();
    for slot in &view.slots {
        let rejected = slot.state == JobState::Failed
            && slot_failure(slot).is_some_and(|f| f.code == FailureCode::ResultRejected);
        let left: Vec<&str> = match (&slot.result, slot.state) {
            (
                Some(SlotResult::Imported {
                    listed: Some(listed),
                    ..
                }),
                _,
            ) if rejected => listed.iter().map(|s| s.song_id.as_str()).collect(),
            (Some(SlotResult::Imported { not_replaced, .. }), JobState::Done) => {
                not_replaced.iter().map(String::as_str).collect()
            }
            _ => continue,
        };
        for song in left {
            if !songs.contains(&song) {
                songs.push(song);
            }
        }
    }
    (songs.chunks(SONGS_PER_TOKEN))
        .map(|songs| {
            NextCall::new(
                "preview_delete_tracks",
                json!({"song_ids": songs, "reason": "replaced"}),
            )
        })
        .collect()
}

/// retry_job in `mode` for the job, with its deciding facts: same_source the wait its failure
/// asks for, next_source how many untried candidates are left and the best one, new_search after
/// a failed search the wait that search's failure asks for.
fn retry_call(
    view: &JobView,
    untried: &Untried,
    mode: RetryMode,
    failure: Option<&Failure>,
) -> NextCall {
    let call = NextCall::new(
        "retry_job",
        json!({"job_id": view.job.id, "mode": mode.as_str()}),
    );
    let wait = failure.and_then(|f| f.retry_after_seconds);
    match mode {
        RetryMode::SameSource => call.with("retry_after_seconds", json!(wait)),
        RetryMode::NextSource => {
            let wanted = view.search.as_ref().map_or(&view.job.wanted, |s| &s.query);
            let best = untried.best().map(|c| candidate_document(c, wanted));
            call.with("untried_candidates", json!(untried.candidates.len()))
                .with("best_candidate", json!(best))
        }
        RetryMode::NewSearch => match (failure, wait) {
            (Some(f), Some(wait)) if f.stage == Stage::SourceSearch => {
                call.with("retry_after_seconds", json!(wait))
            }
            _ => call,
        },
        RetryMode::IndexOnly | RetryMode::RejectResult => call,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::UserId;
    use crate::core::jobs::retry::retry;
    use crate::core::jobs::runner::Acquire;
    use crate::core::jobs::test_support::{
        cancelled_track_job, choosing_track_job, done_track_job, downloading_track_job,
        exhausted_track_job, failed_track_job, failed_track_job_with, failed_web_ui_job,
        index_failed_track_job, queued_track_jobs, settle_done, view_of,
    };
    use crate::core::jobs::untried;
    use crate::ledger::{store, Transitioned};
    use shared::failure::{FailureCode, UserAction, UserActionCode};

    async fn read(acq: &Acquire, job: &str) -> (JobView, Untried) {
        let view = view_of(acq, job).await;
        let untried = untried(&acq.db, &view, chrono::Utc::now().timestamp())
            .await
            .unwrap();
        (view, untried)
    }

    /// The modes of the retry_job entries of `next`.
    fn modes(next: &[NextCall]) -> Vec<String> {
        (next.iter())
            .filter(|n| n.tool == "retry_job")
            .map(|n| n.args["mode"].as_str().unwrap().to_string())
            .collect()
    }

    #[tokio::test]
    async fn a_running_job_offers_a_wait_and_a_cancel() {
        let (acq, _) = Acquire::for_alice().await;
        let job = downloading_track_job(&acq).await;
        let (view, untried) = read(&acq, &job).await;
        let next = next_calls(&view, &untried, false);
        assert_eq!(
            serde_json::to_value(&next).unwrap(),
            json!([{"tool": "get_jobs",
                "args": {"job_ids": [job], "detail": "full", "wait_seconds": 60, "wait_for": "any_change"}},
                {"tool": "cancel_job", "args": {"job_id": job}}])
        );
    }

    #[tokio::test]
    async fn next_lists_only_the_modes_retry_job_accepts() {
        let (acq, _) = Acquire::for_alice().await;
        // Soulful's text for a broken importer (importer_health), which needs a person.
        let importer_broken = Failure::new(
            FailureCode::ImporterBroken,
            Stage::Import,
            RetryAdvice::NeedsUser,
            "No metadata source plugin loaded",
        )
        .with_user_action(UserAction::new(
            UserActionCode::FixImporter,
            "Fix the importer",
            "Settings > System",
        ));
        let fixtures = [
            ("failed, an untried source", failed_track_job(&acq).await),
            (
                "failed, every source tried",
                exhausted_track_job(&acq).await,
            ),
            ("failed, busy source", busy_job(&acq).await),
            ("failed at the index", index_failed_track_job(&acq).await),
            (
                "needs a person",
                failed_track_job_with(&acq, "Silent Night", importer_broken).await,
            ),
            ("done", done_track_job(&acq).await),
            ("cancelled", cancelled_track_job(&acq).await),
            ("waiting for a choice", choosing_track_job(&acq).await),
        ];
        let mut offered = Vec::new();
        for (name, job) in &fixtures {
            let (view, untried) = read(&acq, job).await;
            let next = modes(&next_calls(&view, &untried, false));
            let accepted: Vec<String> = (RetryMode::ALL.into_iter())
                .filter(|m| allowed(&view, &untried, *m))
                .map(|m| m.as_str().to_string())
                .collect();
            assert!(
                next.iter().all(|m| accepted.contains(m)),
                "{name}: {next:?} {accepted:?}"
            );
            if *name == "needs a person" {
                // Offered nothing; accepted once the person did what user_action says.
                assert!(next.is_empty(), "{name}");
                assert_eq!(accepted, vec!["next_source", "new_search"], "{name}");
            } else {
                let mut sorted = next.clone();
                sorted.sort();
                let mut accepted = accepted;
                accepted.sort();
                assert_eq!(sorted, accepted, "{name}");
            }
            offered.push((*name, next));
        }
        let expected: Vec<(&str, Vec<&str>)> = vec![
            (
                "failed, an untried source",
                vec!["next_source", "new_search"],
            ),
            ("failed, every source tried", vec!["new_search"]),
            (
                "failed, busy source",
                vec!["same_source", "next_source", "new_search"],
            ),
            ("failed at the index", vec!["index_only", "new_search"]),
            ("needs a person", vec![]),
            ("done", vec!["reject_result"]),
            (
                "cancelled",
                vec!["same_source", "next_source", "new_search"],
            ),
            ("waiting for a choice", vec!["new_search"]),
        ];
        let offered: Vec<(&str, Vec<&str>)> = (offered.iter())
            .map(|(name, modes)| (*name, modes.iter().map(String::as_str).collect()))
            .collect();
        assert_eq!(offered, expected);
    }

    #[tokio::test]
    async fn a_web_ui_job_is_offered_only_dismiss_job() {
        let (acq, _) = Acquire::for_alice().await;
        let job = failed_web_ui_job(&acq).await;
        let (view, untried) = read(&acq, &job).await;
        assert_eq!(
            serde_json::to_value(next_calls(&view, &untried, false)).unwrap(),
            json!([{"tool": "dismiss_job", "args": {"job_id": job}}])
        );
    }

    /// slskd's text for a peer whose upload queue is full (Application.cs:763-766), which says to
    /// try the same peer later.
    async fn busy_job(acq: &Acquire) -> String {
        let busy = Failure::new(
            FailureCode::TransferRejectedPeerBusy,
            Stage::Transfer,
            RetryAdvice::SameSourceLater,
            "Transfer rejected: Too many files",
        )
        .with_retry_after(600);
        failed_track_job_with(acq, "Coventry Carol", busy).await
    }

    #[tokio::test]
    async fn a_peer_busy_failure_offers_the_same_source_later_with_its_wait() {
        let (acq, _) = Acquire::for_alice().await;
        let job = busy_job(&acq).await;
        let (view, untried) = read(&acq, &job).await;
        let next = next_calls(&view, &untried, false);
        let same = next
            .iter()
            .find(|n| n.tool == "retry_job" && n.args["mode"] == "same_source")
            .expect("same_source is offered");
        assert_eq!(same.facts["retry_after_seconds"], 600);
        let other = next
            .iter()
            .find(|n| n.args["mode"] == "next_source")
            .unwrap();
        assert_eq!(
            (
                &other.facts["untried_candidates"],
                &other.facts["best_candidate"]["source"]
            ),
            (&json!(1), &json!("peer_b"))
        );
    }

    #[tokio::test]
    async fn a_stranded_job_is_offered_no_wait() {
        let (acq, _) = Acquire::for_alice().await;
        let job = downloading_track_job(&acq).await;
        let mut view = view_of(&acq, &job).await;
        let waits = |view: &JobView| {
            (next_calls(view, &Untried::default(), false).iter())
                .any(|call| call.tool == "get_jobs")
        };
        assert!(waits(&view));
        // Its failure could not be written: nothing changes it before the next start.
        view.stranded = true;
        assert!(!waits(&view));
    }

    #[tokio::test]
    async fn a_stranded_job_cancelled_and_retried_is_offered_its_wait_again() {
        let (acq, _) = Acquire::for_alice().await;
        let job = queued_track_jobs(&acq, 1).await.remove(0);
        let waits = |view: &JobView| {
            (next_calls(view, &Untried::default(), false).iter())
                .any(|call| call.tool == "get_jobs")
        };
        acq.ledger.strand(&job);
        assert!(!waits(&view_of(&acq, &job).await));
        // cancel_job, then retry_job same_source: the retry starts a runner for it.
        let picks = store::queued_picks(&acq.db, &job).await.unwrap();
        let user = UserId::new("alice");
        for change in [
            Transition::Cancel,
            Transition::Queue {
                items: picks,
                search_id: None,
            },
        ] {
            let done = acq.ledger.transition(&user, &job, change).await.unwrap();
            assert!(matches!(done, Transitioned::Applied { .. }), "{done:?}");
        }
        let view = view_of(&acq, &job).await;
        assert_eq!((view.job.state, view.stranded), (JobState::Queued, false));
        assert!(waits(&view));
    }

    fn preview_of(songs: &[&str]) -> NextCall {
        NextCall::new(
            "preview_delete_tracks",
            json!({"song_ids": songs, "reason": "replaced"}),
        )
    }

    #[tokio::test]
    async fn a_rejected_result_offers_its_files_deletion_only_when_the_app_may_delete() {
        let (acq, _) = Acquire::for_alice().await;
        let job = downloading_track_job(&acq).await;
        settle_done(&acq, &job, "s1").await;
        let ctx = crate::core::test_ctx("alice");
        retry(&acq, &ctx, &job, RetryMode::RejectResult, None)
            .await
            .unwrap();
        let (view, untried) = read(&acq, &job).await;
        assert_eq!(view.job.state, JobState::Failed);
        let offer = preview_of(&["s1"]);
        assert!(next_calls(&view, &untried, true).contains(&offer));
        assert!(!next_calls(&view, &untried, false).contains(&offer));
    }

    #[tokio::test]
    async fn a_rejected_result_that_lists_no_song_offers_no_deletion() {
        let (acq, _) = Acquire::for_alice().await;
        let job = done_track_job(&acq).await;
        let ctx = crate::core::test_ctx("alice");
        retry(&acq, &ctx, &job, RetryMode::RejectResult, None)
            .await
            .unwrap();
        let (view, untried) = read(&acq, &job).await;
        assert_eq!(
            (
                view.slots[0].state,
                slot_failure(&view.slots[0]).map(|f| f.code)
            ),
            (JobState::Failed, Some(FailureCode::ResultRejected))
        );
        let next = next_calls(&view, &untried, true);
        assert!(
            next.iter().all(|c| c.tool != "preview_delete_tracks"),
            "{next:?}"
        );
    }

    #[tokio::test]
    async fn a_done_job_offers_the_copies_it_did_not_replace() {
        let (acq, _) = Acquire::for_alice().await;
        let job = done_track_job(&acq).await;
        let mut view = view_of(&acq, &job).await;
        let Some(SlotResult::Imported { not_replaced, .. }) = view.slots[0].result.as_mut() else {
            panic!("{:?}", view.slots[0].result)
        };
        *not_replaced = vec!["s2".into()];
        let untried = untried(&acq.db, &view, chrono::Utc::now().timestamp())
            .await
            .unwrap();
        let offer = preview_of(&["s2"]);
        assert!(next_calls(&view, &untried, true).contains(&offer));
        assert!(!next_calls(&view, &untried, false).contains(&offer));
    }

    #[tokio::test]
    async fn the_leftovers_are_offered_once_each_one_tokens_songs_a_call() {
        let (acq, _) = Acquire::for_alice().await;
        let job = done_track_job(&acq).await;
        let mut view = view_of(&acq, &job).await;
        let Some(SlotResult::Imported { not_replaced, .. }) = view.slots[0].result.as_mut() else {
            panic!("{:?}", view.slots[0].result)
        };
        *not_replaced = (0..SONGS_PER_TOKEN + 5).map(|i| format!("s{i}")).collect();
        not_replaced.push("s0".into());
        let next = next_calls(&view, &Untried::default(), true);
        let offered: Vec<Vec<String>> = (next.iter())
            .filter(|c| c.tool == "preview_delete_tracks")
            .map(|c| serde_json::from_value(c.args["song_ids"].clone()).unwrap())
            .collect();
        let all: Vec<String> = (0..SONGS_PER_TOKEN + 5).map(|i| format!("s{i}")).collect();
        assert_eq!(
            offered,
            vec![
                all[..SONGS_PER_TOKEN].to_vec(),
                all[SONGS_PER_TOKEN..].to_vec()
            ]
        );
    }
}
