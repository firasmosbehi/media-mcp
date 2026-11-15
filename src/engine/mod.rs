use std::path::{Path, PathBuf};

use crate::error::{Error, Result};

/// Locations of the external binaries the server shells out to.
#[derive(Debug, Clone)]
pub struct Engines {
    pub ffmpeg: PathBuf,
    pub ffprobe: PathBuf,
    pub vips: PathBuf,
}

impl Engines {
    /// Each binary comes from its env var if set, otherwise from PATH.
    pub fn detect() -> Result<Self> {
        let path = std::env::var_os("PATH").unwrap_or_default();
        let dirs: Vec<PathBuf> = std::env::split_paths(&path).collect();
        let find = |name: &'static str, env: &'static str| -> Result<PathBuf> {
            match std::env::var_os(env) {
                Some(p) if !p.is_empty() => Ok(PathBuf::from(p)),
                _ => find_in(&dirs, name).ok_or(Error::EngineMissing { name, env }),
            }
        };
        Ok(Self {
            ffmpeg: find("ffmpeg", "FFMPEG_PATH")?,
            ffprobe: find("ffprobe", "FFPROBE_PATH")?,
            vips: find("vips", "VIPS_PATH")?,
        })
    }
}

fn find_in(dirs: &[PathBuf], name: &str) -> Option<PathBuf> {
    dirs.iter().map(|d| d.join(name)).find(|p| is_executable(p))
}

fn is_executable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    p.metadata()
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn find_in_skips_missing_and_non_executable() {
        let d = std::env::temp_dir().join(format!("media-mcp-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("tool"), b"").unwrap(); // mode 0644
        assert!(find_in(std::slice::from_ref(&d), "tool").is_none());
        assert!(find_in(std::slice::from_ref(&d), "absent").is_none());
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(d.join("tool"), std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(
            find_in(std::slice::from_ref(&d), "tool"),
            Some(d.join("tool"))
        );
        std::fs::remove_dir_all(d).unwrap();
    }
}
