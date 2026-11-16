use schemars::JsonSchema;
use serde::Deserialize;

use super::{AssetInfo, Output, video_edit::VideoExportSettings};
use crate::{
    engine::{
        compose::{self, Clip},
        ffmpeg::{self, MAX_DIM},
        generate::Fill,
        probe,
        vips::{self, Export, ImageFormat},
    },
    error::{Error, Result},
    jobs::Progress,
    sandbox::Scratch,
    server::MediaServer,
};

#[derive(Debug, Deserialize, JsonSchema)]
pub struct GenerateVideoInput {
    /// Video width in pixels, 1-4096 (rounded down to even).
    pub width: u32,
    /// Video height in pixels, 1-4096 (rounded down to even).
    pub height: u32,
    /// Length in seconds, 0.1-3600.
    pub secs: f64,
    /// Frames per second, 1-120. Default 30.
    pub fps: Option<u32>,
    /// What to show: a `solid` colour or a `gradient`. Shown unchanged for the whole video.
    pub fill: Fill,
    #[serde(default)]
    pub export: VideoExportSettings,
}

pub async fn run(
    server: &MediaServer,
    input: GenerateVideoInput,
    progress: Progress,
) -> Result<AssetInfo> {
    let export = input.export.resolve()?;
    if export.stream_copy {
        return Err(Error::InvalidInput(
            "stream_copy does not apply to a generated video".into(),
        ));
    }
    if input.width > MAX_DIM || input.height > MAX_DIM {
        return Err(Error::InvalidInput(format!(
            "width and height must be at most {MAX_DIM}"
        )));
    }
    let sandbox = server.assets.sandbox();
    // Paint one frame with libvips, then loop it as an image clip of the composer.
    let work = sandbox.make_temp_dir().await?;
    let _cleanup = Scratch::new(&work);
    let frame = work.join("frame.png");
    vips::render_fill(
        &server.engines,
        sandbox,
        &input.fill,
        (input.width, input.height),
        &Export {
            format: ImageFormat::Png,
            quality: 90,
            strip_metadata: true,
        },
        &frame,
    )
    .await?;
    let clips = [Clip {
        info: probe::probe(&server.engines, &frame).await?,
        path: frame,
        secs: input.secs,
        explicit_secs: false,
    }];
    let out = Output::prepare(
        server,
        input.export.output_name.as_deref(),
        input.export.overwrite,
        export.format.ext(),
    )
    .await?;
    let compiled = compose::compile(&compose::Spec {
        clips: &clips,
        width: input.width,
        height: input.height,
        fps: input.fps.unwrap_or(30),
        export,
        output: &out.tmp,
    })?;
    ffmpeg::execute(&server.engines, sandbox, compiled, &progress).await?;
    let info = probe::probe(&server.engines, &out.tmp).await?;
    let (asset_id, path) = out.commit(server).await?;
    Ok(AssetInfo {
        asset_id,
        path,
        info,
    })
}
