//! Real Unix sockets and exact production coordinator/client code, not an in-memory network.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicU64, Ordering},
};
use std::time::Duration;

use blaze_dist::admission::{Admission, AdmissionConfig};
use blaze_dist::jobs::{Effect, ExecutionConfig, Jobs, LocalAction};
use blaze_dist_local::client::{Client, terminal_action};
use blaze_dist_local::coordinator::Config;
use blaze_dist_local::protocol::*;
use blaze_dist_local::server::{Clock, Server, frame, random_id};
use blaze_wire::{
    Attempt, BatchResult, DoneEvent, ExecutionId, ItemOutcome, ItemResult, JobEvent, JobRequest,
    Limits, LocalPolicy, Outcome, Reservation, RootSet, Set, Stage, StageEvent, Why,
    WorkJoinRequest, WorkLeaseRequest, WorkName, WorkersEventsRequest, decode, encode,
};

#[derive(Default)]
struct TestClock(AtomicU64);
impl Clock for TestClock {
    fn now_ms(&self) -> u64 {
        self.0.load(Ordering::Acquire)
    }
}

struct Fixture {
    path: PathBuf,
    stop: Arc<AtomicBool>,
    clock: Arc<TestClock>,
    effects: Arc<Mutex<Vec<(ExecutionId, Effect)>>>,
    thread: Option<std::thread::JoinHandle<()>>,
    executor: String,
}

impl Fixture {
    fn new(builder: bool, config: Config) -> Self {
        Self::with_callback(builder, config, |_| {})
    }

    fn with_callback(
        builder: bool,
        config: Config,
        mut callback: impl FnMut(&[(ExecutionId, Effect)]) + Send + 'static,
    ) -> Self {
        let id = random_id().unwrap();
        let tag: String = id[..8].iter().map(|b| format!("{b:02x}")).collect();
        let path = PathBuf::from(format!("/tmp/blaze-dist-{tag}.sock"));
        let mut admission = Admission::new(AdmissionConfig::default());
        let executor = if builder {
            admission.add_machine(
                "machine",
                [1; 32],
                Reservation { memory: 16, cpus: 4, disk: 16 },
            );
            admission.add_builder(0, "machine", need()).unwrap()
        } else {
            String::new()
        };
        let jobs = Jobs::new(admission, ExecutionConfig::default());
        let clock = Arc::new(TestClock::default());
        let stop = Arc::new(AtomicBool::new(false));
        let effects = Arc::new(Mutex::new(Vec::new()));
        let delivered = effects.clone();
        let server = Server::bind(
            &path,
            jobs,
            config,
            clock.clone(),
            move |batch: Vec<(ExecutionId, Effect)>| {
                callback(&batch);
                delivered.lock().unwrap().extend(batch);
            },
        )
        .unwrap();
        let signal = stop.clone();
        let thread = std::thread::spawn(move || server.run(signal).unwrap());
        Self { path, stop, clock, effects, thread: Some(thread), executor }
    }

    fn client(&self) -> Client {
        Client::connect(&self.path, random_id().unwrap()).unwrap()
    }
    fn at(&self, now: u64) {
        self.clock.0.store(now, Ordering::Release);
        std::thread::sleep(Duration::from_millis(10));
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        self.thread.take().unwrap().join().unwrap();
        assert!(!self.path.exists());
    }
}

fn need() -> Reservation {
    Reservation { memory: 4, cpus: 1, disk: 4 }
}

fn job(deadline_ms: u64) -> JobRequest {
    let mut spec =
        <blaze_wire::BuildSpec as blaze_wire::Sample>::sample(&mut blaze_wire::Gen::new(1));
    spec.platform = [1; 32];
    JobRequest {
        spec,
        namespace: blaze_wire::Namespace::Main,
        deadline_ms,
        local: LocalPolicy::Fallback,
        test_timeout_ms: None,
    }
}

fn submit(client: &mut Client, deadline: u64) -> Binding {
    let Answer::Binding(binding) =
        client.call(Command::Submit(submission(&job(deadline), need()))).unwrap()
    else {
        panic!("submit failed");
    };
    binding
}

