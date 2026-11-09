//! The agent over the real carrier and the real cache server, with a batch compiler standing in
//! for the code generator: what a daemon's unit does, without a front end.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use blaze_dist::admission::{Admission, AdmissionConfig};
use blaze_dist::holder::HolderConfig;
use blaze_dist::jobs::{CompletionMode, ExecutionConfig, Jobs};
use blaze_dist::work::WorkConfig;
use blaze_dist_local::Error;
use blaze_dist_local::agent::{self, Grant, Granted, NEED, Source, Unit};
use blaze_dist_local::blobs::Blobs;
use blaze_dist_local::client::Client;
use blaze_dist_local::coordinator::Config as CarrierConfig;
use blaze_dist_local::pack::{Pack, Value};
use blaze_dist_local::protocol::{Answer, Command, submission};
use blaze_dist_local::server::{MonotonicClock, Server, random_id};
use blaze_wire::{Digest, LocalPolicy, Namespace, Reservation};

const TRIPLE: &str = "test-triple";

struct World {
    socket: PathBuf,
    blobs: Blobs,
    stop: Arc<AtomicBool>,
    _dir: tempfile::TempDir,
    _runtime: tokio::runtime::Runtime,
}

impl Drop for World {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        let _ = std::fs::remove_file(&self.socket);
    }
}

fn world(executors: u32, work: WorkConfig) -> World {
    world_with(executors, work, CarrierConfig::default())
}

fn world_with(executors: u32, work: WorkConfig, carrier: CarrierConfig) -> World {
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("blobs");
    std::fs::create_dir_all(&store).unwrap();
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let address = runtime.block_on(async {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let config = blaze_serve::Config::at(store, address.port());
        let server = Arc::new(blaze_serve::Server::open(&config).unwrap());
        tokio::spawn(blaze_serve::serve_on(listener, server));
        address
    });
    let mut admission = Admission::new(AdmissionConfig::default());
    admission.add_machine(
        "machine",
        agent::platform(TRIPLE),
        Reservation { memory: u64::from(executors), cpus: executors, disk: u64::from(executors) },
    );
    for _ in 0..executors {
        admission.add_builder(0, "machine", NEED).unwrap();
    }
    let jobs = Jobs::new(
        admission,
        ExecutionConfig { work, completion: CompletionMode::PublishedWork, ..Default::default() },
    );
    let tag: String = random_id().unwrap()[..6].iter().map(|b| format!("{b:02x}")).collect();
    let socket = PathBuf::from(format!("/tmp/blaze-agent-{tag}.sock"));
    let server =
        Server::bind(&socket, jobs, carrier, Arc::new(MonotonicClock::default()), |_: Vec<_>| {})
            .unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let signal = stop.clone();
    std::thread::spawn(move || server.run(signal).unwrap());
    World {
        socket,
        blobs: Blobs { address, timeout: Duration::from_secs(30) },
        stop,
        _dir: dir,
        _runtime: runtime,
    }
}

fn config(socket: &Path, blobs: &Blobs) -> agent::Config {
    agent::Config {
        socket: socket.to_owned(),
        blobs: blobs.clone(),
        claimant: random_id().unwrap(),
        platform: agent::platform(TRIPLE),
        local: LocalPolicy::Never,
        namespace: Namespace::Main,
        source: [0; 32],
        deadline: Duration::from_secs(120),
        holder: HolderConfig { heartbeat_ms: 50, retry_ms: 500, idle_ms: 5 },
        poll: Duration::from_millis(5),
    }
}

fn unit(tag: u8, count: u8) -> Unit {
    Unit { spec: [tag; 32], names: (0..count).map(|at| [at; 32]).collect() }
}

fn value_of(name: &Digest) -> Value {
    Value { key: *name, kind: 1, bytes: vec![name[0]; 16] }
}

/// Compiles what it is granted, slowly enough for another executor to come, and says what.
#[derive(Clone)]
struct Compiler {
    names: Vec<Digest>,
    log: Arc<Mutex<BTreeMap<usize, usize>>>,
    who: usize,
    entered: Arc<AtomicUsize>,
    wait_for: usize,
}

