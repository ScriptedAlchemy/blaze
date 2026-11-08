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
use blaze_dist::jobs::{Effect, ExecutionConfig, Jobs, LocalAction, local_action};
use blaze_dist_local::client::Client;
use blaze_dist_local::coordinator::Config;
use blaze_dist_local::protocol::*;
use blaze_dist_local::server::{Clock, Server, frame, random_id};
use blaze_wire::{
    Attempt, BatchResult, DoneEvent, ErrorCode, ExecutionId, ItemOutcome, ItemResult, JobEvent,
    JobRequest, Limits, LocalPolicy, Outcome, Reservation, RootSet, Set, Stage, StageEvent, Why,
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
    executors: Vec<String>,
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
        let executors = if executor.is_empty() { Vec::new() } else { vec![executor.clone()] };
        Self { path, stop, clock, effects, thread: Some(thread), executor, executors }
    }

    /// `builders` executors on one machine under a caller-chosen execution config, for
    /// the two-worker tests: the first admitted builder leads, the rest are offered
    /// as helpers by the registry itself.
    fn with_workers(builders: usize, exec: ExecutionConfig) -> Self {
        let id = random_id().unwrap();
        let tag: String = id[..8].iter().map(|b| format!("{b:02x}")).collect();
        let path = PathBuf::from(format!("/tmp/blaze-dist-{tag}.sock"));
        let mut admission = Admission::new(AdmissionConfig::default());
        admission.add_machine("machine", [1; 32], Reservation { memory: 64, cpus: 16, disk: 64 });
        let executors: Vec<String> =
            (0..builders).map(|_| admission.add_builder(0, "machine", need()).unwrap()).collect();
        let jobs = Jobs::new(admission, exec);
        let clock = Arc::new(TestClock::default());
        let stop = Arc::new(AtomicBool::new(false));
        let effects = Arc::new(Mutex::new(Vec::new()));
        let delivered = effects.clone();
        let server = Server::bind(&path, jobs, Config::default(), clock.clone(), move |batch| {
            delivered.lock().unwrap().extend(batch);
        })
        .unwrap();
        let signal = stop.clone();
        let thread = std::thread::spawn(move || server.run(signal).unwrap());
        Self {
            path,
            stop,
            clock,
            effects,
            thread: Some(thread),
            executor: executors.first().cloned().unwrap_or_default(),
            executors,
        }
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
        if let Some(thread) = self.thread.take() {
            thread.join().unwrap();
        }
        assert!(!self.path.exists());
    }
}

fn need() -> Reservation {
    Reservation { memory: 4, cpus: 1, disk: 4 }
}

