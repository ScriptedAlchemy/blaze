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
use sha2::{Digest as _, Sha256};

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

    /// Publishes the list of the unit named `spec`. A list another agent published first stays:
    /// both name the same keys, so either reads the same (412 is not an error).
    pub fn publish(&self, spec: &Digest, list: &[u8]) -> Result<(), Error> {
        let path = format!("/v1/list/manifests/{}", hex(spec));
        let (status, body) = self.request("PUT", &path, list)?;
        match status {
            200..=299 | 412 => Ok(()),
            _ => Err(Error::Protocol(format!(
                "PUT {path}: {status}: {}",
                String::from_utf8_lossy(&body)
            ))),
        }
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
