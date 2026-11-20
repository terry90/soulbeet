//! The code exchange at the token endpoint, the ID token's checks, and who
//! the token says signed in.

use std::str::FromStr;

use jsonwebtoken::{
    jwk::{AlgorithmParameters, Jwk, JwkSet, KeyAlgorithm, PublicKeyUse},
    Algorithm, DecodingKey, Validation,
};
use serde::Deserialize;
use serde_json::Value;

use super::flow::Flow;
use super::loggable;
use super::provider::{capped_body, ClientAuth, Provider, HTTP};
use crate::config::OidcConfig;
use crate::models::user::username_fits;

/// Signature algorithms accepted on ID tokens. Asymmetric only: HMAC would be
/// keyed with the client secret, which this flow never verifies with.
const ALLOWED_ALGORITHMS: [Algorithm; 9] = [
    Algorithm::RS256,
    Algorithm::RS384,
    Algorithm::RS512,
    Algorithm::PS256,
    Algorithm::PS384,
    Algorithm::PS512,
    Algorithm::ES256,
    Algorithm::ES384,
    Algorithm::EdDSA,
];

#[derive(Deserialize)]
struct TokenResponse {
    id_token: Option<String>,
}

/// The ID token for `code`, which the provider issued to `flow`.
pub(super) async fn exchange_code(
    provider: &Provider,
    config: &OidcConfig,
    code: &str,
    flow: &Flow,
) -> Result<String, String> {
    let mut form = vec![
        ("grant_type", "authorization_code"),
        ("code", code),
        ("redirect_uri", flow.redirect_uri.as_str()),
        ("code_verifier", flow.verifier.as_str()),
    ];

    let mut request = HTTP
        .post(provider.token_endpoint.clone())
        .header(reqwest::header::ACCEPT, "application/json");
    match (config.client_secret.as_deref(), provider.client_auth) {
        (Some(secret), ClientAuth::Basic) => {
            // RFC 6749, section 2.3.1: both are form-encoded before Basic.
            let encode =
                |s: &str| url::form_urlencoded::byte_serialize(s.as_bytes()).collect::<String>();
            request = request.basic_auth(encode(&config.client_id), Some(encode(secret)));
        }
        (Some(secret), ClientAuth::Post) => {
            form.push(("client_id", config.client_id.as_str()));
            form.push(("client_secret", secret));
        }
        (None, _) => form.push(("client_id", config.client_id.as_str())),
    }

    let response = request
        .form(&form)
        .send()
        .await
        .map_err(|e| format!("token request failed: {e}"))?;
    let status = response.status();
    let body = capped_body(response).await;
    if !status.is_success() {
        let body = match body {
            Ok(body) => loggable(&String::from_utf8_lossy(&body)),
            Err(e) => e,
        };
        return Err(format!("the token endpoint answered {status}: {body}"));
    }

    serde_json::from_slice::<TokenResponse>(&body.map_err(|e| format!("token response: {e}"))?)
        .map_err(|e| format!("unexpected token response: {e}"))?
        .id_token
        .ok_or_else(|| "the token response has no id_token".to_string())
}

/// ID token claims. `iss`, `aud` and `exp` are also checked by `jsonwebtoken`.
#[derive(Deserialize)]
pub(super) struct IdClaims {
    /// Checked against the discovery document's issuer
    iss: String,
    sub: String,
    /// A string or an array; its value is checked by `jsonwebtoken`
    aud: Value,
    azp: Option<String>,
    nonce: Option<String>,
    #[serde(flatten)]
    other: serde_json::Map<String, Value>,
}

#[derive(Debug)]
pub(super) enum TokenError {
    /// No cached key fits the token; the keys may have been rotated.
    UnknownKey,
    Invalid(String),
}

impl std::fmt::Display for TokenError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TokenError::UnknownKey => write!(f, "signed by an unknown key"),
            TokenError::Invalid(e) => write!(f, "{e}"),
        }
    }
}

