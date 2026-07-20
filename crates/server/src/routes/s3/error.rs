//! S3-flavored error responses. S3 clients branch on the `<Code>` in an XML
//! error body (e.g. `aws s3` reports `NoSuchKey` differently from a generic
//! 404), so the gateway must emit that shape rather than the JSON [`AppError`]
//! bodies the rest of the server returns.

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};

use crate::error::AppError;

/// An S3 API error: an HTTP status plus the S3 error `Code` clients match on.
#[derive(Debug)]
pub struct S3Error {
    pub status: StatusCode,
    pub code: &'static str,
    pub message: String,
    /// The object key or bucket the error concerns, echoed in `<Resource>`.
    pub resource: String,
}

impl S3Error {
    pub fn new(status: StatusCode, code: &'static str, message: impl Into<String>) -> Self {
        Self {
            status,
            code,
            message: message.into(),
            resource: String::new(),
        }
    }

    pub fn with_resource(mut self, resource: impl Into<String>) -> Self {
        self.resource = resource.into();
        self
    }

    pub fn no_such_key(key: &str) -> Self {
        Self::new(
            StatusCode::NOT_FOUND,
            "NoSuchKey",
            "The specified key does not exist.",
        )
        .with_resource(key)
    }

    pub fn access_denied(message: impl Into<String>) -> Self {
        Self::new(StatusCode::FORBIDDEN, "AccessDenied", message)
    }

    pub fn invalid_range() -> Self {
        Self::new(
            StatusCode::RANGE_NOT_SATISFIABLE,
            "InvalidRange",
            "The requested range is not satisfiable.",
        )
    }

    pub fn internal(message: impl Into<String>) -> Self {
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, "InternalError", message)
    }
}

/// Map the server's internal [`AppError`] onto the closest S3 error code, so
/// reused internals (reconstruction, storage) surface as proper S3 errors.
/// `resource` is the key/bucket in play, for the `<Resource>` element.
pub(crate) fn from_app_error(err: AppError, resource: &str) -> S3Error {
    match err {
        AppError::NotFound(_) => S3Error::no_such_key(resource),
        AppError::RangeNotSatisfiable => S3Error::invalid_range(),
        AppError::Unauthorized(m) => S3Error::new(StatusCode::UNAUTHORIZED, "AccessDenied", m),
        AppError::Forbidden(m) => S3Error::access_denied(m),
        AppError::BadRequest(m) => S3Error::new(StatusCode::BAD_REQUEST, "InvalidRequest", m),
        // Internal/other: log the detail server-side, keep the client body opaque.
        other => {
            tracing::error!("s3 gateway internal error: {other}");
            S3Error::internal("We encountered an internal error. Please try again.")
        }
    }
}

impl IntoResponse for S3Error {
    fn into_response(self) -> Response {
        // Hand-built XML: the body is small and fully server-controlled, so a
        // serializer dependency would not earn its keep. Values here are error
        // codes/messages and an echoed resource path — XML-escape the dynamic
        // parts defensively.
        let body = format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
             <Error><Code>{}</Code><Message>{}</Message><Resource>{}</Resource></Error>",
            self.code,
            xml_escape(&self.message),
            xml_escape(&self.resource),
        );
        (
            self.status,
            [(axum::http::header::CONTENT_TYPE, "application/xml")],
            body,
        )
            .into_response()
    }
}

/// Minimal XML text escaping for element content.
pub(crate) fn xml_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            _ => out.push(c),
        }
    }
    out
}
