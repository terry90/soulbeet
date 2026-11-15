//! The discovery documents: protected resource metadata (RFC 9728) for
//! `/mcp` and authorization server metadata (RFC 8414), both naming the
//! address the request came to.

use serde_json::{json, Value};

use super::{OFFLINE_ACCESS, SCOPE};
use crate::base_url::BaseUrl;

/// The MCP endpoint at `base`, the resource every token is issued for.
pub(crate) fn mcp_url(base: &BaseUrl) -> String {
    format!("{base}/mcp")
}

/// RFC 9728 metadata of the resource `mcp_url(base)`.
pub(crate) fn protected_resource(base: &BaseUrl) -> Value {
    json!({
        "resource": mcp_url(base),
        "authorization_servers": [base.to_string()],
        "scopes_supported": [SCOPE],
        "bearer_methods_supported": ["header"],
    })
}

/// RFC 8414 metadata of the authorization server whose issuer is `base`.
pub(crate) fn authorization_server(base: &BaseUrl) -> Value {
    json!({
        "issuer": base.to_string(),
        "authorization_endpoint": format!("{base}/oauth/authorize"),
        "token_endpoint": format!("{base}/oauth/token"),
        "registration_endpoint": format!("{base}/oauth/register"),
        "revocation_endpoint": format!("{base}/oauth/revoke"),
        "scopes_supported": [SCOPE, OFFLINE_ACCESS],
        "response_types_supported": ["code"],
        "response_modes_supported": ["query"],
        "grant_types_supported": ["authorization_code", "refresh_token"],
        "token_endpoint_auth_methods_supported": ["none"],
        "revocation_endpoint_auth_methods_supported": ["none"],
        "code_challenge_methods_supported": ["S256"],
        "authorization_response_iss_parameter_supported": true,
        "client_id_metadata_document_supported": true,
    })
}

#[cfg(test)]
mod tests {
    use axum::http::StatusCode;
    use axum::Router;
    use serde_json::json;

    use crate::connection::test_util::*;
    use crate::connection::{router, ConnectionState};

    async fn app() -> Router {
        router(ConnectionState::for_test())
    }

    #[tokio::test]
    async fn the_protected_resource_document_names_this_address() {
        for path in [
            "/.well-known/oauth-protected-resource/mcp",
            "/.well-known/oauth-protected-resource",
        ] {
            let (status, _, body) = get_json(app().await, path, &[("host", HOST)]).await;
            assert_eq!(status, StatusCode::OK, "{path}");
            assert_eq!(
                body,
                json!({
                    "resource": "https://music.example.com/mcp",
                    "authorization_servers": ["https://music.example.com"],
                    "scopes_supported": ["soulful"],
                    "bearer_methods_supported": ["header"]
                }),
                "{path}"
            );
        }
    }

    #[tokio::test]
    async fn the_authorization_server_document_lists_what_soulful_supports() {
        let (status, _, body) = get_json(
            app().await,
            "/.well-known/oauth-authorization-server",
            &[
                ("host", "Music.Example.com"),
                ("x-forwarded-proto", "https"),
            ],
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            body,
            json!({
                "issuer": "https://music.example.com",
                "authorization_endpoint": "https://music.example.com/oauth/authorize",
                "token_endpoint": "https://music.example.com/oauth/token",
                "registration_endpoint": "https://music.example.com/oauth/register",
                "revocation_endpoint": "https://music.example.com/oauth/revoke",
                "scopes_supported": ["soulful", "offline_access"],
                "response_types_supported": ["code"],
                "response_modes_supported": ["query"],
                "grant_types_supported": ["authorization_code", "refresh_token"],
                "token_endpoint_auth_methods_supported": ["none"],
                "revocation_endpoint_auth_methods_supported": ["none"],
                "code_challenge_methods_supported": ["S256"],
                "authorization_response_iss_parameter_supported": true,
                "client_id_metadata_document_supported": true
            })
        );
    }

    #[tokio::test]
    async fn each_address_gets_its_own_documents() {
        let (_, _, a) = get_json(
            app().await,
            "/.well-known/oauth-protected-resource/mcp",
            &[("host", HOST)],
        )
        .await;
        let (_, _, b) = get_json(
            app().await,
            "/.well-known/oauth-protected-resource/mcp",
            &[
                ("host", "soulful:9765"),
                ("x-forwarded-host", "tunnel.example.net"),
                ("x-forwarded-proto", "https"),
            ],
        )
        .await;
        assert_eq!(
            (a["resource"].as_str(), b["resource"].as_str()),
            (
                Some("https://music.example.com/mcp"),
                Some("https://tunnel.example.net/mcp")
            )
        );
    }

    #[tokio::test]
    async fn there_is_no_openid_configuration() {
        let (status, _, body) = get_json(
            app().await,
            "/.well-known/openid-configuration",
            &[("host", HOST)],
        )
        .await;
        assert_eq!(
            (status, body["error"].as_str()),
            (StatusCode::NOT_FOUND, Some("not_found"))
        );
    }
}
