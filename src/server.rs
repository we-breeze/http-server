use std::future::Future;
use std::io::IoSlice;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use bytes::{Buf, BufMut, BytesMut};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Semaphore;
use tokio::task::JoinSet;
use tracing::{debug, warn};

use crate::{
    Authenticator, EphemeralBytes, EphemeralBytesArena, Error, Header, NoAuthenticator, Request,
    Response, Result, StatusCode,
};

const INITIAL_READ_BUFFER_CAPACITY: usize = 4 * 1024;
const MAX_REQUEST_HEADERS: usize = 64;

/// Handles one borrowed request and produces an owned response.
///
/// The returned future may borrow the request, but its [`Response`] must own
/// all of its bytes. The intended response path is
/// `request.response_bytes(capacity).freeze()` followed by
/// [`Response::bytes`]. That allocation remains live until the socket write
/// completes, so its body can be written without a second framework copy.
pub trait Handler<A = NoAuthenticator>: Send + Sync + 'static
where
    A: Authenticator,
{
    fn call<'a>(
        &'a self,
        request: Request<'a>,
        authenticator: &'a A,
    ) -> impl Future<Output = Response> + Send + 'a;
}

/// Runtime limits and socket policy for one HTTP server.
#[derive(Clone, Debug)]
pub struct ServerConfig {
    /// Storage used for all arena-backed response heads, dynamic headers, and
    /// response bodies. Clone the same arena into dependent SDKs at process
    /// startup when they should share its two chunks.
    pub arena: EphemeralBytesArena,
    /// Maximum simultaneously accepted TCP connections.
    pub max_connections: usize,
    /// Maximum bytes allowed before the complete HTTP request header is found.
    pub max_request_head_bytes: usize,
    /// Maximum fixed Content-Length request body.
    pub max_request_body_bytes: usize,
    /// Maximum time for one complete request read, handler invocation, and
    /// response write.
    pub request_timeout: Duration,
    /// Maximum time to wait for accepted connections after shutdown begins.
    pub shutdown_grace: Duration,
    /// Whether to enable `TCP_NODELAY` on each accepted socket.
    pub tcp_nodelay: bool,
}

impl ServerConfig {
    /// Creates bounded defaults around an application-owned arena.
    #[must_use]
    pub fn new(arena: EphemeralBytesArena) -> Self {
        Self {
            arena,
            max_connections: 1024,
            max_request_head_bytes: 32 * 1024,
            max_request_body_bytes: 1024 * 1024,
            request_timeout: Duration::from_secs(30),
            shutdown_grace: Duration::from_secs(10),
            tcp_nodelay: true,
        }
    }

    fn validate(&self) -> Result<()> {
        if self.max_connections == 0 {
            return Err(Error::InvalidConfig(
                "max_connections must be greater than zero",
            ));
        }
        if self.max_request_head_bytes == 0 {
            return Err(Error::InvalidConfig(
                "max_request_head_bytes must be greater than zero",
            ));
        }
        if self.max_request_body_bytes == 0 {
            return Err(Error::InvalidConfig(
                "max_request_body_bytes must be greater than zero",
            ));
        }
        if self.request_timeout.is_zero() {
            return Err(Error::InvalidConfig(
                "request_timeout must be greater than zero",
            ));
        }
        if self.shutdown_grace.is_zero() {
            return Err(Error::InvalidConfig(
                "shutdown_grace must be greater than zero",
            ));
        }
        Ok(())
    }
}

/// A bound HTTP/1.1 server.
pub struct Server<H, A = NoAuthenticator> {
    listener: TcpListener,
    handler: Arc<H>,
    authenticator: Arc<A>,
    config: ServerConfig,
}

impl<H> Server<H, NoAuthenticator>
where
    H: Handler<NoAuthenticator>,
{
    /// Binds an address but does not begin accepting connections yet.
    ///
    /// # Errors
    ///
    /// Returns an error when the configuration is invalid or the socket cannot
    /// be bound.
    pub async fn bind(address: SocketAddr, handler: H, config: ServerConfig) -> Result<Self> {
        bind_server(address, handler, NoAuthenticator, config).await
    }
}