fn read(stream: &mut UnixStream) -> ServerFrame {
    let mut h = [0; 4];
    stream.read_exact(&mut h).unwrap();
    let n = u32::from_be_bytes(h);
    assert!(n < 1 << 20);
    let mut body = vec![0; n as usize];
    stream.read_exact(&mut body).unwrap();
    decode(&body, &Limits::DOCUMENTED).unwrap()
}

fn raw(f: &Fixture) -> UnixStream {
    let stream = UnixStream::connect(&f.path).unwrap();
    stream.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
    stream
}

fn hello_raw(
    _f: &Fixture,
    stream: &mut UnixStream,
    session: blaze_wire::Digest,
    epoch: Option<blaze_wire::Digest>,
) -> LocalWelcome {
    stream.write_all(&frame(&encode(&ClientFrame::Hello(hello(session, epoch))))).unwrap();
    let ServerFrame::Welcome(w) = read(stream) else {
        panic!("hello");
    };
    w
}

#[test]
fn fragmented_coalesced_and_malformed_frames_are_real_socket_boundaries() {
    let f = Fixture::new(false, Config::default());
    let mut stream = raw(&f);
    let bytes = frame(&encode(&ClientFrame::Hello(hello([1; 32], None))));
    for byte in bytes {
        stream.write_all(&[byte]).unwrap();
    }
    assert!(matches!(read(&mut stream), ServerFrame::Welcome(_)));
    let mut bad = raw(&f);
    bad.write_all(&[0, 16, 0, 1]).unwrap(); // oversize before any allocation/body.
    assert!(matches!(read(&mut bad), ServerFrame::Error(_)));
    let mut bad = raw(&f);
    bad.write_all(&frame(&[0xff])).unwrap();
    assert!(matches!(read(&mut bad), ServerFrame::Error(_)));
    let mut bad = raw(&f);
    bad.write_all(&[0, 0, 0, 0]).unwrap();
    assert!(matches!(read(&mut bad), ServerFrame::Error(_)));
    let mut coalesced = raw(&f);
    let h = hello([2; 32], None);
    let mut bytes = frame(&encode(&ClientFrame::Hello(h.clone())));
    bytes.extend(frame(&encode(&ClientFrame::Hello(h))));
    coalesced.write_all(&bytes).unwrap();
    assert!(matches!(read(&mut coalesced), ServerFrame::Welcome(_)));
    assert!(matches!(read(&mut coalesced), ServerFrame::Error(_)));
}

#[test]
fn versions_content_session_and_terminal_bindings_fail_closed() {
    let f = Fixture::new(true, Config::default());
    let mut stream = raw(&f);
    let mut h = hello([3; 32], None);
    h.codec += 1;
    stream.write_all(&frame(&encode(&ClientFrame::Hello(h)))).unwrap();
    assert!(matches!(read(&mut stream), ServerFrame::Error(_)));
    let mut c = f.client();
    let mut s = submission(&job(1000), need());
    s.content = blaze_wire::JobContent([0; 32]);
    assert!(matches!(c.call(Command::Submit(s)).unwrap(), Answer::Error(_)));
    let b = submit(&mut c, 1000);
    let mut wrong = b.subscription.clone();
    wrong.subscriber += 1;
    assert!(matches!(c.call(Command::Terminal(wrong.clone())).unwrap(), Answer::Error(_)));
    let t = Terminal { subscription: wrong, value: None };
    assert!(terminal_action(&b, &t, &LocalPolicy::Fallback).is_err());
}

