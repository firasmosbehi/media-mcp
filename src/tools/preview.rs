use base64::{Engine as _, engine::general_purpose::STANDARD};
use rmcp::model::{CallToolResult, ContentBlock};
use schemars::JsonSchema;
use serde::Deserialize;

use super::AssetInfo;
use crate::{
    assets::AssetRef,
    engine::{
        ffmpeg,
        probe::{self, MediaKind},
        vips::{self, EditJob, Export, ImageFormat, ImageOp, ResizeMode},
    },
    error::{Error, Result},
    jobs::Progress,
    sandbox::Scratch,
    server::MediaServer,
};

/// Longest side of the preview image in pixels.
const PREVIEW_PX: u32 = 1024;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct PreviewInput {
    #[serde(flatten)]
    pub source: AssetRef,
    /// For videos: the moment to show, in seconds. Default: the middle of the clip. Ignored for images.
    pub at_secs: Option<f64>,
}

pub async fn run(server: &MediaServer, input: PreviewInput) -> Result<CallToolResult> {
    let (asset_id, src) = input.source.resolve(&server.assets).await?;
    let info = probe::probe(&server.engines, &src).await?;
    let sandbox = server.assets.sandbox();
    // Videos: pull one frame into a scratch PNG, then downscale it like any image.
    let frame = match info.kind {
        MediaKind::Image => None,
        MediaKind::Video => {
            let dur = info.duration_secs.unwrap_or(0.0);
            let at = input.at_secs.unwrap_or(dur / 2.0);
            if !(at.is_finite() && at >= 0.0) || (dur > 0.0 && at >= dur) {
                return Err(Error::InvalidInput(format!(
                    "at_secs must be between 0 and the duration ({dur:.2}s)"
                )));
            }
            let png = sandbox
                .new_path(&format!(".{}.frame.png", uuid::Uuid::new_v4()))
                .await?;
            let guard = Scratch::new(&png);
            ffmpeg::extract_frame(&server.engines, &src, at, &png).await?;
            Some((png, guard))
        }
        MediaKind::Audio => {
            return Err(Error::InvalidInput(
                "preview needs an image or a video; this file is audio".into(),
            ));
        }
    };
    let vips_src = frame.as_ref().map_or(&src, |(p, _)| p);
    let tmp = sandbox
        .new_path(&format!(".{}.preview.jpg", uuid::Uuid::new_v4()))
        .await?;
    let ops = [ImageOp::Resize {
        width: Some(PREVIEW_PX),
        height: Some(PREVIEW_PX),
        mode: ResizeMode::Fit,
        upscale: false,
    }];
    let job = EditJob {
        input: vips_src,
        overlays: vec![],
        ops: &ops,
        export: Export {
            format: ImageFormat::Jpeg,
            quality: 80,
            strip_metadata: true,
        },
        output: &tmp,
        progress: Progress::default(),
    };
    let _cleanup = Scratch::new(&tmp);
    let bytes = async {
        vips::edit(&server.engines, sandbox, job).await?;
        tokio::fs::read(&tmp).await.map_err(|e| Error::io(&tmp, e))
    }
    .await;
    let bytes = bytes?;
    let meta = serde_json::to_string(&AssetInfo {
        asset_id,
        path: src,
        info,
    })
    .map_err(|e| Error::InvalidInput(e.to_string()))?;
    Ok(CallToolResult::success(vec![
        ContentBlock::text(meta),
        ContentBlock::image(STANDARD.encode(bytes), "image/jpeg"),
    ]))
}
