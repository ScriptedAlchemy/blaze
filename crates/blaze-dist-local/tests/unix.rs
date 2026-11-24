//! Real Unix sockets and exact production coordinator/client code, not an in-memory network.

#![no_std]

use core::convert::Infallible;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use core::time::Duration;

use blaze_dist::Refusal;
use blaze_dist::admission::{Admission, AdmissionConfig};
use blaze_dist::jobs::{Effect, ExecutionConfig, Jobs, LocalAction, local_action};
use blaze_dist_local::client::Client;
use blaze_dist_local::coordinator::Config;
use blaze_dist_local::protocol::*;
use blaze_dist_local::server::{Clock, Server, frame, random_id};
use blaze_dist_local::sys::{
    Child, JoinHandle, Mutex, SystemClock, TempDir, UnixStream, path_exists, path_is_file,
    read_file, scope, sleep_ms, spawn, write_file,
};
use blaze_own::{Arc, BTreeSet, Vec, format_in};
use blaze_wire::{
    Ack, Attempt, BatchResult, DoneEvent, ErrorCode, ExecutionId, ItemOutcome, ItemResult, JobEvent,
    JobRequest, Limits, LocalPolicy, Outcome, Reservation, RootSet, Set, Stage, StageEvent, Why,
    WireAlloc, WireString, WireVec, WorkJoinRequest, WorkLeaseRequest, WorkName,
    WorkersEventsRequest, decode, encode,
};

fn test_alloc() -> WireAlloc {
    blaze_own::testing::heap().shared()
}

#[derive(Default)]
struct TestClock(AtomicU64);
impl Clock for TestClock {
    fn now_ms(&self) -> u64 {
        self.0.load(Ordering::Acquire)
    }
}

struct Fixture {
    path: WireString,
    stop: Arc<AtomicBool, WireAlloc>,
    clock: Arc<TestClock, WireAlloc>,
    effects: Arc<Mutex<WireVec<(ExecutionId, Effect)>>, WireAlloc>,
    thread: Option<JoinHandle<'static, ()>>,
    executor: WireString,
    executors: WireVec<WireString>,
    _dir: TempDir,
    alloc: WireAlloc,
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
        let alloc = test_alloc();
        let dir = TempDir::new("bd", alloc).unwrap();
        let path = dir.join("d.sock");
        let mut admission = Admission::new(AdmissionConfig::default(), alloc);
        let executor = if builder {
            admission.add_machine(
                "machine",
                [1; 32],
                Reservation { memory: 16, cpus: 4, disk: 16 },
            );
            let added = admission.add_builder(0, "machine", need(), alloc).unwrap();
            WireString::from_str_in(&added, alloc)
        } else {
            WireString::new_in(alloc)
        };
        let jobs = Jobs::new(admission, ExecutionConfig::default(), alloc);
        let clock = Arc::new_in(TestClock::default(), alloc);
        let stop = Arc::new_in(AtomicBool::new(false), alloc);
        let effects = Arc::new_in(Mutex::new(Vec::new_in(alloc)), alloc);
        let delivered = effects.clone();
        let server = Server::bind(
            path.as_str(),
            jobs,
            config,
            clock.clone(),
            move |batch: &mut dyn Iterator<Item = (ExecutionId, Effect)>| {
                let mut owned = Vec::new_in(alloc);
                owned.extend(batch);
                callback(&owned);
                delivered.lock().extend(owned);
            },
            alloc,
        )
        .unwrap();
        let signal = stop.clone();
        let thread = spawn(move || server.run(&signal).unwrap(), alloc).unwrap();
        let mut executors = Vec::new_in(alloc);
        if !executor.is_empty() {
            executors.push(executor.clone());
        }
        Self {
            path,
            stop,
            clock,
            effects,
            thread: Some(thread),
            executor,
            executors,
            _dir: dir,
            alloc,
        }
    }

    /// `builders` executors on one machine under a caller-chosen execution config, for
    /// the two-worker tests: the first admitted builder leads, the rest are offered
    /// as helpers by the registry itself.
    fn with_workers(builders: usize, exec: ExecutionConfig) -> Self {
        let alloc = test_alloc();
        let dir = TempDir::new("bd", alloc).unwrap();
        let path = dir.join("w.sock");
        let mut admission = Admission::new(AdmissionConfig::default(), alloc);
        admission.add_machine("machine", [1; 32], Reservation { memory: 64, cpus: 16, disk: 64 });
        let mut executors = Vec::with_capacity_in(builders, alloc);
        for _ in 0..builders {
            let added = admission.add_builder(0, "machine", need(), alloc).unwrap();
            executors.push(WireString::from_str_in(&added, alloc));
        }
        let jobs = Jobs::new(admission, exec, alloc);
        let clock = Arc::new_in(TestClock::default(), alloc);
        let stop = Arc::new_in(AtomicBool::new(false), alloc);
        let effects = Arc::new_in(Mutex::new(Vec::new_in(alloc)), alloc);
        let delivered = effects.clone();
        let server = Server::bind(
            path.as_str(),
            jobs,
            Config::default(),
            clock.clone(),
            move |batch: &mut dyn Iterator<Item = (ExecutionId, Effect)>| {
                delivered.lock().extend(batch);
            },
            alloc,
        )
        .unwrap();
        let signal = stop.clone();
        let thread = spawn(move || server.run(&signal).unwrap(), alloc).unwrap();
        let executor = executors.first().cloned().unwrap_or_else(|| WireString::new_in(alloc));
        Self {
            path,
            stop,
            clock,
            effects,
            thread: Some(thread),
            executor,
            executors,
            _dir: dir,
            alloc,
        }
    }

    fn client(&self) -> Client {
        Client::connect(self.path.as_str(), random_id().unwrap(), self.alloc).unwrap()
    }

    fn at(&self, now: u64) {
        self.clock.0.store(now, Ordering::Release);
        sleep_ms(10);
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            thread.join().unwrap();
        }
        assert!(!path_exists(self.path.as_str()));
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

fn submit(client: &mut Client, deadline: u64, alloc: WireAlloc) -> Binding {
    let Answer::Binding(binding) =
        client.call(Command::Submit(submission(&job(deadline), need(), alloc))).unwrap()
    else {
        panic!("submit failed");
    };
    binding
}

fn read(stream: &mut UnixStream, alloc: WireAlloc) -> ServerFrame {
    decode(&read_body(stream, alloc), &Limits::DOCUMENTED, alloc).unwrap()
}

fn read_body(stream: &mut UnixStream, alloc: WireAlloc) -> WireVec<u8> {
    let mut h = [0u8; 4];
    stream.read_exact(&mut h).unwrap();
    let n = u32::from_be_bytes(h);
    assert!(n < 1 << 20);
    let len = usize::try_from(n).unwrap();
    let mut body = Vec::with_capacity_in(len, alloc);
    body.resize(len, 0);
    stream.read_exact(&mut body).unwrap();
    body
}

fn raw(f: &Fixture) -> UnixStream {
    let stream = UnixStream::connect(f.path.as_str()).unwrap();
    stream.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
    stream
}

fn hello_raw(stream: &mut UnixStream, value: LocalHello, alloc: WireAlloc) -> LocalWelcome {
    stream.write_all(&frame(&encode(&ClientFrame::Hello(value), alloc), alloc)).unwrap();
    let ServerFrame::Welcome(w) = read(stream, alloc) else {
        panic!("hello");
    };
    w
}

/// Create a lifetime with a client-chosen nonce and return its welcome.
fn create_raw(f: &Fixture, stream: &mut UnixStream, nonce: blaze_wire::Digest) -> LocalWelcome {
    hello_raw(stream, hello(nonce), f.alloc)
}

/// Resume the lifetime a previous welcome named.
fn resume_raw(
    stream: &mut UnixStream,
    welcome: &LocalWelcome,
    alloc: WireAlloc,
) -> LocalWelcome {
    hello_raw(stream, resume(welcome.session, welcome.epoch), alloc)
}

fn request_raw(
    stream: &mut UnixStream,
    welcome: &LocalWelcome,
    sequence: u64,
    command: Command,
    alloc: WireAlloc,
) -> Response {
    let request = Request { epoch: welcome.epoch, session: welcome.session, sequence, command };
    stream.write_all(&frame(&encode(&ClientFrame::Request(request), alloc), alloc)).unwrap();
    let ServerFrame::Response(response) = read(stream, alloc) else {
        panic!("request {sequence} had no bound response");
    };
    assert_eq!(response.epoch, welcome.epoch);
    assert_eq!(response.session, welcome.session);
    assert_eq!(response.sequence, sequence);
    response
}

#[test]
fn fragmented_coalesced_and_malformed_frames_are_real_socket_boundaries() {
    let f = Fixture::new(false, Config::default());
    let alloc = f.alloc;
    let mut stream = raw(&f);
    let bytes = frame(&encode(&ClientFrame::Hello(hello([1; 32])), alloc), alloc);
    for byte in bytes {
        stream.write_all(&[byte]).unwrap();
    }
    assert!(matches!(read(&mut stream, alloc), ServerFrame::Welcome(_)));
    let mut bad = raw(&f);
    bad.write_all(&[0, 16, 0, 1]).unwrap(); // oversize before any allocation/body.
    assert!(matches!(read(&mut bad, alloc), ServerFrame::Error(_)));
    let mut bad = raw(&f);
    bad.write_all(&frame(&[0xff], alloc)).unwrap();
    assert!(matches!(read(&mut bad, alloc), ServerFrame::Error(_)));
    let mut bad = raw(&f);
    bad.write_all(&[0, 0, 0, 0]).unwrap();
    assert!(matches!(read(&mut bad, alloc), ServerFrame::Error(_)));
    let mut coalesced = raw(&f);
    let h = hello([2; 32]);
    let mut bytes = frame(&encode(&ClientFrame::Hello(h.clone()), alloc), alloc);
    bytes.extend_from_slice(&frame(&encode(&ClientFrame::Hello(h), alloc), alloc));
    coalesced.write_all(&bytes).unwrap();
    assert!(matches!(read(&mut coalesced, alloc), ServerFrame::Welcome(_)));
    assert!(matches!(read(&mut coalesced, alloc), ServerFrame::Error(_)));
}

#[test]
fn versions_content_session_and_terminal_bindings_fail_closed() {
    let f = Fixture::new(true, Config::default());
    let alloc = f.alloc;
    let mut stream = raw(&f);
    let mut h = hello([3; 32]);
    h.codec += 1;
    stream.write_all(&frame(&encode(&ClientFrame::Hello(h), alloc), alloc)).unwrap();
    assert!(matches!(read(&mut stream, alloc), ServerFrame::Error(_)));
    let mut c = f.client();
    let mut s = submission(&job(1000), need(), alloc);
    s.content = blaze_wire::JobContent([0; 32]);
    assert!(matches!(c.call(Command::Submit(s)).unwrap(), Answer::Error(_)));
    let b = submit(&mut c, 1000, alloc);
    let mut wrong = b.subscription.clone();
    wrong.subscriber += 1;
    assert!(matches!(c.call(Command::Terminal(wrong)).unwrap(), Answer::Error(_)));
}

