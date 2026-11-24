//! The coordinator of a machine's daemons, over the local transport. It compiles nothing.

use core::fmt;
use core::num::ParseIntError;
use core::sync::atomic::{AtomicBool, Ordering};

use blaze_dist::admission::{Admission, AdmissionConfig};
use blaze_dist::jobs::{CompletionMode, ExecutionConfig, Jobs};
use blaze_dist_local::coordinator::Config;
use blaze_dist_local::server::{MonotonicClock, Server};
use blaze_dist_local::sys::{
    IoError, process_args, read_stdin_line, spawn, write_stderr, write_stdout,
};
use blaze_own::{Arc, Heap, String, format_in};
use blaze_wire::WireAlloc;

const USAGE: &str = "usage: blaze-dist-local coordinate SOCKET TRIPLE EXECUTORS [LEASE_MS]";

/// Marker error returning a non-zero exit status from `main` after printing to stderr.
struct CliExit;

impl fmt::Debug for CliExit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("blaze-dist-local exited with error")
    }
}

#[derive(Debug)]
enum CommandError {
    Usage,
    InvalidNumber { argument: &'static str, source: ParseIntError },
    MachineFull,
    Carrier(blaze_dist_local::Error),
    Io(IoError),
}

impl fmt::Display for CommandError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Usage => f.write_str(USAGE),
            Self::InvalidNumber { argument, source } => write!(f, "invalid {argument}: {source}"),
            Self::MachineFull => f.write_str("the machine has no room"),
            Self::Carrier(source) => write!(f, "{source}"),
            Self::Io(source) => write!(f, "{source}"),
        }
    }
}

impl core::error::Error for CommandError {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::InvalidNumber { source, .. } => Some(source),
            Self::Carrier(source) => Some(source),
            Self::Io(source) => Some(source),
            _ => None,
        }
    }
}

impl From<blaze_dist_local::Error> for CommandError {
    fn from(source: blaze_dist_local::Error) -> Self {
        Self::Carrier(source)
    }
}

impl From<IoError> for CommandError {
    fn from(source: IoError) -> Self {
        Self::Io(source)
    }
}

/// The coordinator a machine's daemons build through: `EXECUTORS` builders of one host triple,
/// each of the daemons' units one execution, until stdin closes or says `stop`. Its bounds are
/// a whole build's: the carrier's defaults are one client's. `LEASE_MS` is how long a holder may
/// be silent before its lease ends. What fences a holder or moves an attempt goes to stdout, one
/// line each, for the person or the harness reading what happened.
fn coordinate(args: &[String<WireAlloc>], alloc: WireAlloc) -> Result<(), CommandError> {
    let (path, triple, executors, lease) = match args {
        [path, triple, executors] => (path.as_str(), triple.as_str(), executors.as_str(), None),
        [path, triple, executors, lease] => {
            (path.as_str(), triple.as_str(), executors.as_str(), Some(lease.as_str()))
        }
        _ => return Err(CommandError::Usage),
    };
    let executors: u32 = executors
        .parse()
        .map_err(|source| CommandError::InvalidNumber { argument: "EXECUTORS", source })?;
    let defaults = blaze_dist::work::WorkConfig::default();
    let lease_ms = match lease {
        Some(it) => it
            .parse()
            .map_err(|source| CommandError::InvalidNumber { argument: "LEASE_MS", source })?,
        None => defaults.lease_ms,
    };
    let need = blaze_dist_local::agent::NEED;
    let mut admission = Admission::new(
        AdmissionConfig {
            queue_limit: 1 << 16,
            busy_retry_ms: 100,
            ..AdmissionConfig::default()
        },
        alloc,
    );
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
        admission.add_builder(0, "machine", need.clone(), alloc).ok_or(CommandError::MachineFull)?;
    }
    let jobs = Jobs::new(
        admission,
        ExecutionConfig {
            work: blaze_dist::work::WorkConfig { grant_roots: 64, lease_ms, ..defaults },
            completion: CompletionMode::PublishedWork,
            ..Default::default()
        },
        alloc,
    );
    let stop = Arc::new_in(AtomicBool::new(false), alloc);
    let server = Server::bind(
        path,
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
        Arc::new_in(MonotonicClock::default(), alloc),
        move |effects: &mut dyn Iterator<Item = (blaze_wire::ExecutionId, blaze_dist::jobs::Effect)>| {
            for (execution, effect) in effects {
                report(execution, &effect, alloc);
            }
        },
        alloc,
    )?;
    let signal = stop.clone();
    let _stdin_watcher = spawn(
        move || {
            loop {
                match read_stdin_line(alloc) {
                    Ok(Some(line)) if line.as_str() == "stop" => break,
                    Ok(Some(_)) => {}
                    Ok(None) | Err(_) => break,
                }
            }
            signal.store(true, Ordering::Release);
        },
        alloc,
    )?;
    let ready = format_in!(alloc, "ready {path}\n");
    write_stdout(ready.as_bytes());
    server.run(&stop)?;
    Ok(())
}

/// One line for what took work back from a holder. Everything else is the ordinary run.
fn report(execution: blaze_wire::ExecutionId, effect: &blaze_dist::jobs::Effect, alloc: WireAlloc) {
    use blaze_dist::jobs::Effect;
    use blaze_dist::work::Effect as Work;
    let line = match effect {
        Effect::NeedLead => format_in!(alloc, "execution {}: lead-lost\n", execution.0),
        Effect::Kill { executor, assignment } => {
            format_in!(
                alloc,
                "execution {}: kill {executor} attempt {}\n",
                execution.0,
                assignment.attempt.0
            )
        }
        Effect::Work(Work::Refused { holder, refusal }) => {
            format_in!(alloc, "execution {}: refused {holder}: {refusal:?}\n", execution.0)
        }
        Effect::Work(Work::HolderLost { holder, .. }) => {
            format_in!(alloc, "execution {}: holder-lost {holder}\n", execution.0)
        }
        Effect::Work(Work::Fenced { holder, generation, .. }) => {
            format_in!(
                alloc,
                "execution {}: fenced {holder} generation {generation}\n",
                execution.0
            )
        }
        _ => return,
    };
    write_stdout(line.as_bytes());
}

fn run(alloc: WireAlloc) -> Result<(), CommandError> {
    let args = process_args(alloc).map_err(IoError::from_errno)?;
    if args.get(1).map(String::as_str) != Some("coordinate") {
        return Err(CommandError::Usage);
    }
    let Some(rest) = args.get(2..) else {
        return Err(CommandError::Usage);
    };
    coordinate(rest, alloc)
}

fn main() -> Result<(), CliExit> {
    let heap = Heap::process();
    let alloc = heap.shared();
    if let Err(error) = run(alloc) {
        let msg = format_in!(alloc, "blaze-dist-local: {error}\n");
        write_stderr(msg.as_bytes());
        return Err(CliExit);
    }
    Ok(())
}
