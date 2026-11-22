//! The server over a real socket, as `blaze-serve` serves it: a client of the harness's kind,
//! with nothing but `std` and a small HTTP/1.1 request, so a mistake in how the server frames
//! an answer is not hidden by a client library that agrees with it.
//!
//! What is checked here is what a distributed build rests on: a write of a wrong hash refused,
//! a concurrent write of the same blob, a blob that is only visible whole, and a list that is
//! written once.

#![feature(allocator_api)]

use std::{
    alloc::Global,
    io::{BufRead, BufReader, Read, Write},
    net::{TcpListener as StdListener, TcpStream},
    path::Path,
    sync::Arc,
    thread,
    time::Duration,
};

use blaze_serve::{Config, Server, serve_on};
use blaze_wire::{Digest, Limits, WireError, decode};

// HTTP fixtures use the real producer, so their records and manifest commitments cannot
// drift into an older format behind a current magic header.
#[path = "../../blaze-dist-local/src/pack.rs"]
mod native_pack;

/// A server on a directory of its own, on a port the machine gives, ended when the test ends.
struct Served {
    dir: tempfile::TempDir,
    port: u16,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    task: Option<thread::JoinHandle<()>>,
}

impl Served {
    fn start() -> Served {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let listener = StdListener::bind("127.0.0.1:0").expect("a port");
        let port = listener.local_addr().expect("an address").port();
        listener.set_nonblocking(true).expect("a non-blocking listener");
        let config = Config::at(dir.path(), port);
        let server = Arc::new(Server::open(&config).expect("a server"));
        let (stop, stopped) = tokio::sync::oneshot::channel();
        let task = thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .expect("a runtime");
            runtime.block_on(async move {
                let listener = tokio::net::TcpListener::from_std(listener).expect("a listener");
                tokio::select! {
                    result = serve_on(listener, server) => result.expect("serving"),
                    _ = stopped => {}
                }
            });
        });
        let served = Served { dir, port, stop: Some(stop), task: Some(task) };
        served.wait();
        served
    }

    fn wait(&self) {
        for _ in 0..200 {
            if TcpStream::connect(("127.0.0.1", self.port)).is_ok() {
                return;
            }
            thread::sleep(Duration::from_millis(10));
        }
        panic!("nothing listens on {}", self.port);
    }

    fn dir(&self) -> &Path {
        self.dir.path()
    }

    /// One request, as a client makes it: `Content-Length`, no chunking, one connection.
    fn ask(&self, method: &str, path: &str, headers: &[(&str, &str)], body: &[u8]) -> Answer {
        let mut stream = TcpStream::connect(("127.0.0.1", self.port)).expect("a connection");
        stream.set_read_timeout(Some(Duration::from_secs(30))).expect("a timeout");
        let mut head =
            format!("{method} {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n");
        for (name, value) in headers {
            head.push_str(&format!("{name}: {value}\r\n"));
        }
        head.push_str(&format!("Content-Length: {}\r\n\r\n", body.len()));
        stream.write_all(head.as_bytes()).expect("a request");
        stream.write_all(body).expect("a body");
        Answer::read(stream, method).expect("an answer")
    }
}

impl Drop for Served {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            _ = stop.send(());
        }
        if let Some(task) = self.task.take() {
            task.join().expect("server shutdown");
        }
    }
}

#[derive(Debug)]
struct Answer {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Answer {
    fn read(reader: impl Read, method: &str) -> std::io::Result<Answer> {
        let mut reader = BufReader::new(reader);
        let mut head = Vec::new();
        while !head.ends_with(b"\r\n\r\n") {
            if reader.read_until(b'\n', &mut head)? == 0 {
                return Err(std::io::ErrorKind::UnexpectedEof.into());
            }
        }
        let mut answer = Self::parse(&head);
        let length = if method == "HEAD" || matches!(answer.status, 204 | 304) {
            0
        } else {
            answer
                .header("content-length")
                .ok_or(std::io::ErrorKind::InvalidData)?
                .parse::<usize>()
                .map_err(|_| std::io::ErrorKind::InvalidData)?
        };
        answer.body.resize(length, 0);
        reader.read_exact(&mut answer.body)?;
        Ok(answer)
    }

