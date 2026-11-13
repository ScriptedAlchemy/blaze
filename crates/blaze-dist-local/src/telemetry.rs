//! Optional append-only evidence of actual distributed transitions. Every record is one
//! locked append so processes sharing BLAZE_DIST_EVENTS cannot interleave JSON lines.

use std::fs::{File, OpenOptions, TryLockError};
use std::io::{self, Write};
use std::time::{Duration, Instant};

/// Render a field through the existing wire JSON renderer, including string escaping.
pub fn json<T: blaze_wire::Wire>(value: &T) -> String {
    let mut out = String::new();
    blaze_wire::json::render(value, &mut out);
    out
}

/// Append one real transition promptly when evidence was requested. Failure is visible and
/// the acceptance harness refuses missing evidence; production compilation remains independent.
pub fn emit(event: &str, fields: &[(&str, String)]) {
    let Some(path) = std::env::var_os("BLAZE_DIST_EVENTS") else { return };
    let append = || -> io::Result<()> {
        let mut file = OpenOptions::new().create(true).append(true).open(path)?;
        let bound = Instant::now() + Duration::from_secs(2);
        let _waiting =
            tracing::info_span!(target: "blaze", "dist: appending transition evidence").entered();
        loop {
            match file.try_lock() {
                Ok(()) => break,
                Err(TryLockError::WouldBlock) if Instant::now() < bound => {
                    std::thread::sleep(Duration::from_millis(1))
                }
                Err(TryLockError::WouldBlock) => {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "distributed evidence append lock exceeded 2 seconds",
                    ));
                }
                Err(TryLockError::Error(error)) => return Err(error),
            }
        }
        let mut line = format!("{{\"event\":{}", json(&event.to_owned()));
        for (name, value) in fields {
            line.push(',');
            line.push_str(&json(&name.to_string()));
            line.push(':');
            line.push_str(value);
        }
        line.push_str("}\n");
        let wrote = file.write_all(line.as_bytes());
        let unlocked = File::unlock(&file);
        wrote.and(unlocked)
    };
    if let Err(why) = append() {
        tracing::warn!(target: "blaze", %why, "distributed transition evidence was not written");
    }
}
