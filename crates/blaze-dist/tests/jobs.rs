//! Lifecycle boundaries that must hold independently of the randomized load.

use blaze_dist::admission::{Admission, AdmissionConfig};
use blaze_dist::jobs::{
    Effect, ExecState, Execution, ExecutionConfig, Jobs, Subscriber, SubscriberState,
};
use blaze_dist::work::EndWhy;
use blaze_wire::{
    Attempt, BuildSpec, Command, DoneEvent, ExecutionId, JobContent, JobEvent, JobRequest,
    LocalPolicy, Namespace, Outcome, PrNamespace, ProfileSpec, RefusedEvent, Reservation, RootRef,
    Set, SourceRef, StoreIdentity, WorkersEventsRequest,
};

fn execution() -> Execution {
    let mut e = Execution::new(
        0,
        ExecutionId(1),
        JobContent([1; 32]),
        Namespace::Main,
        subscriber(1, 100_000),
        ExecutionConfig::default(),
        &mut Vec::new(),
    );
    e.assign(0, "lead", &["helper".to_owned()], &mut Vec::new()).unwrap();
    e
}

fn subscriber(id: u64, deadline_ms: u64) -> Subscriber {
    Subscriber {
        id,
        deadline_ms,
        policy: LocalPolicy::Never,
        test_timeout_ms: None,
        state: SubscriberState::Attached,
    }
}

#[test]
fn a_subscribers_timeout_does_not_cancel_another_subscriber() {
    let mut e = execution();
    e.subscribe(subscriber(2, 10)).unwrap();
    let mut effects = Vec::new();
    e.tick(11, &mut effects);
    assert_eq!(e.state(), ExecState::Running);
    assert!(effects.iter().any(|effect| matches!(
        effect,
        Effect::SubscriberEnded { subscriber: 2, outcome: Outcome::TimedOut, .. }
    )));
    e.unsubscribe(12, 1, &mut effects).unwrap();
    assert_eq!(e.state(), ExecState::Ended(Outcome::Cancelled));
    assert_eq!(e.work().unwrap().ended(), Some(EndWhy::Cancelled));
}

#[test]
fn a_reassigned_execution_refuses_the_old_attempts_events() {
    let mut e = execution();
    let mut effects = Vec::new();
    e.assign(1, "helper", &[], &mut effects).unwrap();
    assert_eq!(e.attempt(), Attempt(2));
    assert!(effects.contains(&Effect::Kill { executor: "lead".to_owned(), attempt: Attempt(1) }));
    let request = WorkersEventsRequest { execution: e.id, attempt: Attempt(1), events: Vec::new() };
    assert_eq!(
        e.worker_events(2, "lead", &request, &mut Vec::new()),
        Err(blaze_dist::Refusal::AttemptMoved),
    );
}