#[test]
fn disconnect_retry_has_identical_response_and_no_duplicate_submit_or_multistage_events() {
    let f = Fixture::new(true, Config::default());
    let mut stream = raw(&f);
    let w = hello_raw(&f, &mut stream, [4; 32], None);
    let req = Request {
        epoch: w.epoch,
        session: w.session,
        sequence: 1,
        command: Command::Submit(submission(&job(1000), need())),
    };
    stream.write_all(&frame(&encode(&ClientFrame::Request(req.clone())))).unwrap();
    // Submit can commit before its response is lost. The retry key survives the socket.
    drop(stream);
    std::thread::sleep(Duration::from_millis(20));
    let mut stream = raw(&f);
    hello_raw(&f, &mut stream, [4; 32], Some(w.epoch));
    let bytes = frame(&encode(&ClientFrame::Request(req.clone())));
    stream.write_all(&bytes).unwrap();
    let first = read(&mut stream);
    stream.write_all(&bytes).unwrap();
    assert_eq!(read(&mut stream), first);
    let ServerFrame::Response(Response { answer: Answer::Binding(b), .. }) = first else {
        panic!()
    };
    let progress = Request {
        epoch: w.epoch,
        session: w.session,
        sequence: 2,
        command: Command::WorkerEvents(WorkerEvents {
            content: b.subscription.content,
            executor: f.executor.clone(),
            request: WorkersEventsRequest {
                execution: b.subscription.execution,
                attempt: b.attempt,
                events: vec![
                    JobEvent::Stage(StageEvent { stage: Stage::Fetched, at_ms: u64::MAX }),
                    JobEvent::Stage(StageEvent { stage: Stage::Loaded, at_ms: u64::MAX }),
                ],
            },
        }),
    };
    let bytes = frame(&encode(&ClientFrame::Request(progress)));
    stream.write_all(&bytes).unwrap();
    let first = read(&mut stream);
    stream.write_all(&bytes).unwrap();
    assert_eq!(read(&mut stream), first);
    let effects = f.effects.lock().unwrap();
    assert_eq!(
        effects
            .iter()
            .filter(|(_, e)| matches!(e, Effect::Event(i)
        if matches!(&i.event, JobEvent::Stage(s) if s.stage == Stage::Loaded)))
            .count(),
        1
    );
    assert_eq!(
        effects
            .iter()
            .filter(|(_, e)| matches!(e, Effect::Event(i)
        if matches!(i.event, JobEvent::Assigned(_))))
            .count(),
        1
    );
}

#[test]
fn actual_work_completion_terminal_and_kill_callback_roundtrip() {
    let f = Fixture::new(true, Config::default());
    let mut c = f.client();
    let b = submit(&mut c, 1000);
    let content = b.subscription.content;
    let execution = b.subscription.execution;
    let passed = WorkerEvents {
        content,
        executor: f.executor.clone(),
        request: WorkersEventsRequest {
            execution,
            attempt: b.attempt,
            events: vec![JobEvent::Done(DoneEvent {
                outcome: Outcome::Passed,
                counters: Vec::new(),
            })],
        },
    };
    assert!(matches!(c.call(Command::WorkerEvents(passed.clone())).unwrap(), Answer::Error(_)));
    let join = Join {
        content,
        executor: f.executor.clone(),
        request: WorkJoinRequest {
            execution,
            attempt: b.attempt,
            roots: RootSet {
                spec: [1; 32],
                roots: [2; 32],
                count: 1,
                index: [3; 32],
                inputs: [4; 32],
            },
            rejected: Set::new(Vec::new()),
        },
    };
    assert!(matches!(c.call(Command::Join(join)).unwrap(), Answer::Join(_)));
    let Answer::Lease(answer) = c
        .call(Command::Lease(Lease {
            content,
            executor: f.executor.clone(),
            request: WorkLeaseRequest { execution, attempt: b.attempt },
        }))
        .unwrap()
    else {
        panic!()
    };
    let grant = answer.grant.unwrap();
    assert!(matches!(
        c.call(Command::Result(BatchSubmission {
            content,
            executor: f.executor.clone(),
            request: BatchResult {
                lease: grant.lease.clone(),
                seq: 1,
                items: vec![ItemResult {
                    name: WorkName([9; 32]),
                    outcome: ItemOutcome::Local(Why { why: "not exportable".to_owned() }),
                    found: Set::new(Vec::new()),
                    unnamed: Set::new(Vec::new())
                }],
                packs: Vec::new(),
                returned_roots: Set::new(Vec::new()),
                returned_names: Set::new(Vec::new()),
            }
        }))
        .unwrap(),
        Answer::Result(_)
    ));
    assert!(matches!(c.call(Command::WorkerEvents(passed)).unwrap(), Answer::WorkerEvents(_)));
    let Answer::Terminal(t) = c.call(Command::Terminal(b.subscription.clone())).unwrap() else {
        panic!()
    };
    assert_eq!(terminal_action(&b, &t, &LocalPolicy::Fallback).unwrap(), Some(LocalAction::Take));
    assert_eq!(t.value.as_ref().unwrap().termination.outcome, Outcome::Passed);
    let mut wrong = t.clone();
    wrong.value.as_mut().unwrap().attempt = Attempt(99);
    assert!(terminal_action(&b, &wrong, &LocalPolicy::Fallback).is_err());
    let assignment = f
        .effects
        .lock()
        .unwrap()
        .iter()
        .find_map(|(_, effect)| match effect {
            Effect::Kill { executor, assignment } if executor == &f.executor => Some(*assignment),
            _ => None,
        })
        .expect("actual callback delivered stop");
    let ack = KillAck {
        content,
        executor: f.executor.clone(),
        execution: assignment.execution,
        attempt: assignment.attempt,
        alive: true,
    };
    assert_eq!(
        c.call(Command::KillAck(ack.clone())).unwrap(),
        Answer::KillAck(KillAnswer { accepted: true })
    );
    assert_eq!(
        c.call(Command::KillAck(ack)).unwrap(),
        Answer::KillAck(KillAnswer { accepted: false })
    );
}

