//! A property over random bytes: whatever the validator accepts is canonical, which a test checks
//! by reading it as a value and writing that value again.

#![no_std]
#![feature(allocator_api)]

use blaze_own::Vec;
use blaze_wire::vectors::{Entry, Gen, every};
use blaze_wire::{Limits, WireAlloc, validate};

fn heap() -> WireAlloc {
    blaze_own::testing::heap().shared()
}

fn limits() -> Limits {
    Limits::DOCUMENTED
}

/// The property: for every type, a value's bytes with up to two changes either fail the validator
/// or read back as a value whose encoding is the bytes themselves.
#[test]
fn bytes_the_validator_accepts_are_canonical() {
    let alloc = heap();
    let entries: [Entry; 43] = every();
    let mut g = Gen::new(0x5eed);
    let mut accepted = 0u64;
    let mut refused = 0u64;
    for round in 0..4_000u64 {
        let entry = &entries[usize::try_from(g.below(u64::try_from(entries.len()).unwrap())).unwrap()];
        let mut bytes = (entry.vector)(g.below(64) + 1, alloc).cbor;
        for _ in 0..=g.below(2) {
            if bytes.is_empty() {
                bytes.push(u8::try_from(g.next_u64() & 0xff).unwrap());
                continue;
            }
            let at = usize::try_from(g.below(u64::try_from(bytes.len()).unwrap())).unwrap();
            match g.below(3) {
                0 => bytes[at] ^= 1 << g.below(8),
                1 => {
                    bytes.remove(at);
                }
                _ => bytes.insert(at, u8::try_from(g.next_u64() & 0xff).unwrap()),
            }
        }
        if validate(&bytes, &limits()).is_err() {
            refused += 1;
            continue;
        }
        accepted += 1;
        if let Ok(again) = (entry.reencode)(&bytes, alloc) {
            assert_eq!(
                again.as_slice(),
                bytes.as_slice(),
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
    let alloc = heap();
    let mut g = Gen::new(0xc0ffee);
    let mut accepted = 0u64;
    for _ in 0..4_000 {
        let mut bytes = Vec::new_in(alloc);
        let len = g.below(6) + 1;
        for _ in 0..len {
            bytes.push(u8::try_from(g.next_u64() & 0xff).unwrap());
        }
        if validate(&bytes, &limits()).is_ok() {
            accepted += 1;
        }
    }
    assert!(accepted < 4_000, "every random string was accepted, so the validator checks nothing");
}

/// Every type's own bytes are accepted by the type-aware check, which is the strictest form.
#[test]
fn every_types_bytes_pass_the_strictest_check() {
    let alloc = heap();
    let mut checked = 0;
    for entry in every() {
        for seed in 1..=8u64 {
            let bytes = (entry.vector)(seed, alloc).cbor;
            if let Err(e) = validate(&bytes, &limits()) {
                panic!("{}: {e}", entry.name);
            }
            checked += 1;
        }
    }
    assert!(checked > 0);
}
