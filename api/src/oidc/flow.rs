//! One sign-in between its two legs: where it comes back to, and the
//! state, nonce and PKCE verifier sealed in this browser's flow cookie.

use axum::http::{HeaderMap, Uri};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tower_cookies::{
    cookie::{time, SameSite},
    Cookie, Cookies,
};

use super::provider::Provider;
use crate::auth::{SsoError, OIDC_CALLBACK_PATH, OIDC_LOGIN_PATH};
use crate::base_url::{request_base_with, BaseUrl, BaseUrlError};
use crate::config::OidcConfig;
use crate::crypto;
use crate::server_fns::auth::SessionCookie;

/// Prefix of the cookie that holds the state, nonce and PKCE verifier between
/// the two legs of a sign-in. The state completes the name, so logins started
/// in two tabs do not overwrite each other.
const FLOW_COOKIE_PREFIX: &str = "oidc_flow_";
const FLOW_COOKIE_PATH: &str = "/api/auth/oidc";
const FLOW_TTL_SECS: i64 = 600;

/// What the callback needs to check that it answers the login it started.
#[derive(Serialize, Deserialize)]
pub(super) struct Flow {
    pub(super) state: String,
    pub(super) nonce: String,
    pub(super) verifier: String,
    pub(super) expires_at: i64,
    pub(super) purpose: Purpose,
    /// The callback the provider was asked to send the browser back to,
    /// sent again with the code (RFC 6749, section 4.1.3)
    pub(super) redirect_uri: String,
    /// How the cookies of this sign-in are set, decided by the request that
    /// started it: the callback comes from the provider and carries no
    /// Origin to tell plain http
    pub(super) session: SessionCookie,
}

/// Where a sign-in started by a request comes back to, and how its cookies
/// are set, by the rule of a password login's session cookie.
#[derive(Debug)]
pub(super) struct Start {
    /// The address of the callback the provider sends the browser back to
    pub(super) callback: BaseUrl,
    pub(super) session: SessionCookie,
    /// The callback is pinned to another host than the page that sent the
    /// request: a flow cookie set now would not reach the callback
    pub(super) elsewhere: bool,
}

impl Start {
    /// For the request `headers` and `uri`. The sign-in's address is `pin`
    /// (OIDC_REDIRECT_URL) when set, else `public_url` (PUBLIC_URL) when
    /// set, else the request's own, on plain http when the browser is. The
    /// callback is at that address, and its cookies follow the rule of a
    /// password login's session cookie there. The provider checks the
    /// address against the ones registered there.
    pub(super) fn for_request(
        pin: Option<&BaseUrl>,
        public_url: Option<&Result<BaseUrl, BaseUrlError>>,
        headers: &HeaderMap,
        uri: &Uri,
    ) -> Result<Start, String> {
        // The pin is this sign-in's address, as PUBLIC_URL is a password
        // login's: the move to it is a GET, which carries no Origin to tell
        // plain http, so the pin's own scheme must decide.
        let pin = pin.map(|pin| Ok(pin.clone()));
        let address = pin.as_ref().or(public_url);
        let session =
            SessionCookie::for_request_with(headers, uri, address).map_err(|e| e.to_string())?;
        let base = request_base_with(headers, uri, address)
            .map_err(|e| format!("cannot tell this request's address: {e}"))?;
        let callback = match address {
            None if !session.secure() => base.on_http(),
            _ => base,
        };
        // Only a pinned address can differ from the request's own. The
        // page's Origin says where the browser is; a request without one (a
        // link, or the move to the pinned address) starts where it is.
        let page = headers
            .get(axum::http::header::ORIGIN)
            .and_then(|origin| origin.to_str().ok())
            .and_then(|origin| BaseUrl::parse(origin).ok());
        let elsewhere = address.is_some() && page.is_some_and(|page| !page.same_host(&callback));
        Ok(Start {
            callback,
            session,
            elsewhere,
        })
    }

    fn redirect_uri(&self) -> String {
        format!("{}{OIDC_CALLBACK_PATH}", self.callback)
    }

    /// The same sign-in started at the callback's address.
    pub(super) fn login_url(&self, return_to: Option<&str>) -> String {
        match return_to {
            Some(back) => format!(
                "{}{OIDC_LOGIN_PATH}?{}",
                self.callback,
                shared::oauth::return_to_query(back)
            ),
            None => format!("{}{OIDC_LOGIN_PATH}", self.callback),
        }
    }
}

