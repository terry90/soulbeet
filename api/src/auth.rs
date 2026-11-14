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

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq)]
pub struct AuthResponse {
    pub username: String,
    pub user_id: String,
    #[serde(default)]
    pub navidrome_status: NavidromeStatus,
    #[serde(default)]
    pub role: Role,
}

#[cfg(feature = "server")]
use crate::config::CONFIG;
#[cfg(feature = "server")]
use jsonwebtoken::{decode, encode, DecodingKey, EncodingKey, Header, Validation};

pub static EXPIRATION_DAYS: i64 = 30;

#[derive(Debug, Serialize, Deserialize)]
pub struct Claims {
    pub sub: String, // user_id
    /// The user's credentials version when the token was issued.
    pub ver: i64,
    pub iat: usize,
    pub exp: usize,
}

#[cfg(feature = "server")]
pub fn create_token(user_id: &str, credentials_version: i64) -> Result<String, String> {
    let encoding_key = EncodingKey::from_secret(CONFIG.secret_key().as_bytes());
    let now = chrono::Utc::now();
    let iat = now.timestamp() as usize;

    let exp = now
        .checked_add_signed(chrono::Duration::days(EXPIRATION_DAYS))
        .expect("valid timestamp")
        .timestamp();

    let claims = Claims {
        sub: user_id.to_string(),
        ver: credentials_version,
        iat,
        exp: exp as usize,
    };

    let token = encode(&Header::default(), &claims, &encoding_key).map_err(|e| e.to_string())?;

    Ok(token)
}

#[cfg(feature = "server")]
pub fn verify_token(token: &str) -> Result<Claims, String> {
    let token_data = decode::<Claims>(
        token,
        &DecodingKey::from_secret(CONFIG.secret_key().as_bytes()),
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
        let claims = verify_token(&create_token("id-a", 7).unwrap()).unwrap();
        assert_eq!((claims.sub.as_str(), claims.ver), ("id-a", 7));
    }
}
