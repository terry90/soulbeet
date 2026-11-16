//! D11: the calls a job's `next` offers, built in this one place from the job's state.

use serde_json::json;

use super::contract::NextCall;
use crate::core::jobs::JobView;
use crate::ledger::JobState;

/// The calls that can be made for the job now: while it runs, a wait for its next change.
pub(crate) fn next_calls(view: &JobView) -> Vec<NextCall> {
    use JobState::*;
    match view.job.state {
        Searching | Queued | Downloading | Importing | Indexing => vec![NextCall::new(
            "get_jobs",
            json!({"job_ids": [view.job.id], "detail": "full", "wait_seconds": 60,
                "wait_for": "any_change"}),
        )],
        NeedsSourceChoice | NeedsDecision | Done | Failed | Cancelled | Dismissed => vec![],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::jobs::runner::Acquire;
    use crate::core::jobs::test_support::{downloading_track_job, test_db_with_alice, view_of};

    #[tokio::test]
    async fn a_running_job_offers_only_a_wait() {
        let (acq, _) = Acquire::for_test(test_db_with_alice().await);
        let job = downloading_track_job(&acq).await;
        let view = view_of(&acq, &job).await;
        let next = next_calls(&view);
        assert_eq!(
            serde_json::to_value(&next).unwrap(),
            json!([{"tool": "get_jobs",
                "args": {"job_ids": [job], "detail": "full", "wait_seconds": 60, "wait_for": "any_change"}}])
        );
    }
}
