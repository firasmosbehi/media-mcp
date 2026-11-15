use std::path::PathBuf;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::AssetRef;
use crate::{
    engine::probe::{self, MediaInfo},
    error::Result,
    server::MediaServer,
};

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ProbeInput {
    #[serde(flatten)]
    pub source: AssetRef,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct ProbeOutput {
    /// Asset ID to pass to other tools.
    pub asset_id: String,
    /// Absolute path of the file in the workspace.
    pub path: PathBuf,
    #[serde(flatten)]
    pub info: MediaInfo,
}

pub async fn run(server: &MediaServer, input: ProbeInput) -> Result<ProbeOutput> {
    let (asset_id, path) = input.source.resolve(&server.assets).await?;
    let info = probe::probe(&server.engines, &path).await?;
    Ok(ProbeOutput {
        asset_id,
        path,
        info,
    })
}