impl<H, A> Server<H, A>
where
    H: Handler<A>,
    A: Authenticator,
{
    /// Binds an address with an application-defined request authenticator.
    ///
    /// API routes declared with `auth = required` or `auth = optional` invoke
    /// this authenticator after static route matching and before body decoding.
    /// Routes without an auth option do not invoke it.
    ///
    /// # Errors
    ///
    /// Returns an error when the configuration is invalid or the socket cannot
    /// be bound.
    pub async fn bind_with_authenticator(
        address: SocketAddr,
        handler: H,
        authenticator: A,
        config: ServerConfig,
    ) -> Result<Self> {
        bind_server(address, handler, authenticator, config).await
    }

    /// Returns the local socket address selected by bind.
    ///
    /// # Errors
    ///
    /// Returns an error if the operating system cannot query the bound socket.
    pub fn local_addr(&self) -> Result<SocketAddr> {
        Ok(self.listener.local_addr()?)
    }

    /// Accepts HTTP connections until `shutdown` resolves, then drains active
    /// connections for [`ServerConfig::shutdown_grace`].
    ///
    /// Connections over the configured limit are immediately closed. Individual
    /// malformed client connections are logged and do not stop the listener.
    ///
    /// # Errors
    ///
    /// Returns an error when accepting a connection fails or a connection task
    /// panics while the server is running or draining.
    pub async fn serve_until<F>(self, shutdown: F) -> Result<()>
    where
        F: Future<Output = ()> + Send,
    {
        let Self {
            listener,
            handler,
            authenticator,
            config,
        } = self;
        let connection_limit = Arc::new(Semaphore::new(config.max_connections));
        let mut connections = JoinSet::new();
        tokio::pin!(shutdown);

        loop {
            tokio::select! {
                () = &mut shutdown => break,
                accepted = listener.accept() => {
                    let (stream, peer_addr) = accepted?;
                    let Ok(permit) = connection_limit.clone().try_acquire_owned() else {
                        debug!(%peer_addr, "rejecting HTTP connection at capacity");
                        drop(stream);
                        continue;
                    };
                    if let Err(error) = stream.set_nodelay(config.tcp_nodelay) {
                        warn!(%peer_addr, %error, "failed to configure HTTP connection");
                        continue;
                    }
                    let handler = Arc::clone(&handler);
                    let authenticator = Arc::clone(&authenticator);
                    let config = config.clone();
                    connections.spawn(async move {
                        let _permit = permit;
                        if let Err(error) = serve_connection(
                            stream,
                            peer_addr,
                            handler,
                            authenticator,
                            config,
                        )
                        .await
                        {
                            debug!(%peer_addr, %error, "HTTP connection closed with error");
                        }
                    });
                }
                joined = connections.join_next(), if !connections.is_empty() => {
                    if let Some(Err(error)) = joined {
                        return Err(Error::Task(error.to_string()));
                    }
                }
            }
        }

        if let Ok(result) = tokio::time::timeout(config.shutdown_grace, async {
            while let Some(joined) = connections.join_next().await {
                joined.map_err(|error| Error::Task(error.to_string()))?;
            }
            Ok::<(), Error>(())
        })
        .await
        {
            result
        } else {
            connections.abort_all();
            while connections.join_next().await.is_some() {}
            Ok(())
        }
    }
}

async fn bind_server<H, A>(
    address: SocketAddr,
    handler: H,
    authenticator: A,
    config: ServerConfig,
) -> Result<Server<H, A>>
where
    H: Handler<A>,
    A: Authenticator,
{
    config.validate()?;
    Ok(Server {
        listener: TcpListener::bind(address).await?,
        handler: Arc::new(handler),
        authenticator: Arc::new(authenticator),
        config,
    })
}

async fn serve_connection<H, A>(
    mut stream: TcpStream,
    peer_addr: SocketAddr,
    handler: Arc<H>,
    authenticator: Arc<A>,
    config: ServerConfig,
) -> std::io::Result<()>
where
    H: Handler<A>,
    A: Authenticator,
{
    let mut read_buffer = BytesMut::with_capacity(INITIAL_READ_BUFFER_CAPACITY);
    loop {
        let result = tokio::time::timeout(
            config.request_timeout,
            receive_and_handle(
                &mut stream,
                &mut read_buffer,
                peer_addr,
                handler.as_ref(),
                authenticator.as_ref(),
                &config,
            ),
        )
        .await;

        let (response, consumed, request_keep_alive) = match result {
            Ok(Ok(result)) => result,
            Ok(Err(RequestFailure::Closed)) => return Ok(()),
            Ok(Err(failure)) => {
                let response = Response::empty(failure.status()).close();
                write_response(&mut stream, &config.arena, &response, true).await?;
                return Ok(());
            }
            Err(_) => {
                let response = Response::empty(StatusCode::REQUEST_TIMEOUT).close();
                write_response(&mut stream, &config.arena, &response, true).await?;
                return Ok(());
            }
        };

        let close = !request_keep_alive || response.should_close();
        write_response(&mut stream, &config.arena, &response, close).await?;
        read_buffer.advance(consumed);
        if close {
            return Ok(());
        }
    }
}

