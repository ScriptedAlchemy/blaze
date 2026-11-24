//! Findings retained by the compiler must be released even before the holder has recorded
//! an accepted result: conflicting discoveries, rejected names and cancellation during a
//! result round trip all leave the compiler holding requests.

#![no_std]
#![allow(missing_docs)]

use blaze_dist::Ranges;
use blaze_dist::holder::{Call, Compiled, Effect, Event, Holder, HolderConfig, Reply, StopWhy};
use blaze_own::{BTreeSet, Vec, vec_in};
use blaze_wire::{
    Attempt, EntryKey, EntryRef, ErrorCode, ExecutionId, Found, Grant, ItemOutcome, ItemResult,
    Lease, RootSet, Set, WireAlloc, WireError, WireString, WorkJoinAnswer, WorkLeaseAnswer,
    WorkName, WorkResultAnswer, WorkersEventsAnswer,
};

const FOUND: WorkName = WorkName([7; 32]);

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
        .expect("a request was sent")
}

/// The compiler is working on a grant; its eventual batch retains FOUND.
fn compiling(rejected: bool, alloc: WireAlloc) -> (Holder<WireAlloc>, Compiled) {
    let roots =
        RootSet { spec: [1; 32], roots: [2; 32], count: 1, index: [3; 32], inputs: [4; 32] };
    let mut holder = Holder::new(
        "helper",
        ExecutionId(1),
        Attempt(1),
        roots.clone(),
        if rejected { Set::new(vec_in![alloc; FOUND]) } else { Set::empty_in(alloc) },
        BTreeSet::new_in(alloc),
        HolderConfig::default(),
        alloc,
    );
    let mut effects = Vec::new_in(alloc);
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
            holder: WireString::from_str_in("helper", alloc),
            generation: 1,
        },
        roots: Set::new(vec_in![alloc; (0, 1)]),
        names: Set::empty_in(alloc),
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
            roots: vec_in![
                alloc;
                ItemResult {
                    name: WorkName([8; 32]),
                    outcome: ItemOutcome::Exported(EntryRef { entry: EntryKey([9; 32]) }),
                    found: Set::new(vec_in![alloc; Found { name: FOUND, tuple: [10; 32] }]),
                    unnamed: Set::empty_in(alloc),
                }
            ],
            names: Vec::new_in(alloc),
            packs: Vec::new_in(alloc),
            returned_roots: Ranges::new(alloc),
            returned_names: Vec::new_in(alloc),
        },
    )
}

/// A compiler has retained FOUND and reported it. Its result is awaiting acknowledgment.
fn reporting(rejected: bool, alloc: WireAlloc) -> (Holder<WireAlloc>, u64) {
    let (mut holder, batch) = compiling(rejected, alloc);
    let mut effects = Vec::new_in(alloc);
    holder.step(3, Event::Compiled(batch), &mut effects);
    let id = sent(&effects);
    assert_eq!(holder.findings().len(), 0, "the result has not been acknowledged");
    (holder, id)
}

fn released(effects: &[Effect], alloc: WireAlloc) -> BTreeSet<WorkName, WireAlloc> {
    let mut set = BTreeSet::new_in(alloc);
    for effect in effects {
        if let Effect::Release(names) = effect {
            for name in names.as_slice() {
                set.insert(*name);
            }
        }
    }
    set
}

fn single_set(name: WorkName, alloc: WireAlloc) -> BTreeSet<WorkName, WireAlloc> {
    let mut set = BTreeSet::new_in(alloc);
    set.insert(name);
    set
}

#[test]
fn review_a_fresh_conflicting_finding_is_released() {
    let alloc = heap();
    let (mut holder, id) = reporting(false, alloc);
    let mut effects = Vec::new_in(alloc);
    holder.step(
        4,
        Event::Reply {
            id,
            reply: Reply::Result(Ok(WorkResultAnswer {
                granted: Set::empty_in(alloc),
                conflicts: Set::new(vec_in![alloc; FOUND]),
            })),
        },
        &mut effects,
    );
    assert_eq!(released(&effects, alloc), single_set(FOUND, alloc));
    assert_eq!(holder.findings().len(), 0);
    assert!(!holder.is_stopped());
}

