use std::sync::Arc;

use rmcp::{ServerHandler, tool_handler, tool_router};

use crate::{assets::Assets, engine::Engines};

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

#[tool_router(allow_empty)]
impl MediaServer {}

#[tool_handler(
    name = "media-mcp",
    instructions = "Edit and generate photos and videos. Import files first, then pass asset IDs to tools."
)]
impl ServerHandler for MediaServer {}
