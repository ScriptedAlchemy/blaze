//! An explicit acceptance-test gate for one actual holder result. With no configured gate,
//! the driver allocates no capture and performs no file operation. The gate has no authority:
//! it records the real command, pauses its sender's heartbeats, then sends that same command
//! through Client. Only the coordinator decides whether the issued lease is still live.

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use blaze_core::wait::{self, Clock, Deadline, Monotonic};
use blaze_wire::encode;
use sha2::{Digest as _, Sha256};

use crate::Error;
use crate::protocol::{Answer, Command};
use crate::telemetry::{emit, json};

/// An opt-in test gate, shared by the units of one daemon and claimed once. The caller
/// supplies a new empty directory and a finite timeout; retained captures are evidence,
/// never durable coordinator state and never input to a later build.
#[derive(Clone, Debug)]
pub struct ResultGate {
    directory: PathBuf,
    clock: Monotonic,
    deadline: Deadline,
    timeout: Duration,
}

/// One claimed result. Its real response is recorded after the normal Client call.
pub(crate) struct Capture {
    directory: PathBuf,
}

impl ResultGate {
    /// Select one result in a fresh absolute directory, by a deadline starting now. Time
    /// spent compiling before capture and time SIGSTOPped both consume this finite budget.
    pub fn new(directory: PathBuf, timeout: Duration) -> Result<Self, Error> {
        if !directory.is_absolute() || timeout.is_zero() {
            return Err(Error::Protocol(format!(
                "test result gate {}: needs an absolute directory and a positive timeout",
                directory.display()
            )));
        }
        let clock = Monotonic::new();
        let deadline = Deadline::after(clock.now(), timeout).finite().ok_or_else(|| {
            Error::Protocol(format!(
                "test result gate {}: timeout does not make a finite deadline",
                directory.display()
            ))
        })?;
        Ok(Self { directory, clock, deadline, timeout })
    }

    /// The daemon-only test setting. Normal requests never select a gate from their input
    /// environment; the agent enables this only for a current lead, before it loses authority.
    pub(crate) fn from_env(timeout: Duration) -> Result<Option<Self>, Error> {
        let Some(directory) = std::env::var_os("BLAZE_DIST_TEST_RESULT_GATE") else {
            return Ok(None);
        };
        Self::new(PathBuf::from(directory), timeout).map(Some)
    }

    pub(crate) fn hold(&self, command: &Command) -> Result<Option<Capture>, Error> {
        let Command::Result(submission) = command else { return Ok(None) };
        // An exclusive creation is the whole one-shot selection. No lock or open file is
        // retained across the wait. An old claim is intentionally never reused or erased.
        match OpenOptions::new().write(true).create_new(true).open(self.directory.join("claimed")) {
            Ok(file) => drop(file),
            Err(why) if why.kind() == io::ErrorKind::AlreadyExists => return Ok(None),
            Err(why) => return Err(gate_io(&self.directory, why)),
        }
        let capture = Capture { directory: self.directory.clone() }; // alloc: one retained test capture
        let request = &submission.request;
        let lease = &request.lease;
        let bytes = encode(command);
        let digest: blaze_wire::Digest = Sha256::digest(&bytes).into();
        let work: Vec<String> =
            request.items.iter().map(|item| crate::pack::hex(&item.name.0)).collect(); // alloc: bounded test evidence
        let fields = [
            ("pid", json(&std::process::id())),
            ("execution", json(&lease.execution.0)),
            ("attempt", json(&lease.attempt.0)),
            ("holder", json(&lease.holder)),
            ("generation", json(&lease.generation)),
            ("seq", json(&request.seq)),
            ("work", json(&work)),
            ("command_sha256", json(&crate::pack::hex(&digest))),
        ];
        let result = (|| {
            capture.write("result.cbor", &bytes)?;
            // The wire renderer reads the canonical CBOR. This view lets the harness bind
            // every captured lease field and item to its real grant without another codec.
            let mut ready = String::from("{\"event\":\"result_held\""); // alloc: one bounded capture view
            for (name, value) in &fields {
                ready.push_str(&format!(",{}:{}", json(&name.to_string()), value));
            }
            ready.push_str(&format!(",\"command\":{}}}\n", json(command)));
            capture.write("ready.json", ready.as_bytes())?;
            emit("result_held", &fields);
            let _waiting = tracing::info_span!(target: "blaze", "dist: test result gate release", path = %self.directory.display()).entered();
            loop {
                // Check expiry first: an old release file cannot revive a timed-out gate.
                let now = self.clock.now();
                if self.deadline.passed(now) {
                    return Err(Error::Protocol(format!(
                        "test result gate {}: release did not arrive within {:?}",
                        self.directory.display(),
                        self.timeout
                    )));
                }
                match fs::metadata(self.directory.join("release")) {
                    Ok(metadata) if metadata.is_file() => return Ok(()),
                    Ok(_) => {
                        return Err(Error::Protocol(format!(
                            "test result gate {}: release is not a file",
                            self.directory.display()
                        )));
                    }
                    Err(why) if why.kind() == io::ErrorKind::NotFound => {}
                    Err(why) => return Err(gate_io(&self.directory, why)),
                }
                let next = Deadline::at(
                    now.saturating_add(Duration::from_millis(5)).min(self.deadline.get()),
                );
                wait::sleep_until("dist: test result gate release", next, &self.clock).map_err(
                    |why| {
                        Error::Protocol(format!(
                            "test result gate {}: {why}",
                            self.directory.display()
                        ))
                    },
                )?;
            }
        })();
        if let Err(why) = &result {
            capture.error(why)?;
        }
        result.map(|()| Some(capture))
    }
}

impl Capture {
    fn write(&self, name: &str, bytes: &[u8]) -> Result<(), Error> {
        let target = self.directory.join(name);
        let temporary = self.directory.join(format!(".{name}.tmp"));
        let mut created = false;
        let result = (|| -> io::Result<()> {
            let mut file = OpenOptions::new().write(true).create_new(true).open(&temporary)?;
            created = true;
            file.write_all(bytes)?;
            drop(file);
            fs::rename(&temporary, &target)
        })();
        if let Err(why) = result {
            // A failed capture leaves its claim and complete evidence, but no partial view
            // for the harness to mistake for a result. Never remove an earlier run's files.
            if created {
                let _ = fs::remove_file(&temporary);
            }
            return Err(gate_io(&target, why));
        }
        Ok(())
    }

    pub(crate) fn answer(&self, answer: &Answer) -> Result<(), Error> {
        let bytes = encode(answer);
        let digest: blaze_wire::Digest = Sha256::digest(&bytes).into();
        self.write("answer.cbor", &bytes)?;
        self.write(
            "answer.json",
            format!(
                "{{\"sha256\":{},\"answer\":{}}}\n",
                json(&crate::pack::hex(&digest)),
                json(answer)
            )
            .as_bytes(),
        )
    }

    pub(crate) fn error(&self, why: &Error) -> Result<(), Error> {
        self.write("error.txt", format!("{why}\n").as_bytes())
    }
}

fn gate_io(path: &Path, why: io::Error) -> Error {
    Error::Io(io::Error::new(why.kind(), format!("test result gate {}: {why}", path.display())))
}

#[cfg(test)]
mod tests;
