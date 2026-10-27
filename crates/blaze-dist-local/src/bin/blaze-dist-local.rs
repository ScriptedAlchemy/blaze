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

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args_os().collect();
    if args.len() != 3 {
        return Err("usage: blaze-dist-local serve|hello|client SOCKET".into());
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
            let client = Client::connect(path, random_id()?)?;
            println!("{}", hex(&encode(&client.welcome)));
        }
        Some("client") => {
            let mut client = Client::connect(path, random_id()?)?;
            println!("welcome {}", hex(&encode(&client.welcome)));
            for line in io::stdin().lock().lines() {
                let bytes = unhex(line?.trim())?;
                let command: Command = decode(&bytes, &Limits::DOCUMENTED)?;
                let answer = client.call(command)?;
                println!("{}", hex(&encode(&answer)));
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
