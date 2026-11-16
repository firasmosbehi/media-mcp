pub mod audio_edit;
pub mod image_edit;
pub mod jobs;
pub mod preview;
pub mod probe;
pub mod video_compose;
pub mod video_edit;

use std::path::PathBuf;

use rmcp::model::{CallToolResult, ContentBlock};
use schemars::JsonSchema;
use serde::Serialize;

use crate::{
    engine::probe::MediaInfo,
    error::{Error, Result},
    sandbox::Scratch,
    server::MediaServer,
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

/// Where a new asset goes: written to `tmp` first and renamed to `dest` by `commit`, so a
/// failure or cancel never leaves a partial asset (`tmp` is deleted when this is dropped).
pub struct Output {
    pub dest: PathBuf,
    pub tmp: PathBuf,
    scratch: Scratch,
}

impl Output {
    /// Validate `name` (default: a fresh UUID) and refuse to clobber an existing file
    /// unless `overwrite`.
    pub async fn prepare(
        server: &MediaServer,
        name: Option<&str>,
        overwrite: bool,
        ext: &str,
    ) -> Result<Self> {
        let sandbox = server.assets.sandbox();
        let stem = match name {
            Some(n) if image_edit::valid_stem(n) => n.to_string(),
            Some(n) => return Err(Error::InvalidInput(format!("invalid output_name: {n}"))),
            None => uuid::Uuid::new_v4().to_string(),
        };
        let dest = sandbox.new_path(&format!("{stem}.{ext}")).await?;
        if !overwrite && tokio::fs::try_exists(&dest).await.unwrap_or(false) {
            return Err(Error::InvalidInput(format!(
                "{stem}.{ext} already exists; pass overwrite: true to replace it"
            )));
        }
        let tmp = sandbox
            .new_path(&format!(".{}.tmp.{ext}", uuid::Uuid::new_v4()))
            .await?;
        let scratch = Scratch::new(&tmp);
        Ok(Self { dest, tmp, scratch })
    }

    /// Move the finished file into place and register it as an asset.
    pub async fn commit(self, server: &MediaServer) -> Result<(String, PathBuf)> {
        let Self { dest, tmp, scratch } = self;
        tokio::fs::rename(&tmp, &dest)
            .await
            .map_err(|e| Error::io(&dest, e))?;
        scratch.disarm();
        let id = server.assets.register(&dest).await?;
        Ok((id, dest))
    }
}