impl Granted for Compiler {
    fn compile(&mut self, names: &[usize]) -> Result<Grant, String> {
        self.entered.fetch_add(1, Ordering::SeqCst);
        // Each executor's first grant waits for the other's, so the split is not a race.
        for _ in 0..2_000 {
            if self.entered.load(Ordering::SeqCst) >= self.wait_for {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        let mut log = self.log.lock().unwrap();
        for at in names {
            assert!(log.insert(*at, self.who).is_none(), "{at} was compiled twice");
        }
        Ok(Grant {
            values: names.iter().map(|at| value_of(&self.names[*at])).collect(),
            compiled: names.len() as u64,
            stored: 0,
            compiled_keys: names.iter().map(|at| self.names[*at]).collect(),
        })
    }
}

#[test]
fn two_agents_split_a_unit_and_both_get_all_of_it() {
    let world = world(2, WorkConfig { grant_roots: 1, ..Default::default() });
    let unit = unit(1, 12);
    let log = Arc::new(Mutex::new(BTreeMap::new()));
    let entered = Arc::new(AtomicUsize::new(0));
    let never = AtomicBool::new(false);
    let arrived: Vec<_> = std::thread::scope(|scope| {
        let threads: Vec<_> = (0..2)
            .map(|who| {
                let (world, unit, never) = (&world, &unit, &never);
                let mut compiler = Compiler {
                    names: unit.names.clone(),
                    log: log.clone(),
                    who,
                    entered: entered.clone(),
                    wait_for: 2,
                };
                scope.spawn(move || {
                    agent::run(&config(&world.socket, &world.blobs), unit, &mut compiler, never)
                        .unwrap()
                })
            })
            .collect();
        threads.into_iter().map(|it| it.join().unwrap()).collect()
    });
    let log = log.lock().unwrap();
    assert_eq!(log.len(), 12, "every function was compiled, once");
    let workers: BTreeSet<usize> = log.values().copied().collect();
    assert_eq!(workers.len(), 2, "both executors compiled: {log:?}");
    for it in &arrived {
        assert_eq!(it.source, Source::Executed);
        assert_eq!(it.values.len(), 12, "each got the values of both");
        for name in &unit.names {
            assert_eq!(it.values[name], (1, vec![name[0]; 16]));
        }
    }
    assert_eq!(arrived[0].execution, arrived[1].execution, "one execution of one unit");
}

/// A compiler that takes one grant and then its process is gone: its result never comes.
struct Vanishes;

impl Granted for Vanishes {
    fn compile(&mut self, _: &[usize]) -> Result<Grant, String> {
        Err("this executor's compile failed".to_owned())
    }
}

#[test]
fn a_batch_given_back_leaves_its_names_to_the_build() {
    let world = world(1, WorkConfig { grant_roots: 4, ..Default::default() });
    let unit = unit(2, 4);
    let never = AtomicBool::new(false);
    let arrived =
        agent::run(&config(&world.socket, &world.blobs), &unit, &mut Vanishes, &never).unwrap();
    assert!(arrived.values.is_empty(), "nothing was committed: the final build compiles them");
}

#[test]
fn a_unit_with_no_names_still_has_one_tail_owner() {
    let world = world(1, WorkConfig::default());
    let unit = Unit { spec: [31; 32], names: Vec::new() };
    let never = AtomicBool::new(false);
    let arrived =
        agent::run(&config(&world.socket, &world.blobs), &unit, &mut Vanishes, &never).unwrap();
    assert!(arrived.values.is_empty());
    assert_eq!(arrived.source, Source::Executed);
    assert!(arrived.execution.is_some());
}

#[test]
fn a_published_unit_schedules_nothing() {
    let world = world(1, WorkConfig::default());
    let unit = unit(3, 3);
    let never = AtomicBool::new(false);
    let mut compiler = Compiler {
        names: unit.names.clone(),
        log: Arc::new(Mutex::new(BTreeMap::new())),
        who: 0,
        entered: Arc::new(AtomicUsize::new(0)),
        wait_for: 1,
    };
    let extra = Value { key: [200; 32], kind: 2, bytes: b"a static".to_vec() };
    let mut complete = WithTail { compiler: &mut compiler, extra: extra.clone() };
    let first =
        agent::run(&config(&world.socket, &world.blobs), &unit, &mut complete, &never).unwrap();
    assert_eq!(first.compiled_here, 3);

    // No coordinator is reachable at this socket: a published unit never asks it.
    let unreachable = Path::new("/tmp/blaze-agent-nothing-here.sock");
    let second =
        agent::run(&config(unreachable, &world.blobs), &unit, &mut compiler, &never).unwrap();
    assert_eq!(second.source, Source::Published);
    assert_eq!(second.compiled_here, 0);
    assert_eq!(second.values.len(), 4, "the work and what the build made beyond it");
    assert_eq!(second.values[&extra.key], (2, extra.bytes));
}

#[test]
fn a_committed_pack_the_server_lost_refuses_without_compiling() {
    let world = world(1, WorkConfig::default());
    let unit = unit(4, 2);
    let never = AtomicBool::new(false);
    let pack = Pack::of(vec![value_of(&[9; 32])]).unwrap();
    world.blobs.put_pack(&pack).unwrap();
    let list = blaze_wire::pack_list::encode(&[pack.reference.clone()]);
    world.blobs.publish(&unit.spec, &list).unwrap();
    let path = world._dir.path().join("blobs/packs")
        .join(format!("{}.pack", blaze_dist_local::pack::hex(&pack.reference.pack)));
    std::fs::remove_file(&path).unwrap();
    let mut compiler = Compiler {
        names: unit.names.clone(),
        log: Arc::new(Mutex::new(BTreeMap::new())),
        who: 0,
        entered: Arc::new(AtomicUsize::new(0)),
        wait_for: 1,
    };
    for policy in [LocalPolicy::Never, LocalPolicy::Fallback] {
        let error = agent::run(&config(&world.socket, &world.blobs), &unit, &mut compiler, &never)
            .unwrap_err();
        assert!(matches!(&error, Error::Committed(why)
            if why.contains(&blaze_dist_local::pack::hex(&pack.reference.pack))), "{error}");
        assert!(agent::decide(&policy, error).is_err());
    }
    assert_eq!(compiler.entered.load(Ordering::SeqCst), 0, "a committed absence must not compile");
    assert!(compiler.log.lock().unwrap().is_empty());
    assert_eq!(world.blobs.published(&unit.spec).unwrap().unwrap(), list);
    // Remove only the fault: the same manifest is readable again without any compilation.
    world.blobs.put_pack(&pack).unwrap();
    let restored = agent::run(&config(&world.socket, &world.blobs), &unit, &mut compiler, &never)
        .unwrap();
    assert_eq!(restored.source, Source::Published);
    assert_eq!(compiler.entered.load(Ordering::SeqCst), 0);
}

#[test]
fn a_slow_phase_pack_download_keeps_the_original_lead_alive() {
    use std::io::{Read as _, Write as _};
    use std::net::{TcpListener, TcpStream};

    let world = world(1, WorkConfig::default());
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let address = listener.local_addr().unwrap();
    let defaults = ExecutionConfig::default();
    let delay = Duration::from_millis(defaults.heartbeat_ms * defaults.missed_heartbeats + 500);
    let delayed = AtomicBool::new(false);
    let unit = unit(44, 2);
    let mut compiler = Compiler {
        names: unit.names.clone(),
        log: Arc::new(Mutex::new(BTreeMap::new())),
        who: 0,
        entered: Arc::new(AtomicUsize::new(0)),
        wait_for: 1,
    };
    let mut cfg = config(&world.socket, &world.blobs);
    cfg.blobs.address = address;
    let result = std::thread::scope(|scope| {
        let proxy = scope.spawn(|| {
            let bound = std::time::Instant::now() + Duration::from_secs(120);
            while !world.stop.load(Ordering::Acquire) && std::time::Instant::now() < bound {
                let (mut client, _) = match listener.accept() {
                    Ok(value) => value,
                    Err(why) if why.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(1));
                        continue;
                    }
                    Err(why) => panic!("proxy accept: {why}"),
                };
                client.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
                client.set_write_timeout(Some(Duration::from_secs(30))).unwrap();
                let mut request = Vec::new();
                let mut buffer = [0; 4096];
                let end = loop {
                    let n = client.read(&mut buffer).unwrap();
                    assert_ne!(n, 0);
                    request.extend_from_slice(&buffer[..n]);
                    assert!(request.len() < 1 << 20);
                    if let Some(at) = request.windows(4).position(|bytes| bytes == b"\r\n\r\n") {
                        break at + 4;
                    }
                };
                let headers = std::str::from_utf8(&request[..end]).unwrap();
                let length: usize = headers
                    .lines()
                    .filter_map(|line| line.split_once(':'))
                    .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
                    .map_or(0, |(_, value)| value.trim().parse().unwrap());
                assert!(end + length < 1 << 20);
                while request.len() < end + length {
                    let n = client.read(&mut buffer).unwrap();
                    assert_ne!(n, 0);
                    request.extend_from_slice(&buffer[..n]);
                }
                if request.starts_with(b"GET /v1/blob/packs/")
                    && !delayed.swap(true, Ordering::AcqRel)
                {
                    // Cache IO is held beyond the actual default lead-loss window;
                    // the independent Unix coordinator remains available to heartbeats.
                    std::thread::sleep(delay);
                }
                let mut upstream = TcpStream::connect(world.blobs.address).unwrap();
                upstream.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
                upstream.set_write_timeout(Some(Duration::from_secs(30))).unwrap();
                upstream.write_all(&request).unwrap();
                std::io::copy(&mut upstream, &mut client).unwrap();
            }
        });
        let result = agent::run(&cfg, &unit, &mut compiler, &AtomicBool::new(false));
        world.stop.store(true, Ordering::Release);
        proxy.join().unwrap();
        result
    })
    .unwrap();
    assert!(delayed.load(Ordering::Acquire), "the test must delay an actual phase pack GET");
    assert_eq!(result.attempt, Some(1), "slow cache IO must not reassign a healthy lead");
    assert_eq!(result.publication_attempt, Some(1));
    assert_eq!(result.values.len(), 2);
}

