use std::path::Path;

use blaze_api::{Response, Status};

use super::*;

/// Hands `on` each of `responses` as [`Client::call_ref`] does: encoded as the daemon sends it,
/// read where it lies, with whether another follows it; the last Done's `ok`. The fakes of
/// [`Api`] answer through it.
pub(crate) fn hand(responses: &[Response], mut on: impl FnMut(ResponseRef<'_>, bool)) -> bool {
    let mut ok = false;
    for (at, response) in responses.iter().enumerate() {
        if let Response::Done { ok: it } = response {
            ok = *it;
        }
        let bytes = blaze_protocol::cbor::to_vec(response).unwrap();
        on(
            blaze_protocol::cbor::from_slice::<ResponseRef<'_>>(&bytes).unwrap(),
            at + 1 < responses.len(),
        );
    }
    ok
}

#[test]
fn a_response_handed_in_place_is_the_one_sent() {
    let sent = [
        Response::Fetched {
            verb: blaze_api::FetchVerb::Updating,
            what: "crates.io index".to_owned(),
        },
        Response::Done { ok: true },
    ];
    let mut got = Vec::new();
    assert!(hand(&sent, |response, more| got.push((response.into_owned(), more))));
    assert_eq!(got, [(sent[0].clone(), true), (sent[1].clone(), false)]);
}

#[test]
fn a_start_that_never_listens_names_the_socket() {
    let said = not_listening(
        Path::new("/run/blaze/v2.sock"),
        Path::new("/c/blaze/v2/serve.log"),
        41,
        Phase::Setup,
        None,
    );
    assert_eq!(
        said,
        "the daemon (`blaze serve`, pid 41) was started and did not listen at /run/blaze/v2.sock: it was \
         installing its store and the toolchain's proc macros for 60s, and said nothing about what it was \
         inside; its log is /c/blaze/v2/serve.log"
    );
}

#[test]
fn the_status_as_json_has_every_field_with_or_without_a_daemon() {
    // It names the release, which a test hands over itself: no other test runs first.
    crate::release::for_tests();
    let place = StatusPlace {
        home: PathBuf::from("/h"),
        state: PathBuf::from("/h/v2"),
        socket: PathBuf::from("/h/v2.sock"),
    };
    let none: serde_json::Value = serde_json::from_str(&status_json(&place, None).unwrap()).unwrap();
    assert_eq!(none["daemon"], serde_json::Value::Null);
    assert_eq!(none["api_schema"], blaze_api::SCHEMA);
    assert_eq!(none["format"], 1);
    assert_eq!((none["home"].as_str(), none["socket"].as_str()), (Some("/h"), Some("/h/v2.sock")));
    let bug = |count| blaze_api::Bug {
        kind: "AdtDef".to_owned(),
        what: "formatted with no database attached".to_owned(),
        at: "a.rs:1:1".to_owned(),
        count,
        thread: None,
    };
    let status = Status {
        pid: 41,
        requests: vec![3, 7],
        views: 2,
        history: None,
        memory_bytes: 3 << 29,
        budget_bytes: 8 << 30,
        uptime_ms: 9000,
        build: blaze_api::Build::default(),
        bugs: vec![bug(2), bug(1)],
        settings: vec![
            blaze_api::Setting { name: "BLAZE_OPT".to_owned(), value: None },
            blaze_api::Setting {
                name: "BLAZE_DAEMON_MEMORY_GB".to_owned(),
                value: Some("80".to_owned()),
            },
        ],
        builds: Vec::new(),
        downloads: Vec::new(),
    };
    let line = status_json(&place, Some(&status)).unwrap();
    assert!(line.ends_with("}\n") && line.matches('\n').count() == 1, "{line}");
    let some: serde_json::Value = serde_json::from_str(&line).unwrap();
    let daemon = &some["daemon"];
    assert_eq!((daemon["pid"].as_u64(), daemon["uptime_ms"].as_u64()), (Some(41), Some(9000)));
    assert_eq!(daemon["bugs"], 3, "the bugs of every site, summed");
    assert_eq!(daemon["requests"], serde_json::json!([3, 7]));
    assert_eq!(
        daemon["settings"],
        serde_json::json!({"BLAZE_DAEMON_MEMORY_GB": "80", "BLAZE_OPT": null})
    );
    assert_eq!(daemon["build"].as_str().map(str::len), Some(64), "the build's hash in hex");
}

#[test]
fn the_status_for_people() {
    let status = Status {
        pid: 41,
        requests: vec![3, 7],
        views: 2,
        history: None,
        memory_bytes: 3 << 29,
        budget_bytes: 8 << 30,
        uptime_ms: 40,
        build: blaze_api::Build::default(),
        bugs: Vec::new(),
        settings: Vec::new(),
        builds: Vec::new(),
        downloads: Vec::new(),
    };
    assert_eq!(
        status_lines(&status),
        "pid: 41\nrequests: 2 (3, 7)\nviews: 2\nmemory: 1.5 GiB of 8.0 GiB\nbugs: 0\n"
    );
    let idle = Status {
        pid: 7,
        requests: Vec::new(),
        views: 0,
        history: None,
        memory_bytes: 512,
        budget_bytes: 1536,
        uptime_ms: 40,
        build: blaze_api::Build::default(),
        bugs: Vec::new(),
        settings: Vec::new(),
        builds: Vec::new(),
        downloads: Vec::new(),
    };
    assert_eq!(
        status_lines(&idle),
        "pid: 7\nrequests: 0\nviews: 0\nmemory: 512 B of 1.5 KiB\nbugs: 0\n"
    );
    let told = Status {
        settings: vec![
            blaze_api::Setting {
                name: "BLAZE_DAEMON_MEMORY_GB".to_owned(),
                value: Some("80".to_owned()),
            },
            blaze_api::Setting { name: "BLAZE_OPT".to_owned(), value: None },
        ],
        ..idle.clone()
    };
    assert_eq!(
        status_lines(&told),
        "pid: 7\nrequests: 0\nviews: 0\nmemory: 512 B of 1.5 KiB\nsettings: BLAZE_DAEMON_MEMORY_GB=80\nbugs: 0\n"
    );
    let failed = Status {
        history: Some(
            "the profile history: writing /h/0-00000003.spans.parquet: disk full".to_owned(),
        ),
        ..idle.clone()
    };
    assert!(status_lines(&failed).ends_with(
        "history: the profile history: writing /h/0-00000003.spans.parquet: disk full\nbugs: 0\n"
    ));
    let bug = |count, thread: Option<&str>| blaze_api::Bug {
        kind: "AdtDef".to_owned(),
        what: "formatted with no database attached".to_owned(),
        at: "crates/hir-ty/src/next_solver/interner.rs:951:9".to_owned(),
        count,
        thread: thread.map(str::to_owned),
    };
    let buggy = Status { bugs: vec![bug(2, Some("blaze-worker-3")), bug(1, None)], ..idle };
    assert!(
        status_lines(&buggy).ends_with(
            "bugs: 3\n  a blaze bug: `AdtDef` formatted with no database attached at \
             crates/hir-ty/src/next_solver/interner.rs:951:9, 2 times, first on thread `blaze-worker-3`\n  \
             a blaze bug: `AdtDef` formatted with no database attached at \
             crates/hir-ty/src/next_solver/interner.rs:951:9, 1 time\n"
        ),
        "{}",
        status_lines(&buggy)
    );
    let building = Status {
        builds: vec![blaze_api::Building {
            id: 12,
            now: blaze_protocol::now::Now {
                at_ms: 125_400,
                phase: blaze_protocol::now::Phase::Compiling,
                stage: "builds 412 units, 1 running".to_owned(),
                ..Default::default()
            },
            what: "test -p blaze-api".to_owned(),
            phase: "builds 412 units, 1 running".to_owned(),
            spans: vec!["blaze-worker-3: task (2 min 05 s) > unit bodies `app::main` in app (4 s)".to_owned()],
            for_ms: 125_400,
        }],
        downloads: vec![blaze_api::Downloading {
            url: "https://example.com/llvm.tar.xz".to_owned(),
            done: 34_000_000,
            rate: None,
            what: "llvm.tar.xz".to_owned(),
            total: Some(410_000_000),
            idle_ms: 0,
            elapsed_ms: 9_000,
        }],
        ..buggy
    };
    assert!(
        status_lines(&building).ends_with(
            "builds: 1\n  #12 test -p blaze-api: [2m05s] compiling · mem 0 B · builds 412 units, 1 running\n    \
             blaze-worker-3: task (2 min 05 s) > unit bodies `app::main` in app (4 s)\n\
             downloads: 1\n  llvm.tar.xz: 34.0 MB of 410.0 MB (8%)\n    from https://example.com/llvm.tar.xz\n"
        ),
        "{}",
        status_lines(&building)
    );
}

/// A daemon of one build: its Status says `build`; every request it is sent is kept, and each
/// other than Status is answered Done.
struct OfBuild {
    build: blaze_api::Build,
    asked: Vec<Request>,
    /// What its Status says it was started with.
    settings: Vec<blaze_api::Setting>,
}

impl Api for OfBuild {
    fn call<F: FnMut(ResponseRef<'_>, bool)>(
        &mut self,
        request: Request,
        on: F,
    ) -> Result<bool, ClientError> {
        let answer = match request {
            Request::Status => vec![
                Response::Status(Status {
                    pid: 77,
                    requests: Vec::new(),
                    settings: self.settings.clone(),
                    views: 0,
                    history: None,
                    memory_bytes: 0,
                    budget_bytes: 0,
                    uptime_ms: 40,
                    build: self.build.clone(),
                    bugs: Vec::new(),
                    builds: Vec::new(),
                    downloads: Vec::new(),
                }),
                Response::Done { ok: true },
            ],
            _ => vec![Response::Done { ok: true }],
        };
        self.asked.push(request);
        Ok(hand(&answer, on))
    }
}

fn build(id: u8, path: &str, inode: u64) -> blaze_api::Build {
    blaze_api::Build {
        id: blaze_ids::ContentId([id; 32]),
        exe: blaze_api::Executable { path: path.to_owned(), len: 1 << 20, modified_ns: 5, inode },
    }
}

const SOCKET: &str = "/run/blaze/v2.sock";

#[test]
fn missing_and_stale_sockets_start_only_under_the_default_policy() {
    for kind in [io::ErrorKind::NotFound, io::ErrorKind::ConnectionRefused] {
        for start in [Start::IfMissing, Start::Never, Start::ConnectOnly] {
            let result = find::<()>(
                start,
                Path::new(SOCKET),
                || Ok(false),
                || Err(ClientError::Io(io::Error::from(kind))),
                |_| panic!("checked a daemon that did not answer"),
            )
            .unwrap();
            assert_eq!(matches!(&result, Found::Start(Some(_))), start == Start::IfMissing);
            assert_eq!(matches!(&result, Found::Ready(None)), start != Start::IfMissing);
        }
    }
    for start in [Start::IfMissing, Start::Never, Start::ConnectOnly] {
        assert!(
            find::<()>(
                start,
                Path::new(SOCKET),
                || Ok(false),
                || Err(ClientError::Io(io::Error::from(io::ErrorKind::PermissionDenied))),
                |_| unreachable!()
            )
            .is_err()
        );
    }
}

#[test]
fn connect_only_refuses_activation_before_dialing_and_activation_check_errors() {
    let why = match find::<()>(
        Start::ConnectOnly,
        Path::new(SOCKET),
        || Ok(true),
        || panic!("dialed a socket that could activate a daemon"),
        |_| unreachable!(),
    ) {
        Err(why) => why.to_string(),
        Ok(_) => panic!("accepted activation"),
    };
    assert!(why.contains(SOCKET) && why.contains("connect-only-v1"));
    assert!(
        find::<()>(
            Start::ConnectOnly,
            Path::new(SOCKET),
            || anyhow::bail!("unit state is unread"),
            || panic!("dialed with unread activation state"),
            |_| unreachable!()
        )
        .is_err()
    );
    // Legacy policies neither inspect nor change activation behavior.
    for start in [Start::IfMissing, Start::Never] {
        assert!(matches!(
            find(
                start,
                Path::new(SOCKET),
                || panic!("changed default policy"),
                || Ok(()),
                |client| Ok(Some(client))
            )
            .unwrap(),
            Found::Ready(Some(()))
        ));
    }
}

#[test]
fn connect_only_never_starts_after_a_failed_or_incomplete_identity_check() {
    assert!(
        find(
            Start::ConnectOnly,
            Path::new(SOCKET),
            || Ok(false),
            || Ok(()),
            |_| anyhow::bail!("schema or settings mismatch")
        )
        .is_err()
    );
    assert!(
        find(Start::ConnectOnly, Path::new(SOCKET), || Ok(false), || Ok(()), |_| Ok(None)).is_err()
    );
}

/// A deterministic transcript through the real policy core and build classifier. Hegel
/// draws absent/stale/live peers, build skew and activation between successive client calls.
/// It performs no socket, process, clock, filesystem or environment operations.
fn policy_transcript(steps: &[(u8, bool)], start: Start, plant_replace: bool) -> bool {
    use std::cell::Cell;
    let a = build(1, "/old/blaze", 10);
    let b = build(2, "/new/blaze", 20);
    for &(peer, activated) in steps {
        let stopped = Cell::new(false);
        let dialed = Cell::new(false);
        let found = find(
            start,
            Path::new(SOCKET),
            || Ok(activated),
            || {
                dialed.set(true);
                match peer {
                    0 => Err(ClientError::Io(io::Error::from(io::ErrorKind::NotFound))),
                    1 => Err(ClientError::Io(io::Error::from(io::ErrorKind::ConnectionRefused))),
                    _ => Ok(OfBuild {
                        build: if peer == 2 { b.clone() } else { a.clone() },
                        asked: Vec::new(),
                        settings: Vec::new(),
                    }),
                }
            },
            |mut api| {
                match builds(&mut api, Path::new(SOCKET), &b.exe, &[], || Ok(b.clone()))? {
                    Builds::Same => Ok(Some(api)),
                    Builds::Other { pid, theirs, ours } => {
                        if !plant_replace {
                            allow_replace(start, Path::new(SOCKET), pid, &theirs, &ours)?;
                        }
                        // The driver's replacement effects occur only after this gate permits them.
                        stopped.set(true);
                        Ok(None)
                    }
                    Builds::Settings { .. } => unreachable!(),
                }
            },
        );
        let spawning = matches!(&found, Ok(Found::Start(_)));
        let safe = match start {
            Start::ConnectOnly => {
                !spawning
                    && !stopped.get()
                    && (!activated || !dialed.get())
                    && (activated || peer != 2 || matches!(&found, Ok(Found::Ready(Some(_)))))
            }
            Start::IfMissing => spawning == (peer != 2) && stopped.get() == (peer == 3),
            Start::Never => !spawning && !stopped.get() && dialed.get(),
        };
        if !safe {
            return false;
        }
    }
    true
}

/// The invariant: connect-only emits no spawn or implicit Stop for missing, stale, live or
/// mismatched peers, and never dials known activation; default replacement remains enabled.
#[hegel::test]
fn client_policy_transcripts(tc: hegel::TestCase) {
    use hegel::generators as gs;
    let steps: Vec<(u8, bool)> = tc.draw(
        gs::vecs(hegel::tuples!(gs::integers::<u8>().max_value(3), gs::booleans()))
            .min_size(1)
            .max_size(40),
    );
    for start in [Start::IfMissing, Start::Never, Start::ConnectOnly] {
        assert!(policy_transcript(&steps, start, false));
    }
}

#[test]
fn the_oracle_catches_implicit_stop_of_a_mismatched_daemon() {
    assert!(!policy_transcript(&[(3, false)], Start::ConnectOnly, true));
    assert!(policy_transcript(&[(3, false)], Start::ConnectOnly, false));
}

#[test]
fn a_refused_replacement_names_both_executables_and_the_daemon() {
    let error = allow_replace(
        Start::ConnectOnly,
        Path::new(SOCKET),
        77,
        &build(1, "/old/blaze", 10),
        &build(2, "/new/blaze", 20),
    )
    .unwrap_err()
    .to_string();
    for text in [SOCKET, "77", "/old/blaze", "/new/blaze", "no Stop was sent"] {
        assert!(error.contains(text), "{error}");
    }
}

#[test]
fn explicit_connect_only_stop_waits_but_never_reconnects_after_hangup() {
    use std::cell::Cell;
    let waited = Cell::new(false);
    finish_stop(
        Start::ConnectOnly,
        Path::new(SOCKET),
        || {
            waited.set(true);
            Ok(())
        },
        || panic!("reconnected after the requested Stop"),
    )
    .unwrap();
    assert!(waited.get());
    let failed = finish_stop(
        Start::ConnectOnly,
        Path::new(SOCKET),
        || Err(ClientError::Io(io::Error::from(io::ErrorKind::TimedOut))),
        || panic!("probed after a failed wait"),
    )
    .unwrap_err();
    assert!(failed.to_string().contains(SOCKET));
    assert!(finish_stop(Start::Never, Path::new(SOCKET), || Ok(()), || Ok(false)).is_ok());
    assert!(finish_stop(Start::Never, Path::new(SOCKET), || Ok(()), || Ok(true)).is_err());
}

#[cfg(not(feature = "native"))]
#[test]
fn connect_only_never_starts_the_seed_engine() {
    let Err(error) = connect(Start::ConnectOnly) else { panic!("seed engine started") };
    assert!(error.to_string().contains("connect-only-v1"));
}

/// A client of build B finds a daemon of build A: only Status is sent to it, and the answer
/// names both builds and the daemon's pid; no request of B's runs on A.
#[test]
fn a_client_of_another_build_sends_the_daemon_only_its_status() {
    let a = build(1, "/old/blaze", 10);
    let b = build(2, "/new/blaze", 20);
    let mut daemon = OfBuild { build: a.clone(), asked: Vec::new(), settings: Vec::new() };
    let got = builds(&mut daemon, Path::new(SOCKET), &b.exe, &[], || Ok(b.clone())).unwrap();
    assert_eq!(got, Builds::Other { pid: 77, theirs: a, ours: b });
    assert_eq!(
        daemon.asked,
        [Request::Status],
        "nothing but its Status is asked of another build's daemon"
    );
}

/// The daemon's executable, unchanged, is this blaze: nothing is hashed.
#[test]
fn a_daemon_of_this_very_file_is_this_build_with_nothing_hashed() {
    let a = build(1, "/opt/blaze", 10);
    let mut daemon = OfBuild { build: a.clone(), asked: Vec::new(), settings: Vec::new() };
    let got = builds(&mut daemon, Path::new(SOCKET), &a.exe, &[], || {
        panic!("hashed this blaze for an unchanged file")
    });
    assert_eq!(got.unwrap(), Builds::Same);
}

/// Another file of the same bytes (a copy, a link made again) is the same build: its hash says so.
#[test]
fn a_daemon_of_another_file_with_the_same_bytes_is_this_build() {
    let a = build(1, "/opt/blaze", 10);
    let copy = build(1, "/tmp/blaze", 11);
    let mut daemon = OfBuild { build: a, asked: Vec::new(), settings: Vec::new() };
    let got = builds(&mut daemon, Path::new(SOCKET), &copy.exe, &[], || Ok(copy.clone())).unwrap();
    assert_eq!(got, Builds::Same);
}

/// Replacing the socket pathname after Status cannot redirect a connect-only command.
#[cfg(all(unix, feature = "native"))]
#[test]
fn connect_only_keeps_the_checked_connection_after_the_listener_is_replaced() {
    use std::io::Write as _;

    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("s");
    let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
    let exe = std::env::current_exe().unwrap();
    let file = std::fs::File::open(&exe).unwrap();
    let mut known = OfBuild {
        build: blaze_api::Build {
            exe: crate::serve::executable(&exe, &file).unwrap(),
            ..blaze_api::Build::default()
        },
        settings: crate::serve::settings::of_client(|name| std::env::var_os(name)),
        asked: Vec::new(),
    };
    let mut answers = Vec::new();
    known.call(Request::Status, |it, _| answers.push(it.into_owned())).unwrap();
    let path = socket.clone();
    let (sent, received) = std::sync::mpsc::sync_channel(1);
    let daemon = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
        let item = blaze_protocol::read_frame(&mut stream).unwrap().unwrap();
        let status = blaze_api::frames::decode::<Request>(&item).unwrap();
        assert_eq!(status.body, Request::Status);
        std::fs::remove_file(&path).unwrap();
        let replacement = std::os::unix::net::UnixListener::bind(&path).unwrap();
        replacement.set_nonblocking(true).unwrap();
        sent.send(replacement).unwrap();
        for answer in answers {
            stream
                .write_all(
                    &blaze_api::frames::encode(&blaze_api::Envelope::new(status.id, answer))
                        .unwrap(),
                )
                .unwrap();
        }
        let item = blaze_protocol::read_frame(&mut stream).unwrap().unwrap();
        let command = blaze_api::frames::decode::<Request>(&item).unwrap();
        assert_eq!(command.body, Request::Stop);
        assert_ne!(command.id, status.id);
        stream
            .write_all(
                &blaze_api::frames::encode(&blaze_api::Envelope::new(
                    command.id,
                    Response::Done { ok: true },
                ))
                .unwrap(),
            )
            .unwrap();
    });
    let client = Client::unix(&socket).unwrap();
    let mut client = this_build(client, &socket, Start::ConnectOnly).unwrap().unwrap();
    let replacement = received.recv_timeout(Duration::from_secs(3)).unwrap();
    assert_eq!(replacement.accept().unwrap_err().kind(), io::ErrorKind::WouldBlock);
    assert!(told_to_stop(&mut client, |_, _| {}).unwrap());
    client.until_hung_up(Duration::from_secs(3)).unwrap();
    daemon.join().unwrap();
}

