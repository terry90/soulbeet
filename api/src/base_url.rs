//! The address Soulful is reached at, derived from request headers or pinned by `PUBLIC_URL`.

use axum::http::{header, HeaderMap, Uri};
use url::{Host, Url};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scheme {
    Http,
    Https,
}

impl Scheme {
    fn as_str(self) -> &'static str {
        match self {
            Scheme::Http => "http",
            Scheme::Https => "https",
        }
    }

    fn parse(s: &str) -> Result<Scheme, BaseUrlError> {
        match s.to_ascii_lowercase().as_str() {
            "http" => Ok(Scheme::Http),
            "https" => Ok(Scheme::Https),
            _ => Err(BaseUrlError::InvalidScheme(s.to_string())),
        }
    }
}

/// Scheme plus canonical host and optional non-default port.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BaseUrl {
    scheme: Scheme,
    host: Host<String>,
    port: Option<u16>,
}

impl BaseUrl {
    /// Parse a full address such as the `PUBLIC_URL` value.
    pub fn parse(s: &str) -> Result<BaseUrl, BaseUrlError> {
        let url = Url::parse(s).map_err(|_| BaseUrlError::InvalidHost(s.to_string()))?;
        Self::from_url(&url, s)
    }

    fn from_url(url: &Url, raw: &str) -> Result<BaseUrl, BaseUrlError> {
        let scheme = Scheme::parse(url.scheme())?;
        if !url.username().is_empty() || url.password().is_some() {
            return Err(BaseUrlError::Userinfo);
        }
        if url.path() != "/" && !url.path().is_empty() {
            return Err(BaseUrlError::PathNotAllowed);
        }
        if url.query().is_some() || url.fragment().is_some() {
            return Err(BaseUrlError::InvalidHost(raw.to_string()));
        }
        let host = url
            .host()
            .ok_or_else(|| BaseUrlError::InvalidHost(raw.to_string()))?
            .to_owned();
        Ok(BaseUrl {
            scheme,
            host,
            port: url.port(),
        })
    }

    pub fn is_https(&self) -> bool {
        self.scheme == Scheme::Https
    }

    /// The same host and port over plain http.
    pub fn on_http(&self) -> BaseUrl {
        BaseUrl {
            scheme: Scheme::Http,
            ..self.clone()
        }
    }

    /// The host is `localhost`, an IPv4 address in 127.0.0.0/8 or `::1`.
    pub fn is_loopback(&self) -> bool {
        is_loopback_host(&match &self.host {
            Host::Domain(d) => Host::Domain(d.as_str()),
            Host::Ipv4(a) => Host::Ipv4(*a),
            Host::Ipv6(a) => Host::Ipv6(*a),
        })
    }

    /// Same host and port, whatever the scheme: a browser on plain http to a
    /// host the rules call https is still on this host.
    pub fn same_host(&self, other: &BaseUrl) -> bool {
        self.host == other.host && self.port == other.port
    }

    /// The host, then `:{port}` when the address has a port:
    /// `music.example.com`, `soulful:9765`.
    pub fn host_str(&self) -> String {
        match self.port {
            Some(port) => format!("{}:{port}", self.host),
            None => self.host.to_string(),
        }
    }
}

/// The one loopback rule: the domain `localhost`, any IPv4 address in
/// 127.0.0.0/8, or the IPv6 address `::1`.
pub(crate) fn is_loopback_host(host: &Host<&str>) -> bool {
    match host {
        Host::Domain(d) => *d == "localhost",
        Host::Ipv4(a) => a.is_loopback(),
        Host::Ipv6(a) => a.is_loopback(),
    }
}

impl std::fmt::Display for BaseUrl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}://{}", self.scheme.as_str(), self.host_str())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BaseUrlError {
    MissingHost,
    InvalidHost(String),
    InvalidScheme(String),
    Userinfo,
    PathNotAllowed,
    InvalidPublicUrl(String),
}

impl std::fmt::Display for BaseUrlError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MissingHost => write!(
                f,
                "The request names no host (Forwarded host=, X-Forwarded-Host or Host)"
            ),
            Self::InvalidHost(v) => write!(f, "'{v}' is not a valid host"),
            Self::InvalidScheme(v) => write!(f, "'{v}' is not http or https"),
            Self::Userinfo => write!(f, "The address must not contain a user name or password"),
            Self::PathNotAllowed => write!(
                f,
                "The address must not have a path: Soulbeet cannot run under a sub-path"
            ),
            Self::InvalidPublicUrl(e) => write!(f, "PUBLIC_URL is not usable: {e}"),
        }
    }
}

