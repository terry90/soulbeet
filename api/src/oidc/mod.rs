//! Sign-in through an OpenID Connect provider (Pocket ID, Authelia,
//! Authentik, Keycloak, ...).
//!
//! The authorization code flow with PKCE (S256), a `state` against login CSRF
//! and a `nonce` bound to the ID token. The three travel in a short-lived,
//! encrypted cookie, so the server keeps no per-login state. The provider's
//! discovery document and signing keys are cached for an hour, and fetched
//! again at once when an ID token is signed by a key not in the cache (a key
//! rotation).
//!
//! A failed fetch is not retried for 30 seconds, and a stale copy keeps
//! serving meanwhile, so an unreachable provider does not stall every login.
//! One fetch runs at a time, and a login that has a copy does not wait for
//! it.
//!
//! A successful sign-in starts the same session as a password login. An
//! account is found by its OIDC identity (issuer and subject). A first
//! sign-in creates an account for a name nobody has (when auto-register is
//! on) and never takes an existing one: the claimed name proves nothing
//! (OpenID Connect Core 1.0, section 5.7). Only with OIDC_LINK_BY_USERNAME
//! does it link the account of that name (ignoring ASCII case, as every
//! sign-in does).
//!
//! A sign-in started from the login page of an app's authorize request
//! (`/login/return`) goes back to that request afterwards.

mod account;
mod flow;
mod provider;
mod routes;
#[cfg(test)]
mod testing;
mod token;

use axum::{routing::get, Router};

use crate::auth::{OIDC_CALLBACK_PATH, OIDC_LOGIN_PATH};
use routes::{callback, login, login_form};

pub(crate) use routes::{begin_link, LinkStartError};

/// The two routes of the flow, to merge into the app router.
pub fn router<S: Clone + Send + Sync + 'static>() -> Router<S> {
    Router::new()
        .route(OIDC_LOGIN_PATH, get(login).post(login_form))
        .route(OIDC_CALLBACK_PATH, get(callback))
}

/// A value from the callback URL, safe to log: anyone can craft that URL.
/// Shortened, and quoted with `{:?}` so newlines cannot forge log lines.
fn loggable(value: &str) -> String {
    format!("{:?}", value.chars().take(200).collect::<String>())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loggable_escapes_newlines_and_truncates() {
        let logged = loggable("x\nINFO forged line");
        assert!(!logged.contains('\n'));
        assert!(logged.contains("\\n"));
        assert!(loggable(&"a".repeat(1000)).len() < 210);
    }
}
