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
    /// Secret for the server's self-minted short-lived fetch-URL tokens
    /// (reconstruction responses). Generated randomly at startup and never
    /// shared with clients — external clients authenticate via OIDC only.
    // ponytail: per-process secret; a restart invalidates in-flight fetch URLs
    // and multi-replica deployments need sticky routing. Make it configurable
    // if either bites.
    pub fetch_token_secret: Arc<str>,
}