#[test]
fn a_first_writer_accepts_equal_outputs_in_a_different_pack_partition() {
    // A holder may export verified neighboring values alongside a grant. Both pack
    // partitions below contain exactly the same logical values for the same unit.
    struct WithNeighbors(Vec<Digest>);
    impl Granted for WithNeighbors {
        fn compile(&mut self, names: &[usize]) -> Result<Grant, String> {
            Ok(Grant {
                values: self.0.iter().map(value_of).collect(),
                compiled: names.len() as u64,
                stored: 0,
                compiled_keys: names.iter().map(|at| self.0[*at]).collect(),
            })
        }
    }
    let world = world(1, WorkConfig { grant_roots: 1, ..Default::default() });
    let unit = unit(45, 2);
    let never = AtomicBool::new(false);
    let cfg = config(&world.socket, &world.blobs);
    let first = agent::run(&cfg, &unit, &mut WithNeighbors(unit.names.clone()), &never).unwrap();
    assert_eq!(first.packs.len(), 1);
    let old = world.blobs.published(&unit.spec).unwrap().unwrap();
    let mut split = Vec::new();
    for name in &unit.names {
        let pack = Pack::of(vec![value_of(name)]).unwrap();
        world.blobs.put_pack(&pack).unwrap();
        split.push(pack.reference);
    }
    split.sort();
    let proposed = blaze_wire::pack_list::encode(&split);
    assert_ne!(proposed, old, "the first-writer check must compare actual different partitions");
    world.blobs.publish(&unit.spec, &proposed).unwrap();
    assert_eq!(world.blobs.published(&unit.spec).unwrap().unwrap(), old, "the first writer remains immutable");
    assert_eq!(agent::import(&world.blobs, &split).unwrap(), first.values);

    // Same key sets cannot hide a different value or kind behind a different partition.
    for kind in [1, 2] {
        let mut values: Vec<_> = unit.names.iter().map(value_of).collect();
        values[0].kind = kind;
        if kind == 1 {
            values[0].bytes.push(99);
        }
        let wrong = Pack::of(values).unwrap();
        world.blobs.put_pack(&wrong).unwrap();
        let refused = world.blobs.publish(&unit.spec,
            &blaze_wire::pack_list::encode(&[wrong.reference])).unwrap_err();
        assert!(matches!(agent::decide(&LocalPolicy::Fallback, refused), Err(Error::Committed(_))));
    }
    let mut cold = cfg;
    cold.socket = PathBuf::from("/tmp/blaze-agent-no-coordinator-for-existing-list.sock");
    let restored = agent::run(&cold, &unit, &mut Vanishes, &never).unwrap();
    assert_eq!(restored.source, Source::Published);
    assert_eq!(restored.values, first.values);
}

