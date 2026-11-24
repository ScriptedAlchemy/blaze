//! A blocking client of the cache server (`blaze-serve`) for what an agent moves: a pack by
//! digest and a unit's published list of packs. One request per connection, each bounded by
//! `timeout`; the server checks every blob against its name before it is visible, and the
//! agent checks it again against what was committed ([`crate::pack::read`]).

use core::alloc::Allocator;
use core::net::SocketAddr;
use core::time::Duration;

use blaze_int::Int as _;
use blaze_own::{Vec, format_in};
use blaze_sha256::digest as sha256_digest;
use blaze_wire::json::hex;
use blaze_wire::{Digest, Limits, WireAlloc, WireString};

use crate::Error;
use crate::pack::Pack;
use crate::sys::{Errno, IoError, TcpStream};

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

/// Minimal byte-stream reader so HTTP response framing can be tested without real sockets.
trait ByteRead {
    fn read_bytes(&mut self, buf: &mut [u8]) -> Result<usize, IoError>;
}

impl ByteRead for TcpStream {
    fn read_bytes(&mut self, buf: &mut [u8]) -> Result<usize, IoError> {
        self.read(buf)
    }
}

impl ByteRead for &[u8] {
    fn read_bytes(&mut self, buf: &mut [u8]) -> Result<usize, IoError> {
        let n = self.len().min(buf.len());
        if let (Some(src), Some(dst)) = (self.get(..n), buf.get_mut(..n)) {
            dst.copy_from_slice(src);
        }
        *self = self.get(n..).unwrap_or(&[]);
        Ok(n)
    }
}

/// Formats arbitrary bytes as lossy UTF-8 into `alloc`.
fn lossy_text(mut bytes: &[u8], alloc: WireAlloc) -> WireString {
    let mut out = WireString::with_capacity_in(bytes.len(), alloc);
    while !bytes.is_empty() {
        match core::str::from_utf8(bytes) {
            Ok(valid) => {
                out.push_str(valid);
                break;
            }
            Err(err) => {
                let valid_len = err.valid_up_to();
                if let Some(valid) = bytes.get(..valid_len).and_then(|s| core::str::from_utf8(s).ok()) {
                    out.push_str(valid);
                }
                out.push('\u{FFFD}');
                let skip = valid_len.saturating_add(err.error_len().unwrap_or(1));
                bytes = bytes.get(skip..).unwrap_or(&[]);
            }
        }
    }
    out
}

/// Where packs are kept.
#[derive(Clone, Debug)]
pub struct Blobs {
    /// The server's address.
    pub address: SocketAddr,
    /// How long one read or write of a request may stall.
    pub timeout: Duration,
    /// Allocator used for error messages and wire scratch.
    pub alloc: WireAlloc,
}

impl Blobs {
    fn request<A: Allocator>(
        &self,
        method: &str,
        path: &str,
        body: &[u8],
        alloc: A,
    ) -> Result<(u16, Vec<u8, A>), Error> {
        self.request_matching(method, path, body, None, alloc)
    }

    fn request_matching<A: Allocator>(
        &self,
        method: &str,
        path: &str,
        body: &[u8],
        previous: Option<&Digest>,
        alloc: A,
    ) -> Result<(u16, Vec<u8, A>), Error> {
        let mut stream = TcpStream::connect_timeout(self.address, self.timeout)?;
        stream.set_read_timeout(Some(self.timeout))?;
        stream.set_write_timeout(Some(self.timeout))?;
        stream.set_nodelay(true)?;
        let condition = match previous {
            Some(digest) => format_in!(self.alloc, "If-Match: \"{}\"\r\n", hex(digest)),
            None => WireString::new_in(self.alloc),
        };
        let head = format_in!(
            self.alloc,
            "{method} {path} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\nContent-Length: {}\r\n{condition}\r\n",
            self.address,
            body.len()
        );
        stream.write_all(head.as_bytes())?;
        stream.write_all(body)?;
        read_response(&mut stream, method, path, alloc, self.alloc)
    }

    /// Puts `pack` under its name. The server hashes it before it is visible, so a pack that
    /// is there is whole.
    pub fn put_pack(
        &self,
        pack: &Pack<impl Allocator>,
        alloc: impl Allocator,
    ) -> Result<(), Error> {
        let path = format_in!(self.alloc, "/v1/blob/packs/{}.pack", hex(&pack.reference.pack));
        let (status, body) = self.request("PUT", path.as_str(), &pack.bytes, alloc)?;
        match status {
            200..=299 => Ok(()),
            _ => Err(Error::Protocol(format_in!(
                self.alloc,
                "PUT {path}: {status}: {}",
                lossy_text(&body, self.alloc)
            ))),
        }
    }

