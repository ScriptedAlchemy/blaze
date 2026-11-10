//! A blocking client of the cache server (`blaze-serve`) for what an agent moves: a pack by
//! digest and a unit's published list of packs. One request per connection, each bounded by
//! `timeout`; the server checks every blob against its name before it is visible, and the
//! agent checks it again against what was committed ([`crate::pack::read`]).

use std::io::{Read as _, Write as _};
use std::net::{SocketAddr, TcpStream};
use std::time::Duration;

use crate::Error;
use crate::pack::{Pack, hex};
use blaze_wire::{Digest, Limits};
use sha2::{Digest as _, Sha256};

/// Extra room for HTTP framing (including chunked transfers), not for value bytes.
const HTTP_FRAMING_ROOM: u64 = 16 << 20;
/// A peer cannot send unbounded headers even when the blob itself is large.
const HTTP_HEADER_LIMIT: usize = 64 << 10;

/// Only a successful pack GET may carry a documented blob. A missing
/// pack, an HTTP error, a manifest or a PUT reply has the smaller
/// request-body budget: an error response is never a multi-GiB blob.
fn response_body_limit(method: &str, path: &str, status: u16) -> u64 {
    if method == "GET" && status == 200 && path.starts_with("/v1/blob/") {
        Limits::DOCUMENTED.blob
    } else {
        u64::from(Limits::DOCUMENTED.request)
    }
}

/// Where packs are kept.
#[derive(Clone, Debug)]
pub struct Blobs {
    /// The server's address.
    pub address: SocketAddr,
    /// How long one read or write of a request may stall.
    pub timeout: Duration,
}

impl Blobs {
    fn request(&self, method: &str, path: &str, body: &[u8]) -> Result<(u16, Vec<u8>), Error> {
        self.request_matching(method, path, body, None)
    }

    fn request_matching(
        &self,
        method: &str,
        path: &str,
        body: &[u8],
        previous: Option<&Digest>,
    ) -> Result<(u16, Vec<u8>), Error> {
        let mut stream = TcpStream::connect_timeout(&self.address, self.timeout)?;
        stream.set_read_timeout(Some(self.timeout))?;
        stream.set_write_timeout(Some(self.timeout))?;
        stream.set_nodelay(true)?;
        let condition =
            previous.map(|digest| format!("If-Match: \"{}\"\r\n", hex(digest))).unwrap_or_default();
        let head = format!(
            "{method} {path} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\nContent-Length: {}\r\n{condition}\r\n",
            self.address,
            body.len()
        );
        stream.write_all(head.as_bytes())?;
        stream.write_all(body)?;
        stream.flush()?;
        read_response(&mut stream, method, path)
    }

    /// Puts `pack` under its name. The server hashes it before it is visible, so a pack that
    /// is there is whole.
    pub fn put_pack(&self, pack: &Pack) -> Result<(), Error> {
        let path = format!("/v1/blob/packs/{}.pack", hex(&pack.reference.pack));
        let (status, body) = self.request("PUT", &path, &pack.bytes)?;
        match status {
            200..=299 => Ok(()),
            _ => Err(Error::Protocol(format!(
                "PUT {path}: {status}: {}",
                String::from_utf8_lossy(&body)
            ))),
        }
    }

    /// The bytes of the pack named `digest`; `None` if the server has none.
    pub fn pack(&self, digest: &Digest) -> Result<Option<Vec<u8>>, Error> {
        let path = format!("/v1/blob/packs/{}.pack", hex(digest));
        let (status, body) = self.request("GET", &path, &[])?;
        match status {
            200 => Ok(Some(body)),
            404 => Ok(None),
            _ => Err(Error::Protocol(format!(
                "GET {path}: {status}: {}",
                String::from_utf8_lossy(&body)
            ))),
        }
    }

    /// The published list of the unit named `spec`; `None` if nobody published it.
    pub fn published(&self, spec: &Digest) -> Result<Option<Vec<u8>>, Error> {
        let path = format!("/v1/list/manifests/{}", hex(spec));
        let (status, body) = self.request("GET", &path, &[])?;
        match status {
            200 => Ok(Some(body)),
            404 => Ok(None),
            _ => Err(Error::Protocol(format!(
                "GET {path}: {status}: {}",
                String::from_utf8_lossy(&body)
            ))),
        }
    }