#[test]
fn reconnect_subscribe_replay_cancel_and_deadline_preserve_interests() {
    let f = Fixture::new(true, Config::default());
    let mut c = f.client();
    let a = submit(&mut c, 100);
    let Answer::Binding(b) = c
        .call(Command::Subscribe(Subscribe {
            content: a.subscription.content,
            execution: a.subscription.execution,
            deadline_ms: 200,
            policy: LocalPolicy::Never,
        }))
        .unwrap()
    else {
        panic!()
    };
    c.reconnect(&f.path).unwrap();
    assert_eq!(
        c.call(Command::Resume(a.subscription.clone())).unwrap(),
        Answer::Binding(a.clone())
    );
    let Answer::Events(first) =
        c.call(Command::Events(Events { binding: a.clone(), from: 0, limit: 256 })).unwrap()
    else {
        panic!()
    };
    assert!(!first.events.is_empty());
    assert_eq!(
        c.call(Command::Events(Events { binding: a.clone(), from: 0, limit: 256 })).unwrap(),
        Answer::Events(first)
    );
    c.call(Command::Cancel(a.clone())).unwrap();
    let Answer::Terminal(t) = c.call(Command::Terminal(a.subscription)).unwrap() else { panic!() };
    assert_eq!(t.value.unwrap().termination.outcome, Outcome::Cancelled);
    assert!(!f.effects.lock().unwrap().iter().any(|(_, e)| matches!(e, Effect::Kill { .. })));
    f.at(201);
    let Answer::Terminal(t) = c.call(Command::Terminal(b.subscription.clone())).unwrap() else {
        panic!()
    };
    assert_eq!(t.value.as_ref().unwrap().termination.outcome, Outcome::TimedOut);
    assert_eq!(
        terminal_action(&b, &t, &LocalPolicy::Fallback).unwrap(),
        Some(LocalAction::BuildLocally)
    );
}

#[test]
fn no_worker_never_passes_and_old_attempt_is_fenced_after_reconnect() {
    let f = Fixture::new(true, Config::default());
    let mut c = f.client();
    let b = submit(&mut c, 100_000);
    f.at(15_001); // no worker heartbeat => real NoLead termination, not fabricated success.
    c.reconnect(&f.path).unwrap();
    let Answer::Terminal(t) = c.call(Command::Terminal(b.subscription.clone())).unwrap() else {
        panic!()
    };
    assert_eq!(t.value.as_ref().unwrap().termination.outcome, Outcome::Lost);
    assert_eq!(
        terminal_action(&b, &t, &LocalPolicy::Fallback).unwrap(),
        Some(LocalAction::BuildLocally)
    );
    let f = Fixture::new(false, Config::default());
    let mut c = f.client();
    let b = submit(&mut c, 10);
    f.at(11);
    let Answer::Terminal(t) = c.call(Command::Terminal(b.subscription)).unwrap() else { panic!() };
    assert_ne!(t.value.unwrap().termination.outcome, Outcome::Passed);
}

