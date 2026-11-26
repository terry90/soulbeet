//! The apps connected through OAuth, and how Soulful sees its own address,
//! for Settings.

use dioxus::prelude::*;

use crate::connections::{ConnectedApp, RemoteAccess};

#[cfg(feature = "server")]
use crate::{
    base_url::{explain_request, BaseExplanation, BaseUrl, HostSource},
    config::CONFIG,
    connection::log::{base_and_problem, LIVE_LOG},
    connection::GATE_CLOSED,
    core::database_failure,
    db::DB,
    oauth::{grants, metadata::mcp_url},
    server_fns::failure_error,
    AdminSession, AuthSession,
};
#[cfg(feature = "server")]
use axum::http::{HeaderMap, Uri};
#[cfg(feature = "server")]
use shared::failure::{Failure, FailureCode, RetryAdvice, Stage};

/// The caller's connected apps; an admin's list holds every user's.
#[get("/api/connected-apps", auth: AuthSession)]
pub async fn get_connected_apps() -> Result<Vec<ConnectedApp>, ServerFnError> {
    grants::apps_in(&DB, &auth.0).await.map_err(database_error)
}

/// Lets the caller's own app delete music, or stops it.
#[post("/api/connected-apps/deletion", auth: AuthSession)]
pub async fn set_app_deletion(grant_id: String, allowed: bool) -> Result<(), ServerFnError> {
    match grants::set_allow_delete_in(&DB, &auth.0, &grant_id, allowed).await {
        Ok(true) => Ok(()),
        Ok(false) => Err(app_not_found()),
        Err(e) => Err(database_error(e)),
    }
}

/// Lets an admin's own connected app access all users' library folders (or turns it off).
/// Only an admin can toggle this, and only grants belonging to an admin user can enable it.
#[post("/api/connected-apps/admin", auth: AdminSession)]
pub async fn set_grant_allow_admin(grant_id: String, allowed: bool) -> Result<(), ServerFnError> {
    match grants::set_allow_admin_in(&DB, &auth.0, &grant_id, allowed).await {
        Ok(true) => Ok(()),
        Ok(false) => Err(app_not_found()),
        Err(e) => Err(database_error(e)),
    }
}

/// Disconnects an app of the caller's, or any user's for an admin.
#[post("/api/connected-apps/revoke", auth: AuthSession)]
pub async fn revoke_connected_app(grant_id: String) -> Result<(), ServerFnError> {
    match grants::revoke_grant_in(&DB, &auth.0, &grant_id).await {
        Ok(true) => Ok(()),
        Ok(false) => Err(app_not_found()),
        Err(e) => Err(database_error(e)),
    }
}

/// The address Soulful derives for this request, what to change for MCP clients
/// to connect when the browser is at `browser_origin`, and the latest
/// connection requests.
#[post("/api/remote-access", _: AdminSession, headers: HeaderMap, uri: Uri)]
pub async fn get_remote_access(browser_origin: String) -> Result<RemoteAccess, ServerFnError> {
    let explained = explain_request(&headers, &uri, CONFIG.public_url());
    let closed = closed_because(
        CONFIG.mcp_off(),
        !crate::security::gate_reasons().is_empty(),
    );
    let advice = remote_access_advice(&browser_origin, &explained, &closed);
    let (derived_base, base_problem) = base_and_problem(&explained.result);
    Ok(RemoteAccess {
        derived_base,
        base_problem,
        host_from: explained.host_from.as_str().into(),
        scheme_from: explained.scheme_from.as_str().into(),
        advice,
        recent: LIVE_LOG.recent(),
    })
}

#[cfg(feature = "server")]
fn app_not_found() -> ServerFnError {
    failure_error(Failure::new(
        FailureCode::UnknownHandle,
        Stage::Input,
        RetryAdvice::NotRetryable,
        "Connected app not found",
    ))
}

#[cfg(feature = "server")]
fn database_error(e: String) -> ServerFnError {
    failure_error(database_failure(Stage::Input, e))
}

