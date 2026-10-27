//! The `Serve` place: a cache server that `ra-jit serve` runs, over `blaze+http://host:port`.
//!
//! The server is today's remote layout with the checks a shared one needs, so this place does
//! what the `Dir` place does, one HTTP request at a time: a blob is read from any byte on, a
//! small object is read whole and written back only if nobody wrote first, a listing is asked
//! for a prefix, and `missing` answers for a batch of digests from the server's memory.
//!
//! The wire is `docs/distributed.md` section 8: `hello` and `missing` are deterministic CBOR
//! as `blaze-wire` writes it, a blob's bytes are its own, and a list's bytes are its own text.
//! A blob is named as the `Dir` place names it (`packs/<sha256>.pack` and the rest), so a push,
//! a pull, a lazy build and the keeper are the same code they are for a directory.
//!
//! Nothing here trusts the server with code: what it sends is hashed against the name it was
//! listed under before it is used (`transfer.rs`), and a list is only ever read as text.

use std::{
    path::PathBuf,
    sync::{Arc, atomic::Ordering},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, bail};
use blaze_wire::{
    CacheMissingAnswer, CacheMissingRequest, Digest as WireDigest, HelloAnswer, HelloRequest,
    Limits, Set, VERSION, decode, encode,
};
use http_body_util::{BodyExt, Empty, Full, StreamBody, combinators::BoxBody};
use hyper::{
    Method, Request, Response, StatusCode,
    body::{Bytes, Frame, Incoming},
    header,
};
use hyper_util::{
    client::legacy::{Client, connect::HttpConnector},
    rt::TokioExecutor,
};
use tokio::io::AsyncReadExt;
use url::Url;

use super::transfer::{FIRST_WAIT, Object, RESUMES, Refused, TooLong, Traffic, Version};

/// The bytes of one read of a file that is sent.
const CHUNK: usize = 1 << 16;

/// The header the server says when an object was written in, in milliseconds since the epoch,
/// so that a client reads a time as a number rather than as a date in a header it parses.
const WRITTEN_MS: &str = "x-blaze-written-ms";

/// The body of a request or an answer: nothing, some bytes, or a file as it is read.
pub(super) type Body = BoxBody<Bytes, std::io::Error>;

// Today's listing extension has one shared schema, owned by the server crate.
use blaze_serve::ObjectsAnswer;

pub(crate) struct Serve {
    client: Client<HttpConnector, Body>,
    /// With no `/` at its end.
    base: String,
    patience: Duration,
    traffic: Arc<Traffic>,
}

impl Serve {
    /// `blaze+http://host:port[/prefix]`, as the server's `hello` answers. The address is the
    /// one `ra-jit serve --listen` prints.
    pub(crate) async fn open(
        spec: &Url,
        patience: Duration,
        traffic: Arc<Traffic>,
    ) -> anyhow::Result<Serve> {
        let host = spec.host_str().context("the address of the server names no host")?;
        let port = spec.port().context("the address of the server names no port")?;
        if !spec.username().is_empty()
            || spec.password().is_some()
            || spec.query().is_some()
            || spec.fragment().is_some()
            || !matches!(spec.path(), "" | "/")
        {
            bail!(
                "the cache server address must be blaze+http://host:port without credentials, prefix or query"
            );
        }
        let base = format!("http://{host}:{port}{}", spec.path().trim_end_matches('/'));
        let client = Client::builder(TokioExecutor::new()).build(HttpConnector::new());
        let serve = Serve { client, base, patience, traffic };
        serve.hello().await?;
        Ok(serve)
    }

    /// What tells this remote from another on this machine: where it is, and not how it is
    /// reached.
    pub(crate) fn place(&self) -> String {
        format!("blaze+{}", self.base)
    }

    fn address(&self, path: &str) -> String {
        format!("{}{path}", self.base)
    }

    /// `POST /v1/hello`: the version this client speaks, and what the server answers. A server
    /// that speaks none of the versions this ra-jit speaks is refused by name, not used as if
    /// it did.
    async fn hello(&self) -> anyhow::Result<HelloAnswer> {
        let request = HelloRequest {
            versions: Set::new(vec![VERSION]),
            // C3 carries no identity: the store's identity enters the wire in C5, and this
            // server serves every identity until then.
            identity: blaze_wire::StoreIdentity([0; 32]),
            platform: platform(),
            role: 0,
        };
        let body = encode(&request);
        let (status, bytes) =
            self.whole(Method::POST, "/v1/hello", Some(body), empty(), Duration::ZERO).await?;
        if status != StatusCode::OK {
            bail!("{} says {status}: {}", self.base, said(&bytes));
        }
        let answer: HelloAnswer = decode(&bytes, &Limits::DOCUMENTED).map_err(|e| {
            anyhow::anyhow!(
                "`{}` answers a hello that is not the schema's: {}",
                self.base,
                e.offence()
            )
        })?;
        if answer.version != VERSION {
            bail!(
                "`{}` speaks version {}, and this ra-jit speaks {VERSION}",
                self.base,
                answer.version
            );
        }
        Ok(answer)
    }

