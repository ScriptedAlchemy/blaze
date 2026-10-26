//! Every type round-trips byte-identically, its digest and its signature match the golden vectors
//! checked in, and the validator accepts what the encoder writes.

use blaze_wire::vectors::{Entry, Vector, every};
use blaze_wire::{Limits, validate};

fn limits() -> Limits {
    Limits::DOCUMENTED
}

/// Where a type's golden vector is checked in.
fn golden_path(name: &str) -> String {
    format!("{}/tests/golden/{name}.txt", env!("CARGO_MANIFEST_DIR"))
}

fn entries() -> Vec<Entry> {
    every()
}

#[test]
fn every_type_round_trips_byte_identically() {
    let mut checked = 0;
    for entry in entries() {
        for seed in 1..=64u64 {
            if let Err(e) = (entry.round_trip)(seed) {
                panic!("{} seed {seed}: {e}", entry.name);
            }
            checked += 1;
        }
    }
    assert!(checked > 0, "no type was checked");
    assert_eq!(checked, entries().len() * 64);
}

#[test]
fn the_validator_accepts_every_value_the_encoder_writes() {
    let mut checked = 0;
    for entry in entries() {
        for seed in 1..=128u64 {
            let cbor = (entry.vector)(seed).cbor;
            if let Err(e) = validate(&cbor, &limits()) {
                panic!("{} seed {seed}: the validator refused the encoder: {e}", entry.name);
            }
            checked += 1;
        }
    }
    assert!(checked > 0);
}

#[test]
fn golden_vectors_match() {
    for entry in entries() {
        let name = entry.name;
        let text = std::fs::read_to_string(golden_path(name))
            .unwrap_or_else(|e| panic!("{name}: no golden file: {e}"));
        let golden = Vector::from_golden(&text)
            .unwrap_or_else(|| panic!("{name}: the golden file is not a vector"));
        let made = (entry.vector)(1);
        assert_eq!(golden.name, name, "{name}: the golden file names another type");
        assert_eq!(
            blaze_wire::json::hex(&made.cbor),
            blaze_wire::json::hex(&golden.cbor),
            "{name}: the encoding differs from the golden vector"
        );
        assert_eq!(
            blaze_wire::json::hex(&made.digest),
            blaze_wire::json::hex(&golden.digest),
            "{name}: the digest differs from the golden vector"
        );
        assert_eq!(
            blaze_wire::json::hex(&made.signature),
            blaze_wire::json::hex(&golden.signature),
            "{name}: the signature differs from the golden vector"
        );
        assert_eq!(
            blaze_wire::json::hex(&made.key),
            blaze_wire::json::hex(&golden.key),
            "{name}: the key differs from the golden vector"
        );
        assert!(golden.digest_matches(), "{name}: the golden digest is not its CBOR's");
        assert!(golden.signature_verifies(), "{name}: the golden signature does not verify");
    }
}

#[test]
fn every_type_has_a_golden_file() {
    for entry in entries() {
        assert!(
            std::path::Path::new(&golden_path(entry.name)).exists(),
            "{}: no golden file at {}",
            entry.name,
            golden_path(entry.name)
        );
    }
}

#[test]
fn two_seeds_give_two_values() {
    // A type whose every field is fixed (an `Ack`, a `Limits` of no fields) gives one value from
    // every seed, which is right: the check is that the random types differ, so a test that walked
    // them with one seed would not see their fields at all.
    let mut fixed = Vec::new();
    for entry in entries() {
        if (entry.vector)(1).cbor == (entry.vector)(2).cbor {
            fixed.push(entry.name);
        }
    }
    // Only the types with no field at all may be fixed.
    assert_eq!(fixed, vec!["Ack"], "types with fields gave the same value from two seeds");
}

/// The number of types the schema has, so a type added without a golden vector is caught.
#[test]
fn the_schema_has_every_type() {
    let names: Vec<&str> = entries().iter().map(|entry| entry.name).collect();
    assert!(names.len() >= 90, "the schema has {} types, fewer than the doc's", names.len());
    let mut sorted = names.clone();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(sorted.len(), names.len(), "two types have the same name");
}
