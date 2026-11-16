use schemars::JsonSchema;
use serde::Deserialize;

use super::AssetInfo;
use crate::{
    assets::AssetRef,
    engine::{
        probe::{self, MediaKind},
        vips::{self, EditJob, Export, ImageFormat, ImageOp, MAX_DIM},
    },
    error::{Error, Result},
    jobs::Progress,
    sandbox::Scratch,
    server::MediaServer,
};

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ImageEditInput {
    #[serde(flatten)]
    pub source: AssetRef,
    /// Operations applied in order, each to the result of the previous one. May be empty to
    /// just convert the format. At most 32.
    #[serde(default)]
    pub operations: Vec<ImageOp>,
    #[serde(default)]
    pub export: ExportSettings,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ExportSettings {
    /// Output format. Defaults to the source's format (PNG if the source is not JPEG, PNG or WebP).
    pub format: Option<ImageFormat>,
    /// 1-100 for JPEG and WebP, default 90. Ignored for PNG.
    pub quality: Option<u8>,
    /// Remove EXIF/GPS/ICC metadata from the output; default true.
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

impl Default for ExportSettings {
    // Must agree with the serde field defaults when `export` is omitted entirely.
    fn default() -> Self {
        Self {
            format: None,
            quality: None,
            strip_metadata: true,
            output_name: None,
            overwrite: false,
        }
    }
}

pub(crate) fn valid_stem(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 100
        && !s.starts_with('.')
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

pub async fn run(
    server: &MediaServer,
    input: ImageEditInput,
    progress: Progress,
) -> Result<AssetInfo> {
    let (_, src) = input.source.resolve(&server.assets).await?;
    let src_info = probe::probe(&server.engines, &src).await?;
    if src_info.kind != MediaKind::Image {
        return Err(Error::InvalidInput(format!(
            "image_edit only handles images, this file is {:?}",
            src_info.kind
        )));
    }
    let quality = input.export.quality.unwrap_or(90);
    if !(1..=100).contains(&quality) {
        return Err(Error::InvalidInput("quality must be 1-100".into()));
    }
    let format = input
        .export
        .format
        .unwrap_or_else(|| ImageFormat::from_path(&src));

    let mut overlays = vec![];
    for op in &input.operations {
        if let ImageOp::Overlay { source, .. } = op {
            overlays.push(source.resolve(&server.assets).await?.1);
        }
    }

    let sandbox = server.assets.sandbox();
    let stem = match &input.export.output_name {
        Some(n) if valid_stem(n) => n.clone(),
        Some(n) => return Err(Error::InvalidInput(format!("invalid output_name: {n}"))),
        None => uuid::Uuid::new_v4().to_string(),
    };
    let dest = sandbox
        .new_path(&format!("{stem}.{}", format.ext()))
        .await?;
    if !input.export.overwrite && tokio::fs::try_exists(&dest).await.unwrap_or(false) {
        return Err(Error::InvalidInput(format!(
            "{stem}.{} already exists; pass overwrite: true to replace it",
            format.ext()
        )));
    }
    // Written under a temp name and renamed on success: a failure never leaves a partial asset.
    let tmp = sandbox
        .new_path(&format!(".{}.tmp.{}", uuid::Uuid::new_v4(), format.ext()))
        .await?;
    let job = EditJob {
        input: &src,
        overlays,
        ops: &input.operations,
        export: Export {
            format,
            quality,
            strip_metadata: input.export.strip_metadata,
        },
        output: &tmp,
        progress,
    };
    let scratch = Scratch::new(&tmp); // removed on failure or cancel
    let finish = async {
        vips::edit(&server.engines, sandbox, job).await?;
        let info = probe::probe(&server.engines, &tmp).await?;
        if info.width.unwrap_or(0) > MAX_DIM || info.height.unwrap_or(0) > MAX_DIM {
            return Err(Error::InvalidInput(format!(
                "result would be {}x{}, larger than the {MAX_DIM}px limit",
                info.width.unwrap_or(0),
                info.height.unwrap_or(0)
            )));
        }
        tokio::fs::rename(&tmp, &dest)
            .await
            .map_err(|e| Error::io(&dest, e))?;
        Ok(info)
    };
    let info = finish.await?;
    scratch.disarm();
    let asset_id = server.assets.register(&dest).await?;
    Ok(AssetInfo {
        asset_id,
        path: dest,
        info,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stems() {
        assert!(valid_stem("my-photo_1.edit"));
        for bad in ["", ".hidden", "a/b", "a b", "../x", "a[1]"] {
            assert!(!valid_stem(bad), "{bad}");
        }
    }

    #[test]
    fn input_schema_and_defaults_deserialize() {
        let i: ImageEditInput = serde_json::from_value(serde_json::json!({
            "asset_id": "x",
            "operations": [
                {"op": "crop", "x": 0, "y": 0, "width": 10, "height": 10},
                {"op": "overlay", "asset_id": "y", "x": 1, "y": 2},
                {"op": "resize", "width": 5}
            ]
        }))
        .unwrap();
        assert_eq!(i.operations.len(), 3);
        assert!(i.export.strip_metadata && !i.export.overwrite);
        assert!(
            serde_json::from_value::<ImageEditInput>(serde_json::json!({
                "asset_id": "x", "operations": [{"op": "nope"}]
            }))
            .is_err()
        );
    }
}
