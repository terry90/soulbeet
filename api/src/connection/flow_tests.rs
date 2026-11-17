//! The whole connection flow, driven over TCP by rmcp's own client: the
//! challenge, discovery, registration, sign-in, consent, the PKCE code
//! exchange, `get_status` on both protocol eras, refresh, and the refusals.

use rmcp::model::{CallToolRequestParams, CallToolResult, ClientConfig, ProtocolVersion};
use rmcp::transport::auth::OAuthState;
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use rmcp::transport::{AuthClient, AuthorizationRequest, StreamableHttpClientTransport};
use rmcp::{ClientLifecycleMode, ClientServiceExt, ServiceExt};
use serde_json::Value;
use tower_cookies::CookieManagerLayer;
use url::Url;

use super::test_util::{request_id_of, serve_on_loopback, signed_in, text_of, INITIALIZE};
use super::{router, ConnectionState};
use crate::db::test_db;

/// The loopback redirect URI the test client registers; nothing listens there.
const CALLBACK: &str = "http://127.0.0.1:1/callback";

async fn serve(state: ConnectionState) -> (String, tokio::task::JoinHandle<()>) {
    let (addr, server) = serve_on_loopback(router(state).layer(CookieManagerLayer::new())).await;
    (format!("http://{addr}"), server)
}

/// The `return_to` of a 303 to `/login/return?return_to=..`.
fn return_to_of(location: &str) -> String {
    Url::parse(&format!("http://127.0.0.1{location}"))
        .unwrap()
        .query_pairs()
        .find(|(k, _)| k == "return_to")
        .unwrap()
        .1
        .into_owned()
}

fn form_encoded(s: &str) -> String {
    url::form_urlencoded::byte_serialize(s.as_bytes()).collect()
}

fn json_of(result: &CallToolResult) -> Value {
    serde_json::from_str(text_of(result)).unwrap()
}

