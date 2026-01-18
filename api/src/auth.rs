use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq)]
pub struct AuthResponse {
    pub username: String,
    pub user_id: String,
}

#[cfg(feature = "server")]
use jsonwebtoken::{decode, encode, DecodingKey, EncodingKey, Header, Validation};
#[cfg(feature = "server")]
use crate::config::CONFIG;

pub static EXPIRATION_DAYS: i64 = 30;

#[derive(Debug, Serialize, Deserialize)]
pub struct Claims {
    pub sub: String, // user_id
    pub username: String,
    pub iat: usize,
    pub exp: usize,
}

#[cfg(feature = "server")]
pub fn create_token(user_id: String, username: String) -> Result<String, String> {
    let encoding_key = EncodingKey::from_secret(CONFIG.secret_key().as_bytes());
    let now = chrono::Utc::now();
    let iat = now.timestamp() as usize;

    let exp = now
        .checked_add_signed(chrono::Duration::days(EXPIRATION_DAYS))
        .expect("valid timestamp")
        .timestamp();

    let claims = Claims {
        sub: user_id.clone(),
        username: username.clone(),
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
