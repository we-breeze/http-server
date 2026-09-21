use std::future::Future;
use std::net::SocketAddr;

use crate::{EphemeralBytesArena, Header, Request, Response, StatusCode};

/// Metadata made available to an [`Authenticator`].
///
/// It deliberately excludes the request body and response arena. Header names
/// and values borrow the connection receive buffer and remain valid only while
/// the authenticator future is awaited.
#[derive(Clone, Copy, Debug)]
pub struct AuthRequest<'a> {
    method: &'a str,
    path: &'a str,
    query: Option<&'a str>,
    headers: &'a [Header<'a>],
    peer_addr: SocketAddr,
}

impl<'a> AuthRequest<'a> {
    #[doc(hidden)]
    #[must_use]
    pub fn from_request(request: &'a Request<'a>) -> Self {
        Self {
            method: request.method(),
            path: request.path(),
            query: request.query(),
            headers: request.headers(),
            peer_addr: request.peer_addr(),
        }
    }

    /// HTTP method exactly as it appeared in the request line.
    #[must_use]
    pub fn method(self) -> &'a str {
        self.method
    }

    /// Request path without its query string.
    #[must_use]
    pub fn path(self) -> &'a str {
        self.path
    }

    /// Raw query string without the leading `?`, when present.
    #[must_use]
    pub fn query(self) -> Option<&'a str> {
        self.query
    }

    /// Returns the first case-insensitive matching request header.
    #[must_use]
    pub fn header(self, name: &str) -> Option<&'a [u8]> {
        self.headers
            .iter()
            .find(|header| header.name.eq_ignore_ascii_case(name))
            .map(|header| header.value)
    }

    /// Remote peer accepted for this connection.
    #[must_use]
    pub fn peer_addr(self) -> SocketAddr {
        self.peer_addr
    }
}

/// Authenticates a request before an API method receives its business
/// parameters.
///
/// The implementation returns an owned principal. It must not retain a
/// borrowed header or other [`AuthRequest`] data after its future completes.
pub trait Authenticator<P>: Send + Sync + 'static
where
    P: Send + Sync + 'static,
{
    /// Authenticates one request.
    fn authenticate<'a>(
        &'a self,
        request: AuthRequest<'a>,
    ) -> impl Future<Output = std::result::Result<P, AuthFailure>> + Send + 'a;

    /// Maps authentication failures to the application's HTTP error contract.
    fn reject(
        &self,
        _request: AuthRequest<'_>,
        failure: AuthFailure,
        _arena: &EphemeralBytesArena,
    ) -> Response {
        failure.into_response()
    }
}

/// A failure returned by an [`Authenticator`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AuthFailure {
    /// The request supplied no credentials for this authentication scheme.
    MissingCredentials { challenge: &'static str },
    /// The request supplied credentials which could not be authenticated.
    InvalidCredentials { challenge: &'static str },
    /// The authentication authority is temporarily unavailable.
    Unavailable,
    /// Authentication failed unexpectedly.
    Internal,
}

impl AuthFailure {
    /// Creates a missing-credentials failure with the given WWW-Authenticate
    /// challenge, for example `"Bearer"` or `"Signature"`.
    #[must_use]
    pub const fn missing_credentials(challenge: &'static str) -> Self {
        Self::MissingCredentials { challenge }
    }

    /// Creates an invalid-credentials failure with the given WWW-Authenticate
    /// challenge, for example `"Bearer"` or `"Signature"`.
    #[must_use]
    pub const fn invalid_credentials(challenge: &'static str) -> Self {
        Self::InvalidCredentials { challenge }
    }

    fn into_response(self) -> Response {
        match self {
            Self::MissingCredentials { challenge } | Self::InvalidCredentials { challenge } => {
                Response::unauthorized(challenge)
            }
            Self::Unavailable => Response::empty(StatusCode::SERVICE_UNAVAILABLE),
            Self::Internal => Response::empty(StatusCode::INTERNAL_SERVER_ERROR).close(),
        }
    }
}

/// Default authenticator used by [`crate::Server::bind`].
///
/// Public API routes never invoke it. A route with an `#[auth]` parameter under
/// this server consistently returns `401`.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoAuthenticator;

impl<P> Authenticator<P> for NoAuthenticator
where
    P: Send + Sync + 'static,
{
    fn authenticate<'a>(
        &'a self,
        _request: AuthRequest<'a>,
    ) -> impl std::future::Future<Output = std::result::Result<P, AuthFailure>> + Send {
        std::future::ready(Err(AuthFailure::missing_credentials("Bearer")))
    }
}

// Keep the owned response inline to avoid a separate error-path allocation.
#[allow(clippy::result_large_err)]
#[doc(hidden)]
pub async fn authenticate_required<A, P>(
    authenticator: &A,
    request: AuthRequest<'_>,
    arena: &EphemeralBytesArena,
) -> std::result::Result<P, Response>
where
    A: Authenticator<P>,
    P: Send + Sync + 'static,
{
    <A as Authenticator<P>>::authenticate(authenticator, request)
        .await
        .map_err(|failure| <A as Authenticator<P>>::reject(authenticator, request, failure, arena))
}

// Keep the owned response inline to avoid a separate error-path allocation.
#[allow(clippy::result_large_err)]
#[doc(hidden)]
pub async fn authenticate_optional<A, P>(
    authenticator: &A,
    request: AuthRequest<'_>,
    arena: &EphemeralBytesArena,
) -> std::result::Result<Option<P>, Response>
where
    A: Authenticator<P>,
    P: Send + Sync + 'static,
{
    match <A as Authenticator<P>>::authenticate(authenticator, request).await {
        Ok(principal) => Ok(Some(principal)),
        Err(AuthFailure::MissingCredentials { .. }) => Ok(None),
        Err(error) => Err(<A as Authenticator<P>>::reject(
            authenticator,
            request,
            error,
            arena,
        )),
    }
}
