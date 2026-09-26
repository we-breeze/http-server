//! Arena-backed receive path. Ownership stays outside the cancellable handler.
use std::net::SocketAddr;

use brz_io::{Reader, ReaderView};
use tokio::net::TcpStream;
use tokio::sync::OwnedSemaphorePermit;

use super::{
    Handler, Header, Inspection, MAX_REQUEST_HEADERS, Request, RequestFailure, RequestLimits,
    Response, StatusCode, inspect_parsed_request, map_parse_error, reserve_request_body,
};

// Fields drop in declaration order, including when the connection task is
// aborted: release receive storage before returning the body permits.
pub(super) struct ReceiveState {
    pub(super) input: Reader,
    pub(super) context: RequestContext,
}

pub(super) struct RequestContext {
    observation: crate::api_metrics::Observation,
    inspection: Option<Inspection>,
    complete: bool,
    // Keep the budget with receive storage through cancellation and logging.
    // The caller drops this only AFTER clearing/consuming the request storage.
    _permit: Option<OwnedSemaphorePermit>,
}

impl RequestContext {
    pub(super) fn release_budget(&mut self) {
        self._permit.take();
    }
    pub(super) fn new() -> Self {
        Self {
            observation: crate::api_metrics::Observation::new(),
            inspection: None,
            complete: false,
            _permit: None,
        }
    }
    pub(super) fn record(
        &self,
        status: StatusCode,
        length: Option<u64>,
        timeout: bool,
        input: &Reader,
    ) {
        let view = input.view();
        let head = self.inspection.map_or(&[][..], |i| {
            view.peek_bytes(0, i.head_len)
                .expect("retained complete head")
        });
        let body = self.inspection.filter(|_| self.complete).map(|i| {
            view.slice(i.head_len..i.total_len)
                .expect("retained complete body")
        });
        self.observation.record(status, length, timeout, head, body);
    }
}

/// Incremental delimiter search over segments, without repeated prefix scans.
/// Accept LF blank lines as well as CRLF, retaining httparse's line-ending policy.
#[derive(Default)]
struct HeadScanner {
    offset: usize,
    tail: u32,
    started: bool,
}
impl HeadScanner {
    fn scan(
        &mut self,
        view: ReaderView<'_>,
        limit: usize,
    ) -> Result<Option<usize>, RequestFailure> {
        while self.offset < view.len().min(limit) {
            let chunk = view.chunk_at(self.offset);
            for &byte in chunk.iter().take(limit - self.offset) {
                self.tail = (self.tail << 8) | u32::from(byte);
                self.offset += 1;
                // Ignore leading empty lines; httparse may accept them.
                if !matches!(byte, b'\r' | b'\n') {
                    self.started = true;
                }
                if self.started
                    && (self.tail & 0xffff == 0x0a0a || self.tail & 0xffffff == 0x0a0d0a)
                {
                    return Ok(Some(self.offset));
                }
            }
        }
        if self.offset >= limit {
            Err(RequestFailure::HeadersTooLarge)
        } else {
            Ok(None)
        }
    }
}

// Preserve httparse's early rejection of malformed, incomplete headers. Most
// partial heads fit in the first segment and can be checked without copying.
// For a fragmented larger head, use one temporary bounded allocation: retaining
// every growing prefix in Reader's range cache would make live memory quadratic.
fn validate_partial_head(
    input: &Reader,
    arena: &crate::EphemeralBytesArena,
) -> Result<(), RequestFailure> {
    let view = input.view();
    let first = view.chunk_at(0);
    let mut scratch;
    let head = if first.len() == view.len() {
        first
    } else {
        scratch = arena.alloc(view.len());
        let mut offset = 0;
        while offset < view.len() {
            let bytes = view.chunk_at(offset);
            scratch.extend_from_slice(bytes);
            offset += bytes.len();
        }
        scratch.as_slice()
    };
    let mut headers = [httparse::EMPTY_HEADER; MAX_REQUEST_HEADERS];
    httparse::Request::new(&mut headers)
        .parse(head)
        .map_err(map_parse_error)?;
    Ok(())
}

