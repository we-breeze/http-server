//! Borrow wire fields at emission time. No request string or body snapshot is kept.

use std::fmt;
#[cfg(feature = "api-log")]
use std::net::IpAddr;
use std::net::SocketAddr;
use std::ops::Range;

const METHOD_BYTES: usize = 16;
const TARGET_BYTES: usize = 128;
#[cfg(feature = "slow-log")]
const EXCERPT_BYTES: usize = 512;

/// Offsets remain valid after the handler future is cancelled. The caller owns
/// the receive buffer until recording finishes and must not advance it earlier.
pub(super) struct RequestLog {
    method: Range<usize>,
    target: Range<usize>,
    #[cfg(feature = "api-log")]
    request_id: Option<Range<usize>>,
    #[cfg(feature = "api-log")]
    forwarded_for: Option<Range<usize>>,
    pub(super) peer: SocketAddr,
    pub(super) request_len: usize,
}

impl RequestLog {
    pub(super) fn new(
        head: &[u8],
        parsed: &httparse::Request<'_, '_>,
        peer: SocketAddr,
        request_len: usize,
        api_log: bool,
    ) -> Self {
        #[cfg(feature = "api-log")]
        let find = |name: &str| {
            if !api_log {
                return None;
            }
            parsed
                .headers
                .iter()
                .find(|header| header.name.eq_ignore_ascii_case(name))
                .map(|header| field_range(head, header.value))
        };
        #[cfg(not(feature = "api-log"))]
        let _ = api_log;
        Self {
            method: field_range(head, parsed.method.expect("complete request").as_bytes()),
            target: field_range(head, parsed.path.expect("complete request").as_bytes()),
            #[cfg(feature = "api-log")]
            request_id: find("x-request-id"),
            #[cfg(feature = "api-log")]
            forwarded_for: find("x-forwarded-for"),
            peer,
            request_len,
        }
    }

    pub(super) fn method<'a>(&self, head: &'a [u8]) -> &'a str {
        prefix(
            std::str::from_utf8(&head[self.method.clone()]).expect("parsed method"),
            METHOD_BYTES,
        )
    }

    pub(super) fn target<'a>(&self, head: &'a [u8]) -> &'a str {
        prefix(
            std::str::from_utf8(&head[self.target.clone()]).expect("parsed target"),
            TARGET_BYTES,
        )
    }

    #[cfg(feature = "api-log")]
    pub(super) fn request_id<'a>(&self, head: &'a [u8]) -> RequestId<'a> {
        RequestId(self.request_id.as_ref().map(|range| &head[range.clone()]))
    }

    #[cfg(feature = "api-log")]
    pub(super) fn client_ip<'a>(&self, head: &'a [u8]) -> ClientIp<'a> {
        ClientIp {
            forwarded_for: self
                .forwarded_for
                .as_ref()
                .map(|range| &head[range.clone()]),
            peer: self.peer.ip(),
        }
    }
}

fn field_range(head: &[u8], bytes: &[u8]) -> Range<usize> {
    // Empty values need no pointer identity; a parser may use a static empty slice.
    if bytes.is_empty() {
        return 0..0;
    }
    let start = (bytes.as_ptr() as usize)
        .checked_sub(head.as_ptr() as usize)
        .expect("log field borrows request head");
    let end = start
        .checked_add(bytes.len())
        .expect("field range overflow");
    assert!(end <= head.len(), "log field lies outside request head");
    start..end
}

fn prefix(text: &str, capacity: usize) -> &str {
    let mut end = text.len().min(capacity);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

#[cfg(feature = "api-log")]
pub(super) struct RequestId<'a>(pub(super) Option<&'a [u8]>);

#[cfg(feature = "api-log")]
impl fmt::Display for RequestId<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Some(bytes) = self.0 else {
            return f.write_str("-");
        };
        let Ok(text) = std::str::from_utf8(bytes) else {
            return f.write_str("<invalid-request-id>");
        };
        if text.chars().all(|ch| ch.is_ascii_whitespace()) {
            return f.write_str("-");
        }
        write_stripped(f, text)
    }
}

#[cfg(feature = "api-log")]
pub(super) struct ClientIp<'a> {
    pub(super) forwarded_for: Option<&'a [u8]>,
    pub(super) peer: IpAddr,
}

#[cfg(feature = "api-log")]
impl fmt::Display for ClientIp<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(bytes) = self.forwarded_for {
            let Ok(text) = std::str::from_utf8(bytes) else {
                return f.write_str("<invalid-client-ip>");
            };
            if text
                .chars()
                .any(|ch| !ch.is_ascii_whitespace() && ch != ',')
            {
                return write_stripped(f, text);
            }
        }
        write!(f, "{}", self.peer)
    }
}

#[cfg(feature = "api-log")]
fn write_stripped(f: &mut fmt::Formatter<'_>, text: &str) -> fmt::Result {
    // Write borrowed runs rather than building another String/SmolStr.
    for run in text.split(|ch: char| ch.is_ascii_whitespace()) {
        f.write_str(run)?;
    }
    Ok(())
}

/// Only inspected when a slow event is actually formatted. Never calls
/// Reader::as_slice/peek_bytes, so even a cross-segment excerpt cannot merge or
/// allocate. Four scratch bytes are enough to format one Unicode scalar.
#[cfg(feature = "slow-log")]
pub(super) struct BodyExcerpt<'a>(pub(super) Option<brz_io::ReaderView<'a>>);

