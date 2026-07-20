mod admin;
mod dedup;
mod files;
mod reconstruction;
mod s3;
mod shard;
mod xorb;
mod xorb_meta;

use axum::Router;
use axum::extract::DefaultBodyLimit;
use axum::http::{self, HeaderValue, Request};
use axum::routing::{get, post};
use tower_http::cors::CorsLayer;
use tower_http::services::{ServeDir, ServeFile};
use tower_http::trace::TraceLayer;

use crate::state::AppState;

// 1 MiB above the handlers' own payload caps (68 MiB for serialized xorbs,
// 64 MiB for shards) so those checks fire first and return 413 with a
// payload-specific message, not axum's generic body-limit 413.
const MAX_BODY_SIZE: usize = 69 * 1024 * 1024;

/// Replace any `token=…` value in a query string with a placeholder as a
/// defensive measure so credentials never land in request logs.
fn redact_query(query: &str) -> String {
    query
        .split('&')
        .map(|kv| {
            if kv.starts_with("token=") {
                "token=REDACTED"
            } else {
                kv
            }
        })
        .collect::<Vec<_>>()
        .join("&")
}

pub fn build_router(state: AppState) -> Router {
    let cas_routes = Router::new()
        .route("/v1/xorbs", get(xorb::list_xorbs))
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
        // xet-core prefers the V2 reconstruction API and falls back to V1 on
        // 404/501, caching whichever version answered.
        .route(
            "/v2/reconstructions/{file_id}",
            get(reconstruction::get_reconstruction_v2),
        )
        .route("/v1/chunks/default-merkledb/{hash}", get(dedup::get_dedup))
        // Management/lifecycle endpoints (not part of the Xet wire protocol):
        // list and inspect files, release a file, collect garbage, inspect usage.
        .route("/v1/files", get(files::list_files))
        .route(
            "/v1/files/{file_id}",
            get(files::get_file).delete(files::delete_file),
        )
        .route("/v1/gc", post(admin::post_gc))
        .route("/v1/accounting", get(admin::get_accounting))
        // S3 gateway management: register a friendly (bucket, key) name for an
        // already-uploaded file, list/browse names, and delete them. The read
        // gateway itself is mounted separately (below) under the S3 prefix.
        .route(
            "/v1/s3/objects",
            get(s3::list_objects)
                .post(s3::register_object)
                .delete(s3::delete_object),
        )
        .route("/v1/s3/buckets", get(s3::list_buckets))
        .route("/v1/s3/info", get(s3::gateway_info))
        // Mint / list / revoke SigV4 access-key/secret pairs for signing
        // gateway requests.
        .route(
            "/v1/s3/credentials",
            get(s3::list_credentials).post(s3::create_credential),
        )
        .route(
            "/v1/s3/credentials/{access_key_id}",
            axum::routing::delete(s3::delete_credential),
        );

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
                query = %redact_query(request.uri().query().unwrap_or("")),
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

    let mut app = Router::new().merge(cas_routes);

    // Mount the S3-compatible read gateway under its configured prefix when
    // enabled. Merged (with full `/{prefix}/…` route paths) rather than nested
    // so the SigV4 verifier sees the exact path the client signed. Sits before
    // the SPA fallback so bucket/key paths under the prefix are matched here.
    if state.config.server.s3_gateway_enabled {
        app = app.merge(s3::gateway_routes(&state.config.server.s3_gateway_prefix));
    }

    app.fallback_service(spa_fallback)
        .layer(DefaultBodyLimit::max(MAX_BODY_SIZE))
        .layer(trace_layer)
        .layer(CorsLayer::permissive())
        .with_state(state)
}
