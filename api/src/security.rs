//! Conditions that keep the MCP endpoint closed until the instance is safe to expose.

use serde::{Deserialize, Serialize};

#[cfg(feature = "server")]
use std::sync::atomic::{AtomicBool, Ordering};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GateCode {
    WeakSecretKey,
    DefaultAdminPassword,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct GateReason {
    pub code: GateCode,
    pub message: String,
}

/// What Settings shows about the instance's exposure.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SecurityStatus {
    pub mcp_blocked_by: Vec<GateReason>,
    /// Why every sign-in is refused, when the configuration makes it so.
    pub sign_in_problem: Option<String>,
    /// Why `MCP` switches Claude (MCP) access off; None when it is on.
    #[serde(default)]
    pub mcp_off: Option<String>,
}

#[cfg(feature = "server")]
pub const SEEDED_ADMIN_ID: &str = "00000000-0000-0000-0000-000000000000";

/// True until the startup check or a password change proves otherwise, so an
/// unchecked state blocks.
#[cfg(feature = "server")]
static SEEDED_ADMIN_HAS_DEFAULT_PASSWORD: AtomicBool = AtomicBool::new(true);

#[cfg(feature = "server")]
const WEAK_SECRET_KEY: &str = "SECRET_KEY is shorter than 32 bytes. Set it to a random value \
     of at least 32 bytes, for example the output of `openssl rand -hex 32`, or remove it so \
     Soulful generates one. Changing SECRET_KEY ends every session and makes the stored \
     Navidrome passwords unreadable: each user is asked to sign in again once.";

/// The reasons the instance is not safe to expose. Empty means open. Never hashes.
#[cfg(feature = "server")]
pub fn gate_reasons() -> Vec<GateReason> {
    gate_reasons_from(
        crate::secret_key::current().is_weak(),
        seeded_admin_has_default_password(),
    )
}

#[cfg(feature = "server")]
fn seeded_admin_has_default_password() -> bool {
    SEEDED_ADMIN_HAS_DEFAULT_PASSWORD.load(Ordering::Relaxed)
}

#[cfg(feature = "server")]
pub(crate) fn gate_reasons_from(weak_key: bool, default_admin_password: bool) -> Vec<GateReason> {
    let mut reasons = Vec::new();
    if weak_key {
        reasons.push(GateReason {
            code: GateCode::WeakSecretKey,
            message: WEAK_SECRET_KEY.to_string(),
        });
    }
    if default_admin_password {
        reasons.push(GateReason {
            code: GateCode::DefaultAdminPassword,
            message: "The seeded admin account still has the password 'admin'. Change it in Settings > Users."
                .to_string(),
        });
    }
    reasons
}

#[cfg(feature = "server")]
pub(crate) fn seeded_hash_is_default(hash: &str) -> bool {
    use argon2::{
        password_hash::{PasswordHash, PasswordVerifier},
        Argon2,
    };
    PasswordHash::new(hash)
        .map(|parsed| Argon2::default().verify_password(b"admin", &parsed).is_ok())
        .unwrap_or(false)
}

/// Startup check of the seeded admin row. A database error leaves the flag set.
#[cfg(feature = "server")]
pub async fn check_seeded_admin() {
    let hash: Result<Option<String>, _> =
        sqlx::query_scalar("SELECT password_hash FROM users WHERE id = ?")
            .bind(SEEDED_ADMIN_ID)
            .fetch_optional(&*crate::db::DB)
            .await;
    match hash {
        Ok(hash) => {
            let default = hash.is_some_and(|h| seeded_hash_is_default(&h));
            SEEDED_ADMIN_HAS_DEFAULT_PASSWORD.store(default, Ordering::Relaxed);
        }
        Err(e) => tracing::error!("Could not check the seeded admin password: {e}"),
    }
}

#[cfg(feature = "server")]
pub fn note_password_set(user_id: &str, new_password: &str) {
    if user_id == SEEDED_ADMIN_ID {
        SEEDED_ADMIN_HAS_DEFAULT_PASSWORD.store(new_password == "admin", Ordering::Relaxed);
    }
}

#[cfg(feature = "server")]
pub fn note_user_deleted(user_id: &str) {
    if user_id == SEEDED_ADMIN_ID {
        SEEDED_ADMIN_HAS_DEFAULT_PASSWORD.store(false, Ordering::Relaxed);
    }
}

#[cfg(all(test, feature = "server"))]
mod tests {
    use super::*;
    use argon2::{
        password_hash::{rand_core::OsRng, PasswordHasher, SaltString},
        Argon2,
    };

    #[test]
    fn gate_lists_each_reason() {
        assert!(gate_reasons_from(false, false).is_empty());
        let r = gate_reasons_from(true, true);
        assert_eq!(
            r.iter().map(|g| g.code).collect::<Vec<_>>(),
            vec![GateCode::WeakSecretKey, GateCode::DefaultAdminPassword]
        );
        assert!(r[0]
            .message
            .contains("stored Navidrome passwords unreadable"));
    }

    #[test]
    fn the_seeded_hash_is_the_default_password() {
        assert!(seeded_hash_is_default(
            "$argon2id$v=19$m=19456,t=2,p=1$llsT7N68SnCXwaqcvFP08g$W+5l4cDaOfsY9nK2jFs7JGwkxtVtmN+VLIWC7ZOM9/E"
        ));
        let changed = Argon2::default()
            .hash_password(b"changed", &SaltString::generate(&mut OsRng))
            .unwrap()
            .to_string();
        assert!(!seeded_hash_is_default(&changed));
    }

    #[test]
    fn setting_the_seeded_password_updates_the_flag() {
        note_password_set(SEEDED_ADMIN_ID, "changed");
        assert!(!seeded_admin_has_default_password());
        note_password_set("someone-else", "admin");
        assert!(!seeded_admin_has_default_password());
    }
}