/// Whether `jwk` is a signing key for `alg`.
fn key_fits(jwk: &Jwk, alg: Algorithm) -> bool {
    let family_fits = match &jwk.algorithm {
        AlgorithmParameters::RSA(_) => matches!(
            alg,
            Algorithm::RS256
                | Algorithm::RS384
                | Algorithm::RS512
                | Algorithm::PS256
                | Algorithm::PS384
                | Algorithm::PS512
        ),
        AlgorithmParameters::EllipticCurve(_) => {
            matches!(alg, Algorithm::ES256 | Algorithm::ES384)
        }
        AlgorithmParameters::OctetKeyPair(_) => alg == Algorithm::EdDSA,
        AlgorithmParameters::OctetKey(_) => false,
    };
    let use_fits = !matches!(jwk.common.public_key_use, Some(PublicKeyUse::Encryption));
    let alg_fits = jwk
        .common
        .key_algorithm
        .is_none_or(|key_alg| algorithm_of(key_alg) == Some(alg));
    family_fits && use_fits && alg_fits
}

fn algorithm_of(key_alg: KeyAlgorithm) -> Option<Algorithm> {
    Algorithm::from_str(&key_alg.to_string()).ok()
}

/// The key that signed a token: the one named by `kid`, or the only one that
/// fits when the token names none.
fn find_key<'a>(keys: &'a JwkSet, kid: Option<&str>, alg: Algorithm) -> Option<&'a Jwk> {
    let mut fitting = keys.keys.iter().filter(|jwk| key_fits(jwk, alg));
    match kid {
        Some(kid) => fitting.find(|jwk| jwk.common.key_id.as_deref() == Some(kid)),
        None => match (fitting.next(), fitting.next()) {
            (Some(only), None) => Some(only),
            _ => None,
        },
    }
}

pub(super) fn verify_id_token(
    provider: &Provider,
    config: &OidcConfig,
    token: &str,
    nonce: &str,
) -> Result<IdClaims, TokenError> {
    let header = jsonwebtoken::decode_header(token)
        .map_err(|e| TokenError::Invalid(format!("unreadable header: {e}")))?;
    if !ALLOWED_ALGORITHMS.contains(&header.alg) {
        return Err(TokenError::Invalid(format!(
            "unsupported algorithm {:?}",
            header.alg
        )));
    }

    let jwk = find_key(&provider.keys, header.kid.as_deref(), header.alg)
        .ok_or(TokenError::UnknownKey)?;
    let key = DecodingKey::from_jwk(jwk)
        .map_err(|e| TokenError::Invalid(format!("unusable signing key: {e}")))?;

    let mut validation = Validation::new(header.alg);
    validation.set_issuer(&[&provider.issuer]);
    validation.set_audience(&[&config.client_id]);
    validation.set_required_spec_claims(&["exp", "iss", "aud", "sub"]);

    let claims = jsonwebtoken::decode::<IdClaims>(token, &key, &validation)
        .map_err(|e| TokenError::Invalid(e.to_string()))?
        .claims;

    if claims.nonce.as_deref() != Some(nonce) {
        return Err(TokenError::Invalid("the nonce does not match".to_string()));
    }
    // OpenID Connect Core 1.0, section 3.1.3.7: with several audiences, or an
    // azp at all, this client must be the authorized party.
    let several_audiences = claims.aud.as_array().is_some_and(|aud| aud.len() > 1);
    if (several_audiences || claims.azp.is_some())
        && claims.azp.as_deref() != Some(config.client_id.as_str())
    {
        return Err(TokenError::Invalid(
            "issued to another authorized party (azp)".to_string(),
        ));
    }

    Ok(claims)
}

/// Who signed in: the provider's stable identifier (issuer and subject,
/// OpenID Connect Core 1.0, section 5.7) and the Soulbeet username.
#[derive(Debug, PartialEq)]
pub(super) struct Identity {
    pub(super) issuer: String,
    pub(super) subject: String,
    pub(super) username: String,
}