#[test]
fn a_refusal_never_turns_into_a_passed_result() {
    let mut e = execution();
    let request = WorkersEventsRequest {
        execution: e.id,
        attempt: e.attempt(),
        events: vec![
            JobEvent::Refused(RefusedEvent {
                function: "function".to_owned(),
                why: "cannot compile".to_owned(),
            }),
            JobEvent::Done(DoneEvent { outcome: Outcome::Passed, counters: Vec::new() }),
        ],
    };
    e.worker_events(1, "lead", &request, &mut Vec::new()).unwrap();
    assert_eq!(e.state(), ExecState::Ended(Outcome::Refused));
    let verdicts: Vec<_> = e
        .events(0)
        .iter()
        .filter_map(|item| match &item.event {
            JobEvent::Done(done) => Some(done.outcome.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(verdicts, vec![Outcome::Refused]);
}

#[test]
fn reassignment_does_not_erase_an_executions_refusal() {
    let mut e = execution();
    let refused = WorkersEventsRequest {
        execution: e.id,
        attempt: e.attempt(),
        events: vec![JobEvent::Refused(RefusedEvent {
            function: "function".to_owned(),
            why: "cannot compile".to_owned(),
        })],
    };
    e.worker_events(1, "lead", &refused, &mut Vec::new()).unwrap();
    e.assign(2, "helper", &[], &mut Vec::new()).unwrap();
    let done = WorkersEventsRequest {
        execution: e.id,
        attempt: e.attempt(),
        events: vec![JobEvent::Done(DoneEvent { outcome: Outcome::Passed, counters: Vec::new() })],
    };
    e.worker_events(3, "helper", &done, &mut Vec::new()).unwrap();
    assert_eq!(e.state(), ExecState::Ended(Outcome::Refused));
}

#[test]
fn review_completion_respects_each_subscribers_deadline_without_a_tick() {
    for now in [10, 11] {
        let mut e = execution();
        e.subscribe(subscriber(2, 10)).unwrap();
        let done = WorkersEventsRequest {
            execution: e.id,
            attempt: e.attempt(),
            events: vec![JobEvent::Done(DoneEvent {
                outcome: Outcome::Passed,
                counters: Vec::new(),
            })],
        };
        let mut effects = Vec::new();
        e.worker_events(now, "lead", &done, &mut effects).unwrap();
        assert_eq!(e.state(), ExecState::Ended(Outcome::Passed));
        let expected = if now > 10 { Outcome::TimedOut } else { Outcome::Passed };
        let ended: Vec<_> = effects
            .iter()
            .filter_map(|effect| match effect {
                Effect::SubscriberEnded { subscriber: 2, outcome, .. } => Some(outcome),
                _ => None,
            })
            .collect();
        assert_eq!(ended, vec![&expected]);
        assert!(effects.iter().any(|effect| matches!(
            effect,
            Effect::SubscriberEnded { subscriber: 1, outcome: Outcome::Passed, .. }
        )));
    }
}

#[test]
fn review_completion_after_the_last_deadline_cannot_publish_success() {
    let mut e = execution();
    let done = WorkersEventsRequest {
        execution: e.id,
        attempt: e.attempt(),
        events: vec![JobEvent::Done(DoneEvent { outcome: Outcome::Passed, counters: Vec::new() })],
    };
    let mut effects = Vec::new();
    e.worker_events(100_001, "lead", &done, &mut effects).unwrap();
    assert_eq!(e.state(), ExecState::Ended(Outcome::Cancelled));
    assert_eq!(e.work().unwrap().ended(), Some(EndWhy::Deadline));
    assert!(effects.iter().any(|effect| matches!(
        effect,
        Effect::SubscriberEnded { subscriber: 1, outcome: Outcome::TimedOut, .. }
    )));
    assert!(!e.events(0).iter().any(|item| matches!(
        &item.event,
        JobEvent::Done(DoneEvent { outcome: Outcome::Passed, .. })
    )));
}

fn reservation() -> Reservation {
    Reservation { memory: 4, cpus: 4, disk: 4 }
}

fn request(source: u8) -> JobRequest {
    JobRequest {
        spec: BuildSpec {
            source: SourceRef::Root(RootRef { root: [source; 32] }),
            lock: [1; 32],
            units: Vec::new(),
            asked: Set::new(Vec::new()),
            profile: ProfileSpec {
                name: "dev".to_owned(),
                opt_level: 0,
                debug: 1,
                debug_assertions: true,
                overflow_checks: true,
                panic: 0,
                strip: 0,
                split_debuginfo: 0,
                backend: 0,
                mir_opt: 0,
            },
            platform: [1; 32],
            rustflags: Vec::new(),
            config_env: Set::new(Vec::new()),
            script_env: Set::new(Vec::new()),
            cwd: String::new(),
            command: Command::Build,
            toolchain: StoreIdentity([2; 32]),
        },
        namespace: Namespace::Main,
        deadline_ms: 100_000,
        local: LocalPolicy::Never,
        test_timeout_ms: None,
    }
}

#[test]
fn review_a_late_kill_ack_cannot_release_a_new_execution() {
    for alive in [true, false] {
        let mut admission = Admission::new(AdmissionConfig::default());
        admission.add_machine("machine", [1; 32], reservation());
        let builder = admission.add_builder(0, "machine", reservation()).unwrap();
        let mut jobs = Jobs::new(admission, ExecutionConfig::default());
        let mut effects = Vec::new();
        let first = jobs.submit(0, &request(1), reservation(), &mut effects).unwrap();
        jobs.tick(0, &mut effects);
        jobs.unsubscribe(1, first.execution, first.subscriber, &mut effects).unwrap();
        jobs.killed(1, &builder, first.execution, Attempt(1), true);

        let second = jobs.submit(2, &request(2), reservation(), &mut effects).unwrap();
        jobs.tick(2, &mut effects);
        assert_eq!(jobs.execution(second.execution).unwrap().lead(), Some(builder.as_str()));
        // A duplicated or reordered acknowledgement belongs to the first assignment.
        jobs.killed(3, &builder, first.execution, Attempt(1), alive);
        assert_eq!(
            jobs.admission_view().executor(&builder).unwrap().busy,
            Some((second.execution, Attempt(1))),
        );

        let third = jobs.submit(4, &request(3), reservation(), &mut effects).unwrap();
        jobs.tick(4, &mut effects);
        assert_eq!(jobs.execution(second.execution).unwrap().state(), ExecState::Running);
        assert_eq!(jobs.execution(third.execution).unwrap().state(), ExecState::Queued);
    }
}

#[test]
fn review_pr_supersession_follows_source_and_preserves_shared_executions() {
    let admission = Admission::new(AdmissionConfig::default());
    let mut jobs = Jobs::new(admission, ExecutionConfig::default());
    let mut effects = Vec::new();
    let mut linux = request(1);
    linux.namespace = Namespace::Pr(PrNamespace { n: 7 });
    let mut macos = linux.clone();
    macos.spec.platform = [2; 32];
    let mut release = linux.clone();
    release.spec.profile.name = "release".to_owned();
    let first = jobs.submit(0, &linux, reservation(), &mut effects).unwrap();
    let second = jobs.submit(0, &macos, reservation(), &mut effects).unwrap();
    let shared = jobs.submit(0, &release, reservation(), &mut effects).unwrap();
    let other = jobs.submit(0, &release, reservation(), &mut effects).unwrap();
    assert_eq!(shared.execution, other.execution);
    for id in [first.execution, second.execution, shared.execution] {
        assert_eq!(jobs.execution(id).unwrap().state(), ExecState::Queued);
    }

    let mut next_push = linux.clone();
    next_push.spec.source = request(2).spec.source;
    let next = jobs.submit(1, &next_push, reservation(), &mut effects).unwrap();
    for id in [first.execution, second.execution] {
        assert_eq!(jobs.execution(id).unwrap().state(), ExecState::Ended(Outcome::Cancelled));
    }
    assert_eq!(jobs.execution(shared.execution).unwrap().state(), ExecState::Queued);
    assert_eq!(jobs.execution(next.execution).unwrap().state(), ExecState::Queued);

    // Joining a protected older execution is another subscriber, not a push that rolls the
    // pull request back and cancels its newer source.
    let rejoined = jobs.submit(1, &release, reservation(), &mut effects).unwrap();
    assert_eq!(rejoined.execution, shared.execution);
    assert_eq!(jobs.execution(next.execution).unwrap().state(), ExecState::Queued);
    jobs.unsubscribe(2, rejoined.execution, rejoined.subscriber, &mut effects).unwrap();

    // A shared execution remains tracked: if its other subscriber leaves, a later push
    // supersedes it too rather than leaking an obsolete sole-subscriber job.
    jobs.unsubscribe(2, other.execution, other.subscriber, &mut effects).unwrap();
    next_push.spec.source = request(3).spec.source;
    let latest = jobs.submit(3, &next_push, reservation(), &mut effects).unwrap();
    for id in [shared.execution, next.execution] {
        assert_eq!(jobs.execution(id).unwrap().state(), ExecState::Ended(Outcome::Cancelled));
    }
    assert_eq!(jobs.execution(latest.execution).unwrap().state(), ExecState::Queued);
}

#[test]
fn review_a_replacement_lead_gets_its_own_compiled_event() {
    for changed in [false, true] {
        let mut e = execution();
        let roots = blaze_wire::RootSet {
            spec: [1; 32],
            roots: [2; 32],
            count: 0,
            index: [3; 32],
            inputs: [4; 32],
        };
        let first = blaze_wire::WorkJoinRequest {
            execution: e.id,
            attempt: e.attempt(),
            roots: roots.clone(),
            rejected: Set::new(Vec::new()),
        };
        e.join(0, "lead", &first, &mut Vec::new()).unwrap();
        assert!(e.work().unwrap().is_done());
        e.tick(15_001, &mut Vec::new());
        e.assign(15_002, "helper", &[], &mut Vec::new()).unwrap();
        let heartbeat =
            WorkersEventsRequest { execution: e.id, attempt: e.attempt(), events: Vec::new() };
        let mut effects = Vec::new();
        e.worker_events(15_002, "helper", &heartbeat, &mut effects).unwrap();
        assert!(!effects.iter().any(|effect| matches!(
            effect,
            Effect::Event(blaze_wire::EventItem {
                event: JobEvent::Stage(blaze_wire::StageEvent {
                    stage: blaze_wire::Stage::Compiled,
                    ..
                }),
                ..
            })
        )));
        let next = blaze_wire::WorkJoinRequest {
            execution: e.id,
            attempt: e.attempt(),
            roots: if changed {
                blaze_wire::RootSet { roots: [9; 32], count: 1, ..roots }
            } else {
                roots
            },
            rejected: Set::new(Vec::new()),
        };
        e.join(15_003, "helper", &next, &mut Vec::new()).unwrap();
        if changed {
            assert!(!e.work().unwrap().is_done());
            let request = blaze_wire::WorkLeaseRequest { execution: e.id, attempt: e.attempt() };
            let grant =
                e.lease(15_004, "helper", &request, &mut Vec::new()).unwrap().grant.unwrap();
            let result = blaze_wire::BatchResult {
                lease: grant.lease,
                seq: 1,
                items: vec![blaze_wire::ItemResult {
                    name: blaze_wire::WorkName([7; 32]),
                    outcome: blaze_wire::ItemOutcome::Local(blaze_wire::Why {
                        why: "lead tail".to_owned(),
                    }),
                    found: Set::new(Vec::new()),
                    unnamed: Set::new(Vec::new()),
                }],
                packs: Vec::new(),
                returned_roots: Set::new(Vec::new()),
                returned_names: Set::new(Vec::new()),
            };
            e.result(15_005, "helper", &result, &mut Vec::new()).unwrap();
        }
        assert!(e.work().unwrap().is_done());
        // Replaying the join must not duplicate this attempt's completion notification.
        e.join(15_006, "helper", &next, &mut Vec::new()).unwrap();
        let compiled: Vec<_> = e
            .events(0)
            .iter()
            .filter_map(|item| match &item.event {
                JobEvent::Stage(stage) if stage.stage == blaze_wire::Stage::Compiled => {
                    Some(item.attempt)
                }
                _ => None,
            })
            .collect();
        assert_eq!(compiled, vec![Attempt(1), Attempt(2)]);
    }
}