#[test]
fn an_already_cancelled_build_never_submits_an_execution() {
    let world = world(0, WorkConfig::default());
    let unit = unit(5, 2);
    let config = config(&world.socket, &world.blobs);
    let job = |unit: Digest| blaze_wire::JobRequest {
        unit,
        platform: config.platform,
        namespace: Namespace::Main,
        source: [0; 32],
        deadline_ms: 1_000_000,
        local: LocalPolicy::Never,
    };
    let mut client = Client::connect(&world.socket, random_id().unwrap()).unwrap();
    let mut execution_of = |unit: Digest| {
        let Answer::Binding(binding) =
            client.call(Command::Submit(submission(&job(unit), NEED))).unwrap()
        else {
            panic!("the unit was refused");
        };
        binding.subscription.execution.0
    };
    // Executions are numbered in order. No executor runs them, so one that stays stays running.
    let before = execution_of([200; 32]);
    let cancelled = AtomicBool::new(true);
    let error = agent::run(&config, &unit, &mut Vanishes, &cancelled).unwrap_err();
    assert!(error.to_string().contains("cancelled"), "{error}");
    assert!(matches!(agent::decide(&LocalPolicy::Fallback, error), Err(Error::Cancelled(_))));
    // Cancellation is checked before cache access or submission, so there is no intervening job.
    assert_eq!(execution_of(unit.spec), before + 1);
}