    /// The digests of `asked` that the server does not hold, from its existence set. One
    /// request for the whole batch: what a push of five hundred packs would otherwise ask one
    /// `HEAD` at a time.
    pub(crate) async fn missing(&self, asked: &[WireDigest]) -> anyhow::Result<Vec<WireDigest>> {
        let request = CacheMissingRequest { blobs: Set::new(asked.to_vec()) };
        let body = encode(&request);
        let (status, bytes) = self
            .whole(Method::POST, "/v1/cache/missing", Some(body), empty(), Duration::ZERO)
            .await?;
        if status != StatusCode::OK {
            bail!("{} says {status}: {}", self.base, said(&bytes));
        }
        let answer: CacheMissingAnswer = decode(&bytes, &Limits::DOCUMENTED).map_err(|e| {
            anyhow::anyhow!(
                "`{}` answers a missing that is not the schema's: {}",
                self.base,
                e.offence()
            )
        })?;
        let missing = answer.missing.into_vec();
        if missing.iter().any(|it| !asked.contains(it)) {
            bail!("the server's missing answer names a blob that was not asked for");
        }
        Ok(missing)
    }

    /// The object under `name` from `offset` on, or `None` when there is nothing under it.
    pub(crate) async fn read_from(
        &self,
        name: &str,
        offset: u64,
    ) -> anyhow::Result<Option<super::transfer::Body>> {
        let path = object_path(name)?;
        let mut request = Request::builder().method(Method::GET).uri(self.address(&path));
        if offset != 0 {
            request = request.header(header::RANGE, format!("bytes={offset}-"));
        }
        let answer = self.call(request, empty(), Duration::ZERO).await?;
        let (status, headers, body) = answer.into_parts();
        match status {
            StatusCode::OK if offset == 0 => Ok(Some(super::transfer::Body::Http(body))),
            StatusCode::PARTIAL_CONTENT if offset != 0 => {
                let range = headers.get(header::CONTENT_RANGE).and_then(|it| it.to_str().ok());
                let first = range
                    .and_then(|it| it.strip_prefix("bytes "))
                    .and_then(|it| it.split_once('-'))
                    .and_then(|(first, _)| first.parse::<u64>().ok());
                if first != Some(offset) {
                    bail!("{} sends a range of {name} that does not start at {offset}", self.base);
                }
                Ok(Some(super::transfer::Body::Http(body)))
            }
            // What is asked for past the end is an object with nothing left: a client that has
            // the whole of a blob and asks for its rest gets the end of it, which is nothing.
            StatusCode::RANGE_NOT_SATISFIABLE => Ok(Some(super::transfer::Body::Nothing)),
            StatusCode::NOT_FOUND => Ok(None),
            StatusCode::OK => {
                bail!("{} sends the whole of {name} when asked for its rest", self.base)
            }
            // What the server refuses it refuses each time, and it says why in its answer.
            it if it.is_client_error() => {
                let bytes = body.collect().await.map(|it| it.to_bytes()).unwrap_or_default();
                Err(Refused(format!("{} {name}: {it}: {}", self.base, said(&bytes))).into())
            }
            it => bail!("{} {name}: {it}", self.base),
        }
    }

    /// The whole of a small object and the version it was read at, when it is at most `most`
    /// bytes. The version is the hash of the bytes, which is what a write of the list back
    /// must say it read.
    pub(crate) async fn read_small(
        &self,
        name: &str,
        most: u64,
    ) -> anyhow::Result<Option<(Vec<u8>, Version)>> {
        let path = object_path(name)?;
        let request = Request::builder().method(Method::GET).uri(self.address(&path));
        let answer = self.call(request, empty(), Duration::ZERO).await?;
        let (status, headers, mut body) = answer.into_parts();
        if status == StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if status == StatusCode::PAYLOAD_TOO_LARGE {
            return Err(TooLong { name: name.to_owned(), most }.into());
        }
        if status != StatusCode::OK {
            let bytes = body.collect().await.map(|it| it.to_bytes()).unwrap_or_default();
            return Err(Refused(format!("{} {name}: {status}: {}", self.base, said(&bytes))).into());
        }
        let version = headers
            .get(header::ETAG)
            .and_then(|it| it.to_str().ok())
            .map(|it| it.trim_matches('"').to_owned());
        let mut bytes = Vec::new();
        while let Some(chunk) = tokio::time::timeout(self.patience, super::https::next(&mut body))
            .await
            .context("the server's object read stalled")??
        {
            if (bytes.len() + chunk.len()) as u64 > most {
                return Err(TooLong { name: name.to_owned(), most }.into());
            }
            self.traffic.received.fetch_add(chunk.len() as u64, Ordering::Relaxed);
            bytes.extend_from_slice(&chunk);
        }
        Ok(Some((bytes, Version(version))))
    }

