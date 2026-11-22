//! Optional append-only evidence of actual distributed transitions. Every record is one
//! locked append so processes sharing BLAZE_DIST_EVENTS cannot interleave JSON lines.

use std::alloc::{Allocator, Global};
use std::fmt;
use std::fs::{File, OpenOptions, TryLockError};
use std::io::{self, Write};
use std::time::{Duration, Instant};

/// Render a field through the existing wire JSON renderer, including string escaping.
pub fn json<T: blaze_wire::Wire, A: Allocator + Clone>(
    value: &T,
    alloc: A,
) -> blaze_mem::String<A> {
    let mut out = blaze_mem::String::new_in(alloc.clone());
    blaze_wire::json::render(value, &mut out, alloc)
        .expect("formatting into a string cannot fail");
    out
}

/// Render borrowed text with the wire's own JSON escaping, without an owned text copy.
pub(crate) fn quoted<A: Allocator + Clone>(
    text: &str,
    out: &mut dyn fmt::Write,
    alloc: A,
) -> fmt::Result {
    let mut encoded = blaze_wire::Encoder::new(alloc);
    encoded.text(text);
    blaze_wire::json::render_cbor(&encoded.into_bytes(), out)
}

fn line<A: Allocator + Clone>(
    event: &str,
    fields: &[(&str, impl AsRef<str>)],
    alloc: A,
) -> blaze_mem::String<A> {
    let mut line = blaze_mem::String::new_in(alloc.clone());
    line.push_str("{\"event\":");
    quoted(event, &mut line, alloc.clone()).expect("formatting into a string cannot fail");
    for (name, value) in fields {
        line.push(',');
        quoted(name, &mut line, alloc.clone()).expect("formatting into a string cannot fail");
        line.push(':');
        line.push_str(value.as_ref());
    }
    line.push_str("}\n");
    line
}

/// Append one real transition promptly when evidence was requested. Failure is visible and
/// the acceptance harness refuses missing evidence; production compilation remains independent.
pub fn emit(event: &str, fields: &[(&str, impl AsRef<str>)]) {
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
        let line = line(event, fields, Global);
        let wrote = file.write_all(line.as_bytes());
        let unlocked = File::unlock(&file);
        wrote.and(unlocked)
    };
    if let Err(why) = append() {
        tracing::warn!(target: "blaze", %why, "distributed transition evidence was not written");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn evidence_keeps_wire_json_escaping_and_the_callers_allocator() {
        let value = "line\n\"\\λ".to_owned();
        let mut expected = String::new();
        blaze_wire::json::render(&value, &mut expected, Global).unwrap();
        blaze_mem::frame::task(blaze_mem::Tag::Plane, |alloc| {
            let rendered = json(&value, alloc);
            assert!(std::ptr::eq(*rendered.allocator(), alloc));
            assert_eq!(rendered.as_str(), expected);
            let fields = [("count", json(&7u64, alloc)), ("quote\"", rendered)];
            let record = line("start\n\"λ", &fields, alloc);
            assert!(std::ptr::eq(*record.allocator(), alloc));
            assert_eq!(
                record.as_str(),
                "{\"event\":\"start\\n\\\"λ\",\"count\":7,\"quote\\\"\":\"line\\n\\\"\\\\λ\"}\n"
            );
        });
    }

    #[test]
    fn digest_evidence_matches_the_previous_hex_string_fields() {
        let digest = [0x0a; 32];
        let previous = blaze_wire::json::hex(&digest).to_string();
        assert_eq!(json(&digest, Global), json(&previous, Global));
        assert_eq!(json(&vec![digest], Global), json(&vec![previous], Global));
    }
}
