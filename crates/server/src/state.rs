use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use tokio::sync::Mutex;

use crate::config::AppConfig;
use crate::storage::{FilesystemChunkIndex, FilesystemFileIndex, StorageDispatch};

/// State for an in-progress multipart upload session.
pub struct UploadSession {
    pub file_size: u64,
    pub temp_path: PathBuf,
    pub bytes_received: u64,
    pub next_part: u32,
    pub created_at: Instant,
}

#[derive(Clone)]
pub struct AppState {
    pub storage: Arc<StorageDispatch>,
    pub file_index: Arc<FilesystemFileIndex>,
    pub chunk_index: Arc<FilesystemChunkIndex>,
    pub config: Arc<AppConfig>,
    pub upload_sessions: Arc<Mutex<HashMap<String, UploadSession>>>,
}
