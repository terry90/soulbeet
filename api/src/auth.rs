use serde::{Deserialize, Serialize};
use shared::system::NavidromeStatus;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[cfg_attr(
    feature = "server",
    derive(sqlx::Type),
    sqlx(type_name = "TEXT", rename_all = "lowercase")
)]
pub enum Role {
    Admin,
    #[default]
    User,
}

impl Role {
    pub fn is_admin(self) -> bool {
        self == Role::Admin
    }
}

/// Who a request acts for, read from the user's row on every request.
#[derive(Clone, Debug, PartialEq)]
pub struct UserCtx {
    pub user_id: String,
    pub username: String,
    pub role: Role,
}

/// A `users.id`, the only key of per-user runtime state. No `From<String>`
/// or `From<&str>`, so a username cannot be passed where an id is wanted.
#[cfg(feature = "server")]
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct UserId(String);

#[cfg(feature = "server")]
impl UserId {
    /// Only for values read from `users.id`.
    pub fn new(users_id: &str) -> UserId {
        UserId(users_id.to_string())
    }
}

#[cfg(feature = "server")]
impl std::fmt::Display for UserId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

#[cfg(feature = "server")]
impl UserCtx {
    pub fn id(&self) -> UserId {
        UserId::new(&self.user_id)
    }
}

/// Who can create accounts (`REGISTRATION`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RegistrationMode {
    /// Accounts appear when a Navidrome user signs in.
    Navidrome,
    /// Only an admin creates accounts.
    Admin,
    /// Anyone can register.
    Open,
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq)]
pub struct AuthResponse {
    pub username: String,
    pub user_id: String,
    #[serde(default)]
    pub navidrome_status: NavidromeStatus,
    #[serde(default)]
    pub role: Role,
    /// False while `NAVIDROME_URL` is set: usernames follow Navidrome.
    #[serde(default)]
    pub rename_allowed: bool,
}

/// Starts an OpenID Connect sign-in (a full page navigation, not a fetch).
pub const OIDC_LOGIN_PATH: &str = "/api/auth/oidc/login";
/// Where the OpenID Connect provider sends the browser back.
pub const OIDC_CALLBACK_PATH: &str = "/api/auth/oidc/callback";

/// How the login page offers to sign in.
#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Default)]
pub struct LoginOptions {
    /// Name of the OpenID Connect provider, when single sign-on is set up
    pub sso_provider: Option<String>,
}

/// Why a single sign-on attempt failed, passed to the login page as
/// `/login/error/<code>`. Details go to the server log, not to the browser.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SsoError {
    /// Anything the user cannot fix: see the server log
    Failed,
    /// The provider refused (the user cancelled, or is not allowed)
    Denied,
    /// No account of that name and auto-registration is off
    NotRegistered,
    /// The account of that name is linked to another identity
    Conflict,
    /// Single sign-on is not configured
    Disabled,
}

impl SsoError {
    pub fn code(self) -> &'static str {
        match self {
            SsoError::Failed => "failed",
            SsoError::Denied => "denied",
            SsoError::NotRegistered => "not_registered",
            SsoError::Conflict => "conflict",
            SsoError::Disabled => "disabled",
        }
    }

    pub fn from_code(code: &str) -> Option<Self> {
        [
            SsoError::Failed,
            SsoError::Denied,
            SsoError::NotRegistered,
            SsoError::Conflict,
            SsoError::Disabled,
        ]
        .into_iter()
        .find(|e| e.code() == code)
    }

    pub fn message(self) -> &'static str {
        match self {
            SsoError::Failed => "Single sign-on failed. The server log has the details.",
            SsoError::Denied => "Single sign-on was cancelled or refused by the provider.",
            SsoError::NotRegistered => {
                "No Soulbeet account for this user. Ask an admin to create it."
            }
            SsoError::Conflict => "This username is linked to another single sign-on identity.",
            SsoError::Disabled => "Single sign-on is not configured on this server.",
        }
    }
}

#[cfg(feature = "server")]
use crate::secret_key::SecretKey;
#[cfg(feature = "server")]
use jsonwebtoken::{decode, encode, DecodingKey, EncodingKey, Header, Validation};

pub static EXPIRATION_DAYS: i64 = 30;

#[derive(Debug, Serialize, Deserialize)]
pub struct Claims {
    pub sub: String, // user_id
    /// The user's credentials version when the token was issued.
    pub ver: i64,
    /// This token's own id, so signing out can end it alone.
    pub jti: String,
    pub iat: usize,
    pub exp: usize,
}

#[cfg(feature = "server")]
pub fn create_token(
    key: &SecretKey,
    user_id: &str,
    credentials_version: i64,
) -> Result<String, String> {
    let encoding_key = EncodingKey::from_secret(key.as_bytes());
    let now = chrono::Utc::now();
    let iat = now.timestamp() as usize;

    let exp = now
        .checked_add_signed(chrono::Duration::days(EXPIRATION_DAYS))
        .expect("valid timestamp")
        .timestamp();

    let claims = Claims {
        sub: user_id.to_string(),
        ver: credentials_version,
        jti: uuid::Uuid::new_v4().to_string(),
        iat,
        exp: exp as usize,
    };

    let token = encode(&Header::default(), &claims, &encoding_key).map_err(|e| e.to_string())?;

    Ok(token)
}

#[cfg(feature = "server")]
pub fn verify_token(key: &SecretKey, token: &str) -> Result<Claims, String> {
    let token_data = decode::<Claims>(
        token,
        &DecodingKey::from_secret(key.as_bytes()),
        &Validation::default(),
    )
    .map_err(|e| e.to_string())?;

    Ok(token_data.claims)
}

#[cfg(all(test, feature = "server"))]
mod tests {
    use super::*;

    #[test]
    fn tokens_carry_the_version_and_no_name() {
        let key = SecretKey::new("k".repeat(32));
        let claims = verify_token(&key, &create_token(&key, "id-a", 7).unwrap()).unwrap();
        assert_eq!((claims.sub.as_str(), claims.ver), ("id-a", 7));
    }

    #[test]
    fn each_token_has_its_own_id() {
        let key = SecretKey::new("k".repeat(32));
        let jti = || {
            verify_token(&key, &create_token(&key, "id-a", 0).unwrap())
                .unwrap()
                .jti
        };
        assert_ne!(jti(), jti());
    }

    #[test]
    fn a_token_signed_with_another_key_is_refused() {
        // An install that ran on the old built-in key gets a generated one.
        let old = SecretKey::new("secret".to_string());
        let new = SecretKey::new("n".repeat(64));
        assert!(verify_token(&new, &create_token(&old, "id-a", 0).unwrap()).is_err());
    }
}