    /// Repair a list whose referenced pack is gone, only if its bytes still equal `previous`.
    /// The server additionally verifies absent old bytes and every new pack under its lock;
    /// a different valid list is never replaced. False is a raced comparison, not success.
    pub fn repair(&self, spec: &Digest, previous: &[u8], list: &[u8]) -> Result<bool, Error> {
        let path = format!("/v1/list/manifests/{}", hex(spec));
        let previous: Digest = Sha256::digest(previous).into();
        let (status, body) = self.request_matching("PUT", &path, list, Some(&previous))?;
        match status {
            200..=299 => Ok(true),
            412 => Ok(false),
            _ => Err(Error::Protocol(format!(
                "PUT {path} repairing a missing pack: {status}: {}",
                String::from_utf8_lossy(&body)
            ))),
        }
    }

    /// Publishes a full unit. A first writer may choose a different pack partition,
    /// but its verified logical keys, kinds and bytes must be identical.
    pub fn publish(&self, spec: &Digest, list: &[u8]) -> Result<(), Error> {
        let path = format!("/v1/list/manifests/{}", hex(spec));
        // After sending a publication, a lost response cannot tell us whether
        // the server committed the immutable key. It cannot authorize fallback.
        let (status, body) = publication_response(&path, self.request("PUT", &path, list))?;
        publish_verdict(
            status,
            &body,
            &path,
            list,
            || self.published(spec),
            |existing, proposed| {
                let existing = blaze_wire::pack_list::decode(existing).map_err(Error::Committed)?;
                let proposed = blaze_wire::pack_list::decode(proposed).map_err(Error::Committed)?;
                Ok(crate::agent::import(self, &existing)? == crate::agent::import(self, &proposed)?)
            },
        )
    }
}

/// A GET whose response already started with HTTP 200 names committed data.
/// A read error at that point must never turn into permission to rebuild locally
/// under the immutable key. Earlier IO failures remain subject to local policy.
fn committed_get_head(method: &str, reply: &[u8]) -> bool {
    if method != "GET" {
        return false;
    }
    let remaining =
        reply.strip_prefix(b"HTTP/1.1 200").or_else(|| reply.strip_prefix(b"HTTP/1.0 200"));
    remaining.is_some_and(|after| after.is_empty() || matches!(after[0], b' ' | b'\r' | b'\n'))
}

fn read_failure(method: &str, path: &str, reply: &[u8], error: std::io::Error) -> Error {
    if committed_get_head(method, reply) {
        // alloc: an error path names the committed key and the interrupted transport.
        Error::Committed(format!("{method} {path}: interrupted committed HTTP 200 reply: {error}"))
    } else {
        Error::Io(error)
    }
}

/// Read the head within its own budget, then apply the validated body framing.
/// Never let a blob's much larger allowance become an allowance for headers.
fn read_response(
    stream: &mut impl std::io::Read,
    method: &str,
    path: &str,
) -> Result<(u16, Vec<u8>), Error> {
    let mut reply = Vec::new(); // alloc: this response, bounded before each read
    let mut buffer = [0u8; 8192];
    let head = loop {
        // Include the delimiter, but no bytes beyond the largest valid head.
        let room = HTTP_HEADER_LIMIT + 4 - reply.len();
        if room == 0 {
            let message = format!("{method} {path}: HTTP headers exceed {HTTP_HEADER_LIMIT} bytes");
            return Err(if committed_get_head(method, &reply) {
                Error::Committed(message)
            } else {
                Error::Protocol(message)
            });
        }
        let amount = room.min(buffer.len());
        let read = stream
            .read(&mut buffer[..amount])
            .map_err(|error| read_failure(method, path, &reply, error))?;
        if read == 0 {
            return decode_response(method, path, &reply);
        }
        // Only the last three old bytes can start a delimiter crossing reads.
        let from = reply.len().saturating_sub(3);
        reply.extend_from_slice(&buffer[..read]);
        if reply[from..].windows(4).any(|bytes| bytes == b"\r\n\r\n") {
            break decode_head(method, path, &reply)?;
        }
    };
    let most = response_body_limit(method, path, head.status).saturating_add(HTTP_FRAMING_ROOM);
    // A fixed-length response has no chunk framing. Refuse its first excess
    // byte instead of spending the remaining blob allowance on an invalid body.
    let most = head.length.map_or(most, |length| most.min(head.body as u64 + length as u64));
    if reply.len() as u64 <= most {
        stream
            .take(most + 1 - reply.len() as u64)
            .read_to_end(&mut reply)
            .map_err(|error| read_failure(method, path, &reply, error))?;
    }
    if reply.len() as u64 > most {
        let message = format!("{method} {path}: HTTP reply exceeds {most} bytes");
        return Err(if method == "GET" && head.status == 200 {
            Error::Committed(message)
        } else {
            Error::Protocol(message)
        });
    }
    decode_response(method, path, &reply)
}

