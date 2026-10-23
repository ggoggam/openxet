use axum::extract::FromRequestParts;
use axum::http::HeaderMap;
use axum::http::request::Parts;
use jsonwebtoken::{Algorithm, DecodingKey, Validation, decode, decode_header};
use serde::Deserialize;

use crate::config::AuthConfig;
use crate::error::AppError;
use crate::state::AppState;

use super::jwt::{Claims, Scope, validate_token};

/// Extract the bearer token from the `Authorization` header.
///
/// Fetch URLs in reconstruction responses are self-authenticating (presigned
/// object-store URLs, or HMAC-signed URLs to the filesystem fallback route), so
/// the server does not accept a bearer credential via a `?token=` query
/// parameter.
fn extract_bearer_token(headers: &HeaderMap) -> Result<&str, AppError> {
    let header = headers
        .get("authorization")
        .ok_or_else(|| AppError::Unauthorized("missing authorization header".to_string()))?;
    let header = header
        .to_str()
        .map_err(|_| AppError::Unauthorized("invalid authorization header".to_string()))?;
    header
        .strip_prefix("Bearer ")
        .ok_or_else(|| AppError::Unauthorized("invalid authorization header format".to_string()))
}

/// Claims decoded from an OIDC (e.g. Keycloak) token. Signature, issuer, and
/// expiry are enforced by [`Validation`]. The grant claims are kept as raw JSON
/// because providers disagree on their shape (a space-separated `scope`
/// string, a `roles` array, Keycloak's `realm_access` / `resource_access`).
#[derive(Deserialize)]
struct OidcClaims {
    exp: usize,
    /// Subject identity; becomes the accounting owner for uploads.
    #[serde(default)]
    sub: String,
    #[serde(default)]
    scope: serde_json::Value,
    #[serde(default)]
    roles: serde_json::Value,
    #[serde(default)]
    realm_access: serde_json::Value,
    #[serde(default)]
    resource_access: serde_json::Value,
}

/// Strings from a claim that is either a whitespace-separated string or an
/// array of strings.
fn claim_strings(value: &serde_json::Value) -> Vec<&str> {
    match value {
        serde_json::Value::String(s) => s.split_whitespace().collect(),
        serde_json::Value::Array(items) => items.iter().filter_map(|v| v.as_str()).collect(),
        _ => Vec::new(),
    }
}

impl OidcClaims {
    /// Every scope and role the token carries: OAuth `scope`, a top-level
    /// `roles` claim, Keycloak realm roles, and Keycloak client roles of any
    /// client.
    fn grants(&self) -> Vec<&str> {
        let mut grants = claim_strings(&self.scope);
        grants.extend(claim_strings(&self.roles));
        grants.extend(claim_strings(&self.realm_access["roles"]));
        if let Some(clients) = self.resource_access.as_object() {
            for client in clients.values() {
                grants.extend(claim_strings(&client["roles"]));
            }
        }
        grants
    }
}

/// Map a verified OIDC token onto OpenXet [`Claims`]. The token's scopes and
/// roles are matched against the configured `auth.oidc_{read,write,admin}_role`
/// names and the strongest match wins. A token with none of them falls back to
/// `auth.oidc_default_scope`; when that is unset too, the token is valid but
/// grants nothing, which is a 403.
fn oidc_claims_to_app_claims(oidc: OidcClaims, auth: &AuthConfig) -> Result<Claims, AppError> {
    let grants = oidc.grants();
    let has = |role: &str| !role.is_empty() && grants.contains(&role);
    let scope = if has(&auth.oidc_admin_role) {
        Some(Scope::Admin)
    } else if has(&auth.oidc_write_role) {
        Some(Scope::Write)
    } else if has(&auth.oidc_read_role) {
        Some(Scope::Read)
    } else {
        auth.oidc_default_scope
    };
    let scope = scope.ok_or_else(|| {
        AppError::Forbidden(format!(
            "token carries none of the OpenXet roles ({}, {}, {})",
            auth.oidc_read_role, auth.oidc_write_role, auth.oidc_admin_role
        ))
    })?;
    Ok(Claims {
        scope,
        repo: String::new(),
        exp: oidc.exp,
        sub: oidc.sub,
    })
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
            oidc_claims_to_app_claims(data.claims, &state.config.auth)
        }
    }
}

