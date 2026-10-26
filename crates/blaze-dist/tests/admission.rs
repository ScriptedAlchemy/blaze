//! Admission order and reservations, independent of randomized job scheduling.

use blaze_dist::admission::{Admission, AdmissionConfig, Waiting};
use blaze_wire::{ExecutionId, Namespace, PrNamespace, Reservation};

fn reservation(memory: u64) -> Reservation {
    Reservation { memory, cpus: memory as u32, disk: memory }
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
    let mut a = Admission::new(AdmissionConfig::default());
    a.add_machine("machine", [1; 32], reservation(8));
    a.add_builder(0, "machine", reservation(4)).unwrap();
    a.enqueue(job(1, Namespace::Pr(PrNamespace { n: 1 }))).unwrap();
    a.enqueue(job(2, Namespace::Main)).unwrap();
    let admitted = a.admit(1);
    assert_eq!(
        admitted.iter().map(|j| j.execution).collect::<Vec<_>>(),
        vec![ExecutionId(2), ExecutionId(1)],
    );
    a.check().unwrap();
}

#[test]
fn a_main_head_waiting_for_capacity_blocks_pr_admission() {
    let mut a = Admission::new(AdmissionConfig::default());
    a.add_machine("machine", [1; 32], reservation(4));
    a.enqueue(job(1, Namespace::Pr(PrNamespace { n: 1 }))).unwrap();
    a.enqueue(job(2, Namespace::Main)).unwrap();
    // Main needs an operator-started builder. A PR must not overtake it.
    assert!(a.admit(1).is_empty());
    a.add_builder(2, "machine", reservation(4)).unwrap();
    let admitted = a.admit(3);
    assert_eq!(admitted.len(), 1);
    assert_eq!(admitted[0].execution, ExecutionId(2));
    a.check().unwrap();
}

#[test]
fn rebuild_waits_for_the_old_busy_builders_reservation() {
    let mut a = Admission::new(AdmissionConfig::default());
    a.add_machine("machine", [1; 32], reservation(4));
    let builder = a.add_builder(0, "machine", reservation(4)).unwrap();
    a.enqueue(job(1, Namespace::Main)).unwrap();
    assert_eq!(a.admit(1).len(), 1);
    assert!(a.rebuild(2, &builder, reservation(4)).is_none());
    assert_eq!(a.used("machine").memory, 4);
    a.admit(3);
    assert_eq!(a.used("machine").memory, 4);
    a.finished(4, &builder);
    assert_eq!(a.used("machine").memory, 4);
    a.check().unwrap();
}

#[test]
fn a_draining_rebuild_waits_even_when_another_executor_frees_room() {
    let mut a = Admission::new(AdmissionConfig::default());
    a.add_machine("machine", [1; 32], reservation(8));
    let builder = a.add_builder(0, "machine", reservation(4)).unwrap();
    let neighbor = a.add_builder(0, "machine", reservation(4)).unwrap();
    a.take(&builder, ExecutionId(1));
    assert!(a.rebuild(1, &builder, reservation(4)).is_none());
    a.lost(2, &neighbor);
    a.admit(3);
    assert_eq!(a.used("machine").memory, 4);
    assert_eq!(
        a.executors().filter(|e| e.phase == blaze_dist::admission::ExecutorPhase::Ready).count(),
        0,
    );
    a.finished(4, &builder);
    assert_eq!(a.used("machine").memory, 4);
    a.check().unwrap();
}
