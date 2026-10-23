use jsonwebtoken::{DecodingKey, EncodingKey, Header, Validation, decode, encode};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Scope {
    Read,
    Write,
    /// Operator scope: everything `write` can do plus server-wide management
    /// (GC, cross-tenant reads).
    Admin,
}

impl Scope {
    /// Scopes are ordered: admin supersedes write, which supersedes read.
    pub fn satisfies(self, required: Scope) -> bool {
        self.rank() >= required.rank()
    }

    fn rank(self) -> u8 {
        match self {
            Scope::Read => 0,
            Scope::Write => 1,
            Scope::Admin => 2,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Scope::Read => "read",
            Scope::Write => "write",
            Scope::Admin => "admin",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Claims {
    pub scope: Scope,
    pub repo: String,
    pub exp: usize,
    /// Stable subject identity of the caller (JWT `sub`). Used as the
    /// accounting owner for uploads and deletions. Defaults to empty for
    /// tokens minted before this claim existed.
    #[serde(default)]
    pub sub: String,
}

impl Claims {
    /// The accounting owner for this caller: the token's `sub`, or `"default"`
    /// when the token carries no subject (or auth is disabled).
    pub fn owner(&self) -> &str {
        if self.sub.is_empty() {
            "default"
        } else {
            &self.sub
        }
    }
}

/// Create a signed JWT from the given claims.
pub fn create_token(secret: &str, claims: &Claims) -> Result<String, jsonwebtoken::errors::Error> {
    encode(
        &Header::default(),
        claims,
        &EncodingKey::from_secret(secret.as_bytes()),
    )
}

/// Validate and decode a JWT, returning the claims.
pub fn validate_token(secret: &str, token: &str) -> Result<Claims, jsonwebtoken::errors::Error> {
    let data = decode::<Claims>(
        token,
        &DecodingKey::from_secret(secret.as_bytes()),
        &Validation::default(),
    )?;
    Ok(data.claims)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_write_scope_supersedes_read() {
        assert!(Scope::Write.satisfies(Scope::Read));
        assert!(Scope::Write.satisfies(Scope::Write));
        assert!(Scope::Read.satisfies(Scope::Read));
        assert!(!Scope::Read.satisfies(Scope::Write));
    }

    #[test]
    fn test_admin_scope_supersedes_write() {
        assert!(Scope::Admin.satisfies(Scope::Read));
        assert!(Scope::Admin.satisfies(Scope::Write));
        assert!(Scope::Admin.satisfies(Scope::Admin));
        assert!(!Scope::Write.satisfies(Scope::Admin));
        assert!(!Scope::Read.satisfies(Scope::Admin));
    }
}