/// Authenticate the request and check it has the required scope. When auth is
/// disabled in config, every request passes with admin scope.
pub(crate) async fn authorize(
    headers: &HeaderMap,
    state: &AppState,
    required: Scope,
) -> Result<Claims, AppError> {
    if !state.config.auth.enabled {
        return Ok(Claims {
            scope: Scope::Admin,
            repo: String::new(),
            exp: usize::MAX,
            sub: String::new(),
        });
    }

    let token = extract_bearer_token(headers)?.to_string();
    let claims = verify_token(state, &token).await?;

    // HF xet spec: valid token with insufficient scope is 403, not 401.
    if !claims.scope.satisfies(required) {
        return Err(AppError::Forbidden(format!(
            "insufficient scope: {} required",
            required.as_str()
        )));
    }
    Ok(claims)
}

/// Axum extractor that requires at least `read` scope.
pub struct RequireRead(pub Claims);

impl FromRequestParts<AppState> for RequireRead {
    type Rejection = AppError;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, AppError> {
        Ok(RequireRead(
            authorize(&parts.headers, state, Scope::Read).await?,
        ))
    }
}

/// Axum extractor that requires `admin` scope.
pub struct RequireAdmin(pub Claims);

impl FromRequestParts<AppState> for RequireAdmin {
    type Rejection = AppError;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, AppError> {
        Ok(RequireAdmin(
            authorize(&parts.headers, state, Scope::Admin).await?,
        ))
    }
}

/// Axum extractor that requires `write` scope.
pub struct RequireWrite(pub Claims);

impl FromRequestParts<AppState> for RequireWrite {
    type Rejection = AppError;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, AppError> {
        Ok(RequireWrite(
            authorize(&parts.headers, state, Scope::Write).await?,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn oidc(extra: serde_json::Value) -> OidcClaims {
        let mut claims = serde_json::json!({ "exp": 1, "sub": "alice" });
        claims
            .as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        serde_json::from_value(claims).unwrap()
    }

    fn scope_of(extra: serde_json::Value, auth: &AuthConfig) -> Result<Scope, AppError> {
        oidc_claims_to_app_claims(oidc(extra), auth).map(|c| c.scope)
    }

    #[test]
    fn token_without_roles_is_forbidden_by_default() {
        let auth = AuthConfig::default();
        let err = scope_of(serde_json::json!({}), &auth).unwrap_err();
        assert!(matches!(err, AppError::Forbidden(_)));
        let err = scope_of(serde_json::json!({ "scope": "openid profile" }), &auth).unwrap_err();
        assert!(matches!(err, AppError::Forbidden(_)));
    }

    #[test]
    fn default_scope_applies_only_without_roles() {
        let auth = AuthConfig {
            oidc_default_scope: Some(Scope::Write),
            ..Default::default()
        };
        assert_eq!(
            scope_of(serde_json::json!({}), &auth).unwrap(),
            Scope::Write
        );
        assert_eq!(
            scope_of(serde_json::json!({ "scope": "openxet:read" }), &auth).unwrap(),
            Scope::Read
        );
    }

    #[test]
    fn roles_are_read_from_every_supported_claim() {
        let auth = AuthConfig::default();
        assert_eq!(
            scope_of(serde_json::json!({ "scope": "openid openxet:read" }), &auth).unwrap(),
            Scope::Read
        );
        assert_eq!(
            scope_of(serde_json::json!({ "scope": ["openxet:write"] }), &auth).unwrap(),
            Scope::Write
        );
        assert_eq!(
            scope_of(serde_json::json!({ "roles": ["openxet:write"] }), &auth).unwrap(),
            Scope::Write
        );
        assert_eq!(
            scope_of(
                serde_json::json!({ "realm_access": { "roles": ["openxet:admin"] } }),
                &auth
            )
            .unwrap(),
            Scope::Admin
        );
        assert_eq!(
            scope_of(
                serde_json::json!({ "resource_access": { "cas": { "roles": ["openxet:write"] } } }),
                &auth
            )
            .unwrap(),
            Scope::Write
        );
    }

    #[test]
    fn strongest_role_wins() {
        let auth = AuthConfig::default();
        let claims = serde_json::json!({
            "scope": "openxet:read",
            "realm_access": { "roles": ["openxet:write", "openxet:admin"] }
        });
        assert_eq!(scope_of(claims, &auth).unwrap(), Scope::Admin);
    }

    #[test]
    fn role_names_are_configurable() {
        let auth = AuthConfig {
            oidc_write_role: "cas-uploader".to_string(),
            ..Default::default()
        };
        assert_eq!(
            scope_of(serde_json::json!({ "roles": ["cas-uploader"] }), &auth).unwrap(),
            Scope::Write
        );
        assert!(scope_of(serde_json::json!({ "roles": ["openxet:write"] }), &auth).is_err());
    }

    #[test]
    fn empty_role_name_never_matches() {
        let auth = AuthConfig {
            oidc_admin_role: String::new(),
            ..Default::default()
        };
        assert!(scope_of(serde_json::json!({ "roles": [""] }), &auth).is_err());
    }
}
