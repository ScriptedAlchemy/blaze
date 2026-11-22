//! The caller's allocator follows effects through the real state-machine routes.

use std::alloc::{AllocError, Allocator, Global, Layout};
use std::cell::Cell;
use std::collections::BTreeSet;
use std::ptr::NonNull;

use blaze_wire::{
    Attempt, ExecutionId, JobRequest, LocalPolicy, Namespace, Reservation, RootSet, Set,
    WorkJoinRequest,
};

use crate::admission::Admission;
use crate::holder::{self, Event, Holder, HolderConfig};
use crate::jobs::{self, Jobs};

#[derive(Default)]
struct Counts {
    allocations: Cell<usize>,
    deallocations: Cell<usize>,
}

#[derive(Clone, Copy)]
struct Counted<'a>(&'a Counts);

// SAFETY: every allocation and deallocation is delegated unchanged to Global. Clones refer
// to the same counters and can free one another's blocks, as the Allocator contract requires.
unsafe impl Allocator for Counted<'_> {
    fn allocate(&self, layout: Layout) -> Result<NonNull<[u8]>, AllocError> {
        let allocation = Global.allocate(layout)?;
        self.0.allocations.set(self.0.allocations.get() + 1);
        Ok(allocation)
    }

    unsafe fn deallocate(&self, pointer: NonNull<u8>, layout: Layout) {
        self.0.deallocations.set(self.0.deallocations.get() + 1);
        // SAFETY: this allocator only returns Global's allocations and preserves their layout.
        unsafe { Global.deallocate(pointer, layout) };
    }
}

fn roots() -> RootSet {
    RootSet { spec: [1; 32], roots: [2; 32], count: 1, index: [3; 32], inputs: [4; 32] }
}

fn join_through_registry<A: Allocator + Clone>(
    alloc: A,
    counts: &Counts,
) -> (Vec<(ExecutionId, jobs::Effect), A>, usize) {
    let reservation = Reservation { memory: 1, cpus: 1, disk: 1 };
    let mut admission = Admission::new(Default::default());
    admission.add_machine("machine", [1; 32], reservation.clone());
    let lead = admission.add_builder(0, "machine", reservation.clone()).unwrap();
    let mut jobs = Jobs::new(admission, Default::default());
    let answer = jobs
        .submit(
            0,
            &JobRequest {
                unit: [1; 32],
                platform: [1; 32],
                namespace: Namespace::Main,
                source: [2; 32],
                deadline_ms: 100_000,
                local: LocalPolicy::Never,
            },
            reservation,
            &mut Vec::new_in(Global),
        )
        .unwrap();
    jobs.tick(0, &mut Vec::new_in(Global));
    let attempt = jobs.execution(answer.execution).unwrap().attempt();

    // Preallocate the final log: growth observed during join must come from its two nested
    // logs (Jobs -> Execution -> WorkSet), not from the caller's output buffer.
    let mut out = Vec::with_capacity_in(64, alloc);
    let capacity = out.capacity();
    let before = counts.allocations.get();
    let joined = jobs
        .join(
            1,
            &lead,
            &WorkJoinRequest {
                execution: answer.execution,
                attempt,
                roots: roots(),
                rejected: Set::new(Vec::new()),
            },
            &mut out,
        )
        .unwrap();
    assert!(joined.fixed);
    assert_eq!(out.capacity(), capacity);
    assert!(out.iter().any(|(_, effect)| matches!(effect, jobs::Effect::Work(_))));
    let nested_allocations = counts.allocations.get() - before;
    (out, nested_allocations)
}

#[test]
fn nested_effect_logs_use_the_callers_allocator() {
    let counts = Counts::default();
    let (actual, nested) = join_through_registry(Counted(&counts), &counts);
    assert_eq!(nested, 2, "both routing layers must allocate from the caller");
    assert_eq!(counts.deallocations.get(), nested, "temporary logs were released");

    // Control: run the same transitions with another allocator. Their effects are equal,
    // but the counting oracle must detect that none of those buffers belong to it.
    let other = Counts::default();
    let (control, misplaced) = join_through_registry(Global, &other);
    assert_eq!(actual.as_slice(), control.as_slice());
    assert_eq!(misplaced, 0);
    assert_ne!(nested, misplaced);
    drop(actual);
    assert_eq!(counts.allocations.get(), counts.deallocations.get());
}

fn start_holder<A: Allocator>(alloc: A) -> Vec<holder::Effect, A> {
    let mut holder = Holder::new(
        "lead",
        ExecutionId(1),
        Attempt(1),
        roots(),
        Set::new(Vec::new()),
        BTreeSet::new(),
        HolderConfig::default(),
    );
    let mut out = Vec::new_in(alloc);
    holder.start(0, &mut out);
    holder.step(1, Event::Loaded, &mut out);
    assert_eq!(out.len(), 2, "a heartbeat and the loaded holder's join");
    out
}

#[test]
fn holder_effects_use_the_callers_allocator() {
    let counts = Counts::default();
    let actual = start_holder(Counted(&counts));
    assert!(counts.allocations.get() > 0);
    let control = start_holder(Global);
    assert_eq!(actual.as_slice(), control.as_slice());
    drop(actual);
    assert_eq!(counts.allocations.get(), counts.deallocations.get());
}
