use axum::extract::FromRequestParts;
use axum::http::request::Parts;
use jsonwebtoken::{Algorithm, DecodingKey, Validation, decode, decode_header};
use serde::Deserialize;

use crate::error::AppError;
use crate::state::AppState;

use super::jwt::{Claims, Scope, validate_token};

/// Extract the token from the Authorization header, or from a `token` query
/// parameter. The query form is the presigned-URL analog used by the
/// reconstruction response's fetch_info URLs: xet-core fetches those URLs
/// without attaching any Authorization header (on huggingface.co they are
/// presigned S3 URLs), so the URL itself must carry the credential.
fn extract_bearer_token(parts: &Parts) -> Result<&str, AppError> {
    if let Some(header) = parts.headers.get("authorization") {
        let header = header
            .to_str()
            .map_err(|_| AppError::Unauthorized("invalid authorization header".to_string()))?;
        return header.strip_prefix("Bearer ").ok_or_else(|| {
            AppError::Unauthorized("invalid authorization header format".to_string())
        });
    }

    parts
        .uri
        .query()
        .and_then(|q| q.split('&').find_map(|kv| kv.strip_prefix("token=")))
        .ok_or_else(|| AppError::Unauthorized("missing authorization header".to_string()))
}

/// Minimal claims decoded from an OIDC (e.g. Keycloak) token. Signature,
/// issuer, and expiry are enforced by [`Validation`]; we only need the standard
/// fields here. Keycloak tokens do not carry our custom `scope`/`repo` claims,
/// so mapping realm/client roles onto OpenXet scopes is deferred — see
/// [`oidc_claims_to_app_claims`].
#[derive(Deserialize)]
struct OidcClaims {
    exp: usize,
}

/// Map a verified OIDC token onto OpenXet [`Claims`]. Until role→scope mapping
/// is implemented, OIDC-authenticated callers are granted read-only access,
/// which is the safe default: a valid Keycloak token proves identity but not
/// yet write authorization.
// TODO(auth): derive scope/repo from Keycloak realm/client roles or a custom claim.
fn oidc_claims_to_app_claims(oidc: OidcClaims) -> Claims {
    Claims {
        scope: Scope::Read,
        repo: String::new(),
        exp: oidc.exp,
    }
}

/// Read the unverified `iss` claim so we can pick the JWKS bucket before we have
/// a key to verify with. This value is untrusted until [`Validation::set_issuer`]
/// re-checks it against the same allow-listed issuer during signature
/// verification below.
fn unverified_issuer(token: &str, alg: Algorithm) -> Result<String, AppError> {
    #[derive(Deserialize)]
    struct IssOnly {
        iss: String,
    }
    let mut validation = Validation::new(alg);
    validation.insecure_disable_signature_validation();
    validation.validate_exp = false;
    validation.validate_aud = false;
    validation.required_spec_claims.clear();
    let data = decode::<IssOnly>(token, &DecodingKey::from_secret(b""), &validation)
        .map_err(|e| AppError::Unauthorized(format!("invalid token: {e}")))?;
    Ok(data.claims.iss)
}

/// Verify a bearer token and return its OpenXet claims. HS256/HS384/HS512
/// tokens are checked against the configured shared secret (the self-minted
/// and presigned-URL path). Asymmetric tokens are verified against the issuing
/// OIDC provider's JWKS when OIDC is configured.
async fn verify_token(state: &AppState, token: &str) -> Result<Claims, AppError> {
    let header =
        decode_header(token).map_err(|e| AppError::Unauthorized(format!("invalid token: {e}")))?;

    match header.alg {
        Algorithm::HS256 | Algorithm::HS384 | Algorithm::HS512 => {
            validate_token(&state.config.auth.secret, token)
                .map_err(|e| AppError::Unauthorized(format!("invalid token: {e}")))
        }
        _ => {
            if !state.jwks.is_enabled() {
                return Err(AppError::Unauthorized(
                    "asymmetric tokens require OIDC to be configured".to_string(),
                ));
            }
            let kid = header
                .kid
                .ok_or_else(|| AppError::Unauthorized("token missing kid".to_string()))?;
            let issuer = unverified_issuer(token, header.alg)?;
            if !state.jwks.is_allowed_issuer(&issuer) {
                return Err(AppError::Unauthorized(format!(
                    "issuer not allowed: {issuer}"
                )));
            }

            let jwk = state
                .jwks
                .key_for(&issuer, &kid)
                .await
                .map_err(|e| AppError::Unauthorized(format!("key resolution failed: {e}")))?;
            let decoding_key = DecodingKey::from_jwk(&jwk)
                .map_err(|e| AppError::Unauthorized(format!("invalid signing key: {e}")))?;

            let mut validation = Validation::new(header.alg);
            validation.set_issuer(&[&issuer]);
            match &state.config.auth.oidc_audience {
                Some(aud) => validation.set_audience(&[aud]),
                None => validation.validate_aud = false,
            }

            let data = decode::<OidcClaims>(token, &decoding_key, &validation)
                .map_err(|e| AppError::Unauthorized(format!("invalid token: {e}")))?;
            Ok(oidc_claims_to_app_claims(data.claims))
        }
    }
}

/// Axum extractor that requires at least `read` scope.
pub struct RequireRead(pub Claims);

impl FromRequestParts<AppState> for RequireRead {
    type Rejection = AppError;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, AppError> {
        let token = extract_bearer_token(parts)?.to_string();
        let claims = verify_token(state, &token).await?;

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
        let token = extract_bearer_token(parts)?.to_string();
        let claims = verify_token(state, &token).await?;

        if !claims.scope.satisfies(Scope::Write) {
            return Err(AppError::Unauthorized(
                "insufficient scope: write required".to_string(),
            ));
        }

        Ok(RequireWrite(claims))
    }
}