    fn parse(answer: &[u8]) -> Answer {
        let split = answer.windows(4).position(|it| it == b"\r\n\r\n").expect("a head");
        let head = String::from_utf8_lossy(&answer[..split]).into_owned();
        let mut lines = head.split("\r\n");
        let status = lines
            .next()
            .and_then(|it| it.split(' ').nth(1))
            .and_then(|it| it.parse().ok())
            .expect("a status");
        let headers = lines
            .filter_map(|it| it.split_once(':'))
            .map(|(name, value)| (name.to_ascii_lowercase(), value.trim().to_owned()))
            .collect();
        Answer { status, headers, body: answer[split + 4..].to_vec() }
    }

    fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(it, _)| it == name).map(|(_, value)| value.as_str())
    }

    fn error(&self) -> WireError {
        decode::<WireError>(&self.body, &Limits::DOCUMENTED).expect("an error of section 8")
    }
}

#[test]
fn answer_reader_finishes_at_the_http_message_boundary() {
    struct ResetAfter(std::io::Cursor<Vec<u8>>);
    impl Read for ResetAfter {
        fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
            if self.0.position() == self.0.get_ref().len() as u64 {
                return Err(std::io::ErrorKind::ConnectionReset.into());
            }
            self.0.read(bytes)
        }
    }
    let body = b"an error";
    let mut response = b"HTTP/1.1 404 Not Found\r\nContent-Length: 8\r\n\r\n".to_vec();
    response.extend_from_slice(body);
    let answer = Answer::read(ResetAfter(std::io::Cursor::new(response)), "PUT").unwrap();
    assert_eq!(answer.status, 404);
    assert_eq!(answer.body, body);
}

#[test]
fn answer_reader_rejects_truncated_messages() {
    for response in [
        b"HTTP/1.1 404 Not Found\r\nContent-Length: 4\r\n".as_slice(),
        b"HTTP/1.1 404 Not Found\r\nContent-Length: 4\r\n\r\nabc".as_slice(),
    ] {
        assert!(Answer::read(response, "PUT").is_err());
    }
}

#[test]
fn answer_reader_respects_bodyless_responses() {
    for (method, status) in [("HEAD", 200), ("PUT", 204), ("GET", 304)] {
        let response = format!("HTTP/1.1 {status} Response\r\nContent-Length: 100\r\n\r\n");
        let answer = Answer::read(response.as_bytes(), method).unwrap();
        assert!(answer.body.is_empty());
    }
}

/// The native producer's canonical bytes and commitment for these store values.
fn pack(values: &[(Digest, &[u8])]) -> native_pack::Pack<Global> {
    let mut input = Vec::with_capacity_in(values.len(), Global);
    input.extend(values.iter().map(|(key, bytes)| native_pack::Value {
        key: *key,
        kind: 0,
        bytes: bytes.to_vec_in(Global),
    }));
    native_pack::Pack::of(input, Global).expect("canonical native values")
}

fn digest(bytes: &[u8]) -> Digest {
    use sha2::Digest as _;
    sha2::Sha256::digest(bytes).into()
}