impl std::error::Error for BaseUrlError {}

/// First comma-separated element of a header, trimmed; `None` when absent or empty.
fn first_element<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    let value = headers.get(name)?.to_str().ok()?;
    let first = value.split(',').next()?.trim();
    (!first.is_empty()).then_some(first)
}

/// A parameter of the first element of `Forwarded`, unquoted.
fn forwarded_param(headers: &HeaderMap, key: &str) -> Option<String> {
    let first = first_element(headers, "forwarded")?;
    first.split(';').find_map(|pair| {
        let (k, v) = pair.split_once('=')?;
        if !k.trim().eq_ignore_ascii_case(key) {
            return None;
        }
        let v = v.trim().trim_matches('"').trim();
        (!v.is_empty()).then(|| v.to_string())
    })
}

fn host_url(scheme: Scheme, host: &str) -> Result<Url, BaseUrlError> {
    Url::parse(&format!("{}://{host}", scheme.as_str()))
        .map_err(|_| BaseUrlError::InvalidHost(host.to_string()))
}

/// Which part of the request gave the host.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HostSource {
    Forwarded,
    XForwardedHost,
    Host,
    /// The request URI's authority, which HTTP/2 sends in place of a Host header.
    Authority,
    PublicUrl,
    Missing,
}

impl HostSource {
    pub fn as_str(self) -> &'static str {
        match self {
            HostSource::Forwarded => "forwarded",
            HostSource::XForwardedHost => "x_forwarded_host",
            HostSource::Host => "host",
            HostSource::Authority => "authority",
            HostSource::PublicUrl => "public_url",
            HostSource::Missing => "missing",
        }
    }
}

/// Which part of the request, or which rule, gave the scheme.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SchemeSource {
    Forwarded,
    XForwardedProto,
    /// No hint was sent and the host is `localhost` or an IP literal.
    LocalHostIsHttp,
    /// No hint was sent and the host is a name other than `localhost`.
    NamedHostIsHttps,
    PublicUrl,
    /// The host could not be read, so neither could the scheme.
    Unread,
}

impl SchemeSource {
    pub fn as_str(self) -> &'static str {
        match self {
            SchemeSource::Forwarded => "forwarded",
            SchemeSource::XForwardedProto => "x_forwarded_proto",
            SchemeSource::LocalHostIsHttp => "local_host_is_http",
            SchemeSource::NamedHostIsHttps => "named_host_is_https",
            SchemeSource::PublicUrl => "public_url",
            SchemeSource::Unread => "unread",
        }
    }
}

/// The derived address together with where its host and scheme came from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BaseExplanation {
    pub result: Result<BaseUrl, BaseUrlError>,
    pub host_from: HostSource,
    pub scheme_from: SchemeSource,
}

/// Work out the address a request was made to. `pin` (from `PUBLIC_URL`)
/// replaces the headers. `authority` is the request URI's, which HTTP/2 sends
/// in place of a Host header.
pub fn derive_base(
    headers: &HeaderMap,
    authority: Option<&str>,
    pin: Option<&BaseUrl>,
) -> Result<BaseUrl, BaseUrlError> {
    explain_base(headers, authority, pin).result
}

/// `derive_base`, also saying which header or rule gave the host and the scheme.
pub fn explain_base(
    headers: &HeaderMap,
    authority: Option<&str>,
    pin: Option<&BaseUrl>,
) -> BaseExplanation {
    explain_base_with_proxy(headers, authority, pin, true)
}

