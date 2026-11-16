use schemars::JsonSchema;
use serde::Deserialize;

use super::{AssetInfo, Output};
use crate::{
    assets::AssetRef,
    engine::{
        ffmpeg::{self, MAX_DIM},
        probe::{self, MediaKind},
        raw::{self, OutputFormat, Spec},
        vips,
    },
    error::{Error, Result},
    jobs::Progress,
    server::MediaServer,
};

#[derive(Debug, Deserialize, JsonSchema)]
pub struct RunFfmpegInput {
    /// Input files, referred to in `args` as `{in0}`, `{in1}`, ... (1 to 8).
    pub inputs: Vec<AssetRef>,
    /// ffmpeg arguments, one array element each, e.g.
    /// `["-i","{in0}","-vf","hue=s=0","-c:a","copy","{out}"]`. Every input must appear as
    /// `-i {inN}`, and the last element must be exactly `{out}`. Do not pass `-y`, `-protocol_whitelist`
    /// or `-loglevel`: the server sets its own. Network URLs, file-reading filters (movie,
    /// subtitles, textfile, ...) and generated `lavfi` sources are rejected.
    pub args: Vec<String>,
    /// File type of `{out}`; ffmpeg chooses the container from it.
    pub output_format: OutputFormat,
    /// Output file name without extension (letters, digits, `.`, `_`, `-`). Defaults to the new asset's ID.
    pub output_name: Option<String>,
    /// Replace an existing file with the same output_name; default false.
    #[serde(default)]
    pub overwrite: bool,
}

pub async fn run(
    server: &MediaServer,
    input: RunFfmpegInput,
    progress: Progress,
) -> Result<AssetInfo> {
    let mut paths = vec![];
    let mut expected = 0.0;
    for (i, r) in input.inputs.iter().enumerate() {
        let (_, path) = r.resolve(&server.assets).await?;
        let info = probe::probe(&server.engines, &path).await?;
        if i == 0 {
            expected = info.duration_secs.unwrap_or(0.0);
        }
        paths.push(path);
    }
    let out = Output::prepare(
        server,
        input.output_name.as_deref(),
        input.overwrite,
        input.output_format.ext(),
    )
    .await?;
    let compiled = raw::compile(&Spec {
        inputs: &paths,
        args: &input.args,
        expected_secs: expected,
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
    let limit = if info.kind == MediaKind::Image {
        vips::MAX_DIM
    } else {
        MAX_DIM
    };
    if info.width.unwrap_or(0) > limit || info.height.unwrap_or(0) > limit {
        return Err(Error::InvalidInput(format!(
            "result is {}x{}, larger than the {limit}px limit",
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