#[test]
fn an_execution_that_ended_says_why_and_the_requester_goes_by_its_policy() {
    // No builder ever runs the job: the subscriber's own deadline ends it `TimedOut`, which is
    // no verdict, so `Fallback` compiles here and `Never` stops the build.
    let world = world(0, WorkConfig::default());
    let unit = unit(6, 2);
    let never = AtomicBool::new(false);
    let mut config = config(&world.socket, &world.blobs);
    config.deadline = Duration::from_millis(50);
    let failure = agent::run(&config, &unit, &mut Vanishes, &never).unwrap_err();
    let Error::Ended(termination) = &failure else { panic!("{failure}") };
    assert_eq!(termination.outcome, blaze_wire::Outcome::TimedOut);
    assert!(agent::decide(&LocalPolicy::Fallback, Error::Ended(termination.clone())).is_ok());
    assert!(agent::decide(&LocalPolicy::Never, failure).is_err());
}

#[test]
fn a_verdict_is_taken_under_every_policy() {
    use blaze_wire::job_terminal_v2::{TerminationReasonV2, TerminationV2, VERSION};
    let reported = TerminationV2 {
        version: VERSION,
        outcome: blaze_wire::Outcome::Failed,
        reason: TerminationReasonV2::Reported,
    };
    for policy in [LocalPolicy::Never, LocalPolicy::Fallback] {
        assert!(agent::decide(&policy, Error::Ended(reported.clone())).is_err());
    }
}