#[test]
fn disconnect_retry_has_identical_response_and_no_duplicate_submit_or_multistage_events() {
    let f = Fixture::new(true, Config::default());
    let alloc = f.alloc;
    let mut stream = raw(&f);
    let w = create_raw(&f, &mut stream, [4; 32]);
    let req = Request {
        epoch: w.epoch,
        session: w.session,
        sequence: 1,
        command: Command::Submit(submission(&job(1000), need(), alloc)),
    };
    stream.write_all(&frame(&encode(&ClientFrame::Request(req.clone()), alloc), alloc)).unwrap();
    // Submit can commit before its response is lost. The retry key survives the socket.
    drop(stream);
    sleep_ms(20);
    let mut stream = raw(&f);
    resume_raw(&mut stream, &w, alloc);
    let bytes = frame(&encode(&ClientFrame::Request(req.clone()), alloc), alloc);
    stream.write_all(&bytes).unwrap();
    let first = read(&mut stream, alloc);
    stream.write_all(&bytes).unwrap();
    assert_eq!(read(&mut stream, alloc), first);
    let ServerFrame::Response(Response { answer: Answer::Binding(b), .. }) = first else {
        panic!()
    };
    let mut events = Vec::new_in(alloc);
    events.push(JobEvent::Stage(StageEvent { stage: Stage::Fetched, at_ms: u64::MAX }));
    events.push(JobEvent::Stage(StageEvent { stage: Stage::Loaded, at_ms: u64::MAX }));
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
                events,
            },
        }),
    };
    let bytes = frame(&encode(&ClientFrame::Request(progress), alloc), alloc);
    stream.write_all(&bytes).unwrap();
    let first = read(&mut stream, alloc);
    stream.write_all(&bytes).unwrap();
    assert_eq!(read(&mut stream, alloc), first);
    let effects = f.effects.lock();
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
    let alloc = f.alloc;
    let mut c = f.client();
    let b = submit(&mut c, 1000, alloc);
    let content = b.subscription.content;
    let execution = b.subscription.execution;
    let mut done_events = Vec::new_in(alloc);
    done_events
        .push(JobEvent::Done(DoneEvent { outcome: Outcome::Passed, counters: Vec::new_in(alloc) }));
    let passed = WorkerEvents {
        content,
        executor: f.executor.clone(),
        request: WorkersEventsRequest { execution, attempt: b.attempt, events: done_events },
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
            rejected: Set::new(Vec::new_in(alloc)),
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
    let mut items = Vec::new_in(alloc);
    items.push(ItemResult {
        name: WorkName([9; 32]),
        outcome: ItemOutcome::Local(Why {
            why: WireString::from_str_in("not exportable", alloc),
        }),
        found: Set::new(Vec::new_in(alloc)),
        unnamed: Set::new(Vec::new_in(alloc)),
    });
    assert!(matches!(
        c.call(Command::Result(BatchSubmission {
            content,
            executor: f.executor.clone(),
            request: BatchResult {
                lease: grant.lease.clone(),
                seq: 1,
                items,
                packs: Vec::new_in(alloc),
                returned_roots: Set::new(Vec::new_in(alloc)),
                returned_names: Set::new(Vec::new_in(alloc)),
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
    assert_eq!(local_action(&LocalPolicy::Fallback, termination, alloc).unwrap(), LocalAction::Take);
    assert_eq!(t.value.as_ref().unwrap().termination.outcome, Outcome::Passed);
    let assignment = f
        .effects
        .lock()
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
    let alloc = f.alloc;
    let mut c = f.client();
    let a = submit(&mut c, 100, alloc);
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
    c.reconnect(f.path.as_str()).unwrap();
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
    assert!(!f.effects.lock().iter().any(|(_, e)| matches!(e, Effect::Kill { .. })));
    f.at(201);
    let Answer::Terminal(t) = c.call(Command::Terminal(b.subscription.clone())).unwrap() else {
        panic!()
    };
    assert_eq!(t.value.as_ref().unwrap().termination.outcome, Outcome::TimedOut);
    assert_eq!(
        local_action(&LocalPolicy::Fallback, &t.value.as_ref().unwrap().termination, alloc)
            .unwrap(),
        LocalAction::BuildLocally
    );
}

#[test]
fn no_worker_never_passes_and_old_attempt_is_fenced_after_reconnect() {
    let f = Fixture::new(true, Config::default());
    let alloc = f.alloc;
    let mut c = f.client();
    let b = submit(&mut c, 100_000, alloc);
    f.at(15_001); // no worker heartbeat => real NoLead termination, not fabricated success.
    c.reconnect(f.path.as_str()).unwrap();
    let Answer::Terminal(t) = c.call(Command::Terminal(b.subscription.clone())).unwrap() else {
        panic!()
    };
    assert_eq!(t.value.as_ref().unwrap().termination.outcome, Outcome::Lost);
    assert_eq!(
        local_action(&LocalPolicy::Fallback, &t.value.as_ref().unwrap().termination, alloc)
            .unwrap(),
        LocalAction::BuildLocally
    );
    let f = Fixture::new(false, Config::default());
    let mut c = f.client();
    let b = submit(&mut c, 10, alloc);
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
    let alloc = f.alloc;
    let mut sessions = Vec::new_in(alloc);
    for _ in 0..45 {
        let mut client = Client::connect(f.path.as_str(), random_id().unwrap(), alloc).unwrap();
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
    let mut held = Vec::with_capacity_in(4, alloc);
    for nonce in 0..4u8 {
        held.push(Client::connect(cap.path.as_str(), [nonce; 32], alloc).unwrap());
    }
    let mut overflow = raw(&cap);
    overflow
        .write_all(&frame(&encode(&ClientFrame::Hello(hello([99; 32])), alloc), alloc))
        .unwrap();
    let ServerFrame::Error(refused) = read(&mut overflow, alloc) else {
        panic!("cap not enforced")
    };
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
    let alloc = f.alloc;
    let mut c = f.client();
    let first = c.welcome.clone();
    let b = submit(&mut c, 100_000, alloc);
    // A deliberate release ends the lifetime; its label is not resumable afterwards.
    c.release().unwrap();
    let mut stream = raw(&f);
    stream
        .write_all(&frame(
            &encode(&ClientFrame::Hello(resume(first.session, first.epoch)), alloc),
            alloc,
        ))
        .unwrap();
    assert!(matches!(read(&mut stream, alloc), ServerFrame::Error(_)));
    // The old session's exact request bytes are refused, not replayed into a new lifetime.
    let mut stream = raw(&f);
    let fresh = create_raw(&f, &mut stream, [123; 32]);
    assert_ne!(fresh.session, first.session);
    let stale = Request {
        epoch: first.epoch,
        session: first.session,
        sequence: 1,
        command: Command::Submit(submission(&job(1000), need(), alloc)),
    };
    stream.write_all(&frame(&encode(&ClientFrame::Request(stale), alloc), alloc)).unwrap();
    assert!(matches!(read(&mut stream, alloc), ServerFrame::Error(_)));
    // No second Submit happened: exactly one Assigned event was ever delivered.
    let assigned = f
        .effects
        .lock()
        .iter()
        .filter(|(_, e)| matches!(e, Effect::Event(i) if matches!(i.event, JobEvent::Assigned(_))))
        .count();
    assert_eq!(assigned, 1);
    let _ = b;
}

#[test]
fn hello_shape_is_strict_and_pre_retirement_peers_are_refused() {
    let f = Fixture::new(false, Config::default());
    let alloc = f.alloc;
    // Creation names neither epoch nor resume; resume names both. Half a resume is refused.
    let mut stream = raw(&f);
    let mut half = hello([5; 32]);
    half.epoch = Some([1; 32]);
    stream.write_all(&frame(&encode(&ClientFrame::Hello(half), alloc), alloc)).unwrap();
    assert!(matches!(read(&mut stream, alloc), ServerFrame::Error(_)));
    let mut stream = raw(&f);
    let mut half = hello([5; 32]);
    half.resume = Some([1; 32]);
    stream.write_all(&frame(&encode(&ClientFrame::Hello(half), alloc), alloc)).unwrap();
    assert!(matches!(read(&mut stream, alloc), ServerFrame::Error(_)));
    // The original transport and the last version before owned retirement cannot
    // negotiate this version's assignment ownership semantics.
    for version in [1, 4] {
        let mut stream = raw(&f);
        let mut legacy = hello([5; 32]);
        legacy.transport = version;
        stream.write_all(&frame(&encode(&ClientFrame::Hello(legacy), alloc), alloc)).unwrap();
        let ServerFrame::Error(error) = read(&mut stream, alloc) else { panic!() };
        assert!(error.message.contains("version-mismatch"));
    }
}

#[test]
fn expired_lifetime_creation_retry_is_not_mistaken_for_the_old_one() {
    let f = Fixture::new(false, Config { session_ms: 10, ..Config::default() });
    let alloc = f.alloc;
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
        .write_all(&frame(
            &encode(&ClientFrame::Hello(resume(first.session, first.epoch)), alloc),
            alloc,
        ))
        .unwrap();
    assert!(matches!(read(&mut stream, alloc), ServerFrame::Error(_)));
}

#[test]
fn lifetime_retention_is_bounded() {
    let f = Fixture::new(false, Config { session_ms: 10, ..Config::default() });
    let alloc = f.alloc;
    let mut c = f.client();
    let expired = c.welcome.clone();
    f.at(11);
    // An expired lifetime is refused, and the label it named is never issued again.
    assert!(c.reconnect(f.path.as_str()).is_err());
    let mut stream = raw(&f);
    stream
        .write_all(&frame(
            &encode(&ClientFrame::Hello(resume(expired.session, expired.epoch)), alloc),
            alloc,
        ))
        .unwrap();
    assert!(matches!(read(&mut stream, alloc), ServerFrame::Error(_)));
    // A fresh creation after expiry is ordinary use, not capacity exhaustion.
    let mut fresh = raw(&f);
    let created = create_raw(&f, &mut fresh, [77; 32]);
    assert!(created.created);
    assert_ne!(created.session, expired.session);
}

fn short_lifetimes() -> Config {
    Config {
        session_ms: 10,
        max_execution_ms: 100,
        owner_cleanup_ms: 20,
        idle_ms: 5,
        ..Config::default()
    }
}

fn carrier_refusal(answer: Answer, code: ErrorCode, message: &str) {
    let Answer::Error(error) = answer else { panic!("expected {message}, got {answer:?}") };
    assert_eq!(error.code(), Some(code));
    assert!(error.message.contains(message), "{error:?}");
}

fn expired_resume(f: &Fixture, welcome: &LocalWelcome) {
    let alloc = f.alloc;
    let mut stream = raw(f);
    stream
        .write_all(&frame(
            &encode(&ClientFrame::Hello(resume(welcome.session, welcome.epoch)), alloc),
            alloc,
        ))
        .unwrap();
    let ServerFrame::Error(error) = read(&mut stream, alloc) else {
        panic!("an expired session resumed");
    };
    assert_eq!(error.code(), Some(ErrorCode::NotFound));
    assert!(error.message.contains("session-expired"), "{error:?}");
}

#[test]
fn owner_cleanup_survives_idle_close_and_extended_deadline_but_not_final_expiry() {
    for cleanup_at in [60, 90, 101] {
        let f = Fixture::new(true, short_lifetimes());
        let alloc = f.alloc;
        let mut requester = f.client();
        let binding = submit(&mut requester, 30, alloc);
        // This session only claims: it has neither submitted nor subscribed itself.
        let mut owner = Client::connect(f.path.as_str(), [101; 32], alloc).unwrap();
        let original = owner.welcome.clone();
        assert_eq!(original.expires_ms, 10);
        let claimed = claim_as(&mut owner, &binding, [102; 32]);
        let retired = retirement_of(&binding, &claimed, [102; 32]);
        let lookup = lookup_of(&retired);

        f.at(11);
        let mut longer = f.client();
        assert!(matches!(
            longer
                .call(Command::Subscribe(Subscribe {
                    content: binding.subscription.content,
                    execution: binding.subscription.execution,
                    deadline_ms: 80,
                    policy: LocalPolicy::Never,
                }))
                .unwrap(),
            Answer::Binding(_)
        ));

        // Both ordinary expiry (10) and the first work allowance (30 + 20) passed.
        // Only the later subscriber's extension can preserve the original owner.
        f.at(cleanup_at);
        if cleanup_at == 101 {
            let error = owner.call(Command::OwnedAssignment(lookup.clone())).unwrap_err();
            let rendered = format_in!(alloc, "{error}");
            assert!(rendered.contains("session-expired"), "{error:?}");
            assert_eq!(owner.welcome.session, original.session);
            expired_resume(&f, &original);
            // Reusing even the creation nonce and claimant label cannot recreate authority.
            let mut fresh = Client::connect(f.path.as_str(), [101; 32], alloc).unwrap();
            assert_ne!(fresh.welcome.session, original.session);
            lookup_refused(&mut fresh, &lookup, Refusal::NotHolder);
            let Answer::Error(error) = fresh.call(Command::RetireAssignment(retired)).unwrap()
            else {
                panic!("a fresh session retired the expired owner's assignment");
            };
            assert_eq!(Refusal::of(&error), Some(Refusal::NotHolder));
            continue;
        }

        assert_eq!(owned_attempt(&mut owner, &lookup), claimed.attempt);
        assert_eq!(owner.welcome.session, original.session);
        assert_eq!(owner.welcome.epoch, original.epoch);
        assert_eq!(owner.welcome.now_ms, cleanup_at);
        assert_eq!(owner.welcome.expires_ms, 100);
        assert!(!owner.welcome.created);
        let effects = f.effects.lock().clone();
        let mut foreign = f.client();
        lookup_refused(&mut foreign, &lookup, Refusal::NotHolder);
        let Answer::Error(error) =
            foreign.call(Command::RetireAssignment(retired.clone())).unwrap()
        else {
            panic!("a foreign session retired the live owner's assignment");
        };
        assert_eq!(Refusal::of(&error), Some(Refusal::NotHolder));
        if cleanup_at < 80 {
            assert!(claim_as(&mut foreign, &binding, retired.claimant).executor.is_none());
        }
        assert_eq!(*f.effects.lock(), effects);

        // At 90 the job has timed out; its exact pending stop still belongs to this
        // session until cleanup finishes. Neither deadline expiry nor reconnect is an ACK.
        assert!(matches!(
            owner.call(Command::RetireAssignment(retired.clone())).unwrap(),
            Answer::Ack(_)
        ));
        assert_eq!(owned_attempt(&mut owner, &lookup), claimed.attempt);
        assert_eq!(
            owner.call(Command::KillAck(stop_ack_of(&retired))).unwrap(),
            Answer::KillAck(KillAnswer { accepted: true })
        );
        lookup_refused(&mut owner, &lookup, Refusal::NotHolder);
    }
}

#[test]
fn queued_requesters_keep_exact_reply_bytes_until_their_real_final_expiry() {
    let f = Fixture::new(false, short_lifetimes());
    let alloc = f.alloc;
    let mut stream = raw(&f);
    let original = create_raw(&f, &mut stream, [111; 32]);
    assert_eq!(original.expires_ms, 10);
    let request = Request {
        epoch: original.epoch,
        session: original.session,
        sequence: 1,
        command: Command::Submit(submission(&job(30), need(), alloc)),
    };
    let request_bytes = frame(&encode(&ClientFrame::Request(request.clone()), alloc), alloc);
    stream.write_all(&request_bytes).unwrap();
    let first_reply = read_body(&mut stream, alloc);
    let ServerFrame::Response(response) =
        decode(&first_reply, &Limits::DOCUMENTED, alloc).unwrap()
    else {
        panic!("queued submit had no response");
    };
    let Answer::Binding(binding) = response.answer else { panic!("queued submit failed") };

    f.at(11);
    let mut subscriber = f.client();
    let subscriber_session = subscriber.welcome.session;
    assert!(matches!(
        subscriber
            .call(Command::Subscribe(Subscribe {
                content: binding.subscription.content,
                execution: binding.subscription.execution,
                deadline_ms: 80,
                policy: LocalPolicy::Never,
            }))
            .unwrap(),
        Answer::Binding(_)
    ));
    // No executor exists and neither requester has claimed: their accepted interests
    // alone retain the two sessions, including the first session's unchanged journal.
    f.at(60);
    let mut resumed = raw(&f);
    let welcome = resume_raw(&mut resumed, &original, alloc);
    assert_eq!(welcome.session, original.session);
    assert_eq!(welcome.expires_ms, 100);
    assert_eq!(welcome.next_request, 2);
    let effects = f.effects.lock().clone();
    resumed.write_all(&request_bytes).unwrap();
    assert_eq!(read_body(&mut resumed, alloc), first_reply);
    let mut changed = request.clone();
    changed.command = Command::Submit(submission(&job(31), need(), alloc));
    resumed.write_all(&frame(&encode(&ClientFrame::Request(changed), alloc), alloc)).unwrap();
    let ServerFrame::Error(error) = read(&mut resumed, alloc) else {
        panic!("changed replay accepted")
    };
    assert!(error.message.contains("sequence-reused"), "{error:?}");
    assert_eq!(*f.effects.lock(), effects);
    subscriber.reconnect(f.path.as_str()).unwrap();
    assert_eq!(subscriber.welcome.session, subscriber_session);
    assert_eq!(subscriber.welcome.expires_ms, 100);

    // Resuming and replaying at the inclusive bound must not slide it forward.
    f.at(100);
    let mut last = raw(&f);
    assert_eq!(resume_raw(&mut last, &original, alloc).expires_ms, 100);
    last.write_all(&request_bytes).unwrap();
    assert_eq!(read_body(&mut last, alloc), first_reply);
    f.at(101);
    expired_resume(&f, &original);
    assert!(subscriber.reconnect(f.path.as_str()).is_err());
    let mut fresh = raw(&f);
    let fresh_welcome = create_raw(&f, &mut fresh, [111; 32]);
    assert_ne!(fresh_welcome.session, original.session);
    let effects = f.effects.lock().clone();
    fresh.write_all(&request_bytes).unwrap();
    assert!(matches!(read(&mut fresh, alloc), ServerFrame::Error(_)));
    assert_eq!(*f.effects.lock(), effects);
}

#[test]
fn execution_deadline_ceiling_refuses_submit_and_subscribe_without_mutation() {
    let f = Fixture::new(false, short_lifetimes());
    let alloc = f.alloc;
    let mut client = f.client();
    let binding = submit(&mut client, 30, alloc);
    f.at(11);
    let mut fresh = job(112);
    fresh.unit = [121; 32];
    let effects = f.effects.lock().clone();
    // A new execution would start at 11 and end by 111. Coalescing and subscribing
    // must still use the existing execution's first Queued time and limit of 100.
    for command in [
        Command::Submit(submission(&fresh, need(), alloc)),
        Command::Submit(submission(&job(101), need(), alloc)),
        Command::Subscribe(Subscribe {
            content: binding.subscription.content,
            execution: binding.subscription.execution,
            deadline_ms: 101,
            policy: LocalPolicy::Never,
        }),
    ] {
        carrier_refusal(
            client.call(command).unwrap(),
            ErrorCode::Malformed,
            "execution-deadline-limit",
        );
    }
    assert_eq!(*f.effects.lock(), effects);
    client.reconnect(f.path.as_str()).unwrap();
    // Refusals neither create work nor extend the accepted deadline's cleanup bound.
    assert_eq!(client.welcome.expires_ms, 50);
    let welcome = client.welcome.clone();
    f.at(51);
    expired_resume(&f, &welcome);
}

#[test]
fn an_older_session_cannot_bind_later_work_beyond_its_immutable_lifetime_ceiling() {
    let f = Fixture::new(false, Config { max_subscribers: 2, ..short_lifetimes() });
    let alloc = f.alloc;
    let mut old = f.client();
    submit(&mut old, 100, alloc);
    let original = old.welcome.clone();
    f.at(20);
    let mut current = f.client();
    let mut later_job = job(70);
    later_job.unit = [122; 32];
    let Answer::Binding(later) =
        current.call(Command::Submit(submission(&later_job, need(), alloc))).unwrap()
    else {
        panic!("later job was refused for a fresh session");
    };
    let later_subscribe = Subscribe {
        content: later.subscription.content,
        execution: later.subscription.execution,
        deadline_ms: 70,
        policy: LocalPolicy::Never,
    };
    let effects = f.effects.lock().clone();
    // The old session's immutable ceiling is 0 + 10 + 100 + 20 = 130.
    // This later execution can require 20 + 100 + 20 = 140, so admission refuses
    // even though its currently requested deadline of 70 would fit.
    for command in [
        Command::Submit(submission(&later_job, need(), alloc)),
        Command::Subscribe(later_subscribe.clone()),
        Command::Resume(later.subscription.clone()),
    ] {
        carrier_refusal(
            old.call(command).unwrap(),
            ErrorCode::Conflict,
            "session-work-lifetime-limit",
        );
    }
    assert_eq!(old.welcome.session, original.session);
    assert_eq!(old.welcome.expires_ms, 120);
    assert_eq!(*f.effects.lock(), effects);
    // The rejected requests did not consume the one remaining interest slot.
    assert!(matches!(
        current.call(Command::Subscribe(later_subscribe.clone())).unwrap(),
        Answer::Binding(_)
    ));
    carrier_refusal(
        current.call(Command::Subscribe(later_subscribe)).unwrap(),
        ErrorCode::Busy,
        "subscriber-retention-capacity",
    );
    f.at(121);
    expired_resume(&f, &original);
}

#[test]
fn holder_watcher_and_tail_traffic_retains_sessions_without_claiming_or_subscribing() {
    let f = Fixture::new(true, short_lifetimes());
    let alloc = f.alloc;
    let mut requester = f.client();
    let binding = submit(&mut requester, 30, alloc);
    let content = binding.subscription.content;
    let mut holder = f.client();
    let mut heartbeats = f.client();
    let mut watcher = f.client();
    let mut tail = f.client();
    let originals = [
        holder.welcome.clone(),
        heartbeats.welcome.clone(),
        watcher.welcome.clone(),
        tail.welcome.clone(),
    ];
    join_as(&mut holder, content, &binding, f.executor.as_str(), alloc);
    heartbeat_as(&mut heartbeats, content, &binding, f.executor.as_str(), alloc);
    let packs = Packs { content, execution: binding.subscription.execution };
    assert!(matches!(watcher.call(Command::Packs(packs.clone())).unwrap(), Answer::Packs(_)));
    let events = Events { binding: binding.clone(), from: 0, limit: 256 };
    assert!(matches!(tail.call(Command::Events(events.clone())).unwrap(), Answer::Events(_)));

    f.at(11);
    let mut longer = f.client();
    assert!(matches!(
        longer
            .call(Command::Subscribe(Subscribe {
                content,
                execution: binding.subscription.execution,
                deadline_ms: 80,
                policy: LocalPolicy::Never,
            }))
            .unwrap(),
        Answer::Binding(_)
    ));
    // Each role must survive both ordinary expiry and its first work allowance,
    // then call through an idle-closed socket without replacing its identity.
    f.at(60);
    join_as(&mut holder, content, &binding, f.executor.as_str(), alloc);
    heartbeat_as(&mut heartbeats, content, &binding, f.executor.as_str(), alloc);
    assert!(matches!(watcher.call(Command::Packs(packs)).unwrap(), Answer::Packs(_)));
    assert!(matches!(tail.call(Command::Events(events)).unwrap(), Answer::Events(_)));
    for (client, original) in [&holder, &heartbeats, &watcher, &tail].into_iter().zip(originals) {
        assert_eq!(client.welcome.session, original.session);
        assert_eq!(client.welcome.now_ms, 60);
        assert_eq!(client.welcome.expires_ms, 100);
        assert!(!client.welcome.created);
    }
}

#[test]
fn actual_process_stop_callback_precedes_fenced_acknowledgment() {
    let alloc = test_alloc();
    let sleep_bin = if path_exists("/bin/sleep") { "/bin/sleep" } else { "/usr/bin/sleep" };
    let child = Child::spawn(sleep_bin, &["30"], false, false, alloc).unwrap();
    let process = Arc::new_in(Mutex::new(Some(child)), alloc);
    let stopped = Arc::new_in(AtomicBool::new(false), alloc);
    let process_callback = process.clone();
    let stopped_callback = stopped.clone();
    let f = Fixture::with_callback(true, Config::default(), move |effects| {
        for (_, effect) in effects {
            if let Effect::Kill { .. } = effect
                && let Some(mut child) = process_callback.lock().take()
            {
                child.kill().unwrap();
                let status = child.wait().unwrap();
                assert!(!status.success());
                stopped_callback.store(true, Ordering::Release);
            }
        }
    });
    let mut c = f.client();
    let b = submit(&mut c, 1000, alloc);
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
    assert!(process.lock().is_none());
}

#[test]
fn bounded_retention_and_replay_identity_fail_closed() {
    let f = Fixture::new(true, Config { terminal_ms: 10, max_subscribers: 1, ..Config::default() });
    let alloc = f.alloc;
    let mut c = f.client();
    let b = submit(&mut c, 1000, alloc);
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
        command: Command::Submit(submission(&job(1000), need(), alloc)),
    };
    stream.write_all(&frame(&encode(&ClientFrame::Request(r.clone()), alloc), alloc)).unwrap();
    let first = read(&mut stream, alloc);
    stream.write_all(&frame(&encode(&ClientFrame::Request(r.clone()), alloc), alloc)).unwrap();
    assert_eq!(read(&mut stream, alloc), first);
    r.command = Command::Submit(submission(&job(2000), need(), alloc));
    stream.write_all(&frame(&encode(&ClientFrame::Request(r), alloc), alloc)).unwrap();
    assert!(matches!(read(&mut stream, alloc), ServerFrame::Error(_)));
}

#[test]
fn a_full_execution_pool_can_still_join_an_existing_job_within_its_subscriber_limit() {
    let f =
        Fixture::new(false, Config { max_executions: 1, max_subscribers: 2, ..Config::default() });
    let alloc = f.alloc;
    let mut c = f.client();
    let first = submit(&mut c, 1000, alloc);
    let second = submit(&mut c, 2000, alloc);
    assert_eq!(first.subscription.execution, second.subscription.execution);
    assert_ne!(first.subscription.subscriber, second.subscription.subscriber);
    let Answer::Error(error) =
        c.call(Command::Submit(submission(&job(3000), need(), alloc))).unwrap()
    else {
        panic!("a third subscriber exceeded its execution's cap");
    };
    assert!(error.message.contains("subscriber-retention-capacity"));
    let mut other = job(1000);
    other.unit = [10; 32];
    let Answer::Error(error) = c.call(Command::Submit(submission(&other, need(), alloc))).unwrap()
    else {
        panic!("a different job exceeded the execution cap");
    };
    assert!(error.message.contains("execution-retention-capacity"));
}

#[test]
fn retained_terminals_do_not_spend_a_new_executions_subscriber_capacity() {
    let f =
        Fixture::new(false, Config { max_executions: 2, max_subscribers: 1, ..Config::default() });
    let alloc = f.alloc;
    let mut c = f.client();
    let first = submit(&mut c, 1000, alloc);
    assert!(matches!(c.call(Command::Cancel(first.clone())).unwrap(), Answer::Ack(_)));
    let second = submit(&mut c, 2000, alloc);
    assert_ne!(first.subscription.execution, second.subscription.execution);
    let Answer::Terminal(terminal) = c.call(Command::Terminal(first.subscription)).unwrap() else {
        panic!()
    };
    assert_eq!(terminal.value.unwrap().termination.outcome, Outcome::Cancelled);
    assert!(matches!(c.call(Command::Cancel(second)).unwrap(), Answer::Ack(_)));
    let Answer::Error(error) =
        c.call(Command::Submit(submission(&job(3000), need(), alloc))).unwrap()
    else {
        panic!("terminal executions still count toward execution retention");
    };
    assert!(error.message.contains("execution-retention-capacity"));
}

#[test]
fn late_attempt_work_after_reconnect_is_refused_and_typed_terminal_is_bound() {
    let alloc = test_alloc();
    let dir = TempDir::new("bl", alloc).unwrap();
    let path = dir.join("l.sock");
    let mut admission = Admission::new(AdmissionConfig::default(), alloc);
    admission.add_machine("machine", [1; 32], Reservation { memory: 16, cpus: 4, disk: 16 });
    let old = WireString::from_str_in(
        &admission.add_builder(0, "machine", need(), alloc).unwrap(),
        alloc,
    );
    let replacement = WireString::from_str_in(
        &admission.add_builder(0, "machine", need(), alloc).unwrap(),
        alloc,
    );
    let jobs = Jobs::new(admission, ExecutionConfig::default(), alloc);
    let clock = Arc::new_in(TestClock::default(), alloc);
    let stop = Arc::new_in(AtomicBool::new(false), alloc);
    let server = Server::bind(
        path.as_str(),
        jobs,
        Config::default(),
        clock.clone(),
        |_: &mut dyn Iterator<Item = (ExecutionId, Effect)>| {},
        alloc,
    )
    .unwrap();
    let signal = stop.clone();
    let thread = spawn(move || server.run(&signal).unwrap(), alloc).unwrap();
    let mut c = Client::connect(path.as_str(), random_id().unwrap(), alloc).unwrap();
    let b = submit(&mut c, 100_000, alloc);
    clock.0.store(15_001, Ordering::Release);
    sleep_ms(10);
    c.reconnect(path.as_str()).unwrap();
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
                events: Vec::new_in(alloc),
            },
        }))
        .unwrap(),
        Answer::WorkerEvents(_)
    ));
    c.call(Command::Cancel(current)).unwrap();
    stop.store(true, Ordering::Release);
    thread.join().unwrap();
    assert!(!path_exists(path.as_str()));
}

