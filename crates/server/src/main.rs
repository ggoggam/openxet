use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use clap::Parser;
use tokio::net::TcpListener;
use tokio::sync::Mutex;
use tracing_subscriber::EnvFilter;

use openxet_server::config::{AppConfig, Cli};
use openxet_server::routes::build_router;
use openxet_server::state::AppState;
use openxet_server::storage::{FilesystemChunkIndex, FilesystemFileIndex, build_storage};

/// Upload sessions older than this are cleaned up automatically.
const UPLOAD_SESSION_TTL: Duration = Duration::from_secs(30 * 60); // 30 minutes

/// How often to run the cleanup sweep.
const CLEANUP_INTERVAL: Duration = Duration::from_secs(60);

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

    tracing::info!(
        host = %config.server.host,
        port = config.server.port,
        data_dir = %config.storage.data_dir.display(),
        "starting openxet-server"
    );

    // Initialize storage
    let data_dir = config.data_dir();
    let storage = Arc::new(build_storage(&config.storage).await?);
    let file_index = Arc::new(FilesystemFileIndex::new(data_dir).await?);
    let chunk_index = Arc::new(FilesystemChunkIndex::new(data_dir).await?);

    // Create uploads temp directory
    let uploads_dir = data_dir.join("uploads").join("tmp");
    tokio::fs::create_dir_all(&uploads_dir).await?;

    let upload_sessions = Arc::new(Mutex::new(HashMap::new()));

    let state = AppState {
        storage,
        file_index,
        chunk_index,
        config: Arc::new(config.clone()),
        upload_sessions: upload_sessions.clone(),
    };

    // Spawn background task to clean up expired upload sessions
    tokio::spawn(cleanup_expired_sessions(upload_sessions));

    let app = build_router(state);

    let addr = format!("{}:{}", config.server.host, config.server.port);
    let listener = TcpListener::bind(&addr).await?;
    tracing::info!("listening on {addr}");

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;

    Ok(())
}

async fn cleanup_expired_sessions(
    sessions: Arc<Mutex<HashMap<String, openxet_server::state::UploadSession>>>,
) {
    loop {
        tokio::time::sleep(CLEANUP_INTERVAL).await;
        let now = Instant::now();
        let mut map = sessions.lock().await;
        let expired: Vec<String> = map
            .iter()
            .filter(|(_, s)| now.duration_since(s.created_at) > UPLOAD_SESSION_TTL)
            .map(|(id, _)| id.clone())
            .collect();
        for id in &expired {
            if let Some(session) = map.remove(id) {
                let _ = tokio::fs::remove_file(&session.temp_path).await;
                tracing::info!(session_id = %id, "cleaned up expired upload session");
            }
        }
    }
}

async fn shutdown_signal() {
    tokio::signal::ctrl_c()
        .await
        .expect("failed to listen for ctrl+c");
    tracing::info!("shutdown signal received");
}