#[test]
fn review_a_fresh_rejected_finding_is_released() {
    let alloc = heap();
    let (mut holder, id) = reporting(true, alloc);
    let mut effects = Vec::new_in(alloc);
    holder.step(
        4,
        Event::Reply {
            id,
            reply: Reply::Result(Ok(WorkResultAnswer {
                granted: Set::empty_in(alloc),
                conflicts: Set::empty_in(alloc),
            })),
        },
        &mut effects,
    );
    assert_eq!(released(&effects, alloc), single_set(FOUND, alloc));
    assert_eq!(holder.findings().len(), 0);
}

#[test]
fn review_cancellation_releases_findings_in_an_unacknowledged_result() {
    let alloc = heap();
    let (mut holder, _) = reporting(false, alloc);
    let mut effects = Vec::new_in(alloc);
    holder.step(4, Event::Tick, &mut effects);
    let heartbeat = sent(&effects);
    effects.clear();
    holder.step(
        5,
        Event::Reply { id: heartbeat, reply: Reply::Events(Ok(WorkersEventsAnswer::CANCEL)) },
        &mut effects,
    );
    assert!(holder.is_stopped());
    assert_eq!(released(&effects, alloc), single_set(FOUND, alloc));
}

#[test]
fn review_a_batch_finishing_after_cancellation_releases_its_findings() {
    let alloc = heap();
    let (mut holder, batch) = compiling(false, alloc);
    let mut effects = Vec::new_in(alloc);
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
    assert_eq!(released(&effects, alloc), single_set(FOUND, alloc));
    assert!(effects.iter().all(|effect| matches!(effect, Effect::Release(_))));
}

/// A delayed heartbeat reply must not be invalidated by the next interval. A
/// coordinator's CANCEL is authoritative even if its response took longer than
/// the configured heartbeat period.
#[test]
fn late_cancellation_of_a_retried_heartbeat_stops_the_holder() {
    let alloc = heap();
    let roots =
        RootSet { spec: [1; 32], roots: [2; 32], count: 1, index: [3; 32], inputs: [4; 32] };
    let mut holder = Holder::new(
        "helper",
        ExecutionId(1),
        Attempt(1),
        roots,
        Set::empty_in(alloc),
        BTreeSet::new_in(alloc),
        HolderConfig { heartbeat_ms: 5, retry_ms: 20, idle_ms: 2 },
        alloc,
    );
    let mut effects = Vec::new_in(alloc);
    holder.start(0, &mut effects);
    let first = sent(&effects);
    assert!(
        effects.iter().any(|effect| matches!(effect, Effect::Send { call: Call::Events(_), .. }))
    );
    effects.clear();

    holder.step(5, Event::Tick, &mut effects);
    assert_eq!(sent(&effects), first, "an unanswered heartbeat keeps its request id");
    effects.clear();
    holder.step(
        6,
        Event::Reply { id: first, reply: Reply::Events(Ok(WorkersEventsAnswer::CANCEL)) },
        &mut effects,
    );
    assert!(holder.is_stopped(), "a late cancellation must not be forgotten");
    assert!(effects.iter().any(|effect| matches!(effect, Effect::Stop(StopWhy::Cancelled))));
}

#[test]
fn heartbeat_acknowledgement_allows_a_fresh_request_without_reusing_the_old_id() {
    let alloc = heap();
    let roots =
        RootSet { spec: [1; 32], roots: [2; 32], count: 1, index: [3; 32], inputs: [4; 32] };
    let mut holder = Holder::new(
        "helper",
        ExecutionId(1),
        Attempt(1),
        roots,
        Set::empty_in(alloc),
        BTreeSet::new_in(alloc),
        HolderConfig { heartbeat_ms: 5, retry_ms: 20, idle_ms: 2 },
        alloc,
    );
    let mut effects = Vec::new_in(alloc);
    holder.start(0, &mut effects);
    let first = sent(&effects);
    effects.clear();
    holder.step(
        1,
        Event::Reply { id: first, reply: Reply::Events(Ok(WorkersEventsAnswer::CONTINUE)) },
        &mut effects,
    );
    assert!(!holder.is_stopped());
    holder.step(5, Event::Tick, &mut effects);
    let next = sent(&effects);
    assert_ne!(next, first);
    effects.clear();

    // A delayed duplicate of the first acknowledged heartbeat is not a new
    // cancellation, but the outstanding heartbeat's cancellation is binding.
    holder.step(
        6,
        Event::Reply { id: first, reply: Reply::Events(Ok(WorkersEventsAnswer::CANCEL)) },
        &mut effects,
    );
    assert!(!holder.is_stopped());
    holder.step(
        7,
        Event::Reply { id: next, reply: Reply::Events(Ok(WorkersEventsAnswer::CANCEL)) },
        &mut effects,
    );
    assert!(holder.is_stopped());
}

