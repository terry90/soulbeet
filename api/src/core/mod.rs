//! The logic behind the server functions, as plain functions of the user
//! they act for, failing with a typed `Failure`. The web UI's server
//! functions and the MCP server call the same code.

use crate::services::BackendLookupError;
use shared::failure::{Failure, FailureCode, RetryAdvice, Service, Stage};

pub mod acquire;
pub mod acquire_play;
pub mod archival;
pub mod backends;
pub mod browse;
pub mod connect;
pub mod contextual;
pub mod curation;
pub mod delete_guard;
pub mod deletion;
pub mod discovery;
pub mod folders;
pub mod health;
pub mod index;
pub mod insights;
pub mod jobs;
pub mod library;
pub mod metadata;
pub mod playlists;
pub mod ratings;
pub(crate) mod recommend;
pub mod reconcile;
pub(crate) mod scan;
pub mod settings;
pub mod sources;
pub(crate) mod time;
pub mod tracks;

/// Soulful's own database failed.
pub(crate) fn database_failure(stage: Stage, e: impl std::fmt::Display) -> Failure {
    Failure::new(
        FailureCode::SettingsUnreadable,
        stage,
        RetryAdvice::SameSourceLater,
        "Soulbeet's database is unavailable",
    )
    .with_cause(Service::Database, None, e.to_string())
}

/// A time as a failure message says it: `%Y-%m-%d %H:%M:%S UTC`.
pub(crate) fn utc_text(at: chrono::DateTime<chrono::Utc>) -> String {
    at.format("%Y-%m-%d %H:%M:%S UTC").to_string()
}

/// Unix time `at`, stored by Soulful, as a time; a database failure when it is out of chrono's
/// range, which no time Soulful writes reaches.
pub(crate) fn stored_time(stage: Stage, at: i64) -> Result<chrono::DateTime<chrono::Utc>, Failure> {
    chrono::DateTime::from_timestamp(at, 0)
        .ok_or_else(|| database_failure(stage, format!("stored time {at} is out of range")))
}

/// `f`'s message for a log line, followed by its upstream cause when it has one:
/// "{message} (cause: {service}[ {status}]: {raw})".
pub(crate) fn logged(f: &Failure) -> String {
    let Some(cause) = &f.cause else {
        return f.message.clone();
    };
    let service = match cause.service {
        Service::Slskd => "slskd",
        Service::Beets => "beets",
        Service::Navidrome => "navidrome",
        Service::Musicbrainz => "musicbrainz",
        Service::Filesystem => "filesystem",
        Service::Database => "database",
        Service::Soulful => "soulful",
        Service::InternetArchive => "internet_archive",
        Service::Youtube => "youtube",
        Service::GdStudio => "gd_studio",
    };
    let status = cause.status.map(|s| format!(" {s}")).unwrap_or_default();
    format!("{} (cause: {service}{status}: {})", f.message, cause.raw)
}

/// `e`'s own failure at `stage`, shown as "Download backend not available: {e}".
pub(crate) fn backend_unavailable(stage: Stage, e: &BackendLookupError) -> Failure {
    e.failure(stage, format!("Download backend not available: {e}"))
}

/// `e` with the code `backend_unavailable` gives it at SourceSearch, shown at `stage`: what an
/// acquisition's lookup failure is wherever it happens.
pub(crate) fn backend_unavailable_at(stage: Stage, e: &BackendLookupError) -> Failure {
    Failure {
        stage,
        ..backend_unavailable(Stage::SourceSearch, e)
    }
}

/// The import target cannot be written: `e` is the filesystem's error.
pub(crate) fn target_unwritable(e: &std::io::Error, message: impl Into<String>) -> Failure {
    Failure::new(
        FailureCode::ImportTargetUnwritable,
        Stage::Import,
        RetryAdvice::NotRetryable,
        message,
    )
    .with_cause(Service::Filesystem, None, e.to_string())
}

#[cfg(test)]
pub(crate) fn test_ctx(user_id: &str) -> crate::auth::UserCtx {
    crate::auth::UserCtx {
        user_id: user_id.to_string(),
        username: user_id.to_string(),
        role: crate::auth::Role::User,
    }
}

