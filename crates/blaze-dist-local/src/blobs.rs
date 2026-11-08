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
        let mut reply = Vec::new();
        stream.take(MOST).read_to_end(&mut reply)?;
        let end = reply
            .windows(4)
            .position(|it| it == b"\r\n\r\n")
            .ok_or_else(|| Error::Protocol(format!("{method} {path}: no HTTP head")))?;
        let head = std::str::from_utf8(&reply[..end])
            .map_err(|_| Error::Protocol(format!("{method} {path}: the head is not text")))?;
        let status = head
            .split_ascii_whitespace()
            .nth(1)
            .and_then(|it| it.parse().ok())
            .ok_or_else(|| Error::Protocol(format!("{method} {path}: no status")))?;
        if head.to_ascii_lowercase().contains("transfer-encoding: chunked") {
            return Ok((
                status,
                unchunk(&reply[end + 4..]).ok_or_else(|| {
                    Error::Protocol(format!("{method} {path}: a broken chunked body"))
                })?,
            ));
        }
        Ok((status, reply[end + 4..].to_vec()))
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

fn unchunk(mut body: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    loop {
        let end = body.windows(2).position(|it| it == b"\r\n")?;
        let text = std::str::from_utf8(&body[..end]).ok()?;
        let len = usize::from_str_radix(text.split(';').next()?.trim(), 16).ok()?;
        body = &body[end + 2..];
        if len == 0 {
            return Some(out);
        }
        out.extend_from_slice(body.get(..len)?);
        body = body.get(len + 2..)?;
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
            Some(_) => Err(Error::Protocol(format!(
                "PUT {path}: 412: another publisher committed different packs under this unit key"
            ))),
            None => Err(Error::Protocol(format!(
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
                    matches!(&error, Error::Protocol(why) if why.contains(path)),
                    "the refusal must name the conflicting unit: {error}"
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