async fn receive_and_handle<H, A>(
    stream: &mut TcpStream,
    read_buffer: &mut BytesMut,
    peer_addr: SocketAddr,
    handler: &H,
    authenticator: &A,
    config: &ServerConfig,
) -> std::result::Result<(Response, usize, bool), RequestFailure>
where
    H: Handler<A>,
    A: Authenticator,
{
    let inspection = loop {
        match inspect_request(read_buffer, config)? {
            Some(inspection) if read_buffer.len() >= inspection.total_len => break inspection,
            Some(inspection) => {
                read_more(stream, read_buffer, inspection.total_len).await?;
            }
            None => read_more(stream, read_buffer, config.max_request_head_bytes).await?,
        }
    };

    // Parse a second time only after the complete body exists. This preserves
    // the header array on the stack and keeps request fields borrowed instead
    // of allocating owned method, path, header, or body values.
    let mut header_storage = [httparse::EMPTY_HEADER; MAX_REQUEST_HEADERS];
    let mut parsed = httparse::Request::new(&mut header_storage);
    let httparse::Status::Complete(head_len) = parsed
        .parse(&read_buffer[..inspection.total_len])
        .map_err(map_parse_error)?
    else {
        return Err(RequestFailure::BadRequest);
    };
    debug_assert_eq!(head_len, inspection.head_len);
    let method = parsed.method.ok_or(RequestFailure::BadRequest)?;
    let target = parsed.path.ok_or(RequestFailure::BadRequest)?;
    // The descriptors live on this connection task's stack while the handler
    // awaits. Their names and values still borrow `read_buffer`; neither the
    // request metadata nor the body causes a heap allocation.
    let mut request_header_storage = [Header {
        name: "",
        value: &[],
    }; MAX_REQUEST_HEADERS];
    for (destination, source) in request_header_storage.iter_mut().zip(parsed.headers.iter()) {
        *destination = Header {
            name: source.name,
            value: source.value,
        };
    }
    let request = Request::new(
        method,
        target,
        &request_header_storage[..parsed.headers.len()],
        &read_buffer[head_len..inspection.total_len],
        peer_addr,
        &config.arena,
    );
    let response = handler.call(request, authenticator).await;

    if !response.status().is_valid() {
        return Ok((
            Response::empty(StatusCode::INTERNAL_SERVER_ERROR).close(),
            inspection.total_len,
            false,
        ));
    }
    Ok((response, inspection.total_len, inspection.keep_alive))
}

fn inspect_request(
    read_buffer: &[u8],
    config: &ServerConfig,
) -> std::result::Result<Option<Inspection>, RequestFailure> {
    if read_buffer.len() > config.max_request_head_bytes
        && !read_buffer.windows(4).any(|window| window == b"\r\n\r\n")
    {
        return Err(RequestFailure::HeadersTooLarge);
    }

    let mut header_storage = [httparse::EMPTY_HEADER; MAX_REQUEST_HEADERS];
    let mut parsed = httparse::Request::new(&mut header_storage);
    let status = parsed.parse(read_buffer).map_err(map_parse_error)?;
    let httparse::Status::Complete(head_len) = status else {
        return Ok(None);
    };
    if head_len > config.max_request_head_bytes {
        return Err(RequestFailure::HeadersTooLarge);
    }
    if parsed.version != Some(1) {
        return Err(RequestFailure::UnsupportedVersion);
    }

    let mut content_length = None;
    let mut close = false;
    for header in parsed.headers.iter() {
        if header.name.eq_ignore_ascii_case("content-length") {
            let length = parse_content_length(header.value)?;
            if let Some(previous) = content_length {
                if previous != length {
                    return Err(RequestFailure::BadRequest);
                }
            }
            content_length = Some(length);
        } else if header.name.eq_ignore_ascii_case("transfer-encoding") {
            return Err(RequestFailure::UnsupportedTransferEncoding);
        } else if header.name.eq_ignore_ascii_case("expect") {
            return Err(RequestFailure::ExpectationFailed);
        } else if header.name.eq_ignore_ascii_case("connection")
            && header_contains_token(header.value, b"close")
        {
            close = true;
        }
    }
    let body_len = content_length.unwrap_or(0);
    if body_len > config.max_request_body_bytes {
        return Err(RequestFailure::BodyTooLarge);
    }
    let total_len = head_len
        .checked_add(body_len)
        .ok_or(RequestFailure::BodyTooLarge)?;
    Ok(Some(Inspection {
        head_len,
        total_len,
        keep_alive: !close,
    }))
}