impl Identity {
    pub(super) fn from_claims(claims: &IdClaims, username_claim: &str) -> Result<Self, String> {
        let username = if username_claim == "sub" {
            Some(claims.sub.as_str())
        } else {
            claims.other.get(username_claim).and_then(Value::as_str)
        }
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .ok_or_else(|| {
            format!(
                "the ID token has no '{username_claim}' claim (set OIDC_USERNAME_CLAIM, \
                 or add the scope that carries it to OIDC_SCOPES)"
            )
        })?;
        if !username_fits(username) {
            return Err(format!(
                "the '{username_claim}' claim is too long for a username"
            ));
        }

        // An address is a name only once the provider asserts it is verified:
        // the boolean true (OpenID Connect Core 1.0, section 5.1). Anything
        // else asserts nothing.
        if username_claim == "email" {
            let email = loggable(username);
            match claims.other.get("email_verified") {
                Some(Value::Bool(true)) => {}
                Some(Value::Bool(false)) => {
                    return Err(format!("the email {email} is not verified"));
                }
                None => {
                    return Err(format!(
                        "the ID token has no email_verified claim for the email {email}"
                    ));
                }
                Some(other) => {
                    return Err(format!(
                        "email_verified is not a boolean ({}) for the email {email}",
                        loggable(&other.to_string())
                    ));
                }
            }
        }

        Ok(Self {
            issuer: claims.iss.clone(),
            subject: claims.sub.clone(),
            username: username.to_string(),
        })
    }

