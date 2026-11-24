//! Allocation ownership observed through the transport's real request paths.

use blaze_dist::admission::Admission;
use blaze_dist::jobs::{Effect, Jobs};
use blaze_wire::{JobEvent, JobRequest, LocalPolicy, Namespace, Reservation, Stage, WireAlloc};

use crate::coordinator::{Config, Coordinator};
use crate::pack::test_alloc::{Counted, Counts};
use crate::protocol::{Command, Request, hello, submission};

fn test_alloc() -> WireAlloc {
    blaze_own::testing::heap().shared()
}

fn coordinator() -> (Coordinator, Request) {
    let alloc = test_alloc();
    const NEED: Reservation = Reservation { memory: 1, cpus: 1, disk: 1 };
    let mut admission = Admission::new(Default::default(), alloc);
    admission.add_machine("machine", [1; 32], Reservation { memory: 8, cpus: 8, disk: 8 });
    admission.add_builder(0, "machine", NEED, alloc).unwrap();
    let jobs = Jobs::new(admission, Default::default(), alloc);
    let mut coordinator = Coordinator::new(jobs, Config::default(), [7; 32], alloc);
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
            alloc,
        )),
    };
    (coordinator, request)
}

#[test]
fn coordinator_outputs_and_replays_use_the_callers_allocator() {
    let alloc = test_alloc();
    let (mut actual, request) = coordinator();
    let (mut control, control_request) = coordinator();
    let counts = Counts::default();

    let (reply, effects) = actual.request(0, &request, Counted(&counts)).unwrap();
    let (expected, expected_effects) = control.request(0, &control_request, alloc).unwrap();
    assert_eq!(reply.as_slice(), expected.as_slice());
    assert_eq!(effects.as_slice(), expected_effects.as_slice());
    let mut stages = effects.iter().filter_map(|(_, effect)| match effect {
        Effect::Event(item) => match &item.event {
            JobEvent::Stage(event) => Some(&event.stage),
            _ => None,
        },
        _ => None,
    });
    for stage in [Stage::Queued, Stage::Admitted, Stage::Assigned] {
        assert_eq!(stages.next(), Some(&stage), "the submit route must admit the execution");
    }
    assert_eq!(stages.next(), None);
    assert!(core::ptr::eq(reply.allocator().0, &counts));
    assert!(core::ptr::eq(effects.allocator().0, &counts));
    assert!(counts.allocations.get() > 0);
    drop((reply, effects));
    assert_eq!(
        counts.allocations.get(),
        counts.deallocations.get(),
        "the session journal must not retain the caller's output allocations"
    );

    // A retry copies the journal into this caller's allocator and emits no duplicate effects.
    let before = counts.allocations.get();
    let (replay, effects) = actual.request(1, &request, Counted(&counts)).unwrap();
    assert_eq!(replay.as_slice(), expected.as_slice());
    assert!(effects.is_empty());
    assert!(core::ptr::eq(replay.allocator().0, &counts));
    assert!(core::ptr::eq(effects.allocator().0, &counts));
    assert_eq!(counts.allocations.get(), before + 1);
    drop((replay, effects));
    assert_eq!(counts.allocations.get(), counts.deallocations.get());

    // Submit already admitted the execution. An idle tick keeps this caller's allocator
    // without repeating those transitions; shutdown then produces genuine stop effects.
    let effects = actual.tick(2, Counted(&counts));
    let expected_effects = control.tick(2, alloc);
    assert!(effects.is_empty(), "an idle tick must not repeat the submit route's admission");
    assert_eq!(effects.as_slice(), expected_effects.as_slice());
    assert!(core::ptr::eq(effects.allocator().0, &counts));
    drop(effects);
    assert_eq!(counts.allocations.get(), counts.deallocations.get());

    let effects = actual.shutdown(3, Counted(&counts));
    let expected_effects = control.shutdown(3, alloc);
    assert!(!effects.is_empty());
    assert_eq!(effects.as_slice(), expected_effects.as_slice());
    assert!(core::ptr::eq(effects.allocator().0, &counts));
    drop(effects);
    assert_eq!(counts.allocations.get(), counts.deallocations.get());
}
