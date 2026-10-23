use std::sync::Arc;

use crate::auth::JwksCache;
use crate::config::AppConfig;
use crate::storage::{ChunkIndexBackend, FileIndexBackend, ObjectStoreBackend};

#[derive(Clone)]
pub struct AppState {
    pub storage: Arc<ObjectStoreBackend>,
    pub file_index: Arc<FileIndexBackend>,
    pub chunk_index: Arc<ChunkIndexBackend>,
    pub config: Arc<AppConfig>,
    pub jwks: Arc<JwksCache>,
    /// Secret used to verify symmetric (HS256) bearer tokens. Generated randomly
    /// at startup and never shared with clients — external clients authenticate
    /// via OIDC only. In production nothing mints against it (fetch URLs are now
    /// presigned / unauthenticated); it backs the HS256 path used by tests and
    /// trusted/dev setups.
    pub fetch_token_secret: Arc<str>,
    /// HMAC key for the signed xorb fetch URLs handed out on backends that
    /// cannot presign (see [`crate::auth::fetch_url`]). Kept separate from
    /// `fetch_token_secret` so a URL signature can never double as a JWT
    /// signature or vice versa.
    pub xorb_url_key: Arc<[u8]>,
}
