//! Generic HTTP/1.1 protocol handoff. Protocol framing belongs to the caller.
use crate::{Request, Response, StatusCode};
use bytes::{Buf, Bytes};
use std::{
    fmt,
    future::Future,
    io,
    pin::Pin,
    task::{Context, Poll},
};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

type Callback =
    Box<dyn FnOnce(Upgraded) -> Pin<Box<dyn Future<Output = io::Result<()>> + Send>> + Send>;
trait Io: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Io for T {}

/// The owned connection after the 101 response has been written. Reads consume
/// bytes already received with the HTTP head before reading from the socket.
pub struct Upgraded {
    io: Box<dyn Io>,
    buffered: Bytes,
}
impl Upgraded {
    pub(crate) fn new(
        io: impl AsyncRead + AsyncWrite + Unpin + Send + 'static,
        buffered: Bytes,
    ) -> Self {
        Self {
            io: Box::new(io),
            buffered,
        }
    }
}
impl AsyncRead for Upgraded {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        if !self.buffered.is_empty() {
            let count = buf.remaining().min(self.buffered.len());
            buf.put_slice(&self.buffered[..count]);
            self.buffered.advance(count);
            return Poll::Ready(Ok(()));
        }
        Pin::new(&mut self.io).poll_read(cx, buf)
    }
}
impl AsyncWrite for Upgraded {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.io).poll_write(cx, buf)
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.io).poll_flush(cx)
    }
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.io).poll_shutdown(cx)
    }
}

/// Validated protocol selection and a session callback owned by the server.
/// The HTTP request timeout covers negotiation only; the callback runs until
/// completion or server shutdown. It remains within the connection limit.
pub struct Upgrade {
    protocol: String,
    callback: Option<Callback>,
}
impl fmt::Debug for Upgrade {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Upgrade")
            .field("protocol", &self.protocol)
            .finish_non_exhaustive()
    }
}
impl Upgrade {
    /// Accept a protocol offered in both `Connection: upgrade` and `Upgrade`.
    /// Protocol-specific validation (e.g. WebSocket version/key) is the caller's
    /// responsibility. Add handshake headers to the returned response.
    pub fn accept<F, Fut>(
        request: &Request<'_>,
        protocol: &str,
        callback: F,
    ) -> Result<Response, UpgradeError>
    where
        F: FnOnce(Upgraded) -> Fut + Send + 'static,
        Fut: Future<Output = io::Result<()>> + Send + 'static,
    {
        let valid_protocol = !protocol.is_empty()
            && protocol.split('/').count() <= 2
            && protocol
                .split('/')
                .all(|part| !part.is_empty() && part.bytes().all(is_token));
        if !valid_protocol {
            return Err(UpgradeError);
        }
        let contains = |name: &str, expected: &[u8]| {
            request
                .headers()
                .iter()
                .filter(|h| h.name.eq_ignore_ascii_case(name))
                .any(|h| {
                    h.value
                        .split(|b| *b == b',')
                        .any(|t| t.trim_ascii().eq_ignore_ascii_case(expected))
                })
        };
        if !contains("connection", b"upgrade")
            || contains("connection", b"close")
            || !contains("upgrade", protocol.as_bytes())
            || request.method() == "HEAD"
        {
            return Err(UpgradeError);
        }
        let mut response = Response::empty(StatusCode::SWITCHING_PROTOCOLS);
        response.upgrade = Some(Self {
            protocol: protocol.into(),
            callback: Some(Box::new(move |io| Box::pin(callback(io)))),
        });
        Ok(response)
    }
    pub(crate) fn protocol(&self) -> &str {
        &self.protocol
    }
    pub(crate) fn metadata(protocol: String) -> Self {
        Self {
            protocol,
            callback: None,
        }
    }
    pub(crate) async fn run(mut self, io: Upgraded) -> io::Result<()> {
        self.callback.take().expect("upgrade callback")(io).await
    }
}
fn is_token(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b)
}
/// The request did not offer a valid protocol upgrade.
#[derive(Clone, Copy, Debug, thiserror::Error)]
#[error("request does not offer the selected HTTP upgrade protocol")]
pub struct UpgradeError;