/// What a flow was started for.
#[derive(Debug, PartialEq, Serialize, Deserialize)]
pub(super) enum Purpose {
    /// Sign in, then go back to this authorize request (already checked)
    SignIn { return_to: Option<String> },
    /// Link the identity to this account, which started the flow from a
    /// session and must still be signed in when the flow ends
    Link { user_id: String },
}

/// 256 random bits, base64url: a state, nonce or PKCE verifier (RFC 7636
/// wants 43 to 128 characters; this is 43).
fn random_token() -> String {
    let mut bytes = [0u8; 32];
    rand::fill(&mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
}

fn pkce_challenge(verifier: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

pub(super) fn authorization_url(provider: &Provider, config: &OidcConfig, flow: &Flow) -> url::Url {
    let mut url = provider.authorization_endpoint.clone();
    url.query_pairs_mut()
        .append_pair("response_type", "code")
        .append_pair("client_id", &config.client_id)
        .append_pair("redirect_uri", &flow.redirect_uri)
        .append_pair("scope", &config.scopes)
        .append_pair("state", &flow.state)
        .append_pair("nonce", &flow.nonce)
        .append_pair("code_challenge", &pkce_challenge(&flow.verifier))
        .append_pair("code_challenge_method", "S256");
    // A link binds whoever the provider says signed in, for good. The user
    // signs in at the provider again and so sees who that is, rather than a
    // session someone else left open there being linked unseen (OpenID
    // Connect Core 1.0, section 3.1.2.1).
    if matches!(flow.purpose, Purpose::Link { .. }) {
        url.query_pairs_mut().append_pair("prompt", "login");
    }
    url
}

fn flow_cookie_name(state: &str) -> String {
    format!("{FLOW_COOKIE_PREFIX}{state}")
}

pub(super) fn flow_cookie(state: &str, value: String, session: &SessionCookie) -> Cookie<'static> {
    let mut cookie = Cookie::new(flow_cookie_name(state), value);
    cookie.set_path(FLOW_COOKIE_PATH);
    cookie.set_http_only(true);
    // Lax, not Strict: the callback is a top-level navigation coming from the
    // provider's site, and must carry this cookie.
    cookie.set_same_site(SameSite::Lax);
    cookie.set_secure(session.secure());
    cookie.set_max_age(time::Duration::seconds(FLOW_TTL_SECS));
    cookie
}

/// A new flow for `purpose`, coming back as `start` says.
pub(super) fn new_flow(purpose: Purpose, start: &Start) -> Flow {
    Flow {
        state: random_token(),
        nonce: random_token(),
        verifier: random_token(),
        expires_at: chrono::Utc::now().timestamp() + FLOW_TTL_SECS,
        purpose,
        redirect_uri: start.redirect_uri(),
        session: start.session,
    }
}

/// The flow this browser started that the callback answers. It is single
/// use: its cookie is dropped whatever happens next.
pub(super) fn take_flow(cookies: &Cookies, state: Option<&str>) -> Result<Flow, SsoError> {
    // The state names its cookie; a callback without one matches no login.
    let sealed = state.and_then(|state| {
        let name = flow_cookie_name(state);
        let sealed = cookies.get(&name).map(|c| c.value().to_string());
        if sealed.is_some() {
            let mut removal = Cookie::new(name, "");
            removal.set_path(FLOW_COOKIE_PATH);
            cookies.remove(removal);
        }
        sealed
    });

    let flow = sealed
        .and_then(|sealed| crypto::decrypt(crate::secret_key::current(), &sealed).ok())
        .and_then(|json| serde_json::from_str::<Flow>(&json).ok())
        .filter(|flow| flow.expires_at > chrono::Utc::now().timestamp())
        .ok_or_else(|| {
            tracing::warn!(
                "OIDC sign-in failed: no valid login state (expired, or the login \
                 started in another browser or at another address than the callback's)"
            );
            SsoError::Failed
        })?;

    if state != Some(flow.state.as_str()) {
        tracing::warn!("OIDC sign-in failed: the state does not match this login");
        return Err(SsoError::Failed);
    }
    Ok(flow)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::oidc::testing::{test_config, test_provider};

    #[test]
    fn pkce_challenge_matches_rfc_7636_example() {
        // RFC 7636, appendix B.
        assert_eq!(
            pkce_challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    #[test]
    fn random_tokens_are_long_and_distinct() {
        let (a, b) = (random_token(), random_token());
        assert_eq!(a.len(), 43);
        assert_ne!(a, b);
    }

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        pairs
            .iter()
            .map(|(k, v)| {
                (
                    axum::http::HeaderName::from_bytes(k.as_bytes()).unwrap(),
                    axum::http::HeaderValue::from_str(v).unwrap(),
                )
            })
            .collect()
    }

    /// How a sign-in started by a request with these headers goes, with
    /// `pin` as OIDC_REDIRECT_URL's address and `public_url` as PUBLIC_URL.
    fn start_with(
        pairs: &[(&str, &str)],
        pin: Option<&str>,
        public_url: Option<&str>,
    ) -> Result<Start, String> {
        let pin = pin.map(|p| BaseUrl::parse(p).unwrap());
        let public_url = public_url.map(BaseUrl::parse);
        Start::for_request(
            pin.as_ref(),
            public_url.as_ref(),
            &headers(pairs),
            &Uri::from_static(OIDC_LOGIN_PATH),
        )
    }

    fn start(pairs: &[(&str, &str)]) -> Start {
        start_with(pairs, None, None).unwrap()
    }

    fn flow_for(purpose: Purpose) -> Flow {
        new_flow(purpose, &start(&[("host", "soulbeet.example.com")]))
    }

    fn query_param(url: &url::Url, name: &str) -> Option<String> {
        url.query_pairs()
            .find(|(n, _)| n == name)
            .map(|(_, value)| value.into_owned())
    }

    fn prompt_of(purpose: Purpose) -> Option<String> {
        let url = authorization_url(&test_provider(), &test_config(), &flow_for(purpose));
        query_param(&url, "prompt")
    }

    #[test]
    fn the_callback_is_at_the_address_the_sign_in_starts_at() {
        // A LAN address, a name behind a proxy that says https, a bare name.
        for (pairs, callback) in [
            (
                &[("host", "192.168.1.5:9765")][..],
                "http://192.168.1.5:9765",
            ),
            (
                &[
                    ("host", "soulful:9765"),
                    ("x-forwarded-host", "music.example.com"),
                    ("x-forwarded-proto", "https"),
                    ("origin", "https://music.example.com"),
                ][..],
                "https://music.example.com",
            ),
            (
                &[("host", "music.example.com")][..],
                "https://music.example.com",
            ),
        ] {
            let start = start(pairs);
            assert_eq!(
                start.redirect_uri(),
                format!("{callback}/api/auth/oidc/callback")
            );
            assert_eq!(start.session.secure(), callback.starts_with("https:"));
        }
    }

    #[test]
    fn a_plain_http_sign_in_at_a_name_comes_back_on_http_and_keeps_its_cookies() {
        // http://nas:9765 typed in the browser, no proxy: the name alone
        // derives https. The button's form post carries the browser's Origin,
        // as a password login does.
        let start = start(&[("host", "nas:9765"), ("origin", "http://nas:9765")]);
        assert_eq!(
            start.redirect_uri(),
            "http://nas:9765/api/auth/oidc/callback"
        );
        let flow = new_flow(Purpose::SignIn { return_to: None }, &start);
        // The flow cookie and the session cookie the callback sets from the
        // flow are not Secure, so the browser on http keeps them.
        assert_eq!(
            flow_cookie("s", "v".into(), &flow.session).secure(),
            Some(false)
        );
        assert!(!flow.session.secure());
        // Over https, both are Secure.
        let flow = new_flow(
            Purpose::SignIn { return_to: None },
            &start_with(
                &[("host", "nas:9765"), ("origin", "https://nas:9765")],
                None,
                None,
            )
            .unwrap(),
        );
        assert_eq!(flow.redirect_uri, "https://nas:9765/api/auth/oidc/callback");
        assert_eq!(
            flow_cookie("s", "v".into(), &flow.session).secure(),
            Some(true)
        );
    }

    #[test]
    fn a_sign_in_moved_to_a_plain_http_pin_keeps_its_cookies() {
        // OIDC_REDIRECT_URL at http://nas:9765, no PUBLIC_URL, no proxy. The
        // button is clicked on the LAN address, and the sign-in moves to the
        // pin.
        let pin = Some("http://nas:9765");
        let lan = [
            ("host", "192.168.1.10:9765"),
            ("origin", "http://192.168.1.10:9765"),
        ];
        assert!(start_with(&lan, pin, None).unwrap().elsewhere);
        // The move is a GET without an Origin, and the name alone derives
        // https: the pin's own scheme decides, as PUBLIC_URL's does.
        let start = start_with(&[("host", "nas:9765")], pin, None).unwrap();
        assert!(!start.elsewhere);
        assert_eq!(
            start.redirect_uri(),
            "http://nas:9765/api/auth/oidc/callback"
        );
        assert!(!start.session.secure());
        let flow = new_flow(Purpose::SignIn { return_to: None }, &start);
        assert_eq!(
            flow_cookie("s", "v".into(), &flow.session).secure(),
            Some(false)
        );
        assert!(!flow.session.secure());
        // An https pin gives Secure cookies, also reached by a plain IP.
        let start = start_with(
            &[("host", "192.168.1.10:9765")],
            Some("https://music.example.com"),
            None,
        )
        .unwrap();
        assert!(start.session.secure());
    }

    #[test]
    fn a_pinned_address_is_the_callback() {
        let lan = [("host", "192.168.1.5:9765")];
        let callback = |pin, public_url| start_with(&lan, pin, public_url).unwrap().redirect_uri();
        assert_eq!(
            callback(None, Some("https://music.example.com")),
            "https://music.example.com/api/auth/oidc/callback"
        );
        // OIDC_REDIRECT_URL wins over PUBLIC_URL.
        assert_eq!(
            callback(
                Some("https://sso.example.com"),
                Some("https://music.example.com")
            ),
            "https://sso.example.com/api/auth/oidc/callback"
        );
        // An unusable PUBLIC_URL stops a sign-in as it stops a password
        // login, and so does a request whose address cannot be told.
        assert!(start_with(&lan, None, Some("music.example.com")).is_err());
        assert!(start_with(&[], None, None).is_err());
        // With OIDC_REDIRECT_URL, PUBLIC_URL plays no part.
        assert_eq!(
            callback(Some("https://sso.example.com"), Some("music.example.com")),
            "https://sso.example.com/api/auth/oidc/callback"
        );
    }

    #[test]
    fn a_sign_in_from_another_address_than_a_pinned_callback_moves_there_first() {
        // PUBLIC_URL (or OIDC_REDIRECT_URL) is the public name, and the
        // button is clicked on the LAN address: the flow cookie would be set
        // there, and the provider sends the browser to the public name.
        let lan = [
            ("host", "192.168.1.5:9765"),
            ("origin", "http://192.168.1.5:9765"),
        ];
        for (pin, public_url) in [
            (None, Some("https://music.example.com")),
            (Some("https://music.example.com"), None),
        ] {
            let start = start_with(&lan, pin, public_url).unwrap();
            assert!(start.elsewhere);
            let back = "/oauth/authorize?client_id=c&state=a%26b";
            let moved = start.login_url(Some(back));
            assert_eq!(
                moved,
                format!(
                    "https://music.example.com/api/auth/oidc/login?{}",
                    shared::oauth::return_to_query(back)
                )
            );
            assert_eq!(
                start.login_url(None),
                "https://music.example.com/api/auth/oidc/login"
            );
        }

        // There, it starts: the request comes from the pinned name (on any
        // scheme), or without an Origin, as the redirect there does.
        let pinned = Some("https://music.example.com");
        for pairs in [
            &[
                ("host", "music.example.com"),
                ("origin", "https://music.example.com"),
            ][..],
            &[
                ("host", "music.example.com"),
                ("origin", "http://music.example.com"),
            ][..],
            &[("host", "soulful:9765")][..],
        ] {
            assert!(
                !start_with(pairs, None, pinned).unwrap().elsewhere,
                "{pairs:?}"
            );
        }
        // Without a pin, the callback is wherever the request came from.
        let proxied = [
            ("host", "soulful:9765"),
            ("origin", "https://music.example.com"),
        ];
        assert!(!start_with(&proxied, None, None).unwrap().elsewhere);
    }

    #[test]
    fn the_provider_is_asked_to_send_the_browser_back_where_it_started() {
        let flow = new_flow(
            Purpose::SignIn { return_to: None },
            &start(&[("host", "192.168.1.5:9765")]),
        );
        assert_eq!(
            flow.redirect_uri,
            "http://192.168.1.5:9765/api/auth/oidc/callback"
        );
        // The code exchange sends the same flow's value.
        let url = authorization_url(&test_provider(), &test_config(), &flow);
        assert_eq!(query_param(&url, "redirect_uri"), Some(flow.redirect_uri));
    }

    #[test]
    fn a_link_asks_the_provider_to_sign_in_again() {
        // A link binds whoever the provider says signed in. Without a new
        // sign-in, a session someone else left open at the provider would be
        // linked without anyone seeing it (OpenID Connect Core 1.0, 3.1.2.1).
        assert_eq!(
            prompt_of(Purpose::Link {
                user_id: "u".into()
            })
            .as_deref(),
            Some("login")
        );
        // A sign-in keeps the provider's session: that is the point of it.
        assert_eq!(prompt_of(Purpose::SignIn { return_to: None }), None);
    }
}