    /// The identity in a log line: the name the provider gave and the
    /// subject at the issuer, escaped, as they come from the provider.
    pub(super) fn for_log(&self) -> String {
        format!(
            "{} (subject {} at {})",
            loggable(&self.username),
            loggable(&self.subject),
            loggable(&self.issuer)
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connection::test_util::serve_on_loopback;
    use crate::oidc::flow::{new_flow, Purpose, Start};
    use crate::oidc::provider::MAX_BODY;
    use crate::oidc::testing::{
        identity_at, json_of_size, streamed, test_config, test_provider, ISSUER,
    };
    use axum::http::{header::HOST, HeaderMap, StatusCode, Uri};
    use axum::{routing::post, Router};
    use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
    use serde_json::json;

    #[tokio::test]
    async fn a_token_error_cannot_forge_log_lines() {
        let app = Router::new().route(
            "/token",
            post(|| async { (StatusCode::BAD_REQUEST, "bad\nINFO User 'admin' signed in") }),
        );
        let (addr, _server) = serve_on_loopback(app).await;
        let provider = Provider {
            token_endpoint: url::Url::parse(&format!("http://{addr}/token")).unwrap(),
            ..test_provider()
        };
        let refused = exchange_code(&provider, &test_config(), "code", &test_flow())
            .await
            .unwrap_err();
        assert!(refused.contains("bad\\nINFO"), "{refused}");
        assert!(!refused.contains('\n'), "{refused}");
    }

    fn test_flow() -> Flow {
        let mut headers = HeaderMap::new();
        headers.insert(HOST, "soulbeet.example.com".parse().unwrap());
        let start = Start::for_request(None, None, &headers, &Uri::from_static("/")).unwrap();
        new_flow(Purpose::SignIn { return_to: None }, &start)
    }

    #[tokio::test]
    async fn a_token_response_past_the_limit_is_refused() {
        let app = Router::new()
            .route(
                "/token",
                post(|| async { streamed(json_of_size(MAX_BODY + 1)) }),
            )
            .route(
                "/refused",
                post(|| async {
                    (
                        StatusCode::BAD_REQUEST,
                        streamed(json_of_size(MAX_BODY + 1)),
                    )
                }),
            );
        let (addr, _server) = serve_on_loopback(app).await;
        let flow = &test_flow();
        let exchange = |path: &str| {
            let provider = Provider {
                token_endpoint: url::Url::parse(&format!("http://{addr}{path}")).unwrap(),
                ..test_provider()
            };
            async move { exchange_code(&provider, &test_config(), "code", flow).await }
        };

        let refused = exchange("/token").await.unwrap_err();
        assert!(refused.contains("larger than"), "{refused}");
        // An error answer names its status, without its body read whole.
        let refused = exchange("/refused").await.unwrap_err();
        assert!(refused.contains("400"), "{refused}");
        assert!(refused.contains("larger than"), "{refused}");
    }

    fn claims(value: Value) -> IdClaims {
        serde_json::from_value(value).unwrap()
    }

    #[test]
    fn find_key_by_kid_and_algorithm() {
        let keys: JwkSet = serde_json::from_value(json!({ "keys": [
            { "kty": "RSA", "kid": "rsa", "use": "sig", "alg": "RS256", "n": "AQAB", "e": "AQAB" },
            { "kty": "RSA", "kid": "enc", "use": "enc", "n": "AQAB", "e": "AQAB" },
            { "kty": "OKP", "kid": "ed", "crv": "Ed25519", "x": "AQAB" },
        ]}))
        .unwrap();

        let kid = |jwk: Option<&Jwk>| jwk.and_then(|k| k.common.key_id.clone());
        assert_eq!(
            kid(find_key(&keys, Some("rsa"), Algorithm::RS256)),
            Some("rsa".into())
        );
        assert_eq!(
            kid(find_key(&keys, Some("ed"), Algorithm::EdDSA)),
            Some("ed".into())
        );
        // The key says RS256: not usable for anything else, even RSA.
        assert!(find_key(&keys, Some("rsa"), Algorithm::PS256).is_none());
        // Wrong family for the algorithm.
        assert!(find_key(&keys, Some("ed"), Algorithm::RS256).is_none());
        // An encryption key never verifies a signature.
        assert!(find_key(&keys, Some("enc"), Algorithm::RS256).is_none());
        assert!(find_key(&keys, Some("missing"), Algorithm::RS256).is_none());
        // Without a kid, only an unambiguous key is used.
        assert_eq!(
            kid(find_key(&keys, None, Algorithm::EdDSA)),
            Some("ed".into())
        );
    }

    #[test]
    fn identity_from_username_claim() {
        let c = claims(json!({
            "iss": ISSUER, "sub": "abc", "aud": "soulbeet", "preferred_username": " alice "
        }));
        assert_eq!(
            Identity::from_claims(&c, "preferred_username").unwrap(),
            Identity {
                issuer: ISSUER.into(),
                subject: "abc".into(),
                username: "alice".into()
            }
        );
        assert_eq!(Identity::from_claims(&c, "sub").unwrap().username, "abc");
        assert!(Identity::from_claims(&c, "email").is_err());
    }

    #[test]
    fn identity_refuses_unverified_email() {
        let with_verified = |verified: Option<Value>| {
            let mut c = json!({
                "iss": ISSUER, "sub": "abc", "aud": ["soulbeet"], "email": "a@example.com"
            });
            if let Some(verified) = verified {
                c["email_verified"] = verified;
            }
            Identity::from_claims(&claims(c), "email")
        };

        assert_eq!(
            with_verified(Some(json!(true))).unwrap().username,
            "a@example.com"
        );
        // Only the boolean true asserts a verified address (OpenID Connect
        // Core 1.0, section 5.1); each refusal says why.
        let refusal = |verified| with_verified(verified).unwrap_err();
        assert!(refusal(Some(json!(false))).contains("not verified"));
        assert!(refusal(None).contains("no email_verified"));
        assert!(refusal(Some(json!("true"))).contains("not a boolean"));
        assert!(refusal(Some(json!("false"))).contains("not a boolean"));
        assert!(refusal(Some(json!(1))).contains("not a boolean"));
    }

    /// A throwaway Ed25519 key, only for these tests.
    const TEST_KEY_PEM: &str = "-----BEGIN PRIVATE KEY-----
MC4CAQAwBQYDK2VwBCIEIB+2XiVDwrLs+fb3j6GY6K1Ltn7QqMeBcNI5DUjMxJ9e
-----END PRIVATE KEY-----";

    fn sign(kid: &str, claims: Value) -> String {
        let mut header = jsonwebtoken::Header::new(Algorithm::EdDSA);
        header.kid = Some(kid.to_string());
        let key = jsonwebtoken::EncodingKey::from_ed_pem(TEST_KEY_PEM.as_bytes()).unwrap();
        jsonwebtoken::encode(&header, &claims, &key).unwrap()
    }

    fn valid_claims() -> Value {
        let now = chrono::Utc::now().timestamp();
        json!({
            "iss": ISSUER, "aud": "soulbeet", "sub": "abc", "nonce": "n0nce",
            "iat": now, "exp": now + 300, "preferred_username": "alice",
        })
    }

    fn verify(token: &str) -> Result<IdClaims, TokenError> {
        verify_id_token(&test_provider(), &test_config(), token, "n0nce")
    }

    #[test]
    fn verifies_a_valid_id_token() {
        let claims = verify(&sign("k1", valid_claims())).unwrap();
        assert_eq!(claims.sub, "abc");
        assert_eq!(claims.other["preferred_username"], "alice");
    }

    #[test]
    fn rejects_bad_id_tokens() {
        let with = |key: &str, value: Value| {
            let mut claims = valid_claims();
            claims[key] = value;
            sign("k1", claims)
        };
        let invalid = |token: String| matches!(verify(&token), Err(TokenError::Invalid(_)));

        assert!(invalid(with("nonce", json!("replayed"))));
        assert!(invalid(with("aud", json!("another-client"))));
        assert!(invalid(with("iss", json!("https://evil.example.com"))));
        assert!(invalid(with(
            "exp",
            json!(chrono::Utc::now().timestamp() - 3600)
        )));
        assert!(invalid(with("azp", json!("another-client"))));
        assert!(invalid(with("aud", json!(["soulbeet", "another-client"]))));
        assert!(verify(&with("aud", json!(["soulbeet"]))).is_ok());

        let mut claims = valid_claims();
        claims.as_object_mut().unwrap().remove("nonce");
        assert!(invalid(sign("k1", claims)));

        // Signed by a key the cache does not have: the caller refreshes.
        assert!(matches!(
            verify(&sign("rotated", valid_claims())),
            Err(TokenError::UnknownKey)
        ));

        // A tampered payload breaks the signature.
        let token = sign("k1", valid_claims());
        let mut parts: Vec<&str> = token.split('.').collect();
        let forged = URL_SAFE_NO_PAD
            .encode(serde_json::to_vec(&with_username(valid_claims(), "admin")).unwrap());
        parts[1] = &forged;
        assert!(invalid(parts.join(".")));
    }

    #[test]
    fn rejects_symmetric_and_unsigned_tokens() {
        let hs256 = jsonwebtoken::encode(
            &jsonwebtoken::Header::new(Algorithm::HS256),
            &valid_claims(),
            &jsonwebtoken::EncodingKey::from_secret(b"client-secret"),
        )
        .unwrap();
        assert!(matches!(verify(&hs256), Err(TokenError::Invalid(_))));

        let none = format!(
            "{}.{}.",
            URL_SAFE_NO_PAD.encode(r#"{"alg":"none"}"#),
            URL_SAFE_NO_PAD.encode(valid_claims().to_string())
        );
        assert!(matches!(verify(&none), Err(TokenError::Invalid(_))));
    }

    fn with_username(mut claims: Value, username: &str) -> Value {
        claims["preferred_username"] = json!(username);
        claims
    }

    #[test]
    fn a_logged_identity_names_who_it_is_and_cannot_forge_lines() {
        // An admin tracing a link to the wrong person reads who it was.
        let logged = identity_at(ISSUER, "sub-1", "alice\nINFO forged").for_log();
        assert!(logged.contains("alice"), "{logged}");
        assert!(logged.contains("\"sub-1\""), "{logged}");
        assert!(logged.contains(ISSUER), "{logged}");
        assert!(!logged.contains('\n'), "{logged}");
    }
}
