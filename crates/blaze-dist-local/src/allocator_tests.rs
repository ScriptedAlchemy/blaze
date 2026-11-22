//! Allocation ownership observed through the transport's real request paths.

use std::alloc::Global;

use blaze_dist::admission::Admission;
use blaze_dist::jobs::Jobs;
use blaze_wire::{JobRequest, LocalPolicy, Namespace, Reservation};

use crate::coordinator::{Config, Coordinator};
use crate::pack::test_alloc::{Counted, Counts};
use crate::protocol::{Command, Request, hello, submission};

fn coordinator() -> (Coordinator, Request) {
    const NEED: Reservation = Reservation { memory: 1, cpus: 1, disk: 1 };
    let mut admission = Admission::new(Default::default());
    admission.add_machine("machine", [1; 32], Reservation { memory: 8, cpus: 8, disk: 8 });
    admission.add_builder(0, "machine", NEED, Global).unwrap();
    let jobs = Jobs::new(admission, Default::default());
    let mut coordinator = Coordinator::new(jobs, Config::default(), [7; 32]);
    let welcome = coordinator.hello(0, &hello([8; 32])).unwrap();
    let request = Request {
        epoch: welcome.epoch,
        session: welcome.session,
        sequence: 1,
        command: Command::Submit(submission(
            &JobRequest {
                unit: [9; 32],
                platform: [1; 32],
                namespace: Namespace::Main,
                source: [2; 32],
                deadline_ms: 1_000,
                local: LocalPolicy::Never,
            },
            NEED,
        )),
    };
    (coordinator, request)
}

#[test]
fn coordinator_outputs_and_replays_use_the_callers_allocator() {
    let (mut actual, request) = coordinator();
    let (mut control, control_request) = coordinator();
    let counts = Counts::default();

    let (reply, effects) = actual.request(0, &request, Counted(&counts)).unwrap();
    let (expected, expected_effects) = control.request(0, &control_request, Global).unwrap();
    assert_eq!(reply.as_slice(), expected.as_slice());
    assert_eq!(effects.as_slice(), expected_effects.as_slice());
    assert!(!effects.is_empty(), "submitting a real execution must emit its queued stage");
    assert!(std::ptr::eq(reply.allocator().0, &counts));
    assert!(std::ptr::eq(effects.allocator().0, &counts));
    assert!(counts.allocations.get() > 0);
    drop((reply, effects));
    assert_eq!(
        counts.allocations.get(), counts.deallocations.get(),
        "the session journal must not retain the caller's output allocations"
    );

    // A retry copies the journal into this caller's allocator and emits no duplicate effects.
    let before = counts.allocations.get();
    let (replay, effects) = actual.request(1, &request, Counted(&counts)).unwrap();
    assert_eq!(replay.as_slice(), expected.as_slice());
    assert!(effects.is_empty());
    assert!(std::ptr::eq(replay.allocator().0, &counts));
    assert!(std::ptr::eq(effects.allocator().0, &counts));
    assert_eq!(counts.allocations.get(), before + 1);
    drop((replay, effects));
    assert_eq!(counts.allocations.get(), counts.deallocations.get());

    // The same allocator also owns autonomous admission and shutdown output buffers.
    let effects = actual.tick(2, Counted(&counts));
    let expected_effects = control.tick(2, Global);
    assert!(!effects.is_empty());
    assert_eq!(effects.as_slice(), expected_effects.as_slice());
    assert!(std::ptr::eq(effects.allocator().0, &counts));
    drop(effects);
    assert_eq!(counts.allocations.get(), counts.deallocations.get());

    let effects = actual.shutdown(3, Counted(&counts));
    let expected_effects = control.shutdown(3, Global);
    assert!(!effects.is_empty());
    assert_eq!(effects.as_slice(), expected_effects.as_slice());
    assert!(std::ptr::eq(effects.allocator().0, &counts));
    drop(effects);
    assert_eq!(counts.allocations.get(), counts.deallocations.get());
}
