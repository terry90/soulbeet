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

    /// Same host and port, whatever the scheme: a browser on plain http to a
    /// host the rules call https is still on this host.
    pub fn same_host(&self, other: &BaseUrl) -> bool {
        self.host == other.host && self.port == other.port
    }
}

impl std::fmt::Display for BaseUrl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}://{}", self.scheme.as_str(), self.host)?;
        if let Some(port) = self.port {
            write!(f, ":{port}")?;
        }
        Ok(())
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
                "The address must not have a path: Soulful cannot run under a sub-path"
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

/// Work out the address a request was made to. `pin` (from `PUBLIC_URL`)
/// replaces the headers. `authority` is the request URI's, which HTTP/2 sends
/// in place of a Host header.
pub fn derive_base(
    headers: &HeaderMap,
    authority: Option<&str>,
    pin: Option<&BaseUrl>,
) -> Result<BaseUrl, BaseUrlError> {
    if let Some(pin) = pin {
        return Ok(pin.clone());
    }

    let host = if let Some(h) = forwarded_param(headers, "host") {
        h
    } else if let Some(h) = first_element(headers, "x-forwarded-host") {
        h.to_string()
    } else if let Some(h) = first_element(headers, header::HOST.as_str()) {
        h.to_string()
    } else if let Some(h) = authority.map(str::trim).filter(|a| !a.is_empty()) {
        h.to_string()
    } else {
        return Err(BaseUrlError::MissingHost);
    };

    let scheme = if let Some(p) = forwarded_param(headers, "proto") {
        Scheme::parse(&p)?
    } else if let Some(p) = first_element(headers, "x-forwarded-proto") {
        Scheme::parse(p)?
    } else {
        let probe = BaseUrl::from_url(&host_url(Scheme::Http, &host)?, &host)?;
        let local = match probe.host {
            Host::Domain(d) => d == "localhost",
            Host::Ipv4(_) | Host::Ipv6(_) => true,
        };
        if local {
            Scheme::Http
        } else {
            Scheme::Https
        }
    };

    BaseUrl::from_url(&host_url(scheme, &host)?, &host)
}

/// `derive_base` for a request, with `public_url`, the parsed `PUBLIC_URL`.
pub fn request_base_with(
    headers: &HeaderMap,
    uri: &Uri,
    public_url: Option<&Result<BaseUrl, BaseUrlError>>,
) -> Result<BaseUrl, BaseUrlError> {
    let authority = uri.authority().map(|a| a.as_str());
    match public_url {
        None => derive_base(headers, authority, None),
        Some(Ok(pin)) => derive_base(headers, authority, Some(pin)),
        Some(Err(e)) => Err(BaseUrlError::InvalidPublicUrl(e.to_string())),
    }
}

/// `derive_base` for a request, with the configured `PUBLIC_URL`.
pub fn request_base(headers: &HeaderMap, uri: &Uri) -> Result<BaseUrl, BaseUrlError> {
    request_base_with(headers, uri, crate::config::CONFIG.public_url())
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
}
