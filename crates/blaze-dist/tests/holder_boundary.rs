//! The compiler is not allowed to run or publish work outside its exact lease.

#![no_std]
#![allow(missing_docs)]

use blaze_dist::Ranges;
use blaze_dist::holder::{Call, Compiled, Effect, Event, Holder, HolderConfig, Reply, StopWhy};
use blaze_own::{BTreeSet, Vec, vec_in};
use blaze_wire::{
    Attempt, EntryKey, EntryRef, ExecutionId, Found, Grant, ItemOutcome, ItemResult, Lease,
    RootSet, Set, WireAlloc, WireString, WireVec, WorkJoinAnswer, WorkLeaseAnswer, WorkName,
};

const FIRST: WorkName = WorkName([1; 32]);
const SECOND: WorkName = WorkName([2; 32]);
const FOUND: WorkName = WorkName([3; 32]);

fn heap() -> WireAlloc {
    blaze_own::testing::heap().shared()
}

fn sent(effects: &[Effect]) -> u64 {
    effects
        .iter()
        .find_map(|effect| match effect {
            Effect::Send { id, .. } => Some(*id),
            _ => None,
        })
        .expect("expected a coordinator request")
}

fn ready(index: BTreeSet<WorkName, WireAlloc>, alloc: WireAlloc) -> (Holder<WireAlloc>, u64) {
    let roots =
        RootSet { spec: [1; 32], roots: [2; 32], count: 2, index: [3; 32], inputs: [4; 32] };
    let mut holder = Holder::new(
        "worker",
        ExecutionId(1),
        Attempt(1),
        roots.clone(),
        Set::empty_in(alloc),
        index,
        HolderConfig::default(),
        alloc,
    );
    let mut out = Vec::new_in(alloc);
    holder.step(0, Event::Loaded, &mut out);
    let id = sent(&out);
    out.clear();
    holder.step(
        1,
        Event::Reply { id, reply: Reply::Join(Ok(WorkJoinAnswer { roots, fixed: true })) },
        &mut out,
    );
    (holder, sent(&out))
}

fn grant(roots: WireVec<(u64, u64)>, names: WireVec<WorkName>, alloc: WireAlloc) -> Grant {
    Grant {
        lease: Lease {
            execution: ExecutionId(1),
            attempt: Attempt(1),
            holder: WireString::from_str_in("worker", alloc),
            generation: 1,
        },
        roots: Set::new(roots),
        names: Set::new(names),
    }
}

fn compiling(
    roots: WireVec<(u64, u64)>,
    names: WireVec<WorkName>,
    alloc: WireAlloc,
) -> Holder<WireAlloc> {
    let mut index = BTreeSet::new_in(alloc);
    for name in names.iter().copied() {
        index.insert(name);
    }
    let (mut holder, id) = ready(index, alloc);
    let mut out = Vec::new_in(alloc);
    holder.step(
        2,
        Event::Reply {
            id,
            reply: Reply::Lease(Ok(WorkLeaseAnswer { grant: Some(grant(roots, names, alloc)) })),
        },
        &mut out,
    );
    assert!(out.iter().any(|effect| matches!(effect, Effect::Compile(_))));
    holder
}

fn item(name: WorkName, alloc: WireAlloc) -> ItemResult {
    ItemResult {
        name,
        outcome: ItemOutcome::Exported(EntryRef { entry: EntryKey([9; 32]) }),
        found: Set::new(vec_in![alloc; Found { name: FOUND, tuple: [7; 32] }]),
        unnamed: Set::empty_in(alloc),
    }
}

fn batch(roots: WireVec<ItemResult>, names: WireVec<ItemResult>, alloc: WireAlloc) -> Compiled {
    Compiled {
        generation: 1,
        roots,
        names,
        packs: Vec::new_in(alloc),
        returned_roots: Ranges::new(alloc),
        returned_names: Vec::new_in(alloc),
    }
}

fn refuses_batch(holder: &mut Holder<WireAlloc>, compiled: Compiled, alloc: WireAlloc) {
    let mut retained = BTreeSet::new_in(alloc);
    for item in compiled.roots.iter().chain(&compiled.names) {
        for found in item.found.iter() {
            retained.insert(found.name);
        }
    }
    let mut out = Vec::new_in(alloc);
    holder.step(3, Event::Compiled(compiled), &mut out);
    assert!(holder.is_stopped(), "a malformed compiler batch cannot be reported as a valid prefix");
    assert!(out.iter().any(|effect| matches!(effect, Effect::Stop(StopWhy::Protocol(_)))));
    assert!(!out.iter().any(|effect| matches!(effect, Effect::Send { call: Call::Result(_), .. })));
    let mut released = BTreeSet::new_in(alloc);
    for effect in &out {
        if let Effect::Release(names) = effect {
            for name in names.as_slice() {
                released.insert(*name);
            }
        }
    }
    assert!(
        retained.iter().all(|name| released.contains(name)),
        "refused batches must release compiler-retained discoveries"
    );
}