#[test]
fn coalesced_requests_and_partial_disconnect_do_not_duplicate_effects() {
    let f = Fixture::new(true, Config::default());
    let alloc = f.alloc;
    let mut stream = raw(&f);
    let w = create_raw(&f, &mut stream, [8; 32]);
    let r = Request {
        epoch: w.epoch,
        session: w.session,
        sequence: 1,
        command: Command::Submit(submission(&job(1000), need(), alloc)),
    };
    let mut bytes = frame(&encode(&ClientFrame::Request(r.clone()), alloc), alloc);
    let second = bytes.clone();
    bytes.extend_from_slice(&second);
    stream.write_all(&bytes).unwrap();
    assert_eq!(read(&mut stream, alloc), read(&mut stream, alloc));
    assert_eq!(
        f.effects
            .lock()
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
        command: Command::Submit(submission(&job(2000), need(), alloc)),
    };
    let bytes = frame(&encode(&ClientFrame::Request(r.clone()), alloc), alloc);
    stream.write_all(&bytes[..bytes.len() / 2]).unwrap();
    drop(stream);
    let mut stream = raw(&f);
    let resumed = resume_raw(&mut stream, &w, alloc);
    assert_eq!(resumed.next_request, 2);
    stream.write_all(&bytes).unwrap();
    assert!(matches!(read(&mut stream, alloc), ServerFrame::Response(_)));
}

