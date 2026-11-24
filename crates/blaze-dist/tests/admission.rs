//! Admission order and reservations, independent of randomized job scheduling.

#![no_std]
#![allow(missing_docs)]

use blaze_dist::admission::{Admission, AdmissionConfig, Waiting};
use blaze_own::Vec;
use blaze_wire::{Attempt, ExecutionId, Namespace, PrNamespace, Reservation, WireAlloc};

fn heap() -> WireAlloc {
    blaze_own::testing::heap().shared()
}

fn reservation(memory: u64) -> Reservation {
    Reservation { memory, cpus: u32::try_from(memory).unwrap(), disk: memory }
}

fn job(id: u64, namespace: Namespace) -> Waiting {
    Waiting {
        execution: ExecutionId(id),
        namespace,
        platform: [1; 32],
        need: reservation(4),
        at: 0,
    }
}

#[test]
fn main_goes_first_even_when_a_pr_was_queued_first() {
    let alloc = heap();
    let mut a = Admission::new(AdmissionConfig::default(), alloc);
    a.add_machine("machine", [1; 32], reservation(8));
    a.add_builder(0, "machine", reservation(4), alloc).unwrap();
    a.enqueue(job(1, Namespace::Pr(PrNamespace { n: 1 }))).unwrap();
    a.enqueue(job(2, Namespace::Main)).unwrap();
    let admitted = a.admit(1, alloc);
    let ids = Vec::from_iter_in(admitted.iter().map(|j| j.execution), alloc);
    assert_eq!(ids.as_slice(), &[ExecutionId(2), ExecutionId(1)]);
    a.check().unwrap();
}

#[test]
fn a_main_head_waiting_for_capacity_blocks_pr_admission() {
    let alloc = heap();
    let mut a = Admission::new(AdmissionConfig::default(), alloc);
    a.add_machine("machine", [1; 32], reservation(4));
    a.enqueue(job(1, Namespace::Pr(PrNamespace { n: 1 }))).unwrap();
    a.enqueue(job(2, Namespace::Main)).unwrap();
    // Main needs an operator-started builder. A PR must not overtake it.
    assert!(a.admit(1, alloc).is_empty());
    a.add_builder(2, "machine", reservation(4), alloc).unwrap();
    let admitted = a.admit(3, alloc);
    assert_eq!(admitted.len(), 1);
    assert_eq!(admitted[0].execution, ExecutionId(2));
    a.check().unwrap();
}

#[test]
fn rebuild_waits_for_the_old_busy_builders_reservation() {
    let alloc = heap();
    let mut a = Admission::new(AdmissionConfig::default(), alloc);
    a.add_machine("machine", [1; 32], reservation(4));
    let builder = a.add_builder(0, "machine", reservation(4), alloc).unwrap();
    a.enqueue(job(1, Namespace::Main)).unwrap();
    assert_eq!(a.admit(1, alloc).len(), 1);
    assert!(a.rebuild(2, &builder, reservation(4), alloc).is_none());
    assert_eq!(a.used("machine").memory, 4);
    a.admit(3, alloc);
    assert_eq!(a.used("machine").memory, 4);
    a.finished(4, &builder, ExecutionId(1), Attempt(1));
    assert_eq!(a.used("machine").memory, 4);
    a.check().unwrap();
}

#[test]
fn a_draining_rebuild_waits_even_when_another_executor_frees_room() {
    let alloc = heap();
    let mut a = Admission::new(AdmissionConfig::default(), alloc);
    a.add_machine("machine", [1; 32], reservation(8));
    let builder = a.add_builder(0, "machine", reservation(4), alloc).unwrap();
    let neighbor = a.add_builder(0, "machine", reservation(4), alloc).unwrap();
    a.take(&builder, ExecutionId(1), Attempt(1));
    assert!(a.rebuild(1, &builder, reservation(4), alloc).is_none());
    a.lost(2, &neighbor);
    a.admit(3, alloc);
    assert_eq!(a.used("machine").memory, 4);
    assert_eq!(
        a.executors().filter(|e| e.phase == blaze_dist::admission::ExecutorPhase::Ready).count(),
        0,
    );
    a.finished(4, &builder, ExecutionId(1), Attempt(1));
    assert_eq!(a.used("machine").memory, 4);
    a.check().unwrap();
}

#[test]
fn an_old_attempts_completion_does_not_release_the_current_assignment() {
    let alloc = heap();
    let mut a = Admission::new(AdmissionConfig::default(), alloc);
    a.add_machine("machine", [1; 32], reservation(4));
    let builder = a.add_builder(0, "machine", reservation(4), alloc).unwrap();
    a.take(&builder, ExecutionId(1), Attempt(2));
    a.finished(1, &builder, ExecutionId(1), Attempt(1));
    assert_eq!(a.executor(&builder).unwrap().busy, Some((ExecutionId(1), Attempt(2))));
    a.finished(2, &builder, ExecutionId(1), Attempt(2));
    assert_eq!(a.executor(&builder).unwrap().busy, None);
}
