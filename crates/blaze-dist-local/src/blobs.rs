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
        stream.take(MOST + 1).read_to_end(&mut reply)?;
        if reply.len() as u64 > MOST {
            return Err(Error::Protocol(format!(
                "{method} {path}: HTTP reply exceeds {MOST} bytes"
            )));
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
        let (status, body) = self.request("PUT", &path, list)?;
        publish_verdict(status, &body, &path, list, || self.published(spec))
    }
}

/// Decode a complete HTTP/1.1 response after the peer has closed its connection.
/// Never treat a short body as a complete persisted manifest or pack.
fn decode_response(method: &str, path: &str, reply: &[u8]) -> Result<(u16, Vec<u8>), Error> {
    let invalid = |why: &str| Error::Protocol(format!("{method} {path}: {why}"));
    let end = reply
        .windows(4)
        .position(|it| it == b"\r\n\r\n")
        .ok_or_else(|| invalid("no HTTP head"))?;
    let head = std::str::from_utf8(&reply[..end]).map_err(|_| invalid("the head is not text"))?;
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
        412 => match read_existing()? {
            Some(existing) if existing.as_slice() == proposed => Ok(()),
            Some(_) => Err(Error::Committed(format!(
                "PUT {path}: 412: another publisher committed different packs under this unit key"
            ))),
            None => Err(Error::Committed(format!(
                "PUT {path}: 412: publication was refused but the committed list is missing"
            ))),
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