fn job(deadline_ms: u64) -> JobRequest {
    JobRequest {
        unit: [9; 32],
        platform: [1; 32],
        namespace: blaze_wire::Namespace::Main,
        source: [0; 32],
        deadline_ms,
        local: LocalPolicy::Fallback,
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

fn hello_raw(stream: &mut UnixStream, value: LocalHello) -> LocalWelcome {
    stream.write_all(&frame(&encode(&ClientFrame::Hello(value)))).unwrap();
    let ServerFrame::Welcome(w) = read(stream) else {
        panic!("hello");
    };
    w
}

/// Create a lifetime with a client-chosen nonce and return its welcome.
fn create_raw(f: &Fixture, stream: &mut UnixStream, nonce: blaze_wire::Digest) -> LocalWelcome {
    let _ = f;
    hello_raw(stream, hello(nonce))
}

/// Resume the lifetime a previous welcome named.
fn resume_raw(stream: &mut UnixStream, welcome: &LocalWelcome) -> LocalWelcome {
    hello_raw(stream, resume(welcome.session, welcome.epoch))
}

#[test]
fn fragmented_coalesced_and_malformed_frames_are_real_socket_boundaries() {
    let f = Fixture::new(false, Config::default());
    let mut stream = raw(&f);
    let bytes = frame(&encode(&ClientFrame::Hello(hello([1; 32]))));
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
    let h = hello([2; 32]);
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
    let mut h = hello([3; 32]);
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
    assert!(matches!(c.call(Command::Terminal(wrong)).unwrap(), Answer::Error(_)));
}

#[test]
fn disconnect_retry_has_identical_response_and_no_duplicate_submit_or_multistage_events() {
    let f = Fixture::new(true, Config::default());
    let mut stream = raw(&f);
    let w = create_raw(&f, &mut stream, [4; 32]);
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
    resume_raw(&mut stream, &w);
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
    let termination = &t.value.as_ref().unwrap().termination;
    assert_eq!(local_action(&LocalPolicy::Fallback, termination).unwrap(), LocalAction::Take);
    assert_eq!(t.value.as_ref().unwrap().termination.outcome, Outcome::Passed);
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
        local_action(&LocalPolicy::Fallback, &t.value.as_ref().unwrap().termination).unwrap(),
        LocalAction::BuildLocally
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
        local_action(&LocalPolicy::Fallback, &t.value.as_ref().unwrap().termination).unwrap(),
        LocalAction::BuildLocally
    );
    let f = Fixture::new(false, Config::default());
    let mut c = f.client();
    let b = submit(&mut c, 10);
    f.at(11);
    let Answer::Terminal(t) = c.call(Command::Terminal(b.subscription)).unwrap() else { panic!() };
    assert_ne!(t.value.unwrap().termination.outcome, Outcome::Passed);
}

#[test]
fn released_lifetimes_exceed_the_live_cap_without_restart_or_capacity_loss() {
    // The reported defect: a client that mints a lifetime per call consumed the default
    // 32-session cap forever. Forty-five clients against one daemon, each releasing its
    // lifetime, must all succeed, and the daemon must stay bounded.
    let f = Fixture::new(false, Config::default());
    let mut sessions = Vec::new();
    for _ in 0..45 {
        let mut client = Client::connect(&f.path, random_id().unwrap()).unwrap();
        assert!(client.welcome.created);
        assert_eq!(client.welcome.transport, TRANSPORT_VERSION);
        sessions.push(client.welcome.session);
        client.release().unwrap();
    }
    // Issued labels are distinct: no call was answered with another lifetime's identity.
    let mut unique = sessions.clone();
    unique.sort_unstable();
    unique.dedup();
    assert_eq!(unique.len(), sessions.len());
    // The cap is still enforced for genuinely concurrent live sessions. A small dedicated
    // daemon keeps this assertion independent of connection slots the CLI runs used.
    let cap = Fixture::new(false, Config { max_sessions: 4, ..Config::default() });
    let mut held: Vec<Client> =
        (0..4u8).map(|nonce| Client::connect(&cap.path, [nonce; 32]).unwrap()).collect();
    let mut overflow = raw(&cap);
    overflow.write_all(&frame(&encode(&ClientFrame::Hello(hello([99; 32]))))).unwrap();
    let ServerFrame::Error(refused) = read(&mut overflow) else { panic!("cap not enforced") };
    assert_eq!(refused.code(), Some(ErrorCode::Busy));
    // Releasing one live lifetime returns exactly one slot, without a restart.
    held.pop().unwrap().release().unwrap();
    let mut stream = raw(&cap);
    let w = create_raw(&cap, &mut stream, [98; 32]);
    assert!(w.created);
    drop(held);
}

#[test]
fn released_and_expired_lifetimes_never_revive_an_old_sequence() {
    let f = Fixture::new(true, Config::default());
    let mut c = f.client();
    let first = c.welcome.clone();
    let b = submit(&mut c, 100_000);
    // A deliberate release ends the lifetime; its label is not resumable afterwards.
    c.release().unwrap();
    let mut stream = raw(&f);
    stream
        .write_all(&frame(&encode(&ClientFrame::Hello(resume(first.session, first.epoch)))))
        .unwrap();
    assert!(matches!(read(&mut stream), ServerFrame::Error(_)));
    // The old session's exact request bytes are refused, not replayed into a new lifetime.
    let mut stream = raw(&f);
    let fresh = create_raw(&f, &mut stream, [123; 32]);
    assert_ne!(fresh.session, first.session);
    let stale = Request {
        epoch: first.epoch,
        session: first.session,
        sequence: 1,
        command: Command::Submit(submission(&job(1000), need())),
    };
    stream.write_all(&frame(&encode(&ClientFrame::Request(stale)))).unwrap();
    assert!(matches!(read(&mut stream), ServerFrame::Error(_)));
    // No second Submit happened: exactly one Assigned event was ever delivered. The count
    // is taken out of the guard so a failed assertion cannot poison the fixture's mutex.
    let assigned = f
        .effects
        .lock()
        .unwrap()
        .iter()
        .filter(|(_, e)| matches!(e, Effect::Event(i) if matches!(i.event, JobEvent::Assigned(_))))
        .count();
    assert_eq!(assigned, 1);
    let _ = b;
}

#[test]
fn transport_two_hello_shape_is_strict_and_legacy_peers_are_refused() {
    let f = Fixture::new(false, Config::default());
    // Creation names neither epoch nor resume; resume names both. Half a resume is refused.
    let mut stream = raw(&f);
    let mut half = hello([5; 32]);
    half.epoch = Some([1; 32]);
    stream.write_all(&frame(&encode(&ClientFrame::Hello(half)))).unwrap();
    assert!(matches!(read(&mut stream), ServerFrame::Error(_)));
    let mut stream = raw(&f);
    let mut half = hello([5; 32]);
    half.resume = Some([1; 32]);
    stream.write_all(&frame(&encode(&ClientFrame::Hello(half)))).unwrap();
    assert!(matches!(read(&mut stream), ServerFrame::Error(_)));
    // An older transport-1 peer is refused by version, not reinterpreted as transport 2.
    let mut stream = raw(&f);
    let mut legacy = hello([5; 32]);
    legacy.transport = 1;
    stream.write_all(&frame(&encode(&ClientFrame::Hello(legacy)))).unwrap();
    let ServerFrame::Error(error) = read(&mut stream) else { panic!() };
    assert!(error.message.contains("version-mismatch"));
}

#[test]
fn expired_lifetime_creation_retry_is_not_mistaken_for_the_old_one() {
    let f = Fixture::new(false, Config { session_ms: 10, ..Config::default() });
    let mut stream = raw(&f);
    let first = create_raw(&f, &mut stream, [42; 32]);
    assert!(first.created);
    drop(stream);
    // Retrying the same nonce while live is idempotent and creates nothing.
    let mut stream = raw(&f);
    let again = create_raw(&f, &mut stream, [42; 32]);
    assert_eq!(again.session, first.session);
    assert!(!again.created);
    f.at(11);
    // After expiry the same nonce is a new lifetime, and the old label is not revived.
    let mut stream = raw(&f);
    let fresh = create_raw(&f, &mut stream, [42; 32]);
    assert!(fresh.created);
    assert_ne!(fresh.session, first.session);
    let mut stream = raw(&f);
    stream
        .write_all(&frame(&encode(&ClientFrame::Hello(resume(first.session, first.epoch)))))
        .unwrap();
    assert!(matches!(read(&mut stream), ServerFrame::Error(_)));
}

#[test]
fn lifetime_retention_is_bounded() {
    let f = Fixture::new(false, Config { session_ms: 10, ..Config::default() });
    let mut c = f.client();
    let expired = c.welcome.clone();
    f.at(11);
    // An expired lifetime is refused, and the label it named is never issued again.
    assert!(c.reconnect(&f.path).is_err());
    let mut stream = raw(&f);
    stream
        .write_all(&frame(&encode(&ClientFrame::Hello(resume(expired.session, expired.epoch)))))
        .unwrap();
    assert!(matches!(read(&mut stream), ServerFrame::Error(_)));
    // A fresh creation after expiry is ordinary use, not capacity exhaustion.
    let mut fresh = raw(&f);
    let created = create_raw(&f, &mut fresh, [77; 32]);
    assert!(created.created);
    assert_ne!(created.session, expired.session);
}

#[test]
fn actual_process_stop_callback_precedes_fenced_acknowledgment() {
    let child = std::process::Command::new("sleep").arg("30").spawn().unwrap();
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

    // A retry of the last request is answered again with the same bytes, and a sequence reused
    // with other bytes is refused.
    let f = Fixture::new(false, Config::default());
    let mut stream = raw(&f);
    let w = create_raw(&f, &mut stream, [7; 32]);
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
fn a_full_execution_pool_can_still_join_an_existing_job_within_its_subscriber_limit() {
    let f =
        Fixture::new(false, Config { max_executions: 1, max_subscribers: 2, ..Config::default() });
    let mut c = f.client();
    let first = submit(&mut c, 1000);
    let second = submit(&mut c, 2000);
    assert_eq!(first.subscription.execution, second.subscription.execution);
    assert_ne!(first.subscription.subscriber, second.subscription.subscriber);
    let Answer::Error(error) = c.call(Command::Submit(submission(&job(3000), need()))).unwrap()
    else {
        panic!("a third subscriber exceeded its execution's cap");
    };
    assert!(error.message.contains("subscriber-retention-capacity"));
    let mut other = job(1000);
    other.unit = [10; 32];
    let Answer::Error(error) = c.call(Command::Submit(submission(&other, need()))).unwrap() else {
        panic!("a different job exceeded the execution cap");
    };
    assert!(error.message.contains("execution-retention-capacity"));
}

#[test]
fn retained_terminals_do_not_spend_a_new_executions_subscriber_capacity() {
    let f =
        Fixture::new(false, Config { max_executions: 2, max_subscribers: 1, ..Config::default() });
    let mut c = f.client();
    let first = submit(&mut c, 1000);
    assert!(matches!(c.call(Command::Cancel(first.clone())).unwrap(), Answer::Ack(_)));
    let second = submit(&mut c, 2000);
    assert_ne!(first.subscription.execution, second.subscription.execution);
    let Answer::Terminal(terminal) = c.call(Command::Terminal(first.subscription)).unwrap() else {
        panic!()
    };
    assert_eq!(terminal.value.unwrap().termination.outcome, Outcome::Cancelled);
    assert!(matches!(c.call(Command::Cancel(second)).unwrap(), Answer::Ack(_)));
    let Answer::Error(error) = c.call(Command::Submit(submission(&job(3000), need()))).unwrap()
    else {
        panic!("terminal executions still count toward execution retention");
    };
    assert!(error.message.contains("execution-retention-capacity"));
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
fn coalesced_requests_and_partial_disconnect_do_not_duplicate_effects() {
    let f = Fixture::new(true, Config::default());
    let mut stream = raw(&f);
    let w = create_raw(&f, &mut stream, [8; 32]);
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
    let resumed = resume_raw(&mut stream, &w);
    assert_eq!(resumed.next_request, 2);
    stream.write_all(&bytes).unwrap();
    assert!(matches!(read(&mut stream), ServerFrame::Response(_)));
}

#[test]
fn daemon_epoch_and_out_of_order_ids_cannot_create_a_new_transition() {
    let f = Fixture::new(false, Config::default());
    let mut stream = raw(&f);
    let w = create_raw(&f, &mut stream, [9; 32]);
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
    stream.write_all(&frame(&encode(&ClientFrame::Hello(resume(w.session, w.epoch))))).unwrap();
    assert!(matches!(read(&mut stream), ServerFrame::Error(_)));
}

#[test]
fn hello_retry_is_idempotent_and_clock_rollback_does_not_extend_lifetime() {
    let f = Fixture::new(false, Config::default());
    let mut stream = raw(&f);
    let first = create_raw(&f, &mut stream, [10; 32]);
    drop(stream);
    let mut stream = raw(&f);
    let second = create_raw(&f, &mut stream, [10; 32]);
    assert_eq!(first.epoch, second.epoch);
    assert_eq!(first.expires_ms, second.expires_ms);
    f.at(100);
    f.at(1);
    drop(stream);
    let mut stream = raw(&f);
    let resumed = resume_raw(&mut stream, &first);
    assert!(resumed.now_ms >= 100);
    assert_eq!(resumed.expires_ms, first.expires_ms);
}

#[test]
fn the_coordinator_owns_and_removes_its_actual_socket() {
    let path = PathBuf::from(format!(
        "/tmp/blaze-cli-{}.sock",
        random_id().unwrap()[..8].iter().map(|b| format!("{b:02x}")).collect::<String>()
    ));
    let executable = env!("CARGO_BIN_EXE_blaze-dist-local");
    let mut child = std::process::Command::new(executable)
        .arg("coordinate")
        .arg(&path)
        .arg("some-triple")
        .arg("0")
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
    assert!(t.value.is_none()); // No executor: nothing can claim Passed.
    writeln!(child.stdin.take().unwrap(), "stop").unwrap();
    assert!(child.wait().unwrap().success());
    assert!(!path.exists());
}

fn two_roots() -> RootSet {
    RootSet { spec: [1; 32], roots: [2; 32], count: 2, index: [3; 32], inputs: [4; 32] }
}

fn join_as(c: &mut Client, content: blaze_wire::JobContent, b: &Binding, who: &str) {
    let answer = c
        .call(Command::Join(Join {
            content,
            executor: who.to_owned(),
            request: WorkJoinRequest {
                execution: b.subscription.execution,
                attempt: b.attempt,
                roots: two_roots(),
                rejected: Set::new(Vec::new()),
            },
        }))
        .unwrap();
    assert!(matches!(answer, Answer::Join(_)), "{who} could not join: {answer:?}");
}

fn lease_as(
    c: &mut Client,
    content: blaze_wire::JobContent,
    b: &Binding,
    who: &str,
) -> blaze_wire::Grant {
    let Answer::Lease(answer) = c
        .call(Command::Lease(Lease {
            content,
            executor: who.to_owned(),
            request: WorkLeaseRequest { execution: b.subscription.execution, attempt: b.attempt },
        }))
        .unwrap()
    else {
        panic!("{who} got no lease answer")
    };
    answer.grant.unwrap_or_else(|| panic!("{who} got an empty grant"))
}

fn result_as(
    c: &mut Client,
    content: blaze_wire::JobContent,
    who: &str,
    grant: &blaze_wire::Grant,
    tag: u8,
) -> Answer {
    c.call(Command::Result(BatchSubmission {
        content,
        executor: who.to_owned(),
        request: BatchResult {
            lease: grant.lease.clone(),
            seq: 1,
            items: vec![ItemResult {
                name: WorkName([tag; 32]),
                outcome: ItemOutcome::Local(Why { why: "fixture".to_owned() }),
                found: Set::new(Vec::new()),
                unnamed: Set::new(Vec::new()),
            }],
            packs: Vec::new(),
            returned_roots: Set::new(Vec::new()),
            returned_names: Set::new(Vec::new()),
        },
    }))
    .unwrap()
}

fn heartbeat_as(c: &mut Client, content: blaze_wire::JobContent, b: &Binding, who: &str) {
    let answer = c
        .call(Command::WorkerEvents(WorkerEvents {
            content,
            executor: who.to_owned(),
            request: WorkersEventsRequest {
                execution: b.subscription.execution,
                attempt: b.attempt,
                events: Vec::new(),
            },
        }))
        .unwrap();
    assert!(matches!(answer, Answer::WorkerEvents(_)), "{answer:?}");
}

fn done_passed(c: &mut Client, content: blaze_wire::JobContent, b: &Binding, lead: &str) {
    let answer = c
        .call(Command::WorkerEvents(WorkerEvents {
            content,
            executor: lead.to_owned(),
            request: WorkersEventsRequest {
                execution: b.subscription.execution,
                attempt: b.attempt,
                events: vec![JobEvent::Done(DoneEvent {
                    outcome: Outcome::Passed,
                    counters: Vec::new(),
                })],
            },
        }))
        .unwrap();
    assert!(matches!(answer, Answer::WorkerEvents(_)), "{answer:?}");
    let Answer::Terminal(t) = c.call(Command::Terminal(b.subscription.clone())).unwrap() else {
        panic!("no terminal")
    };
    assert_eq!(t.value.as_ref().unwrap().termination.outcome, Outcome::Passed);
}

/// Two builders, one build, over the real socket: the registry offers the second
/// builder as a helper, both join the attempt, their grants are disjoint, both
/// commit, and the job passes. Until the registry assigned helpers, the helper's
/// join was refused `not-holder` and no build could split.
#[test]
fn two_workers_split_one_build_over_sockets() {
    let f = Fixture::with_workers(
        2,
        ExecutionConfig {
            work: blaze_dist::work::WorkConfig { grant_roots: 1, ..Default::default() },
            ..Default::default()
        },
    );
    let (lead, helper) = (f.executors[0].clone(), f.executors[1].clone());
    let mut c = f.client();
    let b = submit(&mut c, 1_000_000);
    let content = b.subscription.content;
    join_as(&mut c, content, &b, &lead);
    join_as(&mut c, content, &b, &helper);
    let first = lease_as(&mut c, content, &b, &lead);
    let second = lease_as(&mut c, content, &b, &helper);
    assert_eq!(first.roots.iter().copied().collect::<Vec<_>>(), vec![(0, 1)]);
    assert_eq!(second.roots.iter().copied().collect::<Vec<_>>(), vec![(1, 2)]);
    assert!(matches!(result_as(&mut c, content, &lead, &first, 9), Answer::Result(_)));
    assert!(matches!(result_as(&mut c, content, &helper, &second, 10), Answer::Result(_)));
    done_passed(&mut c, content, &b, &lead);

    let effects = f.effects.lock().unwrap();
    assert!(
        effects.iter().any(|(_, effect)| matches!(
            effect,
            Effect::HelperAssigned { executor, .. } if executor == &helper
        )),
        "no helper assignment was delivered: {effects:?}"
    );
    let committed: Vec<&str> = effects
        .iter()
        .filter_map(|(_, effect)| match effect {
            Effect::Work(blaze_dist::work::Effect::Committed { by, .. }) => {
                Some(by.holder.as_str())
            }
            _ => None,
        })
        .collect();
    assert!(committed.contains(&lead.as_str()), "{committed:?}");
    assert!(committed.contains(&helper.as_str()), "{committed:?}");
}

/// A helper that takes a grant and goes silent is fenced by the clock: its root is
/// re-granted to the lead at a higher generation, its late result is refused
/// `dead-lease` whole, and the build still passes on the survivor alone.
#[test]
fn a_silent_helpers_lease_is_fenced_and_its_root_regranted() {
    let f = Fixture::with_workers(
        2,
        ExecutionConfig {
            work: blaze_dist::work::WorkConfig {
                grant_roots: 1,
                lease_ms: 20_000,
                progress_first_ms: 3_600_000,
                ..Default::default()
            },
            ..Default::default()
        },
    );
    let (lead, helper) = (f.executors[0].clone(), f.executors[1].clone());
    let mut c = f.client();
    let b = submit(&mut c, 1_000_000);
    let content = b.subscription.content;
    join_as(&mut c, content, &b, &lead);
    join_as(&mut c, content, &b, &helper);
    let stale = lease_as(&mut c, content, &b, &helper);
    assert_eq!(stale.roots.iter().copied().collect::<Vec<_>>(), vec![(0, 1)]);

    // The helper never speaks again. The lead heartbeats inside its own 15 s
    // lead-loss window while the clock passes the helper's 20 s lease.
    for at in [10_000, 20_000, 25_000] {
        f.at(at);
        heartbeat_as(&mut c, content, &b, &lead);
    }
    let regranted = lease_as(&mut c, content, &b, &lead);
    assert_eq!(regranted.roots.iter().copied().collect::<Vec<_>>(), vec![(0, 1)]);
    assert!(regranted.lease.generation > stale.lease.generation, "{regranted:?} vs {stale:?}");

    let late = result_as(&mut c, content, &helper, &stale, 10);
    let Answer::Error(error) = late else { panic!("a fenced result was accepted: {late:?}") };
    assert!(error.message.contains("dead-lease"), "{error:?}");

    assert!(matches!(result_as(&mut c, content, &lead, &regranted, 9), Answer::Result(_)));
    let rest = lease_as(&mut c, content, &b, &lead);
    assert_eq!(rest.roots.iter().copied().collect::<Vec<_>>(), vec![(1, 2)]);
    // The lead's second batch of the attempt carries its next sequence number.
    let second = BatchSubmission {
        content,
        executor: lead.clone(),
        request: BatchResult {
            lease: rest.lease.clone(),
            seq: 2,
            items: vec![ItemResult {
                name: WorkName([11; 32]),
                outcome: ItemOutcome::Local(Why { why: "fixture".to_owned() }),
                found: Set::new(Vec::new()),
                unnamed: Set::new(Vec::new()),
            }],
            packs: Vec::new(),
            returned_roots: Set::new(Vec::new()),
            returned_names: Set::new(Vec::new()),
        },
    };
    assert!(matches!(c.call(Command::Result(second)).unwrap(), Answer::Result(_)));
    done_passed(&mut c, content, &b, &lead);
}

/// The acceptance gate intercepts a real Holder-produced result, after the real server
/// issued its lease. Removing the expiry lets that exact carrier commit; adding expiry
/// and a replacement commit makes that same carrier receive the real typed DeadLease.
#[test]
fn the_real_driver_submits_a_gated_result_with_valid_and_expired_lease_controls() {
    use blaze_dist::holder::{BatchCompiler, Compiled, Holder, HolderConfig, Plan};
    use blaze_dist_local::{
        driver::{DriverConfig, run_holder_until},
        result_gate::ResultGate,
    };
    use blaze_wire::{EntryKey, EntryRef};

    struct Compiler;
    impl BatchCompiler for Compiler {
        fn compile(&mut self, plan: &Plan) -> Result<Compiled, String> {
            let ranges = blaze_dist::Ranges::from_wire(&plan.grant.roots).unwrap();
            Ok(Compiled {
                generation: plan.grant.lease.generation,
                roots: ranges
                    .indexes()
                    .map(|index| ItemResult {
                        name: WorkName([index as u8 + 9; 32]),
                        outcome: ItemOutcome::Exported(EntryRef {
                            entry: EntryKey([index as u8 + 20; 32]),
                        }),
                        found: Set::new(Vec::new()),
                        unnamed: Set::new(Vec::new()),
                    })
                    .collect(),
                names: Vec::new(),
                packs: Vec::new(),
                returned_roots: blaze_dist::Ranges::new(),
                returned_names: Vec::new(),
            })
        }
        fn release(&mut self, _: &[WorkName]) {}
    }

    for expired in [false, true] {
        let f = Fixture::with_workers(
            2,
            ExecutionConfig {
                heartbeat_ms: 1_000_000,
                work: blaze_dist::work::WorkConfig {
                    grant_roots: 1,
                    lease_ms: 20,
                    progress_first_ms: 1_000_000,
                    ..Default::default()
                },
                ..Default::default()
            },
        );
        let (lead, helper) = (f.executors[0].clone(), f.executors[1].clone());
        let mut control = f.client();
        let binding = submit(&mut control, 100_000);
        let content = binding.subscription.content;
        join_as(&mut control, content, &binding, &lead);
        let directory = tempfile::tempdir().unwrap();
        let gate_path = directory.path().to_owned();
        let config = DriverConfig {
            result_gate: Some(ResultGate::new(gate_path.clone(), Duration::from_secs(3)).unwrap()),
            ..Default::default()
        };
        let leave = AtomicBool::new(false);
        std::thread::scope(|scope| {
            let mut client = f.client();
            let mut holder = Holder::new(
                &helper,
                binding.subscription.execution,
                binding.attempt,
                two_roots(),
                Set::new(Vec::new()),
                Default::default(),
                HolderConfig { heartbeat_ms: 5, retry_ms: 100, idle_ms: 5 },
            );
            let clock = f.clock.clone();
            let (socket, executor, config, leave) = (&f.path, &helper, &config, &leave);
            let worker = scope.spawn(move || {
                run_holder_until(
                    &mut client,
                    socket,
                    &mut holder,
                    content,
                    executor,
                    config,
                    || clock.now_ms(),
                    &mut Compiler,
                    leave,
                )
            });
            let deadline = std::time::Instant::now() + Duration::from_secs(2);
            while !gate_path.join("ready.json").is_file() {
                assert!(
                    std::time::Instant::now() < deadline,
                    "the real Holder never produced its gated result"
                );
                std::thread::sleep(Duration::from_millis(1));
            }
            let bytes = std::fs::read(gate_path.join("result.cbor")).unwrap();
            let Command::Result(captured) = decode::<Command>(&bytes, &Limits::DOCUMENTED).unwrap()
            else {
                panic!("capture was not Client::Result")
            };
            assert_eq!(captured.executor, helper);
            assert_eq!(captured.request.lease.holder, helper);
            assert_eq!(captured.request.lease.execution, binding.subscription.execution);
            assert!(f.effects.lock().unwrap().iter().any(|(_, effect)| matches!(effect, Effect::Work(blaze_dist::work::Effect::Granted { holder, generation, .. }) if holder == &helper && *generation == captured.request.lease.generation)), "capture did not name an actually issued lease");
            if expired {
                f.at(21);
                // The lead stayed assigned, but its work-set membership also expired.
                join_as(&mut control, content, &binding, &lead);
                let replacement = lease_as(&mut control, content, &binding, &lead);
                assert!(replacement.lease.generation > captured.request.lease.generation);
                let mut result = captured.clone();
                result.executor = lead.clone();
                result.request.lease = replacement.lease;
                assert!(matches!(
                    control.call(Command::Result(result)).unwrap(),
                    Answer::Result(_)
                ));
            }
            // Complete the other root before releasing the held one. The positive result
            // now has no next batch to compile; the stale result still must be submitted.
            let other = lease_as(&mut control, content, &binding, &lead);
            let mut result = captured.clone();
            result.executor = lead.clone();
            result.request.lease = other.lease;
            result.request.seq = if expired { 2 } else { 1 };
            result.request.items[0].name = WorkName([10; 32]);
            assert!(matches!(control.call(Command::Result(result)).unwrap(), Answer::Result(_)));
            leave.store(true, Ordering::Release);
            std::fs::write(gate_path.join("release"), b"").unwrap();
            worker.join().unwrap().unwrap();
            let answer = decode::<Answer>(
                &std::fs::read(gate_path.join("answer.cbor")).unwrap(),
                &Limits::DOCUMENTED,
            )
            .unwrap();
            if expired {
                let Answer::Error(error) = answer else {
                    panic!("a stale result was accepted: {answer:?}")
                };
                assert_eq!(blaze_dist::Refusal::of(&error), Some(blaze_dist::Refusal::DeadLease));
            } else {
                assert!(matches!(answer, Answer::Result(_)), "{answer:?}");
            }
            let committed = f.effects.lock().unwrap().iter().any(|(_, effect)| matches!(effect, Effect::Work(blaze_dist::work::Effect::Committed { by, .. }) if by.holder == helper && by.generation == captured.request.lease.generation && by.seq == captured.request.seq));
            assert_eq!(committed, !expired, "only the unexpired captured identity may commit");
            assert_eq!(
                std::fs::read(gate_path.join("result.cbor")).unwrap(),
                bytes,
                "the gate never rewrites the real carrier"
            );
        });
    }
}
