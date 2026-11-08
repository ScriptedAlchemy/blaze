//! A blocking client of the cache server (`blaze-serve`) for what an agent moves: a pack by
//! digest and a unit's published list of packs. One request per connection, each bounded by
//! `timeout`; the server checks every blob against its name before it is visible, and the
//! agent checks it again against what was committed ([`crate::pack::read`]).

use std::io::{Read as _, Write as _};
use std::net::{SocketAddr, TcpStream};
use std::time::Duration;

use crate::Error;
use crate::pack::{Pack, hex};
use blaze_wire::Digest;

/// The most bytes one pack or list may be: what a client reads before refusing.
const MOST: u64 = 16 << 30;

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
        let mut stream = TcpStream::connect_timeout(&self.address, self.timeout)?;
        stream.set_read_timeout(Some(self.timeout))?;
        stream.set_write_timeout(Some(self.timeout))?;
        stream.set_nodelay(true)?;
        let head = format!(
            "{method} {path} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\nContent-Length: {}\r\n\r\n",
            self.address,
            body.len()
        );
        stream.write_all(head.as_bytes())?;
        stream.write_all(body)?;
        stream.flush()?;
        // Read one byte past the cap: reaching an exact read limit is not EOF.
        // A truncated manifest must never masquerade as a complete published unit.
        let mut reply = Vec::new();
        stream.take(MOST + 1).read_to_end(&mut reply).map_err(|error| {
            read_failure(method, path, &reply, error)
        })?;
        if reply.len() as u64 > MOST {
            let message = format!("{method} {path}: HTTP reply exceeds {MOST} bytes");
            return Err(if committed_get_head(method, &reply) {
                Error::Committed(message)
            } else {
                Error::Protocol(message)
            });
        }
        decode_response(method, path, &reply)
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

    /// Publishes the list of the unit named `spec`. Another agent may win the
    /// first-writer race, but the same unit key must never name different packs.
    pub fn publish(&self, spec: &Digest, list: &[u8]) -> Result<(), Error> {
        let path = format!("/v1/list/manifests/{}", hex(spec));
        // After sending a publication, a lost response cannot tell us whether
        // the server committed the immutable key. It cannot authorize fallback.
        let (status, body) = publication_response(&path, self.request("PUT", &path, list))?;
        publish_verdict(status, &body, &path, list, || self.published(spec))
    }
}

/// A GET whose response already started with HTTP 200 names committed data.
/// A read error at that point must never turn into permission to rebuild locally
/// under the immutable key. Earlier IO failures remain subject to local policy.
fn committed_get_head(method: &str, reply: &[u8]) -> bool {
    if method != "GET" {
        return false;
    }
    let remaining = reply.strip_prefix(b"HTTP/1.1 200")
        .or_else(|| reply.strip_prefix(b"HTTP/1.0 200"));
    remaining.is_some_and(|after| {
        after.is_empty() || matches!(after[0], b' ' | b'\r' | b'\n')
    })
}

fn read_failure(method: &str, path: &str, reply: &[u8], error: std::io::Error) -> Error {
    if committed_get_head(method, reply) {
        // alloc: an error path names the committed key and the interrupted transport.
        Error::Committed(format!("{method} {path}: interrupted committed HTTP 200 reply: {error}"))
    } else {
        Error::Io(error)
    }
}

/// Decode a complete HTTP/1.1 response after the peer has closed its connection.
/// Never treat a short body as a complete persisted manifest or pack.
fn decode_response(method: &str, path: &str, reply: &[u8]) -> Result<(u16, Vec<u8>), Error> {
    let invalid = |why: &str| Error::Protocol(format!("{method} {path}: {why}"));
    let end = reply
        .windows(4)
        .position(|it| it == b"\r\n\r\n")
        .ok_or_else(|| {
            if committed_get_head(method, reply) {
                // alloc: an incomplete committed reply cannot authorize local recompilation.
                Error::Committed(format!("{method} {path}: truncated committed HTTP 200 headers"))
            } else {
                invalid("no HTTP head")
            }
        })?;
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
        let (name, value) =
            line.split_once(':').ok_or_else(|| invalid("malformed HTTP header"))?;
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
    let body = &reply[end + 4..];
    if chunked {
        if length.is_some() {
            return Err(invalid("ambiguous HTTP body framing"));
        }
        return Ok((
            status,
            unchunk(body).ok_or_else(|| invalid("a broken chunked body"))?,
        ));
    }
    if method == "GET" && status == 200 && length.is_none() {
        return Err(invalid("a successful GET has no explicit HTTP body length"));
    }
    if let Some(expected) = length {
        if body.len() != expected {
            return Err(invalid("Content-Length does not match the complete body"));
        }
    }
    Ok((status, body.to_vec()))
}

