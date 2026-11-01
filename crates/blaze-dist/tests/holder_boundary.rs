//! The compiler is not allowed to run or publish work outside its exact lease.
use std::collections::BTreeSet;

use blaze_dist::Ranges;
use blaze_dist::holder::{Call, Compiled, Effect, Event, Holder, HolderConfig, Reply, StopWhy};
use blaze_wire::{
    Attempt, EntryKey, EntryRef, ExecutionId, Found, Grant, ItemOutcome, ItemResult, Lease,
    RootSet, Set, WorkJoinAnswer, WorkLeaseAnswer, WorkName,
};

const FIRST: WorkName = WorkName([1; 32]);
const SECOND: WorkName = WorkName([2; 32]);
const FOUND: WorkName = WorkName([3; 32]);

fn sent(effects: &[Effect]) -> u64 {
    effects
        .iter()
        .find_map(|effect| match effect {
            Effect::Send { id, .. } => Some(*id),
            _ => None,
        })
        .expect("expected a coordinator request")
}

fn ready(index: BTreeSet<WorkName>) -> (Holder, u64) {
    let roots =
        RootSet { spec: [1; 32], roots: [2; 32], count: 2, index: [3; 32], inputs: [4; 32] };
    let mut holder = Holder::new(
        "worker",
        ExecutionId(1),
        Attempt(1),
        roots.clone(),
        Set::new(Vec::new()),
        index,
        HolderConfig::default(),
    );
    let mut out = Vec::new();
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

fn grant(roots: Vec<(u64, u64)>, names: Vec<WorkName>) -> Grant {
    Grant {
        lease: Lease {
            execution: ExecutionId(1),
            attempt: Attempt(1),
            holder: "worker".into(),
            generation: 1,
        },
        roots: Set::new(roots),
        names: Set::new(names),
    }
}

fn compiling(roots: Vec<(u64, u64)>, names: Vec<WorkName>) -> Holder {
    let (mut holder, id) = ready(names.iter().copied().collect());
    let mut out = Vec::new();
    holder.step(
        2,
        Event::Reply {
            id,
            reply: Reply::Lease(Ok(WorkLeaseAnswer { grant: Some(grant(roots, names)) })),
        },
        &mut out,
    );
    assert!(out.iter().any(|effect| matches!(effect, Effect::Compile(_))));
    holder
}

fn item(name: WorkName) -> ItemResult {
    ItemResult {
        name,
        outcome: ItemOutcome::Exported(EntryRef { entry: EntryKey([9; 32]) }),
        found: Set::new(vec![Found { name: FOUND, tuple: [7; 32] }]),
        unnamed: Set::new(Vec::new()),
    }
}

fn batch(roots: Vec<ItemResult>, names: Vec<ItemResult>) -> Compiled {
    Compiled {
        generation: 1,
        roots,
        names,
        packs: Vec::new(),
        returned_roots: Ranges::new(),
        returned_names: Vec::new(),
    }
}

fn refuses_batch(holder: &mut Holder, compiled: Compiled) {
    let retained: BTreeSet<_> = compiled
        .roots
        .iter()
        .chain(&compiled.names)
        .flat_map(|item| item.found.iter().map(|found| found.name))
        .collect();
    let mut out = Vec::new();
    holder.step(3, Event::Compiled(compiled), &mut out);
    assert!(holder.is_stopped(), "a malformed compiler batch cannot be reported as a valid prefix");
    assert!(out.iter().any(|effect| matches!(effect, Effect::Stop(StopWhy::Protocol(_)))));
    assert!(!out.iter().any(|effect| matches!(effect, Effect::Send { call: Call::Result(_), .. })));
    let released: BTreeSet<_> = out
        .iter()
        .flat_map(|effect| match effect {
            Effect::Release(names) => names.as_slice(),
            _ => &[],
        })
        .copied()
        .collect();
    assert!(
        retained.is_subset(&released),
        "refused batches must release compiler-retained discoveries"
    );
}

#[test]
fn malformed_and_out_of_bounds_root_grants_never_reach_the_compiler() {
    for ranges in [vec![(0, 3)], vec![(2, 3)], vec![(1, 0)], vec![(0, 0)], vec![(0, 2), (1, 2)]] {
        let (mut holder, id) = ready(BTreeSet::new());
        let mut out = Vec::new();
        holder.step(
            2,
            Event::Reply {
                id,
                reply: Reply::Lease(Ok(WorkLeaseAnswer { grant: Some(grant(ranges, Vec::new())) })),
            },
            &mut out,
        );
        assert!(holder.is_stopped(), "invalid root range reached a live compiler");
        assert!(!out.iter().any(|effect| matches!(effect, Effect::Compile(_))));
    }
}

#[test]
fn surplus_named_results_are_not_silently_discarded() {
    let mut holder = compiling(vec![(0, 1)], Vec::new());
    refuses_batch(&mut holder, batch(vec![item(FIRST)], vec![item(SECOND)]));
}

#[test]
fn every_unreturned_root_requires_exactly_one_result() {
    for roots in [vec![], vec![item(FIRST), item(SECOND), item(FIRST)]] {
        let mut holder = compiling(vec![(0, 2)], Vec::new());
        refuses_batch(&mut holder, batch(roots, Vec::new()));
    }
}

#[test]
fn results_must_follow_the_granted_name_order() {
    let mut holder = compiling(Vec::new(), vec![FIRST, SECOND]);
    refuses_batch(&mut holder, batch(Vec::new(), vec![item(SECOND), item(FIRST)]));
}

#[test]
fn returned_work_must_belong_to_this_compile_plan() {
    let mut holder = compiling(vec![(0, 1)], Vec::new());
    let mut compiled = batch(vec![item(FIRST)], Vec::new());
    compiled.returned_roots.insert(1, 2);
    refuses_batch(&mut holder, compiled);

    let mut holder = compiling(Vec::new(), vec![FIRST]);
    let mut compiled = batch(Vec::new(), vec![item(FIRST)]);
    compiled.returned_names.push(SECOND);
    refuses_batch(&mut holder, compiled);
}

#[test]
fn duplicate_returns_do_not_hide_a_missing_result() {
    let mut holder = compiling(Vec::new(), vec![FIRST, SECOND]);
    let mut compiled = batch(Vec::new(), vec![item(SECOND)]);
    compiled.returned_names = vec![FIRST, FIRST];
    refuses_batch(&mut holder, compiled);
}

#[test]
fn a_valid_partial_batch_preserves_exact_returned_work() {
    let mut holder = compiling(vec![(0, 2)], vec![FIRST, SECOND]);
    let mut compiled = batch(vec![item(WorkName([4; 32]))], vec![item(SECOND)]);
    compiled.returned_roots.insert(1, 2);
    compiled.returned_names.push(FIRST);
    let mut out = Vec::new();
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
    assert_eq!(result.returned_roots, Set::new(vec![(1, 2)]));
    assert_eq!(result.returned_names, Set::new(vec![FIRST]));
}
