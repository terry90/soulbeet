//! What the tests of the oidc module share.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Instant;

use axum::http::{header::HOST, HeaderMap, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::{
    routing::{get, post},
    Json, Router,
};
use jsonwebtoken::Algorithm;
use serde_json::{json, Value};

use super::flow::{new_flow, Flow, Purpose, Start};
use super::provider::{ClientAuth, Provider};
use super::token::Identity;
use crate::config::OidcConfig;
use crate::connection::test_util::serve_on_loopback;

/// The public half of the token tests' throwaway Ed25519 key.
pub(super) const TEST_KEY_X: &str = "N2BuOb1DeE-Kl98L_u7momt6p9k_aNeU1z6oJcR7KOU";
pub(super) const ISSUER: &str = "https://id.example.com";

/// The private half, only for these tests.
const TEST_KEY_PEM: &str = "-----BEGIN PRIVATE KEY-----
MC4CAQAwBQYDK2VwBCIEIB+2XiVDwrLs+fb3j6GY6K1Ltn7QqMeBcNI5DUjMxJ9e
-----END PRIVATE KEY-----";

/// `claims` as an ID token signed with the test key, named `kid`.
pub(super) fn sign_id_token(kid: &str, claims: Value) -> String {
    let mut header = jsonwebtoken::Header::new(Algorithm::EdDSA);
    header.kid = Some(kid.to_string());
    let key = jsonwebtoken::EncodingKey::from_ed_pem(TEST_KEY_PEM.as_bytes()).unwrap();
    jsonwebtoken::encode(&header, &claims, &key).unwrap()
}

/// A key set publishing the test key under each of `kids`.
fn key_set(kids: &[&str]) -> Value {
    let keys: Vec<Value> = kids
        .iter()
        .map(|kid| json!({ "kty": "OKP", "kid": kid, "crv": "Ed25519", "x": TEST_KEY_X }))
        .collect();
    json!({ "keys": keys })
}

pub(super) fn test_provider() -> Provider {
    Provider {
        issuer: ISSUER.to_string(),
        authorization_endpoint: url::Url::parse("https://id.example.com/authorize").unwrap(),
        token_endpoint: url::Url::parse("https://id.example.com/token").unwrap(),
        client_auth: ClientAuth::Basic,
        keys: serde_json::from_value(key_set(&["k1"])).unwrap(),
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

/// A sign-in started at `https://soulbeet.example.com`.
pub(super) fn test_flow() -> Flow {
    let mut headers = HeaderMap::new();
    headers.insert(HOST, "soulbeet.example.com".parse().unwrap());
    let start = Start::for_request(None, None, &headers, &Uri::from_static("/")).unwrap();
    new_flow(Purpose::SignIn { return_to: None }, &start)
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

/// What the stub provider publishes and answers.
#[derive(Default)]
struct Published {
    issuer: OnceLock<String>,
    kids: Mutex<Vec<String>>,
    id_token: Mutex<String>,
    down: AtomicBool,
    fetches: AtomicUsize,
}

/// `answer`, or an error while the provider is down.
fn unless_down(published: &Published, answer: Value) -> Response {
    if published.down.load(Ordering::SeqCst) {
        StatusCode::SERVICE_UNAVAILABLE.into_response()
    } else {
        Json(answer).into_response()
    }
}

/// A provider on a loopback port, on plain http as on a LAN: its discovery
/// document, the key set it publishes and a token endpoint that answers
/// the ID token a test sets. It counts its discovery fetches and can go
/// down, which fails the discovery document and the key set.
pub(super) struct StubProvider {
    pub(super) issuer: String,
    published: Arc<Published>,
}

impl StubProvider {
    /// Publishing the test key under `kids`.
    pub(super) async fn start(kids: &[&str]) -> StubProvider {
        let published = Arc::new(Published::default());
        let (discovery, keys, token) = (published.clone(), published.clone(), published.clone());
        let app = Router::new()
            .route(
                "/.well-known/openid-configuration",
                get(move || async move {
                    discovery.fetches.fetch_add(1, Ordering::SeqCst);
                    let issuer = discovery.issuer.get().unwrap();
                    let document = json!({
                        "issuer": issuer,
                        "authorization_endpoint": format!("{issuer}/authorize"),
                        "token_endpoint": format!("{issuer}/token"),
                        "jwks_uri": format!("{issuer}/jwks"),
                    });
                    unless_down(&discovery, document)
                }),
            )
            .route(
                "/jwks",
                get(move || async move {
                    let kids = keys.kids.lock().unwrap().clone();
                    let kids: Vec<&str> = kids.iter().map(String::as_str).collect();
                    unless_down(&keys, key_set(&kids))
                }),
            )
            .route(
                "/token",
                post(move || async move {
                    Json(json!({ "id_token": token.id_token.lock().unwrap().clone() }))
                }),
            );
        let (addr, _server) = serve_on_loopback(app).await;
        let stub = StubProvider {
            issuer: format!("http://{addr}"),
            published,
        };
        stub.published.issuer.set(stub.issuer.clone()).unwrap();
        stub.publish(kids);
        stub
    }

    /// From now on, the test key under `kids` (a key rotation).
    pub(super) fn publish(&self, kids: &[&str]) {
        *self.published.kids.lock().unwrap() = kids.iter().map(|k| k.to_string()).collect();
    }

    /// The token endpoint answers this ID token.
    pub(super) fn answer_id_token(&self, id_token: String) {
        *self.published.id_token.lock().unwrap() = id_token;
    }

    pub(super) fn set_down(&self, down: bool) {
        self.published.down.store(down, Ordering::SeqCst);
    }

    /// How often the discovery document was fetched.
    pub(super) fn fetches(&self) -> usize {
        self.published.fetches.load(Ordering::SeqCst)
    }

    /// The settings of a client of this provider.
    pub(super) fn config(&self) -> OidcConfig {
        OidcConfig {
            issuer_url: self.issuer.clone(),
            ..test_config()
        }
    }

    /// This provider as Soulbeet fetched it at `fetched_at`, when it
    /// published `kids`.
    pub(super) fn copy(&self, kids: &[&str], fetched_at: Instant) -> Provider {
        let endpoint = |path: &str| url::Url::parse(&format!("{}{path}", self.issuer)).unwrap();
        Provider {
            issuer: self.issuer.clone(),
            authorization_endpoint: endpoint("/authorize"),
            token_endpoint: endpoint("/token"),
            client_auth: ClientAuth::Basic,
            keys: serde_json::from_value(key_set(kids)).unwrap(),
            fetched_at,
        }
    }

    /// The claims of an ID token this provider issues to `config`'s client
    /// for `flow`, naming `username`.
    pub(super) fn claims(&self, flow: &Flow, username: &str) -> Value {
        let now = chrono::Utc::now().timestamp();
        json!({
            "iss": self.issuer, "aud": test_config().client_id, "sub": format!("sub-{username}"),
            "nonce": flow.nonce, "iat": now, "exp": now + 300, "preferred_username": username,
        })
    }
}
