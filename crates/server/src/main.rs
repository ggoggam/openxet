use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use clap::Parser;
use tokio::net::TcpListener;
use tracing_subscriber::EnvFilter;

use openxet_server::auth::JwksCache;
use openxet_server::config::{AppConfig, Cli};
use openxet_server::routes::build_router;
use openxet_server::state::AppState;
use openxet_server::storage::{build_index, build_storage};

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

    if !config.auth.enabled {
        tracing::warn!(
            "auth is DISABLED — anyone who can reach this server can read and \
             write. Development/trusted-network use only."
        );
    } else if config.auth.oidc_issuers.is_empty() {
        tracing::warn!(
            "auth is enabled but no OIDC issuers are configured — no external \
             client can authenticate. Set auth.oidc_issuers (or \
             OPENXET_OIDC_ISSUERS), or disable auth for development."
        );
    }

    tracing::info!(
        host = %config.server.host,
        port = config.server.port,
        data_dir = %config.storage.data_dir.display(),
        "starting openxet-server"
    );

    // Initialize storage
    let storage = Arc::new(build_storage(&config.storage).await?);
    let (file_index, chunk_index, s3_index) = build_index(&config.storage).await?;
    let file_index = Arc::new(file_index);
    let chunk_index = Arc::new(chunk_index);
    let s3_index = Arc::new(s3_index);

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

    let fetch_token_secret: Arc<str> = {
        use rand::Rng;
        let bytes: [u8; 32] = rand::thread_rng().r#gen();
        hex::encode(bytes).into()
    };

    let state = AppState {
        storage,
        file_index,
        chunk_index,
        s3_index,
        config: Arc::new(config.clone()),
        jwks,
        fetch_token_secret,
    };

    if let Some(interval_seconds) = config.gc.interval_seconds {
        let gc_state = state.clone();
        let grace = Duration::from_secs(config.gc.grace_seconds);
        let interval = Duration::from_secs(interval_seconds.max(1));
        tracing::info!(
            interval_seconds,
            grace_seconds = config.gc.grace_seconds,
            "background GC enabled"
        );
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(interval).await;
                if let Err(e) = openxet_server::gc::run_gc(&gc_state, grace).await {
                    tracing::error!(error = %e, "background gc pass failed");
                }
            }
        });
    }

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