async fn read_more(
    stream: &mut TcpStream,
    read_buffer: &mut BytesMut,
    limit: usize,
) -> std::result::Result<(), RequestFailure> {
    let Some(remaining) = limit.checked_sub(read_buffer.len()) else {
        return Err(RequestFailure::BadRequest);
    };
    if remaining == 0 {
        return Err(RequestFailure::BadRequest);
    }
    // BufMut::limit constrains read_buf to the exact protocol limit while
    // retaining a direct kernel-to-connection-buffer read. No stack staging
    // buffer and no second input-body copy are introduced here.
    let mut destination = (&mut *read_buffer).limit(remaining);
    let read = stream
        .read_buf(&mut destination)
        .await
        .map_err(RequestFailure::Io)?;
    if read == 0 {
        return Err(RequestFailure::Closed);
    }
    Ok(())
}

async fn write_response(
    stream: &mut TcpStream,
    arena: &EphemeralBytesArena,
    response: &Response,
    close: bool,
) -> std::io::Result<()> {
    let head = encode_response_head(arena, response, close);
    write_all_vectored(stream, head.as_ref(), response.body().as_slice()).await
}

fn encode_response_head(
    arena: &EphemeralBytesArena,
    response: &Response,
    close: bool,
) -> EphemeralBytes {
    let body_len = response.body().len();
    let mut body_decimal = itoa::Buffer::new();
    let content_length = body_decimal.format(body_len);
    let status = response.status();
    let reason = status.reason();
    let custom_headers = response
        .header_block()
        .map_or(&[][..], |headers| headers.as_slice());
    let content_type_len = response
        .content_type_ref()
        .map_or(0, |value| b"Content-Type: ".len() + value.len() + 2);
    let allow_len = response
        .allow_ref()
        .map_or(0, |value| b"Allow: ".len() + value.len() + 2);
    let www_authenticate_len = response
        .www_authenticate_ref()
        .map_or(0, |value| b"WWW-Authenticate: ".len() + value.len() + 2);
    let connection_len = if close {
        b"Connection: close\r\n".len()
    } else {
        0
    };
    let capacity = b"HTTP/1.1 ".len()
        + 3
        + 1
        + reason.len()
        + 2
        + custom_headers.len()
        + content_type_len
        + allow_len
        + www_authenticate_len
        + b"Content-Length: ".len()
        + content_length.len()
        + 2
        + connection_len
        + 2;
    let mut output = arena.alloc(capacity);
    output.extend_from_slice(b"HTTP/1.1 ");
    let mut status_decimal = itoa::Buffer::new();
    output.extend_from_slice(status_decimal.format(status.as_u16()).as_bytes());
    output.extend_from_slice(b" ");
    output.extend_from_slice(reason.as_bytes());
    output.extend_from_slice(b"\r\n");
    output.extend_from_slice(custom_headers);
    if let Some(content_type) = response.content_type_ref() {
        output.extend_from_slice(b"Content-Type: ");
        output.extend_from_slice(content_type.as_bytes());
        output.extend_from_slice(b"\r\n");
    }
    if let Some(allow) = response.allow_ref() {
        output.extend_from_slice(b"Allow: ");
        output.extend_from_slice(allow.as_bytes());
        output.extend_from_slice(b"\r\n");
    }
    if let Some(challenge) = response.www_authenticate_ref() {
        output.extend_from_slice(b"WWW-Authenticate: ");
        output.extend_from_slice(challenge.as_bytes());
        output.extend_from_slice(b"\r\n");
    }
    output.extend_from_slice(b"Content-Length: ");
    output.extend_from_slice(content_length.as_bytes());
    output.extend_from_slice(b"\r\n");
    if close {
        output.extend_from_slice(b"Connection: close\r\n");
    }
    output.extend_from_slice(b"\r\n");
    debug_assert_eq!(output.len(), capacity);
    output.freeze()
}

