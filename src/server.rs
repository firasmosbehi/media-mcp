use std::sync::Arc;

use rmcp::{
    RoleServer, ServerHandler, handler::server::wrapper::Parameters, model::CallToolResult,
    service::RequestContext, tool, tool_handler, tool_router,
};

use crate::{
    assets::Assets,
    engine::Engines,
    jobs::Jobs,
    tools::{
        self,
        audio_edit::AudioEditInput,
        error_result,
        image_edit::ImageEditInput,
        jobs::{JobCancelInput, JobStatusInput},
        preview::PreviewInput,
        probe::ProbeInput,
        to_result,
        video_compose::VideoComposeInput,
        video_edit::VideoEditInput,
    },
};

/// MCP server state, shared by all tool handlers.
#[derive(Clone)]
pub struct MediaServer {
    pub assets: Assets,
    pub engines: Arc<Engines>,
    pub jobs: Jobs,
}

impl MediaServer {
    pub fn new(assets: Assets, engines: Engines, jobs: Jobs) -> Self {
        Self {
            assets,
            engines: Arc::new(engines),
            jobs,
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
        description = "Edit a photo (PNG, JPEG, WebP, ...) with an ordered list of operations (crop, resize, rotate, flip, color, blur, text, overlay) and save the result as a new asset; the original is never modified. Use this for every photo change; use video_edit for video. Pass asset_id or import_path for the source. Operations run in order, so crop before resize, etc.; overlay takes another image as asset_id or import_path. `export` picks format (jpeg, png, webp), quality, metadata stripping and an optional output_name. Returns the new asset_id, path, format, dimensions and size. Check the result with preview. Runs as a job: this returns a job_id immediately; call job_status (with wait_secs) to get the result."
    )]
    async fn image_edit(&self, Parameters(input): Parameters<ImageEditInput>) -> CallToolResult {
        let server = self.clone();
        let started = self.jobs.submit(move |progress| async move {
            let info = tools::image_edit::run(&server, input, progress).await?;
            serde_json::to_value(info).map_err(|e| crate::error::Error::InvalidInput(e.to_string()))
        });
        to_result(Ok(started))
    }

    #[tool(
        description = "Edit a video with an ordered list of operations (trim, crop, resize, rotate, flip, speed, color, text, overlay) compiled into one ffmpeg render, and save the result as a new asset; the original is never modified. Use this for every video change; use image_edit for photos. Pass asset_id or import_path for the source. Operations run in order, so trim before speed refers to original time; overlay takes an image as asset_id or import_path; audio follows trim and speed automatically. `export` picks mp4 (H.264+AAC) or webm (VP9+Opus), crf quality, an optional output_name, and `stream_copy: true` for an instant lossless cut when the only operation is one trim (cuts then snap to keyframes). Runs as a job: this returns a job_id immediately; call job_status with wait_secs for progress and the result (new asset_id, path, dimensions, duration, size). Check the result with preview."
    )]
    async fn video_edit(&self, Parameters(input): Parameters<VideoEditInput>) -> CallToolResult {
        let server = self.clone();
        let started = self.jobs.submit(move |progress| async move {
            let info = tools::video_edit::run(&server, input, progress).await?;
            serde_json::to_value(info).map_err(|e| crate::error::Error::InvalidInput(e.to_string()))
        });
        to_result(Ok(started))
    }

    #[tool(
        description = "Edit audio, or extract the audio track of a video, with an ordered list of operations (trim, volume, fade_in, fade_out, speed) and save it as a new audio asset (mp3, wav or m4a); the source is never modified. Pass asset_id or import_path for an audio file or a video. Operations run in order, so fade_out after a trim fades the end of the trimmed clip; with no operations it just extracts or converts. Use video_edit to change a video's picture and sound together. Runs as a job: this returns a job_id immediately; call job_status with wait_secs for the result (new asset_id, path, duration, size)."
    )]
    async fn audio_edit(&self, Parameters(input): Parameters<AudioEditInput>) -> CallToolResult {
        let server = self.clone();
        let started = self.jobs.submit(move |progress| async move {
            let info = tools::audio_edit::run(&server, input, progress).await?;
            serde_json::to_value(info).map_err(|e| crate::error::Error::InvalidInput(e.to_string()))
        });
        to_result(Ok(started))
    }

    #[tool(
        description = "Join several videos and still images into one video, in order: a concatenation or a photo slideshow. Each clip is an asset_id or import_path; images show for image_secs (default 3) or their own secs, videos can be shortened with secs. Clips are scaled to fit the output width/height (default: the first clip's size, padded with black) and fps (default 30); sound is kept and silent clips are padded so audio stays in sync. `export` picks mp4 or webm, crf, an optional output_name, and `stream_copy: true` for an instant lossless join when every clip is a video with identical codecs, size and fps. Use video_edit to change a single video. Runs as a job: this returns a job_id immediately; call job_status with wait_secs for progress and the result (new asset_id, path, dimensions, duration, size). Check the result with preview."
    )]
    async fn video_compose(
        &self,
        Parameters(input): Parameters<VideoComposeInput>,
    ) -> CallToolResult {
        let server = self.clone();
        let started = self.jobs.submit(move |progress| async move {
            let info = tools::video_compose::run(&server, input, progress).await?;
            serde_json::to_value(info).map_err(|e| crate::error::Error::InvalidInput(e.to_string()))
        });
        to_result(Ok(started))
    }

    #[tool(
        description = "Get the state of a job started by another tool: queued, running, succeeded, failed or cancelled, with progress (0-1), and the result (e.g. the new asset) once succeeded or the error message once failed. Pass wait_secs (up to 30) to block until the job finishes instead of polling. Use job_cancel to stop a job."
    )]
    async fn job_status(
        &self,
        Parameters(input): Parameters<JobStatusInput>,
        ctx: RequestContext<RoleServer>,
    ) -> CallToolResult {
        let sink = ctx.meta.get_progress_token().map(|t| (ctx.peer.clone(), t));
        to_result(tools::jobs::status(self, input, sink).await)
    }

    #[tool(
        description = "Cancel a queued or running job: stops the underlying process and deletes any partial output. Cancelling a job that already finished changes nothing. Returns the job's final state."
    )]
    async fn job_cancel(&self, Parameters(input): Parameters<JobCancelInput>) -> CallToolResult {
        to_result(tools::jobs::cancel(self, input))
    }

    #[tool(
        description = "Look at an image or a video frame: returns one downscaled JPEG (at most 1024 px on the long edge) as image content, plus the asset's info. For a video pass at_secs to pick the moment (default: the middle). Use it to verify the result of image_edit or video_edit, or to see what an imported file contains. Never modifies the asset."
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
