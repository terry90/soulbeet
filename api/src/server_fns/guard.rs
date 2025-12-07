use crate::auth::{self, Claims};
use dioxus::prelude::ServerFnError;

#[cfg(feature = "server")]
use axum::{
    extract::{FromRequest, FromRequestParts, Request},
    http::StatusCode,
};

pub struct AuthSession(pub Claims);

#[cfg(feature = "server")]
impl<S> FromRequestParts<S> for AuthSession
where
    S: Send + Sync,
{
    type Rejection = (axum::http::StatusCode, String);

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        state: &S,
    ) -> Result<Self, Self::Rejection> {
        let cookies = parts
            .extensions
            .get::<tower_cookies::Cookies>()
            .ok_or_else(|| {
                (
                    StatusCode::UNAUTHORIZED,
                    String::from("Missing cookie middleware"),
                )
            })?;

        let token = cookies
            .get(crate::AUTH_COOKIE_NAME)
            .map(|c| c.value().to_string());

        match token {
            Some(token) => match auth::verify_token(&token) {
                Ok(claims) => Ok(AuthSession(claims)),
                Err(e) => Err((StatusCode::UNAUTHORIZED, format!("Invalid token: {}", e))),
            },
            None => Err((StatusCode::UNAUTHORIZED, "No auth token found".to_string())),
        }
    }
}