/// Like `explain_base`, but only honors `Forwarded`, `X-Forwarded-Host`, and `X-Forwarded-Proto`
/// when `trust_proxy` is true. When `trust_proxy` is false and `pin` is `None`, proxy headers
/// are ignored so untrusted peers cannot spoof `X-Forwarded-Proto: https`.
pub fn explain_base_with_proxy(
    headers: &HeaderMap,
    authority: Option<&str>,
    pin: Option<&BaseUrl>,
    trust_proxy: bool,
) -> BaseExplanation {
    let explained = |result, host_from, scheme_from| BaseExplanation {
        result,
        host_from,
        scheme_from,
    };

    if let Some(pin) = pin {
        return explained(
            Ok(pin.clone()),
            HostSource::PublicUrl,
            SchemeSource::PublicUrl,
        );
    }

    let (host, host_from) = if trust_proxy {
        if let Some(h) = forwarded_param(headers, "host") {
            (h, HostSource::Forwarded)
        } else if let Some(h) = first_element(headers, "x-forwarded-host") {
            (h.to_string(), HostSource::XForwardedHost)
        } else if let Some(h) = first_element(headers, header::HOST.as_str()) {
            (h.to_string(), HostSource::Host)
        } else if let Some(h) = authority.map(str::trim).filter(|a| !a.is_empty()) {
            (h.to_string(), HostSource::Authority)
        } else {
            return explained(
                Err(BaseUrlError::MissingHost),
                HostSource::Missing,
                SchemeSource::Unread,
            );
        }
    } else if let Some(h) = first_element(headers, header::HOST.as_str()) {
        (h.to_string(), HostSource::Host)
    } else if let Some(h) = authority.map(str::trim).filter(|a| !a.is_empty()) {
        (h.to_string(), HostSource::Authority)
    } else {
        return explained(
            Err(BaseUrlError::MissingHost),
            HostSource::Missing,
            SchemeSource::Unread,
        );
    };

    let (scheme, scheme_from) = if trust_proxy {
        if let Some(p) = forwarded_param(headers, "proto") {
            (Scheme::parse(&p), SchemeSource::Forwarded)
        } else if let Some(p) = first_element(headers, "x-forwarded-proto") {
            (Scheme::parse(p), SchemeSource::XForwardedProto)
        } else {
            let probe = host_url(Scheme::Http, &host).and_then(|url| BaseUrl::from_url(&url, &host));
            match probe {
                Err(e) => return explained(Err(e), host_from, SchemeSource::Unread),
                Ok(probe) => match probe.host {
                    Host::Domain(d) if d != "localhost" => {
                        (Ok(Scheme::Https), SchemeSource::NamedHostIsHttps)
                    }
                    _ => (Ok(Scheme::Http), SchemeSource::LocalHostIsHttp),
                },
            }
        }
    } else {
        let probe = host_url(Scheme::Http, &host).and_then(|url| BaseUrl::from_url(&url, &host));
        match probe {
            Err(e) => return explained(Err(e), host_from, SchemeSource::Unread),
            Ok(probe) => match probe.host {
                Host::Domain(d) if d != "localhost" => {
                    (Ok(Scheme::Https), SchemeSource::NamedHostIsHttps)
                }
                _ => (Ok(Scheme::Http), SchemeSource::LocalHostIsHttp),
            },
        }
    };

    let result = scheme.and_then(|scheme| BaseUrl::from_url(&host_url(scheme, &host)?, &host));
    explained(result, host_from, scheme_from)
}

/// `derive_base` for a request, with `public_url`, the parsed `PUBLIC_URL`.
pub fn request_base_with(
    headers: &HeaderMap,
    uri: &Uri,
    public_url: Option<&Result<BaseUrl, BaseUrlError>>,
) -> Result<BaseUrl, BaseUrlError> {
    explain_request(headers, uri, public_url).result
}

/// `request_base_with`, also saying where host and scheme came from. An
/// unusable `PUBLIC_URL` is the source of both.
pub fn explain_request(
    headers: &HeaderMap,
    uri: &Uri,
    public_url: Option<&Result<BaseUrl, BaseUrlError>>,
) -> BaseExplanation {
    explain_request_with_proxy(headers, uri, public_url, true)
}

/// Like `explain_request`, gating proxy header trust behind `trust_proxy`.
pub fn explain_request_with_proxy(
    headers: &HeaderMap,
    uri: &Uri,
    public_url: Option<&Result<BaseUrl, BaseUrlError>>,
    trust_proxy: bool,
) -> BaseExplanation {
    match public_url {
        None => explain_base_with_proxy(headers, authority(uri), None, trust_proxy),
        Some(Ok(pin)) => explain_base_with_proxy(headers, authority(uri), Some(pin), trust_proxy),
        Some(Err(e)) => BaseExplanation {
            result: Err(BaseUrlError::InvalidPublicUrl(e.to_string())),
            host_from: HostSource::PublicUrl,
            scheme_from: SchemeSource::PublicUrl,
        },
    }
}

/// The address the request itself was sent to, from its headers, whatever
/// `PUBLIC_URL` pins: a page opened at a LAN address talks to that address.
pub fn addressed_to(headers: &HeaderMap, uri: &Uri) -> Result<BaseUrl, BaseUrlError> {
    derive_base(headers, authority(uri), None)
}

