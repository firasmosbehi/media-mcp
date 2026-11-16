use schemars::JsonSchema;
use serde::Deserialize;

use super::{AssetInfo, Output, video_edit::VideoExportSettings};
use crate::{
    assets::AssetRef,
    engine::{
        compose::{self, Clip, Spec},
        ffmpeg::{self, MAX_DIM},
        probe::{self, MediaKind},
    },
    error::{Error, Result},
    jobs::Progress,
    server::MediaServer,
};

#[derive(Debug, Deserialize, JsonSchema)]
pub struct VideoComposeInput {
    /// The clips in play order, 1 to 100. Each is a video or a still image.
    pub clips: Vec<ClipInput>,
    /// Seconds each image is shown unless the clip sets its own `secs`. Default 3.
    pub image_secs: Option<f64>,
    /// Output width in pixels. Default: the first clip's width. Clips with another aspect
    /// ratio are scaled to fit and padded with black bars.
    pub width: Option<u32>,
    /// Output height in pixels. Default: the first clip's height.
    pub height: Option<u32>,
    /// Output frames per second, 1-120. Default 30.
    pub fps: Option<u32>,
    #[serde(default)]
    pub export: VideoExportSettings,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ClipInput {
    #[serde(flatten)]
    pub source: AssetRef,
    /// How long this clip plays, in seconds: the display time of an image, or the length to
    /// keep from the start of a video (default: all of it).
    pub secs: Option<f64>,
}

pub async fn run(
    server: &MediaServer,
    input: VideoComposeInput,
    progress: Progress,
) -> Result<AssetInfo> {
    let export = input.export.resolve()?;
    let image_secs = input.image_secs.unwrap_or(3.0);
    let mut clips = vec![];
    for (i, c) in input.clips.iter().enumerate() {
        if i >= compose::MAX_CLIPS {
            break; // compile() reports the limit
        }
        let (_, path) = c.source.resolve(&server.assets).await?;
        let info = probe::probe(&server.engines, &path).await?;
        let secs = match info.kind {
            MediaKind::Image => c.secs.unwrap_or(image_secs),
            MediaKind::Video => {
                let len = info.duration_secs.unwrap_or(0.0);
                c.secs.map_or(len, |s| s.min(len))
            }
            MediaKind::Audio => {
                return Err(Error::InvalidInput(format!(
                    "clip {i} is audio; video_compose only joins videos and images"
                )));
            }
        };
        clips.push(Clip {
            path,
            info,
            secs,
            explicit_secs: c.secs.is_some(),
        });
    }
    if clips.is_empty() {
        return Err(Error::InvalidInput("give at least one clip".into()));
    }
    let first = &clips[0].info;
    let (width, height) = (
        input.width.or(first.width).unwrap_or(0),
        input.height.or(first.height).unwrap_or(0),
    );
    let out = Output::prepare(
        server,
        input.export.output_name.as_deref(),
        input.export.overwrite,
        export.format.ext(),
    )
    .await?;
    let compiled = compose::compile(&Spec {
        clips: &clips,
        width,
        height,
        fps: input.fps.unwrap_or(30),
        export,
        output: &out.tmp,
    })?;
    ffmpeg::execute(
        &server.engines,
        server.assets.sandbox(),
        compiled,
        &progress,
    )
    .await?;
    let info = probe::probe(&server.engines, &out.tmp).await?;
    if info.width.unwrap_or(0) > MAX_DIM || info.height.unwrap_or(0) > MAX_DIM {
        return Err(Error::InvalidInput(format!(
            "result is larger than the {MAX_DIM}px limit"
        )));
    }
    let (asset_id, path) = out.commit(server).await?;
    Ok(AssetInfo {
        asset_id,
        path,
        info,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clip_json_shape() {
        let i: VideoComposeInput = serde_json::from_value(serde_json::json!({
            "clips": [{"asset_id": "a", "secs": 2}, {"import_path": "/x.png"}]
        }))
        .unwrap();
        assert_eq!(i.clips.len(), 2);
        assert_eq!(i.clips[0].secs, Some(2.0));
        assert!(i.clips[1].secs.is_none() && i.export.strip_metadata);
    }
}