pub(super) async fn receive_and_handle<H, A>(
    stream: &mut TcpStream,
    input: &mut Reader,
    peer_addr: SocketAddr,
    handler: &H,
    authenticator: &A,
    limits: RequestLimits<'_>,
    context: &mut RequestContext,
) -> Result<(Response, usize, bool), RequestFailure>
where
    H: Handler<A>,
    A: Send + Sync + 'static,
{
    let config = limits.config;
    let mut scanner = HeadScanner::default();
    let head_len = loop {
        if let Some(length) = scanner.scan(input.view(), config.max_request_head_bytes)? {
            break length;
        }
        if !input.is_empty() {
            validate_partial_head(input, &config.arena)?;
        }
        // Avoid giving a totally idle connection an arena ticket. Readiness can
        // be spurious; at most a normal read/timeout then retains the segment.
        if input.is_empty() {
            stream.readable().await.map_err(RequestFailure::Io)?;
        }
        let maximum = config.max_request_head_bytes - input.len();
        let count = input
            .read_from(stream, maximum)
            .await
            .map_err(RequestFailure::Io)?;
        if count == 0 {
            return Err(RequestFailure::Closed);
        }
    };
    // Header-only coalescing, if needed. Range cache survives later appends.
    // All borrowed descriptors/captures are scoped away BEFORE &mut IO resumes.
    let inspection = {
        let head = input
            .view()
            .peek_bytes(0, head_len)
            .map_err(RequestFailure::Io)?;
        let mut headers = [httparse::EMPTY_HEADER; MAX_REQUEST_HEADERS];
        let mut parsed = httparse::Request::new(&mut headers);
        let httparse::Status::Complete(length) = parsed.parse(head).map_err(map_parse_error)?
        else {
            return Err(RequestFailure::BadRequest);
        };
        let inspection = inspect_parsed_request(&parsed, length, config)?;
        if length != head_len {
            return Err(RequestFailure::BadRequest);
        }
        let method = parsed.method.ok_or(RequestFailure::BadRequest)?;
        let target = parsed.path.ok_or(RequestFailure::BadRequest)?;
        let path = target.split_once('?').map_or(target, |(path, _)| path);
        let prepared = handler.prepare(path, method);
        context.observation.matched(prepared.metrics());
        context.observation.set_api_log(prepared.api_log());
        context
            .observation
            .request_head(head, &parsed, peer_addr, inspection.total_len - head_len);
        inspection
    };
    context.inspection = Some(inspection);
    context._permit =
        reserve_request_body(limits.body_limit, inspection.total_len - head_len).await?;
    let remaining = inspection.total_len.saturating_sub(input.len());
    if remaining != 0 && remaining <= config.max_preallocated_request_body_bytes {
        // Contiguous extra tail; never move the prefix and never read the next
        // request while completing this Body. Larger payloads grow in segments.
        input.reserve_exact(remaining).map_err(RequestFailure::Io)?;
    }
    while input.len() < inspection.total_len {
        let maximum = inspection.total_len - input.len();
        if input
            .read_from(stream, maximum)
            .await
            .map_err(RequestFailure::Io)?
            == 0
        {
            return Err(RequestFailure::Closed);
        }
    }
    context.complete = true;

    let head = input
        .view()
        .peek_bytes(0, head_len)
        .map_err(RequestFailure::Io)?;
    let mut header_storage = [httparse::EMPTY_HEADER; MAX_REQUEST_HEADERS];
    let mut parsed = httparse::Request::new(&mut header_storage);
    if !parsed.parse(head).map_err(map_parse_error)?.is_complete() {
        return Err(RequestFailure::BadRequest);
    }
    let method = parsed.method.ok_or(RequestFailure::BadRequest)?;
    let target = parsed.path.ok_or(RequestFailure::BadRequest)?;
    let path = target.split_once('?').map_or(target, |(path, _)| path);
    // Re-resolve to get borrowed captures after receive mutation. Metrics' start
    // time is NOT reset. This costs an extra route lookup (see the handoff notes).
    let prepared = handler.prepare(path, method);
    let mut request_headers = [Header {
        name: "",
        value: &[],
    }; MAX_REQUEST_HEADERS];
    for (to, from) in request_headers.iter_mut().zip(parsed.headers.iter()) {
        *to = Header {
            name: from.name,
            value: from.value,
        };
    }
    let body = input
        .view()
        .slice(head_len..inspection.total_len)
        .map_err(RequestFailure::Io)?;
    let mut request = Request::new(
        method,
        target,
        &request_headers[..parsed.headers.len()],
        input,
        peer_addr,
        &config.arena,
        #[cfg(feature = "api-log")]
        context.observation.api_log_context(),
    )
    .with_body_view(body);
    request.rejection_handler = config.rejection_handler;
    let origin = request.header("origin");
    let preflight = config
        .cors
        .as_ref()
        .and_then(|cors| cors.preflight(&request));
    let mut response = if let Some(response) = preflight {
        response
    } else {
        let response = handler
            .call_prepared(request, authenticator, &prepared)
            .await;
        if let Some(cors) = &config.cors {
            cors.apply(origin, response, &config.arena)
        } else {
            response
        }
    };
    if method == "HEAD" {
        response.suppress_body();
    }
    if response.status().as_u16() > 599 {
        return Ok((
            Response::empty(StatusCode::INTERNAL_SERVER_ERROR).close(),
            inspection.total_len,
            false,
        ));
    }
    Ok((response, inspection.total_len, inspection.keep_alive))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn scanner_handles_every_boundary_lf_and_pipelining() {
        for raw in [
            &b"GET / HTTP/1.1\r\nHost: a\r\n\r\nBODY"[..],
            &b"GET / HTTP/1.1\nHost: a\n\nBODY"[..],
            &b"\r\n\r\nGET / HTTP/1.1\r\n\r\nBODY"[..],
        ] {
            for first in 1..raw.len() {
                let arena = crate::EphemeralBytesArena::new(4096);
                let mut writer = brz_io::Writer::with_initial_segment_size(&arena, first);
                writer.write_all(raw).unwrap();
                let reader = writer.into_reader();
                let mut scanner = HeadScanner::default();
                let end = scanner.scan(reader.view(), raw.len()).unwrap().unwrap();
                assert_eq!(end, raw.len() - 4);
                let mut headers = [httparse::EMPTY_HEADER; 4];
                let mut parsed = httparse::Request::new(&mut headers);
                assert!(
                    parsed
                        .parse(reader.view().peek_bytes(0, end).unwrap())
                        .unwrap()
                        .is_complete()
                );
            }
        }
    }

    #[test]
    fn scanner_enforces_header_limit() {
        let arena = crate::EphemeralBytesArena::new(4096);
        let mut writer = brz_io::Writer::new(&arena);
        writer
            .write_all(b"GET / HTTP/1.1\r\nHost: unfinished")
            .unwrap();
        let input = writer.into_reader();
        assert!(matches!(
            HeadScanner::default().scan(input.view(), 8),
            Err(RequestFailure::HeadersTooLarge)
        ));
    }
}