#[test]
fn daemon_epoch_and_out_of_order_ids_cannot_create_a_new_transition() {
    let f = Fixture::new(false, Config::default());
    let alloc = f.alloc;
    let mut stream = raw(&f);
    let w = create_raw(&f, &mut stream, [9; 32]);
    let r = Request {
        epoch: w.epoch,
        session: w.session,
        sequence: 2,
        command: Command::Submit(submission(&job(1000), need(), alloc)),
    };
    stream.write_all(&frame(&encode(&ClientFrame::Request(r), alloc), alloc)).unwrap();
    assert!(matches!(read(&mut stream, alloc), ServerFrame::Error(_)));
    let g = Fixture::new(false, Config::default());
    let mut stream = raw(&g);
    stream
        .write_all(&frame(&encode(&ClientFrame::Hello(resume(w.session, w.epoch)), alloc), alloc))
        .unwrap();
    assert!(matches!(read(&mut stream, alloc), ServerFrame::Error(_)));
}

#[test]
fn hello_retry_is_idempotent_and_clock_rollback_does_not_extend_lifetime() {
    let f = Fixture::new(false, Config::default());
    let alloc = f.alloc;
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
    let resumed = resume_raw(&mut stream, &first, alloc);
    assert!(resumed.now_ms >= 100);
    assert_eq!(resumed.expires_ms, first.expires_ms);
}

