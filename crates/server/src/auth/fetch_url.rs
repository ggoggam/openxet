//! Signed, expiring xorb fetch URLs.
//!
//! xet-core fetches the URLs in a reconstruction response without an
//! `Authorization` header, so on backends that cannot presign (the local
//! filesystem) the server hands out its own `/v1/xorbs/default/{hash}` route
//! with an HMAC in the query string instead. The MAC covers the xorb hash, the
//! inclusive byte span the reconstruction granted, and an expiry, so a URL
//! only ever unlocks the bytes its reconstruction response described, and only
//! until it expires. Knowing a xorb hash alone no longer grants a read.

use std::time::{SystemTime, UNIX_EPOCH};

use hmac::{Hmac, Mac};
use serde::Deserialize;
use sha2::Sha256;

use crate::error::AppError;

type HmacSha256 = Hmac<Sha256>;

/// Query parameters carried by a signed xorb fetch URL.
#[derive(Debug, Default, Deserialize)]
pub struct XorbUrlParams {
    /// Signed inclusive byte span, `start-end`.
    pub r: Option<String>,
    /// Expiry, unix seconds.
    pub exp: Option<u64>,
    /// Hex HMAC-SHA256 over the hash, span, and expiry.
    pub sig: Option<String>,
}

impl XorbUrlParams {
    /// Whether the request carries any signing parameter, i.e. the caller is
    /// presenting a signed URL rather than relying on a bearer token.
    pub fn is_signed(&self) -> bool {
        self.r.is_some() || self.exp.is_some() || self.sig.is_some()
    }
}

pub(crate) fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn mac(key: &[u8], hash: &str, start: u64, end: u64, exp: u64) -> HmacSha256 {
    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC accepts any key length");
    // Every field is hex or decimal, so `|` cannot appear inside one and the
    // encoding is unambiguous.
    mac.update(format!("openxet-xorb-fetch-v1|{hash}|{start}|{end}|{exp}").as_bytes());
    mac
}

/// Build a signed fetch URL for bytes `start..=end` of xorb `hash`.
pub fn sign_xorb_url(
    key: &[u8],
    base_url: &str,
    hash: &str,
    start: u64,
    end: u64,
    exp: u64,
) -> String {
    let sig = hex::encode(mac(key, hash, start, end, exp).finalize().into_bytes());
    format!("{base_url}/v1/xorbs/default/{hash}?r={start}-{end}&exp={exp}&sig={sig}")
}

/// Verify a signed fetch URL for xorb `hash` and return the inclusive byte
/// span it grants. Any malformed, tampered, or expired URL is a 401.
pub fn verify_xorb_url(
    key: &[u8],
    hash: &str,
    params: &XorbUrlParams,
    now: u64,
) -> Result<(u64, u64), AppError> {
    let invalid = || AppError::Unauthorized("invalid or expired xorb fetch URL".to_string());

    let (r, exp, sig) = match (&params.r, params.exp, &params.sig) {
        (Some(r), Some(exp), Some(sig)) => (r, exp, sig),
        _ => return Err(invalid()),
    };
    let (start, end) = r
        .split_once('-')
        .and_then(|(s, e)| Some((s.parse::<u64>().ok()?, e.parse::<u64>().ok()?)))
        .ok_or_else(invalid)?;
    let sig = hex::decode(sig).map_err(|_| invalid())?;

    // Constant-time comparison.
    mac(key, hash, start, end, exp)
        .verify_slice(&sig)
        .map_err(|_| invalid())?;

    if now > exp {
        return Err(invalid());
    }
    Ok((start, end))
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: &[u8] = b"test-key";
    const HASH: &str = "a1b2c3d4e5f60708091011121314151617181920212223242526272829303132";

    fn params_of(url: &str) -> XorbUrlParams {
        let query = url.split_once('?').unwrap().1;
        let mut p = XorbUrlParams::default();
        for kv in query.split('&') {
            let (k, v) = kv.split_once('=').unwrap();
            match k {
                "r" => p.r = Some(v.to_string()),
                "exp" => p.exp = Some(v.parse().unwrap()),
                "sig" => p.sig = Some(v.to_string()),
                _ => {}
            }
        }
        p
    }

    #[test]
    fn roundtrip() {
        let url = sign_xorb_url(KEY, "http://h", HASH, 10, 99, 1_000);
        assert!(url.starts_with(&format!(
            "http://h/v1/xorbs/default/{HASH}?r=10-99&exp=1000&sig="
        )));
        assert_eq!(
            verify_xorb_url(KEY, HASH, &params_of(&url), 999).unwrap(),
            (10, 99)
        );
        assert_eq!(
            verify_xorb_url(KEY, HASH, &params_of(&url), 1_000).unwrap(),
            (10, 99)
        );
    }

    #[test]
    fn rejects_expired() {
        let url = sign_xorb_url(KEY, "http://h", HASH, 0, 9, 1_000);
        assert!(verify_xorb_url(KEY, HASH, &params_of(&url), 1_001).is_err());
    }

    #[test]
    fn rejects_tampering() {
        let url = sign_xorb_url(KEY, "http://h", HASH, 0, 9, 1_000);
        let good = params_of(&url);

        let mut widened = params_of(&url);
        widened.r = Some("0-1000000".to_string());
        assert!(verify_xorb_url(KEY, HASH, &widened, 0).is_err());

        let mut extended = params_of(&url);
        extended.exp = Some(u64::MAX);
        assert!(verify_xorb_url(KEY, HASH, &extended, 0).is_err());

        let other_hash = "0".repeat(64);
        assert!(verify_xorb_url(KEY, &other_hash, &good, 0).is_err());

        assert!(verify_xorb_url(b"other-key", HASH, &good, 0).is_err());

        let mut flipped = params_of(&url);
        let mut sig = flipped.sig.take().unwrap().into_bytes();
        sig[0] = if sig[0] == b'0' { b'1' } else { b'0' };
        flipped.sig = Some(String::from_utf8(sig).unwrap());
        assert!(verify_xorb_url(KEY, HASH, &flipped, 0).is_err());
    }

    #[test]
    fn rejects_missing_or_malformed_params() {
        assert!(verify_xorb_url(KEY, HASH, &XorbUrlParams::default(), 0).is_err());
        let url = sign_xorb_url(KEY, "http://h", HASH, 0, 9, 1_000);
        let mut p = params_of(&url);
        p.r = Some("nonsense".to_string());
        assert!(verify_xorb_url(KEY, HASH, &p, 0).is_err());
        let mut p = params_of(&url);
        p.sig = Some("zz".to_string());
        assert!(verify_xorb_url(KEY, HASH, &p, 0).is_err());
    }
}
