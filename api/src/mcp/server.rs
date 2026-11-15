//! The MCP server behind `/mcp`: what it says about itself, its tools and
//! how it answers a tool call.

use std::sync::Arc;
use std::time::Instant;

use rmcp::model::{
    CallToolRequestParams, CallToolResponse, Implementation, JsonObject, ListToolsResult,
    PaginatedRequestParams, ServerCapabilities, ServerConfig, Tool, ToolAnnotations,
};
use rmcp::service::RequestContext;
use rmcp::transport::streamable_http_server::session::never::NeverSessionManager;
use rmcp::transport::{StreamableHttpServerConfig, StreamableHttpService};
use rmcp::{ErrorData, RoleServer, ServerHandler};

use super::contract::{failed, rate_limited};
use super::{status, McpCaller};
use crate::connection::ConnectionState;

/// What the server tells a client at `initialize`.
pub(crate) const INSTRUCTIONS: &str = "Soulful finds music, downloads it, imports it into the user's library and manages their Navidrome playlists. Call get_status first: it says what is connected and working. Every tool answers one JSON document; a failure has a stable error.code, error.stage and error.next with the calls that can still be tried.";

/// A tool as `tools/list` declares it.
pub(crate) struct ToolSpec {
    pub name: &'static str,
    pub title: &'static str,
    pub description: &'static str,
    /// The JSON Schema of the arguments, as JSON text.
    pub input_schema: &'static str,
    pub read_only: bool,
    pub destructive: bool,
    pub idempotent: bool,
    pub open_world: bool,
}

/// Every tool, in the fixed order `tools/list` answers (MCP 2026-07-28
/// `server/tools.mdx`, "deterministic order").
pub(crate) const TOOLS: &[ToolSpec] = &[ToolSpec {
    name: "get_status",
    title: "Soulful status",
    description: "Shows what Soulful can do for this user right now: whether the download backend (slskd) and the importer (beets) work and which importer issues stand, the user's Navidrome link (connected, admin or not, scan status, whether this account can start a scan), the user's folders with free space and which one is the default, the batches downloading now, whether this app may delete music, and the address Soulful is reached at. Takes no arguments. Failure codes: invalid_input (an argument was given), rate_limited (more than 120 tool calls in a minute; retry_after_seconds says when), settings_unreadable (Soulful's database could not be read). Answers in well under a second; a slow slskd or Navidrome can take up to 30 seconds.",
    // The schema for a tool without arguments (`server/tools.mdx`, "Tool").
    input_schema: r#"{"type": "object", "additionalProperties": false}"#,
    read_only: true,
    destructive: false,
    idempotent: true,
    open_world: false,
}];

impl ToolSpec {
    /// The rmcp tool, every hint set (design 10.3).
    fn tool(&self) -> Tool {
        let schema: JsonObject = serde_json::from_str(self.input_schema)
            .expect("every tool's input schema is a JSON object");
        Tool::new(self.name, self.description, schema)
            .with_title(self.title)
            .with_annotations(
                ToolAnnotations::new()
                    .read_only(self.read_only)
                    .destructive(self.destructive)
                    .idempotent(self.idempotent)
                    .open_world(self.open_world),
            )
    }
}

/// Soulful's MCP server.
#[derive(Clone)]
pub(crate) struct SoulfulMcp {
    pub(crate) state: Arc<ConnectionState>,
}

