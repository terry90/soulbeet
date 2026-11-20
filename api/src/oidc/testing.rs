//! What the tests of the oidc module share.

use std::cell::RefCell;
use std::fmt::{Debug, Write};
use std::sync::Once;
use std::time::Instant;

use serde_json::json;
use tracing::subscriber::Interest;

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

/// The messages logged on this thread while it lives.
///
/// A global subscriber, installed once, keeps them per thread: a scoped one
/// would race the interest other test threads cache for the same log lines.
pub(super) struct LogCapture(());

thread_local! {
    static CAPTURED: RefCell<Option<Vec<String>>> = const { RefCell::new(None) };
}

impl LogCapture {
    pub(super) fn start() -> Self {
        static INSTALLED: Once = Once::new();
        INSTALLED.call_once(|| {
            tracing::subscriber::set_global_default(Collector)
                .expect("no other test sets a global subscriber");
        });
        // A line first logged on another thread while it was installed may
        // have cached that nobody listens.
        tracing::callsite::rebuild_interest_cache();
        CAPTURED.with(|lines| *lines.borrow_mut() = Some(Vec::new()));
        LogCapture(())
    }

    pub(super) fn lines(&self) -> Vec<String> {
        CAPTURED.with(|lines| lines.borrow().clone().unwrap_or_default())
    }
}

impl Drop for LogCapture {
    fn drop(&mut self) {
        CAPTURED.with(|lines| *lines.borrow_mut() = None);
    }
}

struct Collector;

impl tracing::Subscriber for Collector {
    /// Asked at each event: whether its thread captures changes.
    fn register_callsite(&self, _: &'static tracing::Metadata<'static>) -> Interest {
        Interest::sometimes()
    }
    fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
        CAPTURED.with(|lines| lines.borrow().is_some())
    }
    fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        tracing::span::Id::from_u64(1)
    }
    fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
    fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
    fn event(&self, event: &tracing::Event<'_>) {
        struct Message<'a>(&'a mut String);
        impl tracing::field::Visit for Message<'_> {
            fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn Debug) {
                if field.name() == "message" {
                    // The message's Debug is its text as written.
                    let _ = write!(self.0, "{value:?}");
                }
            }
        }
        let mut line = String::new();
        event.record(&mut Message(&mut line));
        CAPTURED.with(|lines| {
            if let Some(lines) = lines.borrow_mut().as_mut() {
                lines.push(line);
            }
        });
    }
    fn enter(&self, _: &tracing::span::Id) {}
    fn exit(&self, _: &tracing::span::Id) {}
}
