//! Apps that identify with a Client ID Metadata Document
//! (draft-ietf-oauth-client-id-metadata-document-00): the client id is an
//! https URL, and the JSON document there names the app and its redirect
//! URIs.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use reqwest::dns::{Addrs, Name, Resolve, Resolving};
use reqwest::header::{HeaderMap, HeaderValue, ACCEPT};
use reqwest::redirect::Policy;
use serde_json::Value;
use url::{Host, Url};

use super::clients::{client_metadata, MetadataRefusal};
use crate::capped_body::{read_capped, CappedBodyError};

/// The largest document Soulful reads.
pub(crate) const DOCUMENT_MAX_BYTES: usize = 64 * 1024;

const FETCH_TIMEOUT: Duration = Duration::from_secs(5);

/// Why an app's client id or document cannot be used. Each text completes
/// "Soulful could not use this app's client metadata at {url}: ".
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum CimdError {
    NotAClientIdUrl(&'static str),
    /// The request failed, with its causes. The page says only that it
    /// failed, since the causes would tell a signed-in user whether a name
    /// resolves to a private address; the log keeps them.
    Fetch(String),
    Status(u16),
    TooLarge,
    NotJson(String),
    ClientIdMismatch {
        document: String,
    },
    MissingField(&'static str),
    NotPublic(String),
    /// A member breaks a rule registration also applies; the rule.
    Invalid(&'static str),
}

impl std::fmt::Display for CimdError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CimdError::NotAClientIdUrl(m) | CimdError::Invalid(m) => write!(f, "{m}"),
            CimdError::Fetch(_) => write!(f, "Soulbeet could not fetch it"),
            CimdError::Status(s) => write!(f, "it answered HTTP {s}"),
            CimdError::TooLarge => write!(f, "it is larger than 64 KiB"),
            CimdError::NotJson(e) => write!(f, "it is not a JSON object ({e})"),
            CimdError::ClientIdMismatch { document } => {
                write!(f, "it names another client id: {document}")
            }
            CimdError::MissingField(field) => write!(f, "it has no {field}"),
            CimdError::NotPublic(m) => write!(
                f,
                "it asks for client authentication ({m}), which Soulbeet does not support"
            ),
        }
    }
}

/// What Soulful keeps of an app's document.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ClientDocument {
    pub client_id: String,
    pub client_name: Option<String>,
    pub redirect_uris: Vec<String>,
}

/// `raw` as a client id URL (draft section 3): https, a path, no dot
/// segment, no fragment, no user name or password, and no IP literal that
/// is not global. The document's `client_id` is compared with the URL as a
/// string (section 4.1), so `raw` must also be written exactly as the URL
/// serializes.
pub(crate) fn client_id_url(raw: &str) -> Result<Url, CimdError> {
    let refused = CimdError::NotAClientIdUrl;
    let url = Url::parse(raw).map_err(|_| refused("a client id URL must be a valid URL"))?;
    if url.scheme() != "https" {
        return Err(refused("a client id URL must use https"));
    }
    // The url crate resolves dot segments, %2e ones included, as it parses.
    if has_dot_segment(raw) {
        return Err(refused("a client id URL must not have dot segments"));
    }
    if url.fragment().is_some() || !url.username().is_empty() || url.password().is_some() {
        return Err(refused(
            "a client id URL must not have a fragment or a user name",
        ));
    }
    if matches!(url.path(), "" | "/") {
        return Err(refused("a client id URL must have a path"));
    }
    // reqwest connects to an IP literal without asking the resolver.
    let literal = match url.host() {
        Some(Host::Ipv4(ip)) => Some(IpAddr::V4(ip)),
        Some(Host::Ipv6(ip)) => Some(IpAddr::V6(ip)),
        _ => None,
    };
    if literal.is_some_and(|ip| !is_global(ip)) {
        return Err(refused(
            "a client id URL must not name a private or local address",
        ));
    }
    if url.as_str() != raw {
        return Err(refused(
            "a client id URL must be written in its canonical form",
        ));
    }
    Ok(url)
}

/// A path segment of `raw` is `.` or `..`, each dot written plain or as `%2e`.
fn has_dot_segment(raw: &str) -> bool {
    let rest = raw.split_once("://").map_or(raw, |(_, rest)| rest);
    let after_authority = rest.find(['/', '?', '#']).map_or("", |at| &rest[at..]);
    let path = after_authority.split(['?', '#']).next().unwrap_or("");
    path.split('/').any(|segment| {
        let segment = segment.to_ascii_lowercase().replace("%2e", ".");
        segment == "." || segment == ".."
    })
}

