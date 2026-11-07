//! Local transport operator/test client; no compiler or automatic fallback command.

use std::io::{self, BufRead};
use std::path::Path;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use blaze_dist::admission::{Admission, AdmissionConfig};
use blaze_dist::jobs::{ExecutionConfig, Jobs};
use blaze_dist_local::client::Client;
use blaze_dist_local::coordinator::Config;
use blaze_dist_local::protocol::Command;
use blaze_dist_local::server::{MonotonicClock, Server, random_id};
use blaze_wire::{Limits, decode, encode};

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn unhex(text: &str) -> Result<Vec<u8>, String> {
    if !text.len().is_multiple_of(2) {
        return Err("odd hex length".to_owned());
    }
    text.as_bytes()
        .chunks_exact(2)
        .map(|pair| {
            let s = std::str::from_utf8(pair).map_err(|_| "hex UTF8")?;
            u8::from_str_radix(s, 16).map_err(|_| "invalid hex".to_owned())
        })
        .collect()
}

/// The coordinator a machine's daemons build through: `EXECUTORS` builders of one host triple,
/// each of the daemons' units one execution, until stdin closes or says `stop`. Its bounds are
/// a whole build's: the carrier's defaults are one client's.
fn coordinate(args: &[std::ffi::OsString]) -> Result<(), Box<dyn std::error::Error>> {
    let [path, triple, executors] = args else {
        return Err("usage: blaze-dist-local coordinate SOCKET TRIPLE EXECUTORS".into());
    };
    let (triple, executors): (&str, u32) = (
        triple.to_str().ok_or("TRIPLE is not text")?,
        executors.to_str().ok_or("EXECUTORS is not text")?.parse()?,
    );
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
        admission.add_builder(0, "machine", need.clone()).ok_or("the machine has no room")?;
    }
    let jobs = Jobs::new(
        admission,
        ExecutionConfig {
            work: blaze_dist::work::WorkConfig { grant_roots: 64, ..Default::default() },
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
            max_requests: 1 << 20,
            replay_bytes: 1 << 30,
            max_executions: 1 << 16,
            max_events: 1 << 20,
            event_bytes: 1 << 30,
            work_bytes: 1 << 30,
            session_ms: 6 * 3_600_000,
            ..Config::default()
        },
        Arc::new(MonotonicClock::default()),
        |_: Vec<(blaze_wire::ExecutionId, blaze_dist::jobs::Effect)>| {},
    )?;
    println!("ready {}", Path::new(path).display());
    server.run(stop)?;
    Ok(())
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args_os().collect();
    if args.get(1).and_then(|it| it.to_str()) == Some("coordinate") {
        return coordinate(&args[2..]);
    }
    if args.len() != 3 {
        return Err(
            "usage: blaze-dist-local serve|hello|client SOCKET, or coordinate SOCKET TRIPLE EXECUTORS".into()
        );
    }
    let path = Path::new(&args[2]);
    match args[1].to_str() {
        Some("serve") => {
            // No configured worker => no fabricated availability/completion. An embedding
            // agent configures admission and supplies an actual holder callback instead.
            let jobs =
                Jobs::new(Admission::new(AdmissionConfig::default()), ExecutionConfig::default());
            let stop = Arc::new(AtomicBool::new(false));
            let signal = stop.clone();
            std::thread::spawn(move || {
                for line in io::stdin().lock().lines() {
                    if line.is_ok_and(|line| line == "stop") {
                        signal.store(true, Ordering::Release);
                        return;
                    }
                }
                signal.store(true, Ordering::Release);
            });
            let server = Server::bind(
                path,
                jobs,
                Config::default(),
                Arc::new(MonotonicClock::default()),
                |effects: Vec<(blaze_wire::ExecutionId, blaze_dist::jobs::Effect)>| {
                    if !effects.is_empty() {
                        eprintln!("holder callback: {effects:?}");
                    }
                },
            )?;
            println!("ready {}", path.display());
            server.run(stop)?;
        }
        Some("hello") => {
            let mut client = Client::connect(path, random_id()?)?;
            println!("{}", hex(&encode(&client.welcome)));
            // The daemon issued this lifetime; give it back so repeated `hello` runs do not
            // consume the fixed lifetime of the daemon and exhaust live capacity.
            client.release()?;
        }
        Some("client") => {
            let mut client = Client::connect(path, random_id()?)?;
            println!("welcome {}", hex(&encode(&client.welcome)));
            let mut failed = None;
            for line in io::stdin().lock().lines() {
                let bytes = unhex(line?.trim())?;
                let command: Command = decode(&bytes, &Limits::DOCUMENTED)?;
                match client.call(command) {
                    Ok(answer) => println!("{}", hex(&encode(&answer))),
                    Err(error) => {
                        failed = Some(error);
                        break;
                    }
                }
            }
            // Release only a clean lifetime: a pending request stays pending so its exact
            // bytes are never migrated into a new session, and the error is reported.
            match failed {
                Some(error) => return Err(error.into()),
                None => client.release()?,
            }
        }
        _ => return Err("unknown local transport mode".into()),
    }
    Ok(())
}

fn main() {
    if let Err(error) = run() {
        eprintln!("blaze-dist-local: {error}");
        std::process::exit(1);
    }
}