struct ResponseHead {
    status: u16,
    body: usize,
    length: Option<usize>,
    chunked: bool,
}

/// The same header validation is used before receiving a body and when decoding
/// a complete response, so an oversized declared length is refused before EOF.
fn decode_head(method: &str, path: &str, reply: &[u8]) -> Result<ResponseHead, Error> {
    let invalid = |why: &str| Error::Protocol(format!("{method} {path}: {why}"));
    let end = reply.windows(4).position(|it| it == b"\r\n\r\n").ok_or_else(|| {
        if committed_get_head(method, reply) {
            // alloc: an incomplete committed reply cannot authorize local recompilation.
            Error::Committed(format!("{method} {path}: truncated committed HTTP 200 headers"))
        } else {
            invalid("no HTTP head")
        }
    })?;
    if end > HTTP_HEADER_LIMIT {
        let message = format!("{method} {path}: HTTP headers exceed {HTTP_HEADER_LIMIT} bytes");
        return Err(if committed_get_head(method, reply) {
            Error::Committed(message)
        } else {
            Error::Protocol(message)
        });
    }
    let head = std::str::from_utf8(&reply[..end]).map_err(|_| {
        if committed_get_head(method, reply) {
            // alloc: a corrupt committed reply must not be treated as a cache miss.
            Error::Committed(format!("{method} {path}: committed HTTP 200 headers are not text"))
        } else {
            invalid("the head is not text")
        }
    })?;
    let status = head
        .split("\r\n")
        .next()
        .and_then(|line| {
            let mut fields = line.split_ascii_whitespace();
            if !fields.next()?.starts_with("HTTP/1.") {
                return None;
            }
            fields.next()?.parse::<u16>().ok()
        })
        .ok_or_else(|| invalid("no HTTP/1 status"))?;
    // A successful GET names committed data. Bad framing must not permit
    // the caller to rebuild locally under that immutable cache key.
    let invalid = |why: &str| {
        let message = format!("{method} {path}: {why}");
        if method == "GET" && status == 200 {
            Error::Committed(message)
        } else {
            Error::Protocol(message)
        }
    };
    let mut length = None;
    let mut chunked = false;
    for line in head.split("\r\n").skip(1) {
        let (name, value) = line.split_once(':').ok_or_else(|| invalid("malformed HTTP header"))?;
        if name.eq_ignore_ascii_case("content-length") {
            let bytes =
                value.trim().parse::<usize>().map_err(|_| invalid("invalid Content-Length"))?;
            if length.replace(bytes).is_some() {
                return Err(invalid("duplicate Content-Length"));
            }
        } else if name.eq_ignore_ascii_case("transfer-encoding") {
            if chunked || !value.trim().eq_ignore_ascii_case("chunked") {
                return Err(invalid("unsupported Transfer-Encoding"));
            }
            chunked = true;
        }
    }
    let limit = response_body_limit(method, path, status);
    if length.is_some_and(|expected| expected as u64 > limit) {
        return Err(invalid("Content-Length exceeds the documented response body limit"));
    }
    if chunked && length.is_some() {
        return Err(invalid("ambiguous HTTP body framing"));
    }
    if method == "GET" && status == 200 && length.is_none() && !chunked {
        return Err(invalid("a successful GET has no explicit HTTP body length"));
    }
    Ok(ResponseHead { status, body: end + 4, length, chunked })
}

/// Decode a complete HTTP/1.1 response after the peer has closed its connection.
/// Never treat a short body as a complete persisted manifest or pack.
fn decode_response(method: &str, path: &str, reply: &[u8]) -> Result<(u16, Vec<u8>), Error> {
    let ResponseHead { status, body, length, chunked } = decode_head(method, path, reply)?;
    let invalid = |why: &str| {
        let message = format!("{method} {path}: {why}");
        if method == "GET" && status == 200 {
            Error::Committed(message)
        } else {
            Error::Protocol(message)
        }
    };
    let body = &reply[body..];
    let limit = response_body_limit(method, path, status);
    if chunked {
        let decoded = unchunk(body, limit)
            .ok_or_else(|| invalid("a broken or oversized chunked body"))?;
        if decoded.len() as u64 > limit {
            return Err(invalid("chunked body exceeds the documented response body limit"));
        }
        return Ok((status, decoded));
    }
    if body.len() as u64 > limit {
        return Err(invalid("HTTP body exceeds the documented response body limit"));
    }
    if let Some(expected) = length {
        if body.len() != expected {
            return Err(invalid("Content-Length does not match the complete body"));
        }
    }
    Ok((status, body.to_vec()))
}

