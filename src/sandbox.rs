use std::path::{Path, PathBuf};

use crate::error::{Error, Result};

/// All media files live under one canonical workspace root.
#[derive(Debug, Clone)]
pub struct Sandbox {
    root: PathBuf,
}

/// Network URLs and protocol-style inputs are never valid paths.
pub fn reject_url(s: &str) -> Result<()> {
    if s.contains("://") || s.starts_with("data:") || s.contains('\0') {
        return Err(Error::Sandbox(format!("not a local path: {s}")));
    }
    Ok(())
}

impl Sandbox {
    /// Create the workspace directory if needed and canonicalize it.
    pub async fn new(root: impl AsRef<Path>) -> Result<Self> {
        let root = root.as_ref();
        tokio::fs::create_dir_all(root)
            .await
            .map_err(|e| Error::io(root, e))?;
        let root = tokio::fs::canonicalize(root)
            .await
            .map_err(|e| Error::io(root, e))?;
        Ok(Self { root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Resolve an existing file; symlinks are followed and the result must stay inside the root.
    pub async fn resolve(&self, path: impl AsRef<Path>) -> Result<PathBuf> {
        let path = path.as_ref();
        reject_url(&path.to_string_lossy())?;
        let joined = self.root.join(path); // absolute input replaces the root; checked below
        let canon = tokio::fs::canonicalize(&joined)
            .await
            .map_err(|e| Error::io(&joined, e))?;
        if !canon.starts_with(&self.root) {
            return Err(Error::Sandbox(format!(
                "{} is outside the workspace",
                path.display()
            )));
        }
        Ok(canon)
    }

    /// Path for a file that does not exist yet: a bare file name directly inside the root.
    pub async fn new_path(&self, file_name: &str) -> Result<PathBuf> {
        reject_url(file_name)?;
        let ok = !file_name.is_empty()
            && file_name != "."
            && file_name != ".."
            && !file_name.contains(['/', '\\']);
        if !ok {
            return Err(Error::Sandbox(format!("invalid file name: {file_name}")));
        }
        let p = self.root.join(file_name);
        // A pre-existing symlink at the target could redirect the write out of the root.
        if let Ok(meta) = tokio::fs::symlink_metadata(&p).await
            && meta.file_type().is_symlink()
        {
            return Err(Error::Sandbox(format!("{file_name} is a symlink")));
        }
        Ok(p)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn sandbox() -> (tempdir::Dir, Sandbox) {
        let d = tempdir::Dir::new();
        let s = Sandbox::new(d.path().join("ws")).await.unwrap();
        (d, s)
    }

    /// Tiny temp dir helper to avoid a dev-dependency.
    mod tempdir {
        use std::path::{Path, PathBuf};
        pub struct Dir(PathBuf);
        impl Dir {
            pub fn new() -> Self {
                let p = std::env::temp_dir().join(format!("media-mcp-{}", uuid::Uuid::new_v4()));
                std::fs::create_dir_all(&p).unwrap();
                Dir(p)
            }
            pub fn path(&self) -> &Path {
                &self.0
            }
        }
        impl Drop for Dir {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
    }

    #[tokio::test]
    async fn resolves_inside_and_rejects_dotdot() {
        let (d, s) = sandbox().await;
        std::fs::write(s.root().join("a.txt"), b"x").unwrap();
        std::fs::write(d.path().join("outside.txt"), b"x").unwrap();
        assert!(s.resolve("a.txt").await.is_ok());
        assert!(s.resolve("../outside.txt").await.is_err());
    }

    #[tokio::test]
    async fn rejects_absolute_outside_and_urls() {
        let (d, s) = sandbox().await;
        std::fs::write(d.path().join("outside.txt"), b"x").unwrap();
        assert!(s.resolve(d.path().join("outside.txt")).await.is_err());
        assert!(s.resolve("http://example.com/a.mp4").await.is_err());
        assert!(s.resolve("file:///etc/passwd").await.is_err());
    }

    #[tokio::test]
    async fn rejects_symlink_escape() {
        let (d, s) = sandbox().await;
        std::fs::write(d.path().join("outside.txt"), b"x").unwrap();
        std::os::unix::fs::symlink(d.path().join("outside.txt"), s.root().join("link")).unwrap();
        assert!(s.resolve("link").await.is_err());
        assert!(s.new_path("link").await.is_err());
    }

    #[tokio::test]
    async fn new_path_requires_bare_name() {
        let (_d, s) = sandbox().await;
        assert!(s.new_path("out.mp4").await.is_ok());
        for bad in ["", ".", "..", "a/b.mp4", "../x", "a\\b", "http://x/y"] {
            assert!(s.new_path(bad).await.is_err(), "{bad}");
        }
    }
}