/// A daemon of this build told another memory limit than this command sets (the 80 GB daemon
/// a client with `BLAZE_DAEMON_MEMORY_GB=100` built on, failing ten minutes in) is found from its
/// Status alone, with the setting and both values, for the CLI to refuse before any request. A
/// command that leaves the limit unset builds with the daemon's, `--memory-gb 80` included.
#[test]
fn a_daemon_started_with_another_limit_is_found_before_any_request() {
    let a = build(1, "/opt/blaze", 10);
    let theirs = crate::serve::settings::of_daemon(
        Some(&(80u64 << 30, "--memory-gb=80".to_owned())),
        |_| None,
    );
    let ours = crate::serve::settings::of_client(|name| {
        (name == crate::serve::MEMORY_VAR).then(|| "100".into())
    });
    let mut daemon = OfBuild { build: a.clone(), asked: Vec::new(), settings: theirs.clone() };
    let got = builds(&mut daemon, Path::new(SOCKET), &a.exe, &ours, || {
        panic!("hashed this blaze for an unchanged file")
    })
    .unwrap();
    let drift = vec![crate::serve::settings::Drift {
        name: crate::serve::MEMORY_VAR.to_owned(),
        theirs: Some("80".to_owned()),
        ours: "100".to_owned(),
    }];
    assert_eq!(got, Builds::Settings { pid: 77, drift });
    assert_eq!(
        daemon.asked,
        [Request::Status],
        "nothing but its Status is asked before the settings agree"
    );
    for asked in [Some("80"), None] {
        let mut same = OfBuild { build: a.clone(), asked: Vec::new(), settings: theirs.clone() };
        let ours = crate::serve::settings::of_client(|name| {
            (name == crate::serve::MEMORY_VAR).then_some(asked).flatten().map(Into::into)
        });
        assert_eq!(
            builds(&mut same, Path::new(SOCKET), &a.exe, &ours, || panic!("hashed")).unwrap(),
            Builds::Same,
            "{asked:?}"
        );
    }
}

