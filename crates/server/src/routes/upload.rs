use axum::Json;
use axum::Router;
use axum::extract::{Path, State};
use axum::routing::{delete, post, put};
use bytes::Bytes;
use rand::RngCore;
use serde::{Deserialize, Serialize};
use std::time::Instant;
use tokio::io::{AsyncSeekExt, AsyncWriteExt};

use crate::error::AppError;
use crate::state::{AppState, UploadSession};

use super::management::{UploadResponse, process_file_upload};

pub fn upload_router() -> Router<AppState> {
    Router::new()
        .route("/upload/init", post(init_upload))
        .route("/upload/{session_id}/{part_index}", put(upload_part))
        .route("/upload/{session_id}/complete", post(complete_upload))
        .route("/upload/{session_id}", delete(abort_upload))
}

// ─── POST /api/upload/init ───────────────────────────────────────────────────

#[derive(Deserialize)]
struct InitRequest {
    file_size: u64,
}

#[derive(Serialize)]
struct InitResponse {
    session_id: String,
}

async fn init_upload(
    State(state): State<AppState>,
    Json(req): Json<InitRequest>,
) -> Result<Json<InitResponse>, AppError> {
    if req.file_size == 0 {
        return Err(AppError::BadRequest("file_size must be > 0".to_string()));
    }

    // Generate a random 32-char hex session ID
    let mut buf = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut buf);
    let session_id: String = buf.iter().map(|b| format!("{:02x}", b)).collect();

    // Create temp file
    let uploads_dir = state.config.data_dir().join("uploads").join("tmp");
    let temp_path = uploads_dir.join(&session_id);

    // Create empty temp file
    tokio::fs::File::create(&temp_path)
        .await
        .map_err(|e| AppError::Internal(anyhow::anyhow!("failed to create temp file: {e}")))?;

    let session = UploadSession {
        file_size: req.file_size,
        temp_path,
        bytes_received: 0,
        next_part: 0,
        created_at: Instant::now(),
    };

    state
        .upload_sessions
        .lock()
        .await
        .insert(session_id.clone(), session);

    tracing::info!(session_id = %session_id, file_size = req.file_size, "upload session created");

    Ok(Json(InitResponse { session_id }))
}

// ─── PUT /api/upload/{session_id}/{part_index} ───────────────────────────────

#[derive(Serialize)]
struct PartResponse {
    received: u64,
}

async fn upload_part(
    State(state): State<AppState>,
    Path((session_id, part_index)): Path<(String, u32)>,
    body: Bytes,
) -> Result<Json<PartResponse>, AppError> {
    if body.is_empty() {
        return Err(AppError::BadRequest("empty part body".to_string()));
    }

    let mut sessions = state.upload_sessions.lock().await;
    let session = sessions
        .get_mut(&session_id)
        .ok_or_else(|| AppError::NotFound(format!("upload session not found: {session_id}")))?;

    if part_index != session.next_part {
        return Err(AppError::BadRequest(format!(
            "expected part index {}, got {part_index}",
            session.next_part
        )));
    }

    let new_total = session.bytes_received + body.len() as u64;
    if new_total > session.file_size {
        return Err(AppError::BadRequest(format!(
            "received bytes ({new_total}) would exceed declared file_size ({})",
            session.file_size
        )));
    }

    // Append to temp file
    let mut file = tokio::fs::OpenOptions::new()
        .write(true)
        .open(&session.temp_path)
        .await
        .map_err(|e| AppError::Internal(anyhow::anyhow!("failed to open temp file: {e}")))?;

    file.seek(std::io::SeekFrom::End(0))
        .await
        .map_err(|e| AppError::Internal(anyhow::anyhow!("seek failed: {e}")))?;

    file.write_all(&body)
        .await
        .map_err(|e| AppError::Internal(anyhow::anyhow!("write failed: {e}")))?;

    file.flush()
        .await
        .map_err(|e| AppError::Internal(anyhow::anyhow!("flush failed: {e}")))?;

    session.bytes_received = new_total;
    session.next_part = part_index + 1;

    Ok(Json(PartResponse {
        received: new_total,
    }))
}

// ─── POST /api/upload/{session_id}/complete ──────────────────────────────────

async fn complete_upload(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
) -> Result<Json<UploadResponse>, AppError> {
    let session = {
        let mut sessions = state.upload_sessions.lock().await;
        sessions
            .remove(&session_id)
            .ok_or_else(|| AppError::NotFound(format!("upload session not found: {session_id}")))?
    };

    if session.bytes_received != session.file_size {
        // Clean up temp file
        let _ = tokio::fs::remove_file(&session.temp_path).await;
        return Err(AppError::BadRequest(format!(
            "incomplete upload: received {} of {} bytes",
            session.bytes_received, session.file_size
        )));
    }

    // Read the complete file from disk
    let file_data = tokio::fs::read(&session.temp_path)
        .await
        .map_err(|e| AppError::Internal(anyhow::anyhow!("failed to read temp file: {e}")))?;

    // Clean up temp file
    let _ = tokio::fs::remove_file(&session.temp_path).await;

    tracing::info!(
        session_id = %session_id,
        file_size = file_data.len(),
        "processing completed upload"
    );

    // Reuse the shared processing pipeline
    let result = process_file_upload(&state, &file_data).await?;
    Ok(Json(result))
}

// ─── DELETE /api/upload/{session_id} ─────────────────────────────────────────

async fn abort_upload(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
) -> Result<Json<serde_json::Value>, AppError> {
    let session = {
        let mut sessions = state.upload_sessions.lock().await;
        sessions.remove(&session_id)
    };

    if let Some(session) = session {
        let _ = tokio::fs::remove_file(&session.temp_path).await;
        tracing::info!(session_id = %session_id, "upload session aborted");
    }

    Ok(Json(serde_json::json!({})))
}