#[test]
fn the_coordinator_owns_and_removes_its_actual_socket() {
    let alloc = test_alloc();
    let dir = TempDir::new("bc", alloc).unwrap();
    let path = dir.join("c.sock");
    let executable = env!("CARGO_BIN_EXE_blaze-dist-local");
    let mut child = Child::spawn(
        executable,
        &["coordinate", path.as_str(), "some-triple", "0"],
        true,
        true,
        alloc,
    )
    .unwrap();
    let ready = child.read_stdout_line(alloc).unwrap().unwrap();
    assert!(ready.starts_with("ready "));
    let mut c = Client::connect(path.as_str(), random_id().unwrap(), alloc).unwrap();
    let deadline = c.welcome.now_ms + 1000;
    let b = submit(&mut c, deadline, alloc);
    let Answer::Terminal(t) = c.call(Command::Terminal(b.subscription)).unwrap() else { panic!() };
    assert!(t.value.is_none()); // No executor: nothing can claim Passed.
    child.write_stdin_and_close(b"stop\n").unwrap();
    assert!(child.wait().unwrap().success());
    assert!(!path_exists(path.as_str()));
}

fn two_roots() -> RootSet {
    RootSet { spec: [1; 32], roots: [2; 32], count: 2, index: [3; 32], inputs: [4; 32] }
}

fn claim_as(c: &mut Client, binding: &Binding, claimant: blaze_wire::Digest) -> Claimed {
    let answer = c
        .call(Command::Claim(Claim {
            content: binding.subscription.content,
            execution: binding.subscription.execution,
            claimant,
        }))
        .unwrap();
    let Answer::Claimed(claimed) = answer else { panic!("claim was refused: {answer:?}") };
    claimed
}

fn retirement_of(
    binding: &Binding,
    claimed: &Claimed,
    claimant: blaze_wire::Digest,
) -> RetireAssignment {
    RetireAssignment {
        content: binding.subscription.content,
        executor: claimed.executor.clone().expect("an assigned executor"),
        execution: binding.subscription.execution,
        attempt: claimed.attempt,
        claimant,
    }
}

fn stop_ack_of(retired: &RetireAssignment) -> KillAck {
    KillAck {
        content: retired.content,
        executor: retired.executor.clone(),
        execution: retired.execution,
        attempt: retired.attempt,
        alive: true,
    }
}

fn lookup_of(retired: &RetireAssignment) -> OwnedAssignment {
    OwnedAssignment {
        content: retired.content,
        executor: retired.executor.clone(),
        execution: retired.execution,
        claimant: retired.claimant,
    }
}

fn owned_attempt(client: &mut Client, lookup: &OwnedAssignment) -> Attempt {
    let answer = client.call(Command::OwnedAssignment(lookup.clone())).unwrap();
    let Answer::OwnedAssignment(owned) = answer else {
        panic!("owned assignment lookup {lookup:?} was refused: {answer:?}");
    };
    owned.attempt
}

fn lookup_refused(client: &mut Client, lookup: &OwnedAssignment, refusal: Refusal) {
    let answer = client.call(Command::OwnedAssignment(lookup.clone())).unwrap();
    let Answer::Error(error) = answer else {
        panic!("unowned assignment lookup {lookup:?} answered {answer:?}");
    };
    assert_eq!(Refusal::of(&error), Some(refusal));
}

