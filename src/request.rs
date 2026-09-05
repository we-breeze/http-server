use std::net::SocketAddr;

use crate::{EphemeralBytesArena, EphemeralBytesMut};

/// One parsed HTTP request, borrowing its connection's receive buffer.
///
/// The request is valid only for the duration of
/// [`Handler::call`](crate::Handler::call). In particular, a handler must not
/// return a response that borrows `body`, `target`, or `headers`.
#[derive(Debug)]
pub struct Request<'a> {
    method: &'a str,
    target: &'a str,
    headers: &'a [Header<'a>],
    body: &'a [u8],
    peer_addr: SocketAddr,
    response_arena: &'a EphemeralBytesArena,
    pub(crate) rejection_handler: crate::RejectionHandler,
}

impl<'a> Request<'a> {
    pub(crate) fn new(
        method: &'a str,
        target: &'a str,
        headers: &'a [Header<'a>],
        body: &'a [u8],
        peer_addr: SocketAddr,
        response_arena: &'a EphemeralBytesArena,
    ) -> Self {
        Self {
            method,
            target,
            headers,
            body,
            peer_addr,
            response_arena,
            rejection_handler: crate::rejection::default_rejection,
        }
    }

    /// HTTP method exactly as it appeared in the request line.
    #[must_use]
    pub fn method(&self) -> &str {
        self.method
    }

    /// Raw request target, including its query string when present.
    #[must_use]
    pub fn target(&self) -> &str {
        self.target
    }

    /// The path portion of [`Request::target`].
    #[must_use]
    pub fn path(&self) -> &str {
        self.target
            .split_once('?')
            .map_or(self.target, |(path, _)| path)
    }

    /// The raw query string, without the leading `?`.
    #[must_use]
    pub fn query(&self) -> Option<&str> {
        self.target.split_once('?').map(|(_, query)| query)
    }

    /// Request headers in wire order. Both names and values borrow the receive
    /// buffer; header values are intentionally exposed as bytes.
    #[must_use]
    pub fn headers(&self) -> &[Header<'a>] {
        self.headers
    }

    /// Returns the first case-insensitive matching header value.
    #[must_use]
    pub fn header(&self, name: &str) -> Option<&'a [u8]> {
        self.headers
            .iter()
            .find(|header| header.name.eq_ignore_ascii_case(name))
            .map(|header| header.value)
    }

    /// The complete fixed-length request body.
    #[must_use]
    pub fn body(&self) -> &'a [u8] {
        self.body
    }

    /// Remote peer accepted for this connection.
    #[must_use]
    pub fn peer_addr(&self) -> SocketAddr {
        self.peer_addr
    }

    /// Reserves writable response-body storage in the server's arena.
    ///
    /// Write the final body bytes and call [`EphemeralBytesMut::freeze`]; the
    /// resulting [`crate::EphemeralBytes`] can be passed to
    /// [`crate::Response::bytes`]. The server will not copy that body before
    /// writing it to the socket.
    #[must_use]
    pub fn response_bytes(&self, capacity: usize) -> EphemeralBytesMut {
        self.response_arena.alloc(capacity)
    }

    #[doc(hidden)]
    #[must_use]
    pub fn response_arena(&self) -> &EphemeralBytesArena {
        self.response_arena
    }

    #[doc(hidden)]
    #[must_use]
    pub fn reject(&self, error: crate::Rejection) -> crate::Response {
        (self.rejection_handler)(error, self.response_arena)
    }

    #[doc(hidden)]
    #[must_use]
    pub fn reject_body(
        &self,
        error: &crate::BodyError,
        parameter: &'static str,
    ) -> crate::Response {
        let kind = match error {
            crate::BodyError::UnsupportedMediaType => crate::RejectionKind::ContentType,
            crate::BodyError::Invalid(_) => crate::RejectionKind::Body,
        };
        self.reject(crate::Rejection::new(
            kind,
            parameter,
            None,
            error.to_string(),
        ))
    }
}

/// One HTTP request header borrowed from the connection receive buffer.
#[derive(Clone, Copy, Debug)]
pub struct Header<'a> {
    pub name: &'a str,
    pub value: &'a [u8],
}
