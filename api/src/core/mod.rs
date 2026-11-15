//! The logic behind the server functions, as plain functions of the user
//! they act for, failing with a typed `Failure`. The web UI's server
//! functions and the MCP server call the same code.

use shared::failure::{Failure, FailureCode, RetryAdvice, Service, Stage};

pub mod folders;
pub mod health;
pub mod settings;

/// Soulful's own database failed.
pub(crate) fn database_failure(stage: Stage, e: impl std::fmt::Display) -> Failure {
    Failure::new(
        FailureCode::SettingsUnreadable,
        stage,
        RetryAdvice::SameSourceLater,
        "Soulful's database is unavailable",
    )
    .with_cause(Service::Database, None, e.to_string())
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
    use shared::failure::Cause;

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
                "Soulful's database is unavailable"
            )
        );
        assert_eq!(f.cause, Some(Cause::new(Service::Database, None, raw)));
    }
}
