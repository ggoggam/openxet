//! Cursor-based (keyset) pagination for the management list APIs.
//!
//! All paginated resources are ordered by a stable string key (a content
//! hash), so a page is "the next `limit` rows whose key sorts after the
//! cursor". The cursor is the last-returned key, base64url-encoded so callers
//! treat it as opaque and we stay free to change what it encodes. Keyset
//! pagination (rather than offset/limit) means a page costs an indexed range
//! scan regardless of how far in it is, and concurrent inserts/deletes never
//! shift rows across page boundaries.

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::Serialize;

use crate::error::AppError;

/// Page size used when a request omits `limit`.
pub const DEFAULT_LIMIT: usize = 100;
/// Hard cap on page size, so one request can't ask for an unbounded scan.
pub const MAX_LIMIT: usize = 1000;

/// Clamp a client-supplied limit into `[1, MAX_LIMIT]`, defaulting when absent.
pub fn clamp_limit(limit: Option<usize>) -> usize {
    limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT)
}

/// Encode a row key as an opaque cursor.
pub fn encode_cursor(key: &str) -> String {
    URL_SAFE_NO_PAD.encode(key.as_bytes())
}

/// Decode a cursor back to the row key it points after. A cursor that is not
/// valid base64url of UTF-8 is a client error, not an empty page, so callers
/// notice a mangled token instead of silently restarting from the top.
pub fn decode_cursor(cursor: &str) -> Result<String, AppError> {
    let bytes = URL_SAFE_NO_PAD
        .decode(cursor)
        .map_err(|_| AppError::BadRequest("invalid cursor".to_string()))?;
    String::from_utf8(bytes).map_err(|_| AppError::BadRequest("invalid cursor".to_string()))
}

/// Decode an optional cursor into the exclusive lower-bound key for a scan.
pub fn cursor_after(cursor: Option<&str>) -> Result<Option<String>, AppError> {
    match cursor {
        Some(c) => Ok(Some(decode_cursor(c)?)),
        None => Ok(None),
    }
}

/// One page of results plus the cursor to fetch the next one.
#[derive(Debug, Serialize)]
pub struct Page<T> {
    pub items: Vec<T>,
    /// Cursor for the following page, or absent on the last page.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
}

impl<T> Page<T> {
    /// Build a page from rows fetched with a limit of `limit + 1`. The extra
    /// row, if present, is what tells us another page exists without a second
    /// count query; it is dropped from the response and the last *kept* row's
    /// key becomes `next_cursor`.
    pub fn from_overfetched(mut rows: Vec<T>, limit: usize, key_of: impl Fn(&T) -> String) -> Self {
        let has_more = rows.len() > limit;
        if has_more {
            rows.truncate(limit);
        }
        let next_cursor = has_more
            .then(|| rows.last().map(|row| encode_cursor(&key_of(row))))
            .flatten();
        Page {
            items: rows,
            next_cursor,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cursor_roundtrip() {
        let key = "a1b2c3";
        let encoded = encode_cursor(key);
        assert_ne!(encoded, key, "cursor should be encoded, not raw");
        assert_eq!(decode_cursor(&encoded).unwrap(), key);
    }

    #[test]
    fn bad_cursor_is_rejected() {
        assert!(decode_cursor("not valid base64!!!").is_err());
    }

    #[test]
    fn limit_is_clamped() {
        assert_eq!(clamp_limit(None), DEFAULT_LIMIT);
        assert_eq!(clamp_limit(Some(0)), 1);
        assert_eq!(clamp_limit(Some(5)), 5);
        assert_eq!(clamp_limit(Some(usize::MAX)), MAX_LIMIT);
    }

    #[test]
    fn page_reports_more_when_overfetched() {
        // Asked for 2, got 3: there is a next page keyed by the 2nd row.
        let page = Page::from_overfetched(vec!["a", "b", "c"], 2, |s| s.to_string());
        assert_eq!(page.items, vec!["a", "b"]);
        assert_eq!(page.next_cursor, Some(encode_cursor("b")));
    }

    #[test]
    fn page_is_last_when_not_overfetched() {
        let page = Page::from_overfetched(vec!["a", "b"], 2, |s| s.to_string());
        assert_eq!(page.items, vec!["a", "b"]);
        assert_eq!(page.next_cursor, None);
    }
}