/// A daemon of another schema can't be told to stop by this client: the error names both.
#[test]
fn a_daemon_of_another_schema_is_refused_naming_both() {
    struct OtherSchema;
    impl Api for OtherSchema {
        fn call<F: FnMut(ResponseRef<'_>, bool)>(
            &mut self,
            _: Request,
            _: F,
        ) -> Result<bool, ClientError> {
            Err(ClientError::Schema { theirs: blaze_api::SCHEMA - 1 })
        }
    }
    let ours = build(2, "/new/blaze", 20);
    let why = builds(&mut OtherSchema, Path::new(SOCKET), &ours.exe, &[], || Ok(ours.clone()))
        .unwrap_err()
        .to_string();
    let schemas =
        format!("speaks API schema {}, this blaze {}", blaze_api::SCHEMA - 1, blaze_api::SCHEMA);
    assert!(why.contains(SOCKET) && why.contains(&schemas), "{why}");
}

/// The daemon answers Stop's Done at once and says each step of its stop on the same request
/// right after, so one read often holds the Done and a step: `blaze stop` (and the replacing of
/// another build's daemon) takes them as the steps of the wait, never as a protocol error.
#[cfg(unix)]
#[test]
fn a_stops_steps_read_with_its_done_are_the_waits() {
    use std::io::{Read as _, Write as _};
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("s");
    let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
    let daemon = std::thread::spawn(move || {
        for _ in 0..2 {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0u8; 64];
            _ = stream.read(&mut request).unwrap();
            let mut burst = blaze_api::frames::encode(&blaze_api::Envelope::new(
                1,
                Response::Done { ok: true },
            ))
            .unwrap();
            let step = Response::Stopping { doing: "writing the daemon image".to_owned() };
            burst.extend(blaze_api::frames::encode(&blaze_api::Envelope::new(1, step)).unwrap());
            stream.write_all(&burst).unwrap();
        }
    });
    let mut client = Client::unix(&socket).unwrap();
    assert!(told_to_stop(&mut client, |_, _| {}).unwrap());
    client.until_hung_up(std::time::Duration::from_secs(10)).unwrap();
    // What `Api::call` makes of the same burst: the error `blaze stop` failed with at random.
    let mut client = Client::unix(&socket).unwrap();
    assert!(Api::call(&mut client, Request::Stop, |_, _| {}).is_err());
    daemon.join().unwrap();
}
