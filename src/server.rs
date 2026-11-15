use std::sync::Arc;

use rmcp::{
    ServerHandler, handler::server::wrapper::Parameters, model::CallToolResult, tool, tool_handler,
    tool_router,
};

use crate::{
    assets::Assets,
    engine::Engines,
    tools::{
        self, error_result, image_edit::ImageEditInput, preview::PreviewInput, probe::ProbeInput,
        to_result,
    },
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

    #[tool(
        description = "Edit a photo (PNG, JPEG, WebP, ...) with an ordered list of operations (crop, resize, rotate, flip, color, blur, text, overlay) and save the result as a new asset; the original is never modified. Use this for every photo change; use video_edit for video. Pass asset_id or import_path for the source. Operations run in order, so crop before resize, etc.; overlay takes another image as asset_id or import_path. `export` picks format (jpeg, png, webp), quality, metadata stripping and an optional output_name. Returns the new asset_id, path, format, dimensions and size. Check the result with preview."
    )]
    async fn image_edit(&self, Parameters(input): Parameters<ImageEditInput>) -> CallToolResult {
        to_result(tools::image_edit::run(self, input).await)
    }

    #[tool(
        description = "Look at an image: returns one downscaled JPEG (at most 1024 px on the long edge) as image content, plus the asset's info. Use it to verify the result of image_edit or to see what an imported photo contains. Images only for now. Never modifies the asset."
    )]
    async fn preview(&self, Parameters(input): Parameters<PreviewInput>) -> CallToolResult {
        match tools::preview::run(self, input).await {
            Ok(r) => r,
            Err(e) => error_result(e),
        }
    }
}

#[tool_handler(
    name = "media-mcp",
    instructions = "Edit and generate photos and videos. Import files first, then pass asset IDs to tools."
)]
impl ServerHandler for MediaServer {}