fn authority(uri: &Uri) -> Option<&str> {
    uri.authority().map(|a| a.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::{HeaderName, HeaderValue};

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (k, v) in pairs {
            map.insert(
                HeaderName::from_bytes(k.as_bytes()).unwrap(),
                HeaderValue::from_str(v).unwrap(),
            );
        }
        map
    }

    fn base(pairs: &[(&str, &str)]) -> String {
        derive_base(&headers(pairs), None, None)
            .unwrap()
            .to_string()
    }

    #[test]
    fn named_host_without_proxy_hint_is_https() {
        assert_eq!(
            base(&[("host", "music.example.com")]),
            "https://music.example.com"
        );
    }

    #[test]
    fn localhost_and_ip_literals_without_hint_are_http() {
        assert_eq!(base(&[("host", "localhost:9765")]), "http://localhost:9765");
        assert_eq!(
            base(&[("host", "192.168.1.5:9765")]),
            "http://192.168.1.5:9765"
        );
        assert_eq!(base(&[("host", "[::1]:9765")]), "http://[::1]:9765");
    }

    #[test]
    fn x_forwarded_headers_win_over_host() {
        assert_eq!(
            base(&[
                ("host", "soulful:9765"),
                ("x-forwarded-host", "music.example.com"),
                ("x-forwarded-proto", "https"),
            ]),
            "https://music.example.com"
        );
    }

    #[test]
    fn forwarded_wins_over_x_forwarded() {
        assert_eq!(
            base(&[
                (
                    "forwarded",
                    "for=1.2.3.4;host=\"a.example:8443\";proto=https",
                ),
                ("x-forwarded-host", "b.example"),
                ("x-forwarded-proto", "http"),
            ]),
            "https://a.example:8443"
        );
    }

    #[test]
    fn first_value_of_comma_lists_wins() {
        assert_eq!(
            base(&[
                ("x-forwarded-host", "a.example, b.example"),
                ("x-forwarded-proto", "https, http"),
            ]),
            "https://a.example"
        );
        assert_eq!(
            base(&[(
                "forwarded",
                "host=a.example;proto=https, host=b.example;proto=http"
            )]),
            "https://a.example"
        );
    }

    #[test]
    fn explicit_http_hint_is_kept_for_a_named_host() {
        assert_eq!(
            base(&[("host", "music.example.com"), ("x-forwarded-proto", "HTTP")]),
            "http://music.example.com"
        );
    }

    #[test]
    fn host_is_lowercased_and_default_port_dropped() {
        assert_eq!(
            base(&[
                ("host", "Music.Example.COM:443"),
                ("x-forwarded-proto", "https")
            ]),
            "https://music.example.com"
        );
        assert_eq!(
            base(&[
                ("host", "music.example.com:80"),
                ("x-forwarded-proto", "http")
            ]),
            "http://music.example.com"
        );
    }

    #[test]
    fn bad_values_are_refused() {
        let err = |pairs: &[(&str, &str)]| derive_base(&headers(pairs), None, None).unwrap_err();
        assert_eq!(err(&[("host", "user@host")]), BaseUrlError::Userinfo);
        assert_eq!(
            err(&[("host", "a.example"), ("x-forwarded-proto", "ftp")]),
            BaseUrlError::InvalidScheme("ftp".into())
        );
        assert_eq!(err(&[]), BaseUrlError::MissingHost);
        assert_eq!(
            err(&[("host", "bad host")]),
            BaseUrlError::InvalidHost("bad host".into())
        );
    }

    #[test]
    fn public_url_pin_replaces_derivation() {
        let pin = BaseUrl::parse("https://pinned.example").unwrap();
        let d = derive_base(&headers(&[("host", "other.example")]), None, Some(&pin)).unwrap();
        assert_eq!(d.to_string(), "https://pinned.example");
    }

    #[test]
    fn an_http2_request_without_host_uses_the_uri_authority() {
        // h2c through a proxy: `:authority` lands in the URI, no Host header.
        let uri: Uri = "http://music.example.com:8443/api/auth/login"
            .parse()
            .unwrap();
        let base = request_base_with(&HeaderMap::new(), &uri, None).unwrap();
        assert_eq!(base.to_string(), "https://music.example.com:8443");

        // A Host header still wins over the authority.
        let host = headers(&[("host", "192.168.1.5:9765")]);
        let base = request_base_with(&host, &uri, None).unwrap();
        assert_eq!(base.to_string(), "http://192.168.1.5:9765");

        // HTTP/1 sends only a path, so nothing is left to use.
        let path_only: Uri = "/api/auth/login".parse().unwrap();
        assert_eq!(
            request_base_with(&HeaderMap::new(), &path_only, None).unwrap_err(),
            BaseUrlError::MissingHost
        );
    }

    #[test]
    fn same_host_ignores_the_scheme_only() {
        let a = |s: &str| BaseUrl::parse(s).unwrap();
        assert!(a("http://nas:9765").same_host(&a("https://nas:9765")));
        assert!(!a("http://nas:9765").same_host(&a("http://nas:9766")));
        assert!(!a("https://evil.example").same_host(&a("https://music.example.com")));
    }

    #[test]
    fn the_explanation_names_the_headers_the_base_came_from() {
        let e = explain_base(
            &headers(&[
                ("host", "soulful:9765"),
                ("x-forwarded-host", "music.example.com"),
                ("x-forwarded-proto", "https"),
            ]),
            None,
            None,
        );
        assert_eq!(e.result.unwrap().to_string(), "https://music.example.com");
        assert_eq!(
            (e.host_from, e.scheme_from),
            (HostSource::XForwardedHost, SchemeSource::XForwardedProto)
        );
        let e = explain_base(
            &headers(&[("forwarded", "host=a.example;proto=https")]),
            None,
            None,
        );
        assert_eq!(
            (e.host_from, e.scheme_from),
            (HostSource::Forwarded, SchemeSource::Forwarded)
        );
        let e = explain_base(&headers(&[("host", "music.example.com")]), None, None);
        assert_eq!(
            (e.host_from, e.scheme_from),
            (HostSource::Host, SchemeSource::NamedHostIsHttps)
        );
        let e = explain_base(&headers(&[("host", "192.168.1.5:9765")]), None, None);
        assert_eq!(e.scheme_from, SchemeSource::LocalHostIsHttp);
        let e = explain_base(&HeaderMap::new(), Some("music.example.com"), None);
        assert_eq!(
            (e.host_from, e.scheme_from),
            (HostSource::Authority, SchemeSource::NamedHostIsHttps)
        );
        let pin = BaseUrl::parse("https://pinned.example").unwrap();
        let e = explain_base(&headers(&[("host", "other.example")]), None, Some(&pin));
        assert_eq!(
            (e.host_from, e.scheme_from),
            (HostSource::PublicUrl, SchemeSource::PublicUrl)
        );
        let e = explain_base(&HeaderMap::new(), None, None);
        assert_eq!(
            (e.result, e.host_from, e.scheme_from),
            (
                Err(BaseUrlError::MissingHost),
                HostSource::Missing,
                SchemeSource::Unread
            )
        );
        let e = explain_base(&headers(&[("host", "bad host")]), None, None);
        assert_eq!(
            (e.result, e.host_from, e.scheme_from),
            (
                Err(BaseUrlError::InvalidHost("bad host".into())),
                HostSource::Host,
                SchemeSource::Unread
            )
        );
        let unusable = BaseUrl::parse("music.example.com");
        let uri: Uri = "/oauth/token".parse().unwrap();
        let e = explain_request(
            &headers(&[("host", "music.example.com")]),
            &uri,
            Some(&unusable),
        );
        assert_eq!(
            (e.result, e.host_from, e.scheme_from),
            (
                Err(BaseUrlError::InvalidPublicUrl(
                    "'music.example.com' is not a valid host".into()
                )),
                HostSource::PublicUrl,
                SchemeSource::PublicUrl
            )
        );
    }

    #[test]
    fn only_localhost_and_loopback_addresses_are_loopback() {
        for (s, want) in [
            ("http://localhost:9765", true),
            ("http://127.0.0.1:9765", true),
            ("http://127.1.2.3", true),
            ("http://[::1]:9765", true),
            ("http://192.168.1.5:9765", false),
            ("https://music.example.com", false),
            ("http://localhost.example.com", false),
        ] {
            assert_eq!(BaseUrl::parse(s).unwrap().is_loopback(), want, "{s}");
        }
    }

    #[test]
    fn public_url_refuses_a_path() {
        assert_eq!(
            BaseUrl::parse("https://x.example/soulful").unwrap_err(),
            BaseUrlError::PathNotAllowed
        );
        assert_eq!(
            BaseUrl::parse("https://x.example/").unwrap().to_string(),
            "https://x.example"
        );
    }

    #[test]
    fn untrusted_proxy_headers_are_ignored_when_trust_proxy_is_false() {
        let h = headers(&[
            ("host", "192.168.1.5:9765"),
            ("x-forwarded-proto", "https"),
            ("x-forwarded-host", "spoof.example.com"),
        ]);
        let e = explain_base_with_proxy(&h, None, None, false);
        assert_eq!(e.result.unwrap().to_string(), "http://192.168.1.5:9765");
        assert_eq!(e.host_from, HostSource::Host);
        assert_eq!(e.scheme_from, SchemeSource::LocalHostIsHttp);
    }
}
