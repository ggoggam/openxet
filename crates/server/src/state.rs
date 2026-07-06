use std::sync::Arc;

use crate::config::AppConfig;
use crate::storage::{RocksDbChunkIndex, RocksDbFileIndex, ObjectStoreBackend};

#[derive(Clone)]
pub struct AppState {
    pub storage: Arc<ObjectStoreBackend>,
    pub file_index: Arc<RocksDbFileIndex>,
    pub chunk_index: Arc<RocksDbChunkIndex>,
    pub config: Arc<AppConfig>,
}
