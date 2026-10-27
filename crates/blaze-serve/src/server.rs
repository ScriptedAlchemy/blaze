//! The HTTP/1.1 side: section 8's calls over `hyper`, and the answers they get.
//!
//! One connection is one task. A message is read whole, held to [`Limits::request`] and
//! refused by the validator when it is not canonical CBOR; a `PUT` of a blob is streamed to a
//! temporary file as it arrives, so a blob larger than the memory of the machine is still
//! taken, and nothing of it is named until it is checked against its name.
//!
//! Every answer that is not a success is section 8's [`WireError`] in CBOR, with the status of
//! its code's class. A blob's bytes are not CBOR; the bytes of a list are the list's own text.

use std::{
    convert::Infallible,
    io::SeekFrom,
    net::SocketAddr,
    path::PathBuf,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use blaze_wire::{
    CacheMissingAnswer, CacheMissingRequest, Digest, ErrorCode, HelloAnswer, HelloRequest, Limits,
    Namespace, Set, VERSION, Wire, decode, encode,
};
use bytes::Bytes;
use ed25519_dalek::{SigningKey, VerifyingKey};
use http_body_util::{BodyExt, Full, StreamBody, combinators::BoxBody};
use hyper::{
    Method, Request as HttpRequest, Response as HttpResponse, StatusCode,
    body::{Frame, Incoming},
    header::{self, HeaderName, HeaderValue},
    server::conn::http1,
    service::service_fn,
};
use hyper_util::rt::{TokioIo, TokioTimer};
use sha2::{Digest as _, Sha256};
use tokio::{
    io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::Semaphore,
};

use crate::blob::{ObjectName, hex, parse_range};
use crate::error::{Error, failed};
use crate::store::{Expect, Store, Verification, temporary};

mod pages;
mod preconditions;
mod publication;

use preconditions::Condition;
use publication::{Temporary, key_of};

/// The bytes of one read of a file that is sent.
const CHUNK: usize = 1 << 16;

/// The header an object's write time is answered in, in milliseconds since the epoch: a client
/// reads a number rather than parsing a date, and a collection goes by it.
const WRITTEN_MS: &str = "x-blaze-written-ms";

/// What the server is told when it starts.
#[derive(Clone, Debug)]
pub struct Config {
    /// The directory that holds today's layout: `packs/`, `keys/`, `units/`, `manifests/`.
    pub dir: PathBuf,
    /// Where it listens. `127.0.0.1:PORT` unless the operator says otherwise.
    pub listen: SocketAddr,
    /// Request and connection liveness policy, not a compile-worker memory limit.
    pub resources: Resources,
    /// Wire ceilings advertised in hello.
    pub limits: Limits,
}

/// Resource leases for network peers and listing snapshots.
#[derive(Clone, Debug)]
pub struct Resources {
    /// Longest pause between request-body frames or while reading request headers.
    pub idle: Duration,
    /// Concurrent accepted connections.
    pub connections: usize,
    /// Interval between sweeps of abandoned staging files.
    pub sweep_every: Duration,
    /// Age after which an unlocked staging file can be removed.
    pub temporary_age: Duration,
    /// Lifetime of an immutable listing snapshot.
    pub snapshot_age: Duration,
}

impl Default for Resources {
    fn default() -> Self {
        Self {
            idle: Duration::from_secs(30),
            connections: 256,
            sweep_every: Duration::from_secs(60),
            temporary_age: Duration::from_secs(3600),
            snapshot_age: Duration::from_secs(600),
        }
    }
}

impl Config {
    /// A server on `dir`, listening on `127.0.0.1:port`.
    pub fn at(dir: impl Into<PathBuf>, port: u16) -> Config {
        Config {
            dir: dir.into(),
            listen: SocketAddr::from(([127, 0, 0, 1], port)),
            resources: Resources::default(),
            limits: Limits::DOCUMENTED,
        }
    }
}

/// A running server's state, shared by its connections.
pub struct Server {
    store: Arc<Store>,
    key: SigningKey,
    limits: Limits,
    /// One epoch, Main's. C6 makes epochs real; until then a record that names one names 1.
    epochs: Vec<(Namespace, u64)>,
    resources: Resources,
    pages: pages::Pages,
}

impl Server {
    /// The server on `config.dir`, with its key read from the directory or made now.
    pub fn open(config: &Config) -> Result<Server, Error> {
        if config.resources.idle.is_zero()
            || config.resources.connections == 0
            || config.resources.sweep_every.is_zero()
            || config.resources.snapshot_age.is_zero()
            || config.limits.request == 0
            || config.limits.batch_items == 0
        {
            return Err(Error::malformed(
                "server resource leases and wire limits must be positive",
            ));
        }
        let limits = config.limits;
        let ceiling = Limits::DOCUMENTED;
        if limits.request > ceiling.request
            || limits.blob < 32
            || limits.blob > ceiling.blob
            || limits.event == 0
            || limits.event > ceiling.event
            || limits.lookup_items == 0
            || limits.lookup_items > limits.batch_items
            || limits.lookup_items > ceiling.lookup_items
            || limits.batch_items > ceiling.batch_items
            || limits.string == 0
            || limits.string > ceiling.string
        {
            return Err(Error::malformed("unsupported server wire limits"));
        }
        let store = Store::open(&config.dir)?;
        let key = key_of(&config.dir)?;
        Ok(Server {
            store: Arc::new(store),
            key,
            limits: config.limits,
            epochs: vec![(Namespace::Main, 1)],
            resources: config.resources.clone(),
            pages: pages::Pages::default(),
        })
    }

    /// The directory it serves.
    pub fn store(&self) -> &Store {
        &self.store
    }

    /// The key the `hello` answer carries, and the one C6 signs records with.
    pub fn verifying_key(&self) -> VerifyingKey {
        self.key.verifying_key()
    }

    /// The limits the `hello` answer carries.
    pub fn limits(&self) -> &Limits {
        &self.limits
    }

    async fn disk<T: Send + 'static>(
        &self,
        work: impl FnOnce(&Store) -> Result<T, Error> + Send + 'static,
    ) -> Result<T, Error> {
        let store = self.store.clone();
        tokio::task::spawn_blocking(move || work(&store))
            .await
            .map_err(|e| failed("cache filesystem work", e))?
    }

    /// One request, answered. This is what a connection calls.
    pub async fn answer(&self, request: HttpRequest<Incoming>) -> HttpResponse<Body> {
        let (parts, body) = request.into_parts();
        let path = parts.uri.path().to_owned();
        let query = parts.uri.query().unwrap_or_default().to_owned();
        let answered = self.route(&parts.method, &path, &query, &parts.headers, body).await;
        match answered {
            Ok(answer) => answer,
            Err(error) => error_answer(&error),
        }
    }

    async fn route(
        &self,
        method: &Method,
        path: &str,
        query: &str,
        headers: &hyper::HeaderMap,
        body: Incoming,
    ) -> Result<HttpResponse<Body>, Error> {
        match path {
            "/" => Ok(text(StatusCode::OK, "blaze-serve: a cache of today's layout\n")),
            "/v1/hello" => self.hello(method, headers, body).await,
            "/v1/cache/missing" => self.missing(method, headers, body).await,
            "/v1/objects" => self.objects(method, query, headers).await,
            _ => {
                let Some((endpoint, name)) =
                    path.strip_prefix("/v1/").and_then(|it| it.split_once('/'))
                else {
                    return Err(Error::not_found(format!("{path} is not a call of this server")));
                };
                match endpoint {
                    "blob" => self.blob(method, name, headers, body).await,
                    "list" => self.list(method, name, headers, body).await,
                    _ => Err(Error::not_found(format!("{path} is not a call of this server"))),
                }
            }
        }
    }

    /// `POST /v1/hello`: the versions the client speaks, and the one this server does.
    async fn hello(
        &self,
        method: &Method,
        headers: &hyper::HeaderMap,
        body: Incoming,
    ) -> Result<HttpResponse<Body>, Error> {
        if *method != Method::POST {
            return Err(method_not_allowed(method, "POST"));
        }
        let request: HelloRequest = self.message(headers, body).await?;
        let Some(version) = request.versions.iter().copied().filter(|it| *it == VERSION).max()
        else {
            return Err(Error::version(format!(
                "this server speaks {VERSION}, and the client speaks {:?}",
                request.versions.as_slice()
            )));
        };
        // C3 serves every identity: it holds no records and no identity table, so there is
        // nothing to refuse. C5 answers the identities its records name.
        let answer = HelloAnswer {
            version,
            key: self.verifying_key().to_bytes(),
            epochs: Set::new(self.epochs.clone()),
            identities: Set::new(Vec::new()),
            limits: self.limits.clone(),
        };
        Ok(cbor(&answer))
    }

    /// `POST /v1/cache/missing`: the blobs asked for that this directory does not hold,
    /// answered from the existence set in memory.
    async fn missing(
        &self,
        method: &Method,
        headers: &hyper::HeaderMap,
        body: Incoming,
    ) -> Result<HttpResponse<Body>, Error> {
        if *method != Method::POST {
            return Err(method_not_allowed(method, "POST"));
        }
        let request: CacheMissingRequest = self.message(headers, body).await?;
        let asked: Vec<Digest> = request.blobs.iter().copied().collect();
        let answer = CacheMissingAnswer { missing: Set::new(self.store.missing(&asked)) };
        Ok(cbor(&answer))
    }

    /// `GET /v1/objects?prefix=<p>`: the objects under a prefix, for `cache gc --remote`.
    async fn objects(
        &self,
        method: &Method,
        query: &str,
        headers: &hyper::HeaderMap,
    ) -> Result<HttpResponse<Body>, Error> {
        if *method != Method::GET {
            return Err(method_not_allowed(method, "GET"));
        }
        if headers.contains_key("x-blaze-listing-version") {
            return self.paged_objects(query, headers).await;
        }
        let prefix = query
            .split('&')
            .find_map(|it| it.strip_prefix("prefix="))
            .map(unescape)
            .unwrap_or_default();
        let objects: Vec<_> = self
            .disk(move |store| store.list(&prefix))
            .await?
            .into_iter()
            .map(|it| (it.name, it.len, written_ms(it.written)))
            .collect();
        if objects.len() > self.limits.batch_items as usize {
            return Err(Error::too_large("the listing exceeds the advertised batch limit"));
        }
        let bytes = encode(&crate::list::answer(objects));
        if bytes.len() > self.limits.request as usize {
            return Err(Error::too_large("the listing exceeds the advertised request limit"));
        }
        Ok(HttpResponse::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, "application/cbor")
            .body(full(bytes))?)
    }

    /// `GET`, `HEAD`, `PUT`, `DELETE /v1/blob/<name>`.
    #[allow(clippy::too_many_lines)]
    async fn blob(
        &self,
        method: &Method,
        name: &str,
        headers: &hyper::HeaderMap,
        body: Incoming,
    ) -> Result<HttpResponse<Body>, Error> {
        // Section 8 writes a blob's path as `/v1/blob/<sha256>`: a bare digest finds the object
        // named by it, whichever of today's four kinds that is.
        let lookup = name.to_owned();
        let name = match bare_digest(name) {
            Some(digest) if *method == Method::PUT => {
                crate::blob::ObjectName::of(crate::blob::Kind::Unit, &digest).expect("a unit name")
            }
            Some(digest) => {
                self.disk(move |store| Ok(store.name_of_digest(&digest))).await?.ok_or_else(
                    || Error::not_found(format!("no blob of this cache is named {}", hex(&digest))),
                )?
            }
            None => lookup,
        };
        let object = ObjectName::parse(&name)
            .ok_or_else(|| Error::not_found(format!("`{name}` is not an object of this cache")))?;
        if !object.is_blob() {
            return Err(Error::not_found(format!("`{name}` is a list, not a blob")));
        }
        let name = object.name.clone();
        let name = name.as_str();
        match *method {
            Method::GET | Method::HEAD => {
                let owned = name.to_owned();
                let opened = self
                    .disk(move |store| {
                        let Some((file, len)) = store.open_blob(&owned)? else { return Ok(None) };
                        let written = file.metadata()?.modified().ok();
                        Ok(Some((file, len, written)))
                    })
                    .await?;
                let Some((file, len, written)) = opened else {
                    return Err(Error::not_found(format!("{name} is not here")));
                };
                let mut file = tokio::fs::File::from_std(file);
                let asked = match headers.get(header::RANGE).and_then(|it| it.to_str().ok()) {
                    None => None,
                    Some(text) => match parse_range(text) {
                        Ok(range) => Some(range.of(len)),
                        Err(_) => Some(None),
                    },
                };
                let (start, end, status) = match asked {
                    None => (0, len.saturating_sub(1), StatusCode::OK),
                    Some(Some((first, last))) => (first, last, StatusCode::PARTIAL_CONTENT),
                    // A range this object cannot answer is 416, as HTTP says: a client that
                    // asked for the rest of a blob it has all of gets no bytes.
                    Some(None) => {
                        let mut answer = empty(StatusCode::RANGE_NOT_SATISFIABLE);
                        if let Ok(value) = HeaderValue::from_str(&format!("bytes */{len}")) {
                            answer.headers_mut().insert(header::CONTENT_RANGE, value);
                        }
                        return Ok(answer);
                    }
                };
                let count = if len == 0 { 0 } else { end - start + 1 };
                file.seek(SeekFrom::Start(start))
                    .await
                    .map_err(|e| failed(&format!("reading {name}"), e))?;
                let mut answer = HttpResponse::builder()
                    .status(status)
                    .header(header::CONTENT_TYPE, "application/octet-stream")
                    .header(header::ACCEPT_RANGES, "bytes")
                    .header(header::CONTENT_LENGTH, count.to_string());
                if object.kind != crate::blob::Kind::Packed {
                    answer = answer.header(
                        header::ETAG,
                        format!("\"{}\"", hex(&object.digest.expect("a blob digest"))),
                    );
                }
                if let Some(written) = written
                    && let Ok(value) = HeaderValue::from_str(&written_ms(written).to_string())
                {
                    answer = answer.header(HeaderName::from_static(WRITTEN_MS), value);
                }
                if status == StatusCode::PARTIAL_CONTENT {
                    answer =
                        answer.header(header::CONTENT_RANGE, format!("bytes {start}-{end}/{len}"));
                }
                let answer = answer.body(if *method == Method::HEAD {
                    empty_body()
                } else {
                    file_body(file, count)
                })?;
                Ok(answer)
            }
            Method::PUT => {
                let (verification, written) = self.put_blob(&object, headers, body).await?;
                let mut answer = empty(StatusCode::NO_CONTENT);
                answer.headers_mut().insert(
                    HeaderName::from_static("x-blaze-verified"),
                    HeaderValue::from_static(match verification {
                        Verification::Hashed => "hashed",
                        Verification::Unpacked => "unpacked",
                    }),
                );
                let said = if written { "written" } else { "already there" };
                if let Ok(value) = HeaderValue::from_str(&format!("{name}: {said}")) {
                    answer.headers_mut().insert(HeaderName::from_static("x-blaze-blob"), value);
                }
                Ok(answer)
            }
            Method::DELETE => {
                let name = name.to_owned();
                self.disk(move |store| store.delete(&name)).await?;
                Ok(empty(StatusCode::NO_CONTENT))
            }
            _ => Err(method_not_allowed(method, "GET, HEAD, PUT, DELETE")),
        }
    }

    /// A blob is streamed to a temporary file as it arrives, then checked against its name and
    /// renamed. Nothing is named that is not what its name says, or that was cut short.
    async fn put_blob(
        &self,
        object: &ObjectName,
        headers: &hyper::HeaderMap,
        mut body: Incoming,
    ) -> Result<(Verification, bool), Error> {
        if let Some(len) = content_length(headers)
            && len > self.limits.blob
        {
            return Err(Error::too_large(format!(
                "{len} bytes is more than the {} a blob may be",
                self.limits.blob
            )));
        }
        let name = object.name.clone();
        let to = self.disk(move |store| store.checked_path(&name)).await?;
        let partial = temporary(&to);
        // Creation returns both the file and its cleanup owner from the blocking task. If
        // this await is cancelled, dropping that task's result still removes its own file.
        let (file, cleanup) = tokio::task::spawn_blocking(move || Temporary::create(partial))
            .await
            .map_err(|e| failed("creating an upload staging file", e))?
            .map_err(|e| failed(&format!("writing {}", object.name), e))?;
        let mut file = tokio::fs::File::from_std(file);
        let mut taken = 0u64;
        while let Some(frame) = tokio::time::timeout(self.resources.idle, body.frame())
            .await
            .map_err(|_| Error::new(ErrorCode::Refused, "upload body idle deadline expired"))?
        {
            let frame =
                frame.map_err(|e| Error::malformed(format!("the body of a PUT broke: {e}")))?;
            let Some(data) = frame.into_data().ok() else { continue };
            taken += data.len() as u64;
            if taken > self.limits.blob {
                drop(file);
                return Err(Error::too_large(format!(
                    "a blob of more than the {} bytes the server takes",
                    self.limits.blob
                )));
            }
            file.write_all(&data)
                .await
                .map_err(|e| failed(&format!("writing {}", object.name), e))?;
        }
        file.flush().await.map_err(|e| failed(&format!("writing {}", object.name), e))?;
        drop(file);
        // Verification owns cleanup until it finishes, even if the HTTP request goes away.
        let store = self.store.clone();
        let name = object.name.clone();
        cleanup
            .blocking(move |partial| store.put_blob_from(&name, partial))
            .await
            .map_err(|e| failed("verifying a blob", e))?
    }

    /// `GET`, `HEAD`, `PUT`, `DELETE /v1/list/<name>`: a manifest or a project's units.
    async fn list(
        &self,
        method: &Method,
        name: &str,
        headers: &hyper::HeaderMap,
        body: Incoming,
    ) -> Result<HttpResponse<Body>, Error> {
        let name = unescape(name);
        let name = name.as_str();
        let object = ObjectName::parse(name)
            .ok_or_else(|| Error::not_found(format!("`{name}` is not an object of this cache")))?;
        if object.is_blob() {
            return Err(Error::not_found(format!("`{name}` is a blob, not a list")));
        }
        match *method {
            Method::GET | Method::HEAD => {
                let owned = name.to_owned();
                let most = u64::from(self.limits.request);
                let read = self
                    .disk(move |store| {
                        let bytes = store.read_small(&owned, most)?;
                        let found = store.found(&owned)?;
                        Ok((bytes, found))
                    })
                    .await?;
                let (Some(bytes), found) = read else {
                    return Err(Error::not_found(format!("{name} is not here")));
                };
                let etag = hex(&sha256(&bytes));
                let mut answer = HttpResponse::builder()
                    .status(StatusCode::OK)
                    .header(header::CONTENT_TYPE, "text/plain; charset=utf-8")
                    .header(header::CONTENT_LENGTH, bytes.len().to_string())
                    .header(header::ETAG, format!("\"{etag}\""));
                if let Some(found) = found {
                    answer = answer
                        .header(header::LAST_MODIFIED, httpdate::fmt_http_date(found.written));
                    answer = answer.header(WRITTEN_MS, written_ms(found.written).to_string());
                }
                let answer = answer.body(if *method == Method::HEAD {
                    empty_body()
                } else {
                    full(bytes)
                })?;
                Ok(answer)
            }
            Method::PUT => {
                let condition = preconditions::parse(headers);
                let bytes = self.read_whole(headers, body).await?;
                let condition = condition?;
                let owned = name.to_owned();
                let wrote = self
                    .disk(move |store| {
                        let expect = match &condition {
                            Condition::Any => Expect::Any,
                            Condition::None => Expect::Absent,
                            Condition::Match(tag) => Expect::Read(tag),
                        };
                        store.put_list(&owned, &bytes, expect)
                    })
                    .await?;
                if !wrote {
                    return Err(Error::new(
                        ErrorCode::Conflict,
                        format!("{name} was written by another since it was read"),
                    ));
                }
                Ok(empty(StatusCode::NO_CONTENT))
            }
            Method::DELETE => {
                let name = name.to_owned();
                self.disk(move |store| store.delete(&name)).await?;
                Ok(empty(StatusCode::NO_CONTENT))
            }
            _ => Err(method_not_allowed(method, "GET, HEAD, PUT, DELETE")),
        }
    }

    /// A message of section 8: read whole, held to the request limit, and refused by the
    /// validator when it is not canonical CBOR of the type the call names.
    async fn message<T: Wire>(
        &self,
        headers: &hyper::HeaderMap,
        body: Incoming,
    ) -> Result<T, Error> {
        let bytes = self.read_whole(headers, body).await?;
        decode::<T>(&bytes, &self.limits).map_err(|error| {
            Error::malformed(format!(
                "the message is not {}: {}",
                std::any::type_name::<T>(),
                error.offence()
            ))
        })
    }

    async fn read_whole(
        &self,
        headers: &hyper::HeaderMap,
        mut body: Incoming,
    ) -> Result<Vec<u8>, Error> {
        if let Some(len) = content_length(headers)
            && len > u64::from(self.limits.request)
        {
            return Err(Error::too_large(format!(
                "{len} bytes is more than the {} a request may be",
                self.limits.request
            )));
        }
        let mut bytes = Vec::new();
        while let Some(frame) = tokio::time::timeout(self.resources.idle, body.frame())
            .await
            .map_err(|_| Error::new(ErrorCode::Refused, "request body idle deadline expired"))?
        {
            let frame = frame.map_err(|e| Error::malformed(format!("the body broke: {e}")))?;
            let Ok(data) = frame.into_data() else { continue };
            if bytes.len().saturating_add(data.len()) > self.limits.request as usize {
                return Err(Error::too_large("the request exceeds the advertised limit"));
            }
            bytes.extend_from_slice(&data);
        }
        Ok(bytes)
    }
}

