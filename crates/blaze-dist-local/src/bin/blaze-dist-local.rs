//! The coordinator of a machine's daemons, over the local transport. It compiles nothing.

#![feature(allocator_api)]

use std::alloc::Global;
use std::io::{self, BufRead};
use std::path::Path;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use blaze_dist::admission::{Admission, AdmissionConfig};
use blaze_dist::jobs::{CompletionMode, ExecutionConfig, Jobs};
use blaze_dist_local::coordinator::Config;
use blaze_dist_local::server::{MonotonicClock, Server};

const USAGE: &str = "usage: blaze-dist-local coordinate SOCKET TRIPLE EXECUTORS [LEASE_MS]";

#[derive(Debug)]
enum CommandError {
    Usage,
    NotText(&'static str),
    InvalidNumber { argument: &'static str, source: std::num::ParseIntError },
    MachineFull,
    Carrier(blaze_dist_local::Error),
}

impl std::fmt::Display for CommandError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Usage => f.write_str(USAGE),
            Self::NotText(argument) => write!(f, "{argument} is not text"),
            Self::InvalidNumber { argument, source } => write!(f, "invalid {argument}: {source}"),
            Self::MachineFull => f.write_str("the machine has no room"),
            Self::Carrier(source) => write!(f, "{source}"),
        }
    }
}

impl std::error::Error for CommandError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::InvalidNumber { source, .. } => Some(source),
            Self::Carrier(source) => Some(source),
            _ => None,
        }
    }
}

impl From<blaze_dist_local::Error> for CommandError {
    fn from(source: blaze_dist_local::Error) -> Self {
        Self::Carrier(source)
    }
}

/// The coordinator a machine's daemons build through: `EXECUTORS` builders of one host triple,
/// each of the daemons' units one execution, until stdin closes or says `stop`. Its bounds are
/// a whole build's: the carrier's defaults are one client's. `LEASE_MS` is how long a holder may
/// be silent before its lease ends. What fences a holder or moves an attempt goes to stdout, one
/// line each, for the person or the harness reading what happened.
fn coordinate(args: &[std::ffi::OsString]) -> Result<(), CommandError> {
    let (path, triple, executors, lease) = match args {
        [path, triple, executors] => (path, triple, executors, None),
        [path, triple, executors, lease] => (path, triple, executors, Some(lease)),
        _ => return Err(CommandError::Usage),
    };
    let (triple, executors): (&str, u32) = (
        triple.to_str().ok_or(CommandError::NotText("TRIPLE"))?,
        executors.to_str().ok_or(CommandError::NotText("EXECUTORS"))?.parse()
            .map_err(|source| CommandError::InvalidNumber { argument: "EXECUTORS", source })?,
    );
    let defaults = blaze_dist::work::WorkConfig::default();
    let lease_ms = match lease {
        Some(it) => it.to_str().ok_or(CommandError::NotText("LEASE_MS"))?.parse()
            .map_err(|source| CommandError::InvalidNumber { argument: "LEASE_MS", source })?,
        None => defaults.lease_ms,
    };
    let need = blaze_dist_local::agent::NEED;
    let mut admission = Admission::new(AdmissionConfig {
        queue_limit: 1 << 16,
        busy_retry_ms: 100,
        ..AdmissionConfig::default()
    });
    admission.add_machine(
        "machine",
        blaze_dist_local::agent::platform(triple),
        blaze_wire::Reservation {
            memory: u64::from(executors),
            cpus: executors,
            disk: u64::from(executors),
        },
    );
    for _ in 0..executors {
        admission.add_builder(0, "machine", need.clone(), Global).ok_or(CommandError::MachineFull)?;
    }
    let jobs = Jobs::new(
        admission,
        ExecutionConfig {
            work: blaze_dist::work::WorkConfig { grant_roots: 64, lease_ms, ..defaults },
            completion: CompletionMode::PublishedWork,
            ..Default::default()
        },
    );
    let stop = Arc::new(AtomicBool::new(false));
    let signal = stop.clone();
    std::thread::spawn(move || {
        for line in io::stdin().lock().lines() {
            if line.is_ok_and(|line| line == "stop") {
                break;
            }
        }
        signal.store(true, Ordering::Release);
    });
    let server = Server::bind(
        Path::new(path),
        jobs,
        Config {
            max_connections: 1_024,
            max_sessions: 1_024,
            max_executions: 1 << 16,
            max_events: 1 << 20,
            event_bytes: 1 << 30,
            work_bytes: 1 << 30,
            max_roots: defaults.max_roots,
            max_grants: 1 << 20,
            session_ms: 6 * 3_600_000,
            ..Config::default()
        },
        Arc::new(MonotonicClock::default()),
        |effects: &mut dyn Iterator<Item = (blaze_wire::ExecutionId, blaze_dist::jobs::Effect)>| {
            for (execution, effect) in effects {
                report(execution, &effect);
            }
        },
    )?;
    println!("ready {}", Path::new(path).display());
    server.run(&stop)?;
    Ok(())
}

/// One line for what took work back from a holder. Everything else is the ordinary run.
fn report(execution: blaze_wire::ExecutionId, effect: &blaze_dist::jobs::Effect) {
    use blaze_dist::jobs::Effect;
    use blaze_dist::work::Effect as Work;
    let what = match effect {
        Effect::NeedLead => "lead-lost".to_owned(),
        Effect::Kill { executor, assignment } => {
            format!("kill {executor} attempt {}", assignment.attempt.0)
        }
        Effect::Work(Work::Refused { holder, refusal }) => format!("refused {holder}: {refusal:?}"),
        Effect::Work(Work::HolderLost { holder, .. }) => format!("holder-lost {holder}"),
        Effect::Work(Work::Fenced { holder, generation, .. }) => {
            format!("fenced {holder} generation {generation}")
        }
        _ => return,
    };
    println!("execution {}: {what}", execution.0);
}

fn run() -> Result<(), CommandError> {
    let args: Vec<_> = std::env::args_os().collect();
    if args.get(1).and_then(|it| it.to_str()) != Some("coordinate") {
        return Err(CommandError::Usage);
    }
    coordinate(&args[2..])
}

fn main() {
    if let Err(error) = run() {
        eprintln!("blaze-dist-local: {error}");
        std::process::exit(1);
    }
}