#[test]
fn real_cli_hello_and_lifetime_retention_are_bounded() {
    let f = Fixture::new(false, Config { session_ms: 10, ..Config::default() });
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_blaze-dist-local"))
        .arg("hello")
        .arg(&f.path)
        .output()
        .unwrap();
    assert!(output.status.success());
    let hex = String::from_utf8(output.stdout).unwrap();
    assert!(!hex.trim().is_empty());
    let mut c = f.client();
    f.at(11);
    assert!(c.reconnect(&f.path).is_err());
    let mut stream = raw(&f);
    stream.write_all(&frame(&encode(&ClientFrame::Hello(hello(c.welcome.session, None))))).unwrap();
    assert!(matches!(read(&mut stream), ServerFrame::Error(_)));
}

#[test]
fn actual_process_stop_callback_precedes_fenced_acknowledgment() {
    let child = std::process::Command::new("/bin/sleep").arg("30").spawn().unwrap();
    let process = Arc::new(Mutex::new(Some(child)));
    let stopped = Arc::new(AtomicBool::new(false));
    let process_callback = process.clone();
    let stopped_callback = stopped.clone();
    let f = Fixture::with_callback(true, Config::default(), move |effects| {
        for (_, effect) in effects {
            if let Effect::Kill { .. } = effect
                && let Some(mut child) = process_callback.lock().unwrap().take()
            {
                child.kill().unwrap();
                let status = child.wait().unwrap();
                assert!(!status.success());
                stopped_callback.store(true, Ordering::Release);
            }
        }
    });
    let mut c = f.client();
    let b = submit(&mut c, 1000);
    let ack = KillAck {
        content: b.subscription.content,
        executor: f.executor.clone(),
        execution: b.subscription.execution,
        attempt: b.attempt,
        alive: true,
    };
    assert_eq!(
        c.call(Command::KillAck(ack.clone())).unwrap(),
        Answer::KillAck(KillAnswer { accepted: false })
    );
    assert!(!stopped.load(Ordering::Acquire));
    c.call(Command::Cancel(b)).unwrap();
    assert!(stopped.load(Ordering::Acquire));
    assert_eq!(
        c.call(Command::KillAck(ack.clone())).unwrap(),
        Answer::KillAck(KillAnswer { accepted: true })
    );
    assert_eq!(
        c.call(Command::KillAck(ack)).unwrap(),
        Answer::KillAck(KillAnswer { accepted: false })
    );
    assert!(process.lock().unwrap().is_none());
}

#[test]
fn bounded_retention_and_replay_identity_fail_closed() {
    let f = Fixture::new(true, Config { terminal_ms: 10, max_subscribers: 1, ..Config::default() });
    let mut c = f.client();
    let b = submit(&mut c, 1000);
    assert!(matches!(
        c.call(Command::Subscribe(Subscribe {
            content: b.subscription.content,
            execution: b.subscription.execution,
            deadline_ms: 1000,
            policy: LocalPolicy::Never,
        }))
        .unwrap(),
        Answer::Error(_)
    ));
    c.call(Command::Cancel(b.clone())).unwrap();
    f.at(11);
    // Retention cannot remove an unacknowledged assignment or release its reservation.
    assert!(matches!(
        c.call(Command::Terminal(b.subscription.clone())).unwrap(),
        Answer::Terminal(_)
    ));
    c.call(Command::KillAck(KillAck {
        content: b.subscription.content,
        executor: f.executor.clone(),
        execution: b.subscription.execution,
        attempt: b.attempt,
        alive: true,
    }))
    .unwrap();
    f.at(22); // Retention starts when the reactor observes terminal, not at request decode.
    assert!(matches!(c.call(Command::Terminal(b.subscription)).unwrap(), Answer::Error(_)));

    let f = Fixture::new(false, Config { max_requests: 1, ..Config::default() });
    let mut c = f.client();
    let b = submit(&mut c, 1000);
    assert!(c.call(Command::Resume(b.subscription)).is_err());
    let mut stream = raw(&f);
    let w = hello_raw(&f, &mut stream, [7; 32], None);
    let mut r = Request {
        epoch: w.epoch,
        session: w.session,
        sequence: 1,
        command: Command::Submit(submission(&job(1000), need())),
    };
    stream.write_all(&frame(&encode(&ClientFrame::Request(r.clone())))).unwrap();
    let first = read(&mut stream);
    stream.write_all(&frame(&encode(&ClientFrame::Request(r.clone())))).unwrap();
    assert_eq!(read(&mut stream), first);
    r.command = Command::Submit(submission(&job(2000), need()));
    stream.write_all(&frame(&encode(&ClientFrame::Request(r)))).unwrap();
    assert!(matches!(read(&mut stream), ServerFrame::Error(_)));
}

