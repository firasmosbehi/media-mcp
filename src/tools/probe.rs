use schemars::JsonSchema;
use serde::Deserialize;

use super::AssetInfo;
use crate::{assets::AssetRef, engine::probe, error::Result, server::MediaServer};

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ProbeInput {
    #[serde(flatten)]
    pub source: AssetRef,
}

pub async fn run(server: &MediaServer, input: ProbeInput) -> Result<AssetInfo> {
    let (asset_id, path) = input.source.resolve(&server.assets).await?;
    let info = probe::probe(&server.engines, &path).await?;
    Ok(AssetInfo {
        asset_id,
        path,
        info,
    })
}
