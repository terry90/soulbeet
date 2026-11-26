//! Turning download backends on and off.

use crate::auth::UserCtx;
use crate::core::database_failure;
use crate::models::app_config::{keys, AppConfig};
use crate::services::{reload_download_backend, stop_backend, turned_on};
use async_trait::async_trait;
use shared::backends::{BackendMeta, BackendSwitch, Switch};
use shared::failure::{Failure, FailureCode, RetryAdvice, Stage};
use sqlx::SqlitePool;

/// What turning a backend on or off does to the running server.
#[async_trait]
pub(crate) trait Lifecycle: Send + Sync {
    /// Stops the backend's searches and unfinished transfers, deleting their partial files and
    /// forgetting their transfer book entries.
    async fn stop(&self, id: &str);
    /// Drops the built backend, so the next lookup reads its switch again.
    async fn reload(&self, id: &str);
}

struct Server;

#[async_trait]
impl Lifecycle for Server {
    async fn stop(&self, id: &str) {
        stop_backend(id).await;
    }
    async fn reload(&self, id: &str) {
        reload_download_backend(id).await;
    }
}

/// Turns the backend `id` on or off, for an admin. Off deletes its switch, drops it, so every
/// later lookup reads the switch, then stops what it runs: batches still on it then fail through
/// the monitor's lookup, and finished files stay where they are.
pub async fn set_enabled(ctx: &UserCtx, id: &str, enabled: bool) -> Result<(), Failure> {
    set_enabled_in(
        &crate::db::DB,
        shared::backends::ALL,
        &Server,
        ctx,
        id,
        enabled,
    )
    .await
}

fn refused(message: String) -> Failure {
    Failure::new(
        FailureCode::InvalidInput,
        Stage::Input,
        RetryAdvice::NotRetryable,
        message,
    )
}

pub(crate) async fn set_enabled_in(
    db: &SqlitePool,
    all: &'static [BackendMeta],
    life: &dyn Lifecycle,
    ctx: &UserCtx,
    id: &str,
    enabled: bool,
) -> Result<(), Failure> {
    if !ctx.role.is_admin() {
        return Err(Failure::new(
            FailureCode::PermissionNotGranted,
            Stage::Input,
            RetryAdvice::NotRetryable,
            crate::server_fns::guard::ADMIN_ONLY,
        ));
    }
    let meta = (all.iter())
        .find(|m| m.id == id)
        .ok_or_else(|| refused(format!("Unknown download backend: {id}")))?;
    if meta.switch == Switch::AlwaysOn {
        return Err(refused(format!("{} cannot be turned off", meta.name)));
    }
    let key = keys::backend_enabled(id);
    if enabled {
        AppConfig::set_in(db, &key, "true")
            .await
            .map_err(|e| database_failure(Stage::Input, e))?;
    } else {
        AppConfig::delete_in(db, &key)
            .await
            .map_err(|e| database_failure(Stage::Input, e))?;
    }
    // The reload waits for a build in flight, so the stop then covers whatever it started.
    life.reload(id).await;
    if !enabled {
        life.stop(id).await;
    }
    Ok(())
}

/// Every backend an admin can turn on, and whether it is on.
pub async fn switches() -> Result<Vec<BackendSwitch>, Failure> {
    switches_in(&crate::db::DB, shared::backends::ALL).await
}

pub(crate) async fn switches_in(
    db: &SqlitePool,
    all: &'static [BackendMeta],
) -> Result<Vec<BackendSwitch>, Failure> {
    let mut switches = Vec::new();
    for meta in all.iter().filter(|m| m.switch == Switch::AdminToggle) {
        let enabled = turned_on(db, meta.id)
            .await
            .map_err(|e| database_failure(Stage::Input, e))?;
        switches.push(BackendSwitch {
            id: meta.id.to_string(),
            enabled,
        });
    }
    Ok(switches)
}

/// Every download backend currently enabled (always-on or admin-toggled on in `app_config`).
pub async fn enabled() -> Result<Vec<BackendMeta>, Failure> {
    enabled_in(&crate::db::DB, shared::backends::ALL).await
}

