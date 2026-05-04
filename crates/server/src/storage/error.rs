use std::io;
use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub enum StorageError {
    #[error("blob not found: {0}")]
    NotFound(String),

    #[error("invalid hash format: {0}")]
    InvalidHash(String),

    #[error("io error on {path}: {source}")]
    Io { source: io::Error, path: PathBuf },

    #[error("data too large: {size} bytes (max {max})")]
    TooLarge { size: usize, max: usize },

    #[error("object store error: {0}")]
    ObjectStore(String),
}

impl StorageError {
    /// Create an IO error with the path that caused it.
    pub fn io(source: io::Error, path: impl Into<PathBuf>) -> Self {
        Self::Io {
            source,
            path: path.into(),
        }
    }
}
