use std::fmt;

use crate::EphemeralBytes;

/// HTTP response status code.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct StatusCode(u16);

impl StatusCode {
    pub const CONTINUE: Self = Self(100);
    pub const OK: Self = Self(200);
    pub const NO_CONTENT: Self = Self(204);
    pub const BAD_REQUEST: Self = Self(400);
    pub const UNAUTHORIZED: Self = Self(401);
    pub const FORBIDDEN: Self = Self(403);
    pub const NOT_FOUND: Self = Self(404);
    pub const METHOD_NOT_ALLOWED: Self = Self(405);
    pub const REQUEST_TIMEOUT: Self = Self(408);
    pub const PAYLOAD_TOO_LARGE: Self = Self(413);
    pub const EXPECTATION_FAILED: Self = Self(417);
    pub const REQUEST_HEADER_FIELDS_TOO_LARGE: Self = Self(431);
    pub const INTERNAL_SERVER_ERROR: Self = Self(500);
    pub const SERVICE_UNAVAILABLE: Self = Self(503);
    pub const NOT_IMPLEMENTED: Self = Self(501);
    pub const HTTP_VERSION_NOT_SUPPORTED: Self = Self(505);

    /// Creates a status code. Values outside the three-digit HTTP range are
    /// rejected by [`Server`](crate::Server) as an internal server error.
    #[must_use]
    pub const fn new(value: u16) -> Self {
        Self(value)
    }

    #[must_use]
    pub const fn as_u16(self) -> u16 {
        self.0
    }

    pub(crate) const fn reason(self) -> &'static str {
        match self.0 {
            100 => "Continue",
            200 => "OK",
            201 => "Created",
            202 => "Accepted",
            204 => "No Content",
            400 => "Bad Request",
            401 => "Unauthorized",
            403 => "Forbidden",
            404 => "Not Found",
            405 => "Method Not Allowed",
            408 => "Request Timeout",
            409 => "Conflict",
            413 => "Payload Too Large",
            415 => "Unsupported Media Type",
            417 => "Expectation Failed",
            429 => "Too Many Requests",
            431 => "Request Header Fields Too Large",
            500 => "Internal Server Error",
            501 => "Not Implemented",
            502 => "Bad Gateway",
            503 => "Service Unavailable",
            504 => "Gateway Timeout",
            505 => "HTTP Version Not Supported",
            _ => "Unknown",
        }
    }

    pub(crate) const fn is_valid(self) -> bool {
        self.0 >= 100 && self.0 <= 599
    }
}

impl fmt::Display for StatusCode {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

/// An HTTP response body that can be written without a framework body copy.
#[derive(Debug)]
pub enum ResponseBody {
    Empty,
    Static(&'static [u8]),
    Arena(EphemeralBytes),
}

impl ResponseBody {
    #[must_use]
    pub fn len(&self) -> usize {
        match self {
            Self::Empty => 0,
            Self::Static(bytes) => bytes.len(),
            Self::Arena(bytes) => bytes.len(),
        }
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub(crate) fn as_slice(&self) -> &[u8] {
        match self {
            Self::Empty => &[],
            Self::Static(bytes) => bytes,
            Self::Arena(bytes) => bytes.as_ref(),
        }
    }
}

/// An owned, validated HTTP header block allocated by the caller.
///
/// Each line must end in `\r\n`. The block may contain arbitrary response
/// headers except `Connection`, `Content-Length`, and `Transfer-Encoding`,
/// which are owned by the server runtime. The block is sent without copying.
#[derive(Debug)]
pub struct HeaderBlock(EphemeralBytes);

impl HeaderBlock {
    /// Validates an already encoded header block without copying it.
    ///
    /// Use [`crate::Request::response_bytes`] to reserve the final header
    /// storage from the request's arena, append complete header lines, freeze
    /// it, and pass it here.
    ///
    /// # Errors
    ///
    /// Returns [`HeaderBlockError`] when a line is malformed or attempts to
    /// set a transport header owned by the server.
    pub fn new(bytes: EphemeralBytes) -> Result<Self, HeaderBlockError> {
        validate_header_block(bytes.as_ref())?;
        Ok(Self(bytes))
    }

    pub(crate) fn as_slice(&self) -> &[u8] {
        self.0.as_ref()
    }
}

/// Validation error for a user-provided [`HeaderBlock`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HeaderBlockError {
    MissingLineEnding,
    EmptyLine,
    InvalidName,
    InvalidValue,
    ReservedName,
}

impl fmt::Display for HeaderBlockError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self {
            Self::MissingLineEnding => "response header lines must end with CRLF",
            Self::EmptyLine => "response header block must not contain an empty line",
            Self::InvalidName => "response header name is invalid",
            Self::InvalidValue => "response header value contains a line break",
            Self::ReservedName => "response header is owned by the server runtime",
        };
        formatter.write_str(message)
    }
}

impl std::error::Error for HeaderBlockError {}

/// A complete HTTP response.
#[derive(Debug)]
pub struct Response {
    status: StatusCode,
    body: ResponseBody,
    content_type: Option<&'static str>,
    headers: Option<HeaderBlock>,
    allow: Option<&'static str>,
    www_authenticate: Option<&'static str>,
    close: bool,
}

impl Response {
    #[must_use]
    pub fn new(status: StatusCode, body: ResponseBody) -> Self {
        Self {
            status,
            body,
            content_type: None,
            headers: None,
            allow: None,
            www_authenticate: None,
            close: false,
        }
    }