/// What keeps the connection paths closed whatever the address: the `MCP`
/// switch (`mcp_off` says why) and a failing security check, as the
/// connection router words them.
#[cfg(feature = "server")]
fn closed_because(mcp_off: Option<&str>, gate_fails: bool) -> Vec<String> {
    let mut closed = Vec::new();
    if let Some(why) = mcp_off {
        closed.push(format!("MCP access is turned off: {why}."));
    }
    if gate_fails {
        closed.push(format!("{GATE_CLOSED}."));
    }
    closed
}

/// The advice for a browser at `browser_origin`: what keeps the paths
/// closed, then what keeps MCP clients from the address `explained` derives.
#[cfg(feature = "server")]
fn remote_access_advice(
    browser_origin: &str,
    explained: &BaseExplanation,
    closed: &[String],
) -> Vec<String> {
    let mut advice = closed.to_vec();
    match BaseUrl::parse(browser_origin) {
        Ok(browser) => advice.extend(address_advice(&browser, explained, closed.is_empty())),
        Err(e) => advice.push(format!("Your browser's address could not be read: {e}")),
    }
    advice
}

/// What keeps MCP clients from connecting at the address `explained` derives,
/// for a browser at `browser`, or, when the paths are `open`, that it can
/// connect.
#[cfg(feature = "server")]
pub(crate) fn address_advice(
    browser: &BaseUrl,
    explained: &BaseExplanation,
    open: bool,
) -> Vec<String> {
    let mut advice = Vec::new();
    let base = match &explained.result {
        Ok(base) => base,
        Err(e) => {
            advice.push(format!("Soulbeet cannot tell this request's address: {e}"));
            return advice;
        }
    };
    if explained.host_from == HostSource::PublicUrl {
        advice.push(format!(
            "PUBLIC_URL pins the address to {base}; request headers are not read."
        ));
    }
    let (browser_host, base_host) = (browser.host_str(), base.host_str());
    let proxy_names = |header: &str| {
        format!(
            "Your browser is on {browser_host}, but the proxy names {base_host} in {header}. \
             Check the proxy's forwarded host."
        )
    };
    let host_problem = match explained.host_from {
        _ if browser.same_host(base) => None,
        HostSource::Host | HostSource::Authority => Some(format!(
            "Your browser is on {browser_host}, but the request reached Soulbeet naming \
             {base_host}. Have the proxy send X-Forwarded-Host (or Forwarded host=) with the \
             public name."
        )),
        HostSource::XForwardedHost => Some(proxy_names("X-Forwarded-Host")),
        HostSource::Forwarded => Some(proxy_names("Forwarded")),
        // PUBLIC_URL is the address whatever host the browser is on, and a
        // missing host derives no address.
        HostSource::PublicUrl | HostSource::Missing => None,
    };
    let mut problem = host_problem.is_some();
    advice.extend(host_problem);
    if browser.is_https() && !base.is_https() {
        advice.push(
            "Your browser uses https, but Soulbeet sees http: have the proxy send \
             X-Forwarded-Proto: https (or Forwarded proto=https)."
                .into(),
        );
        problem = true;
    }
    if !base.is_https() && !base.is_loopback() {
        advice.push("MCP clients cannot use this address: it is plain http.".into());
        problem = true;
    }
    if base.is_https() && !problem && open {
        advice.push(format!(
            "This address works for MCP clients: add {} as a remote MCP server in your AI assistant.",
            mcp_url(base)
        ));
    }
    advice
}

#[cfg(all(test, feature = "server"))]
mod tests {
    use axum::http::HeaderMap;

    use super::{address_advice, closed_because, remote_access_advice};
    use crate::base_url::{explain_base, BaseUrl};
    use crate::connection::test_util::header_map;

    const WORKS: &str =
        "This address works for MCP clients: add https://music.example.com/mcp as a remote MCP server in your AI assistant.";

    fn advice(pairs: &[(&str, &str)], authority: Option<&str>) -> Vec<String> {
        let browser = BaseUrl::parse("https://music.example.com").unwrap();
        address_advice(
            &browser,
            &explain_base(&header_map(pairs), authority, None),
            true,
        )
    }

