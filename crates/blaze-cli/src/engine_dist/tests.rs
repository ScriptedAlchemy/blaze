//! The distributed code store uses the real home store over a deterministic filesystem.
//! A batch exports the current value of each touched key and every pointer target, while
//! locally accepted bytes have only their home-store allocation.

use super::*;
use blaze_cas::plain::{Lent, Settings};
use blaze_core::{FileName, sim::{Faults, Sim}};
use blaze_own::{BTreeMap, CollectIn};
use crate::store::plain_place::{Facts, StoreAlloc, after_answer, config, open, test_cx};

fn process(sim: Sim, nonce: u64) -> Lent<Sim, StoreAlloc> {
    let alloc = blaze_own::testing::heap().alloc();
    let cfg = config(
        FileName::new("distributed-store").unwrap(),
        &Settings::default(),
        Facts { boot: Some(1), nonce },
        alloc,
    );
    open(sim, cfg, alloc).unwrap()
}

fn has_pointer_and_target(values: &[Value], pointer: CodeKey, content: CodeKey, bytes: &[u8]) -> bool {
    values.iter().any(|value| {
        value.key == pointer.0.0 && value.kind == KIND_POINTER && value.bytes.as_slice() == content.0.0.as_slice()
    }) && values.iter().any(|value| {
        value.key == content.0.0 && value.kind == KIND_CODE && value.bytes.as_slice() == bytes
    })
}

#[test]
fn distributed_written_pointers_export_their_cached_targets() {
    blaze_sim::check(blaze_sim::Config::cases(64).must_reach(&["target before pointer", "target after pointer"]), |tape| {
        let heap = blaze_own::testing::heap();
        let wire = heap.shared();
        let pointer = CodeKey(ContentId([2; 32]));
        let before = tape.coin("the target sorts before its pointer");
        tape.reached(if before { "target before pointer" } else { "target after pointer" });
        let old = CodeKey(ContentId([if before { 1 } else { 3 }; 32]));
        let current = CodeKey(ContentId([if before { 0 } else { 4 }; 32]));
        let lent = process(Sim::new(tape.next_u64("store schedule"), Faults::default()), 1);
        {
            let mut home = HomeCodeStore::new(&lent);
            home.put(CodeClass::Program, old, b"old compiler record").unwrap();
            home.put(CodeClass::Program, current, b"current compiler record").unwrap();
        }
        after_answer(&test_cx(), &lent, 0).unwrap();
        let lent = process(lent.into_store().into_driver(), 2);
        let mut home = HomeCodeStore::new(&lent);
        let mut dist = DistStore::new(&mut home, wire);
        // Neither target was put or read through this batch. The pointer writes alone
        // must carry the final target, even after a second write to the logical name.
        dist.point(CodeClass::Program, pointer, old).unwrap();
        dist.point(CodeClass::Program, pointer, current).unwrap();
        let values = dist.drain_tracked_values().unwrap();
        assert_eq!(values.len(), 2);
        assert!(has_pointer_and_target(&values, pointer, current, b"current compiler record"));
        assert!(dist.drain_tracked_values().unwrap().is_empty());

        // The previous write-first collector exported just the pointer. Removing the
        // target must fail the same self-contained-publication oracle.
        let pointer_only: WireVec<_> = values.into_iter()
            .filter(|value| value.kind == KIND_POINTER)
            .collect_in(wire);
        assert!(!has_pointer_and_target(&pointer_only, pointer, current, b"current compiler record"));
    });
}

#[test]
fn distributed_local_records_are_borrowed_and_failed_writes_keep_the_previous_value() {
    let heap = blaze_own::testing::heap();
    let wire = heap.shared();
    let lent = process(Sim::new(7, Faults::default()), 1);
    let mut home = HomeCodeStore::new(&lent);
    let key = CodeKey(ContentId([5; 32]));
    let mut dist = DistStore::new(&mut home, wire);
    let bytes = [17; 128 * 1024];
    dist.put(CodeClass::Program, key, &bytes).unwrap();
    dist.put(CodeClass::Program, key, &bytes).unwrap();
    let borrowed = dist.get(key).unwrap().unwrap();
    let stored = dist.local.get(key).unwrap().unwrap();
    assert!(core::ptr::eq(borrowed.as_ptr(), stored.as_ptr()), "a local record has one owner");
    assert_eq!(borrowed, bytes.as_slice());
    dist.clear_tracking();
    assert!(matches!(
        dist.put(CodeClass::Program, key, b"changed immutable record"),
        Err(StoreError::TwoValues { key: refused }) if refused == key
    ));
    assert!(dist.drain_tracked_values().unwrap().is_empty(), "a refused write was not accepted work");
    assert_eq!(dist.get(key).unwrap(), Some(bytes.as_slice()));
    let values = dist.drain_tracked_values().unwrap();
    assert_eq!(values.len(), 1);
    assert_eq!(values[0].key, key.0.0);
    assert_eq!(values[0].bytes.as_slice(), bytes.as_slice());
}

#[test]
fn distributed_accepted_pointers_supersede_temporary_peer_values() {
    blaze_sim::check(blaze_sim::Config::cases(64).must_reach(&["local acceptance", "peer acceptance"]), |tape| {
        let wire = blaze_own::testing::heap().shared();
        let lent = process(Sim::new(tape.next_u64("store schedule"), Faults::default()), 1);
        let mut home = HomeCodeStore::new(&lent);
        let pointer = CodeKey(ContentId([2; 32]));
        let old = CodeKey(ContentId([1; 32]));
        let current = CodeKey(ContentId([3; 32]));
        let missing = CodeKey(ContentId([4; 32]));
        home.put(CodeClass::Program, old, b"old peer record").unwrap();
        home.put(CodeClass::Program, current, b"current peer record").unwrap();
        home.point(CodeClass::Program, pointer, old).unwrap();
        let mut dist = DistStore::new(&mut home, wire);
        dist.insert_overlay(pointer.0.0, KIND_POINTER, &current.0.0);
        assert_eq!(dist.pointed(pointer).unwrap(), Some(current), "unimported peer values precede the home");
        let values = dist.drain_tracked_values().unwrap();
        assert!(has_pointer_and_target(&values, pointer, current, b"current peer record"));
        assert!(dist.point(CodeClass::Program, pointer, missing).is_err());
        assert_eq!(dist.pointed(pointer).unwrap(), Some(current), "refused writes leave the peer value intact");

        // Accepted work may replace a mutable pointer which an earlier grant supplied.
        // Exercise both paths that accept a new value into the home.
        dist.clear_tracking();
        if tape.coin("import the accepted value") {
            tape.reached("peer acceptance");
            let mut arrived = BTreeMap::new_in(wire);
            arrived.insert(pointer.0.0, (KIND_POINTER, copy_wire_bytes(&old.0.0, wire)));
            arrived.insert(old.0.0, (KIND_CODE, copy_wire_bytes(b"old peer record", wire)));
            dist.merge_arrived(&arrived).unwrap();
        } else {
            tape.reached("local acceptance");
            dist.point(CodeClass::Program, pointer, old).unwrap();
        }
        assert_eq!(dist.pointed(pointer).unwrap(), Some(old));
        let values = dist.drain_tracked_values().unwrap();
        assert_eq!(values.len(), 2);
        assert!(has_pointer_and_target(&values, pointer, old, b"old peer record"));
        let borrowed = dist.get(old).unwrap().unwrap();
        let stored = dist.local.get(old).unwrap().unwrap();
        assert!(core::ptr::eq(borrowed.as_ptr(), stored.as_ptr()), "accepted peer bytes live in the home");
    });
}
