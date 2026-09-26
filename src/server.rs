use std::future::Future;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Semaphore;
use tokio::task::JoinSet;
use tracing::{debug, warn};

use crate::{
    EphemeralBytesArena, Error, Header, NoAuthenticator, Request, Response, Result, StatusCode,
};

mod receive;
use receive::{ReceiveState, RequestContext, receive_and_handle};

mod write;
use write::write_response;

const MAX_REQUEST_HEADERS: usize = 64;
const DEFAULT_ARENA_CHUNK_CAPACITY: usize = 16 * 1024 * 1024;

/// Handles one borrowed request and produces an owned response.
///
/// The returned future may borrow the request, but its [`Response`] must own
/// all of its bytes. The intended response path is
/// `request.response_bytes(capacity).freeze()` followed by
/// [`Response::bytes`]. That allocation remains live until the socket write
/// completes, so its body can be written without a second framework copy.
pub trait Handler<A = NoAuthenticator>: Send + Sync + 'static
where
    A: Send + Sync + 'static,
{
    /// Retained for source compatibility. Route metrics initialize lazily.
    #[doc(hidden)]
    fn register_metrics(&self) {}

    /// Route priority and fixed metrics, including a matched-path 405 fallback.
    /// Raw request paths must never become metric keys.
    #[doc(hidden)]
    fn route_metrics(&self, _path: &str, _method: &str) -> Option<(usize, crate::ApiMetrics)> {
        None
    }

    /// Static route metadata used to compose macro-exported API groups.
    #[doc(hidden)]
    fn route_priority(&self, _path: &str, _method: &str) -> Option<usize> {
        None
    }

    /// Bitset of standard methods accepted by paths matching this request.
    #[doc(hidden)]
    fn route_methods(&self, _path: &str) -> u16 {
        0
    }

    /// Static templates and method groups exported by the API macro.
    #[doc(hidden)]
    fn routes(&self) -> &'static [crate::__private::RouteDescriptor] {
        &[]
    }

    /// Adds an API, or flattens an existing router, into a listener's routes.
    #[doc(hidden)]
    fn append_to(self, router: &mut crate::Router<A>)
    where
        Self: Sized,
    {
        router.push(self);
    }

    /// Resolve before reading the body so timeouts retain route metrics.
    #[doc(hidden)]
    fn prepare<'p>(&self, path: &'p str, method: &str) -> crate::__private::PreparedRoute<'p> {
        crate::__private::PreparedRoute::legacy(path, self.route_metrics(path, method))
    }

    #[doc(hidden)]
    fn call_prepared<'a>(
        &'a self,
        request: Request<'a>,
        authenticator: &'a A,
        _prepared: &'a crate::__private::PreparedRoute<'_>,
    ) -> impl Future<Output = Response> + Send + 'a {
        self.call(request, authenticator)
    }

    /// Dispatch an already matched API group without searching its templates.
    #[doc(hidden)]
    fn call_route<'a>(
        &'a self,
        request: Request<'a>,
        authenticator: &'a A,
        _route: usize,
        _captures: crate::__private::RouteMatch<'a>,
    ) -> impl Future<Output = Response> + Send + 'a {
        self.call(request, authenticator)
    }

    fn call<'a>(
        &'a self,
        request: Request<'a>,
        authenticator: &'a A,
    ) -> impl Future<Output = Response> + Send + 'a;
}