/// The IPv4 ranges of the IANA special-purpose registry that are not
/// globally reachable, and the multicast and reserved blocks.
const NOT_GLOBAL_V4: &[(Ipv4Addr, u32)] = &[
    (Ipv4Addr::new(0, 0, 0, 0), 8),
    (Ipv4Addr::new(10, 0, 0, 0), 8),
    (Ipv4Addr::new(100, 64, 0, 0), 10),
    (Ipv4Addr::new(127, 0, 0, 0), 8),
    (Ipv4Addr::new(169, 254, 0, 0), 16),
    (Ipv4Addr::new(172, 16, 0, 0), 12),
    (Ipv4Addr::new(192, 0, 0, 0), 24),
    (Ipv4Addr::new(192, 0, 2, 0), 24),
    (Ipv4Addr::new(192, 88, 99, 0), 24),
    (Ipv4Addr::new(192, 168, 0, 0), 16),
    (Ipv4Addr::new(198, 18, 0, 0), 15),
    (Ipv4Addr::new(198, 51, 100, 0), 24),
    (Ipv4Addr::new(203, 0, 113, 0), 24),
    (Ipv4Addr::new(224, 0, 0, 0), 4),
    (Ipv4Addr::new(240, 0, 0, 0), 4),
];

/// The IPv6 ranges of the IANA special-purpose registry that are not
/// globally reachable or that reach IPv4 or local networks, and multicast.
const NOT_GLOBAL_V6: &[(Ipv6Addr, u32)] = &[
    // IPv4-compatible, :: and ::1 among them.
    (Ipv6Addr::new(0, 0, 0, 0, 0, 0, 0, 0), 96),
    // IPv4-mapped.
    (Ipv6Addr::new(0, 0, 0, 0, 0, 0xffff, 0, 0), 96),
    // NAT64, well-known and local-use.
    (Ipv6Addr::new(0x64, 0xff9b, 0, 0, 0, 0, 0, 0), 96),
    (Ipv6Addr::new(0x64, 0xff9b, 1, 0, 0, 0, 0, 0), 48),
    // Discard-only.
    (Ipv6Addr::new(0x100, 0, 0, 0, 0, 0, 0, 0), 64),
    // Teredo.
    (Ipv6Addr::new(0x2001, 0, 0, 0, 0, 0, 0, 0), 32),
    // Benchmarking.
    (Ipv6Addr::new(0x2001, 2, 0, 0, 0, 0, 0, 0), 48),
    // Documentation, the 2001:db8::/32 and 3fff::/20 blocks.
    (Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 0), 32),
    (Ipv6Addr::new(0x3fff, 0, 0, 0, 0, 0, 0, 0), 20),
    // Segment Routing SIDs.
    (Ipv6Addr::new(0x5f00, 0, 0, 0, 0, 0, 0, 0), 16),
    // 6to4.
    (Ipv6Addr::new(0x2002, 0, 0, 0, 0, 0, 0, 0), 16),
    // Unique-local.
    (Ipv6Addr::new(0xfc00, 0, 0, 0, 0, 0, 0, 0), 7),
    // Link-local and the deprecated site-local.
    (Ipv6Addr::new(0xfe80, 0, 0, 0, 0, 0, 0, 0), 10),
    (Ipv6Addr::new(0xfec0, 0, 0, 0, 0, 0, 0, 0), 10),
    // Multicast.
    (Ipv6Addr::new(0xff00, 0, 0, 0, 0, 0, 0, 0), 8),
];

/// Whether `ip` is a public address Soulful may fetch a document from.
pub(crate) fn is_global(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => !NOT_GLOBAL_V4.iter().any(|&(net, len)| {
            let mask = u32::MAX << (32 - len);
            u32::from(ip) & mask == u32::from(net)
        }),
        IpAddr::V6(ip) => !NOT_GLOBAL_V6.iter().any(|&(net, len)| {
            let mask = u128::MAX << (128 - len);
            u128::from(ip) & mask == u128::from(net)
        }),
    }
}

/// The global addresses of `addrs`, or why there are none.
pub(crate) fn keep_global(addrs: Vec<SocketAddr>) -> Result<Vec<SocketAddr>, String> {
    let kept: Vec<SocketAddr> = addrs
        .into_iter()
        .filter(|addr| is_global(addr.ip()))
        .collect();
    if kept.is_empty() {
        return Err("the name resolves only to private or local addresses".to_string());
    }
    Ok(kept)
}