#[test]
fn a_coordinator_that_is_gone_stops_the_build_unless_it_may_compile_here() {
    let world = world(1, WorkConfig::default());
    let unit = unit(7, 2);
    let never = AtomicBool::new(false);
    let unreachable = Path::new("/tmp/blaze-agent-nothing-here.sock");
    let failure =
        agent::run(&config(unreachable, &world.blobs), &unit, &mut Vanishes, &never).unwrap_err();
    assert!(matches!(failure, Error::Io(_)), "{failure}");
    assert!(agent::decide(&LocalPolicy::Fallback, failure).is_ok());
    let failure =
        agent::run(&config(unreachable, &world.blobs), &unit, &mut Vanishes, &never).unwrap_err();
    assert!(agent::decide(&LocalPolicy::Never, failure).is_err());
}

/// A pull request's unit and the main line's run apart, and what either published is the other's.
#[test]
fn a_unit_published_by_one_pull_request_is_taken_by_another() {
    let world = world(2, WorkConfig::default());
    let unit = unit(8, 3);
    let never = AtomicBool::new(false);
    let log = Arc::new(Mutex::new(BTreeMap::new()));
    let mut compiler = Compiler {
        names: unit.names.clone(),
        log,
        who: 0,
        entered: Arc::new(AtomicUsize::new(0)),
        wait_for: 1,
    };
    let of_pr = |n: u64, source: u8| {
        let mut config = config(&world.socket, &world.blobs);
        config.namespace = Namespace::Pr(blaze_wire::PrNamespace { n });
        config.source = [source; 32];
        config
    };
    let first = agent::run(&of_pr(1, 1), &unit, &mut compiler, &never).unwrap();
    assert_eq!(first.source, Source::Executed);
    assert_eq!(first.compiled_here, 3);
    let second = agent::run(&of_pr(2, 9), &unit, &mut compiler, &never).unwrap();
    assert_eq!(second.source, Source::Published, "another pull request schedules nothing");
    assert_eq!(second.compiled_here, 0);
    assert_eq!(second.values.len(), 3);
}

/// A unit of more functions than the carrier's own default allows is one a coordinator with the
/// bounds of a build (`blaze-dist-local coordinate`) runs; with the defaults its join waits.
#[test]
fn a_unit_of_many_functions_is_run_under_a_builds_bounds() {
    let carrier =
        CarrierConfig { max_roots: 1 << 24, max_grants: 1 << 20, ..CarrierConfig::default() };
    let world = world_with(1, WorkConfig { grant_roots: 1024, ..Default::default() }, carrier);
    let names: Vec<Digest> = (0..5_000u32)
        .map(|at| {
            let mut it = [0; 32];
            it[..4].copy_from_slice(&at.to_be_bytes());
            it[4] = 0x5a;
            it
        })
        .collect();
    let unit = Unit { spec: [33; 32], names: names.clone() };
    let never = AtomicBool::new(false);
    let mut compiler = Compiler {
        names,
        log: Arc::new(Mutex::new(BTreeMap::new())),
        who: 0,
        entered: Arc::new(AtomicUsize::new(0)),
        wait_for: 1,
    };
    let arrived =
        agent::run(&config(&world.socket, &world.blobs), &unit, &mut compiler, &never).unwrap();
    assert_eq!(arrived.values.len(), 5_000);
    assert_eq!(arrived.compiled_here, 5_000);
}

/// An agent and its holder ask again and again in one session each, a grant of one name at a
/// time here: every answer but the last is let go, so no number of asks fills a session.
#[test]
fn an_agent_that_waits_asks_in_one_session_however_often() {
    let carrier = CarrierConfig { max_sessions: 4, ..CarrierConfig::default() };
    let world = world_with(1, WorkConfig { grant_roots: 1, ..Default::default() }, carrier);
    let unit = unit(34, 12);
    let never = AtomicBool::new(false);
    let mut compiler = Compiler {
        names: unit.names.clone(),
        log: Arc::new(Mutex::new(BTreeMap::new())),
        who: 0,
        entered: Arc::new(AtomicUsize::new(0)),
        wait_for: 1,
    };
    let arrived =
        agent::run(&config(&world.socket, &world.blobs), &unit, &mut compiler, &never).unwrap();
    assert_eq!(arrived.values.len(), 12);
}