/// A carrier `Busy` on a lease (a retention bound, answered with the bare code and no
/// `busy:` token) is a wait: the holder asks again after `retry_after_ms` instead of
/// stopping as a protocol fault.
#[test]
fn a_busy_lease_answer_waits_and_asks_again() {
    let alloc = heap();
    let roots =
        RootSet { spec: [1; 32], roots: [2; 32], count: 1, index: [3; 32], inputs: [4; 32] };
    let mut holder = Holder::new(
        "helper",
        ExecutionId(1),
        Attempt(1),
        roots.clone(),
        Set::empty_in(alloc),
        BTreeSet::new_in(alloc),
        HolderConfig::default(),
        alloc,
    );
    let mut effects = Vec::new_in(alloc);
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
    let busy = WireError::new(ErrorCode::Busy, format_args!("grant-retention-capacity"), alloc)
        .retry_after(500);
    holder.step(2, Event::Reply { id, reply: Reply::Lease(Err(busy)) }, &mut effects);
    assert!(!holder.is_stopped(), "{effects:?}");
    assert!(effects.is_empty(), "{effects:?}");
    holder.step(501, Event::Tick, &mut effects);
    assert!(
        !effects.iter().any(|effect| matches!(effect, Effect::Send { call: Call::Lease(_), .. })),
        "asked again before the wait elapsed: {effects:?}"
    );
    effects.clear();
    holder.step(502, Event::Tick, &mut effects);
    assert!(
        effects.iter().any(|effect| matches!(effect, Effect::Send { call: Call::Lease(_), .. })),
        "{effects:?}"
    );
}

/// A heartbeat refused with `attempt-moved` fences the holder at once: it does not
/// keep compiling for the dead attempt until its next work call.
#[test]
fn a_heartbeat_that_names_a_moved_attempt_stops_the_holder() {
    let alloc = heap();
    let roots =
        RootSet { spec: [1; 32], roots: [2; 32], count: 1, index: [3; 32], inputs: [4; 32] };
    let mut holder = Holder::new(
        "helper",
        ExecutionId(1),
        Attempt(1),
        roots,
        Set::empty_in(alloc),
        BTreeSet::new_in(alloc),
        HolderConfig::default(),
        alloc,
    );
    let mut effects = Vec::new_in(alloc);
    holder.start(0, &mut effects);
    let beat = effects
        .iter()
        .find_map(|effect| match effect {
            Effect::Send { id, call: Call::Events(_) } => Some(*id),
            _ => None,
        })
        .expect("a heartbeat was sent");
    effects.clear();
    let moved = blaze_dist::Refusal::AttemptMoved.to_wire(None, alloc);
    holder.step(1, Event::Reply { id: beat, reply: Reply::Events(Err(moved)) }, &mut effects);
    assert!(holder.is_stopped());
    assert!(
        effects.iter().any(|effect| matches!(effect, Effect::Stop(StopWhy::AttemptMoved))),
        "{effects:?}"
    );
}

#[test]
fn heartbeat_resend_does_not_grow_independent_pending_heartbeats() {
    let alloc = heap();
    let roots =
        RootSet { spec: [1; 32], roots: [2; 32], count: 1, index: [3; 32], inputs: [4; 32] };
    let mut holder = Holder::new(
        "helper",
        ExecutionId(1),
        Attempt(1),
        roots,
        Set::empty_in(alloc),
        BTreeSet::new_in(alloc),
        HolderConfig { heartbeat_ms: 5, retry_ms: 20, idle_ms: 2 },
        alloc,
    );
    let mut effects = Vec::new_in(alloc);
    holder.start(0, &mut effects);
    let first = sent(&effects);
    for at in [5, 10, 15, 20, 25, 30] {
        effects.clear();
        holder.step(at, Event::Tick, &mut effects);
        let beats = Vec::from_iter_in(
            effects.iter().filter_map(|effect| match effect {
                Effect::Send { id, call: Call::Events(_) } => Some(*id),
                _ => None,
            }),
            alloc,
        );
        assert_eq!(beats.as_slice(), &[first]);
    }
}
