//! The HTTP/1.1 side: the calls over `hyper`, and the answers they get.
//!
//! One connection is one task. A message is read whole, held to [`Limits::request`] and
//! refused by the validator when it is not canonical CBOR; a `PUT` of a blob is streamed to a
//! temporary file as it arrives, so a blob larger than the memory of the machine is still
//! taken, and nothing of it is named until it is checked against its name.
//!
//! Every answer that is not a success is a [`WireError`] in CBOR, with the status of
//! its code's class. A blob's bytes are not CBOR; the bytes of a list are the list's own text.

use std::{convert::Infallible, net::SocketAddr, path::PathBuf, sync::Arc, time::Duration};

use blaze_wire::{ErrorCode, Limits, encode};
use bytes::Bytes;
use http_body_util::{BodyExt, Full, StreamBody, combinators::BoxBody};
use hyper::{
    Method, Request as HttpRequest, Response as HttpResponse, StatusCode,
    body::{Frame, Incoming},
    header::{self, HeaderName, HeaderValue},
    server::conn::http1,
    service::service_fn,
};
use hyper_util::rt::{TokioIo, TokioTimer};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::Semaphore,
};

use crate::blob::{ObjectName, hex};
use crate::error::{Error, failed};
use crate::store::{Store, temporary};

mod progress;
mod publication;

use publication::Temporary;

/// The bytes of one read of a file that is sent.
const CHUNK: usize = 1 << 16;

/// What the server is told when it starts.
#[derive(Clone, Debug)]
pub struct Config {
    /// The directory that holds `packs/` and `manifests/`.
    pub dir: PathBuf,
    /// Where it listens. `127.0.0.1:PORT` unless the operator says otherwise.
    pub listen: SocketAddr,
    /// Request and connection liveness policy, not a compile-worker memory limit.
    pub resources: Resources,
    /// The most bytes a blob or a request may be.
    pub limits: Limits,
}

/// Resource leases for network peers.
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
}

