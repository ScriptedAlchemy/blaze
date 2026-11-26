//! The agent over the real carrier and the real cache server, with a batch compiler standing in
//! for the code generator: what a daemon's unit does, without a front end.

#![no_std]

use core::convert::Infallible;
use core::fmt;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use core::time::Duration;

use blaze_dist::admission::{Admission, AdmissionConfig, ExecutorPhase, Waiting};
use blaze_dist::holder::HolderConfig;
use blaze_dist::jobs::{CompletionMode, ExecutionConfig, Jobs};
use blaze_dist::work::WorkConfig;
use blaze_dist_local::Error;
use blaze_dist_local::agent::{self, Emission, Grant, Granted, NEED, Source, Unit};
use blaze_dist_local::blobs::Blobs;
use blaze_dist_local::client::Client;
use blaze_dist_local::coordinator::Config as CarrierConfig;
use blaze_dist_local::pack::{Pack, Value, ValueRef};
use blaze_dist_local::protocol::{Answer, Command, submission};
use blaze_dist_local::server::{MonotonicClock, Server, random_id};
use blaze_dist_local::sys::{
    JoinHandle, Mutex, Receiver, Sender, SystemClock, TcpListener, TcpStream, TempDir, channel,
    create_dir_all, scope, sleep, sleep_ms, spawn, unlink_path,
};
use blaze_own::{Arc, BTreeMap, BTreeSet, Vec, format_in};
use blaze_wire::{
    Attempt, Digest, ExecutionId, LocalPolicy, Namespace, PrNamespace, Reservation, WireAlloc,
    WireString, WireVec,
};

const TRIPLE: &str = "test-triple";

fn test_alloc() -> WireAlloc {
    blaze_own::testing::heap().shared()
}

#[derive(Debug)]
struct TestError(WireString);

impl fmt::Display for TestError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0.as_str())
    }
}

impl core::error::Error for TestError {}

struct World {
    socket: WireString,
    blobs: Blobs,
    stop: Arc<AtomicBool, WireAlloc>,
    serve_stop: blaze_serve::ShutdownHandle,
    coord_thread: Option<JoinHandle<'static, ()>>,
    serve_thread: Option<JoinHandle<'static, ()>>,
    dir: TempDir,
    alloc: WireAlloc,
}

impl Drop for World {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        self.serve_stop.stop();
        if let Some(thread) = self.coord_thread.take() {
            let _ = thread.join();
        }
        if let Some(thread) = self.serve_thread.take() {
            let _ = thread.join();
        }
        let _ = unlink_path(self.socket.as_str());
    }
}

fn world(executors: u32, work: WorkConfig) -> World {
    let alloc = test_alloc();
    let mut namespaces = Vec::with_capacity_in(usize::try_from(executors).unwrap(), alloc);
    for _ in 0..executors {
        namespaces.push(Namespace::Main);
    }
    world_with(&namespaces, work, CarrierConfig::default())
}

fn world_with(namespaces: &[Namespace], work: WorkConfig, carrier: CarrierConfig) -> World {
    let alloc = test_alloc();
    let dir = TempDir::new("ba", alloc).unwrap();
    let store = dir.join("blobs");
    create_dir_all(store.as_str()).unwrap();
    let serve_config = blaze_serve::Config::at(store.as_str(), 0, alloc);
    let serve = blaze_serve::Server::bind(serve_config, alloc).unwrap();
    let address = serve.local_addr();
    let serve_stop = serve.shutdown_handle().unwrap();
    let mut admission = Admission::new(AdmissionConfig::default(), alloc);
    let executors = u32::try_from(namespaces.len()).unwrap();
    let capacity = Reservation {
        memory: u64::from(executors),
        cpus: executors,
        disk: u64::from(executors),
    };
    admission.add_machine("machine", agent::platform(TRIPLE), capacity.clone());
    let mut slots = Vec::with_capacity_in(namespaces.len(), alloc);
    for (at, namespace) in namespaces.iter().enumerate() {
        slots.push(match namespace {
            Namespace::Main => (admission.add_builder(0, "machine", NEED, alloc).unwrap(), None),
            Namespace::Pr(_) => {
                // A PR slot is created by real admission and kept idle after its setup job.
                // Reserving every slot for Main would leave this namespace waiting forever.
                let execution = ExecutionId(u64::try_from(at).unwrap() + 1);
                admission
                    .enqueue(Waiting {
                        execution,
                        namespace: namespace.clone(),
                        platform: agent::platform(TRIPLE),
                        need: NEED,
                        at: 0,
                    })
                    .unwrap();
                let mut admitted = admission.admit(0, alloc);
                assert_eq!(admitted.len(), 1, "the fixture's PR slot must fit its machine");
                let admitted = admitted.pop().unwrap();
                assert!(admitted.created);
                assert_eq!(admitted.execution, execution);
                assert_eq!(&admitted.namespace, namespace);
                // Keep this slot occupied until every requested slot exists. Admission
                // otherwise reuses it for the next job in the same PR namespace.
                (admitted.executor, Some(execution))
            }
        });
    }
    for ((executor, setup), namespace) in slots.into_iter().zip(namespaces) {
        if let Some(execution) = setup {
            admission.finished(0, &executor, execution, Attempt(1));
        }
        let slot = admission.executor(&executor).unwrap();
        assert_eq!(&slot.namespace, namespace);
        assert_eq!(slot.phase, ExecutorPhase::Ready);
        assert_eq!(slot.busy, None);
        assert_eq!(slot.reservation, NEED);
    }
    assert_eq!(admission.used("machine"), capacity);
    admission.check().unwrap();
    let jobs = Jobs::new(
        admission,
        ExecutionConfig { work, completion: CompletionMode::PublishedWork, ..Default::default() },
        alloc,
    );
    let socket = dir.join("a.sock");
    let server = Server::bind(
        socket.as_str(),
        jobs,
        carrier,
        Arc::new_in(MonotonicClock::default(), alloc),
        |_: &mut dyn Iterator<Item = (blaze_wire::ExecutionId, blaze_dist::jobs::Effect)>| {},
        alloc,
    )
    .unwrap();
    let stop = Arc::new_in(AtomicBool::new(false), alloc);
    let signal = stop.clone();
    let mut world = World {
        socket,
        blobs: Blobs { address, timeout: Duration::from_secs(30), alloc },
        stop,
        serve_stop,
        coord_thread: None,
        serve_thread: None,
        dir,
        alloc,
    };
    world.serve_thread = Some(spawn(move || serve.run().unwrap(), alloc).unwrap());
    world.coord_thread = Some(spawn(move || server.run(&signal).unwrap(), alloc).unwrap());
    world
}

