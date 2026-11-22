//! The caller's allocator follows effects through the real state-machine routes.

use std::alloc::{AllocError, Allocator, AllocatorClone, Global, Layout};
use std::cell::Cell;
use std::collections::BTreeSet;
use std::ptr::NonNull;

use blaze_wire::{
    Attempt, BatchResult, EntryKey, EntryRef, ExecutionId, ItemOutcome, ItemResult, JobRequest,
    LocalPolicy, Namespace, Reservation, RootSet, Set, WorkJoinRequest, WorkLeaseRequest, WorkName,
};

use crate::admission::{Admission, AdmissionConfig, Waiting};
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

#[test]
fn admission_names_and_scratch_use_the_callers_allocator() {
    let counts = Counts::default();
    let need = Reservation { memory: 1, cpus: 1, disk: 1 };
    let mut actual = Admission::new(AdmissionConfig { pr_idle_ms: 5, ..Default::default() });
    actual.add_machine("machine", [1; 32], Reservation { memory: 4, cpus: 4, disk: 4 });
    let mut control = actual.clone();
    {
        let builder = actual.add_builder(0, "machine", need.clone(), Counted(&counts)).unwrap();
        let expected_builder = control.add_builder(0, "machine", need.clone(), Global).unwrap();
        assert_eq!(builder.as_ref(), expected_builder.as_ref());
        assert!(std::ptr::eq(Box::allocator(&builder).0, &counts));
        assert_eq!(counts.allocations.get(), 1, "the returned name is allocated directly in the caller");

        let idle = actual.idle_for(&Namespace::Main, &[1; 32], &need, &[]).unwrap();
        assert_eq!(idle, builder.as_ref());
        assert_eq!(idle.as_ptr(), actual.executor(&builder).unwrap().id.as_ptr());
        assert!(actual.idle_for(&Namespace::Main, &[1; 32], &need, &[idle]).is_none());
        assert_eq!(counts.allocations.get(), 1, "idle lookup borrows the retained name");

        for (execution, namespace) in [
            (1, Namespace::Pr(blaze_wire::PrNamespace { n: 7 })),
            (2, Namespace::Main),
            (3, Namespace::Pr(blaze_wire::PrNamespace { n: 8 })),
        ] {
            let waiting = Waiting { execution: ExecutionId(execution), namespace, platform: [1; 32], need: need.clone(), at: 0 };
            actual.enqueue(waiting.clone()).unwrap();
            control.enqueue(waiting).unwrap();
        }
        let before_admit = counts.allocations.get();
        let admitted = actual.admit(1, Counted(&counts));
        let expected = control.admit(1, Global);
        assert_eq!(admitted, expected);
        assert_eq!(admitted.iter().map(|item| item.execution).collect::<Vec<_>>(), [ExecutionId(2), ExecutionId(1), ExecutionId(3)]);
        assert!(std::ptr::eq(admitted.allocator().0, &counts));
        for item in &admitted {
            assert!(std::ptr::eq(Box::allocator(&item.executor).0, &counts));
        }
        assert!(counts.allocations.get() >= before_admit + 4, "names and admission scratch use the caller");

        let replacement = actual.rebuild(2, &builder, need.clone(), Counted(&counts)).unwrap();
        let expected_replacement = control.rebuild(2, &expected_builder, need.clone(), Global).unwrap();
        assert_eq!(replacement.as_ref(), expected_replacement.as_ref());
        assert!(std::ptr::eq(Box::allocator(&replacement).0, &counts));

        let idle_pr = &admitted[1].executor;
        actual.finished(3, idle_pr, ExecutionId(1), Attempt(1));
        control.finished(3, &expected[1].executor, ExecutionId(1), Attempt(1));
        let reaped = actual.reap(9, Counted(&counts));
        let expected_reaped = control.reap(9, Global);
        assert_eq!(reaped.len(), 1);
        assert!(reaped.iter().map(|name| name.as_ref()).eq(expected_reaped.iter().map(|name| name.as_ref())));
        assert!(std::ptr::eq(reaped.allocator().0, &counts));
        assert!(std::ptr::eq(Box::allocator(&reaped[0]).0, &counts));

        let closed = actual.close_pr(8, Counted(&counts));
        let expected_closed = control.close_pr(8, Global);
        assert_eq!(closed.len(), 1);
        assert_eq!(closed[0].1, Some((ExecutionId(3), Attempt(1))));
        assert!(closed.iter().map(|(name, busy)| (name.as_ref(), busy))
            .eq(expected_closed.iter().map(|(name, busy)| (name.as_ref(), busy))));
        assert!(std::ptr::eq(closed.allocator().0, &counts));
        assert!(std::ptr::eq(Box::allocator(&closed[0].0).0, &counts));
        assert_eq!(actual.used("machine"), control.used("machine"));
        actual.check().unwrap();
        control.check().unwrap();
    }
    assert_eq!(counts.allocations.get(), counts.deallocations.get(), "admission retains no caller-owned scratch or names");
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
    let lead = admission.add_builder(0, "machine", reservation.clone(), Global).unwrap();
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

#[test]
fn holder_keeps_the_callers_index_storage() {
    fn indexed_plan<A: AllocatorClone>(holder: &mut Holder<A>, name: WorkName) -> holder::Plan {
        let mut out = Vec::new_in(Global);
        holder.step(0, Event::Loaded, &mut out);
        let id = out.iter().find_map(|effect| match effect {
            holder::Effect::Send { id, .. } => Some(*id),
            _ => None,
        }).unwrap();
        out.clear();
        holder.step(1, Event::Reply {
            id,
            reply: holder::Reply::Join(Ok(blaze_wire::WorkJoinAnswer { roots: roots(), fixed: true })),
        }, &mut out);
        let id = out.iter().find_map(|effect| match effect {
            holder::Effect::Send { id, .. } => Some(*id),
            _ => None,
        }).unwrap();
        out.clear();
        holder.step(2, Event::Reply {
            id,
            reply: holder::Reply::Lease(Ok(blaze_wire::WorkLeaseAnswer {
                grant: Some(blaze_wire::Grant {
                    lease: blaze_wire::Lease {
                        execution: ExecutionId(1), attempt: Attempt(1), holder: "helper".into(), generation: 1,
                    },
                    roots: Set::default(),
                    names: Set::new(vec![name]),
                }),
            })),
        }, &mut out);
        out.into_iter().find_map(|effect| match effect {
            holder::Effect::Compile(plan) => Some(plan),
            _ => None,
        }).unwrap()
    }

    let counts = Counts::default();
    let alloc = Counted(&counts);
    let name = WorkName([7; 32]);
    let mut index = BTreeSet::new_in(&alloc);
    index.insert(name);
    let allocated = counts.allocations.get();
    assert!(allocated > 0);
    let mut holder = Holder::new("helper", ExecutionId(1), Attempt(1), roots(), Set::default(), index, HolderConfig::default());
    assert_eq!(counts.allocations.get(), allocated, "construction keeps the existing index nodes");
    assert_eq!(counts.deallocations.get(), 0, "the supplied index was not replaced");
    assert_eq!(indexed_plan(&mut holder, name).compile, [name]);
    assert_eq!(counts.allocations.get(), allocated);
    assert_eq!(counts.deallocations.get(), 0);

    // Removing the index changes the real holder's compile plan for the same granted name.
    let mut without_index = Holder::new("helper", ExecutionId(1), Attempt(1), roots(), Set::default(), BTreeSet::new_in(Global), HolderConfig::default());
    assert!(indexed_plan(&mut without_index, name).compile.is_empty());
    drop(holder);
    assert_eq!(counts.allocations.get(), counts.deallocations.get());
}

#[test]
fn simulation_trace_writes_directly_into_the_callers_buffer() {
    use std::io::Write as _;

    let counts = Counts::default();
    let mut actual = Vec::new_in(Counted(&counts));
    let actual_result = crate::sim::work::trace(0, &mut |event| writeln!(&mut actual, "{event}").unwrap()).unwrap();
    let mut expected = Vec::new_in(Global);
    let expected_result = crate::sim::work::trace(0, &mut |event| writeln!(&mut expected, "{event}").unwrap()).unwrap();
    assert_eq!(actual_result, expected_result);
    assert_eq!(actual.as_slice(), expected.as_slice());
    assert!(!actual.is_empty());
    assert!(counts.allocations.get() > 0);
    assert!(std::ptr::eq(actual.allocator().0, &counts));
    drop(actual);
    assert_eq!(counts.allocations.get(), counts.deallocations.get());
}

#[test]
fn committed_names_use_the_callers_allocator_and_exact_result_identity() {
    let execution = ExecutionId(1);
    let attempt = Attempt(1);
    let mut work = crate::work::WorkSet::new(execution, attempt, "lead", Default::default());
    work.join(
        0,
        "lead",
        &WorkJoinRequest { execution, attempt, roots: roots(), rejected: Set::default() },
        &mut Vec::new_in(Global),
    ).unwrap();
    let grant = work.lease(
        1,
        "lead",
        &WorkLeaseRequest { execution, attempt },
        &mut Vec::new_in(Global),
    ).unwrap().grant.unwrap();
    let name = WorkName([7; 32]);
    let mut result = BatchResult {
        lease: grant.lease,
        seq: 1,
        items: vec![ItemResult {
            name,
            outcome: ItemOutcome::Exported(EntryRef { entry: EntryKey(name.0) }),
            found: Set::default(),
            unnamed: Set::default(),
        }],
        packs: Vec::new(),
        returned_roots: Set::default(),
        returned_names: Set::default(),
    };
    work.result(2, "lead", &result, &mut Vec::new_in(Global)).unwrap();

    let counts = Counts::default();
    let committed = work.committed_names(&result, Counted(&counts));
    assert_eq!(committed.as_slice(), &[name]);
    assert!(counts.allocations.get() > 0);
    assert!(std::ptr::eq(committed.allocator().0, &counts));

    let before = counts.allocations.get();
    assert_eq!(work.committed_names(&result, Global).as_slice(), committed.as_slice());
    assert_eq!(counts.allocations.get(), before);

    // The same shape from another result cannot claim this result's publication evidence.
    result.seq += 1;
    assert!(work.committed_names(&result, Counted(&counts)).is_empty());
    drop(committed);
    assert_eq!(counts.allocations.get(), counts.deallocations.get());
}
