use std::sync::Arc;

use rmcp::{
    ServerHandler, handler::server::wrapper::Parameters, model::CallToolResult, tool, tool_handler,
    tool_router,
};

use crate::{
    assets::Assets,
    engine::Engines,
    tools::{self, probe::ProbeInput, to_result},
};

/// MCP server state, shared by all tool handlers.
#[derive(Clone)]
pub struct MediaServer {
    pub assets: Assets,
    pub engines: Arc<Engines>,
}

impl MediaServer {
    pub fn new(assets: Assets, engines: Engines) -> Self {
        Self {
            assets,
            engines: Arc::new(engines),
        }
    }
}

#[tool_router]
impl MediaServer {
    #[tool(
        description = "Inspect a media file (image, video or audio) with ffprobe and return its kind, format, dimensions, duration, fps, codecs, audio presence and size. Pass asset_id for a file already in the workspace, or import_path (absolute path) to copy a new file in; the result always includes the asset_id and workspace path. Use this first on any new file; it is fast and never modifies media."
    )]
    async fn probe(&self, Parameters(input): Parameters<ProbeInput>) -> CallToolResult {
        to_result(tools::probe::run(self, input).await)
    }
}

#[tool_handler(
    name = "media-mcp",
    instructions = "Edit and generate photos and videos. Import files first, then pass asset IDs to tools."
)]
impl ServerHandler for MediaServer {}
