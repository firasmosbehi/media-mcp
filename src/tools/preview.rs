use base64::{Engine as _, engine::general_purpose::STANDARD};
use rmcp::model::{CallToolResult, ContentBlock};
use schemars::JsonSchema;
use serde::Deserialize;

use super::AssetInfo;
use crate::{
    assets::AssetRef,
    engine::{
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
}

pub async fn run(server: &MediaServer, input: PreviewInput) -> Result<CallToolResult> {
    let (asset_id, src) = input.source.resolve(&server.assets).await?;
    let info = probe::probe(&server.engines, &src).await?;
    if info.kind != MediaKind::Image {
        return Err(Error::InvalidInput(
            "preview currently supports images only; video frames are not implemented yet".into(),
        ));
    }
    let sandbox = server.assets.sandbox();
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
        input: &src,
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
