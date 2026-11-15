use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use schemars::JsonSchema;
use serde::Deserialize;

use crate::{
    error::{Error, Result},
    sandbox::{Sandbox, reject_url},
};

/// Reference to a media file. Give exactly one field.
#[derive(Debug, Clone, Default, Deserialize, JsonSchema)]
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

/// Maps opaque asset IDs to files inside the workspace.
/// ponytail: in-memory only, IDs are lost on restart; persist an index file if that hurts.
#[derive(Debug, Clone)]
pub struct Assets {
    sandbox: Sandbox,
    map: Arc<Mutex<HashMap<String, PathBuf>>>,
}

impl Assets {
    pub fn new(sandbox: Sandbox) -> Self {
        Self {
            sandbox,
            map: Default::default(),
        }
    }

    pub fn sandbox(&self) -> &Sandbox {
        &self.sandbox
    }

    /// The only way a file from outside the workspace enters it: by copy.
    pub async fn import(&self, src: impl AsRef<Path>) -> Result<(String, PathBuf)> {
        let src = src.as_ref();
        reject_url(&src.to_string_lossy())?;
        let src = tokio::fs::canonicalize(src)
            .await
            .map_err(|e| Error::io(src, e))?;
        let meta = tokio::fs::metadata(&src)
            .await
            .map_err(|e| Error::io(&src, e))?;
        if !meta.is_file() {
            return Err(Error::Sandbox(format!("{} is not a file", src.display())));
        }
        let id = uuid::Uuid::new_v4().to_string();
        let name = match src.extension().and_then(|e| e.to_str()) {
            Some(ext) => format!("{id}.{}", ext.to_ascii_lowercase()),
            None => id.clone(),
        };
        let dest = self.sandbox.new_path(&name).await?;
        tokio::fs::copy(&src, &dest)
            .await
            .map_err(|e| Error::io(&dest, e))?;
        self.insert(id.clone(), dest.clone());
        Ok((id, dest))
    }

    /// Register a file a tool just produced; it must already be inside the workspace.
    pub async fn register(&self, path: impl AsRef<Path>) -> Result<String> {
        let path = self.sandbox.resolve(path).await?;
        let id = uuid::Uuid::new_v4().to_string();
        self.insert(id.clone(), path);
        Ok(id)
    }

    pub fn get(&self, id: &str) -> Result<PathBuf> {
        self.lock()
            .get(id)
            .cloned()
            .ok_or_else(|| Error::AssetNotFound(id.to_string()))
    }

    fn insert(&self, id: String, path: PathBuf) {
        self.lock().insert(id, path);
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, PathBuf>> {
        // A poisoned map is still a valid map: no invariant spans the lock.
        self.map.lock().unwrap_or_else(|p| p.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp() -> PathBuf {
        let p = std::env::temp_dir().join(format!("media-mcp-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[tokio::test]
    async fn import_copies_into_workspace_and_roundtrips() {
        let d = tmp();
        let src = d.join("Clip.MP4");
        std::fs::write(&src, b"data").unwrap();
        let a = Assets::new(Sandbox::new(d.join("ws")).await.unwrap());
        let (id, path) = a.import(&src).await.unwrap();
        assert!(path.starts_with(a.sandbox().root()));
        assert_eq!(path.extension().unwrap(), "mp4");
        assert_eq!(std::fs::read(&path).unwrap(), b"data");
        assert_eq!(a.get(&id).unwrap(), path);
        assert!(a.get("nope").is_err());
        std::fs::remove_dir_all(d).unwrap();
    }

    #[tokio::test]
    async fn import_rejects_urls_and_dirs() {
        let d = tmp();
        let a = Assets::new(Sandbox::new(d.join("ws")).await.unwrap());
        assert!(a.import("https://x/y.mp4").await.is_err());
        assert!(a.import(&d).await.is_err());
        std::fs::remove_dir_all(d).unwrap();
    }

    #[tokio::test]
    async fn register_rejects_outside_files() {
        let d = tmp();
        std::fs::write(d.join("o.png"), b"x").unwrap();
        let a = Assets::new(Sandbox::new(d.join("ws")).await.unwrap());
        assert!(a.register(d.join("o.png")).await.is_err());
        std::fs::remove_dir_all(d).unwrap();
    }
}
