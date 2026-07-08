use axum::extract::FromRequestParts;
use axum::http::request::Parts;
use jsonwebtoken::{Algorithm, DecodingKey, Validation, decode, decode_header};
use serde::Deserialize;

use crate::error::AppError;
use crate::state::AppState;

use super::jwt::{Claims, Scope, validate_token};

/// Extract the bearer token from the `Authorization` header.
///
/// Fetch URLs in reconstruction responses are self-authenticating (presigned
/// object-store URLs, or the unauthenticated filesystem fallback route), so the
/// server no longer accepts a credential via a `?token=` query parameter.
fn extract_bearer_token(parts: &Parts) -> Result<&str, AppError> {
    let header = parts
        .headers
        .get("authorization")
        .ok_or_else(|| AppError::Unauthorized("missing authorization header".to_string()))?;
    let header = header
        .to_str()
        .map_err(|_| AppError::Unauthorized("invalid authorization header".to_string()))?;
    header
        .strip_prefix("Bearer ")
        .ok_or_else(|| AppError::Unauthorized("invalid authorization header format".to_string()))
}

/// Minimal claims decoded from an OIDC (e.g. Keycloak) token. Signature,
/// issuer, and expiry are enforced by [`Validation`]; we only need the standard
/// fields here. Keycloak tokens do not carry our custom `scope`/`repo` claims,
/// so mapping realm/client roles onto OpenXet scopes is deferred — see
/// [`oidc_claims_to_app_claims`].
#[derive(Deserialize)]
struct OidcClaims {
    exp: usize,
    /// Subject identity; becomes the accounting owner for uploads.
    #[serde(default)]
    sub: String,
}

/// Map a verified OIDC token onto OpenXet [`Claims`]. Any valid token from an
/// allowed issuer is granted full read/write access to all repos: issuance is
/// the deployment's access-control point (e.g. who Keycloak gives tokens to).
// ponytail: no role→scope mapping; add a scope/role claim check when a
// deployment needs read-only or per-repo grants.
fn oidc_claims_to_app_claims(oidc: OidcClaims) -> Claims {
    Claims {
        scope: Scope::Write,
        repo: String::new(),
        exp: oidc.exp,
        sub: oidc.sub,
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

/// Verify a bearer token and return its OpenXet claims. HS256 tokens are
/// checked against the server's symmetric secret (used by tests and
/// trusted/dev setups; clients never hold this secret). Asymmetric tokens are
/// verified against the issuing OIDC provider's JWKS.
async fn verify_token(state: &AppState, token: &str) -> Result<Claims, AppError> {
    let header =
        decode_header(token).map_err(|e| AppError::Unauthorized(format!("invalid token: {e}")))?;

    match header.alg {
        Algorithm::HS256 => validate_token(&state.fetch_token_secret, token)
            .map_err(|e| AppError::Unauthorized(format!("invalid token: {e}"))),
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

/// Authenticate the request and check it has the required scope. When auth is
/// disabled in config, every request passes with write scope.
async fn authorize(parts: &Parts, state: &AppState, required: Scope) -> Result<Claims, AppError> {
    if !state.config.auth.enabled {
        return Ok(Claims {
            scope: Scope::Write,
            repo: String::new(),
            exp: usize::MAX,
            sub: String::new(),
        });
    }

    let token = extract_bearer_token(parts)?.to_string();
    let claims = verify_token(state, &token).await?;

    // HF xet spec: valid token with insufficient scope is 403, not 401.
    if !claims.scope.satisfies(required) {
        return Err(AppError::Forbidden(format!(
            "insufficient scope: {} required",
            match required {
                Scope::Read => "read",
                Scope::Write => "write",
            }
        )));
    }
    Ok(claims)
}

/// Axum extractor that requires at least `read` scope.
pub struct RequireRead(pub Claims);

impl FromRequestParts<AppState> for RequireRead {
    type Rejection = AppError;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, AppError> {
        Ok(RequireRead(authorize(parts, state, Scope::Read).await?))
    }
}

/// Axum extractor that requires `write` scope.
pub struct RequireWrite(pub Claims);

impl FromRequestParts<AppState> for RequireWrite {
    type Rejection = AppError;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, AppError> {
        Ok(RequireWrite(authorize(parts, state, Scope::Write).await?))
    }
}