fn unchunk(mut body: &[u8], limit: u64) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    loop {
        let end = body.windows(2).position(|it| it == b"\r\n")?;
        let text = std::str::from_utf8(&body[..end]).ok()?;
        let len = usize::from_str_radix(text.split(';').next()?.trim(), 16).ok()?;
        body = &body[end + 2..];
        if len == 0 {
            if body == b"\r\n" {
                return Some(out);
            }
            // Trailer fields are allowed, but a missing final CRLF or extra
            // bytes after the terminator are an invalid response.
            let trailers = std::str::from_utf8(body).ok()?.strip_suffix("\r\n\r\n")?;
            if trailers.split("\r\n").all(|line| {
                line.split_once(':').is_some_and(|(name, _)| {
                    !name.is_empty() && !name.bytes().any(|byte| byte.is_ascii_whitespace())
                })
            }) {
                return Some(out);
            }
            return None;
        }
        // Check before extending: a rejected response may not allocate its
        // over-limit decoded body first, even within the framing allowance.
        if len as u64 > limit.saturating_sub(out.len() as u64) {
            return None;
        }
        let data = body.get(..len)?;
        let rest = body.get(len..)?;
        if !rest.starts_with(b"\r\n") {
            return None;
        }
        out.extend_from_slice(data);
        body = &rest[2..];
    }
}

/// The reply may be lost after the server has committed an immutable manifest.
/// A transport/framing failure is therefore an unknown publication outcome,
/// never evidence that it is safe to rebuild under the same key.
fn publication_response(
    path: &str,
    response: Result<(u16, Vec<u8>), Error>,
) -> Result<(u16, Vec<u8>), Error> {
    response.map_err(|error| {
        Error::Committed(format!(
            "PUT {path}: publication outcome unknown after request failure: {error}"
        ))
    })
}

