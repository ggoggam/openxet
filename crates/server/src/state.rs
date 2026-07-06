use std::sync::Arc;

use crate::auth::JwksCache;
use crate::config::AppConfig;
use crate::storage::{ObjectStoreBackend, RocksDbChunkIndex, RocksDbFileIndex};

#[derive(Clone)]
pub struct AppState {
    pub storage: Arc<ObjectStoreBackend>,
    pub file_index: Arc<RocksDbFileIndex>,
    pub chunk_index: Arc<RocksDbChunkIndex>,
    pub config: Arc<AppConfig>,
    pub jwks: Arc<JwksCache>,
}