/// Runtime limits and socket policy for one HTTP server.
#[derive(Clone, Debug)]
pub struct ServerConfig {
    /// Storage for request bodies, response bodies, and generated headers.
    /// Clone the same arena into dependent SDKs at process startup when they
    /// should share its two chunks.
    pub arena: EphemeralBytesArena,
    /// First receive segment, allocated lazily on socket readability (default 2 KiB).
    /// This is not the arena's backing chunk capacity.
    pub initial_request_segment_bytes: usize,
    /// Maximum *remaining* Body size reserved in one contiguous tail (default 64 KiB).
    /// Larger requests grow in bounded segments; zero disables this fast path.
    /// The full Body is still buffered before Handler::call in this API.
    pub max_preallocated_request_body_bytes: usize,
    /// Origin policy; absent when the application does not expose cross-origin APIs.
    pub cors: Option<crate::Cors>,
    /// Application mapping for failed parameter bindings.
    pub rejection_handler: crate::RejectionHandler,
    /// Maximum simultaneously accepted TCP connections.
    pub max_connections: usize,
    /// Maximum bytes allowed before the complete HTTP request header is found.
    pub max_request_head_bytes: usize,
    /// Maximum fixed Content-Length request body.
    pub max_request_body_bytes: usize,
    /// Maximum aggregate bytes retained for request bodies currently being
    /// handled. This prevents many valid, large requests from multiplying the
    /// process memory footprint up to `max_connections * max_request_body_bytes`.
    pub max_in_flight_request_body_bytes: usize,
    /// Maximum time for request read and handler invocation, or an inactive
    /// response write. Streaming downloads reset this timeout after each chunk.
    pub request_timeout: Duration,
    /// Maximum time to wait for accepted connections after shutdown begins.
    pub shutdown_grace: Duration,
    /// Whether to enable `TCP_NODELAY` on each accepted socket.
    pub tcp_nodelay: bool,
}

impl Default for ServerConfig {
    /// Creates the default server limits and a server-owned response arena.
    fn default() -> Self {
        Self::new(EphemeralBytesArena::new(DEFAULT_ARENA_CHUNK_CAPACITY))
    }
}

impl ServerConfig {
    /// Creates bounded defaults around an application-owned arena.
    #[must_use]
    pub fn new(arena: EphemeralBytesArena) -> Self {
        Self {
            arena,
            initial_request_segment_bytes: 2 * 1024,
            max_preallocated_request_body_bytes: 64 * 1024,
            cors: None,
            rejection_handler: crate::rejection::default_rejection,
            max_connections: 65_536,
            max_request_head_bytes: 32 * 1024,
            max_request_body_bytes: 8 * 1024 * 1024,
            max_in_flight_request_body_bytes: 64 * 1024 * 1024,
            request_timeout: Duration::from_secs(15),
            shutdown_grace: Duration::from_secs(10),
            tcp_nodelay: true,
        }
    }

