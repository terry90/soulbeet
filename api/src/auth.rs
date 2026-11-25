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

    pub fn as_str(&self) -> &str {
        &self.0
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
    /// The account still has the default password, so the session can do
    /// nothing but choose a new one.
    #[serde(default)]
    pub password_change_required: bool,
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
    /// An account of that name exists and was never linked, and it is an
    /// admin account or OIDC_LINK_BY_USERNAME is off: its owner links it from
    /// Settings after signing in with its password
    AccountExists,
    /// The account of that name is linked to another identity
    Conflict,
    /// Single sign-on is not configured
    Disabled,
}

impl SsoError {
    const ALL: [SsoError; 6] = [
        SsoError::Failed,
        SsoError::Denied,
        SsoError::NotRegistered,
        SsoError::AccountExists,
        SsoError::Conflict,
        SsoError::Disabled,
    ];

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

    /// The login page that shows this error, on the way to `return_to`, an
    /// app's authorize request, when the sign-in was for one.
    pub fn location(self, return_to: Option<&str>) -> String {
        match return_to {
            None => format!("/login/error/{}", self.code()),
            Some(back) => format!(
                "/login/error/{}/return?{}",
                self.code(),
                shared::oauth::return_to_query(back)
            ),
        }
    }

    /// The error a code names; an unknown code is a failure.
    pub fn from_code(code: &str) -> Self {
        Self::ALL
            .into_iter()
            .find(|e| e.code() == code)
            .unwrap_or(SsoError::Failed)
    }

    pub fn message(self) -> &'static str {
        match self {
            SsoError::Failed => "Single sign-on failed. The server log has the details.",
            SsoError::Denied => "Single sign-on was cancelled or refused by the provider.",
            SsoError::NotRegistered => {
                "No Soulbeet account is linked to this single sign-on identity. Sign in with \
                 your password first, then link single sign-on in Settings > Users."
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

/// The password that proves an account is the signed-in person's before
/// they link it to single sign-on or change its password: the one the login
/// form signs in to it with.
#[derive(Debug, Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Default)]
pub enum PasswordProof {
    /// Its own password (for an account linked to Navidrome, the Navidrome
    /// password, which its own follows)
    #[default]
    Password,
    /// It has no password of its own, only a stored Navidrome login
    NavidromePassword,
    /// It has neither: an admin sets a password first
    NoPassword,
}

/// Why an account without a password can neither link single sign-on nor
/// set a password itself, and what the password an admin sets costs it.
pub const NO_PASSWORD: &str = "This account has no password. Ask an admin to set one in \
     Settings > Users. If single sign-on is linked, that removes the link, so link it again \
     from Settings after.";

/// What the owner of an account linked to single sign-on can do before
/// unlinking it, when it has no password (see `PasswordProof::NoPassword`):
/// connect Navidrome while it is set up (`navidrome`), or get a password
/// from an admin, which removes the link itself.
pub fn sso_unlink_advice(navidrome: bool) -> &'static str {
    if navidrome {
        "To unlink single sign-on, connect Navidrome first, or ask an admin to set a \
         password, which also removes the link."
    } else {
        "To unlink single sign-on, ask an admin to set a password, which also removes the link."
    }
}

/// How the signed-in user can sign in, for Settings.
#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Default)]
pub struct SignInMethods {
    /// Name of the OpenID Connect provider, when single sign-on is set up
    pub sso_provider: Option<String>,
    /// The provider's name for the identity the account is linked to, when
    /// it is linked
    pub sso_name: Option<String>,
    /// What linking it or changing its password asks for. Without one
    /// (`PasswordProof::NoPassword`) the account has no way in but the
    /// provider, so its owner cannot unlink it.
    pub password_proof: PasswordProof,
    /// `NAVIDROME_URL` is set, so the account can connect Navidrome
    pub navidrome: bool,
}

#[cfg(test)]
mod sso_error_tests {
    use super::*;

    #[test]
    fn codes_name_one_error_and_unknown_ones_fail() {
        for error in SsoError::ALL {
            assert_eq!(SsoError::from_code(error.code()), error);
        }
        // A typed or stale /login/error address still says something failed.
        assert_eq!(SsoError::from_code("nonsense"), SsoError::Failed);
    }

    #[test]
    fn a_user_without_a_linked_account_is_sent_to_link_it() {
        for error in [SsoError::NotRegistered, SsoError::AccountExists] {
            let message = error.message();
            assert!(message.contains("Sign in with"), "{message}");
            assert!(
                message.ends_with("then link single sign-on in Settings > Users."),
                "{message}"
            );
        }
    }

    #[test]
    fn an_error_keeps_the_app_request_the_sign_in_was_for() {
        assert_eq!(SsoError::Denied.location(None), "/login/error/denied");
        let back = "/oauth/authorize?client_id=c&state=a%26b";
        assert_eq!(
            SsoError::Denied.location(Some(back)),
            format!(
                "/login/error/denied/return?{}",
                shared::oauth::return_to_query(back)
            )
        );
    }
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