/// Serves `config` until the process ends: one task a connection.
pub async fn serve(config: Config) -> anyhow::Result<()> {
    let setup = config.clone();
    let server = Arc::new(tokio::task::spawn_blocking(move || Server::open(&setup)).await??);
    let listener = TcpListener::bind(config.listen)
        .await
        .map_err(|e| anyhow::anyhow!("listening on {}: {e}", config.listen))?;
    let local = listener.local_addr()?;
    println!(
        "blaze-serve: serving {} on http://{local}, routes /v1, codec {VERSION}, no authentication",
        server.store().dir().display()
    );
    tokio::select! {
        done = serve_on(listener, server) => done,
        _ = tokio::signal::ctrl_c() => Ok(()),
    }
}

/// Serves the connections of `listener` with `server`, until the listener fails. A test uses
/// this with a listener of its own, so the port is one the machine gives.
pub async fn serve_on(listener: TcpListener, server: Arc<Server>) -> anyhow::Result<()> {
    let permits = Arc::new(Semaphore::new(server.resources.connections));
    let mut connections = tokio::task::JoinSet::new();
    let mut sweep = tokio::time::interval(server.resources.sweep_every);
    loop {
        tokio::select! {
            _ = sweep.tick() => {
                let age = server.resources.temporary_age;
                server.disk(move |store| store.sweep_temporaries(age)).await?;
                server.pages.expire(server.resources.snapshot_age);
            }
            Some(_) = connections.join_next(), if !connections.is_empty() => {}
            accepted = listener.accept(), if permits.available_permits() != 0 => {
                let (stream, _) = accepted?;
                let permit = permits.clone().acquire_owned().await?;
                let server = server.clone();
                connections.spawn(async move {
                    let _permit = permit;
                    _ = connection(stream, server).await;
                });
            }
        }
    }
}

