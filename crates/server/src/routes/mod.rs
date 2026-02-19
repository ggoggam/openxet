mod dedup;
mod management;
mod reconstruction;
mod shard;
mod upload;
mod xorb;

use axum::Router;
use axum::extract::DefaultBodyLimit;
use axum::http::{self, HeaderValue, Request};
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

    let trace_layer = TraceLayer::new_for_http()
        .make_span_with(|request: &Request<_>| {
            tracing::info_span!(
                "http_request",
                method = %request.method(),
                path = %request.uri().path(),
            )
        })
        .on_request(|request: &Request<_>, _span: &tracing::Span| {
            let content_length = request
                .headers()
                .get(http::header::CONTENT_LENGTH)
                .and_then(|v: &HeaderValue| v.to_str().ok())
                .unwrap_or("-");
            tracing::info!(
                method = %request.method(),
                path = %request.uri().path(),
                query = request.uri().query().unwrap_or(""),
                content_length = %content_length,
                user_agent = request.headers().get(http::header::USER_AGENT)
                    .and_then(|v: &HeaderValue| v.to_str().ok())
                    .unwrap_or("-"),
                "request started",
            );
        })
        .on_response(
            |response: &http::Response<_>, latency: std::time::Duration, _span: &tracing::Span| {
                let content_length = response
                    .headers()
                    .get(http::header::CONTENT_LENGTH)
                    .and_then(|v: &HeaderValue| v.to_str().ok())
                    .unwrap_or("-");
                tracing::info!(
                    status = response.status().as_u16(),
                    latency_ms = latency.as_secs_f64() * 1000.0,
                    content_length = %content_length,
                    "request completed",
                );
            },
        )
        .on_failure(
            |error: tower_http::classify::ServerErrorsFailureClass,
             latency: std::time::Duration,
             _span: &tracing::Span| {
                tracing::error!(
                    error = %error,
                    latency_ms = latency.as_secs_f64() * 1000.0,
                    "request failed",
                );
            },
        );

    Router::new()
        .nest("/api", management::management_router())
        .nest("/api", upload::upload_router())
        .merge(cas_routes)
        .fallback_service(spa_fallback)
        .layer(DefaultBodyLimit::max(MAX_BODY_SIZE))
        .layer(trace_layer)
        .layer(CorsLayer::permissive())
        .with_state(state)
}
