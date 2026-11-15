pub mod image_edit;
pub mod preview;
pub mod probe;

use std::path::PathBuf;

use rmcp::model::{CallToolResult, ContentBlock};
use schemars::JsonSchema;
use serde::Serialize;

use crate::{
    engine::probe::MediaInfo,
    error::{Error, Result},
};

/// Standard tool result: the asset plus what ffprobe says about it.
#[derive(Debug, Serialize, JsonSchema)]
pub struct AssetInfo {
    /// Asset ID to pass to other tools.
    pub asset_id: String,
    /// Absolute path of the file in the workspace.
    pub path: PathBuf,
    #[serde(flatten)]
    pub info: MediaInfo,
}

/// Engine and sandbox failures become tool results with is_error, so the model can self-correct.
pub fn to_result<T: Serialize>(r: Result<T>) -> CallToolResult {
    match r.and_then(|v| {
        serde_json::to_value(v).map_err(|e| Error::InvalidInput(format!("serialize: {e}")))
    }) {
        Ok(v) => CallToolResult::structured(v),
        Err(e) => error_result(e),
    }
}

pub fn error_result(e: Error) -> CallToolResult {
    CallToolResult::error(vec![ContentBlock::text(e.to_string())])
}