fn config(socket: &str, blobs: &Blobs) -> agent::Config {
    let alloc = blobs.alloc;
    agent::Config {
        socket: WireString::from_str_in(socket, alloc),
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

fn unit(tag: u8, count: u8, alloc: WireAlloc) -> Unit {
    let mut names = Vec::with_capacity_in(usize::from(count), alloc);
    for at in 0..count {
        names.push([at; 32]);
    }
    Unit { spec: [tag; 32], names }
}

fn value_of(name: &Digest, alloc: WireAlloc) -> Value {
    let mut bytes = Vec::with_capacity_in(16, alloc);
    bytes.resize(16, name[0]);
    Value { key: *name, kind: 1, bytes }
}

/// Compiles what it is granted, slowly enough for another executor to come, and says what.
#[derive(Clone)]
struct Compiler {
    names: WireVec<Digest>,
    log: Arc<Mutex<BTreeMap<usize, usize, WireAlloc>>, WireAlloc>,
    who: usize,
    entered: Arc<AtomicUsize, WireAlloc>,
    wait_for: usize,
    alloc: WireAlloc,
}

impl Granted for Compiler {
    type Error = Infallible;

    fn compile(&mut self, names: &[usize]) -> Result<Grant, Self::Error> {
        let alloc = self.alloc;
        self.entered.fetch_add(1, Ordering::SeqCst);
        // Each executor's first grant waits for the other's, so the split is not a race.
        for _ in 0..2_000 {
            if self.entered.load(Ordering::SeqCst) >= self.wait_for {
                break;
            }
            sleep(Duration::from_millis(5));
        }
        let mut log = self.log.lock();
        for at in names {
            assert!(log.insert(*at, self.who).is_none(), "{at} was compiled twice");
        }
        let mut values = Vec::with_capacity_in(names.len(), alloc);
        let mut emissions = Vec::with_capacity_in(names.len(), alloc);
        for at in names {
            values.push(value_of(&self.names[*at], alloc));
            emissions.push(Emission { key: self.names[*at], backend: None, native: None });
        }
        Ok(Grant {
            values,
            compiled: u64::try_from(names.len()).unwrap(),
            stored: 0,
            emissions,
        })
    }
}

#[test]
fn two_agents_split_a_unit_and_both_get_all_of_it() {
    let world = world(2, WorkConfig { grant_roots: 1, ..Default::default() });
    let alloc = world.alloc;
    let unit = unit(1, 12, alloc);
    let log = Arc::new_in(Mutex::new(BTreeMap::new_in(alloc)), alloc);
    let entered = Arc::new_in(AtomicUsize::new(0), alloc);
    let never = AtomicBool::new(false);
    let arrived: WireVec<_> = scope(alloc, |s| {
        let mut threads = Vec::with_capacity_in(2, alloc);
        for who in 0..2 {
            let (world, unit, never) = (&world, &unit, &never);
            let mut compiler = Compiler {
                names: unit.names.clone(),
                log: log.clone(),
                who,
                entered: entered.clone(),
                wait_for: 2,
                alloc,
            };
            threads.push(
                s.spawn(move || {
                    agent::run(&config(world.socket.as_str(), &world.blobs), unit, &mut compiler, never)
                        .unwrap()
                })
                .unwrap(),
            );
        }
        let mut out = Vec::with_capacity_in(threads.len(), alloc);
        for thread in threads {
            out.push(thread.join().unwrap());
        }
        out
    });
    let log = log.lock();
    assert_eq!(log.len(), 12, "every function was compiled, once");
    let mut workers = BTreeSet::new_in(alloc);
    workers.extend(log.values().copied());
    assert_eq!(workers.len(), 2, "both executors compiled: {log:?}");
    for it in &arrived {
        assert_eq!(it.source, Source::Executed);
        assert_eq!(it.values.len(), 12, "each got the values of both");
        for name in &unit.names {
            let mut expected = Vec::with_capacity_in(16, alloc);
            expected.resize(16, name[0]);
            assert_eq!(it.values[name], (1, expected));
        }
    }
    assert_eq!(arrived[0].execution, arrived[1].execution, "one execution of one unit");
}

/// A compiler that takes one grant and then its process is gone: its result never comes.
struct Vanishes(WireAlloc);

impl Granted for Vanishes {
    type Error = TestError;

    fn compile(&mut self, _: &[usize]) -> Result<Grant, Self::Error> {
        Err(TestError(WireString::from_str_in("this executor's compile failed", self.0)))
    }
}

#[test]
fn a_batch_given_back_leaves_its_names_to_the_build() {
    let world = world(1, WorkConfig { grant_roots: 4, ..Default::default() });
    let alloc = world.alloc;
    let unit = unit(2, 4, alloc);
    let never = AtomicBool::new(false);
    let mut vanishes = Vanishes(alloc);
    let arrived = agent::run(
        &config(world.socket.as_str(), &world.blobs),
        &unit,
        &mut vanishes as &mut dyn Granted<Error = TestError>,
        &never,
    )
    .unwrap();
    assert!(arrived.values.is_empty(), "nothing was committed: the final build compiles them");
}

#[test]
fn a_unit_with_no_names_still_has_one_tail_owner() {
    let world = world(1, WorkConfig::default());
    let alloc = world.alloc;
    let unit = Unit { spec: [31; 32], names: Vec::new_in(alloc) };
    let never = AtomicBool::new(false);
    let arrived = agent::run(
        &config(world.socket.as_str(), &world.blobs),
        &unit,
        &mut Vanishes(alloc),
        &never,
    )
    .unwrap();
    assert!(arrived.values.is_empty());
    assert_eq!(arrived.source, Source::Executed);
    assert!(arrived.execution.is_some());
}

#[test]
fn a_published_unit_schedules_nothing() {
    let world = world(1, WorkConfig::default());
    let alloc = world.alloc;
    let unit = unit(3, 3, alloc);
    let never = AtomicBool::new(false);
    let mut compiler = Compiler {
        names: unit.names.clone(),
        log: Arc::new_in(Mutex::new(BTreeMap::new_in(alloc)), alloc),
        who: 0,
        entered: Arc::new_in(AtomicUsize::new(0), alloc),
        wait_for: 1,
        alloc,
    };
    let extra =
        Value { key: [200; 32], kind: 2, bytes: Vec::from_slice_in(b"a static", alloc) };
    let mut complete = WithTail { compiler: &mut compiler, extra: extra.clone() };
    let first = agent::run(
        &config(world.socket.as_str(), &world.blobs),
        &unit,
        &mut complete,
        &never,
    )
    .unwrap();
    assert_eq!(first.compiled_here, 3);

    // No coordinator is reachable at this socket: a published unit never asks it.
    let unreachable = world.dir.join("nothing-here.sock");
    let second = agent::run(
        &config(unreachable.as_str(), &world.blobs),
        &unit,
        &mut compiler,
        &never,
    )
    .unwrap();
    assert_eq!(second.source, Source::Published);
    assert_eq!(second.compiled_here, 0);
    assert_eq!(second.values.len(), 4, "the work and what the build made beyond it");
    assert_eq!(second.values[&extra.key], (2, extra.bytes));
}

#[test]
fn a_committed_pack_the_server_lost_refuses_without_compiling() {
    let world = world(1, WorkConfig::default());
    let alloc = world.alloc;
    let unit = unit(4, 2, alloc);
    let never = AtomicBool::new(false);
    let mut values = Vec::new_in(alloc);
    values.push(value_of(&[9; 32], alloc));
    let pack = Pack::of(values, alloc).unwrap();
    world.blobs.put_pack(&pack, alloc).unwrap();
    let list = blaze_wire::pack_list::encode(&[pack.reference.clone()], alloc);
    world.blobs.publish(&unit.spec, &list, alloc).unwrap();
    let pack_hex = format_in!(alloc, "{}", blaze_wire::json::hex(&pack.reference.pack));
    let rel = format_in!(alloc, "blobs/packs/{pack_hex}.pack");
    let path = world.dir.join(rel.as_str());
    unlink_path(path.as_str()).unwrap();
    let mut compiler = Compiler {
        names: unit.names.clone(),
        log: Arc::new_in(Mutex::new(BTreeMap::new_in(alloc)), alloc),
        who: 0,
        entered: Arc::new_in(AtomicUsize::new(0), alloc),
        wait_for: 1,
        alloc,
    };
    for policy in [LocalPolicy::Never, LocalPolicy::Fallback] {
        let error = agent::run(
            &config(world.socket.as_str(), &world.blobs),
            &unit,
            &mut compiler,
            &never,
        )
        .unwrap_err();
        assert!(
            matches!(&error, Error::Committed(why) if why.contains(pack_hex.as_str())),
            "{error}"
        );
        assert!(agent::decide(&policy, error, alloc).is_err());
    }
    assert_eq!(compiler.entered.load(Ordering::SeqCst), 0, "a committed absence must not compile");
    assert!(compiler.log.lock().is_empty());
    assert_eq!(world.blobs.published(&unit.spec, alloc).unwrap().unwrap(), list);
    // Remove only the fault: the same manifest is readable again without any compilation.
    world.blobs.put_pack(&pack, alloc).unwrap();
    let restored = agent::run(
        &config(world.socket.as_str(), &world.blobs),
        &unit,
        &mut compiler,
        &never,
    )
    .unwrap();
    assert_eq!(restored.source, Source::Published);
    assert_eq!(compiler.entered.load(Ordering::SeqCst), 0);
}

fn blob_proxy(listener: &TcpListener, world: &World, stop: &AtomicBool, mut inspect: impl FnMut(&[u8], Option<&mut WireVec<u8>>)) {
    let clock = SystemClock;
    let alloc = world.alloc;
    let start_ns = clock.monotonic_nanos().unwrap();
    let bound_ns = start_ns.saturating_add(120_000_000_000);
    while !stop.load(Ordering::Acquire)
        && clock.monotonic_nanos().unwrap() < bound_ns
    {
        let mut client = match listener.accept() {
            Ok(value) => value,
            Err(why) if why.is_would_block() => {
                sleep_ms(1);
                continue;
            }
            Err(why) => panic!("proxy accept: {why}"),
        };
        client.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
        client.set_write_timeout(Some(Duration::from_secs(30))).unwrap();
        let mut request = Vec::new_in(alloc);
        let mut buffer = [0u8; 4096];
        let end = loop {
            let n = client.read(&mut buffer).unwrap();
            assert_ne!(n, 0);
            request.extend_from_slice(&buffer[..n]);
            assert!(request.len() < 1 << 20);
            if let Some(at) = request.windows(4).position(|bytes| bytes == b"\r\n\r\n") {
                break at + 4;
            }
        };
        let headers = core::str::from_utf8(&request[..end]).unwrap();
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
        inspect(&request, None);
        let mut upstream =
            TcpStream::connect_timeout(world.blobs.address, Duration::from_secs(30))
                .unwrap();
        upstream.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
        upstream.set_write_timeout(Some(Duration::from_secs(30))).unwrap();
        upstream.write_all(&request).unwrap();
        let mut relay = [0u8; 8192];
        let mut response = Vec::new_in(alloc);
        loop {
            let n = upstream.read(&mut relay).unwrap();
            if n == 0 {
                break;
            }
            response.extend_from_slice(&relay[..n]);
            assert!(response.len() < 1 << 20);
        }
        inspect(&request, Some(&mut response));
        client.write_all(&response).unwrap();
    }
}

#[test]
fn a_slow_phase_pack_download_keeps_the_original_lead_alive() {
    let world = world(1, WorkConfig::default());
    let alloc = world.alloc;
    let listener = TcpListener::bind("127.0.0.1:0".parse().unwrap()).unwrap();
    listener.set_nonblocking(true).unwrap();
    let address = listener.local_addr();
    let defaults = ExecutionConfig::default();
    let delay = Duration::from_millis(defaults.heartbeat_ms * defaults.missed_heartbeats + 500);
    let delayed = AtomicBool::new(false);
    let unit = unit(44, 2, alloc);
    let mut compiler = Compiler {
        names: unit.names.clone(),
        log: Arc::new_in(Mutex::new(BTreeMap::new_in(alloc)), alloc),
        who: 0,
        entered: Arc::new_in(AtomicUsize::new(0), alloc),
        wait_for: 1,
        alloc,
    };
    let mut cfg = config(world.socket.as_str(), &world.blobs);
    cfg.blobs.address = address;
    let result = scope(alloc, |s| {
        let proxy = s.spawn(|| blob_proxy(&listener, &world, &world.stop, |request, response| {
            if response.is_none() && request.starts_with(b"GET /v1/blob/packs/") && !delayed.swap(true, Ordering::AcqRel) {
                // The real pack GET is held beyond the default lead-loss window.
                sleep(delay);
            }
        })).unwrap();
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
    struct WithNeighbors(WireVec<Digest>, WireAlloc);
    impl Granted for WithNeighbors {
        type Error = Infallible;

        fn compile(&mut self, names: &[usize]) -> Result<Grant, Self::Error> {
            let alloc = self.1;
            let mut values = Vec::with_capacity_in(self.0.len(), alloc);
            for name in &self.0 {
                values.push(value_of(name, alloc));
            }
            let mut emissions = Vec::with_capacity_in(names.len(), alloc);
            for at in names {
                emissions.push(Emission { key: self.0[*at], backend: None, native: None });
            }
            Ok(Grant {
                values,
                compiled: u64::try_from(names.len()).unwrap(),
                stored: 0,
                emissions,
            })
        }
    }
    let world = world(1, WorkConfig { grant_roots: 1, ..Default::default() });
    let alloc = world.alloc;
    let unit = unit(45, 2, alloc);
    let never = AtomicBool::new(false);
    let cfg = config(world.socket.as_str(), &world.blobs);
    let first =
        agent::run(&cfg, &unit, &mut WithNeighbors(unit.names.clone(), alloc), &never).unwrap();
    assert_eq!(first.packs.len(), 1);
    let old = world.blobs.published(&unit.spec, alloc).unwrap().unwrap();
    let mut split = Vec::new_in(alloc);
    for name in &unit.names {
        let mut vals = Vec::new_in(alloc);
        vals.push(value_of(name, alloc));
        let pack = Pack::of(vals, alloc).unwrap();
        world.blobs.put_pack(&pack, alloc).unwrap();
        split.push(pack.reference);
    }
    split.sort();
    let proposed = blaze_wire::pack_list::encode(&split, alloc);
    assert_ne!(proposed, old, "the first-writer check must compare actual different partitions");
    world.blobs.publish(&unit.spec, &proposed, alloc).unwrap();
    assert_eq!(
        world.blobs.published(&unit.spec, alloc).unwrap().unwrap(),
        old,
        "the first writer remains immutable"
    );
    assert_eq!(agent::import(&world.blobs, &split, alloc).unwrap(), first.values);

    // Same key sets cannot hide a different value or kind behind a different partition.
    for kind in [1, 2] {
        let mut values = Vec::with_capacity_in(unit.names.len(), alloc);
        for name in &unit.names {
            values.push(value_of(name, alloc));
        }
        values[0].kind = kind;
        if kind == 1 {
            values[0].bytes.push(99);
        }
        let wrong = Pack::of(values, alloc).unwrap();
        world.blobs.put_pack(&wrong, alloc).unwrap();
        let refused = world
            .blobs
            .publish(&unit.spec, &blaze_wire::pack_list::encode(&[wrong.reference], alloc), alloc)
            .unwrap_err();
        assert!(matches!(
            agent::decide(&LocalPolicy::Fallback, refused, alloc),
            Err(Error::Committed(_))
        ));
    }
    let mut cold = cfg;
    cold.socket = world.dir.join("no-coordinator.sock");
    let restored = agent::run(&cold, &unit, &mut Vanishes(alloc), &never).unwrap();
    assert_eq!(restored.source, Source::Published);
    assert_eq!(restored.values, first.values);
}

#[test]
fn an_already_cancelled_build_never_submits_an_execution() {
    let world = world(0, WorkConfig::default());
    let alloc = world.alloc;
    let unit = unit(5, 2, alloc);
    let config = config(world.socket.as_str(), &world.blobs);
    let job = |unit: Digest| blaze_wire::JobRequest {
        unit,
        platform: config.platform,
        namespace: Namespace::Main,
        source: [0; 32],
        deadline_ms: 1_000_000,
        local: LocalPolicy::Never,
    };
    let mut client = Client::connect(world.socket.as_str(), random_id().unwrap(), alloc).unwrap();
    let mut execution_of = |unit: Digest| {
        let Answer::Binding(binding) =
            client.call(Command::Submit(submission(&job(unit), NEED, alloc))).unwrap()
        else {
            panic!("the unit was refused");
        };
        binding.subscription.execution.0
    };
    // Executions are numbered in order. No executor runs them, so one that stays stays running.
    let before = execution_of([200; 32]);
    let cancelled = AtomicBool::new(true);
    let error = agent::run(&config, &unit, &mut Vanishes(alloc), &cancelled).unwrap_err();
    let rendered = format_in!(alloc, "{error}");
    assert!(rendered.contains("cancelled"), "{error}");
    assert!(matches!(agent::decide(&LocalPolicy::Fallback, error, alloc), Err(Error::Cancelled(_))));
    // Cancellation is checked before cache access or submission, so there is no intervening job.
    assert_eq!(execution_of(unit.spec), before + 1);
}

/// A compiler gate establishes which work is still running when cancellation occurs.
struct Holding {
    names: WireVec<Digest>,
    entered: Option<Sender<()>>,
    release: Option<Receiver<()>>,
    alloc: WireAlloc,
}

impl Granted for Holding {
    type Error = Infallible;

    fn compile(&mut self, names: &[usize]) -> Result<Grant, Self::Error> {
        let alloc = self.alloc;
        if let Some(entered) = self.entered.take() {
            entered.send(()).unwrap();
            self.release.take().unwrap().recv_timeout(Duration::from_secs(20)).unwrap();
        }
        let mut values = Vec::with_capacity_in(names.len(), alloc);
        let mut emissions = Vec::with_capacity_in(names.len(), alloc);
        for at in names {
            values.push(value_of(&self.names[*at], alloc));
            emissions.push(Emission { key: self.names[*at], backend: None, native: None });
        }
        Ok(Grant {
            values,
            compiled: u64::try_from(names.len()).unwrap(),
            stored: 0,
            emissions,
        })
    }
}

fn hold_first(unit: &Unit, alloc: WireAlloc) -> (Holding, Receiver<()>, Sender<()>) {
    let (entered, waiting) = channel(alloc);
    let (release, gate) = channel(alloc);
    (
        Holding { names: unit.names.clone(), entered: Some(entered), release: Some(gate), alloc },
        waiting,
        release,
    )
}

fn observer_subscribes(
    observer: &mut Client,
    unit: &Unit,
    alloc: WireAlloc,
) -> blaze_dist_local::protocol::Binding {
    let request = blaze_wire::JobRequest {
        unit: unit.spec,
        platform: agent::platform(TRIPLE),
        namespace: Namespace::Main,
        source: [0; 32],
        deadline_ms: observer.welcome.now_ms + 120_000,
        local: LocalPolicy::Never,
    };
    let Answer::Binding(binding) =
        observer.call(Command::Submit(submission(&request, NEED, alloc))).unwrap()
    else {
        panic!("the observer's subscription was refused")
    };
    binding
}

#[test]
fn a_cancelled_helper_releases_its_slot_only_after_its_compiler_stops() {
    let world = world(2, WorkConfig { grant_roots: 1, ..Default::default() });
    let alloc = world.alloc;
    let original = unit(94, 6, alloc);
    let next = unit(95, 0, alloc);
    let (mut lead, lead_waiting, release_lead) = hold_first(&original, alloc);
    let (mut helper, helper_waiting, release_helper) = hold_first(&original, alloc);
    let cancelled = AtomicBool::new(false);
    let never = AtomicBool::new(false);
    let mut observer = Client::connect(world.socket.as_str(), random_id().unwrap(), alloc).unwrap();
    let surviving = observer_subscribes(&mut observer, &original, alloc);
    scope(alloc, |s| {
        let (world, original, never, cancelled) = (&world, &original, &never, &cancelled);
        let leading = s
            .spawn(move || {
                agent::run(&config(world.socket.as_str(), &world.blobs), original, &mut lead, never)
                    .unwrap()
            })
            .unwrap();
        lead_waiting.recv_timeout(Duration::from_secs(20)).unwrap();
        let helping = s
            .spawn(move || {
                agent::run(
                    &config(world.socket.as_str(), &world.blobs),
                    original,
                    &mut helper,
                    cancelled,
                )
            })
            .unwrap();
        helper_waiting.recv_timeout(Duration::from_secs(20)).unwrap();
        cancelled.store(true, Ordering::Release);
        let queued = observer_subscribes(&mut observer, &next, alloc);
        let Answer::Claimed(claim) = observer
            .call(Command::Claim(blaze_dist_local::protocol::Claim {
                content: queued.subscription.content,
                execution: queued.subscription.execution,
                claimant: [96; 32],
            }))
            .unwrap()
        else {
            panic!("the queued unit's claim was refused")
        };
        assert!(claim.executor.is_none(), "a compiler still running must retain its reservation");

        release_helper.send(()).unwrap();
        assert!(matches!(helping.join().unwrap(), Err(Error::Cancelled(_))));
        let Answer::Terminal(terminal) =
            observer.call(Command::Terminal(surviving.subscription.clone())).unwrap()
        else {
            panic!("the surviving subscriber's terminal lookup was refused")
        };
        assert!(terminal.value.is_none(), "the helper's cancellation must leave its lead running");
        let mut next_config = config(world.socket.as_str(), &world.blobs);
        next_config.deadline = Duration::from_secs(2);
        let completed = agent::run(&next_config, &next, &mut Vanishes(alloc), never).unwrap();
        assert_eq!(completed.execution, Some(queued.subscription.execution.0));
        // The lead is still held. Only retirement plus its exact ACK could have
        // made one of this coordinator's two builders available to the next unit.
        release_lead.send(()).unwrap();
        let completed = leading.join().unwrap();
        assert_eq!(completed.execution, Some(surviving.subscription.execution.0));
        assert_eq!(completed.values.len(), original.names.len());
    });
    observer.release().unwrap();
}

#[test]
fn cancellation_before_observing_promotion_retires_the_owned_current_slot() {
    let world = world(2, WorkConfig { grant_roots: 1, ..Default::default() });
    let alloc = world.alloc;
    let original = unit(97, 6, alloc);
    let occupied = unit(98, 1, alloc);
    let next = unit(99, 0, alloc);
    let (mut lead, lead_waiting, release_lead) = hold_first(&original, alloc);
    let (mut helper, helper_waiting, release_helper) = hold_first(&original, alloc);
    let (mut occupying, occupied_waiting, release_occupied) = hold_first(&occupied, alloc);
    let cancel_lead = AtomicBool::new(false);
    let cancel_helper = AtomicBool::new(false);
    let never = AtomicBool::new(false);
    let mut observer = Client::connect(world.socket.as_str(), random_id().unwrap(), alloc).unwrap();
    let surviving = observer_subscribes(&mut observer, &original, alloc);
    scope(alloc, |s| {
        let (world, original, occupied) = (&world, &original, &occupied);
        let (cancel_lead, cancel_helper, never) = (&cancel_lead, &cancel_helper, &never);
        let leading = s
            .spawn(move || {
                agent::run(
                    &config(world.socket.as_str(), &world.blobs),
                    original,
                    &mut lead,
                    cancel_lead,
                )
            })
            .unwrap();
        lead_waiting.recv_timeout(Duration::from_secs(20)).unwrap();
        let helping = s
            .spawn(move || {
                agent::run(
                    &config(world.socket.as_str(), &world.blobs),
                    original,
                    &mut helper,
                    cancel_helper,
                )
            })
            .unwrap();
        helper_waiting.recv_timeout(Duration::from_secs(20)).unwrap();

        cancel_lead.store(true, Ordering::Release);
        release_lead.send(()).unwrap();
        assert!(matches!(leading.join().unwrap(), Err(Error::Cancelled(_))));
        let Answer::Binding(promoted) =
            observer.call(Command::Resume(surviving.subscription.clone())).unwrap()
        else {
            panic!("the survivor could not observe the helper's promotion")
        };
        assert!(promoted.attempt > surviving.attempt);
        // This helper is still inside its original compile callback. It cannot yet
        // have returned to the agent loop and observed or claimed the new attempt.
        cancel_helper.store(true, Ordering::Release);
        let holding = s
            .spawn(move || {
                agent::run(
                    &config(world.socket.as_str(), &world.blobs),
                    occupied,
                    &mut occupying,
                    never,
                )
                .unwrap()
            })
            .unwrap();
        occupied_waiting.recv_timeout(Duration::from_secs(20)).unwrap();
        let queued = observer_subscribes(&mut observer, &next, alloc);
        let Answer::Claimed(claim) = observer
            .call(Command::Claim(blaze_dist_local::protocol::Claim {
                content: queued.subscription.content,
                execution: queued.subscription.execution,
                claimant: [100; 32],
            }))
            .unwrap()
        else {
            panic!("the next unit's claim was refused")
        };
        assert!(claim.executor.is_none(), "both local compilers still hold their slots");

        release_helper.send(()).unwrap();
        assert!(matches!(helping.join().unwrap(), Err(Error::Cancelled(_))));
        let Answer::Terminal(terminal) =
            observer.call(Command::Terminal(surviving.subscription.clone())).unwrap()
        else {
            panic!("the survivor's terminal lookup was refused")
        };
        assert_eq!(
            terminal.value.unwrap().termination.outcome,
            blaze_wire::Outcome::Lost,
            "retiring the second lead preserves the existing reassignment bound"
        );
        let mut next_config = config(world.socket.as_str(), &world.blobs);
        next_config.deadline = Duration::from_secs(2);
        let completed = agent::run(&next_config, &next, &mut Vanishes(alloc), never).unwrap();
        assert_eq!(completed.execution, Some(queued.subscription.execution.0));
        // The first slot is occupied by another unit. The next unit can run only if
        // cleanup discovered and acknowledged the helper's unobserved current token.
        release_occupied.send(()).unwrap();
        assert_eq!(holding.join().unwrap().values.len(), occupied.names.len());
    });
    observer.release().unwrap();
}

#[test]
fn an_execution_that_ended_says_why_and_the_requester_goes_by_its_policy() {
    // No builder ever runs the job: the subscriber's own deadline ends it `TimedOut`, which is
    // no verdict, so `Fallback` compiles here and `Never` stops the build.
    let world = world(0, WorkConfig::default());
    let alloc = world.alloc;
    let unit = unit(6, 2, alloc);
    let never = AtomicBool::new(false);
    let mut config = config(world.socket.as_str(), &world.blobs);
    config.deadline = Duration::from_millis(50);
    let failure = agent::run(&config, &unit, &mut Vanishes(alloc), &never).unwrap_err();
    let Error::Ended(termination) = &failure else { panic!("{failure}") };
    assert_eq!(termination.outcome, blaze_wire::Outcome::TimedOut);
    assert!(
        agent::decide(&LocalPolicy::Fallback, Error::Ended(termination.clone()), alloc).is_ok()
    );
    assert!(agent::decide(&LocalPolicy::Never, failure, alloc).is_err());
}

#[test]
fn a_verdict_is_taken_under_every_policy() {
    use blaze_wire::job_terminal_v2::{TerminationReasonV2, TerminationV2, VERSION};
    let alloc = test_alloc();
    let reported = TerminationV2 {
        version: VERSION,
        outcome: blaze_wire::Outcome::Failed,
        reason: TerminationReasonV2::Reported,
    };
    for policy in [LocalPolicy::Never, LocalPolicy::Fallback] {
        assert!(agent::decide(&policy, Error::Ended(reported.clone()), alloc).is_err());
    }
}

#[test]
fn a_final_compiler_or_output_refusal_cannot_fall_back() {
    struct Refuses {
        malformed_output: bool,
        succeeds: bool,
        calls: usize,
        alloc: WireAlloc,
    }
    impl Granted for Refuses {
        type Error = TestError;

        fn compile(&mut self, _: &[usize]) -> Result<Grant, Self::Error> {
            panic!("this fixture has no distributable roots")
        }

        fn complete(
            &mut self,
            _: &mut dyn Iterator<Item = ValueRef<'_>>,
            output: &mut dyn FnMut(Value),
        ) -> Result<(), Self::Error> {
            self.calls += 1;
            if self.succeeds {
                return Ok(());
            }
            let first = value_of(&[92; 32], self.alloc);
            output(first.clone());
            if !self.malformed_output {
                return Err(TestError(WireString::from_str_in(
                    "fixture::tail: output export refused conflicting immutable bytes",
                    self.alloc,
                )));
            }
            let mut second = first.clone();
            second.bytes.push(1);
            output(second);
            Ok(())
        }
    }

    for malformed_output in [false, true] {
        for policy in [LocalPolicy::Never, LocalPolicy::Fallback] {
            let world = world(1, WorkConfig::default());
            let alloc = world.alloc;
            let unit = unit(93, 0, alloc);
            let mut cfg = config(world.socket.as_str(), &world.blobs);
            cfg.local = policy.clone();
            cfg.deadline = Duration::from_secs(2);
            let mut compiler = Refuses { malformed_output, succeeds: false, calls: 0, alloc };
            let mut observer =
                Client::connect(world.socket.as_str(), random_id().unwrap(), alloc).unwrap();
            let request = blaze_wire::JobRequest {
                unit: unit.spec,
                platform: cfg.platform,
                namespace: cfg.namespace.clone(),
                source: cfg.source,
                deadline_ms: observer.welcome.now_ms + 120_000,
                local: policy.clone(),
            };
            let Answer::Binding(observing) =
                observer.call(Command::Submit(submission(&request, NEED, alloc))).unwrap()
            else {
                panic!("the observer's subscription was refused")
            };
            let failure =
                agent::run(&cfg, &unit, &mut compiler, &AtomicBool::new(false)).unwrap_err();
            let spec_hex = format_in!(alloc, "{}", blaze_wire::json::hex(&unit.spec));
            assert!(
                matches!(&failure, Error::Refused(why)
                    if why.contains(spec_hex.as_str())
                        && why.contains(if malformed_output {
                            "full output cannot be packed"
                        } else {
                            "fixture::tail: output export refused conflicting immutable bytes"
                        })),
                "{failure}"
            );
            assert!(matches!(agent::decide(&policy, failure, alloc), Err(Error::Refused(_))));
            assert_eq!(compiler.calls, 1);
            assert!(world.blobs.published(&unit.spec, alloc).unwrap().is_none());
            let mut emitted_vals = Vec::new_in(alloc);
            emitted_vals.push(value_of(&[92; 32], alloc));
            let emitted_before_refusal = Pack::of(emitted_vals, alloc).unwrap();
            assert!(
                world.blobs.pack(&emitted_before_refusal.reference.pack, alloc).unwrap().is_none(),
                "an emitted value cannot be uploaded before the compiler finishes successfully"
            );
            let Answer::Terminal(blaze_dist_local::protocol::Terminal {
                value: Some(terminal),
                ..
            }) = observer.call(Command::Terminal(observing.subscription)).unwrap()
            else {
                panic!("the lead's refusal must also end its other subscriber")
            };
            assert_eq!(terminal.termination.outcome, blaze_wire::Outcome::Failed);
            assert!(agent::decide(&policy, Error::Ended(terminal.termination), alloc).is_err());
            // Remove only the compiler/output fault. The same one-builder
            // coordinator must run this next execution: a refused tail's stop
            // acknowledgment cannot leave its reservation held forever.
            compiler.succeeds = true;
            let completed =
                agent::run(&cfg, &unit, &mut compiler, &AtomicBool::new(false)).unwrap();
            assert_eq!(completed.source, Source::Executed);
            assert_eq!(compiler.calls, 2);
            observer.release().unwrap();
        }
    }
}

#[test]
fn a_coordinator_that_is_gone_stops_the_build_unless_it_may_compile_here() {
    let world = world(1, WorkConfig::default());
    let alloc = world.alloc;
    let unit = unit(7, 2, alloc);
    let never = AtomicBool::new(false);
    let unreachable = world.dir.join("nothing-here.sock");
    let failure = agent::run(
        &config(unreachable.as_str(), &world.blobs),
        &unit,
        &mut Vanishes(alloc),
        &never,
    )
    .unwrap_err();
    assert!(matches!(failure, Error::Io(_)), "{failure}");
    assert!(agent::decide(&LocalPolicy::Fallback, failure, alloc).is_ok());
    let failure = agent::run(
        &config(unreachable.as_str(), &world.blobs),
        &unit,
        &mut Vanishes(alloc),
        &never,
    )
    .unwrap_err();
    assert!(agent::decide(&LocalPolicy::Never, failure, alloc).is_err());
}

/// A pull request's unit and the main line's run apart, and what either published is the other's.
#[test]
fn a_unit_published_by_one_pull_request_is_taken_by_another() {
    let world = world_with(
        &[Namespace::Pr(PrNamespace { n: 1 }), Namespace::Pr(PrNamespace { n: 2 })],
        WorkConfig::default(),
        CarrierConfig::default(),
    );
    let alloc = world.alloc;
    let unit = unit(8, 3, alloc);
    let never = AtomicBool::new(false);
    let log = Arc::new_in(Mutex::new(BTreeMap::new_in(alloc)), alloc);
    let mut compiler = Compiler {
        names: unit.names.clone(),
        log,
        who: 0,
        entered: Arc::new_in(AtomicUsize::new(0), alloc),
        wait_for: 1,
        alloc,
    };
    let of_pr = |n: u64, source: u8| {
        let mut config = config(world.socket.as_str(), &world.blobs);
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
    let world = world_with(
        &[Namespace::Main],
        WorkConfig { grant_roots: 1024, ..Default::default() },
        carrier,
    );
    let alloc = world.alloc;
    let mut names = Vec::with_capacity_in(5_000, alloc);
    for at in 0..5_000u32 {
        let mut it = [0u8; 32];
        it[..4].copy_from_slice(&at.to_be_bytes());
        it[4] = 0x5a;
        names.push(it);
    }
    let unit = Unit { spec: [33; 32], names: names.clone() };
    let never = AtomicBool::new(false);
    let mut compiler = Compiler {
        names,
        log: Arc::new_in(Mutex::new(BTreeMap::new_in(alloc)), alloc),
        who: 0,
        entered: Arc::new_in(AtomicUsize::new(0), alloc),
        wait_for: 1,
        alloc,
    };
    let arrived = agent::run(
        &config(world.socket.as_str(), &world.blobs),
        &unit,
        &mut compiler,
        &never,
    )
    .unwrap();
    assert_eq!(arrived.values.len(), 5_000);
    assert_eq!(arrived.compiled_here, 5_000);
}

/// An agent and its holder ask again and again in one session each, a grant of one name at a
/// time here: every answer but the last is let go, so no number of asks fills a session.
#[test]
fn an_agent_that_waits_asks_in_one_session_however_often() {
    let carrier = CarrierConfig { max_sessions: 4, ..CarrierConfig::default() };
    let world = world_with(
        &[Namespace::Main],
        WorkConfig { grant_roots: 1, ..Default::default() },
        carrier,
    );
    let alloc = world.alloc;
    let unit = unit(34, 12, alloc);
    let never = AtomicBool::new(false);
    let mut compiler = Compiler {
        names: unit.names.clone(),
        log: Arc::new_in(Mutex::new(BTreeMap::new_in(alloc)), alloc),
        who: 0,
        entered: Arc::new_in(AtomicUsize::new(0), alloc),
        wait_for: 1,
        alloc,
    };
    let arrived = agent::run(
        &config(world.socket.as_str(), &world.blobs),
        &unit,
        &mut compiler,
        &never,
    )
    .unwrap();
    assert_eq!(arrived.values.len(), 12);
}

/// A warm local tail hit still belongs to the manifest sent to an empty subscriber.
struct WithTail<'a> {
    compiler: &'a mut Compiler,
    extra: Value,
}

impl Granted for WithTail<'_> {
    type Error = Infallible;

    fn compile(&mut self, names: &[usize]) -> Result<Grant, Self::Error> {
        self.compiler.compile(names)
    }

    fn complete(
        &mut self,
        _: &mut dyn Iterator<Item = ValueRef<'_>>,
        output: &mut dyn FnMut(Value),
    ) -> Result<(), Self::Error> {
        output(self.extra.clone());
        Ok(())
    }
}

/// The full manifest may repeat a committed value, but cannot accept a different value
/// under its immutable key. Refusal precedes both the tail upload and publication.
#[test]
fn a_tail_conflicting_with_committed_values_is_refused_before_publication() {
    for policy in [LocalPolicy::Never, LocalPolicy::Fallback] {
        for change in ["none", "bytes", "kind"] {
            let world = world(1, WorkConfig::default());
            let alloc = world.alloc;
            let unit = unit(94, 1, alloc);
            let original = value_of(&unit.names[0], alloc);
            let mut orig_vals = Vec::new_in(alloc);
            orig_vals.push(original.clone());
            let committed = Pack::of(orig_vals, alloc).unwrap();
            let mut extra = original.clone();
            match change {
                "bytes" => extra.bytes[0] ^= 1,
                "kind" => extra.kind += 1,
                "none" => {}
                _ => unreachable!(),
            }
            let mut extra_vals = Vec::new_in(alloc);
            extra_vals.push(extra.clone());
            let emitted = Pack::of(extra_vals, alloc).unwrap();
            let mut cfg = config(world.socket.as_str(), &world.blobs);
            cfg.local = policy.clone();
            let log = Arc::new_in(Mutex::new(BTreeMap::new_in(alloc)), alloc);
            let mut compiler = Compiler {
                names: unit.names.clone(),
                log: log.clone(),
                who: 0,
                entered: Arc::new_in(AtomicUsize::new(0), alloc),
                wait_for: 1,
                alloc,
            };
            // Keep a subscription so the accepted publication and terminal can be
            // inspected after the agent has released its own session.
            let mut observer =
                Client::connect(world.socket.as_str(), random_id().unwrap(), alloc).unwrap();
            let request = blaze_wire::JobRequest {
                unit: unit.spec,
                platform: cfg.platform,
                namespace: cfg.namespace.clone(),
                source: cfg.source,
                deadline_ms: observer.welcome.now_ms + 120_000,
                local: policy.clone(),
            };
            let Answer::Binding(observing) =
                observer.call(Command::Submit(submission(&request, NEED, alloc))).unwrap()
            else {
                panic!("the observer's subscription was refused")
            };
            let result = agent::run(
                &cfg,
                &unit,
                &mut WithTail { compiler: &mut compiler, extra },
                &AtomicBool::new(false),
            );
            assert_eq!(log.lock().len(), 1, "the compile phase must actually run");
            assert!(world.blobs.pack(&committed.reference.pack, alloc).unwrap().is_some());
            let Answer::Publication(publication) = observer
                .call(Command::Publication(blaze_dist_local::protocol::Packs {
                    content: observing.subscription.content,
                    execution: observing.subscription.execution,
                }))
                .unwrap()
            else {
                panic!("the observer must see the publication state")
            };
            if change == "none" {
                let arrived = result.unwrap();
                assert_eq!(arrived.source, Source::Executed);
                assert_eq!(arrived.values.len(), 1);
                assert_eq!(arrived.values[&original.key], (original.kind, original.bytes));
                assert_eq!(arrived.packs.as_slice(), &[committed.reference]);
                assert_eq!(publication.packs.as_deref(), Some(arrived.packs.as_slice()));
                assert!(publication.accepted_attempt.is_some());
                assert!(world.blobs.published(&unit.spec, alloc).unwrap().is_some());
            } else {
                let failure = result.unwrap_err();
                let key_hex = format_in!(alloc, "{}", blaze_wire::json::hex(&original.key));
                let spec_hex = format_in!(alloc, "{}", blaze_wire::json::hex(&unit.spec));
                assert!(
                    matches!(&failure, Error::Refused(why)
                        if why.contains("full output conflicts with the committed value")
                            && why.contains(key_hex.as_str())
                            && why.contains(spec_hex.as_str())),
                    "{change}: {failure}"
                );
                assert!(matches!(agent::decide(&policy, failure, alloc), Err(Error::Refused(_))));
                assert!(publication.packs.is_none(), "a conflicting full manifest was accepted");
                assert!(publication.accepted_attempt.is_none());
                assert!(world.blobs.published(&unit.spec, alloc).unwrap().is_none());
                assert!(
                    world.blobs.pack(&emitted.reference.pack, alloc).unwrap().is_none(),
                    "a conflicting tail must be refused before uploading any of its packs"
                );
            }
            let Answer::Terminal(blaze_dist_local::protocol::Terminal {
                value: Some(terminal),
                ..
            }) = observer.call(Command::Terminal(observing.subscription)).unwrap()
            else {
                panic!("the observer must receive the unit's final verdict")
            };
            assert_eq!(
                terminal.termination.outcome,
                if change == "none" {
                    blaze_wire::Outcome::Passed
                } else {
                    blaze_wire::Outcome::Failed
                }
            );
            observer.release().unwrap();
        }
    }
}

#[test]
fn a_late_subscriber_waits_for_the_one_leads_full_tail() {
    struct Paused {
        entered: Sender<()>,
        release: Receiver<()>,
        tails: Arc<AtomicUsize, WireAlloc>,
        extra: Value,
    }
    impl Granted for Paused {
        type Error = Infallible;

        fn compile(&mut self, _: &[usize]) -> Result<Grant, Self::Error> {
            panic!("empty work set")
        }
        fn complete(
            &mut self,
            _: &mut dyn Iterator<Item = ValueRef<'_>>,
            output: &mut dyn FnMut(Value),
        ) -> Result<(), Self::Error> {
            assert_eq!(self.tails.fetch_add(1, Ordering::SeqCst), 0);
            self.entered.send(()).unwrap();
            self.release.recv_timeout(Duration::from_secs(20)).unwrap();
            output(self.extra.clone());
            Ok(())
        }
    }
    struct Reader;
    impl Granted for Reader {
        type Error = TestError;

        fn compile(&mut self, _: &[usize]) -> Result<Grant, Self::Error> {
            Err(TestError(WireString::from_str_in("late subscriber compiled", test_alloc())))
        }
        fn complete(
            &mut self,
            _: &mut dyn Iterator<Item = ValueRef<'_>>,
            _: &mut dyn FnMut(Value),
        ) -> Result<(), Self::Error> {
            Err(TestError(WireString::from_str_in("late subscriber ran a second tail", test_alloc())))
        }
    }
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Arrival { Live, Delivered, Superseded, LostPack, FailedLiveRead }

    for arrival in [Arrival::Live, Arrival::Delivered, Arrival::Superseded, Arrival::LostPack, Arrival::FailedLiveRead] {
        let live = matches!(arrival, Arrival::Live | Arrival::FailedLiveRead);
        let namespace = Namespace::Pr(PrNamespace { n: 1 });
        let world = world_with(&[namespace.clone(), namespace.clone()], WorkConfig::default(), CarrierConfig::default());
        let alloc = world.alloc;
        let unit = Unit { spec: [82; 32], names: Vec::new_in(alloc) };
        let extra = value_of(&[83; 32], alloc);
        let tails = Arc::new_in(AtomicUsize::new(0), alloc);
        let (entered, completing) = channel(alloc);
        let (release, gate) = channel(alloc);
        let (observed, probes) = channel(alloc);
        let (resume, pause) = channel(alloc);
        let never = AtomicBool::new(false);
        let stop_proxy = AtomicBool::new(false);
        let listener = TcpListener::bind("127.0.0.1:0".parse().unwrap()).unwrap();
        listener.set_nonblocking(true).unwrap();
        let mut lead_config = config(world.socket.as_str(), &world.blobs);
        lead_config.namespace = namespace.clone();
        let mut reader_config = config(world.socket.as_str(), &world.blobs);
        reader_config.namespace = namespace.clone();
        reader_config.blobs.address = listener.local_addr();
        let mut observer = Client::connect(world.socket.as_str(), random_id().unwrap(), alloc).unwrap();
        let submit = |observer: &mut Client, spec, source| {
            let request = blaze_wire::JobRequest {
                unit: spec, source, namespace: namespace.clone(), platform: agent::platform(TRIPLE),
                deadline_ms: observer.welcome.now_ms + 120_000, local: LocalPolicy::Never,
            };
            observer.call(Command::Submit(submission(&request, NEED, alloc))).unwrap()
        };
        scope(alloc, |s| {
            let (unit, never) = (&unit, &never);
            let mut compiler = Paused { entered, release: gate, tails: tails.clone(), extra: extra.clone() };
            let lead = s.spawn(move || agent::run(&lead_config, unit, &mut compiler, never).unwrap()).unwrap();
            completing.recv_timeout(Duration::from_secs(20)).unwrap();
            // The first response is a real miss. The second request can start only after
            // this reader's Submit returned a binding.
            let proxy = s.spawn(|| {
                let mut remaining = 2;
                blob_proxy(&listener, &world, &stop_proxy, |request, response| {
                    if remaining > 0 && request.starts_with(b"GET /v1/list/manifests/") {
                        let Some(response) = response else {
                            if remaining == 1 && live {
                                observed.send(None).unwrap();
                                pause.recv_timeout(Duration::from_secs(20)).unwrap();
                            }
                            return;
                        };
                        remaining -= 1;
                        let status = core::str::from_utf8(response.split(|&b| b == b'\n').next().unwrap()).unwrap();
                        let code = status.split_whitespace().nth(1).unwrap().parse::<u16>().unwrap();
                        observed.send(Some(code)).unwrap();
                        pause.recv_timeout(Duration::from_secs(20)).unwrap();
                        if remaining == 0 && arrival == Arrival::FailedLiveRead {
                            response.clear();
                            response.extend_from_slice(b"HTTP/1.1 503 Unavailable\r\nConnection: close\r\nContent-Length: 0\r\n\r\n");
                        }
                    }
                });
            }).unwrap();
            let reader = s.spawn(move || agent::run(&reader_config, unit, &mut Reader, never)).unwrap();
            let response = || probes.recv_timeout(Duration::from_secs(20)).unwrap();
            assert_eq!(response(), Some(404), "the reader observed a real miss");
            if live {
                resume.send(()).unwrap();
                assert_eq!(response(), None, "the reader subscribed before the lead published");
            }
            release.send(()).unwrap();
            let first = lead.join().unwrap();
            let mut withdrawing = None;
            resume.send(()).unwrap();
            assert_eq!(response(), Some(200), "the bound reader found the delivered manifest");
            if !live {
                if arrival == Arrival::Superseded {
                    // The new revision cancels the sole subscriber of the old one.
                    let Answer::Binding(newer) = submit(&mut observer, [84; 32], [1; 32]) else { panic!() };
                    assert!(matches!(observer.call(Command::Cancel(newer)), Ok(Answer::Ack(_))));
                } else {
                    // Observe the reader's new execution and leave it as the sole interest.
                    let Answer::Binding(joined) = submit(&mut observer, unit.spec, [0; 32]) else { panic!() };
                    withdrawing = Some(joined.subscription.execution);
                    assert!(matches!(observer.call(Command::Cancel(joined)), Ok(Answer::Ack(_))));
                }
                if arrival == Arrival::LostPack {
                    let pack = &first.packs.first().unwrap().pack;
                    let file = format_in!(alloc, "blobs/packs/{}.pack", blaze_wire::json::hex(pack));
                    unlink_path(world.dir.join(file.as_str()).as_str()).unwrap();
                }
            }
            resume.send(()).unwrap();
            let result = reader.join().unwrap();
            stop_proxy.store(true, Ordering::Release);
            proxy.join().unwrap();
            match arrival {
                Arrival::Live | Arrival::Delivered => {
                    let got = result.unwrap();
                    assert_eq!(got.values, first.values);
                    assert_eq!(got.values.get(&extra.key), Some(&(extra.kind, extra.bytes.clone())));
                    assert_eq!((got.compiled_here, got.stored_here, got.grants), (0, 0, 0));
                    assert!(got.compiled_keys.is_empty());
                    if arrival == Arrival::Live {
                        assert_eq!(got.source, Source::Executed);
                        assert_eq!((got.execution, got.publication_attempt), (first.execution, first.publication_attempt));
                    } else {
                        assert_eq!(got.source, Source::Published);
                        assert_eq!((got.execution, got.attempt, got.publication_attempt, got.joined), (None, None, None, false));
                        let no_carrier = config("there-is-no-carrier-here", &world.blobs);
                        let warm = agent::run(&no_carrier, unit, &mut Reader, never).unwrap();
                        assert_eq!((warm.source, warm.values), (Source::Published, first.values));
                    }
                }
                Arrival::Superseded => assert!(matches!(result, Err(Error::Ended(t)) if t.outcome == blaze_wire::Outcome::Cancelled)),
                Arrival::LostPack | Arrival::FailedLiveRead => {
                    assert!(matches!(agent::decide(&LocalPolicy::Fallback, result.unwrap_err(), alloc), Err(Error::Committed(_))));
                }
            }
            if let Some(old) = withdrawing {
                let Answer::Binding(fresh) = submit(&mut observer, unit.spec, [0; 32]) else { panic!() };
                assert_ne!(fresh.subscription.execution, old, "the reader left its sole subscription");
                assert!(matches!(observer.call(Command::Cancel(fresh)), Ok(Answer::Ack(_))));
            }
            assert_eq!(tails.load(Ordering::SeqCst), 1);
        });
        observer.release().unwrap();
    }
}
