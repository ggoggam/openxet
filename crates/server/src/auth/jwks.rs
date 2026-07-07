use std::collections::HashMap;
use std::time::{Duration, Instant};

use jsonwebtoken::jwk::{Jwk, JwkSet};
use serde::Deserialize;
use tokio::sync::RwLock;

/// Lower bound on how often a single issuer's JWKS may be refetched in response
/// to an unknown `kid`. Without this, a client presenting tokens with random
/// `kid` headers could force one network fetch per request. Keycloak key
/// rotation is infrequent, so a short throttle is harmless.
const MIN_REFRESH_INTERVAL: Duration = Duration::from_secs(60);

#[derive(Debug, thiserror::Error)]
pub enum JwksError {
    #[error("issuer not allowed: {0}")]
    IssuerNotAllowed(String),
    #[error("unknown signing key (kid={0})")]
    UnknownKid(String),
    #[error("failed to fetch JWKS for {issuer}: {source}")]
    Fetch {
        issuer: String,
        #[source]
        source: reqwest::Error,
    },
}

struct CacheEntry {
    jwks: JwkSet,
    fetched_at: Instant,
}

#[derive(Deserialize)]
struct OidcDiscovery {
    jwks_uri: String,
}

/// Per-issuer TTL cache of JWKS documents. Verifying an OIDC (e.g. Keycloak)
/// token requires the issuer's public signing keys, which are exposed at the
/// issuer's JWKS endpoint. Fetching them on every request would be prohibitively
/// slow, so keys are cached for `ttl` and refreshed on expiry — or eagerly when
/// a token references a `kid` we have not seen yet (key rotation).
pub struct JwksCache {
    allowed_issuers: Vec<String>,
    ttl: Duration,
    http: reqwest::Client,
    entries: RwLock<HashMap<String, CacheEntry>>,
}

impl JwksCache {
    pub fn new(allowed_issuers: Vec<String>, ttl: Duration) -> Self {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .expect("failed to build reqwest client");
        Self {
            allowed_issuers,
            ttl,
            http,
            entries: RwLock::new(HashMap::new()),
        }
    }

    /// Whether any OIDC issuer is configured. When false, the JWKS path is off
    /// and only the server's own self-minted fetch-URL tokens (HS256) are accepted.
    pub fn is_enabled(&self) -> bool {
        !self.allowed_issuers.is_empty()
    }

    pub fn is_allowed_issuer(&self, issuer: &str) -> bool {
        self.allowed_issuers.iter().any(|i| i == issuer)
    }

    /// Resolve the signing key for `(issuer, kid)`, fetching or refreshing the
    /// issuer's JWKS as needed. The issuer must be in the allow-list.
    pub async fn key_for(&self, issuer: &str, kid: &str) -> Result<Jwk, JwksError> {
        if !self.is_allowed_issuer(issuer) {
            return Err(JwksError::IssuerNotAllowed(issuer.to_string()));
        }

        // Fast path: a cache entry that is fresh and already contains this kid.
        let need_refresh = {
            let entries = self.entries.read().await;
            match entries.get(issuer) {
                Some(entry) if entry.fetched_at.elapsed() < self.ttl => {
                    if let Some(jwk) = entry.jwks.find(kid) {
                        return Ok(jwk.clone());
                    }
                    // Fresh but the kid is unknown (possible rotation). Refresh,
                    // but only if we have not just fetched — throttles kid-miss
                    // storms.
                    entry.fetched_at.elapsed() >= MIN_REFRESH_INTERVAL
                }
                // Stale entry, or nothing cached yet.
                _ => true,
            }
        };

        if !need_refresh {
            return Err(JwksError::UnknownKid(kid.to_string()));
        }

        // Fetch outside the lock so concurrent verifications are not blocked.
        let jwks = self.fetch(issuer).await?;
        let found = jwks.find(kid).cloned();
        {
            let mut entries = self.entries.write().await;
            entries.insert(
                issuer.to_string(),
                CacheEntry {
                    jwks,
                    fetched_at: Instant::now(),
                },
            );
        }
        found.ok_or_else(|| JwksError::UnknownKid(kid.to_string()))
    }

    /// Fetch the issuer's JWKS via OIDC discovery
    /// (`{issuer}/.well-known/openid-configuration` → `jwks_uri`).
    async fn fetch(&self, issuer: &str) -> Result<JwkSet, JwksError> {
        let base = issuer.trim_end_matches('/');
        let discovery_url = format!("{base}/.well-known/openid-configuration");

        let map_err = |source: reqwest::Error| JwksError::Fetch {
            issuer: issuer.to_string(),
            source,
        };

        let discovery: OidcDiscovery = self
            .http
            .get(&discovery_url)
            .send()
            .await
            .and_then(|r| r.error_for_status())
            .map_err(map_err)?
            .json()
            .await
            .map_err(map_err)?;

        let jwks: JwkSet = self
            .http
            .get(&discovery.jwks_uri)
            .send()
            .await
            .and_then(|r| r.error_for_status())
            .map_err(map_err)?
            .json()
            .await
            .map_err(map_err)?;

        Ok(jwks)
    }
}
