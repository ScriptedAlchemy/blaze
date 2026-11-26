use super::*;
use blaze_cas::plain::{Lent, Settings};
use blaze_core::FileName;
use blaze_core::sim::{Faults, Sim};
use blaze_ladder::code_records::entry_of;
use crate::store::plain_place::{Facts, StoreAlloc, VISIBILITY, config, open};

fn home(sim: Sim, nonce: u64) -> Lent<Sim, StoreAlloc> {
    let alloc = blaze_own::testing::heap().alloc();
    let settings = Settings { durable: true, ..Settings::default() };
    let cfg = config(FileName::new("store").unwrap(), &settings, Facts { boot: Some(1), nonce }, alloc);
    open(sim, cfg, alloc).unwrap()
}

const POINTER: Digest = [1; 32];
const ENTRY: Digest = [2; 32];
const OTHER: Digest = [3; 32];

fn value(key: Digest, kind: u8, bytes: &[u8]) -> ValueRef<'_> {
    ValueRef { key, kind, bytes }
}

#[test]
fn peer_code_uses_the_home_store_across_delivery_orders_and_restart() {
    let alloc = blaze_own::testing::heap().shared();
    let initial = [value(POINTER, KIND_POINTER, &ENTRY), value(ENTRY, KIND_CODE, b"old")];
    let updated = [value(POINTER, KIND_POINTER, &OTHER), value(OTHER, KIND_CODE, b"new")];
    for seed in 0..2 {
        let lent = home(Sim::new(seed, Faults::default()), seed + 1);
        let mut local = HomeCodeStore::new(&lent);
        let mut store = DistStore::new(&mut local, alloc);
        if seed == 0 {
            store.import(initial.into_iter()).unwrap();
        } else {
            store.import(initial.into_iter().rev()).unwrap();
        }
        store.import(initial.into_iter()).unwrap();
        let key = CodeKey(ContentId(POINTER));
        assert_eq!(entry_of(&store, key).unwrap(), Some(&b"old"[..]));
        let exported = store.drain_tracked_values().unwrap();
        assert_eq!(exported.len(), 2);
        assert_eq!(initial[0], exported[0]);
        assert_eq!(initial[1], exported[1]);
        assert!(store.drain_tracked_values().unwrap().is_empty());
        store.import(updated.into_iter()).unwrap();
        assert_eq!(entry_of(&store, key).unwrap(), Some(&b"new"[..]));
        drop(store);
        lent.commit().unwrap();
        let mut sim = lent.into_store().into_driver();
        sim.crash();
        let reopened = home(sim, seed + 3);
        assert_eq!(entry_of(&HomeCodeStore::new(&reopened), key).unwrap(), Some(&b"new"[..]));
    }
}

#[test]
fn an_invalid_peer_batch_is_refused_before_any_value_is_imported() {
    let alloc = blaze_own::testing::heap().shared();
    let lent = home(Sim::new(7, Faults::default()), 7);
    let mut local = HomeCodeStore::new(&lent);
    let held = CodeKey(ContentId(ENTRY));
    local.put(CodeClass::Program, held, b"kept").unwrap();
    let mut store = DistStore::new(&mut local, alloc);
    let fresh = value(POINTER, KIND_CODE, b"unrelated");
    let plain = value(OTHER, KIND_CODE, b"first");
    let pointer = value(OTHER, KIND_POINTER, &ENTRY);
    for (batch, expected) in [
        ([fresh, plain, value(OTHER, KIND_CODE, b"second")], StoreError::TwoValues { key: CodeKey(ContentId(OTHER)) }),
        ([fresh, plain, pointer], StoreError::TwoValues { key: CodeKey(ContentId(OTHER)) }),
        ([fresh, plain, value(ENTRY, KIND_CODE, b"changed")], StoreError::TwoValues { key: held }),
        ([fresh, plain, value(ENTRY, KIND_POINTER, &OTHER)], StoreError::TwoValues { key: held }),
    ] {
        assert_eq!(store.import(batch.into_iter()), Err(expected));
        assert_eq!(store.get(CodeKey(ContentId(POINTER))).unwrap(), None);
        assert_eq!(store.get(CodeKey(ContentId(OTHER))).unwrap(), None);
        assert_eq!(store.get(held).unwrap(), Some(&b"kept"[..]));
    }
    for bad in [
        value(OTHER, 99, b"unknown kind"),
        value(OTHER, KIND_POINTER, b"short"),
        value(OTHER, KIND_POINTER, &[8; 32]),
        value(OTHER, KIND_POINTER, &OTHER),
    ] {
        assert!(matches!(store.import([fresh, bad].into_iter()), Err(StoreError::Failed { key, .. }) if key == CodeKey(ContentId(OTHER))));
        assert_eq!(store.get(CodeKey(ContentId(POINTER))).unwrap(), None);
    }
    // Even a two-hop chain ending at a real plain entry violates the CodeStore contract.
    let chain = [value(POINTER, KIND_POINTER, &OTHER), pointer];
    assert!(matches!(store.import(chain.into_iter()), Err(StoreError::Failed { key, .. }) if key == CodeKey(ContentId(POINTER))));
    assert_eq!(store.pointed(CodeKey(ContentId(OTHER))).unwrap(), None);
}

#[test]
fn a_failed_home_write_is_a_distributed_store_error_and_stays_a_miss() {
    use blaze_own::testing::{FailAt, TestHeap};
    static FAIL: FailAt<TestHeap> = FailAt::never(TestHeap);
    static ARENA: blaze_own::Erased<&FailAt<TestHeap>> = blaze_own::Erased::new(&FAIL);
    let alloc = blaze_own::testing::heap().shared();
    let driven = home(Sim::new(8, Faults::default()), 8).into_store();
    let lent = Lent::with_visibility(driven, ARENA.handle(), VISIBILITY);
    let mut local = HomeCodeStore::new(&lent);
    let mut store = DistStore::new(&mut local, alloc);
    let key = CodeKey(ContentId(ENTRY));
    let record = value(ENTRY, KIND_CODE, b"private");
    // Only the home arena uses FAIL. The peer batch and the simulated disk use their usual allocators.
    FAIL.fail_at(FAIL.seen());
    let refusal = store.import([record].into_iter()).unwrap_err();
    assert_eq!(refusal, StoreError::Failed { key, what: "the store's arena could not take the entry: its allocator refused" });
    assert_eq!(store.get(key).unwrap(), None);
    assert!(store.drain_tracked_values().unwrap().is_empty());
    store.import([record].into_iter()).unwrap();
    assert_eq!(store.get(key).unwrap(), Some(&b"private"[..]));
}
