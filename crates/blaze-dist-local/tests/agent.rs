//! The agent over the real carrier and the real cache server, with a batch compiler standing in
//! for the code generator: what a daemon's unit does, without a front end.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use blaze_dist::admission::{Admission, AdmissionConfig};
use blaze_dist::holder::HolderConfig;
use blaze_dist::jobs::{ExecutionConfig, Jobs};
use blaze_dist::work::WorkConfig;
use blaze_dist_local::agent::{self, Grant, Granted, NEED, Source, Unit};
use blaze_dist_local::blobs::Blobs;
use blaze_dist_local::coordinator::Config as CarrierConfig;
use blaze_dist_local::pack::{Pack, Value};
use blaze_dist_local::server::{MonotonicClock, Server, random_id};
use blaze_wire::{Digest, LocalPolicy, Reservation};

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
    let jobs = Jobs::new(admission, ExecutionConfig { work, ..Default::default() });
    let tag: String = random_id().unwrap()[..6].iter().map(|b| format!("{b:02x}")).collect();
    let socket = PathBuf::from(format!("/tmp/blaze-agent-{tag}.sock"));
    let server = Server::bind(
        &socket,
        jobs,
        CarrierConfig::default(),
        Arc::new(MonotonicClock::default()),
        |_: Vec<_>| {},
    )
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
    agent::publish(&world.blobs, &unit, arrived.packs, Vec::new()).unwrap();
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
    let first =
        agent::run(&config(&world.socket, &world.blobs), &unit, &mut compiler, &never).unwrap();
    assert_eq!(first.compiled_here, 3);
    let extra = Value { key: [200; 32], kind: 2, bytes: b"a static".to_vec() };
    agent::publish(&world.blobs, &unit, first.packs, vec![extra.clone()]).unwrap();

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
fn a_pack_the_server_lost_is_an_unpublished_unit() {
    let world = world(1, WorkConfig::default());
    let unit = unit(4, 2);
    let never = AtomicBool::new(false);
    let gone = Pack::of(vec![value_of(&[9; 32])]).unwrap();
    // Named by a list and never uploaded.
    let list = format!(
        "{} {} {}\n",
        blaze_dist_local::pack::hex(&gone.reference.pack),
        gone.reference.size,
        blaze_dist_local::pack::hex(&gone.reference.keys)
    );
    world.blobs.publish(&unit.spec, list.as_bytes()).unwrap();
    let mut compiler = Compiler {
        names: unit.names.clone(),
        log: Arc::new(Mutex::new(BTreeMap::new())),
        who: 0,
        entered: Arc::new(AtomicUsize::new(0)),
        wait_for: 1,
    };
    let arrived =
        agent::run(&config(&world.socket, &world.blobs), &unit, &mut compiler, &never).unwrap();
    assert_eq!(arrived.source, Source::Executed, "the work was done again");
    assert_eq!(arrived.values.len(), 2);
}

#[test]
fn a_cancelled_build_leaves_its_execution() {
    let world = world(0, WorkConfig::default());
    let unit = unit(5, 2);
    let cancelled = AtomicBool::new(true);
    let mut compiler = Vanishes;
    let error = agent::run(&config(&world.socket, &world.blobs), &unit, &mut compiler, &cancelled)
        .unwrap_err();
    assert!(error.to_string().contains("cancelled"), "{error}");
}
