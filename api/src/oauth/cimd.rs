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

/// The largest document Soulful reads.
pub(crate) const DOCUMENT_MAX_BYTES: usize = 64 * 1024;

const FETCH_TIMEOUT: Duration = Duration::from_secs(5);

/// Why an app's client id or document cannot be used. Each text completes
/// "Soulful could not use this app's client metadata at {url}: ".
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum CimdError {
    NotAClientIdUrl(&'static str),
    Fetch(String),
    Status(u16),
    TooLarge,
    NotJson(String),
    ClientIdMismatch { document: String },
    MissingField(&'static str),
    NotPublic(String),
}

impl std::fmt::Display for CimdError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CimdError::NotAClientIdUrl(m) => write!(f, "{m}"),
            CimdError::Fetch(e) => write!(f, "{e}"),
            CimdError::Status(s) => write!(f, "it answered HTTP {s}"),
            CimdError::TooLarge => write!(f, "it is larger than 64 KiB"),
            CimdError::NotJson(e) => write!(f, "it is not a JSON object ({e})"),
            CimdError::ClientIdMismatch { document } => {
                write!(f, "it names another client id: {document}")
            }
            CimdError::MissingField(field) => write!(f, "it has no {field}"),
            CimdError::NotPublic(m) => write!(
                f,
                "it asks for client authentication ({m}), which Soulful does not support"
            ),
        }
    }
}

/// What Soulful keeps of an app's document.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ClientDocument {
    pub client_id: String,
    pub client_name: String,
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
    // Documentation.
    (Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 0), 32),
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
/// `url` as written, with a `client_name` and redirect URIs, for a public
/// client (draft section 4.1).
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
    let Some(client_name) = doc
        .get("client_name")
        .and_then(Value::as_str)
        .filter(|name| !name.is_empty())
    else {
        return Err(CimdError::MissingField("client_name"));
    };
    let Some(redirect_uris) = doc
        .get("redirect_uris")
        .and_then(Value::as_array)
        .filter(|uris| !uris.is_empty())
        .and_then(|uris| {
            uris.iter()
                .map(|uri| uri.as_str().map(str::to_string))
                .collect::<Option<Vec<String>>>()
        })
    else {
        return Err(CimdError::MissingField("redirect_uris"));
    };
    Ok(ClientDocument {
        client_id: client_id.to_string(),
        client_name: client_name.to_string(),
        redirect_uris,
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

/// The body of `response`, refused as soon as it passes `cap` bytes.
pub(crate) async fn read_capped(
    mut response: reqwest::Response,
    cap: usize,
) -> Result<Vec<u8>, CimdError> {
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(fetch_error)? {
        if body.len() + chunk.len() > cap {
            return Err(CimdError::TooLarge);
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
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
    let body = read_capped(response, DOCUMENT_MAX_BYTES).await?;
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
    use crate::connection::test_util::{serve_on_loopback, CLAUDE_CODE_DOCUMENT, CLAUDE_CODE_ID};

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
                client_name: "Claude Code".into(),
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
            Err(CimdError::MissingField("redirect_uris"))
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

    #[tokio::test]
    async fn a_body_over_the_cap_is_refused_while_it_is_read() {
        let app = axum::Router::new()
            .route(
                "/big",
                axum::routing::get(|| async { vec![b'a'; DOCUMENT_MAX_BYTES + 1] }),
            )
            .route(
                "/doc",
                axum::routing::get(|| async { CLAUDE_CODE_DOCUMENT }),
            );
        let (addr, _server) = serve_on_loopback(app).await;
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        let big = client
            .get(format!("http://{addr}/big"))
            .send()
            .await
            .unwrap();
        assert_eq!(
            read_capped(big, DOCUMENT_MAX_BYTES).await,
            Err(CimdError::TooLarge)
        );
        let doc = client
            .get(format!("http://{addr}/doc"))
            .send()
            .await
            .unwrap();
        assert_eq!(
            read_capped(doc, DOCUMENT_MAX_BYTES).await,
            Ok(CLAUDE_CODE_DOCUMENT.as_bytes().to_vec())
        );
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
            (doc.to_string(), "Claude Code".to_string())
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
        assert_eq!(
            fetcher.fetch(&plain).await,
            // reqwest 0.12.28's refusal of a non-https URL under https_only.
            Err(CimdError::Fetch(
                "builder error: URL scheme is not allowed".into()
            ))
        );
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
            (
                CimdError::MissingField("client_name"),
                "it has no client_name",
            ),
            (
                CimdError::NotPublic("token_endpoint_auth_method private_key_jwt".into()),
                "it asks for client authentication (token_endpoint_auth_method private_key_jwt), \
                 which Soulful does not support",
            ),
        ] {
            assert_eq!(e.to_string(), text);
        }
    }
}
