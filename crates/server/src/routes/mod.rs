mod dedup;
mod management;
mod reconstruction;
mod shard;
mod upload;
mod xorb;

use axum::Router;
use axum::extract::DefaultBodyLimit;
use axum::routing::{get, post};
use tower_http::cors::CorsLayer;
use tower_http::services::{ServeDir, ServeFile};
use tower_http::trace::TraceLayer;

use crate::state::AppState;

const MAX_BODY_SIZE: usize = 64 * 1024 * 1024; // 64 MiB

pub fn build_router(state: AppState) -> Router {
    let cas_routes = Router::new()
        .route(
            "/v1/xorbs/default/{hash}",
            get(xorb::get_xorb).post(xorb::post_xorb),
        )
        .route("/v1/shards", post(shard::post_shard))
        // xet-core's RemoteClient posts to /shards (no /v1/ prefix)
        .route("/shards", post(shard::post_shard))
        .route(
            "/v1/reconstructions/{file_id}",
            get(reconstruction::get_reconstruction),
        )
        .route("/v1/chunks/default-merkledb/{hash}", get(dedup::get_dedup));

    let frontend_dir = &state.config.server.frontend_dir;
    let spa_fallback = ServeDir::new(frontend_dir)
        .not_found_service(ServeFile::new(frontend_dir.join("index.html")));

    Router::new()
        .nest("/api", management::management_router())
        .nest("/api", upload::upload_router())
        .merge(cas_routes)
        .fallback_service(spa_fallback)
        .layer(DefaultBodyLimit::max(MAX_BODY_SIZE))
        .layer(TraceLayer::new_for_http())
        .layer(CorsLayer::permissive())
        .with_state(state)
}
