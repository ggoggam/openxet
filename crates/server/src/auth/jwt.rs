use jsonwebtoken::{DecodingKey, EncodingKey, Header, Validation, decode, encode};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Scope {
    Read,
    Write,
}

impl Scope {
    /// Write scope supersedes read.
    pub fn satisfies(self, required: Scope) -> bool {
        match required {
            Scope::Read => true,
            Scope::Write => self == Scope::Write,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Claims {
    pub scope: Scope,
    pub repo: String,
    pub exp: usize,
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
}
