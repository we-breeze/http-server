use std::fmt;

use crate::EphemeralBytes;

pub use http::StatusCode;

/// An HTTP response body that can be written without a framework body copy.
#[derive(Debug)]
pub enum ResponseBody {
    Empty,
    Static(&'static [u8]),
    Arena(EphemeralBytes),
    Owned(bytes::Bytes),
    Stream(crate::stream::ResponseStream),
}

impl ResponseBody {
    /// Known body length; `None` for a stream whose length is unknown.
    #[must_use]
    pub fn content_length(&self) -> Option<u64> {
        match self {
            Self::Empty => Some(0),
            Self::Static(bytes) => Some(bytes.len() as u64),
            Self::Arena(bytes) => Some(bytes.len() as u64),
            Self::Owned(bytes) => Some(bytes.len() as u64),
            Self::Stream(stream) => stream.content_length,
        }
    }

    pub(crate) fn as_slice(&self) -> &[u8] {
        match self {
            Self::Empty | Self::Stream(_) => &[],
            Self::Static(bytes) => bytes,
            Self::Arena(bytes) => bytes.as_ref(),
            Self::Owned(bytes) => bytes.as_ref(),
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
    suppress_body: bool,
    conversion_failed: bool,
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
            suppress_body: false,
            conversion_failed: false,
        }
    }

    #[must_use]
    pub fn empty(status: StatusCode) -> Self {
        Self::new(status, ResponseBody::Empty)
    }

    #[must_use]
    pub fn owned_bytes(status: StatusCode, body: impl Into<bytes::Bytes>) -> Self {
        Self::new(status, ResponseBody::Owned(body.into()))
    }

    #[must_use]
    pub fn with_status(mut self, status: StatusCode) -> Self {
        if !self.conversion_failed {
            self.status = status;
        }
        self
    }

    pub(crate) fn conversion_failure() -> Self {
        let mut response = Self::empty(StatusCode::INTERNAL_SERVER_ERROR).close();
        response.conversion_failed = true;
        response
    }

    pub(crate) fn body_mut(&mut self) -> &mut ResponseBody {
        &mut self.body
    }

    pub(crate) fn set_content_length(mut self, length: u64) -> Self {
        match &mut self.body {
            ResponseBody::Stream(stream) => stream.content_length = Some(length),
            body if body.content_length() == Some(length) => {}
            _ => return Self::conversion_failure(),
        }
        self
    }

    pub(crate) fn suppress_body(&mut self) {
        self.suppress_body = true;
    }

    pub(crate) fn sends_body(&self) -> bool {
        !self.suppress_body && self.permits_body()
    }

    pub(crate) fn permits_body(&self) -> bool {
        self.status.as_u16() >= 200 && !matches!(self.status.as_u16(), 204 | 304)
    }

    pub(crate) fn with_http_headers(
        mut self,
        headers: &http::HeaderMap,
        arena: &crate::EphemeralBytesArena,
    ) -> Self {
        if headers.is_empty() {
            return self;
        }
        if headers.contains_key("content-type") {
            self.content_type = None;
        }
        let previous = self.headers.as_ref().map_or(&[][..], HeaderBlock::as_slice);
        let len = previous.len()
            + headers
                .iter()
                .map(|(name, value)| name.as_str().len() + value.as_bytes().len() + 4)
                .sum::<usize>();
        let mut output = arena.alloc(len);
        output.extend_from_slice(previous);
        for (name, value) in headers {
            output.extend_from_slice(name.as_str().as_bytes());
            output.extend_from_slice(b": ");
            output.extend_from_slice(value.as_bytes());
            output.extend_from_slice(b"\r\n");
        }
        match HeaderBlock::new(output.freeze()) {
            Ok(block) => {
                self.headers = Some(block);
                self
            }
            Err(_) => Self::conversion_failure(),
        }
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