    #[test]
    fn advice_names_the_missing_proxy_header() {
        let browser = BaseUrl::parse("https://music.example.com").unwrap();
        // soulful:9765 is the upstream name a plain proxy_pass sends as Host
        assert_eq!(advice(&[("host", "soulful:9765"), ("x-forwarded-proto", "https")], None), vec![
            "Your browser is on music.example.com, but the request reached Soulbeet naming soulful:9765. Have the proxy send X-Forwarded-Host (or Forwarded host=) with the public name.".to_string()]);
        assert_eq!(advice(&[("host", "music.example.com"), ("x-forwarded-proto", "http")], None), vec![
            "Your browser uses https, but Soulbeet sees http: have the proxy send X-Forwarded-Proto: https (or Forwarded proto=https).".to_string(),
            "MCP clients cannot use this address: it is plain http.".to_string()]);
        assert_eq!(advice(&[
                ("host", "soulful:9765"),
                ("x-forwarded-host", "old.example.com"),
                ("x-forwarded-proto", "https"),
            ], None), vec![
            "Your browser is on music.example.com, but the proxy names old.example.com in X-Forwarded-Host. Check the proxy's forwarded host.".to_string()]);
        assert_eq!(
            advice(
                &[
                    ("host", "soulful:9765"),
                    ("x-forwarded-host", "music.example.com"),
                    ("x-forwarded-proto", "https"),
                ],
                None
            ),
            vec![WORKS.to_string()]
        );
        let pin = BaseUrl::parse("https://music.example.com").unwrap();
        let pinned = explain_base(&header_map(&[("host", "soulful:9765")]), None, Some(&pin));
        assert_eq!(address_advice(&browser, &pinned, true), vec![
            "PUBLIC_URL pins the address to https://music.example.com; request headers are not read.".to_string(),
            WORKS.to_string()]);
        let none = explain_base(&HeaderMap::new(), None, None);
        assert_eq!(address_advice(&browser, &none, true), vec![
            "Soulbeet cannot tell this request's address: The request names no host (Forwarded host=, X-Forwarded-Host or Host)".to_string()]);
    }

    #[test]
    fn advice_reads_the_forwarded_header_and_the_request_authority() {
        assert_eq!(
            advice(
                &[
                    ("host", "soulful:9765"),
                    ("forwarded", "host=old.example.com;proto=https"),
                ],
                None
            ),
            vec!["Your browser is on music.example.com, but the proxy names old.example.com in Forwarded. Check the proxy's forwarded host.".to_string()]
        );
        assert_eq!(
            advice(
                &[
                    ("host", "soulful:9765"),
                    ("forwarded", "host=music.example.com;proto=https"),
                ],
                None
            ),
            vec![WORKS.to_string()]
        );
        // HTTP/2 names the host in the request's authority, not in Host.
        assert_eq!(
            advice(&[("x-forwarded-proto", "https")], Some("soulful:9765")),
            vec!["Your browser is on music.example.com, but the request reached Soulbeet naming soulful:9765. Have the proxy send X-Forwarded-Host (or Forwarded host=) with the public name.".to_string()]
        );
        assert_eq!(
            advice(&[("x-forwarded-proto", "https")], Some("music.example.com")),
            vec![WORKS.to_string()]
        );
    }

    #[test]
    fn a_good_address_is_not_called_working_while_the_paths_are_closed() {
        let good = explain_base(
            &header_map(&[
                ("host", "soulful:9765"),
                ("x-forwarded-host", "music.example.com"),
                ("x-forwarded-proto", "https"),
            ]),
            None,
            None,
        );
        let closed = closed_because(Some("MCP=off is set"), true);
        assert_eq!(
            remote_access_advice("https://music.example.com", &good, &closed),
            vec![
                "MCP access is turned off: MCP=off is set.".to_string(),
                "MCP access stays off until the security checks in Settings pass.".to_string(),
            ]
        );
        assert_eq!(
            remote_access_advice(
                "https://music.example.com",
                &good,
                &closed_because(None, false)
            ),
            vec![WORKS.to_string()]
        );
    }

    #[test]
    fn an_unreadable_browser_address_is_named() {
        let good = explain_base(&header_map(&[("host", "music.example.com")]), None, None);
        let advice = remote_access_advice("music.example.com", &good, &[]);
        assert_eq!(advice.len(), 1);
        assert!(
            advice[0].starts_with("Your browser's address could not be read: "),
            "{advice:?}"
        );
    }
}
