pub mod probe;

use std::path::PathBuf;

use rmcp::model::{CallToolResult, ContentBlock};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::{
    assets::Assets,
    error::{Error, Result},
};

/// Reference to a media file. Give exactly one field.
#[derive(Debug, Default, Deserialize, JsonSchema)]
pub struct AssetRef {
    /// ID of an asset already in the workspace (returned by an earlier tool call).
    pub asset_id: Option<String>,
    /// Absolute path of a file outside the workspace. It is copied into the workspace
    /// and gets a new asset ID, returned in the result.
    pub import_path: Option<String>,
}

impl AssetRef {
    pub async fn resolve(&self, assets: &Assets) -> Result<(String, PathBuf)> {
        match (&self.asset_id, &self.import_path) {
            (Some(id), None) => Ok((id.clone(), assets.get(id)?)),
            (None, Some(p)) => assets.import(p).await,
            _ => Err(Error::InvalidInput(
                "give exactly one of asset_id or import_path".into(),
            )),
        }
    }
}

/// Engine and sandbox failures become tool results with is_error, so the model can self-correct.
pub fn to_result<T: Serialize>(r: Result<T>) -> CallToolResult {
    match r.and_then(|v| {
        serde_json::to_value(v).map_err(|e| Error::InvalidInput(format!("serialize: {e}")))
    }) {
        Ok(v) => CallToolResult::structured(v),
        Err(e) => CallToolResult::error(vec![ContentBlock::text(e.to_string())]),
    }
}