/// A first publisher wins the immutable key. A 412 proves that a value exists,
/// not that the value is identical; look up and compare it before accepting.
fn publish_verdict(
    status: u16,
    body: &[u8],
    path: &str,
    proposed: &[u8],
    read_existing: impl FnOnce() -> Result<Option<Vec<u8>>, Error>,
    equivalent: impl FnOnce(&[u8], &[u8]) -> Result<bool, Error>,
) -> Result<(), Error> {
    match status {
        200..=299 => Ok(()),
        412 => {
            // The refused PUT proves an immutable key was already claimed. If the
            // subsequent GET fails, that uncertainty is not a cache miss and must
            // never authorize a local rebuild under the same key.
            let existing = read_existing().map_err(|error| {
                Error::Committed(format!(
                    "PUT {path}: 412: cannot read the committed list: {error}"
                ))
            })?;
            match existing {
                Some(existing) if existing.as_slice() == proposed => Ok(()),
                Some(existing) => {
                    let same = equivalent(&existing, proposed).map_err(|error| {
                        Error::Committed(format!(
                            "PUT {path}: 412: cannot verify committed packs: {error}"
                        ))
                    })?;
                    if same {
                        Ok(())
                    } else {
                        Err(Error::Committed(format!(
                            "PUT {path}: 412: another publisher committed different values under this unit key"
                        )))
                    }
                }
                None => Err(Error::Committed(format!(
                    "PUT {path}: 412: publication was refused but the committed list is missing"
                ))),
            }
        }
        _ => {
            // A non-success status is not proof that the server left the
            // immutable key unclaimed: publication may have committed before
            // a subsequent error response. Refuse local recompilation.
            Err(Error::Committed(format!(
                "PUT {path}: {status}: publication outcome unknown: {}",
                String::from_utf8_lossy(body)
            )))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::io::Read as _;

    /// A socket surrogate that panics if a refusal consumes bytes past its
    /// budget. A decode-only check after read_to_end cannot satisfy this oracle.
    struct Budgeted<'a> {
        bytes: &'a [u8],
        fragment: usize,
        consumed: usize,
        budget: usize,
    }

    impl std::io::Read for Budgeted<'_> {
        fn read(&mut self, into: &mut [u8]) -> std::io::Result<usize> {
            if self.bytes.is_empty() || into.is_empty() {
                return Ok(0);
            }
            assert!(self.consumed < self.budget, "read past the early-refusal budget");
            let n = into.len()
                .min(self.fragment)
                .min(self.bytes.len())
                .min(self.budget - self.consumed);
            into[..n].copy_from_slice(&self.bytes[..n]);
            self.bytes = &self.bytes[n..];
            self.consumed += n;
            Ok(n)
        }
    }

    #[test]
    fn response_limits_stop_the_reader_before_buffering_a_hostile_reply() {
        let path = "/v1/blob/packs/test.pack";
        let mut oversized = b"HTTP/1.1 200 OK\r\nX-Long: ".to_vec(); // alloc: bounded hostile head
        oversized.resize(2 * HTTP_HEADER_LIMIT, b'a');
        for fragment in [1, 3, 8192] {
            let mut stream = Budgeted {
                bytes: &oversized,
                fragment,
                consumed: 0,
                budget: HTTP_HEADER_LIMIT + 4,
            };
            let error = read_response(&mut stream, "GET", path).unwrap_err();
            assert!(matches!(&error, Error::Committed(why) if why.contains("headers exceed")));
            assert!(stream.consumed <= HTTP_HEADER_LIMIT + 4);
            assert!(matches!(
                crate::agent::decide(&blaze_wire::LocalPolicy::Fallback, error),
                Err(Error::Committed(_))
            ));
        }
        // Plant the previous buffering order: this bounded reader must catch it
        // before it can spend the blob allowance on header bytes.
        let caught = std::panic::catch_unwind(|| {
            let mut stream = Budgeted {
                bytes: &oversized,
                fragment: 8192,
                consumed: 0,
                budget: HTTP_HEADER_LIMIT + 4,
            };
            let most = response_body_limit("GET", path, 200) + HTTP_FRAMING_ROOM;
            let mut reply = Vec::new(); // alloc: the bounded planted-bug fixture
            (&mut stream).take(most + 1).read_to_end(&mut reply).unwrap();
        });
        assert!(caught.is_err(), "the oracle must catch checking the head only after EOF");

        let mut boundary = b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nX: ".to_vec();
        boundary.resize(HTTP_HEADER_LIMIT, b'a'); // alloc: one exactly legal header
        boundary.extend_from_slice(b"\r\n\r\n");
        let mut stream = boundary.as_slice();
        assert_eq!(read_response(&mut stream, "GET", path).unwrap(), (200, Vec::new()));

        for path in [path, "/v1/list/manifests/unit"] {
            let length = response_body_limit("GET", path, 200) + 1;
            let head = format!("HTTP/1.1 200 OK\r\nContent-Length: {length}\r\n\r\n");
            let reply = format!("{head}must not read this body");
            let mut stream = Budgeted {
                bytes: reply.as_bytes(),
                fragment: head.len(),
                consumed: 0,
                budget: head.len(),
            };
            assert!(matches!(
                read_response(&mut stream, "GET", path),
                Err(Error::Committed(why)) if why.contains("documented response body limit")
            ));
            assert_eq!(stream.consumed, head.len());
        }
        // The same oracle catches reading past a valid declared length before
        // rejecting surplus bytes. The single excess byte proves the refusal.
        let head = b"HTTP/1.1 200 OK\r\nContent-Length: 1\r\n\r\n";
        let mut reply = head.to_vec(); // alloc: a small fixed-length overrun fixture
        reply.extend_from_slice(b"abcd");
        let mut stream = Budgeted {
            bytes: &reply,
            fragment: head.len(),
            consumed: 0,
            budget: head.len() + 2,
        };
        assert!(matches!(read_response(&mut stream, "GET", path), Err(Error::Committed(_))));
        assert_eq!(stream.consumed, head.len() + 2);
    }

    #[test]
    fn fragmented_replies_preserve_body_boundaries_and_committed_refusals() {
        let path = "/v1/list/manifests/unit";
        for reply in [
            b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nabcde".as_slice(),
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nabc\r\n2\r\nde\r\n0\r\n\r\n".as_slice(),
            b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nabc".as_slice(),
            b"HTTP/1.1 404 Not Found\r\n\r\nabsent".as_slice(),
        ] {
            for fragment in [1, 2, 7, 8192] {
                let mut stream = Budgeted {
                    bytes: reply,
                    fragment,
                    consumed: 0,
                    budget: reply.len(),
                };
                match decode_response("GET", path, reply) {
                    Ok(expected) => {
                        assert_eq!(read_response(&mut stream, "GET", path).unwrap(), expected);
                    }
                    Err(Error::Committed(_)) => assert!(matches!(
                        read_response(&mut stream, "GET", path), Err(Error::Committed(_))
                    )),
                    Err(other) => panic!("unexpected fixture refusal: {other}"),
                }
            }
        }
        let chunks = b"3\r\nabc\r\n2\r\nde\r\n0\r\n\r\n";
        assert_eq!(unchunk(chunks, 5).unwrap(), b"abcde");
        assert!(unchunk(chunks, 4).is_none(), "check the decoded budget before appending");
    }

    #[test]
    fn error_responses_to_blob_gets_cannot_claim_blob_sized_bodies() {
        let path = "/v1/blob/packs/test.pack";
        let small = u64::from(Limits::DOCUMENTED.request);
        assert_eq!(response_body_limit("GET", path, 200), Limits::DOCUMENTED.blob);
        for status in [404, 500] {
            assert_eq!(response_body_limit("GET", path, status), small);
            let claimed = small + 1;
            let head = format!(
                "HTTP/1.1 {status} Error\r\nContent-Length: {claimed}\r\n\r\n"
            );
            // Even a peer sending its response one byte at a time must be
            // refused before any body bytes can be read.
            let mut stream = Budgeted {
                bytes: head.as_bytes(),
                fragment: 1,
                consumed: 0,
                budget: head.len(),
            };
            let error = read_response(&mut stream, "GET", path).unwrap_err();
            assert!(matches!(
                error,
                Error::Protocol(why) if why.contains("documented response body limit")
            ));
            assert_eq!(stream.consumed, head.len());

            // Chunked bodies use the same status-aware decoded limit,
            // independent of their wire framing.
            let chunked = format!(
                "HTTP/1.1 {status} Error\r\nTransfer-Encoding: chunked\r\n\r\n{claimed:x}\r\n"
            );
            assert!(matches!(
                decode_response("GET", path, chunked.as_bytes()),
                Err(Error::Protocol(why)) if why.contains("oversized chunked body")
            ));
        }
        // A real successful blob GET is not incorrectly capped at the
        // manifest/error limit; it still needs the full documented blob budget.
        let large = small + 1;
        let head = format!("HTTP/1.1 200 OK\r\nContent-Length: {large}\r\n\r\n");
        assert_eq!(
            decode_head("GET", path, head.as_bytes()).unwrap().length,
            Some(large as usize)
        );
    }

    #[test]
    fn committed_reply_bounds_follow_the_native_wire_contract() {
        let blob = "/v1/blob/packs/abc.pack";
        let manifest = "/v1/list/manifests/abc";
        assert_eq!(response_body_limit("GET", blob, 200), Limits::DOCUMENTED.blob);
        assert_eq!(
            response_body_limit("GET", manifest, 200),
            u64::from(Limits::DOCUMENTED.request)
        );
        assert_eq!(
            response_body_limit("PUT", blob, 200),
            u64::from(Limits::DOCUMENTED.request)
        );
        // A successful GET with a forged oversize length is a committed-data
        // refusal, never permission to compile again under that immutable key.
        let claimed = u64::from(Limits::DOCUMENTED.request) + 1;
        let oversized =
            format!("HTTP/1.1 200 OK\r\nContent-Length: {claimed}\r\n\r\n");
        let refused = decode_response("GET", manifest, oversized.as_bytes()).unwrap_err();
        assert!(matches!(
            &refused,
            Error::Committed(why) if why.contains("documented response body limit")
        ));
        assert!(matches!(
            crate::agent::decide(&blaze_wire::LocalPolicy::Fallback, refused),
            Err(Error::Committed(_))
        ));
        let mut excessive_headers = b"HTTP/1.1 200 OK\r\nX-Long: ".to_vec();
        excessive_headers.extend(std::iter::repeat_n(b'a', HTTP_HEADER_LIMIT));
        excessive_headers.extend_from_slice(b"\r\n\r\n");
        assert!(matches!(
            decode_response("GET", manifest, &excessive_headers),
            Err(Error::Committed(_))
        ));
    }

    #[test]
    fn an_http_reply_must_end_at_its_declared_body_boundary() {
        let path = "/v1/list/manifests/unit";
        let fixed = b"HTTP/1.1 200 OK\r\nContent-Length: 6\r\n\r\na b c\n";
        assert_eq!(decode_response("GET", path, fixed).unwrap(), (200, b"a b c\n".to_vec()));
        let broken = [
            b"HTTP/1.1 200 OK\r\nContent-Length: 6\r\n\r\na b\n".as_slice(),
            b"HTTP/1.1 200 OK\r\nContent-Length: 6\r\n\r\na b c\nextra".as_slice(),
            b"HTTP/1.1 200 OK\r\nContent-Length: 6\r\nContent-Length: 6\r\n\r\na b c\n".as_slice(),
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nContent-Length: 6\r\n\r\n0\r\n\r\n"
                .as_slice(),
            b"HTTP/1.1 200 OK\r\nConnection: close\r\n\r\na b c\n".as_slice(),
            b"HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n".as_slice(),
        ];
        for bytes in broken {
            assert!(matches!(decode_response("GET", path, bytes), Err(Error::Committed(_))));
        }

        let chunks = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nabc\r\n2\r\nde\r\n0\r\n\r\n";
        assert_eq!(decode_response("GET", path, chunks).unwrap(), (200, b"abcde".to_vec()));
        for bytes in [
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nabcXY0\r\n\r\n".as_slice(),
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nabc\r\n0\r\n".as_slice(),
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nabc\r\n0\r\n\r\nextra"
                .as_slice(),
        ] {
            assert!(matches!(decode_response("GET", path, bytes), Err(Error::Committed(_))));
        }
        // A legitimate 404 does not establish the existence of committed data.
        assert_eq!(
            decode_response("GET", path, b"HTTP/1.1 404 Not Found\r\n\r\nnot here").unwrap(),
            (404, b"not here".to_vec())
        );
    }

    #[test]
    fn an_interrupted_committed_get_cannot_authorize_local_recompilation() {
        let path = "/v1/list/manifests/test-unit";
        for bytes in [
            b"HTTP/1.1 200 OK\r\nContent-Length: 20\r\n".as_slice(),
            b"HTTP/1.1 200 OK\r\nContent-Length: 20\r\n\r\npartial".as_slice(),
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n10\r\npart".as_slice(),
        ] {
            let refused = read_failure(
                "GET",
                path,
                bytes,
                std::io::Error::from(std::io::ErrorKind::TimedOut),
            );
            assert!(
                matches!(&refused, Error::Committed(why) if why.contains(path)),
                "partial HTTP 200 must be a committed-state refusal: {refused}"
            );
            assert!(matches!(
                crate::agent::decide(&blaze_wire::LocalPolicy::Fallback, refused),
                Err(Error::Committed(_))
            ));
        }
        for bytes in [
            b"HTTP/1.1 200 OK\r\nContent-Length: 20\r\n".as_slice(),
            b"HTTP/1.1 200 OK\r\nSome-Header: \xff\r\n\r\n".as_slice(),
        ] {
            assert!(matches!(decode_response("GET", path, bytes), Err(Error::Committed(_))));
        }
        // Before an HTTP 200 is observed, a transport failure can follow the
        // caller's explicit local policy. Neither 404 nor a PUT is proof of a
        // committed GET, and a successful GET must not be misclassified as 404.
        for (method, partial) in [
            ("GET", b"HTTP/1.1 404 Not Found\r\n".as_slice()),
            ("GET", b"".as_slice()),
            ("PUT", b"HTTP/1.1 200 OK".as_slice()),
        ] {
            assert!(matches!(
                read_failure(
                    method,
                    path,
                    partial,
                    std::io::Error::from(std::io::ErrorKind::TimedOut)
                ),
                Error::Io(_)
            ));
        }
    }

    #[test]
    fn a_lost_publication_reply_cannot_authorize_local_recompilation() {
        let path = "/v1/list/manifests/test-unit";
        for error in [
            Error::Io(std::io::Error::from(std::io::ErrorKind::TimedOut)),
            Error::Protocol("the publication reply was truncated".to_owned()),
        ] {
            let refused = publication_response(path, Err(error)).unwrap_err();
            assert!(
                matches!(&refused, Error::Committed(why)
                    if why.contains(path) && why.contains("outcome unknown")),
                "the failed publication must identify the uncertain immutable key: {refused}"
            );
            assert!(
                matches!(
                    crate::agent::decide(&blaze_wire::LocalPolicy::Fallback, refused),
                    Err(Error::Committed(_))
                ),
                "a lost publication acknowledgement cannot authorize local recompilation"
            );
        }

        assert_eq!(publication_response(path, Ok((201, Vec::new()))).unwrap().0, 201);
    }

    #[test]
    fn a_first_writer_conflict_must_compare_the_immutable_manifest() {
        let path = "/v1/list/manifests/test-unit";
        let cases: [(Option<&[u8]>, bool); 3] =
            [(Some(b"same"), true), (Some(b"different"), false), (None, false)];
        for (committed, accepted) in cases {
            let read = Cell::new(false);
            let verdict = publish_verdict(
                412,
                b"",
                path,
                b"same",
                || {
                    read.set(true);
                    Ok(committed.map(|bytes| bytes.to_vec())) // alloc: test manifest fixture
                },
                |_, _| Ok(false),
            );
            assert!(read.get(), "412 must fetch the existing committed bytes");
            assert_eq!(verdict.is_ok(), accepted, "unexpected conflict verdict: {verdict:?}");
            if let Err(error) = verdict {
                assert!(
                    matches!(&error, Error::Committed(why) if why.contains(path)),
                    "the refusal must name the conflicting unit: {error}"
                );
                assert!(
                    matches!(
                        crate::agent::decide(&blaze_wire::LocalPolicy::Fallback, error),
                        Err(Error::Committed(_))
                    ),
                    "an immutable publication conflict must not authorize recompilation"
                );
            }
        }
        // A 412 means another writer already claimed the unit key. A failed
        // verification GET must not become an ordinary transport error that
        // LocalPolicy::Fallback uses to compile again under that key.
        for read_error in [
            Error::Io(std::io::Error::new(std::io::ErrorKind::TimedOut, "verification timed out")),
            Error::Protocol("verification response was malformed".to_owned()),
        ] {
            let refused = publish_verdict(
                412,
                b"",
                path,
                b"same",
                || Err(read_error),
                |_, _| {
                    panic!("a failed GET cannot verify logical equivalence");
                },
            )
            .unwrap_err();
            assert!(
                matches!(&refused, Error::Committed(why) if why.contains(path)
                    && why.contains("cannot read the committed list")),
                "a refused immutable publication must stay committed: {refused}"
            );
            assert!(
                matches!(
                    crate::agent::decide(&blaze_wire::LocalPolicy::Fallback, refused),
                    Err(Error::Committed(_))
                ),
                "failed verification must not authorize local recompilation"
            );
        }
        for verify_error in [
            Error::Io(std::io::Error::from(std::io::ErrorKind::TimedOut)),
            Error::Protocol("committed packs could not be decoded".to_owned()),
        ] {
            let refused = publish_verdict(
                412,
                b"",
                path,
                b"proposed partition",
                || Ok(Some(b"existing partition".to_vec())),
                |_, _| Err(verify_error),
            )
            .unwrap_err();
            assert!(matches!(
                crate::agent::decide(&blaze_wire::LocalPolicy::Fallback, refused),
                Err(Error::Committed(_))
            ));
        }
        let read = Cell::new(false);
        let result = publish_verdict(
            201,
            b"",
            path,
            b"same",
            || {
                read.set(true);
                panic!("an accepted PUT must not issue a GET");
            },
            |_, _| panic!("an accepted PUT needs no partition comparison"),
        );
        assert!(result.is_ok());
        assert!(!read.get());
        // Only explicit accepted publication and a verified 412 establish the
        // immutable outcome. Other statuses may arrive after a server committed
        // but failed before returning its acknowledgement.
        for status in [301, 400, 429, 500, 503] {
            let refused = publish_verdict(
                status,
                b"unavailable",
                path,
                b"same",
                || panic!("an unknown PUT outcome cannot issue a GET"),
                |_, _| panic!("an unknown PUT outcome cannot verify equivalence"),
            )
            .unwrap_err();
            assert!(
                matches!(&refused, Error::Committed(why)
                    if why.contains(path) && why.contains("outcome unknown")),
                "unexpected publication refusal: {refused}"
            );
            assert!(matches!(
                crate::agent::decide(&blaze_wire::LocalPolicy::Fallback, refused),
                Err(Error::Committed(_))
            ));
        }
        // Control: the old Protocol classification would authorize exactly
        // the forbidden local rebuild after this uncertain publication.
        assert!(crate::agent::decide(
            &blaze_wire::LocalPolicy::Fallback,
            Error::Protocol("unknown publication outcome".to_owned()),
        )
        .is_ok());
    }
}
