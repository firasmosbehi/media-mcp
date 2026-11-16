use schemars::JsonSchema;
use serde::Deserialize;

use super::{AssetInfo, Output};
use crate::{
    assets::AssetRef,
    engine::{
        audio::{self, AudioFormat, AudioOp, Spec},
        ffmpeg,
        probe::{self, MediaKind},
    },
    error::{Error, Result},
    jobs::Progress,
    server::MediaServer,
};

#[derive(Debug, Deserialize, JsonSchema)]
pub struct AudioEditInput {
    #[serde(flatten)]
    pub source: AssetRef,
    /// Operations applied in order, each to the result of the previous one. May be empty to
    /// just extract the audio of a video or convert the format. At most 32.
    #[serde(default)]
    pub operations: Vec<AudioOp>,
    #[serde(default)]
    pub export: AudioExportSettings,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct AudioExportSettings {
    /// `mp3` (default), `wav` or `m4a`.
    pub format: Option<AudioFormat>,
    /// Remove metadata (title, artist, ...) from the output; default true.
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

impl Default for AudioExportSettings {
    // Must agree with the serde field defaults when `export` is omitted entirely.
    fn default() -> Self {
        Self {
            format: None,
            strip_metadata: true,
            output_name: None,
            overwrite: false,
        }
    }
}

pub async fn run(
    server: &MediaServer,
    input: AudioEditInput,
    progress: Progress,
) -> Result<AssetInfo> {
    let (_, src) = input.source.resolve(&server.assets).await?;
    let src_info = probe::probe(&server.engines, &src).await?;
    if src_info.kind == MediaKind::Image || !src_info.has_audio {
        return Err(Error::InvalidInput(
            "audio_edit needs an audio file or a video with an audio track; this file has no audio"
                .into(),
        ));
    }
    let format = input.export.format.unwrap_or(AudioFormat::Mp3);
    let out = Output::prepare(
        server,
        input.export.output_name.as_deref(),
        input.export.overwrite,
        format.ext(),
    )
    .await?;
    let compiled = audio::compile(&Spec {
        input: &src,
        input_secs: src_info.duration_secs.unwrap_or(0.0),
        ops: &input.operations,
        format,
        strip_metadata: input.export.strip_metadata,
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
        let i: AudioEditInput = serde_json::from_value(serde_json::json!({
            "asset_id": "x",
            "operations": [{"op": "volume", "factor": 2}]
        }))
        .unwrap();
        assert!(i.export.strip_metadata && !i.export.overwrite && i.export.format.is_none());
        assert_eq!(i.operations.len(), 1);
    }
}