    /// The bytes of the pack named `digest`; `None` if the server has none.
    pub fn pack<A: Allocator>(&self, digest: &Digest, alloc: A) -> Result<Option<Vec<u8, A>>, Error> {
        let path = format_in!(self.alloc, "/v1/blob/packs/{}.pack", hex(digest));
        let (status, body) = self.request("GET", path.as_str(), &[], alloc)?;
        match status {
            200 => Ok(Some(body)),
            404 => Ok(None),
            _ => Err(Error::Protocol(format_in!(
                self.alloc,
                "GET {path}: {status}: {}",
                lossy_text(&body, self.alloc)
            ))),
        }
    }

    /// The published list of the unit named `spec`; `None` if nobody published it.
    pub fn published<A: Allocator>(
        &self,
        spec: &Digest,
        alloc: A,
    ) -> Result<Option<Vec<u8, A>>, Error> {
        let path = format_in!(self.alloc, "/v1/list/manifests/{}", hex(spec));
        let (status, body) = self.request("GET", path.as_str(), &[], alloc)?;
        match status {
            200 => Ok(Some(body)),
            404 => Ok(None),
            _ => Err(Error::Protocol(format_in!(
                self.alloc,
                "GET {path}: {status}: {}",
                lossy_text(&body, self.alloc)
            ))),
        }
    }

    /// Repair a list whose referenced pack is gone, only if its bytes still equal `previous`.
    /// The server additionally verifies absent old bytes and every new pack under its lock;
    /// a different valid list is never replaced. False is a raced comparison, not success.
    pub fn repair(
        &self,
        spec: &Digest,
        previous: &[u8],
        list: &[u8],
        alloc: impl Allocator,
    ) -> Result<bool, Error> {
        let path = format_in!(self.alloc, "/v1/list/manifests/{}", hex(spec));
        let previous: Digest = sha256_digest(previous);
        let (status, body) =
            self.request_matching("PUT", path.as_str(), list, Some(&previous), alloc)?;
        match status {
            200..=299 => Ok(true),
            412 => Ok(false),
            _ => Err(Error::Protocol(format_in!(
                self.alloc,
                "PUT {path} repairing a missing pack: {status}: {}",
                lossy_text(&body, self.alloc)
            ))),
        }
    }