    /// The object under `name` as a listing would have it, if there is one.
    pub(crate) async fn found(&self, name: &str) -> anyhow::Result<Option<Object>> {
        let path = object_path(name)?;
        let request = Request::builder().method(Method::HEAD).uri(self.address(&path));
        let answer = self.call(request, empty(), Duration::ZERO).await?;
        let (status, headers, _) = answer.into_parts();
        if status == StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if status != StatusCode::OK {
            return Err(Refused(format!("{} {name}: {status}", self.base)).into());
        }
        let len = headers
            .get(header::CONTENT_LENGTH)
            .and_then(|it| it.to_str().ok())
            .and_then(|it| it.parse().ok())
            .unwrap_or(0);
        let written = headers
            .get(WRITTEN_MS)
            .and_then(|it| it.to_str().ok())
            .and_then(|it| it.parse::<u64>().ok())
            .map_or_else(SystemTime::now, |ms| UNIX_EPOCH + Duration::from_millis(ms));
        Ok(Some(Object { name: name.to_owned(), len, written }))
    }

    /// The objects whose names start with `prefix`, which ends a directory.
    pub(crate) async fn list(&self, prefix: &str) -> anyhow::Result<Vec<Object>> {
        let path = format!("/v1/objects?prefix={}", escape(prefix));
        let request = Request::builder().method(Method::GET).uri(self.address(&path));
        let answer = self.call(request, empty(), Duration::ZERO).await?;
        let (status, _, bytes) = answer.whole().await?;
        if status != StatusCode::OK {
            bail!("{} does not list {prefix}: {status}", self.base);
        }
        let answer: ObjectsAnswer = decode(&bytes, &Limits::DOCUMENTED).map_err(|e| {
            anyhow::anyhow!(
                "`{}` answers a listing that is not the schema's: {}",
                self.base,
                e.offence()
            )
        })?;
        Ok(answer
            .objects
            .into_iter()
            .map(|it| Object {
                name: it.name,
                len: it.len,
                written: UNIX_EPOCH + Duration::from_millis(it.written_ms),
            })
            .collect())
    }

    /// Removes the object under `name`.
    pub(crate) async fn delete(&self, name: &str) -> anyhow::Result<()> {
        let path = object_path(name)?;
        let request = Request::builder().method(Method::DELETE).uri(self.address(&path));
        let answer = self.call(request, empty(), Duration::ZERO).await?;
        let (status, _, bytes) = answer.whole().await?;
        match status {
            StatusCode::NO_CONTENT | StatusCode::NOT_FOUND => Ok(()),
            it if it.is_client_error() => {
                Err(Refused(format!("{} {name}: {it}", self.base)).into())
            }
            it => bail!("{} {name}: {it}: {}", self.base, said(&bytes)),
        }
    }

    /// Writes a small object, unless what is there is no longer what was read. `None` for what
    /// was read is a name that nothing may be under.
    pub(crate) async fn write_small(
        &self,
        name: &str,
        bytes: Vec<u8>,
        read_at: Option<&Version>,
    ) -> anyhow::Result<super::Written> {
        let path = object_path(name)?;
        let len = bytes.len() as u64;
        let mut request = Request::builder().method(Method::PUT).uri(self.address(&path));
        request = match read_at.and_then(|it| it.0.as_deref()) {
            Some(tag) => request.header(header::IF_MATCH, format!("\"{tag}\"")),
            None => request.header(header::IF_NONE_MATCH, "*"),
        };
        let answer = self.call(request, full(bytes), Duration::ZERO).await?;
        let (status, _, body) = answer.whole().await?;
        match status {
            StatusCode::NO_CONTENT | StatusCode::OK => {
                self.traffic.sent.fetch_add(len, Ordering::Relaxed);
                Ok(super::Written::Done)
            }
            // The condition failed: somebody else wrote first, and the caller reads again and
            // sees there is nothing of its own to add.
            StatusCode::PRECONDITION_FAILED | StatusCode::CONFLICT => Ok(super::Written::Lost),
            it if it.is_client_error() => {
                Err(Refused(format!("{} {name}: {it}: {}", self.base, said(&body))).into())
            }
            it => bail!("{} {name}: {it}: {}", self.base, said(&body)),
        }
    }