fn unchunk(mut body: &[u8]) -> Option<Vec<u8>> {
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
) -> Result<(), Error> {
    match status {
        200..=299 => Ok(()),
        412 => {
            // The refused PUT proves an immutable key was already claimed. If the
            // subsequent GET fails, that uncertainty is not a cache miss and must
            // never authorize a local rebuild under the same key.
            let existing = read_existing().map_err(|error| {
                Error::Committed(format!("PUT {path}: 412: cannot read the committed list: {error}"))
            })?;
            match existing {
                Some(existing) if existing.as_slice() == proposed => Ok(()),
                Some(_) => Err(Error::Committed(format!(
                    "PUT {path}: 412: another publisher committed different packs under this unit key"
                ))),
                None => Err(Error::Committed(format!(
                    "PUT {path}: 412: publication was refused but the committed list is missing"
                ))),
            }
        },
        _ => Err(Error::Protocol(format!(
            "PUT {path}: {status}: {}",
            String::from_utf8_lossy(body)
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    #[test]
    fn an_http_reply_must_end_at_its_declared_body_boundary() {
        let path = "/v1/list/manifests/unit";
        let fixed = b"HTTP/1.1 200 OK\r\nContent-Length: 6\r\n\r\na b c\n";
        assert_eq!(
            decode_response("GET", path, fixed).unwrap(),
            (200, b"a b c\n".to_vec())
        );
        let broken = [
            b"HTTP/1.1 200 OK\r\nContent-Length: 6\r\n\r\na b\n".as_slice(),
            b"HTTP/1.1 200 OK\r\nContent-Length: 6\r\n\r\na b c\nextra".as_slice(),
            b"HTTP/1.1 200 OK\r\nContent-Length: 6\r\nContent-Length: 6\r\n\r\na b c\n".as_slice(),
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nContent-Length: 6\r\n\r\n0\r\n\r\n".as_slice(),
            b"HTTP/1.1 200 OK\r\nConnection: close\r\n\r\na b c\n".as_slice(),
            b"HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n".as_slice(),
        ];
        for bytes in broken {
            assert!(matches!(
                decode_response("GET", path, bytes),
                Err(Error::Committed(_))
            ));
        }

        let chunks = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nabc\r\n2\r\nde\r\n0\r\n\r\n";
        assert_eq!(
            decode_response("GET", path, chunks).unwrap(),
            (200, b"abcde".to_vec())
        );
        for bytes in [
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nabcXY0\r\n\r\n".as_slice(),
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nabc\r\n0\r\n".as_slice(),
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nabc\r\n0\r\n\r\nextra".as_slice(),
        ] {
            assert!(matches!(
                decode_response("GET", path, bytes),
                Err(Error::Committed(_))
            ));
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
                "GET", path, bytes, std::io::Error::from(std::io::ErrorKind::TimedOut),
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
                read_failure(method, path, partial, std::io::Error::from(std::io::ErrorKind::TimedOut)),
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
        let cases: [(Option<&[u8]>, bool); 3] = [
            (Some(b"same"), true),
            (Some(b"different"), false),
            (None, false),
        ];
        for (committed, accepted) in cases {
            let read = Cell::new(false);
            let verdict = publish_verdict(412, b"", path, b"same", || {
                read.set(true);
                Ok(committed.map(|bytes| bytes.to_vec())) // alloc: test manifest fixture
            });
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
            let refused = publish_verdict(412, b"", path, b"same", || Err(read_error))
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
        let read = Cell::new(false);
        let result = publish_verdict(201, b"", path, b"same", || {
            read.set(true);
            panic!("an accepted PUT must not issue a GET");
        });
        assert!(result.is_ok());
        assert!(!read.get());
        assert!(matches!(
            publish_verdict(500, b"unavailable", path, b"same", || {
                panic!("a failed PUT must not issue a GET");
            }),
            Err(Error::Protocol(_))
        ));
    }
}