#[cfg(test)]
mod cancellation_tests {
    use super::*;
    use std::io::Write;
    use std::sync::Arc;
    use std::time::Duration;
    use tokio::net::TcpListener;
    use tokio::sync::Semaphore;

    struct NeverCompletes;
    impl Handler for NeverCompletes {
        async fn call(&self, request: Request<'_>, _: &crate::NoAuthenticator) -> Response {
            let body = request.body_view();
            assert_eq!(body.as_slice(), b"abc");
            std::future::pending::<()>().await;
            assert_eq!(body.as_slice(), b"abc");
            Response::empty(StatusCode::OK)
        }
    }

    #[tokio::test]
    async fn cancellation_keeps_received_body_and_budget_until_recording() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let (client, accepted) = tokio::join!(
            TcpStream::connect(listener.local_addr().unwrap()),
            listener.accept()
        );
        let _client = client.unwrap();
        let (mut stream, peer) = accepted.unwrap();
        let config = super::super::ServerConfig::default();
        let mut writer = brz_io::Writer::new(&config.arena);
        writer
            .write_all(b"POST / HTTP/1.1\r\nContent-Length: 3\r\n\r\nabc")
            .unwrap();
        let mut input = writer.into_reader();
        let budget = Arc::new(Semaphore::new(100));
        let mut context = RequestContext::new();
        let result = tokio::time::timeout(
            Duration::from_millis(10),
            receive_and_handle(
                &mut stream,
                &mut input,
                peer,
                &NeverCompletes,
                &crate::NoAuthenticator,
                RequestLimits {
                    config: &config,
                    body_limit: &budget,
                },
                &mut context,
            ),
        )
        .await;
        assert!(result.is_err());
        assert!(context.complete);
        assert_eq!(
            budget.available_permits(),
            97,
            "budget must outlive cancelled handler"
        );
        context.record(StatusCode::REQUEST_TIMEOUT, Some(0), true, &input);
        let i = context.inspection.unwrap();
        assert_eq!(
            input
                .view()
                .slice(i.head_len..i.total_len)
                .unwrap()
                .as_slice(),
            b"abc"
        );
        input.clear();
        drop(context);
        assert_eq!(budget.available_permits(), 100);
    }
}

#[cfg(test)]
mod compatibility_tests {
    use super::*;
    use std::io::Write;