impl ServerHandler for SoulfulMcp {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("soulful", env!("CARGO_PKG_VERSION")))
            .with_instructions(INSTRUCTIONS)
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        Ok(ListToolsResult::with_all_items(
            TOOLS.iter().map(ToolSpec::tool).collect(),
        ))
    }

    fn get_tool(&self, name: &str) -> Option<Tool> {
        TOOLS
            .iter()
            .find(|spec| spec.name == name)
            .map(ToolSpec::tool)
    }

    /// A name that is no tool is a protocol error (MCP 2026-07-28
    /// `server/tools.mdx`, "Error Handling"); a call over the app's budget
    /// is a tool error.
    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        let Some(spec) = TOOLS.iter().find(|spec| spec.name == request.name) else {
            return Err(ErrorData::invalid_params(
                format!("Unknown tool: {}", request.name),
                None,
            ));
        };
        let caller = context
            .extensions
            .get::<axum::http::request::Parts>()
            .and_then(|parts| parts.extensions.get::<McpCaller>())
            .ok_or_else(|| ErrorData::internal_error("the bearer check did not run", None))?;
        let limit = &self.state.limits.tool_calls;
        if let Err(wait) = limit.admit(&caller.grant_id, Instant::now()) {
            return Ok(failed(rate_limited(limit.limit(), wait)).into());
        }
        let result = match spec.name {
            "get_status" => status::call(&self.state, caller, request.arguments).await,
            _ => unreachable!("every tool in TOOLS is dispatched"),
        };
        Ok(result.into())
    }
}

