//! Findings retained by the compiler must be released even before the holder has recorded
//! an accepted result: conflicting discoveries, rejected names and cancellation during a
//! result round trip all leave the compiler holding requests.

use std::collections::BTreeSet;

use blaze_dist::Ranges;
use blaze_dist::holder::{Compiled, Effect, Event, Holder, HolderConfig, Reply};
use blaze_wire::{
    Attempt, EntryKey, EntryRef, ExecutionId, Found, Grant, ItemOutcome, ItemResult, Lease,
    RootSet, Set, WorkJoinAnswer, WorkLeaseAnswer, WorkName, WorkResultAnswer, WorkersEventsAnswer,
};

const FOUND: WorkName = WorkName([7; 32]);

fn sent(effects: &[Effect]) -> u64 {
    effects
        .iter()
        .find_map(|effect| match effect {
            Effect::Send { id, .. } => Some(*id),
            _ => None,
        })
        .expect("a request was sent")
}

/// The compiler is working on a grant; its eventual batch retains FOUND.
fn compiling(rejected: bool) -> (Holder, Compiled) {
    let roots =
        RootSet { spec: [1; 32], roots: [2; 32], count: 1, index: [3; 32], inputs: [4; 32] };
    let mut holder = Holder::new(
        "helper",
        ExecutionId(1),
        Attempt(1),
        roots.clone(),
        Set::new(if rejected { vec![FOUND] } else { Vec::new() }),
        BTreeSet::new(),
        HolderConfig::default(),
    );
    let mut effects = Vec::new();
    holder.step(0, Event::Loaded, &mut effects);
    let id = sent(&effects);
    effects.clear();
    holder.step(
        1,
        Event::Reply { id, reply: Reply::Join(Ok(WorkJoinAnswer { roots, fixed: true })) },
        &mut effects,
    );
    let id = sent(&effects);
    effects.clear();
    let grant = Grant {
        lease: Lease {
            execution: ExecutionId(1),
            attempt: Attempt(1),
            holder: "helper".to_owned(),
            generation: 1,
        },
        roots: Set::new(vec![(0, 1)]),
        names: Set::new(Vec::new()),
    };
    holder.step(
        2,
        Event::Reply { id, reply: Reply::Lease(Ok(WorkLeaseAnswer { grant: Some(grant) })) },
        &mut effects,
    );
    assert!(effects.iter().any(|effect| matches!(effect, Effect::Compile(_))));
    (
        holder,
        Compiled {
            generation: 1,
            roots: vec![ItemResult {
                name: WorkName([8; 32]),
                outcome: ItemOutcome::Exported(EntryRef { entry: EntryKey([9; 32]) }),
                found: Set::new(vec![Found { name: FOUND, tuple: [10; 32] }]),
                unnamed: Set::new(Vec::new()),
            }],
            names: Vec::new(),
            packs: Vec::new(),
            returned_roots: Ranges::new(),
            returned_names: Vec::new(),
        },
    )
}

/// A compiler has retained FOUND and reported it. Its result is awaiting acknowledgment.
fn reporting(rejected: bool) -> (Holder, u64) {
    let (mut holder, batch) = compiling(rejected);
    let mut effects = Vec::new();
    holder.step(3, Event::Compiled(batch), &mut effects);
    let id = sent(&effects);
    assert!(holder.findings().is_empty(), "the result has not been acknowledged");
    (holder, id)
}

fn released(effects: &[Effect]) -> BTreeSet<WorkName> {
    effects
        .iter()
        .flat_map(|effect| match effect {
            Effect::Release(names) => names.as_slice(),
            _ => &[],
        })
        .copied()
        .collect()
}

#[test]
fn review_a_fresh_conflicting_finding_is_released() {
    let (mut holder, id) = reporting(false);
    let mut effects = Vec::new();
    holder.step(
        4,
        Event::Reply {
            id,
            reply: Reply::Result(Ok(WorkResultAnswer {
                granted: Set::new(Vec::new()),
                conflicts: Set::new(vec![FOUND]),
            })),
        },
        &mut effects,
    );
    assert_eq!(released(&effects), BTreeSet::from([FOUND]));
    assert!(holder.findings().is_empty());
    assert!(!holder.is_stopped());
}

#[test]
fn review_a_fresh_rejected_finding_is_released() {
    let (mut holder, id) = reporting(true);
    let mut effects = Vec::new();
    holder.step(
        4,
        Event::Reply {
            id,
            reply: Reply::Result(Ok(WorkResultAnswer {
                granted: Set::new(Vec::new()),
                conflicts: Set::new(Vec::new()),
            })),
        },
        &mut effects,
    );
    assert_eq!(released(&effects), BTreeSet::from([FOUND]));
    assert!(holder.findings().is_empty());
}

#[test]
fn review_cancellation_releases_findings_in_an_unacknowledged_result() {
    let (mut holder, _) = reporting(false);
    let mut effects = Vec::new();
    holder.step(4, Event::Tick, &mut effects);
    let heartbeat = sent(&effects);
    effects.clear();
    holder.step(
        5,
        Event::Reply { id: heartbeat, reply: Reply::Events(Ok(WorkersEventsAnswer::CANCEL)) },
        &mut effects,
    );
    assert!(holder.is_stopped());
    assert_eq!(released(&effects), BTreeSet::from([FOUND]));
}

#[test]
fn review_a_batch_finishing_after_cancellation_releases_its_findings() {
    let (mut holder, batch) = compiling(false);
    let mut effects = Vec::new();
    holder.step(3, Event::Tick, &mut effects);
    let heartbeat = sent(&effects);
    effects.clear();
    holder.step(
        4,
        Event::Reply { id: heartbeat, reply: Reply::Events(Ok(WorkersEventsAnswer::CANCEL)) },
        &mut effects,
    );
    assert!(holder.is_stopped());
    effects.clear();
    holder.step(5, Event::Compiled(batch), &mut effects);
    assert_eq!(released(&effects), BTreeSet::from([FOUND]));
    assert!(effects.iter().all(|effect| matches!(effect, Effect::Release(_))));
}
