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

use super::transfer::{
    FIRST_WAIT, Object, RESUMES, Refused, StalePartial, TooLong, Traffic, Version, is_final,
};

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
    limits: Limits,
}

impl Serve {
    /// `blaze+http://host:port[/prefix]`, as the server's `hello` answers. The address is the
    /// one `ra-jit serve --listen` prints.
    pub(crate) async fn open(
        spec: &Url,
        patience: Duration,
        traffic: Arc<Traffic>,
    ) -> anyhow::Result<Serve> {
        if spec.scheme() != "blaze+http" {
            bail!("the cache server supports blaze+http only; TLS is not implemented");
        }
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
        let mut serve = Serve { client, base, patience, traffic, limits: Limits::DOCUMENTED };
        let hello = serve.hello().await?;
        serve.limits = checked_limits(hello.limits)?;
        Ok(serve)
    }

    pub(crate) fn lookup_limit(&self) -> usize {
        self.limits.lookup_items as usize
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
        if asked.len() > self.lookup_limit() {
            bail!("the missing request exceeds the server's lookup limit");
        }
        let request = CacheMissingRequest { blobs: Set::new(asked.to_vec()) };
        let body = encode(&request);
        let (status, bytes) = self
            .whole(Method::POST, "/v1/cache/missing", Some(body), empty(), Duration::ZERO)
            .await?;
        if status != StatusCode::OK {
            bail!("{} says {status}: {}", self.base, said(&bytes));
        }
        let answer: CacheMissingAnswer = decode(&bytes, &self.limits).map_err(|e| {
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
                    return Err(Refused(format!(
                        "{} sends a range of {name} that does not start at {offset}",
                        self.base
                    ))
                    .into());
                }
                Ok(Some(super::transfer::Body::Http(body)))
            }
            // What is asked for past the end is an object with nothing left: a client that has
            // the whole of a blob and asks for its rest gets the end of it, which is nothing.
            StatusCode::RANGE_NOT_SATISFIABLE => {
                let len = headers
                    .get(header::CONTENT_RANGE)
                    .and_then(|it| it.to_str().ok())
                    .and_then(|it| it.strip_prefix("bytes */"))
                    .and_then(|it| it.parse::<u64>().ok());
                if let Some(len) = len
                    && len < offset
                {
                    return Err(StalePartial { name: name.to_owned(), offset, len }.into());
                }
                if offset == 0 || len != Some(offset) {
                    return Err(Refused(format!(
                        "{} sends an invalid end-of-blob range for {name}",
                        self.base
                    ))
                    .into());
                }
                Ok(Some(super::transfer::Body::Nothing))
            }
            StatusCode::NOT_FOUND => Ok(None),
            StatusCode::OK => {
                bail!("{} sends the whole of {name} when asked for its rest", self.base)
            }
            // What the server refuses it refuses each time, and it says why in its answer.
            it if it.is_client_error() => {
                let (_, _, bytes) = Answer {
                    status,
                    headers,
                    body,
                    patience: self.patience,
                    most: self.limits.request as usize,
                }
                .whole()
                .await?;
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
        let most = most.min(if path.starts_with("/v1/blob/") {
            self.limits.blob
        } else {
            u64::from(self.limits.request)
        });
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
            let (_, _, bytes) = Answer {
                status,
                headers,
                body,
                patience: self.patience,
                most: self.limits.request as usize,
            }
            .whole()
            .await?;
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
        let answer: ObjectsAnswer = decode(&bytes, &self.limits).map_err(|e| {
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
            if is_final(&failed) {
                return Err(failed);
            }
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
        let most = if request.uri().path().starts_with("/v1/blob/") {
            self.limits.blob
        } else {
            u64::from(self.limits.request)
        };
        if hyper::body::Body::size_hint(request.body()).upper().is_some_and(|len| len > most) {
            return Err(
                Refused("the request exceeds the server's advertised limit".to_owned()).into()
            );
        }
        self.traffic.operations.fetch_add(1, Ordering::Relaxed);
        let patience = self.patience + extra;
        let answer = tokio::time::timeout(patience, self.client.request(request))
            .await
            .with_context(|| format!("no answer for {patience:?} from {}", self.base))?
            .with_context(|| format!("asking {}", self.base))?;
        Ok(Answer::of(answer, patience, self.limits.request as usize))
    }
}

/// The answer to one request, before it is read.
struct Answer {
    status: StatusCode,
    headers: hyper::HeaderMap,
    body: Incoming,
    patience: Duration,
    most: usize,
}

impl Answer {
    fn of(answer: Response<Incoming>, patience: Duration, most: usize) -> Answer {
        let (parts, body) = answer.into_parts();
        Answer { status: parts.status, headers: parts.headers, body, patience, most }
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
            if bytes.len().saturating_add(chunk.len()) > self.most {
                bail!("the server's answer exceeds the advertised request limit");
            }
            bytes.extend_from_slice(&chunk);
        }
        Ok((self.status, self.headers, bytes))
    }
}

fn checked_limits(limits: Limits) -> anyhow::Result<Limits> {
    let ceiling = Limits::DOCUMENTED;
    if limits.request == 0
        || limits.request > ceiling.request
        || limits.event == 0
        || limits.event > ceiling.event
        || limits.blob < 32
        || limits.blob > ceiling.blob
        || limits.lookup_items == 0
        || limits.lookup_items > ceiling.lookup_items
        || limits.batch_items == 0
        || limits.batch_items > ceiling.batch_items
        || limits.lookup_items > limits.batch_items
        || limits.string == 0
        || limits.string > ceiling.string
    {
        bail!("the server advertised invalid or unsupported wire limits");
    }
    Ok(limits)
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

#[cfg(test)]
mod tests {
    use super::*;
    use hyper::service::service_fn;
    use hyper_util::rt::TokioIo;
    use std::{collections::VecDeque, sync::Mutex};
    use tokio::net::TcpListener;

    type Reply = (StatusCode, Vec<(&'static str, String)>, Vec<u8>);

    struct Fake {
        url: Url,
        task: tokio::task::JoinHandle<()>,
        requests: Arc<Mutex<Vec<(String, Option<String>, Vec<u8>)>>>,
    }

    impl Fake {
        async fn start(hello: Vec<u8>, replies: Vec<Reply>) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let url =
                Url::parse(&format!("blaze+http://{}", listener.local_addr().unwrap())).unwrap();
            let replies = Arc::new(Mutex::new(VecDeque::from(replies)));
            let requests = Arc::new(Mutex::new(Vec::new()));
            let seen = requests.clone();
            let task = tokio::spawn(async move {
                let mut connections = tokio::task::JoinSet::new();
                loop {
                    let (stream, _) = listener.accept().await.unwrap();
                    let (hello, replies, requests) = (hello.clone(), replies.clone(), seen.clone());
                    connections.spawn(async move {
                        let service = service_fn(move |request: Request<Incoming>| {
                            let (hello, replies, requests) =
                                (hello.clone(), replies.clone(), requests.clone());
                            async move {
                                let (parts, body) = request.into_parts();
                                let bytes = body.collect().await.unwrap().to_bytes().to_vec();
                                let range = parts
                                    .headers
                                    .get(header::RANGE)
                                    .map(|it| it.to_str().unwrap().to_owned());
                                requests.lock().unwrap().push((
                                    parts.uri.to_string(),
                                    range,
                                    bytes,
                                ));
                                let (status, headers, body) = if parts.uri.path() == "/v1/hello" {
                                    (StatusCode::OK, Vec::new(), hello)
                                } else {
                                    replies
                                        .lock()
                                        .unwrap()
                                        .pop_front()
                                        .expect("a scripted response")
                                };
                                let mut response = Response::builder().status(status);
                                for (key, value) in headers {
                                    response = response.header(key, value);
                                }
                                Ok::<_, std::convert::Infallible>(
                                    response.body(Full::new(Bytes::from(body))).unwrap(),
                                )
                            }
                        });
                        _ = hyper::server::conn::http1::Builder::new()
                            .serve_connection(TokioIo::new(stream), service)
                            .await;
                    });
                }
            });
            Self { url, task, requests }
        }

        async fn client(&self) -> anyhow::Result<Serve> {
            Serve::open(&self.url, Duration::from_secs(2), Arc::new(Traffic::new(4))).await
        }
    }

    impl Drop for Fake {
        fn drop(&mut self) {
            self.task.abort();
        }
    }

    fn hello(limits: Limits) -> HelloAnswer {
        HelloAnswer {
            version: VERSION,
            key: [0; 32],
            epochs: Set::new(Vec::new()),
            identities: Set::new(Vec::new()),
            limits,
        }
    }

    fn reply(status: StatusCode, bytes: Vec<u8>) -> Reply {
        (status, Vec::new(), bytes)
    }

    #[tokio::test]
    async fn client_refuses_https_before_any_connection() {
        let dir = tempfile::tempdir().unwrap();
        let error = super::super::Remote::open("blaze+https://127.0.0.1:1", dir.path())
            .await
            .err()
            .unwrap();
        assert!(error.to_string().contains("TLS is not implemented"));
        let url = Url::parse("blaze+https://127.0.0.1:1").unwrap();
        assert!(
            Serve::open(&url, Duration::from_secs(1), Arc::new(Traffic::new(1)))
                .await
                .err()
                .unwrap()
                .to_string()
                .contains("blaze+http only")
        );
    }

    #[tokio::test]
    async fn client_rejects_wrong_hello_version_and_malformed_envelope() {
        let mut wrong = hello(Limits::DOCUMENTED);
        wrong.version = VERSION + 1;
        let fake = Fake::start(encode(&wrong), Vec::new()).await;
        assert!(fake.client().await.err().unwrap().to_string().contains("speaks version"));
        let fake = Fake::start(b"\xa1\x00".to_vec(), Vec::new()).await;
        assert!(fake.client().await.err().unwrap().to_string().contains("not the schema"));
    }

    #[tokio::test]
    async fn client_rejects_invalid_hello_limits() {
        let d = Limits::DOCUMENTED;
        let invalid = [
            Limits { request: 0, ..d },
            Limits { request: d.request + 1, ..d },
            Limits { blob: 0, ..d },
            Limits { blob: 31, ..d },
            Limits { blob: d.blob + 1, ..d },
            Limits { lookup_items: 0, ..d },
            Limits { lookup_items: d.lookup_items + 1, ..d },
            Limits { batch_items: 0, ..d },
            Limits { batch_items: d.batch_items + 1, ..d },
            Limits { lookup_items: 2, batch_items: 1, ..d },
            Limits { event: 0, ..d },
            Limits { event: d.event + 1, ..d },
            Limits { string: 0, ..d },
            Limits { string: d.string + 1, ..d },
        ];
        for limits in invalid {
            let fake = Fake::start(encode(&hello(limits)), Vec::new()).await;
            assert!(
                fake.client().await.err().unwrap().to_string().contains("wire limits"),
                "{limits:?}"
            );
        }
    }

    #[tokio::test]
    async fn client_uses_negotiated_lookup_request_and_blob_limits() {
        let limits = Limits { request: 256, blob: 64, lookup_items: 2, ..Limits::DOCUMENTED };
        let answer = CacheMissingAnswer { missing: Set::new(vec![[1; 32]]) };
        let fake = Fake::start(
            encode(&hello(limits)),
            vec![
                reply(StatusCode::OK, encode(&answer)),
                reply(StatusCode::OK, vec![0; 257]),
                reply(StatusCode::OK, vec![0; 65]),
            ],
        )
        .await;
        let client = fake.client().await.unwrap();
        assert_eq!(client.lookup_limit(), 2);
        assert!(client.missing(&[[1; 32], [2; 32], [3; 32]]).await.is_err());
        assert_eq!(fake.requests.lock().unwrap().len(), 1);
        assert_eq!(client.missing(&[[1; 32], [2; 32]]).await.unwrap(), vec![[1; 32]]);
        assert!(client.read_small("manifests/default", 1024).await.err().unwrap().is::<TooLong>());
        let name =
            format!("units/{}", [1; 32].iter().map(|it| format!("{it:02x}")).collect::<String>());
        assert!(client.read_small(&name, 1024).await.err().unwrap().is::<TooLong>());
        assert!(client.write_small("manifests/default", vec![0; 257], None).await.is_err());
        assert_eq!(fake.requests.lock().unwrap().len(), 4);
    }

    #[tokio::test]
    async fn client_rejects_unasked_missing_digest_and_bounded_answer_overflow() {
        let limits = Limits { request: 256, ..Limits::DOCUMENTED };
        let wrong = CacheMissingAnswer { missing: Set::new(vec![[9; 32]]) };
        let fake = Fake::start(
            encode(&hello(limits)),
            vec![reply(StatusCode::OK, encode(&wrong)), reply(StatusCode::OK, vec![0; 257])],
        )
        .await;
        let client = fake.client().await.unwrap();
        assert!(client.missing(&[[1; 32]]).await.err().unwrap().to_string().contains("not asked"));
        assert!(client.missing(&[[1; 32]]).await.err().unwrap().to_string().contains("exceeds"));
    }

    #[tokio::test]
    async fn remote_missing_batches_use_the_negotiated_lookup_limit() {
        let limits = Limits { lookup_items: 2, ..Limits::DOCUMENTED };
        let empty = encode(&CacheMissingAnswer { missing: Set::new(Vec::new()) });
        let fake = Fake::start(
            encode(&hello(limits)),
            vec![
                reply(StatusCode::OK, empty.clone()),
                reply(StatusCode::OK, empty.clone()),
                reply(StatusCode::OK, empty),
            ],
        )
        .await;
        let dir = tempfile::tempdir().unwrap();
        let remote = super::super::Remote::open(fake.url.as_str(), dir.path()).await.unwrap();
        let asked: Vec<_> = (0..5).map(|at| super::super::Digest::from_wire([at; 32])).collect();
        assert!(remote.missing(&asked).await.unwrap().is_empty());
        let sizes: Vec<_> = fake
            .requests
            .lock()
            .unwrap()
            .iter()
            .skip(1)
            .map(|(_, _, bytes)| decode::<CacheMissingRequest>(bytes, &limits).unwrap().blobs.len())
            .collect();
        assert_eq!(sizes, [2, 2, 1]);
    }

    #[tokio::test]
    async fn client_checks_partial_start_and_416_end_before_accepting_a_body() {
        let partial = |range: &str| {
            (
                StatusCode::PARTIAL_CONTENT,
                vec![("content-range", range.to_owned())],
                b"rest".to_vec(),
            )
        };
        let end = |range: &str| {
            (
                StatusCode::RANGE_NOT_SATISFIABLE,
                vec![("content-range", range.to_owned())],
                Vec::new(),
            )
        };
        let fake = Fake::start(
            encode(&hello(Limits::DOCUMENTED)),
            vec![
                partial("bytes 1-7/8"),
                partial("bytes 4-7/8"),
                end("bytes */2"),
                end("bytes */4"),
                reply(StatusCode::RANGE_NOT_SATISFIABLE, Vec::new()),
            ],
        )
        .await;
        let client = fake.client().await.unwrap();
        let name = format!("units/{}", "01".repeat(32));
        assert!(client.read_from(&name, 4).await.is_err());
        let mut body = client.read_from(&name, 4).await.unwrap().unwrap();
        assert_eq!(body.next().await.unwrap().unwrap(), b"rest");
        assert!(client.read_from(&name, 4).await.is_err());
        let mut body = client.read_from(&name, 4).await.unwrap().unwrap();
        assert!(body.next().await.unwrap().is_none());
        assert!(client.read_from(&name, 4).await.is_err());
        for (_, range, _) in fake.requests.lock().unwrap().iter().skip(1) {
            assert_eq!(range.as_deref(), Some("bytes=4-"));
        }
    }

    #[tokio::test]
    async fn fetch_restarts_a_too_long_saved_prefix_without_disabling_other_transfers() {
        for compressed in [false, true] {
            let bytes = b"rajitpk\x03a verified pack";
            let encoded =
                if compressed { zstd::bulk::compress(bytes, 3).unwrap() } else { bytes.to_vec() };
            let want = super::super::Digest::of(&encoded);
            let name = if compressed {
                format!("packs/{}.pack.zst", super::super::Digest::of(bytes))
            } else {
                format!("units/{want}")
            };
            let stale_len = encoded.len() + 3;
            let fake = Fake::start(
                encode(&hello(Limits::DOCUMENTED)),
                vec![
                    (
                        StatusCode::RANGE_NOT_SATISFIABLE,
                        vec![("content-range", format!("bytes */{}", encoded.len()))],
                        Vec::new(),
                    ),
                    reply(StatusCode::OK, encoded.clone()),
                    reply(StatusCode::OK, b"unrelated".to_vec()),
                ],
            )
            .await;
            let dir = tempfile::tempdir().unwrap();
            let to = dir.path().join("verified");
            let partial = to.with_file_name(if compressed {
                "verified.zst.tmp"
            } else {
                "verified.fetch.tmp"
            });
            std::fs::write(&partial, vec![b'x'; stale_len]).unwrap();
            let remote = super::super::Remote::open(fake.url.as_str(), dir.path()).await.unwrap();
            let unpacked =
                compressed.then(|| (super::super::Digest::of(bytes), bytes.len() as u64));
            let fetched = tokio::time::timeout(
                Duration::from_secs(3),
                remote.fetch_coded(&name, &want, unpacked, &to, (stale_len + 10) as u64, |_| true),
            )
            .await
            .unwrap()
            .unwrap();
            assert!(matches!(fetched, super::super::Fetched::Whole(_)));
            assert_eq!(std::fs::read(&to).unwrap(), bytes);
            assert!(!partial.exists());
            let other = dir.path().join("unrelated");
            let want = super::super::Digest::of(b"unrelated");
            remote
                .fetch_file(&format!("units/{want}"), &want, &other, 100, |_| true)
                .await
                .unwrap();
            assert_eq!(std::fs::read(other).unwrap(), b"unrelated");
            assert_eq!(remote.traffic.resumed.load(Ordering::Relaxed), 0);
            let requests = fake.requests.lock().unwrap();
            assert_eq!(requests.len(), 4);
            assert_eq!(requests[1].1, Some(format!("bytes={stale_len}-")));
            assert!(requests[2].1.is_none());
            assert!(requests[3].1.is_none());
        }
    }

    #[tokio::test]
    async fn fetch_refuses_a_bad_clean_restart_and_discards_the_saved_prefix() {
        let fake = Fake::start(
            encode(&hello(Limits::DOCUMENTED)),
            vec![
                (
                    StatusCode::RANGE_NOT_SATISFIABLE,
                    vec![("content-range", "bytes */4".to_owned())],
                    Vec::new(),
                ),
                reply(StatusCode::OK, b"wrong".to_vec()),
                reply(StatusCode::OK, b"other".to_vec()),
            ],
        )
        .await;
        let dir = tempfile::tempdir().unwrap();
        let to = dir.path().join("verified");
        let partial = dir.path().join("verified.fetch.tmp");
        std::fs::write(&partial, b"obsolete prefix").unwrap();
        let remote = super::super::Remote::open(fake.url.as_str(), dir.path()).await.unwrap();
        let want = super::super::Digest::of(b"correct");
        let error = remote
            .fetch_file(&format!("units/{want}"), &want, &to, 100, |_| true)
            .await
            .err()
            .unwrap();
        assert!(error.to_string().contains("rejected"));
        assert!(!to.exists());
        assert!(!partial.exists());
        let want = super::super::Digest::of(b"other");
        remote.fetch_file(&format!("units/{want}"), &want, &to, 100, |_| true).await.unwrap();
        assert_eq!(std::fs::read(&to).unwrap(), b"other");
        assert_eq!(fake.requests.lock().unwrap().len(), 4);
        assert_eq!(remote.traffic.resumed.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn a_malformed_416_is_final_and_does_not_keep_a_partial_or_poison_traffic() {
        let fake = Fake::start(
            encode(&hello(Limits::DOCUMENTED)),
            vec![
                reply(StatusCode::RANGE_NOT_SATISFIABLE, Vec::new()),
                reply(StatusCode::OK, b"correct".to_vec()),
            ],
        )
        .await;
        let dir = tempfile::tempdir().unwrap();
        let to = dir.path().join("verified");
        let partial = dir.path().join("verified.fetch.tmp");
        std::fs::write(&partial, b"saved").unwrap();
        let remote = super::super::Remote::open(fake.url.as_str(), dir.path()).await.unwrap();
        let want = super::super::Digest::of(b"correct");
        let name = format!("units/{want}");
        let error = remote.fetch_file(&name, &want, &to, 100, |_| true).await.err().unwrap();
        assert!(error.is::<Refused>());
        assert!(!partial.exists());
        assert!(!to.exists());
        remote.fetch_file(&name, &want, &to, 100, |_| true).await.unwrap();
        assert_eq!(std::fs::read(&to).unwrap(), b"correct");
        assert_eq!(fake.requests.lock().unwrap().len(), 3);
        assert_eq!(remote.traffic.resumed.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn an_oversize_local_upload_is_refused_without_network_retries() {
        let limits = Limits { blob: 64, ..Limits::DOCUMENTED };
        let fake = Fake::start(encode(&hello(limits)), Vec::new()).await;
        let client = fake.client().await.unwrap();
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("upload");
        std::fs::write(&file, vec![0; 65]).unwrap();
        let error = tokio::time::timeout(
            Duration::from_secs(2),
            client.send_file(&format!("units/{}", "01".repeat(32)), &file, 65),
        )
        .await
        .unwrap()
        .unwrap_err();
        assert!(error.is::<Refused>());
        assert_eq!(fake.requests.lock().unwrap().len(), 1);
        assert_eq!(client.traffic.resumed.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn gc_reads_a_manifest_replaced_between_listing_and_fetch_and_keeps_real_errors() {
        let dir = tempfile::tempdir().unwrap();
        let legacy = "manifests/default.123-456.tmp";
        std::fs::write(dir.path().join("old"), b"cut").unwrap();
        let server =
            Arc::new(blaze_serve::Server::open(&blaze_serve::Config::at(dir.path(), 0)).unwrap());
        let store = server.store();
        store.put_list("manifests/default", b"", blaze_serve::Expect::Any).unwrap();
        std::fs::write(dir.path().join(legacy), b"cut").unwrap();
        let names = store.list("manifests/").unwrap();
        assert_eq!(names.len(), 1);
        let listing = blaze_serve::ObjectsAnswer {
            objects: vec![blaze_serve::ListedObject {
                name: names[0].name.clone(),
                len: names[0].len,
                written_ms: 0,
            }],
        };
        store.put_list("manifests/default", b"\n", blaze_serve::Expect::Any).unwrap();
        let current = store.read_small("manifests/default", 100).unwrap().unwrap();
        let empty_listing = encode(&blaze_serve::ObjectsAnswer { objects: Vec::new() });
        let fake = Fake::start(
            encode(&hello(Limits::DOCUMENTED)),
            vec![
                reply(StatusCode::OK, empty_listing.clone()),
                reply(StatusCode::OK, encode(&listing)),
                reply(StatusCode::OK, current.clone()),
                reply(StatusCode::OK, empty_listing.clone()),
                reply(StatusCode::OK, empty_listing.clone()),
                reply(StatusCode::OK, empty_listing.clone()),
                reply(StatusCode::OK, encode(&listing)),
                reply(StatusCode::OK, current.clone()),
                reply(StatusCode::OK, empty_listing.clone()),
            ],
        )
        .await;
        let client = fake.client().await.unwrap();
        let place = super::super::Place::Serve(Box::new(client));
        let remote = super::super::Remote {
            turns: super::super::Turns::of(dir.path(), &place.name()),
            place,
            traffic: Arc::new(Traffic::new(4)),
            patience: Duration::from_secs(2),
            read_only: false,
            trust: super::super::tests::taking_unsigned(),
            object_max: 1024,
        };
        let ages = super::super::collect::Ages {
            unlisted: Duration::from_secs(86400),
            listed: None,
            dry: true,
        };
        let result = super::super::collect::collect(&remote, &ages).await;
        assert!(result.is_ok(), "{result:?}");
        assert!(!fake.requests.lock().unwrap().iter().any(|(path, _, _)| path.contains("123-456")));
        let fake = Fake::start(
            encode(&hello(Limits::DOCUMENTED)),
            vec![
                reply(StatusCode::OK, empty_listing.clone()),
                reply(StatusCode::OK, encode(&listing)),
                reply(StatusCode::OK, current),
                reply(StatusCode::OK, empty_listing.clone()),
                reply(StatusCode::OK, empty_listing.clone()),
                reply(StatusCode::OK, empty_listing),
                reply(StatusCode::OK, encode(&listing)),
                reply(StatusCode::NOT_FOUND, Vec::new()),
            ],
        )
        .await;
        let client = fake.client().await.unwrap();
        let place = super::super::Place::Serve(Box::new(client));
        let remote = super::super::Remote {
            turns: super::super::Turns::of(dir.path(), &place.name()),
            place,
            traffic: Arc::new(Traffic::new(4)),
            patience: Duration::from_secs(2),
            read_only: false,
            trust: super::super::tests::taking_unsigned(),
            object_max: 1024,
        };
        let error = super::super::collect::collect(&remote, &ages).await.unwrap_err();
        assert!(error.to_string().contains("went away while it was read"));
    }
}
