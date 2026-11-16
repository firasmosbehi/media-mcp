use schemars::JsonSchema;
use serde::Deserialize;

use super::{AssetInfo, Output, image_edit::ExportSettings};
use crate::{
    assets::AssetRef,
    engine::{
        probe::{self, MediaKind},
        vips::{self, ComposeJob, Export, ImageFormat},
    },
    error::{Error, Result},
    server::MediaServer,
};

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ImageComposeInput {
    /// The images in reading order (left to right, top to bottom), 1 to 64.
    pub images: Vec<AssetRef>,
    /// Images per row. Default: enough for a roughly square grid.
    pub columns: Option<u32>,
    /// Width in pixels of every cell. Each image is scaled to fit inside its cell, keeping its
    /// aspect ratio, and centred. Default 400.
    pub cell_width: Option<u32>,
    /// Height in pixels of every cell. Default 400.
    pub cell_height: Option<u32>,
    /// Pixels between cells. Default 0.
    pub gap: Option<u32>,
    /// Background and transparency colour as `#RRGGBB`. Default white.
    pub background: Option<String>,
    #[serde(default)]
    pub export: ExportSettings,
}

pub async fn run(server: &MediaServer, input: ImageComposeInput) -> Result<AssetInfo> {
    let mut inputs = vec![];
    for (i, r) in input
        .images
        .iter()
        .enumerate()
        .take(vips::MAX_COMPOSE_IMAGES + 1)
    {
        let (_, path) = r.resolve(&server.assets).await?;
        let kind = probe::probe(&server.engines, &path).await?.kind;
        if kind != MediaKind::Image {
            return Err(Error::InvalidInput(format!(
                "image {i} is {kind:?}; image_compose only joins images"
            )));
        }
        inputs.push(path);
    }
    let quality = input.export.quality.unwrap_or(90);
    if !(1..=100).contains(&quality) {
        return Err(Error::InvalidInput("quality must be 1-100".into()));
    }
    let format = input.export.format.unwrap_or(ImageFormat::Png);
    let columns = input
        .columns
        .unwrap_or_else(|| (inputs.len() as f64).sqrt().ceil() as u32);
    let out = Output::prepare(
        server,
        input.export.output_name.as_deref(),
        input.export.overwrite,
        format.ext(),
    )
    .await?;
    vips::compose(
        &server.engines,
        server.assets.sandbox(),
        ComposeJob {
            inputs: &inputs,
            columns,
            cell: (
                input.cell_width.unwrap_or(400),
                input.cell_height.unwrap_or(400),
            ),
            gap: input.gap.unwrap_or(0),
            background: input.background.as_deref().unwrap_or("#ffffff"),
            export: Export {
                format,
                quality,
                strip_metadata: input.export.strip_metadata,
            },
            output: &out.tmp,
        },
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
