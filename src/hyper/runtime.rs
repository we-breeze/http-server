use super::{BoxError, UpgradeTasks};
use ::hyper::{
    body::{Body, Incoming},
    server::conn::http1,
    service::{Service, service_fn},
};
use bytes::Bytes;
use http::{Request, Response};
use hyper_util::rt::{TokioIo, TokioTimer};
use std::{
    future::Future,
    io,
    net::SocketAddr,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncWrite, ReadBuf},
    net::{TcpListener, TcpStream},
    sync::{OwnedSemaphorePermit, Semaphore, watch},
    task::JoinSet,
};

/// Peer metadata inserted before dispatch to any protocol or HTTP handler.
#[derive(Clone, Copy, Debug)]
pub struct PeerAddr(pub SocketAddr);
/// Listener bounds. These apply to HTTP negotiation and connection ownership,
/// not the duration of a long poll, stream, or upgraded session.
#[derive(Clone, Copy, Debug)]
pub struct Config {
    pub max_connections: usize,
    pub max_request_head_bytes: usize,
    pub header_read_timeout: Duration,
    /// Drain deadline; zero cancels remaining connections immediately.
    pub shutdown_grace: Duration,
    pub tcp_nodelay: bool,
}
impl Default for Config {
    fn default() -> Self {
        Self {
            max_connections: 65_536,
            max_request_head_bytes: 32 * 1024,
            header_read_timeout: Duration::from_secs(15),
            shutdown_grace: Duration::from_secs(10),
            tcp_nodelay: true,
        }
    }
}
impl Config {
    pub(crate) fn validate(&self) -> io::Result<()> {
        if self.max_connections == 0
            || self.max_connections > Semaphore::MAX_PERMITS
            || u32::try_from(self.max_connections).is_err()
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "max_connections must fit a nonzero semaphore count",
            ));
        }
        if self.max_request_head_bytes < 8192 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "max_request_head_bytes must be at least 8192",
            ));
        }
        if self.header_read_timeout.is_zero() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "header_read_timeout must be nonzero",
            ));
        }
        Ok(())
    }
}

/// Serve HTTP services with common upgrade support. A permit lives inside the
/// socket, so upgrading cannot evade the connection limit. Shutdown first
/// drains HTTP and protocol sessions, then cancels all remaining socket I/O.
pub async fn serve<M, S, B, F>(
    listener: TcpListener,
    make_service: M,
    shutdown: F,
    config: Config,
) -> io::Result<()>
where
    M: Fn(SocketAddr) -> S + Send + Sync + 'static,
    S: Service<Request<Incoming>, Response = Response<B>> + Send + 'static,
    S::Future: Send,
    S::Error: Into<BoxError>,
    B: Body<Data = Bytes> + Send + 'static,
    B::Error: Into<BoxError>,
    F: Future<Output = ()>,
{
    config.validate()?;
    let limit = Arc::new(Semaphore::new(config.max_connections));
    let (drain, draining) = watch::channel(false);
    let (cancel, cancelled) = watch::channel(false);
    let mut tasks = JoinSet::new();
    tokio::pin!(shutdown);
    let result = loop {
        tokio::select! {
            _ = &mut shutdown => break Ok(()),
            accepted = listener.accept() => {
                let (stream, peer) = match accepted { Ok(pair) => pair, Err(e) => break Err(e) };
                let Ok(permit) = limit.clone().try_acquire_owned() else { continue; };
                if let Err(error) = stream.set_nodelay(config.tcp_nodelay) { tracing::debug!(%peer, %error, "could not configure HTTP connection"); continue; }
                let service = make_service(peer);
                let mut draining = draining.clone();
                let mut cancelled = cancelled.clone();
                let io = ManagedIo { stopped: false, stream, _permit: permit, cancelled: Box::pin(async move { let _ = cancelled.changed().await; }) };
                tasks.spawn(async move {
                    let upgrades = UpgradeTasks::default();
                    let request_upgrades = upgrades.clone();
                    let service = service_fn(move |mut request: Request<Incoming>| {
                        request.extensions_mut().insert(PeerAddr(peer));
                        request.extensions_mut().insert(request_upgrades.clone());
                        service.call(request)
                    });
                    let connection = http1::Builder::new().timer(TokioTimer::new())
                        .header_read_timeout(config.header_read_timeout).max_buf_size(config.max_request_head_bytes)
                        .serve_connection(TokioIo::new(io), service).with_upgrades();
                    tokio::pin!(connection);
                    let result = tokio::select! {
                        result = &mut connection => result,
                        _ = draining.changed() => { connection.as_mut().graceful_shutdown(); connection.await }
                    };
                    if let Err(error) = result { tracing::debug!(%peer, %error, "HTTP connection closed"); }
                    if let Err(error) = upgrades.finish().await { tracing::debug!(%peer, %error, "protocol task failed"); }
                });
            }
            Some(joined) = tasks.join_next(), if !tasks.is_empty() => {
                if let Err(error) = joined { tracing::warn!(%error, "HTTP connection task failed"); }
            }
        }
    };
    drop(listener);
    let _ = drain.send(true);
    // Detached protocol tasks still own ManagedIo and therefore its permit.
    let drained = tokio::time::timeout(config.shutdown_grace, async {
        let _all = limit.acquire_many(config.max_connections as u32).await;
        while tasks.join_next().await.is_some() {}
    })
    .await;
    if drained.is_err() {
        let _ = cancel.send(true);
        tasks.abort_all();
        while tasks.join_next().await.is_some() {}
    }
    result
}

struct ManagedIo {
    stopped: bool,
    stream: TcpStream,
    _permit: OwnedSemaphorePermit,
    cancelled: Pin<Box<dyn Future<Output = ()> + Send>>,
}
impl ManagedIo {
    fn is_cancelled(&mut self, cx: &mut Context<'_>) -> bool {
        if !self.stopped {
            self.stopped = self.cancelled.as_mut().poll(cx).is_ready();
        }
        self.stopped
    }
}
impl AsyncRead for ManagedIo {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        if self.is_cancelled(cx) {
            return Poll::Ready(Ok(()));
        }
        Pin::new(&mut self.stream).poll_read(cx, buf)
    }
}
impl AsyncWrite for ManagedIo {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        if self.is_cancelled(cx) {
            return Poll::Ready(Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "server stopped",
            )));
        }
        Pin::new(&mut self.stream).poll_write(cx, buf)
    }
    fn is_write_vectored(&self) -> bool {
        self.stream.is_write_vectored()
    }
    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buffers: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        if self.is_cancelled(cx) {
            return Poll::Ready(Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "server stopped",
            )));
        }
        Pin::new(&mut self.stream).poll_write_vectored(cx, buffers)
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        if self.is_cancelled(cx) {
            return Poll::Ready(Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "server stopped",
            )));
        }
        Pin::new(&mut self.stream).poll_flush(cx)
    }
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_shutdown(cx)
    }
}