    #[must_use]
    pub fn empty(status: StatusCode) -> Self {
        Self::new(status, ResponseBody::Empty)
    }

    #[must_use]
    pub fn static_bytes(status: StatusCode, body: &'static [u8]) -> Self {
        Self::new(status, ResponseBody::Static(body))
    }

    #[must_use]
    pub fn bytes(status: StatusCode, body: EphemeralBytes) -> Self {
        Self::new(status, ResponseBody::Arena(body))
    }

    #[must_use]
    pub fn ok(body: EphemeralBytes) -> Self {
        Self::bytes(StatusCode::OK, body)
    }

    #[must_use]
    pub fn method_not_allowed(allow: &'static str) -> Self {
        Self::empty(StatusCode::METHOD_NOT_ALLOWED).allow(allow)
    }

    /// Creates an empty `401 Unauthorized` response with a static
    /// `WWW-Authenticate` challenge.
    #[must_use]
    pub fn unauthorized(challenge: &'static str) -> Self {
        Self::empty(StatusCode::UNAUTHORIZED).www_authenticate(challenge)
    }

    /// Sets a static Content-Type value. Use a literal or another trusted
    /// static string; dynamic response metadata belongs in [`HeaderBlock`].
    #[must_use]
    pub fn content_type(mut self, content_type: &'static str) -> Self {
        self.content_type = Some(content_type);
        self
    }

    /// Appends a validated zero-copy custom header block.
    #[must_use]
    pub fn headers(mut self, headers: HeaderBlock) -> Self {
        self.headers = Some(headers);
        self
    }

    #[doc(hidden)]
    #[must_use]
    pub fn allow(mut self, allow: &'static str) -> Self {
        self.allow = Some(allow);
        self
    }

    #[doc(hidden)]
    #[must_use]
    pub fn www_authenticate(mut self, challenge: &'static str) -> Self {
        self.www_authenticate = Some(challenge);
        self
    }

    /// Closes the connection after this response is flushed.
    #[must_use]
    pub fn close(mut self) -> Self {
        self.close = true;
        self
    }

    #[must_use]
    pub fn status(&self) -> StatusCode {
        self.status
    }

    #[must_use]
    pub fn body(&self) -> &ResponseBody {
        &self.body
    }

    pub(crate) fn content_type_ref(&self) -> Option<&'static str> {
        self.content_type
    }

    pub(crate) fn header_block(&self) -> Option<&HeaderBlock> {
        self.headers.as_ref()
    }

    pub(crate) fn allow_ref(&self) -> Option<&'static str> {
        self.allow
    }

    pub(crate) fn www_authenticate_ref(&self) -> Option<&'static str> {
        self.www_authenticate
    }

    pub(crate) fn should_close(&self) -> bool {
        self.close
    }
}

fn validate_header_block(mut bytes: &[u8]) -> Result<(), HeaderBlockError> {
    while !bytes.is_empty() {
        let Some(line_end) = bytes.windows(2).position(|window| window == b"\r\n") else {
            return Err(HeaderBlockError::MissingLineEnding);
        };
        let line = &bytes[..line_end];
        if line.is_empty() {
            return Err(HeaderBlockError::EmptyLine);
        }
        if line.contains(&b'\r') || line.contains(&b'\n') {
            return Err(HeaderBlockError::InvalidValue);
        }
        let Some(colon) = line.iter().position(|byte| *byte == b':') else {
            return Err(HeaderBlockError::InvalidName);
        };
        if colon == 0 || !line[..colon].iter().copied().all(is_token) {
            return Err(HeaderBlockError::InvalidName);
        }
        let name = &line[..colon];
        if name.eq_ignore_ascii_case(b"connection")
            || name.eq_ignore_ascii_case(b"content-length")
            || name.eq_ignore_ascii_case(b"transfer-encoding")
        {
            return Err(HeaderBlockError::ReservedName);
        }
        bytes = &bytes[line_end + 2..];
    }
    Ok(())
}

const fn is_token(byte: u8) -> bool {
    matches!(
        byte,
        b'!' | b'#'..=b'\'' | b'*' | b'+' | b'-' | b'.' | b'^' | b'_' | b'`' | b'|'
            | b'~' | b'0'..=b'9' | b'A'..=b'Z' | b'a'..=b'z'
    )
}

#[cfg(test)]
mod tests {
    use crate::EphemeralBytesArena;

    use super::*;

    #[test]
    fn accepts_valid_dynamic_header_block_without_copying() {
        let arena = EphemeralBytesArena::new(128);
        let mut bytes = arena.alloc(22);
        bytes.extend_from_slice(b"X-Request-Id: abc\r\n");
        let bytes = bytes.freeze();
        let pointer = bytes.as_ptr();
        let headers = HeaderBlock::new(bytes).unwrap();

        assert_eq!(headers.as_slice().as_ptr(), pointer);
        assert_eq!(headers.as_slice(), b"X-Request-Id: abc\r\n");
    }

    #[test]
    fn rejects_runtime_owned_header_names() {
        let arena = EphemeralBytesArena::new(128);
        let mut bytes = arena.alloc(19);
        bytes.extend_from_slice(b"Content-Length: 1\r\n");
        assert_eq!(
            HeaderBlock::new(bytes.freeze()).unwrap_err(),
            HeaderBlockError::ReservedName
        );
    }
}
