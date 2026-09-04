use std::future::Future;
use std::net::SocketAddr;

use crate::{Header, Request, Response, StatusCode};

/// Metadata made available to an [`Authenticator`].
///
/// It deliberately excludes the request body and response arena. Header names
/// and values borrow the connection receive buffer and remain valid only while
/// the authenticator future is awaited.
#[derive(Clone, Copy, Debug)]
pub struct AuthRequest<'a> {
    method: &'a str,
    path: &'a str,
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
pub trait Authenticator: Send + Sync + 'static {
    /// Application-defined identity available to protected API methods.
    type Principal: Send + Sync + 'static;

    /// Authenticates one request.
    fn authenticate<'a>(
        &'a self,
        request: AuthRequest<'a>,
    ) -> impl Future<Output = std::result::Result<Self::Principal, AuthFailure>> + Send + 'a;
}

/// The identity produced by a successful [`Authenticator`].
///
/// `Authenticated<T>` is injected by `#[api]` for a route declared with
/// `auth = required` or `auth = optional`. Its field is private so normal
/// business code consumes authentication context rather than constructing it.
#[derive(Debug)]
pub struct Authenticated<T>(T);

impl<T> Authenticated<T> {
    /// Borrows the authenticated principal.
    #[must_use]
    pub fn principal(&self) -> &T {
        &self.0
    }

    /// Consumes the authentication wrapper and returns its principal.
    #[must_use]
    pub fn into_principal(self) -> T {
        self.0
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
/// API routes without an `auth` option never invoke it. A route that requires
/// authentication under this server consistently returns `401`.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoAuthenticator;

impl Authenticator for NoAuthenticator {
    type Principal = ();

    async fn authenticate<'a>(
        &'a self,
        _request: AuthRequest<'a>,
    ) -> std::result::Result<Self::Principal, AuthFailure> {
        Err(AuthFailure::missing_credentials("Bearer"))
    }
}

#[doc(hidden)]
pub async fn authenticate_required<A>(
    authenticator: &A,
    request: AuthRequest<'_>,
) -> std::result::Result<Authenticated<A::Principal>, Response>
where
    A: Authenticator,
{
    authenticator
        .authenticate(request)
        .await
        .map(Authenticated)
        .map_err(AuthFailure::into_response)
}

#[doc(hidden)]
pub async fn authenticate_optional<A>(
    authenticator: &A,
    request: AuthRequest<'_>,
) -> std::result::Result<Option<Authenticated<A::Principal>>, Response>
where
    A: Authenticator,
{
    match authenticator.authenticate(request).await {
        Ok(principal) => Ok(Some(Authenticated(principal))),
        Err(AuthFailure::MissingCredentials { .. }) => Ok(None),
        Err(error) => Err(error.into_response()),
    }
}
