//! Fixtures the acquire modules' tests share.

use crate::auth::UserCtx;
use crate::core::test_ctx;
use crate::transfer_registry::BatchRef;

pub(super) fn ctx_of(batch: &BatchRef) -> UserCtx {
    test_ctx(&batch.owner.user_id.to_string())
}

/// A user of its own, so tests sharing the process-wide registry never
/// see each other's claims.
pub(super) fn own_ctx() -> UserCtx {
    test_ctx(&format!("tester-{}", uuid::Uuid::new_v4()))
}