    /// Publishes a full unit. A first writer may choose a different pack partition,
    /// but its verified logical keys, kinds and bytes must be identical.
    pub fn publish(&self, spec: &Digest, list: &[u8], alloc: impl Allocator) -> Result<(), Error> {
        let path = format_in!(self.alloc, "/v1/list/manifests/{}", hex(spec));
        // After sending a publication, a lost response cannot tell us whether
        // the server committed the immutable key. It cannot authorize fallback.
        let (status, body) = publication_response(
            path.as_str(),
            self.request("PUT", path.as_str(), list, &alloc),
            self.alloc,
        )?;
        publish_verdict(
            status,
            &body,
            path.as_str(),
            list,
            self.alloc,
            || self.published(spec, &alloc),
            |existing, proposed| {
                let existing = blaze_wire::pack_list::decode(existing, &alloc)
                    .map_err(|error| Error::Committed(format_in!(self.alloc, "{error}")))?;
                let proposed = blaze_wire::pack_list::decode(proposed, &alloc)
                    .map_err(|error| Error::Committed(format_in!(self.alloc, "{error}")))?;
                Ok(crate::agent::import(self, &existing, &alloc)?
                    == crate::agent::import(self, &proposed, &alloc)?)
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
    remaining.is_some_and(|after| {
        after.first().is_none_or(|&b| matches!(b, b' ' | b'\r' | b'\n'))
    })
}

fn read_failure(
    method: &str,
    path: &str,
    reply: &[u8],
    error: IoError,
    err_alloc: WireAlloc,
) -> Error {
    if committed_get_head(method, reply) {
        Error::Committed(format_in!(
            err_alloc,
            "{method} {path}: interrupted committed HTTP 200 reply: {error}"
        ))
    } else {
        Error::Io(error)
    }
}

/// Read the head within its own budget, then apply the validated body framing.
/// Never let a blob's much larger allowance become an allowance for headers.
fn read_response<A: Allocator>(
    stream: &mut impl ByteRead,
    method: &str,
    path: &str,
    alloc: A,
    err_alloc: WireAlloc,
) -> Result<(u16, Vec<u8, A>), Error> {
    let mut reply = Vec::new_in(alloc);
    let mut buffer = [0u8; 8192];
    let head = loop {
        // Include the delimiter, but no bytes beyond the largest valid head.
        let room = HTTP_HEADER_LIMIT.saturating_add(4).saturating_sub(reply.len());
        if room == 0 {
            let message = format_in!(
                err_alloc,
                "{method} {path}: HTTP headers exceed {HTTP_HEADER_LIMIT} bytes"
            );
            return Err(if committed_get_head(method, &reply) {
                Error::Committed(message)
            } else {
                Error::Protocol(message)
            });
        }
        let amount = room.min(buffer.len());
        let Some(slice) = buffer.get_mut(..amount) else {
            return Err(Error::Io(IoError::from_errno(Errno::Inval)));
        };
        let read = stream
            .read_bytes(slice)
            .map_err(|error| read_failure(method, path, &reply, error, err_alloc))?;
        if read == 0 {
            return decode_response(method, path, reply, err_alloc);
        }
        // Only the last three old bytes can start a delimiter crossing reads.
        let from = reply.len().saturating_sub(3);
        if let Some(got) = buffer.get(..read) {
            reply.extend_from_slice(got);
        }
        if reply.get(from..).is_some_and(|tail| tail.windows(4).any(|bytes| bytes == b"\r\n\r\n")) {
            break decode_head(method, path, &reply, err_alloc)?;
        }
    };
    let most = response_body_limit(method, path, head.status).saturating_add(HTTP_FRAMING_ROOM);
    // A fixed-length response has no chunk framing. Refuse its first excess
    // byte instead of spending the remaining blob allowance on an invalid body.
    let most = head.length.map_or(most, |length| {
        most.min(head.body.widen_to::<u64>().saturating_add(length.widen_to::<u64>()))
    });
    while reply.len().widen_to::<u64>() <= most {
        let remaining = most.saturating_add(1).saturating_sub(reply.len().widen_to::<u64>());
        let amount = usize::try_from(remaining.min(buffer.len().widen_to::<u64>()))
            .unwrap_or(buffer.len());
        let Some(slice) = buffer.get_mut(..amount) else {
            return Err(Error::Io(IoError::from_errno(Errno::Inval)));
        };
        match stream.read_bytes(slice) {
            Ok(0) => break,
            Ok(read) => {
                if let Some(got) = buffer.get(..read) {
                    reply.extend_from_slice(got);
                }
            }
            Err(error) if error.is_interrupted() => continue,
            Err(error) => return Err(read_failure(method, path, &reply, error, err_alloc)),
        }
    }
    if reply.len().widen_to::<u64>() > most {
        let message = format_in!(err_alloc, "{method} {path}: HTTP reply exceeds {most} bytes");
        return Err(if method == "GET" && head.status == 200 {
            Error::Committed(message)
        } else {
            Error::Protocol(message)
        });
    }
    decode_response(method, path, reply, err_alloc)
}

struct ResponseHead {
    status: u16,
    body: usize,
    length: Option<usize>,
    chunked: bool,
}

/// The same header validation is used before receiving a body and when decoding
/// a complete response, so an oversized declared length is refused before EOF.
fn decode_head(
    method: &str,
    path: &str,
    reply: &[u8],
    err_alloc: WireAlloc,
) -> Result<ResponseHead, Error> {
    let invalid = |why: &str| Error::Protocol(format_in!(err_alloc, "{method} {path}: {why}"));
    let end = reply.windows(4).position(|it| it == b"\r\n\r\n").ok_or_else(|| {
        if committed_get_head(method, reply) {
            Error::Committed(format_in!(
                err_alloc,
                "{method} {path}: truncated committed HTTP 200 headers"
            ))
        } else {
            invalid("no HTTP head")
        }
    })?;
    if end > HTTP_HEADER_LIMIT {
        let message = format_in!(
            err_alloc,
            "{method} {path}: HTTP headers exceed {HTTP_HEADER_LIMIT} bytes"
        );
        return Err(if committed_get_head(method, reply) {
            Error::Committed(message)
        } else {
            Error::Protocol(message)
        });
    }
    let head_bytes = reply.get(..end).ok_or_else(|| invalid("no HTTP head"))?;
    let head = core::str::from_utf8(head_bytes).map_err(|_| {
        if committed_get_head(method, reply) {
            Error::Committed(format_in!(
                err_alloc,
                "{method} {path}: committed HTTP 200 headers are not text"
            ))
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
        let message = format_in!(err_alloc, "{method} {path}: {why}");
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
    if length.is_some_and(|expected| expected.widen_to::<u64>() > limit) {
        return Err(invalid("Content-Length exceeds the documented response body limit"));
    }
    if chunked && length.is_some() {
        return Err(invalid("ambiguous HTTP body framing"));
    }
    if method == "GET" && status == 200 && length.is_none() && !chunked {
        return Err(invalid("a successful GET has no explicit HTTP body length"));
    }
    Ok(ResponseHead { status, body: end.saturating_add(4), length, chunked })
}

/// Decode a complete HTTP/1.1 response after the peer has closed its connection.
/// Never treat a short body as a complete persisted manifest or pack.
fn decode_response<A: Allocator>(
    method: &str,
    path: &str,
    mut reply: Vec<u8, A>,
    err_alloc: WireAlloc,
) -> Result<(u16, Vec<u8, A>), Error> {
    let ResponseHead { status, body, length, chunked } =
        decode_head(method, path, &reply, err_alloc)?;
    let invalid = |why: &str| {
        let message = format_in!(err_alloc, "{method} {path}: {why}");
        if method == "GET" && status == 200 {
            Error::Committed(message)
        } else {
            Error::Protocol(message)
        }
    };
    let body_len = reply.len().saturating_sub(body);
    let limit = response_body_limit(method, path, status);
    if chunked {
        reply.copy_within(body.., 0);
        reply.truncate(body_len);
        let decoded = unchunk(reply, limit)
            .ok_or_else(|| invalid("a broken or oversized chunked body"))?;
        if decoded.len().widen_to::<u64>() > limit {
            return Err(invalid("chunked body exceeds the documented response body limit"));
        }
        return Ok((status, decoded));
    }
    if body_len.widen_to::<u64>() > limit {
        return Err(invalid("HTTP body exceeds the documented response body limit"));
    }
    if let Some(expected) = length
        && body_len != expected
    {
        return Err(invalid("Content-Length does not match the complete body"));
    }
    reply.copy_within(body.., 0);
    reply.truncate(body_len);
    Ok((status, reply))
}

fn unchunk<A: Allocator>(mut body: Vec<u8, A>, limit: u64) -> Option<Vec<u8, A>> {
    let mut read = 0usize;
    let mut written = 0usize;
    loop {
        let tail = body.get(read..)?;
        let rel_end = tail.windows(2).position(|it| it == b"\r\n")?;
        let end = read.checked_add(rel_end)?;
        let text = core::str::from_utf8(body.get(read..end)?).ok()?;
        let len = usize::from_str_radix(text.split(';').next()?.trim(), 16).ok()?;
        read = end.checked_add(2)?;
        if len == 0 {
            let rest = body.get(read..)?;
            if rest == b"\r\n" {
                body.truncate(written);
                return Some(body);
            }
            // Trailer fields are allowed, but a missing final CRLF or extra
            // bytes after the terminator are an invalid response.
            let trailers = core::str::from_utf8(rest).ok()?.strip_suffix("\r\n\r\n")?;
            if trailers.split("\r\n").all(|line| {
                line.split_once(':').is_some_and(|(name, _)| {
                    !name.is_empty() && !name.bytes().any(|byte| byte.is_ascii_whitespace())
                })
            }) {
                body.truncate(written);
                return Some(body);
            }
            return None;
        }
        // Check before moving bytes: the decoded budget is independent of the
        // larger allowance for HTTP framing.
        if len.widen_to::<u64>() > limit.saturating_sub(written.widen_to::<u64>()) {
            return None;
        }
        let end = read.checked_add(len)?;
        if !body.get(end..)?.starts_with(b"\r\n") {
            return None;
        }
        body.copy_within(read..end, written);
        written = written.checked_add(len)?;
        read = end.checked_add(2)?;
    }
}

/// The reply may be lost after the server has committed an immutable manifest.
/// A transport/framing failure is therefore an unknown publication outcome,
/// never evidence that it is safe to rebuild under the same key.
fn publication_response<A: Allocator>(
    path: &str,
    response: Result<(u16, Vec<u8, A>), Error>,
    err_alloc: WireAlloc,
) -> Result<(u16, Vec<u8, A>), Error> {
    response.map_err(|error| {
        Error::Committed(format_in!(
            err_alloc,
            "PUT {path}: publication outcome unknown after request failure: {error}"
        ))
    })
}

/// A first publisher wins the immutable key. A 412 proves that a value exists,
/// not that the value is identical; look up and compare it before accepting.
fn publish_verdict<A: Allocator>(
    status: u16,
    body: &[u8],
    path: &str,
    proposed: &[u8],
    err_alloc: WireAlloc,
    read_existing: impl FnOnce() -> Result<Option<Vec<u8, A>>, Error>,
    equivalent: impl FnOnce(&[u8], &[u8]) -> Result<bool, Error>,
) -> Result<(), Error> {
    match status {
        200..=299 => Ok(()),
        412 => {
            // The refused PUT proves an immutable key was already claimed. If the
            // subsequent GET fails, that uncertainty is not a cache miss and must
            // never authorize a local rebuild under the same key.
            let existing = read_existing().map_err(|error| {
                Error::Committed(format_in!(
                    err_alloc,
                    "PUT {path}: 412: cannot read the committed list: {error}"
                ))
            })?;
            match existing {
                Some(existing) if existing.as_slice() == proposed => Ok(()),
                Some(existing) => {
                    let same = equivalent(&existing, proposed).map_err(|error| {
                        Error::Committed(format_in!(
                            err_alloc,
                            "PUT {path}: 412: cannot verify committed packs: {error}"
                        ))
                    })?;
                    if same {
                        Ok(())
                    } else {
                        Err(Error::Committed(format_in!(
                            err_alloc,
                            "PUT {path}: 412: another publisher committed different values under this unit key"
                        )))
                    }
                }
                None => Err(Error::Committed(format_in!(
                    err_alloc,
                    "PUT {path}: 412: publication was refused but the committed list is missing"
                ))),
            }
        }
        _ => {
            // A non-success status is not proof that the server left the
            // immutable key unclaimed: publication may have committed before
            // a subsequent error response. Refuse local recompilation.
            Err(Error::Committed(format_in!(
                err_alloc,
                "PUT {path}: {status}: publication outcome unknown: {}",
                lossy_text(body, err_alloc)
            )))
        }
    }
}

#[cfg(test)]
mod tests {
    use core::cell::Cell;

    use super::*;
    use crate::pack::test_alloc::{Counted, Counts};

    fn test_alloc() -> WireAlloc {
        blaze_own::testing::heap().shared()
    }

    /// A socket surrogate that fails if a refusal consumes bytes past its
    /// budget. A decode-only check after read_to_end cannot satisfy this oracle.
    struct Budgeted<'a> {
        bytes: &'a [u8],
        fragment: usize,
        consumed: usize,
        budget: usize,
        exceeded: bool,
    }

    impl<'a> Budgeted<'a> {
        fn new(bytes: &'a [u8], fragment: usize, budget: usize) -> Self {
            Self { bytes, fragment, consumed: 0, budget, exceeded: false }
        }
    }

    impl ByteRead for Budgeted<'_> {
        fn read_bytes(&mut self, into: &mut [u8]) -> Result<usize, IoError> {
            if self.bytes.is_empty() || into.is_empty() {
                return Ok(0);
            }
            if self.consumed >= self.budget {
                self.exceeded = true;
                return Err(IoError::from_errno(Errno::NoBufs));
            }
            let n = into
                .len()
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
    fn http_bodies_keep_the_callers_allocation_through_decoding() {
        let alloc = test_alloc();
        let path = "/v1/list/manifests/unit";
        for reply in [
            b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nabcde".as_slice(),
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nabc\r\n2\r\nde\r\n0\r\n\r\n".as_slice(),
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n3;name=value\r\nabc\r\n2\r\nde\r\n0\r\nDigest: checked\r\n\r\n".as_slice(),
        ] {
            let counts = Counts::default();
            let bytes = Vec::from_slice_in(reply, Counted(&counts));
            let pointer = bytes.as_ptr();
            let capacity = bytes.capacity();
            let before = counts.allocations.get();
            let (status, body) = decode_response("GET", path, bytes, alloc).unwrap();
            assert_eq!(status, 200);
            assert_eq!(body.as_slice(), b"abcde");
            assert_eq!(body.as_ptr(), pointer, "decoding must compact the received allocation");
            assert_eq!(body.capacity(), capacity);
            assert!(core::ptr::eq(body.allocator().0, &counts));
            assert_eq!(counts.allocations.get(), before);
            assert_eq!(counts.deallocations.get(), 0);
            drop(body);
            assert_eq!(counts.allocations.get(), counts.deallocations.get());

            // The production reader receives the complete small response in one read.
            // A second allocation would expose a body copy after validating its framing.
            let counts = Counts::default();
            let (status, body) =
                read_response(&mut &reply[..], "GET", path, Counted(&counts), alloc).unwrap();
            assert_eq!(status, 200);
            assert_eq!(counts.allocations.get(), 1);
            assert!(core::ptr::eq(body.allocator().0, &counts));
            let (_, control) = read_response(&mut &reply[..], "GET", path, alloc, alloc).unwrap();
            assert_eq!(body.as_slice(), control.as_slice());
            assert_eq!(counts.allocations.get(), 1);
            drop(body);
            assert_eq!(counts.allocations.get(), counts.deallocations.get());
        }

        for reply in [
            b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nabc".as_slice(),
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nabc\r\n0\r\n\r\nextra".as_slice(),
        ] {
            let counts = Counts::default();
            assert!(matches!(
                read_response(&mut &reply[..], "GET", path, Counted(&counts), alloc),
                Err(Error::Committed(_))
            ));
            assert_eq!(counts.allocations.get(), counts.deallocations.get());
        }
    }

    #[test]
    fn response_limits_stop_the_reader_before_buffering_a_hostile_reply() {
        let alloc = test_alloc();
        let path = "/v1/blob/packs/test.pack";
        let mut oversized = Vec::from_slice_in(b"HTTP/1.1 200 OK\r\nX-Long: ", alloc);
        oversized.resize(2 * HTTP_HEADER_LIMIT, b'a');
        for fragment in [1, 3, 8192] {
            let mut stream = Budgeted::new(&oversized, fragment, HTTP_HEADER_LIMIT + 4);
            let error = read_response(&mut stream, "GET", path, alloc, alloc).unwrap_err();
            assert!(!stream.exceeded, "read past the early-refusal budget");
            assert!(matches!(&error, Error::Committed(why) if why.contains("headers exceed")));
            assert!(stream.consumed <= HTTP_HEADER_LIMIT + 4);
            assert!(matches!(
                crate::agent::decide(&blaze_wire::LocalPolicy::Fallback, error, alloc),
                Err(Error::Committed(_))
            ));
        }
        // Plant the previous buffering order: this bounded reader must catch it
        // before it can spend the blob allowance on header bytes.
        {
            let mut stream = Budgeted::new(&oversized, 8192, HTTP_HEADER_LIMIT + 4);
            let most = usize::try_from(response_body_limit("GET", path, 200) + HTTP_FRAMING_ROOM)
                .unwrap_or(usize::MAX);
            let mut buf = [0u8; 8192];
            let mut total = 0usize;
            while total <= most {
                match stream.read_bytes(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => total += n,
                    Err(_) => break,
                }
            }
            assert!(stream.exceeded, "the oracle must catch checking the head only after EOF");
        }

        let mut boundary = Vec::from_slice_in(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nX: ", alloc);
        boundary.resize(HTTP_HEADER_LIMIT, b'a');
        boundary.extend_from_slice(b"\r\n\r\n");
        let mut stream = boundary.as_slice();
        let (status, body) = read_response(&mut stream, "GET", path, alloc, alloc).unwrap();
        assert_eq!(status, 200);
        assert!(body.is_empty());

        for path in [path, "/v1/list/manifests/unit"] {
            let length = response_body_limit("GET", path, 200) + 1;
            let head = format_in!(alloc, "HTTP/1.1 200 OK\r\nContent-Length: {length}\r\n\r\n");
            let reply = format_in!(alloc, "{head}must not read this body");
            let mut stream = Budgeted::new(reply.as_bytes(), head.len(), head.len());
            assert!(matches!(
                read_response(&mut stream, "GET", path, alloc, alloc),
                Err(Error::Committed(why)) if why.contains("documented response body limit")
            ));
            assert!(!stream.exceeded);
            assert_eq!(stream.consumed, head.len());
        }
        // The same oracle catches reading past a valid declared length before
        // rejecting surplus bytes. The single excess byte proves the refusal.
        let head = b"HTTP/1.1 200 OK\r\nContent-Length: 1\r\n\r\n";
        let mut reply = Vec::from_slice_in(head, alloc);
        reply.extend_from_slice(b"abcd");
        let mut stream = Budgeted::new(&reply, head.len(), head.len() + 2);
        assert!(matches!(
            read_response(&mut stream, "GET", path, alloc, alloc),
            Err(Error::Committed(_))
        ));
        assert!(!stream.exceeded);
        assert_eq!(stream.consumed, head.len() + 2);
    }

    #[test]
    fn fragmented_replies_preserve_body_boundaries_and_committed_refusals() {
        let alloc = test_alloc();
        let path = "/v1/list/manifests/unit";
        for reply in [
            b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nabcde".as_slice(),
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nabc\r\n2\r\nde\r\n0\r\n\r\n".as_slice(),
            b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nabc".as_slice(),
            b"HTTP/1.1 404 Not Found\r\n\r\nabsent".as_slice(),
        ] {
            for fragment in [1, 2, 7, 8192] {
                let mut stream = Budgeted::new(reply, fragment, reply.len());
                match decode_response("GET", path, Vec::from_slice_in(reply, alloc), alloc) {
                    Ok(expected) => {
                        assert_eq!(
                            read_response(&mut stream, "GET", path, alloc, alloc).unwrap(),
                            expected
                        );
                    }
                    Err(Error::Committed(_)) => assert!(matches!(
                        read_response(&mut stream, "GET", path, alloc, alloc),
                        Err(Error::Committed(_))
                    )),
                    Err(other) => panic!("unexpected fixture refusal: {other}"),
                }
                assert!(!stream.exceeded);
            }
        }
        let chunks = b"3\r\nabc\r\n2\r\nde\r\n0\r\n\r\n";
        assert_eq!(unchunk(Vec::from_slice_in(chunks, alloc), 5).unwrap().as_slice(), b"abcde");
        assert!(
            unchunk(Vec::from_slice_in(chunks, alloc), 4).is_none(),
            "check the decoded budget before moving bytes"
        );
    }

    #[test]
    fn error_responses_to_blob_gets_cannot_claim_blob_sized_bodies() {
        let alloc = test_alloc();
        let path = "/v1/blob/packs/test.pack";
        let small = u64::from(Limits::DOCUMENTED.request);
        assert_eq!(response_body_limit("GET", path, 200), Limits::DOCUMENTED.blob);
        for status in [404, 500] {
            assert_eq!(response_body_limit("GET", path, status), small);
            let claimed = small + 1;
            let head = format_in!(
                alloc,
                "HTTP/1.1 {status} Error\r\nContent-Length: {claimed}\r\n\r\n"
            );
            // Even a peer sending its response one byte at a time must be
            // refused before any body bytes can be read.
            let mut stream = Budgeted::new(head.as_bytes(), 1, head.len());
            let error = read_response(&mut stream, "GET", path, alloc, alloc).unwrap_err();
            assert!(!stream.exceeded);
            assert!(matches!(
                error,
                Error::Protocol(why) if why.contains("documented response body limit")
            ));
            assert_eq!(stream.consumed, head.len());

            // Chunked bodies use the same status-aware decoded limit,
            // independent of their wire framing.
            let chunked = format_in!(
                alloc,
                "HTTP/1.1 {status} Error\r\nTransfer-Encoding: chunked\r\n\r\n{claimed:x}\r\n"
            );
            assert!(matches!(
                decode_response("GET", path, Vec::from_slice_in(chunked.as_bytes(), alloc), alloc),
                Err(Error::Protocol(why)) if why.contains("oversized chunked body")
            ));
        }
        // A real successful blob GET is not incorrectly capped at the
        // manifest/error limit; it still needs the full documented blob budget.
        let large = small + 1;
        let head = format_in!(alloc, "HTTP/1.1 200 OK\r\nContent-Length: {large}\r\n\r\n");
        assert_eq!(
            decode_head("GET", path, head.as_bytes(), alloc).unwrap().length,
            Some(usize::try_from(large).unwrap())
        );
    }

    #[test]
    fn committed_reply_bounds_follow_the_native_wire_contract() {
        let alloc = test_alloc();
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
        let oversized = format_in!(alloc, "HTTP/1.1 200 OK\r\nContent-Length: {claimed}\r\n\r\n");
        let refused =
            decode_response("GET", manifest, Vec::from_slice_in(oversized.as_bytes(), alloc), alloc)
                .unwrap_err();
        assert!(matches!(
            &refused,
            Error::Committed(why) if why.contains("documented response body limit")
        ));
        assert!(matches!(
            crate::agent::decide(&blaze_wire::LocalPolicy::Fallback, refused, alloc),
            Err(Error::Committed(_))
        ));
        let mut excessive_headers = Vec::from_slice_in(b"HTTP/1.1 200 OK\r\nX-Long: ", alloc);
        excessive_headers.extend(core::iter::repeat_n(b'a', HTTP_HEADER_LIMIT));
        excessive_headers.extend_from_slice(b"\r\n\r\n");
        assert!(matches!(
            decode_response("GET", manifest, excessive_headers, alloc),
            Err(Error::Committed(_))
        ));
    }

    #[test]
    fn an_http_reply_must_end_at_its_declared_body_boundary() {
        let alloc = test_alloc();
        let path = "/v1/list/manifests/unit";
        let fixed = b"HTTP/1.1 200 OK\r\nContent-Length: 6\r\n\r\na b c\n";
        let (status, body) =
            decode_response("GET", path, Vec::from_slice_in(fixed, alloc), alloc).unwrap();
        assert_eq!(status, 200);
        assert_eq!(body.as_slice(), b"a b c\n");
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
            assert!(matches!(
                decode_response("GET", path, Vec::from_slice_in(bytes, alloc), alloc),
                Err(Error::Committed(_))
            ));
        }

        let chunks =
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nabc\r\n2\r\nde\r\n0\r\n\r\n";
        let (status, body) =
            decode_response("GET", path, Vec::from_slice_in(chunks, alloc), alloc).unwrap();
        assert_eq!(status, 200);
        assert_eq!(body.as_slice(), b"abcde");
        for bytes in [
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nabcXY0\r\n\r\n".as_slice(),
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nabc\r\n0\r\n".as_slice(),
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nabc\r\n0\r\n\r\nextra"
                .as_slice(),
        ] {
            assert!(matches!(
                decode_response("GET", path, Vec::from_slice_in(bytes, alloc), alloc),
                Err(Error::Committed(_))
            ));
        }
        // A legitimate 404 does not establish the existence of committed data.
        let (status, body) = decode_response(
            "GET",
            path,
            Vec::from_slice_in(b"HTTP/1.1 404 Not Found\r\n\r\nnot here", alloc),
            alloc,
        )
        .unwrap();
        assert_eq!(status, 404);
        assert_eq!(body.as_slice(), b"not here");
    }

    #[test]
    fn an_interrupted_committed_get_cannot_authorize_local_recompilation() {
        let alloc = test_alloc();
        let path = "/v1/list/manifests/test-unit";
        for bytes in [
            b"HTTP/1.1 200 OK\r\nContent-Length: 20\r\n".as_slice(),
            b"HTTP/1.1 200 OK\r\nContent-Length: 20\r\n\r\npartial".as_slice(),
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n10\r\npart".as_slice(),
        ] {
            let refused =
                read_failure("GET", path, bytes, IoError::from_errno(Errno::TimedOut), alloc);
            assert!(
                matches!(&refused, Error::Committed(why) if why.contains(path)),
                "partial HTTP 200 must be a committed-state refusal: {refused}"
            );
            assert!(matches!(
                crate::agent::decide(&blaze_wire::LocalPolicy::Fallback, refused, alloc),
                Err(Error::Committed(_))
            ));
        }
        for bytes in [
            b"HTTP/1.1 200 OK\r\nContent-Length: 20\r\n".as_slice(),
            b"HTTP/1.1 200 OK\r\nSome-Header: \xff\r\n\r\n".as_slice(),
        ] {
            assert!(matches!(
                decode_response("GET", path, Vec::from_slice_in(bytes, alloc), alloc),
                Err(Error::Committed(_))
            ));
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
                read_failure(method, path, partial, IoError::from_errno(Errno::TimedOut), alloc),
                Error::Io(_)
            ));
        }
    }

    #[test]
    fn a_lost_publication_reply_cannot_authorize_local_recompilation() {
        let alloc = test_alloc();
        let path = "/v1/list/manifests/test-unit";
        for error in [
            Error::Io(IoError::from_errno(Errno::TimedOut)),
            Error::Protocol(WireString::from_str_in("the publication reply was truncated", alloc)),
        ] {
            let refused = publication_response::<WireAlloc>(path, Err(error), alloc).unwrap_err();
            assert!(
                matches!(&refused, Error::Committed(why)
                    if why.contains(path) && why.contains("outcome unknown")),
                "the failed publication must identify the uncertain immutable key: {refused}"
            );
            assert!(
                matches!(
                    crate::agent::decide(&blaze_wire::LocalPolicy::Fallback, refused, alloc),
                    Err(Error::Committed(_))
                ),
                "a lost publication acknowledgement cannot authorize local recompilation"
            );
        }

        assert_eq!(
            publication_response(path, Ok((201, Vec::new_in(alloc))), alloc).unwrap().0,
            201
        );
    }

    #[test]
    fn a_first_writer_conflict_must_compare_the_immutable_manifest() {
        let alloc = test_alloc();
        let path = "/v1/list/manifests/test-unit";
        let cases: [(Option<&[u8]>, bool); 3] =
            [(Some(b"same"), true), (Some(b"different"), false), (None, false)];
        for (committed, accepted) in cases {
            let read = Cell::new(false);
            let verdict = publish_verdict::<WireAlloc>(
                412,
                b"",
                path,
                b"same",
                alloc,
                || {
                    read.set(true);
                    Ok(committed.map(|bytes| Vec::from_slice_in(bytes, alloc)))
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
                        crate::agent::decide(&blaze_wire::LocalPolicy::Fallback, error, alloc),
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
            Error::Io(IoError::with_message(
                Errno::TimedOut,
                WireString::from_str_in("verification timed out", alloc),
            )),
            Error::Protocol(WireString::from_str_in(
                "verification response was malformed",
                alloc,
            )),
        ] {
            let refused = publish_verdict::<WireAlloc>(
                412,
                b"",
                path,
                b"same",
                alloc,
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
                    crate::agent::decide(&blaze_wire::LocalPolicy::Fallback, refused, alloc),
                    Err(Error::Committed(_))
                ),
                "failed verification must not authorize local recompilation"
            );
        }
        for verify_error in [
            Error::Io(IoError::from_errno(Errno::TimedOut)),
            Error::Protocol(WireString::from_str_in("committed packs could not be decoded", alloc)),
        ] {
            let refused = publish_verdict::<WireAlloc>(
                412,
                b"",
                path,
                b"proposed partition",
                alloc,
                || Ok(Some(Vec::from_slice_in(b"existing partition", alloc))),
                |_, _| Err(verify_error),
            )
            .unwrap_err();
            assert!(matches!(
                crate::agent::decide(&blaze_wire::LocalPolicy::Fallback, refused, alloc),
                Err(Error::Committed(_))
            ));
        }
        let read = Cell::new(false);
        let result = publish_verdict::<WireAlloc>(
            201,
            b"",
            path,
            b"same",
            alloc,
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
            let refused = publish_verdict::<WireAlloc>(
                status,
                b"unavailable",
                path,
                b"same",
                alloc,
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
                crate::agent::decide(&blaze_wire::LocalPolicy::Fallback, refused, alloc),
                Err(Error::Committed(_))
            ));
        }
        // Control: the old Protocol classification would authorize exactly
        // the forbidden local rebuild after this uncertain publication.
        assert!(
            crate::agent::decide(
                &blaze_wire::LocalPolicy::Fallback,
                Error::Protocol(WireString::from_str_in("unknown publication outcome", alloc)),
                alloc,
            )
            .is_ok()
        );
    }
}
