use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use clap::Parser;
use tokio::net::TcpListener;
use tracing_subscriber::EnvFilter;

use openxet_server::auth::JwksCache;
use openxet_server::config::{AppConfig, Cli, DEFAULT_AUTH_SECRET};
use openxet_server::routes::build_router;
use openxet_server::state::AppState;
use openxet_server::storage::{RocksDbChunkIndex, RocksDbFileIndex, build_storage};

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize tracing with JSON output for OTel-compatible structured logging
    tracing_subscriber::fmt()
        .json()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_target(true)
        .with_current_span(true)
        .init();

    // Parse CLI and load config
    let cli = Cli::parse();
    let config = AppConfig::load(&cli)?;

    if config.auth.secret.is_empty() || config.auth.secret == DEFAULT_AUTH_SECRET {
        tracing::warn!(
            "OPENXET_AUTH_SECRET is unset or the built-in default — all JWTs are \
             forgeable and anyone can read or write. Set a strong secret before \
             exposing this server."
        );
    }

    tracing::info!(
        host = %config.server.host,
        port = config.server.port,
        data_dir = %config.storage.data_dir.display(),
        "starting openxet-server"
    );

    // Initialize storage
    let data_dir = config.data_dir();
    let storage = Arc::new(build_storage(&config.storage).await?);
    let file_index = Arc::new(RocksDbFileIndex::new(data_dir)?);
    let chunk_index = Arc::new(RocksDbChunkIndex::new(data_dir)?);

    let jwks = Arc::new(JwksCache::new(
        config.auth.oidc_issuers.clone(),
        Duration::from_secs(config.auth.jwks_ttl_seconds),
    ));
    if jwks.is_enabled() {
        tracing::info!(
            issuers = ?config.auth.oidc_issuers,
            jwks_ttl_seconds = config.auth.jwks_ttl_seconds,
            "OIDC JWKS verification enabled"
        );
    }

    let state = AppState {
        storage,
        file_index,
        chunk_index,
        config: Arc::new(config.clone()),
        jwks,
    };

    let app = build_router(state);

    let addr = format!("{}:{}", config.server.host, config.server.port);
    let listener = TcpListener::bind(&addr).await?;
    tracing::info!("listening on {addr}");

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;

    Ok(())
}

async fn shutdown_signal() {
    tokio::signal::ctrl_c()
        .await
        .expect("failed to listen for ctrl+c");
    tracing::info!("shutdown signal received");
}