/// A tools/list POST to `url` with `bearer` and extra headers, outside rmcp's client.
async fn rpc(
    client: &reqwest::Client,
    url: &str,
    bearer: Option<&str>,
    headers: &[(&str, &str)],
) -> reqwest::Response {
    let mut req = client
        .post(url)
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream")
        .header("mcp-protocol-version", "2025-11-25")
        .body(r#"{"jsonrpc":"2.0","id":9,"method":"tools/list"}"#);
    if let Some(token) = bearer {
        req = req.bearer_auth(token);
    }
    for (name, value) in headers {
        req = req.header(*name, *value);
    }
    req.send().await.unwrap()
}

#[tokio::test]
async fn an_mcp_client_signs_in_lists_tools_calls_get_status_and_refreshes() {
    let db = test_db::migrated().await;
    let cookie = signed_in(&db, "u1", "alice").await;
    let (base, _server) = serve(ConnectionState::for_test(db.clone())).await;
    let mcp = format!("{base}/mcp");
    let browser = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();

    // 1. Unauthenticated call: the challenge.
    let r = browser
        .post(&mcp)
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream")
        .body(INITIALIZE)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 401);
    let challenge = r.headers()["www-authenticate"]
        .to_str()
        .unwrap()
        .to_string();
    assert_eq!(
        challenge,
        format!(
            r#"Bearer error="invalid_token", error_description="No access token was sent", resource_metadata="{base}/.well-known/oauth-protected-resource/mcp", scope="soulful""#
        )
    );

    // 2. Discovery and registration by rmcp's OAuth client.
    let mut oauth = OAuthState::new(&mcp, None).await.unwrap();
    oauth
        .start_authorization(
            AuthorizationRequest::new(CALLBACK)
                .with_client_name("flow test")
                .with_challenge(challenge),
        )
        .await
        .unwrap();
    let auth_url = oauth.get_authorization_url().await.unwrap();
    assert!(auth_url.starts_with(&format!("{base}/oauth/authorize?")));

    // 3. The browser: sent to sign in, comes back with the session, allows.
    let r = browser.get(&auth_url).send().await.unwrap();
    assert_eq!(r.status(), 303);
    let return_to = return_to_of(r.headers()["location"].to_str().unwrap());
    assert_eq!(
        shared::oauth::accepted_return_to(&return_to),
        Some(return_to.as_str())
    );
    let page = browser
        .get(format!("{base}{return_to}"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(page.contains("Connect 127.0.0.1 to Soulbeet") && page.contains("as alice"));
    let r = browser
        .post(format!("{base}/oauth/authorize"))
        .header("cookie", &cookie)
        .form(&[
            ("request", request_id_of(&page).as_str()),
            ("decision", "allow"),
        ])
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 303);
    let callback = r.headers()["location"].to_str().unwrap().to_string();
    assert!(
        callback.starts_with(&format!("{CALLBACK}?code=")),
        "{callback}"
    );
    assert!(
        callback.contains(&format!("iss={}", form_encoded(&base))),
        "{callback}"
    );

    // 4. Code exchange with PKCE (rmcp checks iss against the metadata issuer).
    oauth.handle_callback_url(&callback).await.unwrap();
    let manager = oauth.into_authorization_manager().unwrap();
    let auth = AuthClient::new(reqwest_013::Client::new(), manager);

    // 5. A legacy-era client: initialize, tools/list, tools/call.
    let legacy = ()
        .serve(StreamableHttpClientTransport::with_client(
            auth.clone(),
            StreamableHttpClientTransportConfig::with_uri(mcp.clone()),
        ))
        .await
        .unwrap();
    let tools = legacy.list_all_tools().await.unwrap();
    assert_eq!(
        tools.iter().map(|t| t.name.as_ref()).collect::<Vec<_>>(),
        crate::mcp::server::TOOLS
            .iter()
            .map(|t| t.name)
            .collect::<Vec<_>>()
    );
    assert_eq!(
        tools[0].annotations.as_ref().unwrap().read_only_hint,
        Some(true)
    );
    let status = json_of(
        &legacy
            .call_tool(CallToolRequestParams::new("get_status"))
            .await
            .unwrap(),
    );
    assert_eq!(
        (
            status["ok"].as_bool(),
            status["base_url"].as_str(),
            status["user"]["username"].as_str()
        ),
        (Some(true), Some(base.as_str()), Some("alice"))
    );

    // 6. A 2026-07-28 client: server/discover and per-request metadata, no initialize.
    let modern = ClientConfig::default()
        .serve_with_lifecycle(
            StreamableHttpClientTransport::with_client(
                auth.clone(),
                StreamableHttpClientTransportConfig::with_uri(mcp.clone()),
            ),
            ClientLifecycleMode::Discover {
                preferred_versions: vec![ProtocolVersion::V_2026_07_28],
            },
        )
        .await
        .unwrap();
    assert_eq!(
        json_of(
            &modern
                .call_tool(CallToolRequestParams::new("get_status"))
                .await
                .unwrap()
        )["ok"],
        true
    );

    // 7. Refresh rotates the pair and the new access token works.
    let old_access = auth.get_access_token().await.unwrap();
    auth.auth_manager
        .lock()
        .await
        .refresh_token()
        .await
        .unwrap();
    let new_access = auth.get_access_token().await.unwrap();
    assert_ne!(old_access, new_access);
    assert_eq!(
        json_of(
            &legacy
                .call_tool(CallToolRequestParams::new("get_status"))
                .await
                .unwrap()
        )["ok"],
        true
    );

    // 8. Refused calls: a made-up token, and the real one through another address of the same server.
    assert_eq!(
        rpc(&browser, &mcp, Some(&new_access), &[]).await.status(),
        200
    );
    let r = rpc(&browser, &mcp, Some("made-up"), &[]).await;
    assert_eq!(r.status(), 401);
    assert!(r.headers()["www-authenticate"].to_str().unwrap().contains(
        r#"error="invalid_token", error_description="The access token is unknown, expired or revoked""#
    ));
    let r = rpc(
        &browser,
        &mcp,
        Some(&new_access),
        &[
            ("x-forwarded-host", "tunnel.example.net"),
            ("x-forwarded-proto", "https"),
        ],
    )
    .await;
    assert_eq!(r.status(), 401);
    assert!(r.headers()["www-authenticate"].to_str().unwrap().contains(
        r#"error="invalid_token", error_description="The access token was issued for another address of this server", resource_metadata="https://tunnel.example.net/.well-known/oauth-protected-resource/mcp""#
    ));

    // 9. A password change ends the connection.
    sqlx::query("UPDATE users SET credentials_version = credentials_version + 1 WHERE id = 'u1'")
        .execute(&db)
        .await
        .unwrap();
    assert_eq!(
        rpc(&browser, &mcp, Some(&new_access), &[]).await.status(),
        401
    );
}
