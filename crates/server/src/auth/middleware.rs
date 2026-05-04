use axum::extract::FromRequestParts;
use axum::http::request::Parts;

use crate::error::AppError;
use crate::state::AppState;

use super::jwt::{Claims, Scope, validate_token};

/// Extract and validate the Bearer token from the Authorization header.
fn extract_bearer_token(parts: &Parts) -> Result<&str, AppError> {
    let header = parts
        .headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| AppError::Unauthorized("missing authorization header".to_string()))?;

    header
        .strip_prefix("Bearer ")
        .ok_or_else(|| AppError::Unauthorized("invalid authorization header format".to_string()))
}

/// Axum extractor that requires at least `read` scope.
pub struct RequireRead(pub Claims);

impl FromRequestParts<AppState> for RequireRead {
    type Rejection = AppError;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, AppError> {
        let token = extract_bearer_token(parts)?;
        let claims = validate_token(&state.config.auth.secret, token)
            .map_err(|e| AppError::Unauthorized(format!("invalid token: {e}")))?;

        if !claims.scope.satisfies(Scope::Read) {
            return Err(AppError::Unauthorized(
                "insufficient scope: read required".to_string(),
            ));
        }

        Ok(RequireRead(claims))
    }
}

/// Axum extractor that requires `write` scope.
pub struct RequireWrite(pub Claims);

impl FromRequestParts<AppState> for RequireWrite {
    type Rejection = AppError;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, AppError> {
        let token = extract_bearer_token(parts)?;
        let claims = validate_token(&state.config.auth.secret, token)
            .map_err(|e| AppError::Unauthorized(format!("invalid token: {e}")))?;

        if !claims.scope.satisfies(Scope::Write) {
            return Err(AppError::Unauthorized(
                "insufficient scope: write required".to_string(),
            ));
        }

        Ok(RequireWrite(claims))
    }
}
