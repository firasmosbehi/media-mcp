use schemars::JsonSchema;
use serde::Deserialize;

use super::{AssetInfo, Output};
use crate::{
    assets::AssetRef,
    engine::{
        ffmpeg::{self, Export, MAX_DIM, Spec, VideoFormat, VideoOp},
        probe::{self, MediaKind},
    },
    error::{Error, Result},
    jobs::Progress,
    server::MediaServer,
};

#[derive(Debug, Deserialize, JsonSchema)]
pub struct VideoEditInput {
    #[serde(flatten)]
    pub source: AssetRef,
    /// Operations applied in order, each to the result of the previous one. May be empty to
    /// just re-encode or convert the container. At most 32.
    #[serde(default)]
    pub operations: Vec<VideoOp>,
    #[serde(default)]
    pub export: VideoExportSettings,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct VideoExportSettings {
    /// `mp4` (H.264 + AAC, default) or `webm` (VP9 + Opus).
    pub format: Option<VideoFormat>,
    /// Quality as constant rate factor, 0-51, lower is better and bigger. Default 23 (mp4) or 31 (webm).
    pub crf: Option<u8>,
    /// Copy the streams instead of re-encoding: instant and lossless. Only allowed when no
    /// re-encoding is needed: for video_edit no operations or a single `trim` (the cut snaps
    /// to the nearest earlier keyframe); for video_compose clips that are all videos with the
    /// same codecs, size and fps. Default false (re-encode).
    #[serde(default)]
    pub stream_copy: bool,
    /// Remove metadata (title, GPS, ...) from the output; default true.
    #[serde(default = "yes")]
    pub strip_metadata: bool,
    /// Output file name without extension (letters, digits, `.`, `_`, `-`). Defaults to the new asset's ID.
    pub output_name: Option<String>,
    /// Replace an existing file with the same output_name; default false.
    #[serde(default)]
    pub overwrite: bool,
}

fn yes() -> bool {
    true
}

impl Default for VideoExportSettings {
    // Must agree with the serde field defaults when `export` is omitted entirely.
    fn default() -> Self {
        Self {
            format: None,
            crf: None,
            stream_copy: false,
            strip_metadata: true,
            output_name: None,
            overwrite: false,
        }
    }
}

impl VideoExportSettings {
    /// Apply defaults and validate the encoder settings.
    pub fn resolve(&self) -> Result<Export> {
        let format = self.format.unwrap_or(VideoFormat::Mp4);
        let crf = self.crf.unwrap_or(match format {
            VideoFormat::Mp4 => 23,
            VideoFormat::Webm => 31,
        });
        if crf > 51 {
            return Err(Error::InvalidInput("crf must be 0-51".into()));
        }
        Ok(Export {
            format,
            crf,
            strip_metadata: self.strip_metadata,
            stream_copy: self.stream_copy,
        })
    }
}

pub async fn run(
    server: &MediaServer,
    input: VideoEditInput,
    progress: Progress,
) -> Result<AssetInfo> {
    let (_, src) = input.source.resolve(&server.assets).await?;
    let src_info = probe::probe(&server.engines, &src).await?;
    if src_info.kind != MediaKind::Video {
        return Err(Error::InvalidInput(format!(
            "video_edit only handles videos, this file is {:?}",
            src_info.kind
        )));
    }
    let export = input.export.resolve()?;
    let mut overlays = vec![];
    for op in &input.operations {
        if let VideoOp::Overlay { source, .. } = op {
            overlays.push(source.resolve(&server.assets).await?.1);
        }
    }
    let out = Output::prepare(
        server,
        input.export.output_name.as_deref(),
        input.export.overwrite,
        export.format.ext(),
    )
    .await?;
    let spec = Spec {
        input: &src,
        input_secs: src_info.duration_secs.unwrap_or(0.0),
        has_audio: src_info.has_audio,
        fps: src_info.fps,
        overlays,
        ops: &input.operations,
        export,
        output: &out.tmp,
    };
    ffmpeg::edit(&server.engines, server.assets.sandbox(), spec, &progress).await?;
    let info = probe::probe(&server.engines, &out.tmp).await?;
    if info.width.unwrap_or(0) > MAX_DIM || info.height.unwrap_or(0) > MAX_DIM {
        return Err(Error::InvalidInput(format!(
            "result is {}x{}, larger than the {MAX_DIM}px limit",
            info.width.unwrap_or(0),
            info.height.unwrap_or(0)
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
    fn defaults_when_export_omitted() {
        let i: VideoEditInput = serde_json::from_value(serde_json::json!({
            "asset_id": "x",
            "operations": [{"op": "speed", "factor": 2}]
        }))
        .unwrap();
        assert!(i.export.strip_metadata && !i.export.overwrite && !i.export.stream_copy);
        assert_eq!(i.operations.len(), 1);
    }
}