pub(crate) async fn enabled_in(
    db: &SqlitePool,
    all: &'static [BackendMeta],
) -> Result<Vec<BackendMeta>, Failure> {
    let mut out = Vec::new();
    for meta in all {
        let is_on = match meta.switch {
            Switch::AlwaysOn => true,
            Switch::AdminToggle => turned_on(db, meta.id)
                .await
                .map_err(|e| database_failure(Stage::Input, e))?,
        };
        if is_on {
            out.push(*meta);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::Role;
    use crate::db::test_db;
    use std::sync::Mutex;

    const ARCHIVE: BackendMeta = BackendMeta {
        id: "archive",
        name: "Internet Archive",
        service: "archive.org",
        switch: Switch::AdminToggle,
        ..shared::backends::SLSKD
    };
    static METAS: &[BackendMeta] = &[shared::backends::SLSKD, ARCHIVE];

    #[derive(Default)]
    struct Recorded(Mutex<Vec<String>>);

    #[async_trait]
    impl Lifecycle for Recorded {
        async fn stop(&self, id: &str) {
            self.0.lock().unwrap().push(format!("stop {id}"));
        }
        async fn reload(&self, id: &str) {
            self.0.lock().unwrap().push(format!("reload {id}"));
        }
    }

    fn user(role: Role) -> UserCtx {
        UserCtx {
            user_id: "u1".into(),
            username: "alice".into(),
            role,
        }
    }

    #[tokio::test]
    async fn turning_a_backend_off_deletes_its_switch_then_drops_it_then_stops_it() {
        let db = test_db::migrated().await;
        let life = Recorded::default();
        let admin = user(Role::Admin);

        set_enabled_in(&db, METAS, &life, &admin, "archive", true)
            .await
            .unwrap();
        assert_eq!(
            switches_in(&db, METAS).await.unwrap(),
            vec![BackendSwitch {
                id: "archive".into(),
                enabled: true
            }]
        );
        assert_eq!(*life.0.lock().unwrap(), ["reload archive"]);

        set_enabled_in(&db, METAS, &life, &admin, "archive", false)
            .await
            .unwrap();
        assert_eq!(
            AppConfig::get_in(&db, "archive_enabled").await.unwrap(),
            None
        );
        assert_eq!(
            *life.0.lock().unwrap(),
            ["reload archive", "reload archive", "stop archive"]
        );
        assert!(!switches_in(&db, METAS).await.unwrap()[0].enabled);
    }

    #[tokio::test]
    async fn only_an_admin_turns_a_backend_and_never_one_always_on() {
        let db = test_db::migrated().await;
        let life = Recorded::default();
        let f = set_enabled_in(&db, METAS, &life, &user(Role::User), "archive", true)
            .await
            .unwrap_err();
        assert_eq!(f.code, FailureCode::PermissionNotGranted);
        assert_eq!(f.message, "Only an admin can do this");

        let admin = user(Role::Admin);
        let f = set_enabled_in(&db, METAS, &life, &admin, "slskd", false)
            .await
            .unwrap_err();
        assert_eq!(
            (f.code, f.message.as_str()),
            (FailureCode::InvalidInput, "Soulseek cannot be turned off")
        );
        let f = set_enabled_in(&db, METAS, &life, &admin, "nicotine", true)
            .await
            .unwrap_err();
        assert_eq!(f.message, "Unknown download backend: nicotine");
        assert!(life.0.lock().unwrap().is_empty());
        assert_eq!(
            AppConfig::get_in(&db, "archive_enabled").await.unwrap(),
            None
        );
    }

    #[tokio::test]
    async fn enabled_lists_always_on_and_toggled_on_backends() {
        let db = test_db::migrated().await;
        let life = Recorded::default();
        let admin = user(Role::Admin);

        assert_eq!(
            enabled_in(&db, METAS).await.unwrap(),
            vec![shared::backends::SLSKD]
        );

        set_enabled_in(&db, METAS, &life, &admin, "archive", true)
            .await
            .unwrap();
        assert_eq!(
            enabled_in(&db, METAS).await.unwrap(),
            vec![shared::backends::SLSKD, ARCHIVE]
        );

        set_enabled_in(&db, METAS, &life, &admin, "archive", false)
            .await
            .unwrap();
        assert_eq!(
            enabled_in(&db, METAS).await.unwrap(),
            vec![shared::backends::SLSKD]
        );
    }
}