    /// Puts the file under `name`, all of it or nothing: the server names it only when it has
    /// every byte and the bytes are what the name says.
    ///
    /// A push asks again when the line broke or the server answered 5xx, with the waits a
    /// broken transfer is taken up after: the other paths of a push are wrapped by
    /// `Remote::patiently`, and this one is not.
    pub(crate) async fn send_file(
        &self,
        name: &str,
        file: &PathBuf,
        len: u64,
    ) -> anyhow::Result<()> {
        let _permit = self.traffic.permit().await?;
        let path = object_path(name)?;
        // A pack of `len` bytes is given as long as it takes at 64 KiB a second, on top of the
        // patience a request is given: a slow line is not a remote that is down.
        let patience = Duration::from_secs(len / (64 << 10));
        let mut again = 0;
        loop {
            let request = Request::builder().method(Method::PUT).uri(self.address(&path));
            let body = file_body(file.clone(), len).await?;
            let asked = match self.call(request, body, patience).await {
                Ok(answer) => answer.whole().await,
                Err(error) => Err(error),
            };
            let failed = match asked {
                Ok((status, _, _)) if matches!(status, StatusCode::NO_CONTENT | StatusCode::OK) => {
                    self.traffic.sent.fetch_add(len, Ordering::Relaxed);
                    return Ok(());
                }
                // What the server refuses it refuses each time: a wrong hash, a name that is
                // not the layout's, a body longer than a blob may be.
                Ok((status, _, bytes)) if status.is_client_error() => {
                    return Err(Refused(format!(
                        "{} {name}: {status}: {}",
                        self.base,
                        said(&bytes)
                    ))
                    .into());
                }
                Ok((status, _, bytes)) => {
                    anyhow::anyhow!("{} {name}: {status}: {}", self.base, said(&bytes))
                }
                Err(e) => e,
            };
            if again == RESUMES {
                bail!("{failed:#}, after asking {again} times more");
            }
            tokio::time::sleep(FIRST_WAIT * 2u32.pow(again)).await;
            again += 1;
            self.traffic.resumed.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// One message of section 8, and its answer's bytes.
    async fn whole(
        &self,
        method: Method,
        path: &str,
        body: Option<Vec<u8>>,
        empty_body: Body,
        extra: Duration,
    ) -> anyhow::Result<(StatusCode, Vec<u8>)> {
        let request = Request::builder()
            .method(method)
            .uri(self.address(path))
            .header(header::CONTENT_TYPE, "application/cbor");
        let body = body.map_or(empty_body, full);
        let answer = self.call(request, body, extra).await?;
        let (status, _, bytes) = answer.whole().await?;
        Ok((status, bytes))
    }

    /// One request, counted and held to the patience the remote was given.
    async fn call(
        &self,
        request: hyper::http::request::Builder,
        body: Body,
        extra: Duration,
    ) -> anyhow::Result<Answer> {
        let request = request.body(body)?;
        self.traffic.operations.fetch_add(1, Ordering::Relaxed);
        let patience = self.patience + extra;
        let answer = tokio::time::timeout(patience, self.client.request(request))
            .await
            .with_context(|| format!("no answer for {patience:?} from {}", self.base))?
            .with_context(|| format!("asking {}", self.base))?;
        Ok(Answer::of(answer, patience))
    }
}

/// The answer to one request, before it is read.
struct Answer {
    status: StatusCode,
    headers: hyper::HeaderMap,
    body: Incoming,
    patience: Duration,
}

impl Answer {
    fn of(answer: Response<Incoming>, patience: Duration) -> Answer {
        let (parts, body) = answer.into_parts();
        Answer { status: parts.status, headers: parts.headers, body, patience }
    }

    fn into_parts(self) -> (StatusCode, hyper::HeaderMap, Incoming) {
        (self.status, self.headers, self.body)
    }

    /// The body, whole.
    async fn whole(mut self) -> anyhow::Result<(StatusCode, hyper::HeaderMap, Vec<u8>)> {
        let mut bytes = Vec::new();
        while let Some(chunk) =
            tokio::time::timeout(self.patience, super::https::next(&mut self.body))
                .await
                .context("the server's answer stalled")??
        {
            if bytes.len().saturating_add(chunk.len()) > Limits::DOCUMENTED.request as usize {
                bail!("the server's answer exceeds the advertised request limit");
            }
            bytes.extend_from_slice(&chunk);
        }
        Ok((self.status, self.headers, bytes))
    }
}

/// The platform a `hello` carries. C3's server serves every platform and refuses none; the
/// fields of the real one enter the wire with the build spec (C2). This says what the machine
/// this process runs on is, in the shape section 8 gives.
fn platform() -> blaze_wire::ExecPlatform {
    let triple = host_triple();
    blaze_wire::ExecPlatform {
        triple,
        os: std::env::consts::OS.to_owned(),
        sdk: None,
        libc: None,
        linker: (String::new(), String::new(), [0; 32]),
        cpu: std::env::consts::ARCH.to_owned(),
    }
}

/// The triple of the machine this process runs on, as rustc names it.
fn host_triple() -> String {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => "aarch64-apple-darwin".to_owned(),
        ("macos", "x86_64") => "x86_64-apple-darwin".to_owned(),
        ("linux", "x86_64") => "x86_64-unknown-linux-gnu".to_owned(),
        ("linux", "aarch64") => "aarch64-unknown-linux-gnu".to_owned(),
        ("windows", "x86_64") => "x86_64-pc-windows-msvc".to_owned(),
        (os, arch) => format!("{arch}-{os}"),
    }
}

/// The path an object is asked for: a blob's or a list's, by the name today's layout gives it.
fn object_path(name: &str) -> anyhow::Result<String> {
    let (dir, rest) = name
        .split_once('/')
        .with_context(|| format!("`{name}` is not an object of the server's layout"))?;
    let kind = match dir {
        "packs" | "keys" | "units" => "blob",
        "manifests" | "deps" => "list",
        other => bail!("`{other}/` is not a directory of a remote"),
    };
    if rest.is_empty() || rest.contains(['/', '\\']) || rest.starts_with('.') {
        bail!("`{name}` is not an object of the server's layout");
    }
    Ok(format!("/v1/{kind}/{dir}/{}", escape(rest)))
}

fn empty() -> Body {
    Empty::<Bytes>::new().map_err(|never| match never {}).boxed()
}

fn full(bytes: Vec<u8>) -> Body {
    Full::new(Bytes::from(bytes)).map_err(|never| match never {}).boxed()
}

/// A body of a known length, which the client says in `Content-Length` rather than in chunked
/// framing: the length is what a server holds a blob to before it reads it.
struct Sized {
    inner: Body,
    len: u64,
}

impl hyper::body::Body for Sized {
    type Data = Bytes;
    type Error = std::io::Error;

