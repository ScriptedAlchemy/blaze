//! An explicit acceptance-test gate for one actual holder result. With no configured gate,
//! the driver allocates no capture and performs no file operation. The gate has no authority:
//! it records the real command, pauses its sender's heartbeats, then sends that same command
//! through Client. Only the coordinator decides whether the issued lease is still live.

use core::time::Duration;

use blaze_own::{String, Vec, format_in};
use blaze_sha256::digest as sha256_digest;
use blaze_wire::{WireAlloc, WireString, encode};

use crate::Error;
use crate::protocol::{Answer, Command};
use crate::sys::{
    Errno, IoError, SystemClock, current_pid, env_var, join_path, open_create_new, rename_path,
    sleep, stat_path, unlink_path, write_all_fd,
};
use crate::telemetry::{emit, json, quoted};

/// An opt-in test gate, shared by the units of one daemon and claimed once. The caller
/// supplies a new empty directory and a finite timeout; retained captures are evidence,
/// never durable coordinator state and never input to a later build.
#[derive(Clone, Debug)]
pub struct ResultGate {
    directory: WireString,
    clock: SystemClock,
    deadline_ns: u64,
    timeout: Duration,
    alloc: WireAlloc,
}

/// One claimed result. The gate outlives the normal Client call and its recorded response.
pub(crate) struct Capture<'gate> {
    directory: &'gate str,
    alloc: WireAlloc,
}

impl ResultGate {
    /// Select one result in a fresh absolute directory, by a deadline starting now. Time
    /// spent compiling before capture and time SIGSTOPped both consume this finite budget.
    pub fn new(directory: WireString, timeout: Duration, alloc: WireAlloc) -> Result<Self, Error> {
        if !directory.as_str().starts_with('/') || timeout.is_zero() {
            return Err(Error::Protocol(format_in!(
                alloc,
                "test result gate {directory}: needs an absolute directory and a positive timeout"
            )));
        }
        let clock = SystemClock;
        let now = clock.monotonic_nanos()?;
        let timeout_ns = u64::try_from(timeout.as_nanos()).map_err(|_| {
            Error::Protocol(format_in!(
                alloc,
                "test result gate {directory}: timeout does not make a finite deadline"
            ))
        })?;
        let deadline_ns = now.checked_add(timeout_ns).ok_or_else(|| {
            Error::Protocol(format_in!(
                alloc,
                "test result gate {directory}: timeout does not make a finite deadline"
            ))
        })?;
        Ok(Self { directory, clock, deadline_ns, timeout, alloc })
    }

    /// The daemon-only test setting. Normal requests never select a gate from their input
    /// environment; the agent enables this only for a current lead, before it loses authority.
    pub(crate) fn from_env(timeout: Duration, alloc: WireAlloc) -> Result<Option<Self>, Error> {
        let Some(directory) = env_var("BLAZE_DIST_TEST_RESULT_GATE", alloc) else {
            return Ok(None);
        };
        Self::new(directory, timeout, alloc).map(Some)
    }

