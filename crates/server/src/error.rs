use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};

use openxet_cas_types::shard::ShardError;
use openxet_cas_types::xorb::XorbError;

use crate::storage::StorageError;

#[derive(Debug, thiserror::Error)]
pub enum AppError {
    #[error("bad request: {0}")]
    BadRequest(String),

    #[error("unauthorized: {0}")]
    Unauthorized(String),

    #[error("not found: {0}")]
    NotFound(String),

    #[error("range not satisfiable")]
    RangeNotSatisfiable,

    #[error("payload too large")]
    PayloadTooLarge,

    #[error("internal error: {0}")]
    Internal(#[from] anyhow::Error),
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let (status, message) = match &self {
            AppError::BadRequest(msg) => (StatusCode::BAD_REQUEST, msg.clone()),
            AppError::Unauthorized(msg) => (StatusCode::UNAUTHORIZED, msg.clone()),
            AppError::NotFound(msg) => (StatusCode::NOT_FOUND, msg.clone()),
            AppError::RangeNotSatisfiable => (
                StatusCode::RANGE_NOT_SATISFIABLE,
                "range not satisfiable".to_string(),
            ),
            AppError::PayloadTooLarge => (
                StatusCode::PAYLOAD_TOO_LARGE,
                "payload too large".to_string(),
            ),
            AppError::Internal(err) => {
                tracing::error!("internal error: {err:?}");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "internal server error".to_string(),
                )
            }
        };

        let body = serde_json::json!({ "error": message });
        (status, axum::Json(body)).into_response()
    }
}

impl From<StorageError> for AppError {
    fn from(err: StorageError) -> Self {
        match err {
            StorageError::NotFound(msg) => AppError::NotFound(msg),
            StorageError::InvalidHash(msg) => AppError::BadRequest(format!("invalid hash: {msg}")),
            StorageError::TooLarge { size, max } => {
                tracing::warn!("payload too large: {size} bytes (max {max})");
                AppError::PayloadTooLarge
            }
            StorageError::Io { source, path } => AppError::Internal(anyhow::anyhow!(
                "io error on {}: {}",
                path.display(),
                source
            )),
            StorageError::ObjectStore(msg) => {
                AppError::Internal(anyhow::anyhow!("object store error: {msg}"))
            }
        }
    }
}

impl From<XorbError> for AppError {
    fn from(err: XorbError) -> Self {
        AppError::BadRequest(format!("invalid xorb: {err}"))
    }
}

impl From<ShardError> for AppError {
    fn from(err: ShardError) -> Self {
        AppError::BadRequest(format!("invalid shard: {err}"))
    }
}