    fn poll_frame(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        std::pin::Pin::new(&mut self.inner).poll_frame(cx)
    }

    fn size_hint(&self) -> hyper::body::SizeHint {
        hyper::body::SizeHint::with_exact(self.len)
    }

    fn is_end_stream(&self) -> bool {
        self.len == 0 || self.inner.is_end_stream()
    }
}

/// The file `path` of `len` bytes, as it is read: a pack is not held in memory to be sent.
async fn file_body(path: PathBuf, len: u64) -> anyhow::Result<Body> {
    let file = tokio::fs::File::open(&path)
        .await
        .with_context(|| format!("reading {}", path.display()))?;
    let chunks = futures_util::stream::unfold((file, len), |(mut file, left)| async move {
        if left == 0 {
            return None;
        }
        let mut buffer = vec![0; CHUNK.min(left as usize)];
        match file.read(&mut buffer).await {
            Ok(0) => {
                Some((Err(std::io::Error::from(std::io::ErrorKind::UnexpectedEof)), (file, 0)))
            }
            Ok(read) => {
                buffer.truncate(read);
                Some((Ok(Frame::data(Bytes::from(buffer))), (file, left - read as u64)))
            }
            Err(e) => Some((Err(e), (file, 0))),
        }
    });
    let inner: Body = StreamBody::new(chunks).boxed();
    Ok(BoxBody::new(Sized { inner, len }))
}

/// What an error answer says, for a message: section 8's `WireError`, or the text of an answer
/// that is not one.
fn said(bytes: &[u8]) -> String {
    match decode::<blaze_wire::WireError>(bytes, &Limits::DOCUMENTED) {
        Ok(error) => format!("{}: {}", error.code, error.message),
        Err(_) => String::from_utf8_lossy(bytes).trim().to_owned(),
    }
}

/// `text` with everything that is not plain in a query escaped.
fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for byte in text.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => {
                out.push(byte as char);
            }
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}
