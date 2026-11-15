use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("path rejected: {0}")]
    Sandbox(String),
    #[error("unknown asset id: {0}")]
    AssetNotFound(String),
    #[error("{name} not found: set {env} or put it on PATH")]
    EngineMissing {
        name: &'static str,
        env: &'static str,
    },
    #[error("unknown job id: {0}")]
    JobNotFound(String),
    #[error("invalid input: {0}")]
    InvalidInput(String),
    #[error("{program} failed ({status}):\n{stderr_tail}")]
    Engine {
        program: &'static str,
        status: String,
        stderr_tail: String,
    },
    #[error("could not parse {program} output: {detail}")]
    Parse {
        program: &'static str,
        detail: String,
    },
    #[error("io error on {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

impl Error {
    pub fn io(path: impl Into<PathBuf>, source: std::io::Error) -> Self {
        Error::Io {
            path: path.into(),
            source,
        }
    }
}

pub type Result<T> = std::result::Result<T, Error>;