impl Default for Resources {
    fn default() -> Self {
        Self {
            idle: Duration::from_secs(30),
            connections: 256,
            sweep_every: Duration::from_secs(60),
            temporary_age: Duration::from_secs(3600),
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
    limits: Limits,
    resources: Resources,
    #[cfg(test)]
    accept_errors: std::sync::Mutex<std::collections::VecDeque<std::io::Error>>,
}

impl Server {
    /// The server on `config.dir`.
    pub fn open(config: &Config) -> Result<Server, Error> {
        if config.resources.idle.is_zero()
            || config.resources.connections == 0
            || config.resources.sweep_every.is_zero()
            || config.limits.request == 0
            || config.limits.batch_items == 0
        {
            return Err(Error::malformed("server resource leases and wire limits must be positive"));
        }
        let limits = config.limits;
        let ceiling = Limits::DOCUMENTED;
        if limits.request > ceiling.request || limits.blob < 32 || limits.blob > ceiling.blob {
            return Err(Error::malformed("unsupported server wire limits"));
        }
        let store = Store::open(&config.dir)?;
        Ok(Server {
            store: Arc::new(store),
            limits: config.limits,
            resources: config.resources.clone(),
            #[cfg(test)]
            accept_errors: std::sync::Mutex::new(std::collections::VecDeque::new()),
        })
    }

    /// The directory it serves.
    pub fn store(&self) -> &Store {
        &self.store
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
        let answered = self.route(&parts.method, &path, &parts.headers, body).await;
        match answered {
            Ok(answer) => answer,
            Err(error) => error_answer(&error),
        }
    }

    async fn route(
        &self,
        method: &Method,
        path: &str,
        headers: &hyper::HeaderMap,
        body: Incoming,
    ) -> Result<HttpResponse<Body>, Error> {
        if path == "/" {
            return Ok(text(StatusCode::OK, "blaze-serve\n"));
        }
        let Some((endpoint, name)) = path.strip_prefix("/v1/").and_then(|it| it.split_once('/'))
        else {
            return Err(Error::not_found(format!("{path} is not a call of this server")));
        };
        match endpoint {
            "blob" => self.blob(method, name, headers, body).await,
            "list" => self.list(method, name, headers, body).await,
            _ => Err(Error::not_found(format!("{path} is not a call of this server"))),
        }
    }

    async fn blob(
        &self,
        method: &Method,
        name: &str,
        headers: &hyper::HeaderMap,
        body: Incoming,
    ) -> Result<HttpResponse<Body>, Error> {
        let object = ObjectName::parse(name)
            .ok_or_else(|| Error::not_found(format!("`{name}` is not an object of this cache")))?;
        if !object.is_blob() {
            return Err(Error::not_found(format!("`{name}` is a list, not a blob")));
        }
        match *method {
            Method::GET => {
                let owned = object.name.clone();
                let opened = self.disk(move |store| store.open_blob(&owned)).await?;
                let Some((file, len)) = opened else {
                    return Err(Error::not_found(format!("{name} is not here")));
                };
                Ok(HttpResponse::builder()
                    .status(StatusCode::OK)
                    .header(header::CONTENT_TYPE, "application/octet-stream")
                    .header(header::CONTENT_LENGTH, len.to_string())
                    .header(header::ETAG, format!("\"{}\"", hex(&object.digest)))
                    .body(file_body(tokio::fs::File::from_std(file), len))?)
            }
            Method::PUT => {
                let written = self.put_blob(&object, headers, body).await?;
                let mut answer = empty(StatusCode::NO_CONTENT);
                let said = if written { "written" } else { "already there" };
                if let Ok(value) = HeaderValue::from_str(&format!("{name}: {said}")) {
                    answer.headers_mut().insert(HeaderName::from_static("x-blaze-blob"), value);
                }
                Ok(answer)
            }
            _ => Err(method_not_allowed(method, "GET, PUT")),
        }
    }

    /// A blob is streamed to a temporary file as it arrives, then checked against its name and
    /// renamed. Nothing is named that is not what its name says, or that was cut short.
    async fn put_blob(
        &self,
        object: &ObjectName,
        headers: &hyper::HeaderMap,
        mut body: Incoming,
    ) -> Result<bool, Error> {
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
    /// A unit's list. A list is written once: what a second writer would put names the same
    /// keys, so its `PUT` answers 412 and the first stays.
    async fn list(
        &self,
        method: &Method,
        name: &str,
        headers: &hyper::HeaderMap,
        body: Incoming,
    ) -> Result<HttpResponse<Body>, Error> {
        let object = ObjectName::parse(name)
            .ok_or_else(|| Error::not_found(format!("`{name}` is not an object of this cache")))?;
        if object.is_blob() {
            return Err(Error::not_found(format!("`{name}` is a blob, not a list")));
        }
        let owned = object.name.clone();
        match *method {
            Method::GET => {
                let most = u64::from(self.limits.request);
                let read = self.disk(move |store| store.read_small(&owned, most)).await?;
                let Some(bytes) = read else {
                    return Err(Error::not_found(format!("{name} is not here")));
                };
                Ok(HttpResponse::builder()
                    .status(StatusCode::OK)
                    .header(header::CONTENT_TYPE, "text/plain; charset=utf-8")
                    .header(header::CONTENT_LENGTH, bytes.len().to_string())
                    .body(full(bytes))?)
            }
            Method::PUT => {
                let bytes = self.read_whole(headers, body).await?;
                let wrote = self.disk(move |store| store.put_list(&owned, &bytes)).await?;
                Ok(empty(if wrote {
                    StatusCode::NO_CONTENT
                } else {
                    StatusCode::PRECONDITION_FAILED
                }))
            }
            _ => Err(method_not_allowed(method, "GET, PUT")),
        }
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
        "blaze-serve: serving {} on http://{local}, routes /v1, codec {}, no authentication",
        server.store().dir().display(),
        blaze_wire::VERSION
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
                if let Err(error) = server.disk(move |store| store.sweep_temporaries(age)).await {
                    eprintln!("blaze-serve: staging janitor failed: {error}; service continues");
                }
            }
            Some(_) = connections.join_next(), if !connections.is_empty() => {}
            accepted = accept(&listener, &server), if permits.available_permits() != 0 => {
                let (stream, _) = match accepted {
                    Ok(accepted) => accepted,
                    Err(error) if retry_accept(&error) => {
                        eprintln!("blaze-serve: accept failed: {error}; retrying");
                        tokio::time::sleep(Duration::from_millis(100)).await;
                        continue;
                    }
                    Err(error) => return Err(error.into()),
                };
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

async fn accept(
    listener: &TcpListener,
    _server: &Server,
) -> std::io::Result<(TcpStream, SocketAddr)> {
    #[cfg(test)]
    if let Some(error) = _server.accept_errors.lock().unwrap().pop_front() {
        return Err(error);
    }
    listener.accept().await
}

async fn connection(stream: TcpStream, server: Arc<Server>) -> anyhow::Result<()> {
    let idle = server.resources.idle;
    let socket = socket2::SockRef::from(&stream);
    socket.set_tcp_keepalive(
        &socket2::TcpKeepalive::new()
            .with_time(Duration::from_secs(30))
            .with_interval(Duration::from_secs(10)),
    )?;
    let io = TokioIo::new(progress::ProgressIo::new(stream, idle));
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

fn retry_accept(error: &std::io::Error) -> bool {
    use std::io::ErrorKind;
    matches!(
        error.kind(),
        ErrorKind::Interrupted
            | ErrorKind::WouldBlock
            | ErrorKind::ConnectionAborted
            | ErrorKind::ConnectionReset
            | ErrorKind::OutOfMemory
    ) || {
        #[cfg(unix)]
        {
            let resource =
                matches!(error.raw_os_error(), Some(libc::EMFILE | libc::ENFILE | libc::ENOBUFS));
            #[cfg(target_os = "linux")]
            let pending_network = matches!(
                error.raw_os_error(),
                Some(
                    libc::ENETDOWN
                        | libc::EPROTO
                        | libc::ENOPROTOOPT
                        | libc::EHOSTDOWN
                        | libc::ENONET
                        | libc::EHOSTUNREACH
                        | libc::EOPNOTSUPP
                        | libc::ENETUNREACH
                )
            );
            #[cfg(not(target_os = "linux"))]
            let pending_network = false;
            resource || pending_network
        }
        #[cfg(not(unix))]
        {
            false
        }
    }
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

fn content_length(headers: &hyper::HeaderMap) -> Option<u64> {
    headers
        .get(header::CONTENT_LENGTH)
        .and_then(|it| it.to_str().ok())
        .and_then(|it| it.parse().ok())
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
            use tokio::io::{AsyncBufReadExt, BufReader};

            let mut stream = TcpStream::connect(("127.0.0.1", self.port)).await.unwrap();
            let head = format!(
                "{method} {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\nContent-Length: {}\r\n{headers}\r\n",
                bytes.len()
            );
            stream.write_all(head.as_bytes()).await.unwrap();
            stream.write_all(bytes).await.unwrap();
            tokio::time::timeout(std::time::Duration::from_secs(5), async {
                // A rejected upload can close with unread request bytes and reset TCP on
                // macOS. HTTP completion is its message framing, not a subsequent TCP EOF.
                let mut reader = BufReader::new(stream);
                let mut head = String::new();
                while !head.ends_with("\r\n\r\n") {
                    assert_ne!(reader.read_line(&mut head).await.unwrap(), 0, "truncated headers");
                }
                let head = head.trim_end_matches("\r\n").to_lowercase();
                let status = head.split_whitespace().nth(1).unwrap().parse::<u16>().unwrap();
                let length = if method == "HEAD" || matches!(status, 204 | 304) {
                    0
                } else {
                    head.lines()
                        .find_map(|line| line.strip_prefix("content-length:"))
                        .expect("the test server frames responses with Content-Length")
                        .trim()
                        .parse::<u64>()
                        .unwrap()
                };
                let mut body = Vec::new();
                let read = reader.take(length).read_to_end(&mut body).await.unwrap();
                assert_eq!(read as u64, length, "truncated response body");
                (status, head, body)
            })
            .await
            .unwrap()
        }
    }

    impl Drop for Running {
        fn drop(&mut self) {
            self.task.abort();
        }
    }

    /// Bytes the server takes as a pack, with the digest that names them.
    fn pack(rest: &[u8]) -> (Vec<u8>, String) {
        let mut bytes = blaze_wire::DIST_PACK_MAGIC.to_vec();
        bytes.extend_from_slice(rest);
        let name = {
            use sha2::Digest as _;
            let digest: blaze_wire::Digest = sha2::Sha256::digest(&bytes).into();
            hex(&digest)
        };
        (bytes, name)
    }

    #[tokio::test]
    async fn an_idle_upload_times_out_without_blocking_a_healthy_client() {
        let run = Running::with_config(tempfile::tempdir().unwrap(), |mut config| {
            config.resources.idle = Duration::from_millis(200);
            config
        })
        .await;
        let (_, digest) = pack(b"complete");
        let name = format!("packs/{digest}.pack");
        let mut idle = TcpStream::connect(("127.0.0.1", run.port)).await.unwrap();
        idle.write_all(
            format!(
                "PUT /v1/blob/{name} HTTP/1.1\r\nHost: localhost\r\nContent-Length: 8\r\n\r\ncut"
            )
            .as_bytes(),
        )
        .await
        .unwrap();
        let directory = run.server.store().dir().join("packs");
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
        assert!(run.server.store().open_blob(&name).unwrap().is_none());
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
        let abandoned = run.server.store().dir().join("packs/.abandoned.123-456-1.tmp");
        std::fs::write(&abandoned, b"cut").unwrap();
        let path = run.server.store().dir().join("packs/.active.123-456-2.tmp");
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
        let _run = Running::with_config(dir, |mut config| {
            config.resources.sweep_every = Duration::from_millis(20);
            config.resources.temporary_age = Duration::ZERO;
            config
        })
        .await;
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
    async fn stalled_response_releases_its_connection_permit() {
        let run = Running::with_config(tempfile::tempdir().unwrap(), |mut config| {
            config.resources.connections = 1;
            config.resources.idle = Duration::from_millis(200);
            config
        })
        .await;
        let bytes = vec![b'x'; 16 << 20];
        let (bytes, digest) = pack(&bytes);
        let name = format!("packs/{digest}.pack");
        run.server.store().put_blob(&name, &bytes).unwrap();
        let mut stalled = TcpStream::connect(("127.0.0.1", run.port)).await.unwrap();
        socket2::SockRef::from(&stalled).set_recv_buffer_size(4096).unwrap();
        stalled
            .write_all(
                format!("GET /v1/blob/{name} HTTP/1.1\r\nHost: localhost\r\n\r\n").as_bytes(),
            )
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        // With one permit, this response cannot arrive until the blocked writer expires.
        assert_eq!(run.request("GET", "/", "", &[]).await.0, 200);
        assert!(run.server.store().open_blob(&name).unwrap().is_some());
        drop(stalled);
    }

    #[tokio::test]
    async fn transient_accept_errors_and_bad_janitor_entries_leave_service_usable() {
        let dir = tempfile::tempdir().unwrap();
        let run = Running::with_config(dir, |mut config| {
            config.resources.sweep_every = Duration::from_millis(20);
            config.resources.temporary_age = Duration::ZERO;
            config
        })
        .await;
        run.server.accept_errors.lock().unwrap().push_back(std::io::ErrorKind::Interrupted.into());
        #[cfg(unix)]
        run.server
            .accept_errors
            .lock()
            .unwrap()
            .push_back(std::io::Error::from_raw_os_error(libc::EMFILE));
        #[cfg(target_os = "linux")]
        {
            let mut errors = run.server.accept_errors.lock().unwrap();
            errors.push_back(std::io::Error::from_raw_os_error(libc::EPROTO));
            errors.push_back(std::io::Error::from_raw_os_error(libc::ENETUNREACH));
        }
        let stale_dir = run.server.store().dir().join("packs/.bad.123-456-1.tmp");
        std::fs::create_dir(&stale_dir).unwrap();
        let abandoned = run.server.store().dir().join("packs/.abandoned.123-456-2.tmp");
        std::fs::write(&abandoned, b"leftover").unwrap();
        assert_eq!(run.request("GET", "/", "", &[]).await.0, 200);
        tokio::time::timeout(Duration::from_secs(2), async {
            while abandoned.exists() {
                tokio::time::sleep(Duration::from_millis(10)).await
            }
        })
        .await
        .unwrap();
        assert!(stale_dir.is_dir());
        assert_eq!(run.request("GET", "/", "", &[]).await.0, 200);
        assert!(run.server.accept_errors.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn malformed_and_traversing_object_paths_are_not_published() {
        let run = Running::start().await;
        for name in ["%", "%2", "%GG", "%ff", "%00", "a%2Fb", "a%5Cb", "%2e%2e", "default"] {
            let (status, _, _) = run
                .request("PUT", &format!("/v1/list/manifests/{name}"), "", b"must not appear\n")
                .await;
            assert_eq!(status, 404, "{name}: {status}");
        }
        assert_eq!(
            std::fs::read_dir(run.server.store().dir().join("manifests")).unwrap().count(),
            0
        );
    }

    #[tokio::test]
    async fn http_put_get_and_wrong_digest() {
        let run = Running::start().await;
        let (bytes, name) = pack(b"0123456789");
        let path = format!("/v1/blob/packs/{name}.pack");
        assert_eq!(run.request("PUT", &path, "", &bytes).await.0, 204);
        let (status, head, body) = run.request("GET", &path, "", &[]).await;
        assert_eq!((status, body), (200, bytes.clone()));
        assert!(head.contains(&format!("etag: \"{name}\"")));
        let wrong = format!("/v1/blob/packs/{}.pack", hex(&[1; 32]));
        assert_eq!(run.request("PUT", &wrong, "", &bytes).await.0, 422);
        assert_eq!(run.request("GET", &wrong, "", &[]).await.0, 404);
        let (unframed, name) = (b"not a pack".to_vec(), hex(&[2; 32]));
        let path = format!("/v1/blob/packs/{name}.pack");
        assert_eq!(run.request("PUT", &path, "", &unframed).await.0, 422);
        for gone in ["/v1/hello", "/v1/cache/missing", "/v1/objects?prefix=packs/"] {
            assert_eq!(run.request("POST", gone, "", &[]).await.0, 404, "{gone}");
        }
        // A method no call is made with is the request's fault.
        assert_eq!(
            run.request("DELETE", &format!("/v1/blob/packs/{name}.pack"), "", &[]).await.0,
            400
        );
    }

    #[tokio::test]
    async fn http_list_is_written_once_and_a_second_writer_is_told() {
        let run = Running::start().await;
        let path = format!("/v1/list/manifests/{}", hex(&[7; 32]));
        let (a, b) = tokio::join!(
            run.request("PUT", &path, "", b"first\n"),
            run.request("PUT", &path, "", b"second\n"),
        );
        let mut statuses = [a.0, b.0];
        statuses.sort();
        assert_eq!(statuses, [204, 412]);
        let (status, _, body) = run.request("GET", &path, "", &[]).await;
        assert_eq!(status, 200);
        assert!(body == b"first\n" || body == b"second\n");
        for path in [
            "/v1/blob/packs/../key",
            "/v1/list/manifests/../key",
            "/v1/list/manifests/%2e%2e%2fkey",
        ] {
            assert_ne!(run.request("GET", path, "", &[]).await.0, 200, "{path}");
        }
    }

    #[tokio::test]
    async fn a_short_http_upload_is_not_published_and_leaves_no_temporary() {
        let run = Running::start().await;
        let (_, digest) = pack(b"complete");
        let name = format!("packs/{digest}.pack");
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
            if std::fs::read_dir(run.server.store().dir().join("packs")).unwrap().count() == 0 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(run.server.store().open_blob(&name).unwrap().is_none());
        assert_eq!(std::fs::read_dir(run.server.store().dir().join("packs")).unwrap().count(), 0);
    }
}