async fn connection(stream: TcpStream, server: Arc<Server>) -> anyhow::Result<()> {
    let idle = server.resources.idle;
    let socket = socket2::SockRef::from(&stream);
    socket.set_tcp_keepalive(
        &socket2::TcpKeepalive::new()
            .with_time(Duration::from_secs(30))
            .with_interval(Duration::from_secs(10)),
    )?;
    let io = TokioIo::new(stream);
    let service = service_fn(move |request| {
        let server = server.clone();
        async move { Ok::<_, Infallible>(server.answer(request).await) }
    });
    http1::Builder::new()
        .timer(TokioTimer::new())
        .header_read_timeout(idle)
        .serve_connection(io, service)
        .await?;
    Ok(())
}

/// The body of every answer: bytes, or a file streamed from it, or nothing.
pub type Body = BoxBody<Bytes, std::io::Error>;

fn full(bytes: Vec<u8>) -> Body {
    Full::new(Bytes::from(bytes)).map_err(|never| match never {}).boxed()
}

fn empty_body() -> Body {
    Full::new(Bytes::new()).map_err(|never| match never {}).boxed()
}

/// `count` bytes of `file` from where it is, as they are read: a blob is not held in memory.
fn file_body(file: tokio::fs::File, count: u64) -> Body {
    let chunks = futures_util::stream::unfold((file, count), |(mut file, left)| async move {
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
    BodyExt::boxed(StreamBody::new(chunks))
}

fn text(status: StatusCode, text: &'static str) -> HttpResponse<Body> {
    HttpResponse::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "text/plain; charset=utf-8")
        .body(full(text.as_bytes().to_vec()))
        .expect("a text answer")
}

fn empty(status: StatusCode) -> HttpResponse<Body> {
    HttpResponse::builder().status(status).body(empty_body()).expect("an empty answer")
}

fn cbor<T: Wire>(value: &T) -> HttpResponse<Body> {
    HttpResponse::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/cbor")
        .body(full(encode(value)))
        .expect("a CBOR answer")
}

fn error_answer(error: &Error) -> HttpResponse<Body> {
    let status = StatusCode::from_u16(error.status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    let mut answer = HttpResponse::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "application/cbor")
        .body(full(encode(&error.wire())))
        .expect("an error answer");
    if let Some(ms) = error.retry_after_ms
        && let Ok(value) = HeaderValue::from_str(&ms.div_ceil(1000).to_string())
    {
        answer.headers_mut().insert(header::RETRY_AFTER, value);
    }
    answer
}

fn method_not_allowed(method: &Method, allowed: &str) -> Error {
    Error::malformed(format!("{method} is not how this call is made: {allowed}"))
}

/// The digest a bare `/v1/blob/<sha256>` names, when the name is one.
fn bare_digest(name: &str) -> Option<Digest> {
    let mut digest = [0u8; 32];
    if name.len() != 64
        || !name.bytes().all(|it| it.is_ascii_digit() || (b'a'..=b'f').contains(&it))
    {
        return None;
    }
    for (byte, pair) in digest.iter_mut().zip(name.as_bytes().chunks(2)) {
        *byte = u8::from_str_radix(std::str::from_utf8(pair).ok()?, 16).ok()?;
    }
    Some(digest)
}

fn content_length(headers: &hyper::HeaderMap) -> Option<u64> {
    headers
        .get(header::CONTENT_LENGTH)
        .and_then(|it| it.to_str().ok())
        .and_then(|it| it.parse().ok())
}

/// A `%XX` in a query, as its byte.
fn unescape(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut at = 0;
    while at < bytes.len() {
        if bytes[at] == b'%' && at + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[at + 1..at + 3]).ok();
            if let Some(byte) = hex.and_then(|it| u8::from_str_radix(it, 16).ok()) {
                out.push(byte);
                at += 3;
                continue;
            }
        }
        out.push(bytes[at]);
        at += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn sha256(bytes: &[u8]) -> Digest {
    Sha256::digest(bytes).into()
}

fn written_ms(at: SystemTime) -> u64 {
    at.duration_since(UNIX_EPOCH).map_or(0, |it| it.as_millis() as u64)
}

/// The objects a listing answer holds, for a test.
pub fn listed(answer: &[u8]) -> Result<Vec<(String, u64, u64)>, Error> {
    let answer: crate::list::ObjectsAnswer =
        decode(answer, &Limits::DOCUMENTED).map_err(|error| Error::malformed(error.offence()))?;
    Ok(answer.objects.into_iter().map(|it| (it.name, it.len, it.written_ms)).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Running {
        _dir: tempfile::TempDir,
        server: Arc<Server>,
        port: u16,
        task: tokio::task::JoinHandle<anyhow::Result<()>>,
    }

    impl Running {
        async fn start() -> Self {
            let dir = tempfile::tempdir().unwrap();
            Self::with_config(dir, |config| config).await
        }

        async fn with_config(
            dir: tempfile::TempDir,
            change: impl FnOnce(Config) -> Config,
        ) -> Self {
            let server = Arc::new(Server::open(&change(Config::at(dir.path(), 0))).unwrap());
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let port = listener.local_addr().unwrap().port();
            let task = tokio::spawn(serve_on(listener, server.clone()));
            Self { _dir: dir, server, port, task }
        }

        async fn request(
            &self,
            method: &str,
            path: &str,
            headers: &str,
            bytes: &[u8],
        ) -> (u16, String, Vec<u8>) {
            let mut stream = TcpStream::connect(("127.0.0.1", self.port)).await.unwrap();
            let head = format!(
                "{method} {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\nContent-Length: {}\r\n{headers}\r\n",
                bytes.len()
            );
            stream.write_all(head.as_bytes()).await.unwrap();
            stream.write_all(bytes).await.unwrap();
            let mut out = Vec::new();
            tokio::time::timeout(std::time::Duration::from_secs(5), stream.read_to_end(&mut out))
                .await
                .unwrap()
                .unwrap();
            let split = out.windows(4).position(|it| it == b"\r\n\r\n").unwrap();
            let head = String::from_utf8(out[..split].to_vec()).unwrap().to_lowercase();
            let status = head.split_whitespace().nth(1).unwrap().parse().unwrap();
            (status, head, out[split + 4..].to_vec())
        }
    }

    impl Drop for Running {
        fn drop(&mut self) {
            self.task.abort();
        }
    }

    #[tokio::test]
    async fn an_idle_upload_times_out_without_blocking_a_healthy_client() {
        let run = Running::with_config(tempfile::tempdir().unwrap(), |mut config| {
            config.resources.idle = Duration::from_millis(200);
            config
        })
        .await;
        let name = format!("units/{}", hex(&sha256(b"complete")));
        let mut idle = TcpStream::connect(("127.0.0.1", run.port)).await.unwrap();
        idle.write_all(
            format!(
                "PUT /v1/blob/{name} HTTP/1.1\r\nHost: localhost\r\nContent-Length: 8\r\n\r\ncut"
            )
            .as_bytes(),
        )
        .await
        .unwrap();
        let directory = run.server.store().dir().join("units");
        for _ in 0..100 {
            if std::fs::read_dir(&directory).unwrap().count() != 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        assert_eq!(std::fs::read_dir(&directory).unwrap().count(), 1);
        assert_eq!(run.request("GET", "/", "", &[]).await.0, 200);
        tokio::time::timeout(Duration::from_secs(2), async {
            while std::fs::read_dir(&directory).unwrap().count() != 0 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        assert!(run.server.store().found(&name).unwrap().is_none());
        assert_eq!(run.request("GET", "/", "", &[]).await.0, 200);
    }

    #[tokio::test]
    async fn periodic_sweeps_remove_only_unlocked_abandoned_staging_files() {
        let run = Running::with_config(tempfile::tempdir().unwrap(), |mut config| {
            config.resources.sweep_every = Duration::from_millis(20);
            config.resources.temporary_age = Duration::ZERO;
            config
        })
        .await;
        let abandoned = run.server.store().dir().join("units/.abandoned.123-456-1.tmp");
        std::fs::write(&abandoned, b"cut").unwrap();
        let path = run.server.store().dir().join("units/.active.123-456-2.tmp");
        let (file, owner) = Temporary::create(path.clone()).unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            while abandoned.exists() {
                tokio::time::sleep(Duration::from_millis(10)).await
            }
        })
        .await
        .unwrap();
        assert!(path.exists());
        drop(file);
        assert!(path.exists(), "the cleanup lease retains the staging lock");
        drop(owner);
        assert!(!path.exists());
        assert_eq!(run.request("GET", "/", "", &[]).await.0, 200);
    }

    #[tokio::test]
    async fn restarting_a_server_keeps_fresh_stale_files_hidden_until_periodic_cleanup() {
        let dir = tempfile::tempdir().unwrap();
        let server = Server::open(&Config::at(dir.path(), 0)).unwrap();
        let stale = server.store().dir().join("manifests/.default.123-456-3.tmp");
        std::fs::write(&stale, b"incomplete").unwrap();
        drop(server);
        let run = Running::with_config(dir, |mut config| {
            config.resources.sweep_every = Duration::from_millis(20);
            config.resources.temporary_age = Duration::ZERO;
            config
        })
        .await;
        assert!(run.server.store().list("manifests/").unwrap().is_empty());
        tokio::time::timeout(Duration::from_secs(2), async {
            while stale.exists() {
                tokio::time::sleep(Duration::from_millis(10)).await
            }
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn a_connection_slot_is_released_when_idle_headers_expire() {
        let run = Running::with_config(tempfile::tempdir().unwrap(), |mut config| {
            config.resources.connections = 1;
            config.resources.idle = Duration::from_millis(100);
            config
        })
        .await;
        let mut stalled = TcpStream::connect(("127.0.0.1", run.port)).await.unwrap();
        stalled.write_all(b"PUT /v1/blob/").await.unwrap();
        let mut bytes = Vec::new();
        let _ = tokio::time::timeout(Duration::from_secs(2), stalled.read_to_end(&mut bytes))
            .await
            .unwrap();
        assert_eq!(run.request("GET", "/", "", &[]).await.0, 200);
    }

    #[tokio::test]
    async fn expired_listing_cursors_fail_without_silently_restarting_the_snapshot() {
        let run = Running::with_config(tempfile::tempdir().unwrap(), |mut config| {
            config.resources.snapshot_age = Duration::from_millis(50);
            config
        })
        .await;
        for name in ["manifests/a", "manifests/b"] {
            run.server.store().put_list(name, b"\n", Expect::Any).unwrap();
        }
        let (status, headers, _) = run
            .request(
                "GET",
                "/v1/objects?prefix=manifests/&limit=1",
                "X-Blaze-Listing-Version: 1\r\n",
                &[],
            )
            .await;
        assert_eq!(status, 200);
        let cursor = headers
            .lines()
            .find_map(|it| it.strip_prefix("x-blaze-listing-next: "))
            .unwrap()
            .to_owned();
        tokio::time::sleep(Duration::from_millis(80)).await;
        assert_eq!(
            run.request(
                "GET",
                &format!("/v1/objects?prefix=manifests/&cursor={cursor}"),
                "X-Blaze-Listing-Version: 1\r\n",
                &[],
            )
            .await
            .0,
            409
        );
        assert_eq!(run.request("GET", "/", "", &[]).await.0, 200);
    }

    #[tokio::test]
    async fn http_put_get_head_ranges_and_wrong_digest() {
        let run = Running::start().await;
        let bytes = b"0123456789";
        let name = hex(&sha256(bytes));
        let path = format!("/v1/blob/{name}");
        assert_eq!(run.request("PUT", &path, "", bytes).await.0, 204);
        let (status, head, body) = run.request("GET", &path, "", &[]).await;
        assert_eq!((status, body), (200, bytes.to_vec()));
        assert!(head.contains(&format!("etag: \"{name}\"")));
        let (status, head, body) = run.request("HEAD", &path, "", &[]).await;
        assert_eq!((status, body), (200, Vec::new()));
        assert!(head.contains("content-length: 10"));
        for (range, expected, content) in [
            ("bytes=2-4", &bytes[2..5], "bytes 2-4/10"),
            ("bytes=-3", &bytes[7..], "bytes 7-9/10"),
            ("bytes=8-", &bytes[8..], "bytes 8-9/10"),
        ] {
            let (status, head, body) =
                run.request("GET", &path, &format!("Range: {range}\r\n"), &[]).await;
            assert_eq!((status, body), (206, expected.to_vec()));
            assert!(head.contains(content));
        }
        for range in ["bytes=10-", "bytes=9-2", "bytes=-0", "bytes=a-b"] {
            let (status, head, _) =
                run.request("GET", &path, &format!("Range: {range}\r\n"), &[]).await;
            assert_eq!(status, 416);
            assert!(head.contains("bytes */10"));
        }
        let wrong = format!("/v1/blob/units/{}", hex(&[1; 32]));
        assert_eq!(run.request("PUT", &wrong, "", bytes).await.0, 422);
        assert_eq!(run.request("GET", &wrong, "", &[]).await.0, 404);
    }

    #[tokio::test]
    async fn http_missing_and_version_limits() {
        let run = Running::start().await;
        for count in [0, 1, 4096] {
            let blobs = Set::new((0..count).map(|at: u32| sha256(&at.to_le_bytes())).collect());
            let request = CacheMissingRequest { blobs: blobs.clone() };
            let (status, _, body) = run
                .request(
                    "POST",
                    "/v1/cache/missing",
                    "Content-Type: application/cbor\r\n",
                    &encode(&request),
                )
                .await;
            assert_eq!(status, 200);
            let answer: CacheMissingAnswer = decode(&body, &Limits::DOCUMENTED).unwrap();
            assert_eq!(answer.missing, blobs);
        }
        let request = CacheMissingRequest {
            blobs: Set::new((0..4097u32).map(|at| sha256(&at.to_le_bytes())).collect()),
        };
        assert_ne!(run.request("POST", "/v1/cache/missing", "", &encode(&request)).await.0, 200);
        let hello = HelloRequest {
            versions: Set::new(vec![VERSION]),
            identity: blaze_wire::StoreIdentity([0; 32]),
            platform: blaze_wire::ExecPlatform {
                triple: "aarch64-apple-darwin".into(),
                os: "macos".into(),
                sdk: None,
                libc: None,
                linker: ("".into(), "".into(), [0; 32]),
                cpu: "aarch64".into(),
            },
            role: 0,
        };
        let (status, _, body) = run.request("POST", "/v1/hello", "", &encode(&hello)).await;
        assert_eq!(status, 200);
        let answer: HelloAnswer = decode(&body, &Limits::DOCUMENTED).unwrap();
        assert_eq!(answer.version, VERSION);
        assert_eq!(answer.limits.batch_items, 65_536);
        assert_eq!(answer.limits.lookup_items, 4096);
        let mut hello = hello;
        hello.versions = Set::new(vec![VERSION + 1]);
        assert_eq!(run.request("POST", "/v1/hello", "", &encode(&hello)).await.0, 400);
    }

    #[tokio::test]
    async fn http_list_etag_concurrent_publication_and_traversal() {
        let run = Running::start().await;
        let path = "/v1/list/manifests/default";
        assert_eq!(run.request("PUT", path, "If-None-Match: *\r\n", b"first\n").await.0, 204);
        let (status, head, _) = run.request("GET", path, "", &[]).await;
        assert_eq!(status, 200);
        let etag = head.lines().find_map(|it| it.strip_prefix("etag: ")).unwrap();
        let condition = format!("If-Match: {etag}\r\n");
        let (a, b) = tokio::join!(
            run.request("PUT", path, &condition, b"second\n"),
            run.request("PUT", path, &condition, b"third\n"),
        );
        let mut statuses = [a.0, b.0];
        statuses.sort();
        assert_eq!(statuses, [204, 409]);
        for path in [
            "/v1/blob/packs/../key",
            "/v1/list/manifests/../key",
            "/v1/list/manifests/%2e%2e%2fkey",
            "/v1/objects?prefix=../",
            "/v1/objects?prefix=%2Fetc",
        ] {
            assert_ne!(run.request("GET", path, "", &[]).await.0, 200, "{path}");
        }
    }

    #[tokio::test]
    async fn unsupported_conditions_never_overwrite_a_list() {
        let run = Running::start().await;
        let path = "/v1/list/manifests/default";
        let original = b"original\n";
        assert_eq!(run.request("PUT", path, "", original).await.0, 204);
        for headers in [
            "If-None-Match: \"existing\"\r\n",
            "If-Match: unquoted\r\n",
            "If-Match: W/\"existing\"\r\n",
            "If-Match: \"one\"\r\nIf-Match: \"two\"\r\n",
            "If-Match: \"one\"\r\nIf-None-Match: *\r\n",
        ] {
            assert_eq!(run.request("PUT", path, headers, b"replacement\n").await.0, 400);
            let (status, _, bytes) = run.request("GET", path, "", &[]).await;
            assert_eq!((status, bytes), (200, original.to_vec()));
        }
    }

    #[tokio::test]
    async fn a_short_http_upload_is_not_published_and_leaves_no_temporary() {
        let run = Running::start().await;
        let name = format!("units/{}", hex(&sha256(b"complete")));
        let mut stream = TcpStream::connect(("127.0.0.1", run.port)).await.unwrap();
        let request = format!(
            "PUT /v1/blob/{name} HTTP/1.1\r\nHost: localhost\r\nContent-Length: 8\r\n\r\ncut"
        );
        stream.write_all(request.as_bytes()).await.unwrap();
        stream.shutdown().await.unwrap();
        let mut out = Vec::new();
        let _ =
            tokio::time::timeout(std::time::Duration::from_secs(2), stream.read_to_end(&mut out))
                .await;
        for _ in 0..100 {
            if std::fs::read_dir(run.server.store().dir().join("units")).unwrap().count() == 0 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(run.server.store().found(&name).unwrap().is_none());
        assert_eq!(std::fs::read_dir(run.server.store().dir().join("units")).unwrap().count(), 0);
    }
}