#[test]
fn malformed_and_out_of_bounds_root_grants_never_reach_the_compiler() {
    let alloc = heap();
    for ranges in [
        vec_in![alloc; (0, 3)],
        vec_in![alloc; (2, 3)],
        vec_in![alloc; (1, 0)],
        vec_in![alloc; (0, 0)],
        vec_in![alloc; (0, 2), (1, 2)],
    ] {
        let (mut holder, id) = ready(BTreeSet::new_in(alloc), alloc);
        let mut out = Vec::new_in(alloc);
        holder.step(
            2,
            Event::Reply {
                id,
                reply: Reply::Lease(Ok(WorkLeaseAnswer {
                    grant: Some(grant(ranges, Vec::new_in(alloc), alloc)),
                })),
            },
            &mut out,
        );
        assert!(holder.is_stopped(), "invalid root range reached a live compiler");
        assert!(!out.iter().any(|effect| matches!(effect, Effect::Compile(_))));
    }
}

#[test]
fn surplus_named_results_are_not_silently_discarded() {
    let alloc = heap();
    let mut holder = compiling(vec_in![alloc; (0, 1)], Vec::new_in(alloc), alloc);
    refuses_batch(
        &mut holder,
        batch(vec_in![alloc; item(FIRST, alloc)], vec_in![alloc; item(SECOND, alloc)], alloc),
        alloc,
    );
}

#[test]
fn every_unreturned_root_requires_exactly_one_result() {
    let alloc = heap();
    for roots in [
        Vec::new_in(alloc),
        vec_in![alloc; item(FIRST, alloc), item(SECOND, alloc), item(FIRST, alloc)],
    ] {
        let mut holder = compiling(vec_in![alloc; (0, 2)], Vec::new_in(alloc), alloc);
        refuses_batch(&mut holder, batch(roots, Vec::new_in(alloc), alloc), alloc);
    }
}

#[test]
fn results_must_follow_the_granted_name_order() {
    let alloc = heap();
    let mut holder = compiling(Vec::new_in(alloc), vec_in![alloc; FIRST, SECOND], alloc);
    refuses_batch(
        &mut holder,
        batch(Vec::new_in(alloc), vec_in![alloc; item(SECOND, alloc), item(FIRST, alloc)], alloc),
        alloc,
    );
}

#[test]
fn returned_work_must_belong_to_this_compile_plan() {
    let alloc = heap();
    let mut holder = compiling(vec_in![alloc; (0, 1)], Vec::new_in(alloc), alloc);
    let mut compiled = batch(vec_in![alloc; item(FIRST, alloc)], Vec::new_in(alloc), alloc);
    compiled.returned_roots.insert(1, 2);
    refuses_batch(&mut holder, compiled, alloc);

    let mut holder = compiling(Vec::new_in(alloc), vec_in![alloc; FIRST], alloc);
    let mut compiled = batch(Vec::new_in(alloc), vec_in![alloc; item(FIRST, alloc)], alloc);
    compiled.returned_names.push(SECOND);
    refuses_batch(&mut holder, compiled, alloc);
}

#[test]
fn duplicate_returns_do_not_hide_a_missing_result() {
    let alloc = heap();
    let mut holder = compiling(Vec::new_in(alloc), vec_in![alloc; FIRST, SECOND], alloc);
    let mut compiled = batch(Vec::new_in(alloc), vec_in![alloc; item(SECOND, alloc)], alloc);
    compiled.returned_names = vec_in![alloc; FIRST, FIRST];
    refuses_batch(&mut holder, compiled, alloc);
}

#[test]
fn a_valid_partial_batch_preserves_exact_returned_work() {
    let alloc = heap();
    let mut holder = compiling(vec_in![alloc; (0, 2)], vec_in![alloc; FIRST, SECOND], alloc);
    let mut compiled = batch(
        vec_in![alloc; item(WorkName([4; 32]), alloc)],
        vec_in![alloc; item(SECOND, alloc)],
        alloc,
    );
    compiled.returned_roots.insert(1, 2);
    compiled.returned_names.push(FIRST);
    let mut out = Vec::new_in(alloc);
    holder.step(3, Event::Compiled(compiled), &mut out);
    assert!(!holder.is_stopped());
    let result = out
        .iter()
        .find_map(|effect| match effect {
            Effect::Send { call: Call::Result(result), .. } => Some(result),
            _ => None,
        })
        .unwrap();
    assert_eq!(result.items.len(), 2);
    assert_eq!(result.items[1].name, SECOND);
    assert_eq!(result.returned_roots, Set::new(vec_in![alloc; (1, 2)]));
    assert_eq!(result.returned_names, Set::new(vec_in![alloc; FIRST]));
}