#[test]
fn late_attempt_work_after_reconnect_is_refused_and_typed_terminal_is_bound() {
    let mut admission = Admission::new(AdmissionConfig::default());
    admission.add_machine("machine", [1; 32], Reservation { memory: 16, cpus: 4, disk: 16 });
    let old = admission.add_builder(0, "machine", need()).unwrap();
    let replacement = admission.add_builder(0, "machine", need()).unwrap();
    let jobs = Jobs::new(admission, ExecutionConfig::default());
    let clock = Arc::new(TestClock::default());
    let stop = Arc::new(AtomicBool::new(false));
    let path = PathBuf::from(format!(
        "/tmp/blaze-late-{}.sock",
        random_id().unwrap()[0..8].iter().map(|b| format!("{b:02x}")).collect::<String>()
    ));
    let server = Server::bind(&path, jobs, Config::default(), clock.clone(), |_| {}).unwrap();
    let signal = stop.clone();
    let thread = std::thread::spawn(move || server.run(signal).unwrap());
    let mut c = Client::connect(&path, random_id().unwrap()).unwrap();
    let b = submit(&mut c, 100_000);
    clock.0.store(15_001, Ordering::Release);
    std::thread::sleep(Duration::from_millis(10));
    c.reconnect(&path).unwrap();
    let Answer::Binding(current) = c.call(Command::Resume(b.subscription.clone())).unwrap() else {
        panic!()
    };
    assert_eq!(current.attempt, Attempt(2));
    assert!(matches!(
        c.call(Command::Lease(Lease {
            content: b.subscription.content,
            executor: old,
            request: WorkLeaseRequest { execution: b.subscription.execution, attempt: b.attempt },
        }))
        .unwrap(),
        Answer::Error(_)
    ));
    assert!(matches!(
        c.call(Command::WorkerEvents(WorkerEvents {
            content: current.subscription.content,
            executor: replacement,
            request: WorkersEventsRequest {
                execution: current.subscription.execution,
                attempt: current.attempt,
                events: Vec::new()
            },
        }))
        .unwrap(),
        Answer::WorkerEvents(_)
    ));
    c.call(Command::Cancel(current)).unwrap();
    stop.store(true, Ordering::Release);
    thread.join().unwrap();
    assert!(!path.exists());
}

#[test]
fn production_cli_client_transmits_cbor_submit_over_the_socket() {
    let f = Fixture::new(false, Config::default());
    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_blaze-dist-local"))
        .arg("client")
        .arg(&f.path)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let body = encode(&Command::Submit(submission(&job(1000), need())));
    let line: String = body.iter().map(|b| format!("{b:02x}")).collect();
    writeln!(child.stdin.take().unwrap(), "{line}").unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success());
    let output = String::from_utf8(output.stdout).unwrap();
    let response = output.lines().nth(1).unwrap();
    let bytes: Vec<u8> = response
        .as_bytes()
        .chunks_exact(2)
        .map(|p| u8::from_str_radix(std::str::from_utf8(p).unwrap(), 16).unwrap())
        .collect();
    assert!(matches!(decode::<Answer>(&bytes, &Limits::DOCUMENTED).unwrap(), Answer::Binding(_)));
}