/// A warm local tail hit still belongs to the manifest sent to an empty subscriber.
struct WithTail<'a> {
    compiler: &'a mut Compiler,
    extra: Value,
}

impl Granted for WithTail<'_> {
    fn compile(&mut self, names: &[usize]) -> Result<Grant, String> {
        self.compiler.compile(names)
    }

    fn complete(&mut self, _: &BTreeMap<Digest, (u8, Vec<u8>)>) -> Result<Vec<Value>, String> {
        Ok(vec![self.extra.clone()])
    }
}

#[test]
fn a_late_subscriber_waits_for_the_one_leads_full_tail() {
    use std::sync::mpsc;

    struct Paused {
        entered: mpsc::Sender<()>,
        release: mpsc::Receiver<()>,
        tails: Arc<AtomicUsize>,
        extra: Value,
    }
    impl Granted for Paused {
        fn compile(&mut self, _: &[usize]) -> Result<Grant, String> {
            panic!("empty work set")
        }
        fn complete(&mut self, _: &BTreeMap<Digest, (u8, Vec<u8>)>) -> Result<Vec<Value>, String> {
            assert_eq!(self.tails.fetch_add(1, Ordering::SeqCst), 0);
            self.entered.send(()).unwrap();
            self.release.recv_timeout(Duration::from_secs(20)).unwrap();
            Ok(vec![self.extra.clone()])
        }
    }
    struct Reader;
    impl Granted for Reader {
        fn compile(&mut self, _: &[usize]) -> Result<Grant, String> {
            panic!("late subscriber compiled")
        }
        fn complete(&mut self, _: &BTreeMap<Digest, (u8, Vec<u8>)>) -> Result<Vec<Value>, String> {
            panic!("late subscriber ran a second tail")
        }
    }
    let world = world(2, WorkConfig::default());
    let unit = Unit { spec: [82; 32], names: Vec::new() };
    let extra = value_of(&[83; 32]);
    let tails = Arc::new(AtomicUsize::new(0));
    let (entered, completing) = mpsc::channel();
    let (release, gate) = mpsc::channel();
    let never = AtomicBool::new(false);
    std::thread::scope(|scope| {
        let (world, unit, never) = (&world, &unit, &never);
        let mut compiler =
            Paused { entered, release: gate, tails: tails.clone(), extra: extra.clone() };
        let lead = scope.spawn(move || {
            agent::run(&config(&world.socket, &world.blobs), unit, &mut compiler, never).unwrap()
        });
        completing.recv_timeout(Duration::from_secs(20)).unwrap();
        // Force a submission in the formerly racy interval: phase done, tail still paused.
        let mut observer = Client::connect(&world.socket, random_id().unwrap()).unwrap();
        let request = blaze_wire::JobRequest {
            unit: unit.spec,
            platform: agent::platform(TRIPLE),
            namespace: Namespace::Main,
            source: [0; 32],
            deadline_ms: observer.welcome.now_ms + 120_000,
            local: LocalPolicy::Never,
        };
        let Answer::Binding(joined) =
            observer.call(Command::Submit(submission(&request, NEED))).unwrap()
        else {
            panic!()
        };
        let reader = scope.spawn(move || {
            agent::run(&config(&world.socket, &world.blobs), unit, &mut Reader, never).unwrap()
        });
        release.send(()).unwrap();
        let first = lead.join().unwrap();
        let second = reader.join().unwrap();
        assert_eq!(first.execution, Some(joined.subscription.execution.0));
        assert_eq!(second.values, first.values);
        assert_eq!(second.values.get(&extra.key), Some(&(extra.kind, extra.bytes.clone())));
        assert_eq!(second.compiled_here, 0);
        assert_eq!(tails.load(Ordering::SeqCst), 1);
        let _ = observer.release();
    });
}