    fn validate(&self) -> Result<()> {
        if self.initial_request_segment_bytes == 0 {
            return Err(Error::InvalidConfig(
                "initial_request_segment_bytes must be greater than zero",
            ));
        }
        if self.cors.as_ref().is_some_and(|cors| !cors.validate()) {
            return Err(Error::InvalidConfig("invalid CORS policy"));
        }
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
        if self.max_in_flight_request_body_bytes < self.max_request_body_bytes {
            return Err(Error::InvalidConfig(
                "max_in_flight_request_body_bytes must be at least max_request_body_bytes",
            ));
        }
        if u32::try_from(self.max_request_body_bytes).is_err() {
            return Err(Error::InvalidConfig(
                "max_request_body_bytes must not exceed u32::MAX",
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
    pub async fn bind(address: SocketAddr, handler: H) -> Result<Self> {
        Self::bind_with_config(address, handler, ServerConfig::default()).await
    }

    /// Binds a server with application-specific limits or policies.
    ///
    /// # Errors
    /// Returns an error for an invalid configuration or listener address.
    pub async fn bind_with_config(
        address: SocketAddr,
        handler: H,
        config: ServerConfig,
    ) -> Result<Self> {
        bind_server(address, handler, NoAuthenticator, config).await
    }
}

impl<H, A> Server<H, A>
where
    H: Handler<A>,
    A: Send + Sync + 'static,
{
    /// Binds an address with an application-defined request authenticator.
    ///
    /// API routes with an `#[auth]` parameter invoke this authenticator after
    /// static route matching and before body decoding. Other routes do not
    /// invoke it.
    ///
    /// # Errors
    ///
    /// Returns an error when the configuration is invalid or the socket cannot
    /// be bound.
    pub async fn bind_with_authenticator(
        address: SocketAddr,
        handler: H,
        authenticator: A,
    ) -> Result<Self> {
        Self::bind_with_authenticator_and_config(
            address,
            handler,
            authenticator,
            ServerConfig::default(),
        )
        .await
    }

    /// Binds authenticated APIs with application-specific limits or policies.
    ///
    /// # Errors
    /// Returns an error for an invalid configuration or listener address.
    pub async fn bind_with_authenticator_and_config(
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
        let request_body_limit = Arc::new(Semaphore::new(config.max_in_flight_request_body_bytes));
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
                    let request_body_limit = Arc::clone(&request_body_limit);
                    let config = config.clone();
                    connections.spawn(async move {
                        let _permit = permit;
                        if let Err(error) = serve_connection(
                            stream,
                            peer_addr,
                            handler,
                            authenticator,
                            request_body_limit,
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
    A: Send + Sync + 'static,
{
    config.validate()?;
    let listener = TcpListener::bind(address).await?;
    Ok(Server {
        listener,
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
    request_body_limit: Arc<Semaphore>,
    config: ServerConfig,
) -> std::io::Result<()>
where
    H: Handler<A>,
    A: Send + Sync + 'static,
{
    // No separate BytesMut payload. The two common descriptor slots are inline;
    // segment payloads are allocated on demand from the process-shared arena.
    let read_buffer = brz_io::Writer::with_initial_segment_size(
        &config.arena,
        config.initial_request_segment_bytes,
    )
    .into_reader();
    let mut state = ReceiveState {
        input: read_buffer,
        context: RequestContext::new(),
    };
    loop {
        let ReceiveState {
            input: read_buffer,
            context,
        } = &mut state;
        *context = RequestContext::new();
        let result = tokio::time::timeout(
            config.request_timeout,
            receive_and_handle(
                &mut stream,
                read_buffer,
                peer_addr,
                handler.as_ref(),
                authenticator.as_ref(),
                RequestLimits {
                    config: &config,
                    body_limit: &request_body_limit,
                },
                context,
            ),
        )
        .await;
        let (response, consumed, request_keep_alive) = match result {
            Ok(Ok(result)) => result,
            Ok(Err(RequestFailure::Closed)) => {
                read_buffer.clear();
                context.release_budget();
                return Ok(());
            }
            Ok(Err(failure)) => {
                let response = Response::empty(failure.status()).close();
                context.record(
                    response.status(),
                    response.body().content_length(),
                    false,
                    read_buffer,
                );
                read_buffer.clear();
                context.release_budget();
                write_response(
                    &mut stream,
                    &config.arena,
                    response,
                    true,
                    config.request_timeout,
                )
                .await?;
                return Ok(());
            }
            Err(_) => {
                let response = Response::empty(StatusCode::REQUEST_TIMEOUT).close();
                context.record(
                    response.status(),
                    response.body().content_length(),
                    true,
                    read_buffer,
                );
                read_buffer.clear();
                context.release_budget();
                write_response(
                    &mut stream,
                    &config.arena,
                    response,
                    true,
                    config.request_timeout,
                )
                .await?;
                return Ok(());
            }
        };
        context.record(
            response.status(),
            response.body().content_length(),
            false,
            read_buffer,
        );
        let close = !request_keep_alive || response.should_close();
        // Request/derived storage and its Body budget must not pin the arena for
        // a slow download. Only pipelined unread bytes survive this boundary.
        std::io::BufRead::consume(read_buffer, consumed);
        if close || read_buffer.is_empty() {
            read_buffer.clear();
        }
        context.release_budget();
        write_response(
            &mut stream,
            &config.arena,
            response,
            close,
            config.request_timeout,
        )
        .await?;
        if close {
            return Ok(());
        }
    }
}

struct RequestLimits<'a> {
    config: &'a ServerConfig,
    body_limit: &'a Arc<Semaphore>,
}

async fn reserve_request_body(
    body_limit: &Arc<Semaphore>,
    body_len: usize,
) -> std::result::Result<Option<tokio::sync::OwnedSemaphorePermit>, RequestFailure> {
    if body_len == 0 {
        return Ok(None);
    }
    Arc::clone(body_limit)
        .acquire_many_owned(u32::try_from(body_len).expect("validated request body size"))
        .await
        .map(Some)
        .map_err(|_| RequestFailure::ServiceUnavailable)
}

fn inspect_parsed_request(
    parsed: &httparse::Request<'_, '_>,
    head_len: usize,
    config: &ServerConfig,
) -> std::result::Result<Inspection, RequestFailure> {
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
    Ok(Inspection {
        head_len,
        total_len,
        keep_alive: !close,
    })
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
    ServiceUnavailable,
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
            Self::ServiceUnavailable => StatusCode::SERVICE_UNAVAILABLE,
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
            Self::ServiceUnavailable => {
                formatter.write_str("HTTP request body capacity is unavailable")
            }
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