#[test]
fn coalesced_requests_and_partial_disconnect_do_not_duplicate_effects() {
    let f = Fixture::new(true, Config::default());
    let mut stream = raw(&f);
    let w = hello_raw(&f, &mut stream, [8; 32], None);
    let r = Request {
        epoch: w.epoch,
        session: w.session,
        sequence: 1,
        command: Command::Submit(submission(&job(1000), need())),
    };
    let mut bytes = frame(&encode(&ClientFrame::Request(r.clone())));
    bytes.extend(bytes.clone());
    stream.write_all(&bytes).unwrap();
    assert_eq!(read(&mut stream), read(&mut stream));
    assert_eq!(
        f.effects
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, e)| matches!(
                e, Effect::Event(i) if matches!(i.event, JobEvent::Assigned(_))
            ))
            .count(),
        1
    );
    let r = Request {
        epoch: w.epoch,
        session: w.session,
        sequence: 2,
        command: Command::Submit(submission(&job(2000), need())),
    };
    let bytes = frame(&encode(&ClientFrame::Request(r.clone())));
    stream.write_all(&bytes[..bytes.len() / 2]).unwrap();
    drop(stream);
    let mut stream = raw(&f);
    let resumed = hello_raw(&f, &mut stream, w.session, Some(w.epoch));
    assert_eq!(resumed.next_request, 2);
    stream.write_all(&bytes).unwrap();
    assert!(matches!(read(&mut stream), ServerFrame::Response(_)));
}

#[test]
fn daemon_epoch_and_out_of_order_ids_cannot_create_a_new_transition() {
    let f = Fixture::new(false, Config::default());
    let mut stream = raw(&f);
    let w = hello_raw(&f, &mut stream, [9; 32], None);
    let r = Request {
        epoch: w.epoch,
        session: w.session,
        sequence: 2,
        command: Command::Submit(submission(&job(1000), need())),
    };
    stream.write_all(&frame(&encode(&ClientFrame::Request(r)))).unwrap();
    assert!(matches!(read(&mut stream), ServerFrame::Error(_)));
    let g = Fixture::new(false, Config::default());
    let mut stream = raw(&g);
    stream
        .write_all(&frame(&encode(&ClientFrame::Hello(hello(w.session, Some(w.epoch))))))
        .unwrap();
    assert!(matches!(read(&mut stream), ServerFrame::Error(_)));
}

#[test]
fn hello_retry_is_idempotent_and_clock_rollback_does_not_extend_lifetime() {
    let f = Fixture::new(false, Config::default());
    let mut stream = raw(&f);
    let first = hello_raw(&f, &mut stream, [10; 32], None);
    drop(stream);
    let mut stream = raw(&f);
    let second = hello_raw(&f, &mut stream, [10; 32], None);
    assert_eq!(first.epoch, second.epoch);
    assert_eq!(first.expires_ms, second.expires_ms);
    f.at(100);
    f.at(1);
    drop(stream);
    let mut stream = raw(&f);
    let resumed = hello_raw(&f, &mut stream, [10; 32], Some(first.epoch));
    assert!(resumed.now_ms >= 100);
    assert_eq!(resumed.expires_ms, first.expires_ms);
}

#[test]
fn production_cli_server_owns_and_removes_its_actual_socket() {
    let path = PathBuf::from(format!(
        "/tmp/blaze-cli-{}.sock",
        random_id().unwrap()[..8].iter().map(|b| format!("{b:02x}")).collect::<String>()
    ));
    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_blaze-dist-local"))
        .arg("serve")
        .arg(&path)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let mut ready = String::new();
    std::io::BufRead::read_line(
        &mut std::io::BufReader::new(child.stdout.take().unwrap()),
        &mut ready,
    )
    .unwrap();
    assert!(ready.starts_with("ready "));
    let mut c = Client::connect(&path, random_id().unwrap()).unwrap();
    let deadline = c.welcome.now_ms + 1000;
    let b = submit(&mut c, deadline);
    let Answer::Terminal(t) = c.call(Command::Terminal(b.subscription)).unwrap() else { panic!() };
    assert!(t.value.is_none()); // CLI has no holder; it cannot claim Passed.
    writeln!(child.stdin.take().unwrap(), "stop").unwrap();
    assert!(child.wait().unwrap().success());
    assert!(!path.exists());
}