    // Compare the framing scanner with the original httparse-based inspection
    // on valid and mutated complete messages, including every receive split.
    #[test]
    fn scanner_agrees_with_httparse_on_generated_complete_requests() {
        let seeds = [
            &b"GET / HTTP/1.1\r\nHost: x\r\n\r\n"[..],
            &b"\r\nGET /x?q=y HTTP/1.1\nHost: x\n\n"[..],
            &b"POST / HTTP/1.1\r\nContent-Length: 0\r\nContent-Length: 0\r\n\r\n"[..],
        ];
        let config = super::super::ServerConfig::default();
        for seed in seeds {
            for mutation in 0..seed.len() * 8 {
                let mut raw = seed.to_vec();
                let index = mutation / 8;
                raw[index] ^= 1 << (mutation % 8);
                // Ensure framing bytes exist even after mutating the delimiter.
                raw.extend_from_slice(b"\r\n\r\n");
                let mut old_headers = [httparse::EMPTY_HEADER; MAX_REQUEST_HEADERS];
                let mut old = httparse::Request::new(&mut old_headers);
                let expected = match old.parse(&raw) {
                    Ok(httparse::Status::Complete(end)) => {
                        inspect_parsed_request(&old, end, &config).map(|_| end).ok()
                    }
                    _ => None,
                };
                for initial in [1, 2, 7, 2048] {
                    let arena = crate::EphemeralBytesArena::new(4096);
                    let mut writer = brz_io::Writer::with_initial_segment_size(&arena, initial);
                    writer.write_all(&raw).unwrap();
                    let input = writer.into_reader();
                    let end = HeadScanner::default()
                        .scan(input.view(), config.max_request_head_bytes)
                        .unwrap();
                    let actual = end.and_then(|end| {
                        let head = input.peek_bytes(0, end).unwrap();
                        let mut headers = [httparse::EMPTY_HEADER; MAX_REQUEST_HEADERS];
                        let mut parsed = httparse::Request::new(&mut headers);
                        match parsed.parse(head) {
                            Ok(httparse::Status::Complete(length)) if length == end => {
                                inspect_parsed_request(&parsed, length, &config)
                                    .map(|_| end)
                                    .ok()
                            }
                            _ => None,
                        }
                    });
                    assert_eq!(actual, expected, "input={raw:?}, initial={initial}");
                }
            }
        }
    }

    #[test]
    fn incomplete_header_errors_match_httparse_before_waiting_for_more_bytes() {
        for raw in [
            &b"\0"[..],
            &b"GET / HTTP/1.1\r\nBad Header: x\r\n"[..],
            &b"GET / HTTP/1.1\r\nGood: x\r\nBad\0"[..],
            &b"POST / HTTP/1.1\r\nGood: partial"[..],
        ] {
            let mut headers = [httparse::EMPTY_HEADER; MAX_REQUEST_HEADERS];
            let expected = httparse::Request::new(&mut headers).parse(raw).is_err();
            for initial in [2, 2048] {
                let arena = crate::EphemeralBytesArena::new(4096);
                let mut writer = brz_io::Writer::with_initial_segment_size(&arena, initial);
                writer.write_all(raw).unwrap();
                let reader = writer.into_reader();
                assert_eq!(validate_partial_head(&reader, &arena).is_err(), expected);
                assert_eq!(reader.position(), 0);
            }
        }
    }

    #[tokio::test]
    async fn aborted_connection_releases_storage_and_body_budget() {
        let arena = crate::EphemeralBytesArena::new(64);
        let mut writer = brz_io::Writer::with_initial_segment_size(&arena, 64);
        writer.write_all(&[1; 64]).unwrap();
        let blocker = arena.alloc(64);
        assert!(arena.alloc(1).is_heap_allocated()); // freeze both chunks
        let budget = std::sync::Arc::new(tokio::sync::Semaphore::new(64));
        let mut context = RequestContext::new();
        context._permit = Some(budget.clone().acquire_many_owned(64).await.unwrap());
        let state = ReceiveState {
            input: writer.into_reader(),
            context,
        };
        let (started, ready) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(async move {
            let state = state;
            started.send(()).unwrap();
            std::future::pending::<()>().await;
            drop(state);
        });
        ready.await.unwrap();
        assert_eq!(budget.available_permits(), 0);
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        assert_eq!(budget.available_permits(), 64);
        assert!(
            !arena.alloc(64).is_heap_allocated(),
            "aborted input released its arena ticket"
        );
        drop(blocker);
    }
}
