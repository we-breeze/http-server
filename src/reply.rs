use http::header::{HeaderMap, HeaderName, HeaderValue};
use serde::Serialize;

use crate::{EphemeralBytesArena, HeaderBlockError, Response, StatusCode};

/// Response conversion categories inferred by the API adapter.
#[doc(hidden)]
pub mod kind {
    pub struct Json;
    pub struct Direct;
    pub struct Stream;
    pub struct Status<T>(pub std::marker::PhantomData<T>);
    pub struct Headers<T>(pub std::marker::PhantomData<T>);
}

/// Converts a business result into its final HTTP response.
/// The conversion category is inferred; business methods return their own types.
pub trait IntoHttpResponse<Kind = kind::Json> {
    fn into_http_response(self, arena: &EphemeralBytesArena) -> Response;
}

impl<T: Serialize> IntoHttpResponse for T {
    fn into_http_response(self, arena: &EphemeralBytesArena) -> Response {
        crate::json::json_response(self, arena)
    }
}

impl IntoHttpResponse<kind::Direct> for Response {
    fn into_http_response(self, _arena: &EphemeralBytesArena) -> Response {
        self
    }
}

impl IntoHttpResponse<kind::Direct> for StatusCode {
    fn into_http_response(self, _arena: &EphemeralBytesArena) -> Response {
        Response::empty(self)
    }
}

impl<T, K> IntoHttpResponse<kind::Status<K>> for (StatusCode, T)
where
    T: IntoHttpResponse<K>,
{
    fn into_http_response(self, arena: &EphemeralBytesArena) -> Response {
        self.1.into_http_response(arena).with_status(self.0)
    }
}

/// Adds HTTP metadata to a typed business value or binary response.
#[derive(Debug)]
pub struct HttpResponse<T> {
    body: T,
    status: Option<StatusCode>,
    headers: HeaderMap,
    content_length: Option<u64>,
}

impl<T> HttpResponse<T> {
    #[must_use]
    pub fn new(body: T) -> Self {
        Self {
            body,
            status: None,
            headers: HeaderMap::new(),
            content_length: None,
        }
    }

    #[must_use]
    pub fn status(mut self, status: StatusCode) -> Self {
        self.status = Some(status);
        self
    }

    /// Declares an upstream stream's known byte length. The runtime verifies it.
    #[must_use]
    pub fn content_length(mut self, length: u64) -> Self {
        self.content_length = Some(length);
        self
    }

    /// Appends a header, preserving repeated values such as `Set-Cookie`.
    ///
    /// # Errors
    /// Returns an error for invalid or runtime-owned headers.
    pub fn header(mut self, name: &str, value: &str) -> Result<Self, HeaderBlockError> {
        let name =
            HeaderName::from_bytes(name.as_bytes()).map_err(|_| HeaderBlockError::InvalidName)?;
        if matches!(
            name.as_str(),
            "content-length" | "transfer-encoding" | "connection"
        ) {
            return Err(HeaderBlockError::ReservedName);
        }
        let value = HeaderValue::from_str(value).map_err(|_| HeaderBlockError::InvalidValue)?;
        self.headers.append(name, value);
        Ok(self)
    }
}

impl<T: IntoHttpResponse<K>, K> IntoHttpResponse<kind::Headers<K>> for HttpResponse<T> {
    fn into_http_response(self, arena: &EphemeralBytesArena) -> Response {
        let mut response = self.body.into_http_response(arena);
        if let Some(status) = self.status {
            response = response.with_status(status);
        }
        if let Some(length) = self.content_length {
            response = response.set_content_length(length);
        }
        response.with_http_headers(&self.headers, arena)
    }
}

/// A complete binary body. Moving a `Vec<u8>` into it does not copy the bytes.
#[derive(Debug)]
pub struct Binary(pub bytes::Bytes);

impl Binary {
    #[must_use]
    pub fn new(body: impl Into<bytes::Bytes>) -> Self {
        Self(body.into())
    }
}

impl IntoHttpResponse<kind::Direct> for Binary {
    fn into_http_response(self, _arena: &EphemeralBytesArena) -> Response {
        Response::owned_bytes(StatusCode::OK, self.0).content_type("application/octet-stream")
    }
}

/// An owned UTF-8 text response.
#[derive(Debug)]
pub struct Text(pub String);

impl IntoHttpResponse<kind::Direct> for Text {
    fn into_http_response(self, _arena: &EphemeralBytesArena) -> Response {
        Response::owned_bytes(StatusCode::OK, self.0).content_type("text/plain; charset=utf-8")
    }
}

/// An owned HTML response.
#[derive(Debug)]
pub struct Html(pub String);

impl IntoHttpResponse<kind::Direct> for Html {
    fn into_http_response(self, _arena: &EphemeralBytesArena) -> Response {
        Response::owned_bytes(StatusCode::OK, self.0).content_type("text/html; charset=utf-8")
    }
}

/// A redirect with a validated `Location` response header.
#[derive(Debug)]
pub struct Redirect {
    location: String,
    status: StatusCode,
}

impl Redirect {
    #[must_use]
    pub fn found(location: impl Into<String>) -> Self {
        Self {
            location: location.into(),
            status: StatusCode::FOUND,
        }
    }

    #[must_use]
    pub fn temporary(location: impl Into<String>) -> Self {
        Self {
            location: location.into(),
            status: StatusCode::TEMPORARY_REDIRECT,
        }
    }
}

impl IntoHttpResponse<kind::Direct> for Redirect {
    fn into_http_response(self, arena: &EphemeralBytesArena) -> Response {
        HttpResponse::new(Response::empty(self.status))
            .header("location", &encode_location(&self.location))
            .map_or_else(
                |_| Response::conversion_failure(),
                |reply| reply.into_http_response(arena),
            )
    }
}

#[doc(hidden)]
pub fn response<T: IntoHttpResponse<K>, K>(value: T, arena: &EphemeralBytesArena) -> Response {
    value.into_http_response(arena)
}

#[doc(hidden)]
pub fn result_response<T: IntoHttpResponse<K>, E: crate::IntoHttpError, K>(
    value: Result<T, E>,
    arena: &EphemeralBytesArena,
) -> Response {
    match value {
        Ok(value) => value.into_http_response(arena),
        Err(error) => error.into_http_error(arena),
    }
}

fn encode_location(location: &str) -> String {
    // Preserve URL delimiters and existing escapes, matching RedirectResponse.
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut encoded = String::with_capacity(location.len());
    for byte in location.bytes() {
        if byte.is_ascii_alphanumeric() || b"-._~:/%#?=@[]!$&'()*+,;".contains(&byte) {
            encoded.push(char::from(byte));
        } else {
            encoded.push('%');
            encoded.push(char::from(HEX[usize::from(byte >> 4)]));
            encoded.push(char::from(HEX[usize::from(byte & 15)]));
        }
    }
    encoded
}
