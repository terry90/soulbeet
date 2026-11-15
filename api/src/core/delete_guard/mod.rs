//! The one check every deletion of a music file goes through (design section 13).

use shared::failure::{Failure, FailureCode, RetryAdvice, Stage};

pub mod paths;

/// The song's file on this machine cannot be told: `message` says why.
pub(crate) fn path_unresolved(message: impl Into<String>) -> Failure {
    Failure::new(
        FailureCode::DeletePathUnresolved,
        Stage::Delete,
        RetryAdvice::NotRetryable,
        message,
    )
}
