//! The MCP server behind `/mcp`: what it says about itself and how it answers
//! a tool call.

use std::sync::Arc;

use rmcp::model::{
    CallToolRequestParams, CallToolResponse, Implementation, ServerCapabilities, ServerConfig,
};
use rmcp::service::RequestContext;
use rmcp::transport::streamable_http_server::session::never::NeverSessionManager;
use rmcp::transport::{StreamableHttpServerConfig, StreamableHttpService};
use rmcp::{ErrorData, RoleServer, ServerHandler};

/// What the server tells a client at `initialize`.
pub(crate) const INSTRUCTIONS: &str = "Soulful finds music, downloads it, imports it into the user's library and manages their Navidrome playlists. Call get_status first: it says what is connected and working. Every tool answers one JSON document; a failure has a stable error.code, error.stage and error.next with the calls that can still be tried.";

/// Soulful's MCP server. It has no tools yet, so `tools/list` answers rmcp's
/// default empty list and `get_tool` its default `None`.
#[derive(Clone)]
pub(crate) struct SoulfulMcp;

impl ServerHandler for SoulfulMcp {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("soulful", env!("CARGO_PKG_VERSION")))
            .with_instructions(INSTRUCTIONS)
    }

    /// A name that is no tool is a protocol error (MCP 2026-07-28
    /// `server/tools.mdx`, "Error Handling").
    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        Err(ErrorData::invalid_params(
            format!("Unknown tool: {}", request.name),
            None,
        ))
    }
}

/// The Streamable HTTP service: no sessions, so every request is answered on
/// its own, and no `Host` allowlist, since rmcp's default admits loopback
/// names only and the connection guard has already read the address.
pub(crate) fn service() -> StreamableHttpService<SoulfulMcp, NeverSessionManager> {
    StreamableHttpService::new(
        || Ok(SoulfulMcp),
        Arc::new(NeverSessionManager::default()),
        StreamableHttpServerConfig::default()
            .with_legacy_session_mode(false)
            .disable_allowed_hosts(),
    )
}
