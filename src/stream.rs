use std::io;

use bytes::Bytes;
use futures_util::{Stream, StreamExt};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;

use crate::reply::{IntoHttpResponse, kind};
use crate::{EphemeralBytesArena, Response, ResponseBody, StatusCode};

/// An owned response stream with bounded read-ahead and cancellation on drop.
/// Constructed automatically when an API method returns a byte stream.
#[derive(Debug)]
pub struct ResponseStream {
    receiver: mpsc::Receiver<io::Result<Bytes>>,
    producer: JoinHandle<()>,
    start: Option<oneshot::Sender<()>>,
    pub(crate) content_length: Option<u64>,
}

impl ResponseStream {
    fn new<S, E>(stream: S) -> Self
    where
        S: Stream<Item = Result<Bytes, E>> + Send + 'static,
        E: std::fmt::Display + Send + 'static,
    {
        // At most one queued chunk and one chunk being written. Reserve a
        // queue slot before polling upstream so a slow client applies backpressure.
        let (sender, receiver) = mpsc::channel(1);
        let (start, started) = oneshot::channel();
        let producer = tokio::spawn(async move {
            if started.await.is_err() {
                return;
            }
            tokio::pin!(stream);
            while let Ok(slot) = sender.reserve().await {
                let Some(chunk) = stream.next().await else {
                    break;
                };
                let failed = chunk.is_err();
                slot.send(chunk.map_err(|error| io::Error::other(error.to_string())));
                if failed {
                    break;
                }
            }
        });
        Self {
            receiver,
            producer,
            start: Some(start),
            content_length: None,
        }
    }

    pub(crate) async fn next(&mut self) -> io::Result<Option<Bytes>> {
        if let Some(start) = self.start.take() {
            let _ = start.send(());
        }
        if let Some(chunk) = self.receiver.recv().await {
            chunk.map(Some)
        } else {
            (&mut self.producer).await.map_err(io::Error::other)?;
            Ok(None)
        }
    }
}

impl Drop for ResponseStream {
    fn drop(&mut self) {
        self.producer.abort();
    }
}

impl<S, E> IntoHttpResponse<kind::Stream> for S
where
    S: Stream<Item = Result<Bytes, E>> + Send + 'static,
    E: std::fmt::Display + Send + 'static,
{
    fn into_http_response(self, _arena: &EphemeralBytesArena) -> Response {
        Response::new(
            StatusCode::OK,
            ResponseBody::Stream(ResponseStream::new(self)),
        )
        .content_type("application/octet-stream")
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use futures_util::stream;
    use tokio::sync::Notify;

    use super::ResponseStream;

    #[tokio::test]
    async fn producer_polls_only_after_start_and_queues_at_most_one_chunk() {
        let polls = Arc::new(AtomicUsize::new(0));
        let ready = Arc::new(Notify::new());
        let stream_polls = polls.clone();
        let stream_ready = ready.clone();
        let upstream = stream::repeat_with(move || {
            stream_polls.fetch_add(1, Ordering::SeqCst);
            stream_ready.notify_one();
            Ok::<_, std::io::Error>(bytes::Bytes::from_static(b"data"))
        });
        let mut response = ResponseStream::new(upstream);
        tokio::task::yield_now().await;
        assert_eq!(polls.load(Ordering::SeqCst), 0);
        assert_eq!(response.next().await.unwrap().unwrap(), &b"data"[..]);
        tokio::time::timeout(Duration::from_secs(2), async {
            while polls.load(Ordering::SeqCst) < 2 {
                ready.notified().await;
            }
        })
        .await
        .unwrap();
        tokio::task::yield_now().await;
        assert_eq!(polls.load(Ordering::SeqCst), 2);
    }
}
