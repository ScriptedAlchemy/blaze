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
    /// Stop the reactor deliberately, keeping the path so a later call fails on IO rather
    /// than on a missing socket. Drop then finds the thread already joined.
    fn stop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            thread.join().unwrap();
        }
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
    assert!(matches!(c.call(Command::Terminal(wrong.clone())).unwrap(), Answer::Error(_)));
    let t = Terminal { subscription: wrong, value: None };
    assert!(terminal_action(&b, &t, &LocalPolicy::Fallback).is_err());
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
fn ordinary_cli_hellos_exceed_the_live_cap_without_restart_or_capacity_loss() {
    // The reported defect: a CLI that mints a lifetime per call consumed the default
    // 32-session cap forever. Forty-five real `hello` processes against one daemon, each
    // releasing its lifetime, must all succeed, and the daemon must stay bounded.
    let f = Fixture::new(false, Config::default());
    let executable = env!("CARGO_BIN_EXE_blaze-dist-local");
    let mut sessions = Vec::new();
    for _ in 0..45 {
        let output =
            std::process::Command::new(executable).arg("hello").arg(&f.path).output().unwrap();
        assert!(
            output.status.success(),
            "hello failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let hex = String::from_utf8(output.stdout).unwrap();
        let bytes: Vec<u8> = hex
            .trim()
            .as_bytes()
            .chunks_exact(2)
            .map(|p| u8::from_str_radix(std::str::from_utf8(p).unwrap(), 16).unwrap())
            .collect();
        let welcome: LocalWelcome = decode(&bytes, &Limits::DOCUMENTED).unwrap();
        assert!(welcome.created);
        assert_eq!(welcome.transport, TRANSPORT_VERSION);
        sessions.push(welcome.session);
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
fn pending_request_is_never_migrated_into_a_fresh_lifetime() {
    // A client whose answer was lost holds an unresolved request. It may not renew into a
    // new lifetime while that request is pending: renewing would re-issue the work as a new
    // request and repeat the effect. Abandoning is explicit and returns the exact bytes.
    let mut f = Fixture::new(true, Config::default());
    let mut c = f.client();
    let first = c.welcome.clone();
    let b = submit(&mut c, 100_000);
    let count_assigned = |f: &Fixture| {
        f.effects
            .lock()
            .unwrap()
            .iter()
            .filter(
                |(_, e)| matches!(e, Effect::Event(i) if matches!(i.event, JobEvent::Assigned(_))),
            )
            .count()
    };
    assert_eq!(count_assigned(&f), 1);
    // Stop the daemon so the next call cannot be answered: the client keeps the request.
    f.stop();
    assert!(c.call(Command::Resume(b.subscription.clone())).is_err());
    // Renewal is refused before any connection, so the request cannot be re-issued.
    let refused = c.renew(&f.path, [32; 32]).unwrap_err();
    assert!(format!("{refused:?}").contains("pending-request"));
    // Abandoning is explicit, returns the exact pending bytes, and only then is renewal
    // ordinary: a genuinely fresh lifetime, not the old one under a new label.
    let abandoned = c.abandon_pending().expect("the unresolved request is returned");
    assert_eq!(abandoned.session, first.session);
    assert_eq!(abandoned.sequence, 2);
    // With nothing pending the guard is gone; a dead path now fails on IO, not on the guard.
    let renewed = c.renew(&f.path, [32; 32]).unwrap_err();
    assert!(!format!("{renewed:?}").contains("pending-request"));
    // The work was submitted exactly once: abandoning and failing to renew repeats no effect.
    assert_eq!(count_assigned(&f), 1);
}

#[test]
fn committed_lost_answer_cannot_be_reissued_by_renewal_or_replayed_after_expiry() {
    // Drop the answer after Submit commits. Neither renewing into a fresh session nor
    // replaying the old bytes after expiry may submit the work again. The production client
    // receives no answer because the test drops it on the socket.
    let f = Fixture::new(true, Config { session_ms: 40, ..Config::default() });
    let count_assigned = |f: &Fixture| {
        f.effects
            .lock()
            .unwrap()
            .iter()
            .filter(
                |(_, e)| matches!(e, Effect::Event(i) if matches!(i.event, JobEvent::Assigned(_))),
            )
            .count()
    };
    let nonce = [61; 32];
    let mut c = Client::connect(&f.path, nonce).unwrap();
    let committed = c.welcome.clone();
    // Submit on the client's own journaled path, then throw the answer away by dropping the
    // connection after the daemon has committed: the request bytes stay reconstructible.
    let mut stream = raw(&f);
    let w = resume_raw(&mut stream, &committed);
    assert_eq!(w.session, committed.session);
    let submit_bytes = frame(&encode(&ClientFrame::Request(Request {
        epoch: w.epoch,
        session: w.session,
        sequence: w.next_request,
        command: Command::Submit(submission(&job(100_000), need())),
    })));
    stream.write_all(&submit_bytes).unwrap();
    // The daemon answered, but this client never read it: drop the socket with the reply
    // still queued. The commit is real, and exactly one Assigned effect exists.
    drop(stream);
    std::thread::sleep(Duration::from_millis(20));
    assert_eq!(count_assigned(&f), 1);
    // (a) Renewal with the same nonce while the lifetime is live adopts it, so nothing new is
    // submitted; the pending answer is recoverable by replaying the exact bytes, not by re-issuing.
    let mut same = Client::connect(&f.path, nonce).unwrap();
    assert!(!same.welcome.created, "the live nonce names the committed lifetime");
    assert_eq!(same.welcome.session, committed.session);
    let mut stream = raw(&f);
    resume_raw(&mut stream, &committed);
    stream.write_all(&submit_bytes).unwrap();
    let ServerFrame::Response(Response { answer: Answer::Binding(b), .. }) = read(&mut stream)
    else {
        panic!("the journaled answer must replay")
    };
    assert_eq!(b.subscription.content, submission(&job(100_000), need()).content);
    assert_eq!(count_assigned(&f), 1, "replay must not submit twice");
    // (b) After expiry the same nonce creates a NEW lifetime, and the old bytes are refused
    // rather than replayed into it: the committed work is never re-issued.
    f.at(41);
    let mut fresh = raw(&f);
    let created = create_raw(&f, &mut fresh, nonce);
    assert!(created.created);
    assert_ne!(created.session, committed.session);
    fresh.write_all(&submit_bytes).unwrap();
    assert!(matches!(read(&mut fresh), ServerFrame::Error(_)));
    assert_eq!(count_assigned(&f), 1, "expired replay must not submit twice");
    // (c) The old label is not resumable either.
    let mut stream = raw(&f);
    stream
        .write_all(&frame(&encode(&ClientFrame::Hello(resume(committed.session, committed.epoch)))))
        .unwrap();
    assert!(matches!(read(&mut stream), ServerFrame::Error(_)));
    let _ = b;
}

#[test]
fn renew_after_release_creates_a_distinct_lifetime_and_old_label_stays_dead() {
    let f = Fixture::new(false, Config::default());
    let mut c = f.client();
    let first = c.welcome.clone();
    c.release().unwrap();
    // The released label is not resumable, so renew must create, never adopt it.
    c.renew(&f.path, [42; 32]).unwrap();
    assert!(c.welcome.created);
    assert_ne!(c.welcome.session, first.session);
    let mut stream = raw(&f);
    stream
        .write_all(&frame(&encode(&ClientFrame::Hello(resume(first.session, first.epoch)))))
        .unwrap();
    assert!(matches!(read(&mut stream), ServerFrame::Error(_)));
    // The renewed lifetime itself resumes normally.
    c.reconnect(&f.path).unwrap();
}

#[test]
fn real_cli_hello_and_lifetime_retention_are_bounded() {
    let f = Fixture::new(false, Config { session_ms: 10, ..Config::default() });
    // The executable is the transport CLI Cargo built for this integration test,
    // not a Cargo subprocess or a compiler fallback.
    let executable = env!("CARGO_BIN_EXE_blaze-dist-local");
    let output = std::process::Command::new(executable).arg("hello").arg(&f.path).output().unwrap();
    assert!(output.status.success());
    let hex = String::from_utf8(output.stdout).unwrap();
    assert!(!hex.trim().is_empty());
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

    let f = Fixture::new(false, Config { max_requests: 1, ..Config::default() });
    let mut c = f.client();
    let b = submit(&mut c, 1000);
    assert!(c.call(Command::Resume(b.subscription)).is_err());
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
    other.spec.source = blaze_wire::SourceRef::Root(blaze_wire::RootRef { root: [9; 32] });
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
fn production_cli_client_transmits_cbor_submit_over_the_socket() {
    let f = Fixture::new(false, Config::default());
    let executable = env!("CARGO_BIN_EXE_blaze-dist-local");
    let mut child = std::process::Command::new(executable)
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
fn production_cli_server_owns_and_removes_its_actual_socket() {
    let path = PathBuf::from(format!(
        "/tmp/blaze-cli-{}.sock",
        random_id().unwrap()[..8].iter().map(|b| format!("{b:02x}")).collect::<String>()
    ));
    let executable = env!("CARGO_BIN_EXE_blaze-dist-local");
    let mut child = std::process::Command::new(executable)
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