/// The Streamable HTTP service: no sessions, so every request is answered on
/// its own, and no `Host` allowlist, since rmcp's default admits loopback
/// names only and the connection guard has already read the address.
pub(crate) fn service(
    state: Arc<ConnectionState>,
) -> StreamableHttpService<SoulfulMcp, NeverSessionManager> {
    StreamableHttpService::new(
        move || {
            Ok(SoulfulMcp {
                state: state.clone(),
            })
        },
        Arc::new(NeverSessionManager::default()),
        StreamableHttpServerConfig::default()
            .with_legacy_session_mode(false)
            .disable_allowed_hosts(),
    )
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use axum::Router;
    use serde_json::{from_str, json, Value};
    use shared::failure::Stage;

    use super::*;
    use crate::connection::test_util::*;
    use crate::connection::{router, ConnectionState};
    use crate::core::database_failure;
    use crate::db::test_db;
    use crate::mcp::status::status_fields;
    use crate::oauth::limits::{Limits, WindowLimiter};

    async fn call(app: Router, token: &str, name: &str, arguments: Value) -> Value {
        let bearer = format!("Bearer {token}");
        let (_, _, body) = post_rpc(
            app,
            &[
                ("host", HOST),
                ("authorization", &bearer),
                ("mcp-protocol-version", "2025-11-25"),
            ],
            json!({"jsonrpc": "2.0", "id": 3, "method": "tools/call", "params": {"name": name, "arguments": arguments}}),
        )
        .await;
        body["result"].clone()
    }

    fn document(result: &Value) -> Value {
        from_str(result["content"][0]["text"].as_str().unwrap()).unwrap()
    }

    #[tokio::test]
    async fn tools_list_declares_get_status_with_every_hint() {
        let (_, app, token) = mcp_app().await;
        let bearer = format!("Bearer {token}");
        let (_, _, body) = post_rpc(
            app,
            &[
                ("host", HOST),
                ("authorization", &bearer),
                ("mcp-protocol-version", "2025-11-25"),
            ],
            json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}),
        )
        .await;
        let tools = body["result"]["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 1);
        let t = &tools[0];
        assert_eq!(
            (
                t["name"].as_str(),
                t["title"].as_str(),
                t["description"].as_str()
            ),
            (
                Some("get_status"),
                Some("Soulful status"),
                Some(TOOLS[0].description)
            )
        );
        assert_eq!(
            t["annotations"],
            json!({"readOnlyHint": true, "destructiveHint": false, "idempotentHint": true, "openWorldHint": false})
        );
        assert_eq!(
            t["inputSchema"],
            json!({"type": "object", "additionalProperties": false})
        );
    }

    #[tokio::test]
    async fn get_status_answers_one_json_document() {
        let (_, app, token) = mcp_app().await;
        let result = call(app, &token, "get_status", json!({})).await;
        assert_eq!(
            (
                result["isError"].as_bool(),
                result["content"].as_array().unwrap().len()
            ),
            (Some(false), 1)
        );
        assert!(result.get("structuredContent").is_none());
        let mut want = status_fields(&sample_facts(), &alice_caller(false));
        want.insert("ok".into(), json!(true));
        assert_eq!(document(&result), Value::Object(want));
    }

    #[tokio::test]
    async fn every_tool_answers_an_unknown_argument_with_invalid_input() {
        let (_, app, token) = mcp_app().await;
        for spec in TOOLS {
            let result = call(
                app.clone(),
                &token,
                spec.name,
                json!({"verbose": true, "zzz": 1}),
            )
            .await;
            assert_eq!(result["isError"], true, "{}", spec.name);
            let error = &document(&result)["error"];
            assert_eq!(
                (
                    error["code"].as_str(),
                    error["stage"].as_str(),
                    error["facts"]["field"].as_str()
                ),
                (Some("invalid_input"), Some("input"), Some("verbose")),
                "{}",
                spec.name
            );
            assert!(
                !error["next"].as_array().unwrap().is_empty(),
                "{}",
                spec.name
            );
        }
        let error =
            &document(&call(app, &token, "get_status", json!({"verbose": true})).await)["error"];
        assert_eq!(
            error["message"],
            "get_status takes no argument named 'verbose': it takes no arguments"
        );
        assert_eq!(error["next"], json!([{"tool": "get_status", "args": {}}]));
    }

    #[tokio::test]
    async fn a_call_over_the_limit_is_rate_limited() {
        let db = test_db::migrated().await;
        add_user(&db, "u1", "alice").await;
        let token = access_token(&db, "u1", RESOURCE).await;
        let limits = Limits {
            tool_calls: WindowLimiter::new(1, Duration::from_secs(60)),
            ..Limits::default()
        };
        let app = router(ConnectionState {
            limits: Arc::new(limits),
            ..ConnectionState::for_test(db)
        });
        assert_eq!(
            call(app.clone(), &token, "get_status", json!({})).await["isError"],
            false
        );
        let result = call(app, &token, "get_status", json!({})).await;
        assert_eq!(result["isError"], true);
        let error = &document(&result)["error"];
        let wait = error["retry_after_seconds"].as_u64().unwrap();
        assert!((59..=60).contains(&wait), "{wait}");
        assert_eq!(
            (error["code"].as_str(), error["retry"].as_str()),
            (Some("rate_limited"), Some("same_source_later"))
        );
        assert_eq!(
            error["message"],
            format!("Too many tool calls from this app: at most 1 a minute. Try again in {wait} seconds.")
        );
    }

    #[tokio::test]
    async fn a_status_that_cannot_be_read_is_a_tool_error() {
        let db = test_db::migrated().await;
        add_user(&db, "u1", "alice").await;
        let token = access_token(&db, "u1", RESOURCE).await;
        // sqlx's text for SQLITE_BUSY: sqlx-core 0.8.6 src/error.rs:44, sqlx-sqlite 0.8.6 src/error.rs:89
        let failure = database_failure(
            Stage::Input,
            "error returned from database: (code: 5) database is locked",
        );
        let app = router(ConnectionState {
            status: Arc::new(FixedStatus(Err(failure.clone()))),
            ..ConnectionState::for_test(db)
        });
        let result = call(app, &token, "get_status", json!({})).await;
        assert_eq!(result["isError"], true);
        let error = &document(&result)["error"];
        assert_eq!(
            (error["code"].as_str(), error["message"].as_str()),
            (
                Some("settings_unreadable"),
                Some("Soulful's database is unavailable")
            )
        );
        assert_eq!(
            error["cause"]["raw"],
            "error returned from database: (code: 5) database is locked"
        );
        assert_eq!((&error["facts"], &error["next"]), (&json!({}), &json!([])));
    }
}
