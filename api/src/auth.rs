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
    /// An account of that name exists and was never linked: its owner links
    /// it from Settings after signing in with its password
    AccountExists,
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
            SsoError::AccountExists => "account_exists",
            SsoError::Conflict => "conflict",
            SsoError::Disabled => "disabled",
        }
    }

    pub fn from_code(code: &str) -> Option<Self> {
        [
            SsoError::Failed,
            SsoError::Denied,
            SsoError::NotRegistered,
            SsoError::AccountExists,
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
                "No Soulbeet account for this user yet. Sign in with your password first: your \
                 Navidrome password if this server uses Navidrome, otherwise the one an admin \
                 gave you. Then link single sign-on in Settings > Users."
            }
            SsoError::AccountExists => {
                "A Soulbeet account with this username already exists. Sign in with its \
                 password, then link single sign-on in Settings > Users."
            }
            SsoError::Conflict => "This username is linked to another single sign-on identity.",
            SsoError::Disabled => "Single sign-on is not configured on this server.",
        }
    }
}

/// How linking single sign-on from Settings ended, passed to Settings as
/// `/settings/sso/<code>`. Details go to the server log.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SsoLinkResult {
    Linked,
    /// The account already has an identity
    AlreadyLinked,
    /// The identity belongs to another account
    IdentityTaken,
    /// This browser is no longer signed in to the account that started it
    SessionChanged,
    /// The provider refused (the user cancelled, or is not allowed)
    Denied,
    /// Anything the user cannot fix: see the server log
    Failed,
}

impl SsoLinkResult {
    const ALL: [SsoLinkResult; 6] = [
        SsoLinkResult::Linked,
        SsoLinkResult::AlreadyLinked,
        SsoLinkResult::IdentityTaken,
        SsoLinkResult::SessionChanged,
        SsoLinkResult::Denied,
        SsoLinkResult::Failed,
    ];

    pub fn code(self) -> &'static str {
        match self {
            SsoLinkResult::Linked => "linked",
            SsoLinkResult::AlreadyLinked => "already_linked",
            SsoLinkResult::IdentityTaken => "identity_taken",
            SsoLinkResult::SessionChanged => "session_changed",
            SsoLinkResult::Denied => "denied",
            SsoLinkResult::Failed => "failed",
        }
    }

    /// The result a code names; an unknown code is a failure.
    pub fn from_code(code: &str) -> Self {
        Self::ALL
            .into_iter()
            .find(|r| r.code() == code)
            .unwrap_or(SsoLinkResult::Failed)
    }

    /// Where the provider's callback sends the browser.
    pub fn location(self) -> String {
        format!("/settings/sso/{}", self.code())
    }

    pub fn message(self) -> &'static str {
        match self {
            SsoLinkResult::Linked => "Single sign-on is linked to this account.",
            SsoLinkResult::AlreadyLinked => {
                "This account is already linked to single sign-on. Unlink it first to link \
                 another identity."
            }
            SsoLinkResult::IdentityTaken => {
                "This single sign-on identity is already linked to another Soulbeet account."
            }
            SsoLinkResult::SessionChanged => {
                "Nothing was linked: this browser is no longer signed in to the account that \
                 started the link. Sign in and try again."
            }
            SsoLinkResult::Denied => "Linking was cancelled or refused by the provider.",
            SsoLinkResult::Failed => {
                "Linking single sign-on failed. The server log has the details."
            }
        }
    }
}

/// How the signed-in user can sign in, for Settings.
#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Default)]
pub struct SignInMethods {
    /// Name of the OpenID Connect provider, when single sign-on is set up
    pub sso_provider: Option<String>,
    /// The account is linked to an identity at the provider
    pub sso_linked: bool,
    /// The account keeps a way in without the provider, so its owner may
    /// unlink it
    pub sso_unlink_allowed: bool,
    /// `NAVIDROME_URL` is set, so the account can connect Navidrome
    pub navidrome: bool,
}

#[cfg(test)]
mod sso_link_result_tests {
    use super::*;

    #[test]
    fn codes_name_one_result_and_unknown_ones_fail() {
        for result in SsoLinkResult::ALL {
            assert_eq!(SsoLinkResult::from_code(result.code()), result);
        }
        assert_eq!(SsoLinkResult::from_code("nonsense"), SsoLinkResult::Failed);
        assert_eq!(SsoLinkResult::Linked.location(), "/settings/sso/linked");
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
