//! Every type round-trips byte-identically, its digest matches the golden vectors
//! checked in, and the validator accepts what the encoder writes.

#![no_std]
#![feature(allocator_api)]

use blaze_own::Vec;
use blaze_wire::vectors::{Entry, Vector, every};
use blaze_wire::{Limits, WireAlloc, validate};

fn heap() -> WireAlloc {
    blaze_own::testing::heap().shared()
}

fn limits() -> Limits {
    Limits::DOCUMENTED
}

fn entries() -> [Entry; 43] {
    every()
}

const GOLDENS: [(&str, &str); 43] = [
    ("Ack", include_str!("golden/Ack.txt")),
    ("AssignedEvent", include_str!("golden/AssignedEvent.txt")),
    ("Attempt", include_str!("golden/Attempt.txt")),
    ("BatchResult", include_str!("golden/BatchResult.txt")),
    ("DoneEvent", include_str!("golden/DoneEvent.txt")),
    ("EdgeKey", include_str!("golden/EdgeKey.txt")),
    ("EntryKey", include_str!("golden/EntryKey.txt")),
    ("EntryRef", include_str!("golden/EntryRef.txt")),
    ("EventItem", include_str!("golden/EventItem.txt")),
    ("ExecutionId", include_str!("golden/ExecutionId.txt")),
    ("Found", include_str!("golden/Found.txt")),
    ("Grant", include_str!("golden/Grant.txt")),
    ("ItemKey", include_str!("golden/ItemKey.txt")),
    ("ItemOutcome", include_str!("golden/ItemOutcome.txt")),
    ("ItemResult", include_str!("golden/ItemResult.txt")),
    ("JobAnswer", include_str!("golden/JobAnswer.txt")),
    ("JobContent", include_str!("golden/JobContent.txt")),
    ("JobContentInput", include_str!("golden/JobContentInput.txt")),
    ("JobEvent", include_str!("golden/JobEvent.txt")),
    ("JobRequest", include_str!("golden/JobRequest.txt")),
    ("Lease", include_str!("golden/Lease.txt")),
    ("LocalPolicy", include_str!("golden/LocalPolicy.txt")),
    ("Namespace", include_str!("golden/Namespace.txt")),
    ("Outcome", include_str!("golden/Outcome.txt")),
    ("OutputEvent", include_str!("golden/OutputEvent.txt")),
    ("PackRef", include_str!("golden/PackRef.txt")),
    ("PrNamespace", include_str!("golden/PrNamespace.txt")),
    ("RefusedEvent", include_str!("golden/RefusedEvent.txt")),
    ("Reservation", include_str!("golden/Reservation.txt")),
    ("RootSet", include_str!("golden/RootSet.txt")),
    ("Stage", include_str!("golden/Stage.txt")),
    ("StageEvent", include_str!("golden/StageEvent.txt")),
    ("Why", include_str!("golden/Why.txt")),
    ("WireError", include_str!("golden/WireError.txt")),
    ("WorkJoinAnswer", include_str!("golden/WorkJoinAnswer.txt")),
    ("WorkJoinRequest", include_str!("golden/WorkJoinRequest.txt")),
    ("WorkKey", include_str!("golden/WorkKey.txt")),
    ("WorkLeaseAnswer", include_str!("golden/WorkLeaseAnswer.txt")),
    ("WorkLeaseRequest", include_str!("golden/WorkLeaseRequest.txt")),
    ("WorkName", include_str!("golden/WorkName.txt")),
    ("WorkResultAnswer", include_str!("golden/WorkResultAnswer.txt")),
    ("WorkersEventsAnswer", include_str!("golden/WorkersEventsAnswer.txt")),
    ("WorkersEventsRequest", include_str!("golden/WorkersEventsRequest.txt")),
];

fn golden_text(name: &str) -> Option<&'static str> {
    GOLDENS.iter().find(|(k, _)| *k == name).map(|(_, v)| *v)
}

#[test]
fn every_type_round_trips_byte_identically() {
    let alloc = heap();
    let mut checked = 0;
    for entry in entries() {
        for seed in 1..=64u64 {
            if let Err(e) = (entry.round_trip)(seed, alloc) {
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
    let alloc = heap();
    let mut checked = 0;
    for entry in entries() {
        for seed in 1..=128u64 {
            let cbor = (entry.vector)(seed, alloc).cbor;
            if let Err(e) = validate(&cbor, &limits()) {
                panic!("{} seed {seed}: the validator refused the encoder: {e}", entry.name);
            }
            checked += 1;
        }
    }
    assert!(checked > 0);
}

#[test]
fn every_type_has_a_golden_file() {
    for entry in entries() {
        assert!(golden_text(entry.name).is_some(), "{}: no golden file", entry.name);
    }
}

#[test]
fn two_seeds_give_two_values() {
    let alloc = heap();
    let mut fixed = Vec::new_in(alloc);
    for entry in entries() {
        if (entry.vector)(1, alloc).cbor.as_slice() == (entry.vector)(2, alloc).cbor.as_slice() {
            fixed.push(entry.name);
        }
    }
    assert_eq!(fixed.as_slice(), &["Ack"], "types with fields gave the same value from two seeds");
}

#[test]
fn the_schema_has_every_type() {
    let alloc = heap();
    let mut names = Vec::new_in(alloc);
    for entry in entries() {
        names.push(entry.name);
    }
    assert_eq!(names.len(), 43, "a type added or removed: its golden file and the doc go with it");
    let mut sorted = names.clone();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(sorted.len(), names.len(), "two types have the same name");
}

#[test]
fn golden_vectors_match() {
    let alloc = heap();
    for entry in entries() {
        let name = entry.name;
        let made = (entry.vector)(1, alloc);
        let text = golden_text(name).unwrap_or_else(|| panic!("{name}: no golden file"));
        let golden = Vector::from_golden(text, alloc)
            .unwrap_or_else(|| panic!("{name}: the golden file is not a vector"));
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
        assert!(golden.digest_matches(), "{name}: the golden digest is not its CBOR's");
    }
}
