use std::io::{self, IoSlice};
use std::time::Duration;

use tokio::io::AsyncWriteExt;
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
    if !response.sends_body() {
        return tokio::time::timeout(idle_timeout, socket.write_all(head.as_ref())).await?;
    }
    if let ResponseBody::Segmented(body) = response.body_mut() {
        return tokio::time::timeout(idle_timeout, async {
            socket.write_all(head.as_ref()).await?;
            tokio::io::copy_buf(body, socket).await?;
            Ok(())
        })
        .await?;
    }
    let length = response.body().content_length();
    let ResponseBody::Stream(body) = response.body_mut() else {
        return tokio::time::timeout(
            idle_timeout,
            write_all_vectored(socket, head.as_ref(), response.body().as_slice()),
        )
        .await?;
    };
    tokio::time::timeout(idle_timeout, socket.write_all(head.as_ref())).await??;
    let mut sent = 0_u64;
    loop {
        let finished = tokio::time::timeout(idle_timeout, async {
            // Empty upstream chunks are ignored without resetting the idle timer.
            let chunk = loop {
                match body.next().await? {
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
                let framing = format!("{:x}\r\n", chunk.len());
                write_all_vectored(socket, framing.as_bytes(), &chunk).await?;
                socket.write_all(b"\r\n").await?;
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