/// The document `body` read at `url`: a JSON object whose `client_id` is
/// `url` as written, for a public client (draft section 4.1), whose
/// `client_name` and redirect URIs follow the rules registration applies
/// (`client_metadata`). The name is optional, as in RFC 7591.
pub(crate) fn parse_document(url: &Url, body: &[u8]) -> Result<ClientDocument, CimdError> {
    let doc = match serde_json::from_slice::<Value>(body) {
        Ok(Value::Object(doc)) => doc,
        Ok(other) => return Err(CimdError::NotJson(format!("found {}", json_kind(&other)))),
        Err(e) => return Err(CimdError::NotJson(e.to_string())),
    };
    let Some(client_id) = doc.get("client_id").and_then(Value::as_str) else {
        return Err(CimdError::MissingField("client_id"));
    };
    if client_id != url.as_str() {
        return Err(CimdError::ClientIdMismatch {
            document: client_id.to_string(),
        });
    }
    if let Some(method) = doc.get("token_endpoint_auth_method") {
        if method != "none" {
            let method = method
                .as_str()
                .map_or_else(|| method.to_string(), str::to_string);
            return Err(CimdError::NotPublic(format!(
                "token_endpoint_auth_method {method}"
            )));
        }
    }
    if let Some(member) = ["client_secret", "client_secret_expires_at"]
        .into_iter()
        .find(|member| doc.contains_key(*member))
    {
        return Err(CimdError::NotPublic(member.to_string()));
    }
    let client = client_metadata(&doc).map_err(|refused| match refused {
        MetadataRefusal::ClientName(rule) | MetadataRefusal::RedirectUris(rule) => {
            CimdError::Invalid(rule)
        }
    })?;
    Ok(ClientDocument {
        client_id: client_id.to_string(),
        client_name: client.client_name,
        redirect_uris: client.redirect_uris,
    })
}

fn json_kind(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "an array",
        Value::Object(_) => "an object",
    }
}

/// A failed request with each of its causes; the URL is left out, since
/// the page names it already.
fn fetch_error(e: reqwest::Error) -> CimdError {
    let e = e.without_url();
    let mut text = e.to_string();
    let mut cause = std::error::Error::source(&e);
    while let Some(c) = cause {
        text.push_str(": ");
        text.push_str(&c.to_string());
        cause = c.source();
    }
    CimdError::Fetch(text)
}

/// Where the client metadata documents come from, a seam for tests.
#[async_trait]
pub(crate) trait ClientDocuments: Send + Sync {
    async fn fetch(&self, url: &Url) -> Result<ClientDocument, CimdError>;
}

/// Fetches documents with a client that follows no redirect, uses no
/// proxy, speaks only https, gives up after 5 seconds and connects only to
/// global addresses (draft section 6.5).
pub(crate) struct CimdFetcher {
    client: reqwest::Client,
}

impl CimdFetcher {
    pub(crate) fn new() -> Result<CimdFetcher, String> {
        let mut headers = HeaderMap::new();
        headers.insert(ACCEPT, HeaderValue::from_static("application/json"));
        reqwest::Client::builder()
            .redirect(Policy::none())
            .no_proxy()
            .https_only(true)
            .timeout(FETCH_TIMEOUT)
            .dns_resolver(Arc::new(GlobalOnly))
            .default_headers(headers)
            .build()
            .map(|client| CimdFetcher { client })
            .map_err(|e| e.to_string())
    }
}

#[async_trait]
impl ClientDocuments for CimdFetcher {
    async fn fetch(&self, url: &Url) -> Result<ClientDocument, CimdError> {
        fetch_with(&self.client, url).await
    }
}

/// The document at `url` fetched with `client`: a 200 answer of at most
/// `DOCUMENT_MAX_BYTES` that `parse_document` accepts.
pub(crate) async fn fetch_with(
    client: &reqwest::Client,
    url: &Url,
) -> Result<ClientDocument, CimdError> {
    let response = client.get(url.clone()).send().await.map_err(fetch_error)?;
    if response.status() != reqwest::StatusCode::OK {
        return Err(CimdError::Status(response.status().as_u16()));
    }
    let body = read_capped(response, DOCUMENT_MAX_BYTES)
        .await
        .map_err(|e| match e {
            CappedBodyError::TooLarge { .. } => CimdError::TooLarge,
            CappedBodyError::Read(e) => fetch_error(e),
        })?;
    parse_document(url, &body)
}