    pub(crate) fn hold(&self, command: &Command) -> Result<Option<Capture<'_>>, Error> {
        let Command::Result(submission) = command else { return Ok(None) };
        let claimed_path = join_path(self.directory.as_str(), "claimed", self.alloc);
        // An exclusive creation is the whole one-shot selection. No lock or open file is
        // retained across the wait. An old claim is intentionally never reused or erased.
        match open_create_new(claimed_path.as_str()) {
            Ok(file) => drop(file),
            Err(Errno::Exist) => return Ok(None),
            Err(why) => return Err(gate_io(self.directory.as_str(), why, self.alloc)),
        }
        let capture = Capture { directory: self.directory.as_str(), alloc: self.alloc };
        let request = &submission.request;
        let lease = &request.lease;
        let bytes = encode(command, self.alloc);
        let digest: blaze_wire::Digest = sha256_digest(&bytes);
        let mut work = Vec::new_in(self.alloc);
        work.extend(request.items.iter().map(|item| item.name.0));
        let pid = current_pid();
        let fields = [
            ("pid", json(&pid, self.alloc)),
            ("execution", json(&lease.execution.0, self.alloc)),
            ("attempt", json(&lease.attempt.0, self.alloc)),
            ("holder", json(&lease.holder, self.alloc)),
            ("generation", json(&lease.generation, self.alloc)),
            ("seq", json(&request.seq, self.alloc)),
            ("work", json(&work, self.alloc)),
            ("command_sha256", json(&digest, self.alloc)),
        ];
        let result = (|| {
            capture.write("result.cbor", &bytes)?;
            // The wire renderer reads the canonical CBOR. This view lets the harness bind
            // every captured lease field and item to its real grant without another codec.
            let mut ready = String::from_str_in("{\"event\":\"result_held\"", self.alloc);
            for (name, value) in &fields {
                ready.push(',');
                let _ = quoted(name, &mut ready, self.alloc);
                ready.push(':');
                ready.push_str(value.as_str());
            }
            ready.push_str(",\"command\":");
            ready.push_str(json(command, self.alloc).as_str());
            ready.push_str("}\n");
            capture.write("ready.json", ready.as_bytes())?;
            emit("result_held", &fields, self.alloc);
            let release_path = join_path(self.directory.as_str(), "release", self.alloc);
            loop {
                // Check expiry first: an old release file cannot revive a timed-out gate.
                let now = self.clock.monotonic_nanos()?;
                if now >= self.deadline_ns {
                    return Err(Error::Protocol(format_in!(
                        self.alloc,
                        "test result gate {}: release did not arrive within {:?}",
                        self.directory,
                        self.timeout
                    )));
                }
                match stat_path(release_path.as_str()) {
                    Ok(metadata) if metadata.is_file => return Ok(()),
                    Ok(_) => {
                        return Err(Error::Protocol(format_in!(
                            self.alloc,
                            "test result gate {}: release is not a file",
                            self.directory
                        )));
                    }
                    Err(Errno::NoEnt) => {}
                    Err(why) => return Err(gate_io(self.directory.as_str(), why, self.alloc)),
                }
                let remaining_ns = self.deadline_ns.saturating_sub(now);
                let step_ns = remaining_ns.min(5_000_000);
                sleep(Duration::from_nanos(step_ns));
            }
        })();
        if let Err(why) = &result {
            capture.error(why)?;
        }
        result.map(|()| Some(capture))
    }
}

impl Capture<'_> {
    fn write(&self, name: &str, bytes: &[u8]) -> Result<(), Error> {
        let target = join_path(self.directory, name, self.alloc);
        let tmp_name = format_in!(self.alloc, ".{name}.tmp");
        let temporary = join_path(self.directory, tmp_name.as_str(), self.alloc);
        let mut created = false;
        let result = (|| -> Result<(), Errno> {
            let file = open_create_new(temporary.as_str())?;
            created = true;
            write_all_fd(file.fd(), bytes)?;
            drop(file);
            rename_path(temporary.as_str(), target.as_str())
        })();
        if let Err(why) = result {
            // A failed capture leaves its claim and complete evidence, but no partial view
            // for the harness to mistake for a result. Never remove an earlier run's files.
            if created {
                let _ = unlink_path(temporary.as_str());
            }
            return Err(gate_io(target.as_str(), why, self.alloc));
        }
        Ok(())
    }

    pub(crate) fn answer(&self, answer: &Answer) -> Result<(), Error> {
        let bytes = encode(answer, self.alloc);
        let digest: blaze_wire::Digest = sha256_digest(&bytes);
        self.write("answer.cbor", &bytes)?;
        let rendered = format_in!(
            self.alloc,
            "{{\"sha256\":{},\"answer\":{}}}\n",
            json(&digest, self.alloc),
            json(answer, self.alloc)
        );
        self.write("answer.json", rendered.as_bytes())
    }

    pub(crate) fn error(&self, why: &Error) -> Result<(), Error> {
        let text = format_in!(self.alloc, "{why}\n");
        self.write("error.txt", text.as_bytes())
    }
}

fn gate_io(path: &str, why: Errno, alloc: WireAlloc) -> Error {
    Error::Io(IoError::with_message(
        why,
        format_in!(alloc, "test result gate {path}: {} ({})", why.what(), why.name()),
    ))
}

#[cfg(test)]
mod tests;
