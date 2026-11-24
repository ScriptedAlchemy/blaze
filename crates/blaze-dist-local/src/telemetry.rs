//! Optional append-only evidence of actual distributed transitions. Every record is one
//! locked append so processes sharing `BLAZE_DIST_EVENTS` cannot interleave JSON lines.

use core::alloc::Allocator;
use core::fmt;

use blaze_own::String;
use blaze_wire::WireAlloc;

use crate::sys::{
    Errno, SystemClock, env_var, open_create_append, sleep_ms, try_flock_exclusive, unlock_flock,
    write_all_fd,
};

/// Maximum duration to wait for the exclusive append lock on `BLAZE_DIST_EVENTS`.
const LOCK_TIMEOUT_NS: u64 = 2_000_000_000;

/// Render a field through the existing wire JSON renderer, including string escaping.
pub fn json<T: blaze_wire::Wire, A: Allocator + Clone>(value: &T, alloc: A) -> String<A> {
    let mut out = String::new_in(alloc.clone());
    let _ = blaze_wire::json::render(value, &mut out, alloc);
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
) -> String<A> {
    let mut line = String::new_in(alloc.clone());
    line.push_str("{\"event\":");
    let _ = quoted(event, &mut line, alloc.clone());
    for (name, value) in fields {
        line.push(',');
        let _ = quoted(name, &mut line, alloc.clone());
        line.push(':');
        line.push_str(value.as_ref());
    }
    line.push_str("}\n");
    line
}

/// Append one real transition promptly when evidence was requested. Failure is visible and
/// the acceptance harness refuses missing evidence; production compilation remains independent.
pub fn emit(event: &str, fields: &[(&str, impl AsRef<str>)], alloc: WireAlloc) {
    let Some(path) = env_var("BLAZE_DIST_EVENTS", alloc) else { return };
    let _ = append_locked(path.as_str(), event, fields, alloc);
}

fn append_locked(
    path: &str,
    event: &str,
    fields: &[(&str, impl AsRef<str>)],
    alloc: WireAlloc,
) -> Result<(), Errno> {
    let file = open_create_append(path)?;
    let clock = SystemClock;
    let start = clock.monotonic_nanos()?;
    loop {
        if try_flock_exclusive(file.fd())? {
            break;
        }
        let now = clock.monotonic_nanos()?;
        if now.saturating_sub(start) >= LOCK_TIMEOUT_NS {
            return Err(Errno::TimedOut);
        }
        sleep_ms(1);
    }
    let record = line(event, fields, alloc);
    let wrote = write_all_fd(file.fd(), record.as_bytes());
    let unlocked = unlock_flock(file.fd());
    wrote.and(unlocked)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pack::test_alloc::{Counted, Counts};
    use blaze_own::{format_in, vec_in};
    use blaze_wire::WireString;

    #[test]
    fn evidence_keeps_wire_json_escaping_and_the_callers_allocator() {
        let heap = blaze_own::testing::heap().shared();
        let value = WireString::from_str_in("line\n\"\\λ", heap);
        let mut expected = String::new_in(heap);
        blaze_wire::json::render(&value, &mut expected, heap).unwrap();
        let counts = Counts::default();
        let alloc = Counted(&counts);
        let rendered = json(&value, alloc);
        assert!(core::ptr::eq(rendered.allocator().0, &counts));
        assert_eq!(rendered.as_str(), expected.as_str());
        let fields = [("count", json(&7u64, alloc)), ("quote\"", rendered)];
        let record = line("start\n\"λ", &fields, alloc);
        assert!(core::ptr::eq(record.allocator().0, &counts));
        assert_eq!(
            record.as_str(),
            "{\"event\":\"start\\n\\\"λ\",\"count\":7,\"quote\\\"\":\"line\\n\\\"\\\\λ\"}\n"
        );
        drop(fields);
        drop(record);
        assert_eq!(counts.allocations.get(), counts.deallocations.get());
    }

    #[test]
    fn digest_evidence_matches_the_previous_hex_string_fields() {
        let heap = blaze_own::testing::heap().shared();
        let digest = [0x0a; 32];
        let previous = format_in!(heap, "{}", blaze_wire::json::hex(&digest));
        assert_eq!(json(&digest, heap), json(&previous, heap));
        assert_eq!(
            json(&vec_in![heap; digest], heap),
            json(&vec_in![heap; previous], heap)
        );
    }
}