/// Stands in for a `CimdFetcher` that could not be built: every fetch
/// answers the build error.
pub(crate) struct UnbuiltFetcher(pub String);

#[async_trait]
impl ClientDocuments for UnbuiltFetcher {
    async fn fetch(&self, _url: &Url) -> Result<ClientDocument, CimdError> {
        Err(CimdError::Fetch(self.0.clone()))
    }
}

/// Resolves a name to its global addresses only, so the client connects
/// only to an address this checked, however the name answers next time.
struct GlobalOnly;

impl Resolve for GlobalOnly {
    fn resolve(&self, name: Name) -> Resolving {
        let host = name.as_str().to_string();
        Box::pin(async move {
            let addrs = tokio::net::lookup_host((host.as_str(), 0)).await?.collect();
            let kept = keep_global(addrs)?;
            Ok(Box::new(kept.into_iter()) as Addrs)
        })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use axum::http::header::LOCATION;
    use axum::http::{HeaderMap, StatusCode};

    use super::*;
    use crate::connection::test_util::{
        announced_never_sent, serve_on_loopback, streamed, CLAUDE_CODE_DOCUMENT, CLAUDE_CODE_ID,
    };

    #[test]
    fn client_id_urls_follow_the_cimd_rules() {
        assert!(client_id_url(CLAUDE_CODE_ID).is_ok());
        for bad in [
            "http://claude.ai/oauth/x",
            "https://claude.ai",
            "https://claude.ai/",
            "https://claude.ai/a/../b",
            "https://claude.ai/a/./b",
            "https://u:p@claude.ai/x",
            "https://claude.ai/x#f",
            "https://10.0.0.1/x",
            "https://[::1]/x",
            "https://169.254.169.254/latest",
            "https://Claude.ai/oauth/x",
            "https://claude.ai/a/%2e%2e/b",
        ] {
            assert!(client_id_url(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn a_client_id_is_compared_as_written_so_it_must_be_canonical() {
        for (raw, text) in [
            (
                "https://Claude.ai/oauth/x",
                "a client id URL must be written in its canonical form",
            ),
            (
                "https://claude.ai:443/oauth/x",
                "a client id URL must be written in its canonical form",
            ),
            (
                "https://cl%61ude.ai/oauth/x",
                "a client id URL must be written in its canonical form",
            ),
            (
                "https://claude.ai/a/%2e%2e/b",
                "a client id URL must not have dot segments",
            ),
            (
                "https://claude.ai/a/%2E/b",
                "a client id URL must not have dot segments",
            ),
            (
                "https://claude.ai/a/.%2E/b",
                "a client id URL must not have dot segments",
            ),
            ("https://claude.ai", "a client id URL must have a path"),
            (
                "https://[::ffff:a00:1]/x",
                "a client id URL must not name a private or local address",
            ),
        ] {
            assert_eq!(
                client_id_url(raw).map(|url| url.to_string()),
                Err(CimdError::NotAClientIdUrl(text)),
                "{raw}"
            );
        }
    }

    #[test]
    fn only_global_addresses_are_fetched() {
        for (ip, want) in [
            ("10.1.2.3", false),
            ("172.16.0.1", false),
            ("192.168.1.1", false),
            ("127.0.0.1", false),
            ("169.254.169.254", false),
            ("100.64.0.1", false),
            ("0.0.0.0", false),
            ("255.255.255.255", false),
            ("192.0.2.1", false),
            ("198.18.0.1", false),
            ("224.0.0.1", false),
            ("160.79.104.10", true),
            ("1.1.1.1", true),
            ("::1", false),
            ("fc00::1", false),
            ("fe80::1", false),
            ("::ffff:10.0.0.1", false),
            ("::ffff:1.1.1.1", false),
            ("64:ff9b::a00:1", false),
            ("2001:db8::1", false),
            ("ff02::1", false),
            ("2606:4700:4700::1111", true),
            // IPv4-compatible, local-use NAT64, site-local, 6to4, Teredo.
            ("::a00:1", false),
            ("64:ff9b:1::a00:1", false),
            ("fec0::1", false),
            ("2002:a00:1::1", false),
            ("2001:0:4136:e378:8000:63bf:3fff:fdd2", false),
            ("2001:4860:4860::8888", true),
            // Benchmarking, documentation (3fff::/20), Segment Routing SIDs.
            ("2001:2::1", false),
            ("2001:2:0:ffff::1", false),
            ("3fff::1", false),
            ("3fff:fff::1", false),
            ("5f00::1", false),
            ("5f00:ffff::1", false),
            ("2001:3::1", true),
            ("3fff:1000::1", true),
            ("5f01::1", true),
        ] {
            assert_eq!(is_global(ip.parse().unwrap()), want, "{ip}");
        }
    }

    #[test]
    fn the_resolver_keeps_only_global_addresses() {
        let a: SocketAddr = "10.0.0.1:443".parse().unwrap();
        let b: SocketAddr = "1.1.1.1:443".parse().unwrap();
        assert_eq!(keep_global(vec![a, b]), Ok(vec![b]));
        assert_eq!(
            keep_global(vec![a]),
            Err("the name resolves only to private or local addresses".to_string())
        );
    }

    #[test]
    fn a_document_must_be_public_and_name_its_own_url() {
        let url = client_id_url(CLAUDE_CODE_ID).unwrap();
        assert_eq!(
            parse_document(&url, CLAUDE_CODE_DOCUMENT.as_bytes()),
            Ok(ClientDocument {
                client_id: CLAUDE_CODE_ID.into(),
                client_name: Some("Claude Code".into()),
                redirect_uris: vec![
                    "http://localhost/callback".into(),
                    "http://127.0.0.1/callback".into()
                ]
            })
        );
        let other = CLAUDE_CODE_DOCUMENT.replace(
            "claude-code-client-metadata\",\"client_name",
            "x\",\"client_name",
        );
        assert!(matches!(
            parse_document(&url, other.as_bytes()),
            Err(CimdError::ClientIdMismatch { .. })
        ));
        let jwt = CLAUDE_CODE_DOCUMENT.replace("\"none\"", "\"private_key_jwt\"");
        assert!(matches!(
            parse_document(&url, jwt.as_bytes()),
            Err(CimdError::NotPublic(_))
        ));
        let bare = CLAUDE_CODE_DOCUMENT.replace(
            "\"redirect_uris\":[\"http://localhost/callback\",\"http://127.0.0.1/callback\"],",
            "",
        );
        assert_eq!(
            parse_document(&url, bare.as_bytes()),
            Err(CimdError::Invalid(
                "redirect_uris must list 1 to 10 URIs of at most 2,000 characters"
            ))
        );
        // Draft section 4.1: client_secret and client_secret_expires_at MUST NOT be used.
        for member in ["client_secret", "client_secret_expires_at"] {
            let secret = CLAUDE_CODE_DOCUMENT.replace("{", &format!("{{\"{member}\":\"x\","));
            assert_eq!(
                parse_document(&url, secret.as_bytes()),
                Err(CimdError::NotPublic(member.to_string()))
            );
        }
        assert_eq!(
            parse_document(&url, b"[]"),
            Err(CimdError::NotJson("found an array".into()))
        );
    }

    /// Claude Code's document with `member` set to `value`.
    fn with_member(member: &str, value: Value) -> Vec<u8> {
        let mut doc: serde_json::Map<String, Value> =
            serde_json::from_str(CLAUDE_CODE_DOCUMENT).unwrap();
        doc.insert(member.into(), value);
        serde_json::to_vec(&doc).unwrap()
    }

    #[test]
    fn a_document_follows_the_rules_registration_applies() {
        let url = client_id_url(CLAUDE_CODE_ID).unwrap();
        let many: Vec<String> = (0..2000).map(|i| format!("https://c.ai/{i}")).collect();
        for (member, value, rule) in [
            (
                "redirect_uris",
                serde_json::json!(["http://claude.ai/callback"]),
                "redirect URIs must be https, or http on a loopback address, with no fragment",
            ),
            (
                "redirect_uris",
                serde_json::json!(["https://claude.ai/callback#x"]),
                "redirect URIs must be https, or http on a loopback address, with no fragment",
            ),
            (
                "redirect_uris",
                serde_json::json!(many),
                "redirect_uris must list 1 to 10 URIs of at most 2,000 characters",
            ),
            (
                "client_name",
                Value::String("Allow this app. ".repeat(4000)),
                "client_name must be at most 200 characters",
            ),
        ] {
            let body = with_member(member, value);
            assert!(body.len() <= DOCUMENT_MAX_BYTES, "{rule}");
            assert_eq!(
                parse_document(&url, &body),
                Err(CimdError::Invalid(rule)),
                "{rule}"
            );
        }
    }

    #[test]
    fn a_document_without_a_client_name_is_used_unnamed() {
        let url = client_id_url(CLAUDE_CODE_ID).unwrap();
        let mut doc: serde_json::Map<String, Value> =
            serde_json::from_str(CLAUDE_CODE_DOCUMENT).unwrap();
        doc.remove("client_name");
        let parsed = parse_document(&url, &serde_json::to_vec(&doc).unwrap()).unwrap();
        assert_eq!((parsed.client_name, parsed.redirect_uris.len()), (None, 2));
    }

    #[tokio::test]
    async fn a_document_over_the_cap_is_refused() {
        let app = axum::Router::new()
            .route(
                "/streamed",
                axum::routing::get(|| async { streamed(vec![b'a'; DOCUMENT_MAX_BYTES + 1]) }),
            )
            .route(
                "/announced",
                axum::routing::get(|| async { announced_never_sent(DOCUMENT_MAX_BYTES + 1) }),
            );
        let (addr, _server) = serve_on_loopback(app).await;
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        for path in ["/streamed", "/announced"] {
            let url = Url::parse(&format!("http://{addr}{path}")).unwrap();
            let fetched = tokio::time::timeout(Duration::from_secs(2), fetch_with(&client, &url))
                .await
                .unwrap_or_else(|_| panic!("{path} is read past its announced length"));
            assert_eq!(fetched, Err(CimdError::TooLarge), "{path}");
        }
    }

    #[tokio::test]
    async fn a_redirect_is_not_followed_and_a_document_is_read() {
        let doc_requests = Arc::new(AtomicUsize::new(0));
        let seen = doc_requests.clone();
        let app = axum::Router::new()
            .route(
                "/start",
                axum::routing::get(|| async { (StatusCode::FOUND, [(LOCATION, "/doc")]) }),
            )
            .route(
                "/doc",
                axum::routing::get(move |headers: HeaderMap| async move {
                    seen.fetch_add(1, Ordering::SeqCst);
                    let host = headers["host"].to_str().unwrap().to_string();
                    CLAUDE_CODE_DOCUMENT.replace(CLAUDE_CODE_ID, &format!("http://{host}/doc"))
                }),
            );
        let (addr, _server) = serve_on_loopback(app).await;
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .build()
            .unwrap();
        let start = Url::parse(&format!("http://{addr}/start")).unwrap();
        assert_eq!(
            fetch_with(&client, &start).await,
            Err(CimdError::Status(302))
        );
        assert_eq!(doc_requests.load(Ordering::SeqCst), 0);
        let doc = Url::parse(&format!("http://{addr}/doc")).unwrap();
        let fetched = fetch_with(&client, &doc).await.unwrap();
        assert_eq!(
            (fetched.client_id, fetched.client_name),
            (doc.to_string(), Some("Claude Code".to_string()))
        );
    }

    #[tokio::test]
    async fn the_fetcher_connects_to_no_local_name_and_over_https_only() {
        let fetcher = CimdFetcher::new().unwrap();
        let local = Url::parse("https://localhost/oauth/client").unwrap();
        let Err(CimdError::Fetch(text)) = fetcher.fetch(&local).await else {
            panic!("a name that resolves to loopback is fetched");
        };
        assert!(
            text.ends_with("the name resolves only to private or local addresses"),
            "{text}"
        );
        let plain = Url::parse("http://claude.ai/oauth/client").unwrap();
        assert!(matches!(
            fetcher.fetch(&plain).await,
            Err(CimdError::Fetch(_))
        ));
    }

    #[test]
    fn every_refusal_reads_as_a_sentence_after_the_page_prefix() {
        for (e, text) in [
            (
                CimdError::NotAClientIdUrl("a client id URL must have a path"),
                "a client id URL must have a path",
            ),
            (CimdError::Status(404), "it answered HTTP 404"),
            (CimdError::TooLarge, "it is larger than 64 KiB"),
            (
                CimdError::ClientIdMismatch {
                    document: "https://claude.ai/oauth/x".into(),
                },
                "it names another client id: https://claude.ai/oauth/x",
            ),
            (CimdError::MissingField("client_id"), "it has no client_id"),
            (
                CimdError::Fetch(
                    "dns error: the name resolves only to private or local addresses".into(),
                ),
                "Soulbeet could not fetch it",
            ),
            (
                CimdError::Invalid("client_name must be at most 200 characters"),
                "client_name must be at most 200 characters",
            ),
            (
                CimdError::NotPublic("token_endpoint_auth_method private_key_jwt".into()),
                "it asks for client authentication (token_endpoint_auth_method private_key_jwt), \
                 which Soulbeet does not support",
            ),
        ] {
            assert_eq!(e.to_string(), text);
        }
    }
}