/// Makes `user` and one folder of theirs, and returns the folder's id.
#[cfg(test)]
pub(crate) async fn folder_of(db: &sqlx::SqlitePool, user: &str) -> String {
    sqlx::query("INSERT INTO users (id, username, password_hash) VALUES (?, ?, 'x')")
        .bind(user)
        .bind(user)
        .execute(db)
        .await
        .unwrap();
    let id = format!("{user}-folder");
    sqlx::query("INSERT INTO folders (id, user_id, name, path) VALUES (?, ?, 'f', ?)")
        .bind(&id)
        .bind(user)
        .bind(format!("/music/{user}"))
        .execute(db)
        .await
        .unwrap();
    id
}

#[cfg(test)]
mod tests {
    use super::*;
    use shared::failure::{Cause, UserActionCode};
    use soulbeet::error::{MissingSetting, SoulseekError};

    #[test]
    fn a_database_failure_keeps_the_database_text_as_its_cause() {
        let raw = "error returned from database: (code: 5) database is locked";
        let f = database_failure(Stage::Input, raw);
        assert_eq!(
            (f.code, f.stage, f.retry, f.message.as_str()),
            (
                FailureCode::SettingsUnreadable,
                Stage::Input,
                RetryAdvice::SameSourceLater,
                "Soulbeet's database is unavailable"
            )
        );
        assert_eq!(f.cause, Some(Cause::new(Service::Database, None, raw)));
    }

    #[test]
    fn a_logged_failure_carries_its_cause() {
        let raw = "error returned from database: (code: 5) database is locked";
        assert_eq!(
            logged(&database_failure(Stage::Delete, raw)),
            format!("Soulbeet's database is unavailable (cause: database: {raw})")
        );
        let with_status = Failure::new(
            FailureCode::NavidromeUnreachable,
            Stage::Delete,
            RetryAdvice::SameSourceLater,
            "Navidrome could not be read",
        )
        .with_cause(Service::Navidrome, Some(500), "Internal Server Error");
        assert_eq!(
            logged(&with_status),
            "Navidrome could not be read (cause: navidrome 500: Internal Server Error)"
        );
        let bare = Failure::new(
            FailureCode::UnknownHandle,
            Stage::Delete,
            RetryAdvice::NotRetryable,
            "Discovery track not found",
        );
        assert_eq!(logged(&bare), "Discovery track not found");
    }

    #[test]
    fn an_unavailable_backend_keeps_its_text_at_either_stage() {
        let unset =
            BackendLookupError::Backend(SoulseekError::NotConfigured(MissingSetting::Url).into());
        for stage in [Stage::SourceSearch, Stage::Enqueue] {
            let f = backend_unavailable(stage, &unset);
            assert_eq!(
                (f.code, f.stage, f.retry, f.message.as_str()),
                (
                    FailureCode::BackendNotConfigured,
                    stage,
                    RetryAdvice::NeedsUser,
                    "Download backend not available: slskd URL not configured"
                )
            );
            assert_eq!(
                f.user_action.map(|a| a.code),
                Some(UserActionCode::ConfigureBackend)
            );
            assert_eq!(
                f.cause.map(|c| c.raw),
                Some("slskd URL not configured".to_string())
            );
        }
    }

    #[test]
    fn a_lookup_shown_at_another_stage_keeps_the_search_code() {
        let down = BackendLookupError::Backend(
            SoulseekError::Api {
                status: 502,
                message: "Bad Gateway".into(),
            }
            .into(),
        );
        let f = backend_unavailable_at(Stage::Enqueue, &down);
        assert_eq!(
            (f.code, f.stage, f.retry, f.message.as_str()),
            (
                FailureCode::BackendUnreachable,
                Stage::Enqueue,
                RetryAdvice::SameSourceLater,
                "Download backend not available: API error: 502 - Bad Gateway"
            )
        );
        assert_eq!(
            backend_unavailable(Stage::Enqueue, &down).code,
            FailureCode::EnqueueFailed
        );
    }
}
