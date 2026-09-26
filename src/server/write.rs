use std::io::{self, IoSlice};
use std::time::Duration;

use tokio::io::{AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;

use crate::{EphemeralBytes, EphemeralBytesArena, Response, ResponseBody};

pub(super) async fn write_response(
    socket: &mut TcpStream,
    arena: &EphemeralBytesArena,
    mut response: Response,
    close: bool,
    idle_timeout: Duration,
) -> io::Result<()> {
    let head = encode_response_head(arena, &response, close);
    let sends_body = response.sends_body();
    let mut body = std::mem::replace(response.body_mut(), ResponseBody::Empty);
    // The encoded head owns the metadata now. Do not pin the original custom
    // HeaderBlock (and its arena ticket) throughout a long download.
    drop(response);
    if !sends_body {
        drop(body);
        return tokio::time::timeout(idle_timeout, socket.write_all(head.as_ref())).await?;
    }
    if let ResponseBody::Segmented(reader) = &mut body {
        return tokio::time::timeout(idle_timeout, async {
            let first = std::io::BufRead::fill_buf(reader)?;
            write_all_vectored(socket, [head.as_ref(), first]).await?;
            let written = first.len();
            std::io::BufRead::consume(reader, written);
            drop(head);
            // Reader lends each subsequent segment; copy_buf does not merge it.
            tokio::io::copy_buf(reader, socket).await?;
            Ok(())
        })
        .await?;
    }
    let length = body.content_length();
    let ResponseBody::Stream(stream) = &mut body else {
        return tokio::time::timeout(
            idle_timeout,
            write_all_vectored(socket, [head.as_ref(), body.as_slice()]),
        )
        .await?;
    };
    tokio::time::timeout(idle_timeout, socket.write_all(head.as_ref())).await??;
    // With ordinary socket writes it is safe to reuse these bytes now.
    // This must be revisited if kernel MSG_ZEROCOPY is ever introduced.
    drop(head);
    let mut sent = 0_u64;
    loop {
        let finished = tokio::time::timeout(idle_timeout, async {
            // Empty upstream chunks are ignored without resetting the idle timer.
            let chunk = loop {
                match stream.next().await? {
                    Some(chunk) if chunk.is_empty() => {}
                    chunk => break chunk,
                }
            };
            let Some(chunk) = chunk else {
                if let Some(length) = length {
                    if sent != length {
                        return Err(io::Error::new(
                            io::ErrorKind::UnexpectedEof,
                            "download ended before Content-Length",
                        ));
                    }
                } else {
                    socket.write_all(b"0\r\n\r\n").await?;
                }
                return Ok(true);
            };
            let next = sent
                .checked_add(chunk.len() as u64)
                .ok_or_else(|| io::Error::other("download length overflow"))?;
            if length.is_some_and(|length| next > length) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "download exceeded Content-Length",
                ));
            }
            if length.is_none() {
                let mut framing = [0; CHUNK_HEAD_BYTES];
                let framing = encode_chunk_head(chunk.len(), &mut framing);
                write_all_vectored(socket, [framing, chunk.as_ref(), &b"\r\n"[..]]).await?;
            } else {
                socket.write_all(&chunk).await?;
            }
            sent = next;
            Ok(false)
        })
        .await??;
        if finished {
            return Ok(());
        }
    }
}

fn encode_response_head(
    arena: &EphemeralBytesArena,
    response: &Response,
    close: bool,
) -> EphemeralBytes {
    let body_len = response.body().content_length();
    let chunked = response.permits_body() && body_len.is_none();
    let mut body_decimal = itoa::Buffer::new();
    let content_length = body_decimal.format(body_len.unwrap_or(0));
    let status = response.status();
    let reason = status.canonical_reason().unwrap_or("Unknown");
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
    let content_length_len = if response.permits_body() && !chunked {
        b"Content-Length: ".len() + content_length.len() + 2
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
        + content_length_len
        + connection_len
        + if chunked {
            b"Transfer-Encoding: chunked\r\n".len()
        } else {
            0
        }
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
    if response.permits_body() && !chunked {
        output.extend_from_slice(b"Content-Length: ");
        output.extend_from_slice(content_length.as_bytes());
        output.extend_from_slice(b"\r\n");
    }
    if chunked {
        output.extend_from_slice(b"Transfer-Encoding: chunked\r\n");
    }
    if close {
        output.extend_from_slice(b"Connection: close\r\n");
    }
    output.extend_from_slice(b"\r\n");
    debug_assert_eq!(output.len(), capacity);
    output.freeze()
}

const CHUNK_HEAD_BYTES: usize = 2 * std::mem::size_of::<usize>() + 2;

fn encode_chunk_head(mut length: usize, output: &mut [u8; CHUNK_HEAD_BYTES]) -> &[u8] {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut offset = CHUNK_HEAD_BYTES - 2;
    output[offset..].copy_from_slice(b"\r\n");
    loop {
        offset -= 1;
        output[offset] = HEX[length & 15];
        length >>= 4;
        if length == 0 {
            return &output[offset..];
        }
    }
}

async fn write_all_vectored<W: AsyncWrite + Unpin, const N: usize>(
    stream: &mut W,
    buffers: [&[u8]; N],
) -> io::Result<()> {
    let mut slices = buffers.map(IoSlice::new);
    let mut remaining = &mut slices[..];
    loop {
        while remaining.first().is_some_and(|slice| slice.is_empty()) {
            remaining = &mut remaining[1..];
        }
        if remaining.is_empty() {
            return Ok(());
        }
        let written = stream.write_vectored(remaining).await?;
        if written == 0 {
            return Err(io::Error::new(
                io::ErrorKind::WriteZero,
                "failed to write complete HTTP response",
            ));
        }
        IoSlice::advance_slices(&mut remaining, written);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::pin::Pin;
    use std::task::{Context, Poll};
    use tokio::io::AsyncReadExt;

    #[test]
    fn chunk_lengths_match_lower_hex_without_a_string() {
        for length in [0, 1, 15, 16, 255, 256, 8192, usize::MAX] {
            let mut output = [0; CHUNK_HEAD_BYTES];
            assert_eq!(
                encode_chunk_head(length, &mut output),
                format!("{length:x}\r\n").as_bytes()
            );
        }
    }

    #[tokio::test]
    async fn vectored_write_handles_partial_progress_and_empty_slices() {
        let (mut writer, mut reader) = tokio::io::duplex(2);
        let mut output = [0; 10];
        let write = write_all_vectored(&mut writer, [&b""[..], b"abc", b"", b"defghij", b""]);
        let read = reader.read_exact(&mut output);
        let (written, read) = tokio::join!(write, read);
        written.unwrap();
        read.unwrap();
        assert_eq!(&output, b"abcdefghij");
        write_all_vectored(&mut writer, [&b""[..]; 3])
            .await
            .unwrap();
    }

    struct ZeroWriter;
    impl AsyncWrite for ZeroWriter {
        fn poll_write(
            self: Pin<&mut Self>,
            _: &mut Context<'_>,
            _: &[u8],
        ) -> Poll<io::Result<usize>> {
            Poll::Ready(Ok(0))
        }
        fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
        fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    #[tokio::test]
    async fn a_zero_length_write_does_not_spin() {
        let error = write_all_vectored(&mut ZeroWriter, [&b"x"[..]])
            .await
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::WriteZero);
    }
}
