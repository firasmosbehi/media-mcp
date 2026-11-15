use rmcp::tool_router;

/// MCP server state. Shared state (jobs, assets, config) will live here behind `Arc`.
#[derive(Clone, Default)]
pub struct MediaServer;

#[tool_router(server_handler, allow_empty)]
impl MediaServer {}
