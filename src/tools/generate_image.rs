use schemars::JsonSchema;
use serde::Deserialize;

use super::{AssetInfo, Output, image_edit::ExportSettings};
use crate::{
    engine::{
        generate::Fill,
        probe,
        vips::{self, Export, ImageFormat},
    },
    error::{Error, Result},
    server::MediaServer,
};

#[derive(Debug, Deserialize, JsonSchema)]
pub struct GenerateImageInput {
    /// Image width in pixels, 1-16384.
    pub width: u32,
    /// Image height in pixels, 1-16384.
    pub height: u32,
    /// What to paint: a `solid` colour or a `gradient`.
    pub fill: Fill,
    #[serde(default)]
    pub export: ExportSettings,
}

pub async fn run(server: &MediaServer, input: GenerateImageInput) -> Result<AssetInfo> {
    let quality = input.export.quality.unwrap_or(90);
    if !(1..=100).contains(&quality) {
        return Err(Error::InvalidInput("quality must be 1-100".into()));
    }
    let format = input.export.format.unwrap_or(ImageFormat::Png);
    // Validate before touching the workspace.
    vips::fill_steps(&input.fill, input.width, input.height)?;
    let out = Output::prepare(
        server,
        input.export.output_name.as_deref(),
        input.export.overwrite,
        format.ext(),
    )
    .await?;
    vips::render_fill(
        &server.engines,
        server.assets.sandbox(),
        &input.fill,
        (input.width, input.height),
        &Export {
            format,
            quality,
            strip_metadata: input.export.strip_metadata,
        },
        &out.tmp,
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
