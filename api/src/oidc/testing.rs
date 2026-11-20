//! What the tests of the oidc module share.

use std::time::Instant;

use serde_json::json;

use super::provider::{ClientAuth, Provider};
use super::token::Identity;
use crate::config::OidcConfig;

/// The public half of the token tests' throwaway Ed25519 key.
pub(super) const TEST_KEY_X: &str = "N2BuOb1DeE-Kl98L_u7momt6p9k_aNeU1z6oJcR7KOU";
pub(super) const ISSUER: &str = "https://id.example.com";

pub(super) fn test_provider() -> Provider {
    Provider {
        issuer: ISSUER.to_string(),
        authorization_endpoint: url::Url::parse("https://id.example.com/authorize").unwrap(),
        token_endpoint: url::Url::parse("https://id.example.com/token").unwrap(),
        client_auth: ClientAuth::Basic,
        keys: serde_json::from_value(json!({ "keys": [
            { "kty": "OKP", "kid": "k1", "crv": "Ed25519", "x": TEST_KEY_X },
        ]}))
        .unwrap(),
        fetched_at: Instant::now(),
    }
}

pub(super) fn test_config() -> OidcConfig {
    OidcConfig {
        issuer_url: ISSUER.to_string(),
        client_id: "soulbeet".to_string(),
        client_secret: None,
        redirect_pin: None,
        scopes: "openid profile".to_string(),
        username_claim: "preferred_username".to_string(),
        provider_name: "Test".to_string(),
        auto_register: true,
        link_by_username: false,
    }
}

pub(super) fn identity(subject: &str, username: &str) -> Identity {
    identity_at(ISSUER, subject, username)
}

pub(super) fn identity_at(issuer: &str, subject: &str, username: &str) -> Identity {
    Identity {
        issuer: issuer.to_string(),
        subject: subject.to_string(),
        username: username.to_string(),
    }
}

/// A JSON object of exactly `len` bytes that reads as a token response.
pub(super) fn json_of_size(len: usize) -> Vec<u8> {
    let frame = br#"{"id_token":""}"#.len();
    format!(r#"{{"id_token":"{}"}}"#, "a".repeat(len - frame)).into_bytes()
}

/// `bytes` sent in chunks, without a Content-Length to tell their size.
pub(super) fn streamed(bytes: Vec<u8>) -> axum::body::Body {
    let chunks: Vec<Result<Vec<u8>, std::io::Error>> =
        bytes.chunks(64 * 1024).map(|c| Ok(c.to_vec())).collect();
    axum::body::Body::from_stream(futures::stream::iter(chunks))
}