fn hex(digest: &Digest) -> String {
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// The name a pack is sent under.
fn pack_name(bytes: &[u8]) -> String {
    format!("packs/{}.pack", hex(&digest(bytes)))
}

#[test]
fn a_blob_is_sent_whole() {
    let served = Served::start();
    let bytes = pack(&[(digest(b"one"), b"value one"), (digest(b"two"), b"value two")]).bytes;
    let name = pack_name(&bytes);
    let put = served.ask("PUT", &format!("/v1/blob/{name}"), &[], &bytes);
    assert_eq!(put.status, 204, "{}", put.error().message);

    let whole = served.ask("GET", &format!("/v1/blob/{name}"), &[], &[]);
    assert_eq!(whole.status, 200);
    assert_eq!(whole.body, bytes);
    assert_eq!(whole.header("content-length"), Some(bytes.len().to_string().as_str()));

    // What is not there is 404, and code 5.
    let gone = served.ask("GET", &format!("/v1/blob/packs/{}.pack", "0".repeat(64)), &[], &[]);
    assert_eq!(gone.status, 404);
    assert_eq!(gone.error().code, 5);
}

#[test]
fn a_write_of_a_wrong_hash_is_refused_and_names_nothing() {
    let served = Served::start();
    let bytes = pack(&[(digest(b"one"), b"value one")]).bytes;
    let name = pack_name(&bytes);
    // The right bytes under another name: refused, and the name holds nothing after.
    let other = pack(&[(digest(b"other"), b"another value")]).bytes;
    let wrong = served.ask("PUT", &format!("/v1/blob/{name}"), &[], &other);
    assert_eq!(wrong.status, 422);
    assert_eq!(wrong.error().code, 9);
    assert!(wrong.error().message.contains(&hex(&digest(&bytes))), "{}", wrong.error().message);
    assert_eq!(served.ask("GET", &format!("/v1/blob/{name}"), &[], &[]).status, 404);
    // Nor does a temporary file of it stay behind.
    let packs = served.dir().join("packs");
    let left: Vec<String> = std::fs::read_dir(&packs)
        .expect("the packs directory")
        .filter_map(|it| it.ok()?.file_name().into_string().ok())
        .collect();
    assert!(left.is_empty(), "{left:?} was left behind");

    // Bytes that are not a pack of this format are refused under a pack's name, even when the
    // hash fits: the store would empty the directory over one.
    let junk = b"not a pack at all".to_vec();
    let junk_name = format!("packs/{}.pack", hex(&digest(&junk)));
    let refused = served.ask("PUT", &format!("/v1/blob/{junk_name}"), &[], &junk);
    assert_eq!(refused.status, 422);
    assert!(refused.error().message.contains("format"), "{}", refused.error().message);
}

#[test]
fn a_concurrent_write_of_one_blob_leaves_one_whole_blob() {
    let served = Served::start();
    let bytes =
        pack(&[(digest(b"one"), b"a value that is not small"), (digest(b"two"), &[7; 4096])]).bytes;
    let name = pack_name(&bytes);
    let port = served.port;
    let writers: Vec<_> = (0..8)
        .map(|_| {
            let (name, bytes) = (name.clone(), bytes.clone());
            thread::spawn(move || {
                let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("a connection");
                let head = format!(
                    "PUT /v1/blob/{name} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\nContent-Length: {}\r\n\r\n",
                    bytes.len()
                );
                stream.write_all(head.as_bytes()).expect("a request");
                stream.write_all(&bytes).expect("a body");
                Answer::read(stream, "PUT").expect("an answer").status
            })
        })
        .collect();
    for writer in writers {
        assert_eq!(writer.join().expect("a writer"), 204);
    }
    let got = served.ask("GET", &format!("/v1/blob/{name}"), &[], &[]);
    assert_eq!(got.status, 200);
    assert_eq!(got.body, bytes);
    // One blob, and no temporary file of any of the writers.
    let left: Vec<String> = std::fs::read_dir(served.dir().join("packs"))
        .expect("the packs directory")
        .filter_map(|it| it.ok()?.file_name().into_string().ok())
        .collect();
    assert_eq!(left, vec![format!("{}.pack", hex(&digest(&bytes)))]);
}

#[test]
fn a_list_is_written_once() {
    let served = Served::start();
    let path = format!("/v1/list/manifests/{}", hex(&digest(b"a unit")));
    assert_eq!(served.ask("GET", &path, &[], &[]).status, 404);

    // A malformed first writer names nothing; a real list must name verified packs.
    let malformed = served.ask("PUT", &path, &[], b"first\n");
    assert_eq!(malformed.status, 400);
    assert_eq!(malformed.error().code, 1);
    assert!(malformed.error().message.contains("pack list line 1"));
    assert_eq!(served.ask("GET", &path, &[], &[]).status, 404);

    let key = digest(b"an item");
    let first_pack = pack(&[(key, b"first")]);
    let second_pack = pack(&[(key, b"second")]);
    for pack in [&first_pack, &second_pack] {
        let put = served.ask(
            "PUT", &format!("/v1/blob/{}", pack_name(&pack.bytes)), &[], &pack.bytes,
        );
        assert_eq!(put.status, 204, "{}", put.error().message);
    }
    let first = blaze_wire::pack_list::encode(&[first_pack.reference], Global);
    let second = blaze_wire::pack_list::encode(&[second_pack.reference], Global);
    assert_ne!(first, second, "the two writers propose different pack lists");
    let wrote = served.ask("PUT", &path, &[], &first);
    assert_eq!(wrote.status, 204, "{}", wrote.error().message);
    // A second writer is told, and what is there stays: both name the same keys.
    assert_eq!(served.ask("PUT", &path, &[], &second).status, 412);
    let got = served.ask("GET", &path, &[], &[]);
    assert_eq!(got.status, 200);
    assert_eq!(got.body, first);
}

#[test]
fn a_name_that_is_not_the_layouts_reaches_no_file() {
    let served = Served::start();
    for name in [
        "../Cargo.toml",
        "packs/../../etc/passwd",
        "packs/x.pack",
        "keys/..",
        "blobs/x",
        "manifests/default",
    ] {
        let answer = served.ask("GET", &format!("/v1/blob/{name}"), &[], &[]);
        assert_eq!(answer.status, 404, "{name} reached something");
    }
    let answer = served.ask("GET", "/v1/nonsense", &[], &[]);
    assert_eq!(answer.status, 404);
}

/// A path that is not of the layout is not read as one, and a name has one spelling.
#[test]
fn uppercase_digest_routes_are_rejected_without_creating_aliases() {
    let served = Served::start();
    let bytes = pack(&[(digest(b"canonical"), b"lowercase")]).bytes;
    let d = hex(&digest(&bytes));
    assert_ne!(d, d.to_uppercase());
    for name in [
        format!("packs/{}.pack", d.to_uppercase()),
        format!("units/{d}"),
        format!("keys/{d}"),
        d.clone(),
        d.to_uppercase(),
    ] {
        assert_eq!(
            served.ask("PUT", &format!("/v1/blob/{name}"), &[], &bytes).status,
            404,
            "{name}"
        );
    }
    let path = format!("/v1/blob/packs/{d}.pack");
    assert_eq!(served.ask("PUT", &path, &[], &bytes).status, 204);
    assert_eq!(served.ask("GET", &path, &[], &[]).body, bytes);
}

#[cfg(unix)]
#[test]
fn http_reads_reject_symlink_objects_and_oversize_upload_length() {
    let served = Served::start();
    let outside = tempfile::tempdir().unwrap();
    let bytes = b"outside";
    let name = format!("packs/{}.pack", hex(&digest(bytes)));
    std::fs::write(outside.path().join("secret"), bytes).unwrap();
    std::os::unix::fs::symlink(outside.path().join("secret"), served.dir().join(&name)).unwrap();
    assert_eq!(served.ask("GET", &format!("/v1/blob/{name}"), &[], &[]).status, 400);
    let mut stream = TcpStream::connect(("127.0.0.1", served.port)).unwrap();
    stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let request = format!(
        "PUT /v1/blob/packs/{}.pack HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\nContent-Length: {}\r\n\r\n",
        "01".repeat(32),
        Limits::DOCUMENTED.blob + 1
    );
    stream.write_all(request.as_bytes()).unwrap();
    assert_eq!(Answer::read(stream, "PUT").unwrap().status, 413);
    assert!(
        std::fs::read_dir(served.dir().join("packs")).unwrap().all(|entry| !entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with('.'))
    );
}