fn join_as(
    c: &mut Client,
    content: blaze_wire::JobContent,
    b: &Binding,
    who: &str,
    alloc: WireAlloc,
) {
    let answer = c
        .call(Command::Join(Join {
            content,
            executor: WireString::from_str_in(who, alloc),
            request: WorkJoinRequest {
                execution: b.subscription.execution,
                attempt: b.attempt,
                roots: two_roots(),
                rejected: Set::new(Vec::new_in(alloc)),
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
    alloc: WireAlloc,
) -> blaze_wire::Grant {
    let Answer::Lease(answer) = c
        .call(Command::Lease(Lease {
            content,
            executor: WireString::from_str_in(who, alloc),
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
    alloc: WireAlloc,
) -> Answer {
    let mut items = Vec::new_in(alloc);
    items.push(ItemResult {
        name: WorkName([tag; 32]),
        outcome: ItemOutcome::Local(Why { why: WireString::from_str_in("fixture", alloc) }),
        found: Set::new(Vec::new_in(alloc)),
        unnamed: Set::new(Vec::new_in(alloc)),
    });
    c.call(Command::Result(BatchSubmission {
        content,
        executor: WireString::from_str_in(who, alloc),
        request: BatchResult {
            lease: grant.lease.clone(),
            seq: 1,
            items,
            packs: Vec::new_in(alloc),
            returned_roots: Set::new(Vec::new_in(alloc)),
            returned_names: Set::new(Vec::new_in(alloc)),
        },
    }))
    .unwrap()
}

fn heartbeat_as(
    c: &mut Client,
    content: blaze_wire::JobContent,
    b: &Binding,
    who: &str,
    alloc: WireAlloc,
) {
    let answer = c
        .call(Command::WorkerEvents(WorkerEvents {
            content,
            executor: WireString::from_str_in(who, alloc),
            request: WorkersEventsRequest {
                execution: b.subscription.execution,
                attempt: b.attempt,
                events: Vec::new_in(alloc),
            },
        }))
        .unwrap();
    assert!(matches!(answer, Answer::WorkerEvents(_)), "{answer:?}");
}

fn done_passed(
    c: &mut Client,
    content: blaze_wire::JobContent,
    b: &Binding,
    lead: &str,
    alloc: WireAlloc,
) {
    let mut events = Vec::new_in(alloc);
    events
        .push(JobEvent::Done(DoneEvent { outcome: Outcome::Passed, counters: Vec::new_in(alloc) }));
    let answer = c
        .call(Command::WorkerEvents(WorkerEvents {
            content,
            executor: WireString::from_str_in(lead, alloc),
            request: WorkersEventsRequest {
                execution: b.subscription.execution,
                attempt: b.attempt,
                events,
            },
        }))
        .unwrap();
    assert!(matches!(answer, Answer::WorkerEvents(_)), "{answer:?}");
    let Answer::Terminal(t) = c.call(Command::Terminal(b.subscription.clone())).unwrap() else {
        panic!("no terminal")
    };
    assert_eq!(t.value.as_ref().unwrap().termination.outcome, Outcome::Passed);
}

#[test]
fn owned_helper_retirement_is_fenced_and_capacity_waits_for_its_stop_ack() {
    let f = Fixture::with_workers(
        2,
        ExecutionConfig {
            work: blaze_dist::work::WorkConfig { grant_roots: 1, ..Default::default() },
            ..Default::default()
        },
    );
    let alloc = f.alloc;
    let mut lead_client = f.client();
    let binding = submit(&mut lead_client, 1_000_000, alloc);
    let lead = claim_as(&mut lead_client, &binding, [31; 32]);
    assert!(lead.lead);
    let lead_executor = lead.executor.unwrap();
    let mut helper_client = f.client();
    let helper_binding = submit(&mut helper_client, 1_000_000, alloc);
    assert_eq!(helper_binding.subscription.execution, binding.subscription.execution);
    let helper = claim_as(&mut helper_client, &helper_binding, [32; 32]);
    assert!(!helper.lead);
    let retire = retirement_of(&helper_binding, &helper, [32; 32]);
    let content = binding.subscription.content;
    join_as(&mut lead_client, content, &binding, lead_executor.as_str(), alloc);
    join_as(&mut helper_client, content, &binding, retire.executor.as_str(), alloc);
    let first = lease_as(&mut helper_client, content, &binding, retire.executor.as_str(), alloc);

    let before = f.effects.lock().clone();
    let events = Command::Events(Events { binding: binding.clone(), from: 0, limit: 256 });
    let before_events = lead_client.call(events.clone()).unwrap();
    let mut wrong_owner = retire.clone();
    wrong_owner.claimant = [33; 32];
    let mut wrong_executor = retire.clone();
    wrong_executor.executor = lead_executor.clone();
    let mut wrong_content = retire.clone();
    wrong_content.content = blaze_wire::JobContent([34; 32]);
    let mut wrong_execution = retire.clone();
    wrong_execution.execution = ExecutionId(retire.execution.0 + 99);
    let mut wrong_attempt = retire.clone();
    wrong_attempt.attempt = Attempt(retire.attempt.0 + 1);
    for (request, refused) in [
        (wrong_owner, Refusal::NotHolder),
        (wrong_executor, Refusal::NotHolder),
        (wrong_content, Refusal::NoExecution),
        (wrong_execution, Refusal::NoExecution),
        (wrong_attempt, Refusal::AttemptMoved),
    ] {
        let Answer::Error(error) = helper_client.call(Command::RetireAssignment(request)).unwrap()
        else {
            panic!("a retirement with the wrong assignment or owner was accepted");
        };
        assert_eq!(Refusal::of(&error), Some(refused));
    }
    // Copying the label into a separately issued session neither claims nor retires
    // the helper. The session itself is part of the coordinator's owner identity.
    let mut intruder = f.client();
    assert!(claim_as(&mut intruder, &binding, retire.claimant).executor.is_none());
    let Answer::Error(error) = intruder.call(Command::RetireAssignment(retire.clone())).unwrap()
    else {
        panic!("another session retired the helper with a copied claimant label");
    };
    assert_eq!(Refusal::of(&error), Some(Refusal::NotHolder));
    let after = f.effects.lock().clone();
    assert_eq!(after, before, "a refused request published a transition prefix");
    assert_eq!(lead_client.call(events).unwrap(), before_events);
    assert!(matches!(
        result_as(&mut helper_client, content, retire.executor.as_str(), &first, 41, alloc),
        Answer::Result(_)
    ));
    let stale_work =
        lease_as(&mut helper_client, content, &binding, retire.executor.as_str(), alloc);

    assert_eq!(
        helper_client.call(Command::RetireAssignment(retire.clone())).unwrap(),
        Answer::Ack(Ack {})
    );
    let retired = f.effects.lock().clone();
    assert_eq!(
        retired
            .iter()
            .filter(|(_, effect)| {
                matches!(effect, Effect::Kill { executor, assignment }
                    if executor == &retire.executor
                        && assignment.execution == retire.execution
                        && assignment.attempt == retire.attempt)
            })
            .count(),
        1
    );
    // A new request id from the owner repeats the pending retirement without a second stop.
    assert_eq!(
        helper_client.call(Command::RetireAssignment(retire.clone())).unwrap(),
        Answer::Ack(Ack {})
    );
    // Pending-stop idempotence belongs only to the original owner. It must not
    // turn the same request from an unowned claimant or another session into an Ack.
    for (client, claimant) in [(&mut helper_client, [33; 32]), (&mut intruder, retire.claimant)] {
        let mut request = retire.clone();
        request.claimant = claimant;
        let Answer::Error(error) = client.call(Command::RetireAssignment(request)).unwrap() else {
            panic!("a foreign owner acquired pending-retirement idempotence");
        };
        assert_eq!(Refusal::of(&error), Some(Refusal::NotHolder));
    }
    let repeated = f.effects.lock().clone();
    assert_eq!(repeated, retired);
    for subscription in [binding.subscription.clone(), helper_binding.subscription.clone()] {
        let Answer::Terminal(terminal) = lead_client.call(Command::Terminal(subscription)).unwrap()
        else {
            panic!("retirement lost a subscriber");
        };
        assert!(terminal.value.is_none(), "retirement ended a live subscriber");
    }
    assert!(matches!(
        result_as(&mut helper_client, content, retire.executor.as_str(), &stale_work, 42, alloc),
        Answer::Error(_)
    ));

    let mut waiting = f.client();
    let mut other_job = job(1_000_000);
    other_job.unit = [43; 32];
    let Answer::Binding(queued) =
        waiting.call(Command::Submit(submission(&other_job, need(), alloc))).unwrap()
    else {
        panic!("the other execution was not queued");
    };
    assert!(claim_as(&mut waiting, &queued, [44; 32]).executor.is_none());
    let mut wrong_ack = stop_ack_of(&retire);
    wrong_ack.attempt = Attempt(retire.attempt.0 + 1);
    assert_eq!(
        helper_client.call(Command::KillAck(wrong_ack)).unwrap(),
        Answer::KillAck(KillAnswer { accepted: false })
    );
    assert!(claim_as(&mut waiting, &queued, [44; 32]).executor.is_none());
    assert_eq!(
        helper_client.call(Command::KillAck(stop_ack_of(&retire))).unwrap(),
        Answer::KillAck(KillAnswer { accepted: true })
    );
    let replacement = claim_as(&mut waiting, &queued, [44; 32]);
    assert_eq!(replacement.executor.as_deref(), Some(retire.executor.as_str()));

    // The same slot is now a different execution. Neither the old retirement nor its
    // duplicate stop acknowledgment may change that new assignment or its public events.
    let before_stale = f.effects.lock().clone();
    let Answer::Error(error) =
        helper_client.call(Command::RetireAssignment(retire.clone())).unwrap()
    else {
        panic!("the old execution retired a reused slot");
    };
    assert_eq!(Refusal::of(&error), Some(Refusal::AttemptMoved));
    assert_eq!(
        helper_client.call(Command::KillAck(stop_ack_of(&retire))).unwrap(),
        Answer::KillAck(KillAnswer { accepted: false })
    );
    let after_stale = f.effects.lock().clone();
    assert_eq!(after_stale, before_stale);
    assert_eq!(claim_as(&mut waiting, &queued, [44; 32]), replacement);
    join_as(&mut waiting, queued.subscription.content, &queued, retire.executor.as_str(), alloc);
    heartbeat_as(
        &mut waiting,
        queued.subscription.content,
        &queued,
        retire.executor.as_str(),
        alloc,
    );

    let recovered = lease_as(&mut lead_client, content, &binding, lead_executor.as_str(), alloc);
    assert_eq!(recovered.roots, stale_work.roots);
    assert!(recovered.lease.generation > stale_work.lease.generation);
    assert!(matches!(
        result_as(&mut lead_client, content, lead_executor.as_str(), &recovered, 45, alloc),
        Answer::Result(_)
    ));
    done_passed(&mut lead_client, content, &binding, lead_executor.as_str(), alloc);
}

#[test]
fn retirement_replay_and_session_resume_emit_only_one_stop() {
    let f = Fixture::with_workers(2, ExecutionConfig::default());
    let alloc = f.alloc;
    let mut lead = f.client();
    let binding = submit(&mut lead, 1_000_000, alloc);
    assert!(claim_as(&mut lead, &binding, [51; 32]).lead);
    let mut stream = raw(&f);
    let welcome = create_raw(&f, &mut stream, [52; 32]);
    let answer = request_raw(
        &mut stream,
        &welcome,
        1,
        Command::Submit(submission(&job(1_000_000), need(), alloc)),
        alloc,
    )
    .answer;
    let Answer::Binding(helper_binding) = answer else {
        panic!("helper did not subscribe");
    };
    let answer = request_raw(
        &mut stream,
        &welcome,
        2,
        Command::Claim(Claim {
            content: binding.subscription.content,
            execution: binding.subscription.execution,
            claimant: [53; 32],
        }),
        alloc,
    )
    .answer;
    let Answer::Claimed(helper) = answer else {
        panic!("helper did not claim its assignment");
    };
    assert!(!helper.lead);
    let retirement = retirement_of(&helper_binding, &helper, [53; 32]);
    let command = Command::RetireAssignment(retirement.clone());
    let first = request_raw(&mut stream, &welcome, 3, command.clone(), alloc);
    assert_eq!(first.answer, Answer::Ack(Ack {}));
    let before = f.effects.lock().clone();
    drop(stream);

    let mut resumed = raw(&f);
    let again = resume_raw(&mut resumed, &welcome, alloc);
    assert_eq!(again.session, welcome.session);
    assert_eq!(request_raw(&mut resumed, &welcome, 3, command.clone(), alloc), first);
    assert_eq!(request_raw(&mut resumed, &welcome, 4, command, alloc).answer, Answer::Ack(Ack {}));
    let after = f.effects.lock().clone();
    assert_eq!(after, before);
    assert_eq!(
        after
            .iter()
            .filter(|(_, effect)| {
                matches!(effect, Effect::Kill { executor, assignment }
                    if executor == &retirement.executor
                        && assignment.execution == retirement.execution
                        && assignment.attempt == retirement.attempt)
            })
            .count(),
        1
    );
    assert_eq!(
        request_raw(&mut resumed, &welcome, 5, Command::KillAck(stop_ack_of(&retirement)), alloc)
            .answer,
        Answer::KillAck(KillAnswer { accepted: true })
    );
}

#[test]
fn stale_retirement_and_ack_cannot_stop_a_helper_promoted_to_lead() {
    let f = Fixture::with_workers(
        2,
        ExecutionConfig {
            work: blaze_dist::work::WorkConfig { grant_roots: 1, ..Default::default() },
            ..Default::default()
        },
    );
    let alloc = f.alloc;
    let mut lead_client = f.client();
    let binding = submit(&mut lead_client, 1_000_000, alloc);
    let lead = claim_as(&mut lead_client, &binding, [61; 32]);
    assert!(lead.lead);
    let retiring_lead = retirement_of(&binding, &lead, [61; 32]);
    let mut helper_client = f.client();
    let helper_binding = submit(&mut helper_client, 1_000_000, alloc);
    let helper = claim_as(&mut helper_client, &helper_binding, [62; 32]);
    assert!(!helper.lead);
    let stale_helper = retirement_of(&helper_binding, &helper, [62; 32]);
    let content = binding.subscription.content;
    join_as(&mut lead_client, content, &binding, retiring_lead.executor.as_str(), alloc);
    join_as(&mut helper_client, content, &binding, stale_helper.executor.as_str(), alloc);

    assert_eq!(
        lead_client.call(Command::RetireAssignment(retiring_lead.clone())).unwrap(),
        Answer::Ack(Ack {})
    );
    let promoted = claim_as(&mut helper_client, &helper_binding, [62; 32]);
    assert!(promoted.lead);
    assert_eq!(promoted.executor, helper.executor);
    assert!(promoted.attempt > helper.attempt);
    let Answer::Binding(current) =
        helper_client.call(Command::Resume(helper_binding.subscription.clone())).unwrap()
    else {
        panic!("the surviving subscriber did not resume its new attempt");
    };
    assert_eq!(current.attempt, promoted.attempt);
    join_as(&mut helper_client, content, &current, stale_helper.executor.as_str(), alloc);
    let active =
        lease_as(&mut helper_client, content, &current, stale_helper.executor.as_str(), alloc);
    let before = f.effects.lock().clone();
    let Answer::Error(error) =
        helper_client.call(Command::RetireAssignment(stale_helper.clone())).unwrap()
    else {
        panic!("an old helper token retired its new lead assignment");
    };
    assert_eq!(Refusal::of(&error), Some(Refusal::AttemptMoved));
    assert_eq!(
        helper_client.call(Command::KillAck(stop_ack_of(&stale_helper))).unwrap(),
        Answer::KillAck(KillAnswer { accepted: false })
    );
    let after = f.effects.lock().clone();
    assert_eq!(after, before);
    heartbeat_as(&mut helper_client, content, &current, stale_helper.executor.as_str(), alloc);
    assert!(matches!(
        result_as(&mut helper_client, content, stale_helper.executor.as_str(), &active, 63, alloc),
        Answer::Result(_)
    ));

    // Both reservations still belong to live or stopping work. The stale ACK above
    // must not make the promoted helper available to an unrelated execution.
    let mut waiting = f.client();
    let mut other_job = job(1_000_000);
    other_job.unit = [64; 32];
    let Answer::Binding(queued) =
        waiting.call(Command::Submit(submission(&other_job, need(), alloc))).unwrap()
    else {
        panic!("another execution did not queue");
    };
    assert!(claim_as(&mut waiting, &queued, [65; 32]).executor.is_none());
    assert_eq!(
        lead_client.call(Command::KillAck(stop_ack_of(&retiring_lead))).unwrap(),
        Answer::KillAck(KillAnswer { accepted: true })
    );
    let reassigned = claim_as(&mut waiting, &queued, [65; 32]);
    assert_eq!(reassigned.executor.as_deref(), Some(retiring_lead.executor.as_str()));
    assert_eq!(claim_as(&mut helper_client, &current, [62; 32]), promoted);
}

#[test]
fn owned_lookup_never_claims_a_slot_and_binds_owner_session_and_execution() {
    let f = Fixture::with_workers(2, ExecutionConfig::default());
    let alloc = f.alloc;
    let mut owner = f.client();
    let binding = submit(&mut owner, 1_000_000, alloc);
    let claimed = claim_as(&mut owner, &binding, [71; 32]);
    let retired = retirement_of(&binding, &claimed, [71; 32]);
    let lookup = lookup_of(&retired);
    let helper = f
        .executors
        .iter()
        .find(|who| who.as_str() != retired.executor.as_str())
        .unwrap()
        .clone();
    let mut stranger = f.client();
    let before = f.effects.lock().clone();
    let events = Command::Events(Events { binding: binding.clone(), from: 0, limit: 256 });
    let before_events = owner.call(events.clone()).unwrap();

    assert_eq!(owned_attempt(&mut owner, &lookup), claimed.attempt);
    lookup_refused(&mut stranger, &lookup, Refusal::NotHolder);
    let mut wrong_label = lookup.clone();
    wrong_label.claimant = [72; 32];
    lookup_refused(&mut owner, &wrong_label, Refusal::NotHolder);
    let mut unclaimed = lookup.clone();
    unclaimed.executor = helper.clone();
    lookup_refused(&mut owner, &unclaimed, Refusal::NotHolder);
    lookup_refused(&mut stranger, &unclaimed, Refusal::NotHolder);
    let mut missing_slot = lookup.clone();
    missing_slot.executor = WireString::from_str_in("not-an-assigned-executor", alloc);
    lookup_refused(&mut owner, &missing_slot, Refusal::NotHolder);
    let mut wrong_content = lookup.clone();
    wrong_content.content = blaze_wire::JobContent([73; 32]);
    lookup_refused(&mut owner, &wrong_content, Refusal::NoExecution);
    let mut missing_execution = lookup.clone();
    missing_execution.execution = ExecutionId(lookup.execution.0 + 99);
    lookup_refused(&mut owner, &missing_execution, Refusal::NoExecution);

    owner.reconnect(f.path.as_str()).unwrap();
    assert_eq!(owned_attempt(&mut owner, &lookup), claimed.attempt);
    assert_eq!(f.effects.lock().clone(), before, "lookup changed the assignment");
    assert_eq!(owner.call(events).unwrap(), before_events);

    // The earlier lookups used label 71. They must leave the spare slot available
    // to a deliberate claim with a different label, without taking the lead's slot.
    let helper_claim = claim_as(&mut stranger, &binding, [74; 32]);
    assert_eq!(helper_claim.executor.as_deref(), Some(helper.as_str()));
    assert!(!helper_claim.lead);
    lookup_refused(&mut stranger, &unclaimed, Refusal::NotHolder);
    let helper_lookup = lookup_of(&retirement_of(&binding, &helper_claim, [74; 32]));
    assert_eq!(owned_attempt(&mut stranger, &helper_lookup), helper_claim.attempt);
    assert_eq!(owned_attempt(&mut owner, &lookup), claimed.attempt);
}

#[test]
fn lookup_follows_continuing_owners_across_promotion_without_a_second_claim() {
    let f = Fixture::with_workers(3, ExecutionConfig::default());
    let alloc = f.alloc;
    let mut lead_client = f.client();
    let binding = submit(&mut lead_client, 1_000_000, alloc);
    let lead = claim_as(&mut lead_client, &binding, [81; 32]);
    assert!(lead.lead);
    let retiring_lead = retirement_of(&binding, &lead, [81; 32]);
    let mut helper_client = f.client();
    let helper_binding = submit(&mut helper_client, 1_000_000, alloc);
    let helper = claim_as(&mut helper_client, &helper_binding, [82; 32]);
    assert!(!helper.lead);
    let former_helper = retirement_of(&helper_binding, &helper, [82; 32]);
    let mut continuing_client = f.client();
    let continuing_binding = submit(&mut continuing_client, 1_000_000, alloc);
    let continuing = claim_as(&mut continuing_client, &continuing_binding, [83; 32]);
    assert!(!continuing.lead);
    let former_continuing = retirement_of(&continuing_binding, &continuing, [83; 32]);
    assert_ne!(former_helper.executor, former_continuing.executor);
    // Both helpers report work before promotion; a claimed but unheard helper is
    // not a live replacement. Their claim order follows the registry's slot order.
    join_as(
        &mut lead_client,
        binding.subscription.content,
        &binding,
        retiring_lead.executor.as_str(),
        alloc,
    );
    join_as(
        &mut helper_client,
        binding.subscription.content,
        &binding,
        former_helper.executor.as_str(),
        alloc,
    );
    join_as(
        &mut continuing_client,
        binding.subscription.content,
        &binding,
        former_continuing.executor.as_str(),
        alloc,
    );
    assert_eq!(
        lead_client.call(Command::RetireAssignment(retiring_lead.clone())).unwrap(),
        Answer::Ack(Ack {})
    );
    let Answer::Binding(current) =
        helper_client.call(Command::Resume(helper_binding.subscription.clone())).unwrap()
    else {
        panic!("the surviving subscription could not observe promotion");
    };
    assert!(current.attempt > helper.attempt);
    let before = f.effects.lock().clone();
    let promoted = before
        .iter()
        .rev()
        .find_map(|(_, effect)| match effect {
            Effect::Event(item) => match &item.event {
                JobEvent::Assigned(assigned) => Some(assigned),
                _ => None,
            },
            _ => None,
        })
        .expect("promotion emitted its assignment");
    assert_eq!(promoted.executor, former_helper.executor);
    assert_eq!(promoted.attempt, current.attempt);
    let helper_lookup = lookup_of(&former_helper);
    let continuing_lookup = lookup_of(&former_continuing);
    let owned_helper_attempt = owned_attempt(&mut helper_client, &helper_lookup);
    assert_eq!(owned_helper_attempt, current.attempt);
    assert_eq!(owned_attempt(&mut continuing_client, &continuing_lookup), current.attempt);
    assert_eq!(
        owned_attempt(&mut lead_client, &lookup_of(&retiring_lead)),
        retiring_lead.attempt,
        "the stopping lead owns its prior token, not the execution's current token"
    );
    helper_client.reconnect(f.path.as_str()).unwrap();
    assert_eq!(owned_attempt(&mut helper_client, &helper_lookup), current.attempt);
    let mut intruder = f.client();
    for lookup in [&helper_lookup, &continuing_lookup] {
        lookup_refused(&mut intruder, lookup, Refusal::NotHolder);
    }
    // If promotion erased the owners, this same-label foreign claim could steal
    // one. The owner lookups above happen before any claim could repair that loss.
    assert!(claim_as(&mut intruder, &current, [82; 32]).executor.is_none());
    assert_eq!(f.effects.lock().clone(), before);

    // Cleanup uses the looked-up token directly, without another Claim. Retiring
    // the promoted lead exhausts the bounded two-attempt policy; its stop and the
    // continuing helper's stop still retain their exact owners and tokens.
    let retiring_promoted = RetireAssignment { attempt: owned_helper_attempt, ..former_helper };
    assert_eq!(
        helper_client.call(Command::RetireAssignment(retiring_promoted.clone())).unwrap(),
        Answer::Ack(Ack {})
    );
    let Answer::Terminal(terminal) = continuing_client
        .call(Command::Terminal(continuing_binding.subscription.clone()))
        .unwrap()
    else {
        panic!("the remaining subscriber could not observe bounded lead loss");
    };
    assert_eq!(terminal.value.unwrap().termination.outcome, Outcome::Lost);
    assert_eq!(owned_attempt(&mut continuing_client, &continuing_lookup), current.attempt);
    assert_eq!(owned_attempt(&mut helper_client, &helper_lookup), retiring_promoted.attempt);
    assert_eq!(
        owned_attempt(&mut lead_client, &lookup_of(&retiring_lead)),
        retiring_lead.attempt
    );
    assert_eq!(
        helper_client.call(Command::KillAck(stop_ack_of(&retiring_promoted))).unwrap(),
        Answer::KillAck(KillAnswer { accepted: true })
    );
    lookup_refused(&mut helper_client, &helper_lookup, Refusal::NotHolder);
    assert_eq!(owned_attempt(&mut continuing_client, &continuing_lookup), current.attempt);
}

#[test]
fn a_reused_slot_requires_a_new_claim_even_for_the_same_session_label_and_content() {
    let f = Fixture::new(true, Config::default());
    let alloc = f.alloc;
    let mut owner = f.client();
    let binding = submit(&mut owner, 1_000_000, alloc);
    let claimed = claim_as(&mut owner, &binding, [91; 32]);
    let old = retirement_of(&binding, &claimed, [91; 32]);
    let old_lookup = lookup_of(&old);
    assert_eq!(owner.call(Command::Cancel(binding.clone())).unwrap(), Answer::Ack(Ack {}));
    assert_eq!(owned_attempt(&mut owner, &old_lookup), old.attempt);
    assert_eq!(owner.call(Command::RetireAssignment(old.clone())).unwrap(), Answer::Ack(Ack {}));
    assert_eq!(
        owner.call(Command::KillAck(stop_ack_of(&old))).unwrap(),
        Answer::KillAck(KillAnswer { accepted: true })
    );
    lookup_refused(&mut owner, &old_lookup, Refusal::NotHolder);

    let replacement = submit(&mut owner, 1_000_000, alloc);
    assert_eq!(replacement.subscription.content, binding.subscription.content);
    assert_ne!(replacement.subscription.execution, binding.subscription.execution);
    let new_lookup =
        OwnedAssignment { execution: replacement.subscription.execution, ..old_lookup.clone() };
    lookup_refused(&mut owner, &new_lookup, Refusal::NotHolder);
    let new_claim = claim_as(&mut owner, &replacement, [91; 32]);
    assert_eq!(new_claim.executor.as_deref(), Some(old.executor.as_str()));
    assert_eq!(new_claim.attempt, old.attempt);
    assert_eq!(owned_attempt(&mut owner, &new_lookup), new_claim.attempt);

    // Content, slot and owner are deliberately identical. The execution fence
    // alone keeps the old lookup, retirement and stop acknowledgment harmless.
    let before = f.effects.lock().clone();
    lookup_refused(&mut owner, &old_lookup, Refusal::NotHolder);
    let Answer::Error(error) = owner.call(Command::RetireAssignment(old.clone())).unwrap() else {
        panic!("old execution retired its same-owner replacement");
    };
    assert_eq!(Refusal::of(&error), Some(Refusal::AttemptMoved));
    assert_eq!(
        owner.call(Command::KillAck(stop_ack_of(&old))).unwrap(),
        Answer::KillAck(KillAnswer { accepted: false })
    );
    assert_eq!(f.effects.lock().clone(), before);
    assert_eq!(owned_attempt(&mut owner, &new_lookup), new_claim.attempt);
    join_as(
        &mut owner,
        replacement.subscription.content,
        &replacement,
        old.executor.as_str(),
        alloc,
    );
    heartbeat_as(
        &mut owner,
        replacement.subscription.content,
        &replacement,
        old.executor.as_str(),
        alloc,
    );
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
    let alloc = f.alloc;
    let (lead, helper) = (f.executors[0].clone(), f.executors[1].clone());
    let mut c = f.client();
    let b = submit(&mut c, 1_000_000, alloc);
    let content = b.subscription.content;
    join_as(&mut c, content, &b, lead.as_str(), alloc);
    join_as(&mut c, content, &b, helper.as_str(), alloc);
    let first = lease_as(&mut c, content, &b, lead.as_str(), alloc);
    let second = lease_as(&mut c, content, &b, helper.as_str(), alloc);
    assert_eq!(first.roots.as_slice(), &[(0, 1)]);
    assert_eq!(second.roots.as_slice(), &[(1, 2)]);
    assert!(matches!(result_as(&mut c, content, lead.as_str(), &first, 9, alloc), Answer::Result(_)));
    assert!(matches!(
        result_as(&mut c, content, helper.as_str(), &second, 10, alloc),
        Answer::Result(_)
    ));
    done_passed(&mut c, content, &b, lead.as_str(), alloc);

    let effects = f.effects.lock();
    assert!(
        effects.iter().any(|(_, effect)| matches!(
            effect,
            Effect::HelperAssigned { executor, .. } if executor == &helper
        )),
        "no helper assignment was delivered: {effects:?}"
    );
    let mut committed = Vec::new_in(alloc);
    for (_, effect) in effects.iter() {
        if let Effect::Work(blaze_dist::work::Effect::Committed { by, .. }) = effect {
            committed.push(by.holder.as_str());
        }
    }
    assert!(committed.scan_for(&lead.as_str()).is_some(), "{committed:?}");
    assert!(committed.scan_for(&helper.as_str()).is_some(), "{committed:?}");
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
    let alloc = f.alloc;
    let (lead, helper) = (f.executors[0].clone(), f.executors[1].clone());
    let mut c = f.client();
    let b = submit(&mut c, 1_000_000, alloc);
    let content = b.subscription.content;
    join_as(&mut c, content, &b, lead.as_str(), alloc);
    join_as(&mut c, content, &b, helper.as_str(), alloc);
    let stale = lease_as(&mut c, content, &b, helper.as_str(), alloc);
    assert_eq!(stale.roots.as_slice(), &[(0, 1)]);

    // The helper never speaks again. The lead heartbeats inside its own 15 s
    // lead-loss window while the clock passes the helper's 20 s lease.
    for at in [10_000, 20_000, 25_000] {
        f.at(at);
        heartbeat_as(&mut c, content, &b, lead.as_str(), alloc);
    }
    let regranted = lease_as(&mut c, content, &b, lead.as_str(), alloc);
    assert_eq!(regranted.roots.as_slice(), &[(0, 1)]);
    assert!(regranted.lease.generation > stale.lease.generation, "{regranted:?} vs {stale:?}");

    let late = result_as(&mut c, content, helper.as_str(), &stale, 10, alloc);
    let Answer::Error(error) = late else { panic!("a fenced result was accepted: {late:?}") };
    assert!(error.message.contains("dead-lease"), "{error:?}");

    assert!(matches!(
        result_as(&mut c, content, lead.as_str(), &regranted, 9, alloc),
        Answer::Result(_)
    ));
    let rest = lease_as(&mut c, content, &b, lead.as_str(), alloc);
    assert_eq!(rest.roots.as_slice(), &[(1, 2)]);
    // The lead's second batch of the attempt carries its next sequence number.
    let mut items = Vec::new_in(alloc);
    items.push(ItemResult {
        name: WorkName([11; 32]),
        outcome: ItemOutcome::Local(Why { why: WireString::from_str_in("fixture", alloc) }),
        found: Set::new(Vec::new_in(alloc)),
        unnamed: Set::new(Vec::new_in(alloc)),
    });
    let second = BatchSubmission {
        content,
        executor: lead.clone(),
        request: BatchResult {
            lease: rest.lease.clone(),
            seq: 2,
            items,
            packs: Vec::new_in(alloc),
            returned_roots: Set::new(Vec::new_in(alloc)),
            returned_names: Set::new(Vec::new_in(alloc)),
        },
    };
    assert!(matches!(c.call(Command::Result(second)).unwrap(), Answer::Result(_)));
    done_passed(&mut c, content, &b, lead.as_str(), alloc);
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

    struct Compiler(WireAlloc);
    impl BatchCompiler for Compiler {
        type Error = Infallible;

        fn compile(&mut self, plan: &Plan) -> Result<Compiled, Self::Error> {
            let alloc = self.0;
            let ranges = blaze_dist::Ranges::from_wire(&plan.grant.roots, alloc).unwrap();
            let mut roots = Vec::new_in(alloc);
            for index in ranges.indexes() {
                let b = u8::try_from(index).unwrap();
                roots.push(ItemResult {
                    name: WorkName([b + 9; 32]),
                    outcome: ItemOutcome::Exported(EntryRef { entry: EntryKey([b + 20; 32]) }),
                    found: Set::new(Vec::new_in(alloc)),
                    unnamed: Set::new(Vec::new_in(alloc)),
                });
            }
            Ok(Compiled {
                generation: plan.grant.lease.generation,
                roots,
                names: Vec::new_in(alloc),
                packs: Vec::new_in(alloc),
                returned_roots: blaze_dist::Ranges::new(alloc),
                returned_names: Vec::new_in(alloc),
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
        let alloc = f.alloc;
        let (lead, helper) = (f.executors[0].clone(), f.executors[1].clone());
        let mut control = f.client();
        let binding = submit(&mut control, 100_000, alloc);
        let content = binding.subscription.content;
        join_as(&mut control, content, &binding, lead.as_str(), alloc);
        let directory = TempDir::new("bg", alloc).unwrap();
        let gate_path = WireString::from_str_in(directory.path(), alloc);
        let config = DriverConfig {
            result_gate: Some(
                ResultGate::new(gate_path.clone(), Duration::from_secs(3), alloc).unwrap(),
            ),
            ..Default::default()
        };
        let leave = AtomicBool::new(false);
        let clock_sys = SystemClock;
        scope(alloc, |s| {
            let mut client = f.client();
            let mut holder = Holder::new(
                helper.as_str(),
                binding.subscription.execution,
                binding.attempt,
                two_roots(),
                Set::new(Vec::new_in(alloc)),
                BTreeSet::new_in(alloc),
                HolderConfig { heartbeat_ms: 5, retry_ms: 100, idle_ms: 5 },
                alloc,
            );
            let clock = f.clock.clone();
            let (socket, executor, config, leave) =
                (f.path.as_str(), helper.as_str(), &config, &leave);
            let worker = s
                .spawn(move || {
                    run_holder_until(
                        &mut client,
                        socket,
                        &mut holder,
                        content,
                        executor,
                        config,
                        || clock.now_ms(),
                        &mut Compiler(alloc),
                        leave,
                    )
                })
                .unwrap();
            let deadline_ns = clock_sys.monotonic_nanos().unwrap().saturating_add(2_000_000_000);
            let ready_file = directory.join("ready.json");
            while !path_is_file(ready_file.as_str()) {
                assert!(
                    clock_sys.monotonic_nanos().unwrap() < deadline_ns,
                    "the real Holder never produced its gated result"
                );
                sleep_ms(1);
            }
            let result_file = directory.join("result.cbor");
            let bytes = read_file(result_file.as_str(), alloc).unwrap();
            let Command::Result(captured) =
                decode::<Command>(&bytes, &Limits::DOCUMENTED, alloc).unwrap()
            else {
                panic!("capture was not Client::Result")
            };
            assert_eq!(captured.executor, helper);
            assert_eq!(captured.request.lease.holder, helper);
            assert_eq!(captured.request.lease.execution, binding.subscription.execution);
            assert!(
                f.effects.lock().iter().any(|(_, effect)| matches!(
                    effect,
                    Effect::Work(blaze_dist::work::Effect::Granted { holder, generation, .. })
                        if holder == &helper && *generation == captured.request.lease.generation
                )),
                "capture did not name an actually issued lease"
            );
            if expired {
                f.at(21);
                // The lead stayed assigned, but its work-set membership also expired.
                join_as(&mut control, content, &binding, lead.as_str(), alloc);
                let replacement = lease_as(&mut control, content, &binding, lead.as_str(), alloc);
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
            let other = lease_as(&mut control, content, &binding, lead.as_str(), alloc);
            let mut result = captured.clone();
            result.executor = lead.clone();
            result.request.lease = other.lease;
            result.request.seq = if expired { 2 } else { 1 };
            result.request.items[0].name = WorkName([10; 32]);
            assert!(matches!(control.call(Command::Result(result)).unwrap(), Answer::Result(_)));
            leave.store(true, Ordering::Release);
            let release_file = directory.join("release");
            write_file(release_file.as_str(), b"").unwrap();
            worker.join().unwrap().unwrap();
            let answer_file = directory.join("answer.cbor");
            let answer_bytes = read_file(answer_file.as_str(), alloc).unwrap();
            let answer = decode::<Answer>(&answer_bytes, &Limits::DOCUMENTED, alloc).unwrap();
            if expired {
                let Answer::Error(error) = answer else {
                    panic!("a stale result was accepted: {answer:?}")
                };
                assert_eq!(blaze_dist::Refusal::of(&error), Some(blaze_dist::Refusal::DeadLease));
            } else {
                assert!(matches!(answer, Answer::Result(_)), "{answer:?}");
            }
            let committed = f.effects.lock().iter().any(|(_, effect)| matches!(
                effect,
                Effect::Work(blaze_dist::work::Effect::Committed { by, .. })
                    if by.holder == helper
                        && by.generation == captured.request.lease.generation
                        && by.seq == captured.request.seq
            ));
            assert_eq!(committed, !expired, "only the unexpired captured identity may commit");
            assert_eq!(
                read_file(result_file.as_str(), alloc).unwrap(),
                bytes,
                "the gate never rewrites the real carrier"
            );
        });
    }
}