async fn write_all_vectored(
    stream: &mut TcpStream,
    head: &[u8],
    body: &[u8],
) -> std::io::Result<()> {
    let mut head_offset = 0;
    let mut body_offset = 0;
    while head_offset < head.len() || body_offset < body.len() {
        let slices = [
            IoSlice::new(&head[head_offset..]),
            IoSlice::new(&body[body_offset..]),
        ];
        let written = stream.write_vectored(&slices).await?;
        if written == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::WriteZero,
                "failed to write complete HTTP response",
            ));
        }
        let head_remaining = head.len() - head_offset;
        if written < head_remaining {
            head_offset += written;
        } else {
            head_offset = head.len();
            body_offset += written - head_remaining;
        }
    }
    Ok(())
}

#[derive(Clone, Copy)]
struct Inspection {
    head_len: usize,
    total_len: usize,
    keep_alive: bool,
}

#[derive(Debug)]
enum RequestFailure {
    BadRequest,
    HeadersTooLarge,
    BodyTooLarge,
    ExpectationFailed,
    UnsupportedTransferEncoding,
    UnsupportedVersion,
    Closed,
    Io(std::io::Error),
}

impl RequestFailure {
    const fn status(&self) -> StatusCode {
        match self {
            Self::HeadersTooLarge => StatusCode::REQUEST_HEADER_FIELDS_TOO_LARGE,
            Self::BodyTooLarge => StatusCode::PAYLOAD_TOO_LARGE,
            Self::ExpectationFailed => StatusCode::EXPECTATION_FAILED,
            Self::UnsupportedTransferEncoding => StatusCode::NOT_IMPLEMENTED,
            Self::UnsupportedVersion => StatusCode::HTTP_VERSION_NOT_SUPPORTED,
            Self::BadRequest | Self::Closed | Self::Io(_) => StatusCode::BAD_REQUEST,
        }
    }
}

impl std::fmt::Display for RequestFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BadRequest => formatter.write_str("malformed HTTP request"),
            Self::HeadersTooLarge => {
                formatter.write_str("HTTP request headers exceed configured limit")
            }
            Self::BodyTooLarge => formatter.write_str("HTTP request body exceeds configured limit"),
            Self::ExpectationFailed => formatter.write_str("HTTP Expect header is unsupported"),
            Self::UnsupportedTransferEncoding => {
                formatter.write_str("HTTP transfer encoding is unsupported")
            }
            Self::UnsupportedVersion => formatter.write_str("HTTP version is unsupported"),
            Self::Closed => formatter.write_str("peer closed connection"),
            Self::Io(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for RequestFailure {}

fn map_parse_error(error: httparse::Error) -> RequestFailure {
    match error {
        httparse::Error::TooManyHeaders => RequestFailure::HeadersTooLarge,
        _ => RequestFailure::BadRequest,
    }
}

fn parse_content_length(value: &[u8]) -> std::result::Result<usize, RequestFailure> {
    let value = std::str::from_utf8(value).map_err(|_| RequestFailure::BadRequest)?;
    let value = value.trim_matches([' ', '\t']);
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(RequestFailure::BadRequest);
    }
    value.parse().map_err(|_| RequestFailure::BadRequest)
}

fn header_contains_token(value: &[u8], expected: &[u8]) -> bool {
    value
        .split(|byte| *byte == b',')
        .any(|token| trim_ascii(token).eq_ignore_ascii_case(expected))
}

fn trim_ascii(mut bytes: &[u8]) -> &[u8] {
    while matches!(bytes.first(), Some(b' ' | b'\t')) {
        bytes = &bytes[1..];
    }
    while matches!(bytes.last(), Some(b' ' | b'\t')) {
        bytes = &bytes[..bytes.len() - 1];
    }
    bytes
}