#[cfg(feature = "slow-log")]
impl fmt::Display for BodyExcerpt<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Some(body) = self.0 else {
            return Ok(());
        };
        let view = body;
        let source = (0..view.len().min(EXCERPT_BYTES)).map(|index| {
            view.peek_byte(index)
                .expect("bounded immutable reader view")
        });
        write_lossy_prefix(f, source, EXCERPT_BYTES)
    }
}

#[cfg(feature = "slow-log")]
fn write_lossy_prefix(
    output: &mut impl fmt::Write,
    source: impl Iterator<Item = u8>,
    mut capacity: usize,
) -> fmt::Result {
    let mut source = source.peekable();
    while let Some(first) = source.next() {
        let width = match first {
            0x00..=0x7f => 1,
            0xc2..=0xdf => 2,
            0xe0..=0xef => 3,
            0xf0..=0xf4 => 4,
            _ => 0,
        };
        let mut unit = [first, 0, 0, 0];
        let mut valid = width != 0;
        for (index, destination) in unit.iter_mut().enumerate().take(width).skip(1) {
            let Some(&byte) = source.peek() else {
                valid = false;
                break;
            };
            let (low, high) = match (first, index) {
                (0xe0, 1) => (0xa0, 0xbf),
                (0xed, 1) => (0x80, 0x9f),
                (0xf0, 1) => (0x90, 0xbf),
                (0xf4, 1) => (0x80, 0x8f),
                _ => (0x80, 0xbf),
            };
            if !(low..=high).contains(&byte) {
                // Leave the offending byte for the next lossy scalar, matching
                // String::from_utf8_lossy on invalid and incomplete sequences.
                valid = false;
                break;
            }
            *destination = source.next().expect("peeked continuation");
        }
        let text = if valid {
            std::str::from_utf8(&unit[..width]).expect("validated Unicode scalar")
        } else {
            "\u{fffd}"
        };
        if text.len() > capacity {
            break;
        }
        output.write_str(text)?;
        capacity -= text.len();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefixes_do_not_split_unicode() {
        assert_eq!(prefix("ab€€", 4), "ab");
        assert_eq!(prefix("ab€€", 5), "ab€");
        assert_eq!(prefix("abcdefghij", 8), "abcdefgh");
    }

    #[cfg(feature = "api-log")]
    #[test]
    fn request_id_preserves_existing_placeholders_and_sanitizing() {
        for value in [None, Some(&b""[..]), Some(&b" \t"[..])] {
            assert_eq!(RequestId(value).to_string(), "-");
        }
        assert_eq!(RequestId(Some(b"req 123")).to_string(), "req123");
        assert_eq!(
            RequestId(Some(b"\xff\xfe")).to_string(),
            "<invalid-request-id>"
        );
        let long = "请求-1".repeat(100);
        assert_eq!(RequestId(Some(long.as_bytes())).to_string(), long);
    }

    #[cfg(feature = "api-log")]
    #[test]
    fn client_ip_keeps_all_hops_and_falls_back_to_peer() {
        let peer = "203.0.113.7".parse().unwrap();
        let format = |value| {
            ClientIp {
                forwarded_for: value,
                peer,
            }
            .to_string()
        };
        for value in [None, Some(&b""[..]), Some(&b" , \t"[..])] {
            assert_eq!(format(value), "203.0.113.7");
        }
        assert_eq!(format(Some(b"\xff")), "<invalid-client-ip>");
        assert_eq!(
            format(Some(b"198.51.100.4,\t203.0.113.9")),
            "198.51.100.4,203.0.113.9"
        );
        let chain = format!("{}203.0.113.9", "1.1.1.1,".repeat(30));
        assert_eq!(format(Some(chain.as_bytes())), chain);
    }

    #[cfg(feature = "slow-log")]
    fn old_excerpt(bytes: &[u8], capacity: usize) -> String {
        let text = String::from_utf8_lossy(&bytes[..bytes.len().min(capacity)]);
        prefix(&text, capacity).to_owned()
    }

    #[cfg(feature = "slow-log")]
    #[test]
    fn lossy_formatter_matches_original_for_all_two_byte_inputs() {
        for first in 0..=255_u8 {
            for second in 0..=255_u8 {
                let bytes = [first, second];
                for capacity in [1, 2, 3, 4, 6] {
                    let mut actual = String::new();
                    write_lossy_prefix(&mut actual, bytes.into_iter().take(capacity), capacity)
                        .unwrap();
                    assert_eq!(
                        actual,
                        old_excerpt(&bytes, capacity),
                        "{bytes:?}, cap {capacity}"
                    );
                }
            }
        }
    }

    #[cfg(feature = "slow-log")]
    #[test]
    fn segmented_body_utf8_and_source_output_limits_match_original() {
        use std::io::Write;
        let mut bytes = "a€😀".repeat(100).into_bytes();
        bytes.extend_from_slice(b"\xf0\x90\x80\xff\xed\xa0\x80");
        for chunk in [1, 2, 3, 7, 64, 4096] {
            let arena = brz_ds::EphemeralBytesArena::new(chunk);
            let mut writer = brz_io::Writer::new(&arena);
            writer.write_all(&bytes).unwrap();
            let body = writer.into_reader();
            assert_eq!(
                BodyExcerpt(Some(body.view())).to_string(),
                old_excerpt(&bytes, EXCERPT_BYTES)
            );
            assert_eq!(body.position(), 0, "logging must not consume the reader");
        }
        assert_eq!(BodyExcerpt(None).to_string(), "");
    }
}
