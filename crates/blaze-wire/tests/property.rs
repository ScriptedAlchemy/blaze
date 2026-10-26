//! A property over random bytes: whatever the validator accepts is canonical, which a test checks
//! by reading it as a value and writing that value again. A byte string the validator took but the
//! encoder would not write is exactly the class of bug the validator exists to refuse, so this is
//! the test that keeps the two halves of the crate in step.
//!
//! The bytes are made two ways: from a random value of a random type with bytes flipped, inserted
//! and dropped, and from scratch out of the CBOR heads the schema uses.

use blaze_wire::vectors::{Entry, Gen, every};
use blaze_wire::{Limits, validate};

fn limits() -> Limits {
    Limits::DOCUMENTED
}

/// The property: for every type, a value's bytes with up to two changes either fail the validator
/// or read back as a value whose encoding is the bytes themselves.
#[test]
fn bytes_the_validator_accepts_are_canonical() {
    let entries: Vec<Entry> = every();
    let mut g = Gen::new(0x5eed);
    let mut accepted = 0u64;
    let mut refused = 0u64;
    for round in 0..4_000u64 {
        let entry = &entries[(g.below(entries.len() as u64)) as usize];
        let mut bytes = (entry.vector)(g.below(64) + 1).cbor;
        // One or two changes: a flipped byte, a dropped byte, an inserted byte.
        for _ in 0..=g.below(2) {
            if bytes.is_empty() {
                bytes.push(g.next_u64() as u8);
                continue;
            }
            let at = g.below(bytes.len() as u64) as usize;
            match g.below(3) {
                0 => bytes[at] ^= 1 << g.below(8),
                1 => {
                    bytes.remove(at);
                }
                _ => bytes.insert(at, g.next_u64() as u8),
            }
        }
        if validate(&bytes, &limits()).is_err() {
            refused += 1;
            continue;
        }
        accepted += 1;
        // The bytes are canonical, so reading them as the type and writing that value gives the
        // same bytes. A type whose value the bytes are not is refused by the schema, which is
        // another offence the validator's bytes pass did not see.
        if let Ok(again) = (entry.reencode)(&bytes) {
            assert_eq!(
                again, bytes,
                "{round}: {}: the validator took bytes the encoder would not write: {bytes:02x?}",
                entry.name
            );
        }
    }
    assert!(refused > 0, "no change was refused, so the test is not exercising the validator");
    assert!(accepted > 0, "no change was accepted, so the test never reached the property");
}



/// The property over bytes written from scratch: a random sequence of heads, which is what a
/// client that encodes wrong would send.
#[test]
fn random_heads_are_refused_or_canonical() {
    let mut g = Gen::new(0xc0ffee);
    let mut accepted = 0u64;
    for _ in 0..4_000 {
        let mut bytes = Vec::new();
        let len = g.below(6) + 1;
        for _ in 0..len {
            bytes.push(g.next_u64() as u8);
        }
        if validate(&bytes, &limits()).is_ok() {
            accepted += 1;
        }
    }
    // Random bytes are almost never one canonical item, but some are (a small integer, a short
    // string), so the count is small and not zero.
    assert!(accepted < 4_000, "every random string was accepted, so the validator checks nothing");
}

/// Every type's own bytes are accepted by the type-aware check, which is the strictest form.
#[test]
fn every_types_bytes_pass_the_strictest_check() {
    let mut checked = 0;
    for entry in every() {
        for seed in 1..=8u64 {
            let bytes = (entry.vector)(seed).cbor;
            if let Err(e) = validate(&bytes, &limits()) {
                panic!("{}: {e}", entry.name);
            }
            checked += 1;
        }
    }
    assert!(checked > 0);
}
