use std::sync::Arc;

use crate::config::AppConfig;
use crate::storage::{FilesystemChunkIndex, FilesystemFileIndex, ObjectStoreBackend};

#[derive(Clone)]
pub struct AppState {
    pub storage: Arc<ObjectStoreBackend>,
    pub file_index: Arc<FilesystemFileIndex>,
    pub chunk_index: Arc<FilesystemChunkIndex>,
    pub config: Arc<AppConfig>,
}
