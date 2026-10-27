//! The server over a real socket, as `ra-jit serve` serves it: a client of the harness's kind,
//! with nothing but `std` and a small HTTP/1.1 request, so a mistake in how the server frames
//! an answer is not hidden by a client library that agrees with it.
//!
//! What is checked here is what unit C3 asks for: `Range` (partial, suffix, invalid), `missing`
//! with 0, 1 and 4,096 names, a write of a wrong hash refused, a concurrent write of the same
//! blob, and the parts a shared cache rests on: a blob that is only visible whole, a list that
//! is only replaced by a writer that read what was there, and `hello`.

use std::{
    io::{Read, Write},
    net::{TcpListener as StdListener, TcpStream},
    path::Path,
    sync::Arc,
    thread,
    time::Duration,
};

use blaze_serve::{Config, Server, serve_on};
use blaze_wire::{
    CacheMissingAnswer, CacheMissingRequest, Digest, HelloAnswer, HelloRequest, Limits, Namespace,
    Set, VERSION, WireError, decode, encode,
};

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
        let mut answer = Vec::new();
        stream.read_to_end(&mut answer).expect("an answer");
        Answer::parse(&answer)
    }

    fn post(&self, path: &str, body: &[u8]) -> Answer {
        self.ask("POST", path, &[("Content-Type", "application/cbor")], body)
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

/// A sealed pack of `values` in the store's format, laid out as `harness/packs.py` lays one
/// out: the magic, the counts, a block header and an entry per value, then the values as they
/// are (a value is a block of its own here, uncompressed).
fn pack(values: &[(Digest, &[u8])]) -> Vec<u8> {
    let mut out = b"rajitpk\x03".to_vec();
    out.extend_from_slice(&(values.len() as u32).to_le_bytes());
    out.extend_from_slice(&(values.len() as u32).to_le_bytes());
    for (_, value) in values {
        out.extend_from_slice(&(value.len() as u32).to_le_bytes());
        out.extend_from_slice(&(value.len() as u32).to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
    }
    let start = out.len() + 48 * values.len();
    let mut at = start;
    for (block, (key, value)) in values.iter().enumerate() {
        out.extend_from_slice(key);
        out.extend_from_slice(&(block as u32).to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
        out.extend_from_slice(&(value.len() as u32).to_le_bytes());
        let crc = crc32(key, value);
        out.extend_from_slice(&crc.to_le_bytes());
        at += value.len();
    }
    assert_eq!(at, start + values.iter().map(|(_, value)| value.len()).sum::<usize>());
    for (_, value) in values {
        out.extend_from_slice(value);
    }
    out
}

/// The CRC-32 of a record, as the store writes it: the key, the value's length, the value.
fn crc32(key: &Digest, value: &[u8]) -> u32 {
    let mut hasher = crc32fast::Hasher::new();
    hasher.update(key);
    hasher.update(&(value.len() as u32).to_le_bytes());
    hasher.update(value);
    hasher.finalize()
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

fn hello(served: &Served) -> Answer {
    let request = HelloRequest {
        versions: Set::new(vec![VERSION]),
        identity: blaze_wire::StoreIdentity([0; 32]),
        platform: platform(),
        role: 0,
    };
    served.post("/v1/hello", &encode(&request))
}

fn platform() -> blaze_wire::ExecPlatform {
    blaze_wire::ExecPlatform {
        triple: "aarch64-apple-darwin".into(),
        os: "macos".into(),
        sdk: None,
        libc: None,
        linker: ("ld".into(), "0.1".into(), [0; 32]),
        cpu: "arm64".into(),
    }
}

#[test]
fn hello_picks_the_version_the_client_speaks() {
    let served = Served::start();
    let answer = hello(&served);
    assert_eq!(answer.status, 200);
    let hello: HelloAnswer = decode(&answer.body, &Limits::DOCUMENTED).expect("a hello answer");
    assert_eq!(hello.version, VERSION);
    assert_eq!(hello.limits, Limits::DOCUMENTED);
    assert_eq!(hello.epochs.as_slice(), &[(Namespace::Main, 1)]);
    // C3 serves every identity, and holds none: C5 fills this.
    assert!(hello.identities.is_empty());
    assert_eq!(hello.key.len(), 32);
    // A version the server does not speak is code 2.
    let request = HelloRequest {
        versions: Set::new(vec![VERSION + 1]),
        identity: blaze_wire::StoreIdentity([0; 32]),
        platform: platform(),
        role: 0,
    };
    let refused = served.post("/v1/hello", &encode(&request));
    assert_eq!(refused.status, 400);
    assert_eq!(refused.error().code, 2);
    // Not canonical CBOR is code 1, and the validator names the offence.
    let broken = served.post("/v1/hello", b"\xa1\x00");
    assert_eq!(broken.status, 400);
    assert_eq!(broken.error().code, 1);
    assert!(
        broken.error().message.contains("hello")
            || broken.error().message.contains("offence")
            || !broken.error().message.is_empty()
    );
}

#[test]
fn a_blob_is_sent_whole_and_in_parts() {
    let served = Served::start();
    let bytes = pack(&[(digest(b"one"), b"value one"), (digest(b"two"), b"value two")]);
    let name = pack_name(&bytes);
    let put = served.ask("PUT", &format!("/v1/blob/{name}"), &[], &bytes);
    assert_eq!(put.status, 204, "{}", put.error().message);
    assert_eq!(put.header("x-blaze-verified"), Some("hashed"));

    let whole = served.ask("GET", &format!("/v1/blob/{name}"), &[], &[]);
    assert_eq!(whole.status, 200);
    assert_eq!(whole.body, bytes);
    assert_eq!(whole.header("content-length"), Some(bytes.len().to_string().as_str()));
    assert_eq!(whole.header("accept-ranges"), Some("bytes"));

    // A partial range: the bytes from 10 to 19, and the header says which of how many.
    let part = served.ask("GET", &format!("/v1/blob/{name}"), &[("Range", "bytes=10-19")], &[]);
    assert_eq!(part.status, 206);
    assert_eq!(part.body, &bytes[10..20]);
    assert_eq!(part.header("content-range"), Some(format!("bytes 10-19/{}", bytes.len()).as_str()));

    // Open-ended, past the end, and a suffix.
    let rest = served.ask("GET", &format!("/v1/blob/{name}"), &[("Range", "bytes=10-")], &[]);
    assert_eq!((rest.status, rest.body.as_slice()), (206, &bytes[10..]));
    let past = served.ask("GET", &format!("/v1/blob/{name}"), &[("Range", "bytes=999999-")], &[]);
    assert_eq!(past.status, 416);
    assert_eq!(past.header("content-range"), Some(format!("bytes */{}", bytes.len()).as_str()));
    let suffix = served.ask("GET", &format!("/v1/blob/{name}"), &[("Range", "bytes=-16")], &[]);
    assert_eq!((suffix.status, suffix.body.as_slice()), (206, &bytes[bytes.len() - 16..]));

    // A range that is not one is 416 too: nothing of the blob is sent.
    for bad in ["bytes=10", "bytes=9-8", "bytes=-", "bytes=0-1,3-4", "items=0-1"] {
        let answer = served.ask("GET", &format!("/v1/blob/{name}"), &[("Range", bad)], &[]);
        assert_eq!(answer.status, 416, "{bad} is a range");
        assert!(answer.body.is_empty());
    }

    // HEAD says how long it is and sends none of it.
    let head = served.ask("HEAD", &format!("/v1/blob/{name}"), &[], &[]);
    assert_eq!(head.status, 200);
    assert_eq!(head.header("content-length"), Some(bytes.len().to_string().as_str()));
    assert!(head.body.is_empty());

    // What is not there is 404, and code 5.
    let gone = served.ask("GET", &format!("/v1/blob/packs/{}.pack", "0".repeat(64)), &[], &[]);
    assert_eq!(gone.status, 404);
    assert_eq!(gone.error().code, 5);
}

#[test]
fn a_blob_sent_compressed_is_named_by_the_pack_it_holds() {
    let served = Served::start();
    let bytes = pack(&[(digest(b"one"), b"value one")]);
    let compressed = zstd::bulk::compress(&bytes, 3).expect("zstd");
    let name = pack_name(&bytes);
    let put = served.ask("PUT", &format!("/v1/blob/{name}.zst"), &[], &compressed);
    assert_eq!(put.status, 204, "{}", put.error().message);
    assert_eq!(put.header("x-blaze-verified"), Some("unpacked"));
    let got = served.ask("GET", &format!("/v1/blob/{name}.zst"), &[], &[]);
    assert_eq!(got.body, compressed);
}

#[test]
fn a_write_of_a_wrong_hash_is_refused_and_names_nothing() {
    let served = Served::start();
    let bytes = pack(&[(digest(b"one"), b"value one")]);
    let name = pack_name(&bytes);
    // The right bytes under another name: refused, and the name holds nothing after.
    let other = pack(&[(digest(b"other"), b"another value")]);
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

    // A keys object and a unit are named by their bytes' hash, and the same holds.
    let keys = b"a key list".to_vec();
    let keys_name = format!("keys/{}", hex(&digest(&keys)));
    assert_eq!(served.ask("PUT", &format!("/v1/blob/{keys_name}"), &[], &keys).status, 204);
    let wrong_keys = served.ask("PUT", &format!("/v1/blob/{keys_name}"), &[], b"other keys");
    assert_eq!(wrong_keys.status, 422);
    assert_eq!(served.ask("GET", &format!("/v1/blob/{keys_name}"), &[], &[]).body, keys);

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
        pack(&[(digest(b"one"), b"a value that is not small"), (digest(b"two"), &[7; 4096])]);
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
                let mut answer = Vec::new();
                stream.read_to_end(&mut answer).expect("an answer");
                Answer::parse(&answer).status
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
fn missing_is_answered_from_the_existence_set_with_none_one_and_4096_names() {
    let served = Served::start();
    let held = pack(&[(digest(b"one"), b"value one")]);
    let held_name = pack_name(&held);
    assert_eq!(served.ask("PUT", &format!("/v1/blob/{held_name}"), &[], &held).status, 204);
    let held_digest = digest(&held);
    let ask = |blobs: Vec<Digest>| -> CacheMissingAnswer {
        let answer = served
            .post("/v1/cache/missing", &encode(&CacheMissingRequest { blobs: Set::new(blobs) }));
        assert_eq!(answer.status, 200);
        decode(&answer.body, &Limits::DOCUMENTED).expect("a missing answer")
    };
    // None.
    let none = ask(Vec::new());
    assert!(none.missing.is_empty());
    // One, held.
    assert!(ask(vec![held_digest]).missing.is_empty());
    // One, not held.
    let absent = digest(b"nothing was ever sent under this");
    assert_eq!(ask(vec![absent]).missing.as_slice(), &[absent]);
    // 4,096 of them: the limit of a lookup, and the held one among them is the one left out.
    let many: Vec<Digest> = (0..4096u32)
        .map(|at| if at == 0 { held_digest } else { digest(&at.to_le_bytes()) })
        .collect();
    let answer = ask(many.clone());
    assert_eq!(answer.missing.len(), 4095);
    assert!(!answer.missing.as_slice().contains(&held_digest));
    // A lookup of more than 4,096 is refused: the validator's limit, code 1.
    let over: Vec<Digest> = (0..4097u32).map(|at| digest(&at.to_le_bytes())).collect();
    let refused =
        served.post("/v1/cache/missing", &encode(&CacheMissingRequest { blobs: Set::new(over) }));
    assert_eq!(refused.status, 400);
    assert_eq!(refused.error().code, 1);
}

#[test]
fn a_list_is_replaced_only_by_a_writer_that_read_what_was_there() {
    let served = Served::start();
    let path = "/v1/list/manifests/default";
    assert_eq!(served.ask("GET", path, &[], &[]).status, 404);
    let first = served.ask("PUT", path, &[("If-None-Match", "*")], b"first\n");
    assert_eq!(first.status, 204, "{}", first.error().message);
    // A second writer that read nothing is refused: it would undo the first.
    let second = served.ask("PUT", path, &[("If-None-Match", "*")], b"second\n");
    assert_eq!(second.status, 409);
    assert_eq!(second.error().code, 6);
    let read = served.ask("GET", path, &[], &[]);
    assert_eq!(read.body, b"first\n");
    let tag = read.header("etag").expect("an etag");
    // The writer that read it may replace it, and one that read the first after that may not.
    let replaced = served.ask("PUT", path, &[("If-Match", tag)], b"first\nsecond\n");
    assert_eq!(replaced.status, 204, "{}", replaced.error().message);
    let stale = served.ask("PUT", path, &[("If-Match", tag)], b"third\n");
    assert_eq!(stale.status, 409);
    assert_eq!(served.ask("GET", path, &[], &[]).body, b"first\nsecond\n");
}

#[test]
fn a_listing_says_what_is_there_and_a_delete_says_what_it_took() {
    let served = Served::start();
    let pack = pack(&[(digest(b"one"), b"value one")]);
    let name = pack_name(&pack);
    served.ask("PUT", &format!("/v1/blob/{name}"), &[], &pack);
    served.ask("PUT", "/v1/list/manifests/default", &[], b"a line\n");
    let answer = served.ask("GET", "/v1/objects?prefix=packs/", &[], &[]);
    assert_eq!(answer.status, 200);
    let listed = blaze_serve::listed(&answer.body).expect("a listing");
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].0, name);
    assert_eq!(listed[0].1, pack.len() as u64);
    assert!(listed[0].2 > 0);
    let all = served.ask("GET", "/v1/objects", &[], &[]);
    let names: Vec<String> =
        blaze_serve::listed(&all.body).expect("a listing").into_iter().map(|it| it.0).collect();
    assert_eq!(names, vec!["manifests/default".to_owned(), name.clone()]);
    assert_eq!(served.ask("DELETE", &format!("/v1/blob/{name}"), &[], &[]).status, 204);
    assert_eq!(served.ask("GET", &format!("/v1/blob/{name}"), &[], &[]).status, 404);
    let answer = served.post(
        "/v1/cache/missing",
        &encode(&CacheMissingRequest { blobs: Set::new(vec![digest(&pack)]) }),
    );
    let missing: CacheMissingAnswer =
        decode(&answer.body, &Limits::DOCUMENTED).expect("a missing answer");
    assert_eq!(missing.missing.as_slice(), &[digest(&pack)]);
}

#[test]
fn a_name_that_is_not_the_layouts_reaches_no_file() {
    let served = Served::start();
    for name in ["../Cargo.toml", "packs/../../etc/passwd", "packs/x.pack", "keys/..", "blobs/x"] {
        let answer = served.ask("GET", &format!("/v1/blob/{name}"), &[], &[]);
        assert_eq!(answer.status, 404, "{name} reached something");
    }
    let answer = served.ask("GET", "/v1/nonsense", &[], &[]);
    assert_eq!(answer.status, 404);
}

/// A path that is not of the layout is not read as one, and a digest alone finds its object.
#[test]
fn a_bare_digest_finds_the_object_named_by_it() {
    let served = Served::start();
    let keys = b"a key list".to_vec();
    let keys_digest = digest(&keys);
    let keys_name = format!("keys/{}", hex(&keys_digest));
    assert_eq!(served.ask("PUT", &format!("/v1/blob/{keys_name}"), &[], &keys).status, 204);
    let bare = served.ask("GET", &format!("/v1/blob/{}", hex(&keys_digest)), &[], &[]);
    assert_eq!(bare.status, 200);
    assert_eq!(bare.body, keys);
}

#[test]
fn a_compressed_pack_requires_its_named_route_and_has_no_raw_digest_etag() {
    let served = Served::start();
    let raw = pack(&[(digest(b"key"), b"a value")]);
    let compressed = zstd::bulk::compress(&raw, 3).unwrap();
    let d = hex(&digest(&raw));
    let path = format!("/v1/blob/packs/{d}.pack.zst");
    assert_eq!(served.ask("PUT", &path, &[], &compressed).status, 204);
    for method in ["GET", "HEAD"] {
        assert_eq!(served.ask(method, &format!("/v1/blob/{d}"), &[], &[]).status, 404);
        let answer = served.ask(method, &path, &[], &[]);
        assert_eq!(answer.status, 200);
        assert!(answer.header("etag").is_none());
        assert_eq!(answer.header("content-length"), Some(compressed.len().to_string().as_str()));
        if method == "GET" {
            assert_eq!(answer.body, compressed);
        } else {
            assert!(answer.body.is_empty());
        }
    }
}

#[test]
fn uppercase_digest_routes_are_rejected_without_creating_aliases() {
    let served = Served::start();
    let bytes = b"canonical lowercase";
    let d = hex(&digest(bytes));
    assert_ne!(d, d.to_uppercase());
    for name in [
        format!("units/{}", d.to_uppercase()),
        format!("keys/{}", d.to_uppercase()),
        format!("packs/{}.pack", d.to_uppercase()),
        d.to_uppercase(),
    ] {
        assert_eq!(served.ask("PUT", &format!("/v1/blob/{name}"), &[], bytes).status, 404);
    }
    let path = format!("/v1/blob/units/{d}");
    assert_eq!(served.ask("PUT", &path, &[], bytes).status, 204);
    assert_eq!(served.ask("GET", &path, &[], &[]).body, bytes);
    let listed = served.ask("GET", "/v1/objects?prefix=units/", &[], &[]);
    let objects = blaze_serve::listed(&listed.body).unwrap();
    assert_eq!(objects.iter().map(|it| &it.0).collect::<Vec<_>>(), vec![&format!("units/{d}")]);
}

#[cfg(unix)]
#[test]
fn the_server_key_is_private_when_created_and_when_reopened() {
    use std::os::unix::fs::PermissionsExt;
    let served = Served::start();
    let at = served.dir().join("key");
    assert_eq!(std::fs::metadata(&at).unwrap().permissions().mode() & 0o777, 0o600);
    let original = std::fs::read(&at).unwrap();
    std::fs::set_permissions(&at, std::fs::Permissions::from_mode(0o644)).unwrap();
    Server::open(&Config::at(served.dir(), 0)).unwrap();
    assert_eq!(std::fs::read(&at).unwrap(), original);
    assert_eq!(std::fs::metadata(&at).unwrap().permissions().mode() & 0o777, 0o600);
}

#[cfg(unix)]
#[test]
fn http_reads_reject_symlink_objects_and_oversize_upload_length() {
    let served = Served::start();
    let outside = tempfile::tempdir().unwrap();
    let bytes = b"outside";
    let name = format!("units/{}", hex(&digest(bytes)));
    std::fs::write(outside.path().join("secret"), bytes).unwrap();
    std::os::unix::fs::symlink(outside.path().join("secret"), served.dir().join(&name)).unwrap();
    assert_eq!(served.ask("GET", &format!("/v1/blob/{name}"), &[], &[]).status, 400);
    let mut stream = TcpStream::connect(("127.0.0.1", served.port)).unwrap();
    stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let request = format!(
        "PUT /v1/blob/units/{} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\nContent-Length: {}\r\n\r\n",
        "01".repeat(32),
        Limits::DOCUMENTED.blob + 1
    );
    stream.write_all(request.as_bytes()).unwrap();
    let mut answer = Vec::new();
    stream.read_to_end(&mut answer).unwrap();
    assert_eq!(Answer::parse(&answer).status, 413);
    assert!(
        std::fs::read_dir(served.dir().join("units")).unwrap().all(|entry| !entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with('.'))
    );
}
