//! Lifecycle boundaries that must hold independently of the randomized load.

use blaze_dist::jobs::{
    Effect, ExecState, Execution, ExecutionConfig, Subscriber, SubscriberState,
};
use blaze_dist::work::EndWhy;
use blaze_wire::{
    Attempt, BuildSpec, Command, Digest, DoneEvent, ExecutionId, JobContent, JobEvent, LocalPolicy,
    Namespace, Outcome, ProfileSpec, RefusedEvent, SourceRef, StoreIdentity, WorkersEventsRequest,
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
    e.assign(1, "helper", &[], &mut Vec::new()).unwrap();
    assert_eq!(e.attempt(), Attempt(2));
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

fn done(e: &Execution) -> WorkersEventsRequest {
    WorkersEventsRequest {
        execution: e.id,
        attempt: e.attempt(),
        events: vec![JobEvent::Done(DoneEvent { outcome: Outcome::Passed, counters: Vec::new() })],
    }
}

#[test]
fn completion_and_timer_order_preserve_each_subscribers_deadline() {
    for timer_first in [false, true] {
        for last in [false, true] {
            for now in [10, 11] {
                let mut e = Execution::new(
                    0,
                    ExecutionId(1),
                    JobContent([1; 32]),
                    Namespace::Main,
                    subscriber(1, 10),
                    ExecutionConfig::default(),
                    &mut Vec::new(),
                );
                if !last {
                    let mut b = subscriber(2, 100);
                    b.policy = LocalPolicy::Fallback;
                    e.subscribe(b).unwrap();
                }
                e.assign(0, "lead", &[], &mut Vec::new()).unwrap();
                let request = done(&e);
                let mut out = Vec::new();
                if timer_first {
                    e.tick(now, &mut out);
                }
                e.worker_events(now, "lead", &request, &mut out).unwrap();
                e.tick(now, &mut out);
                e.worker_events(now + 1, "lead", &request, &mut out).unwrap();
                let outcomes: Vec<_> = out
                    .iter()
                    .filter_map(|fx| match fx {
                        Effect::SubscriberEnded { subscriber, outcome, policy } => {
                            Some((*subscriber, outcome.clone(), policy.clone()))
                        }
                        _ => None,
                    })
                    .collect();
                let mut expected = vec![(
                    1,
                    if now > 10 { Outcome::TimedOut } else { Outcome::Passed },
                    LocalPolicy::Never,
                )];
                if !last {
                    expected.push((2, Outcome::Passed, LocalPolicy::Fallback));
                }
                assert_eq!(outcomes, expected, "timer_first={timer_first}, last={last}, now={now}");
                assert_eq!(
                    e.state(),
                    ExecState::Ended(if last && now > 10 {
                        Outcome::Cancelled
                    } else {
                        Outcome::Passed
                    })
                );
                assert_eq!(
                    e.events(0).iter().filter(|x| matches!(x.event, JobEvent::Done(_))).count(),
                    1
                );
            }
        }
    }
}

fn reservation() -> blaze_wire::Reservation {
    blaze_wire::Reservation { memory: 4, cpus: 4, disk: 4 }
}

fn registry() -> (blaze_dist::jobs::Jobs, String) {
    let mut a = blaze_dist::admission::Admission::new(Default::default());
    a.add_machine("machine", [1; 32], reservation());
    let slot = a.add_builder(0, "machine", reservation()).unwrap();
    (blaze_dist::jobs::Jobs::new(a, Default::default()), slot)
}

fn submit(jobs: &mut blaze_dist::jobs::Jobs, tag: u64) -> ExecutionId {
    jobs.submit(
        0,
        &blaze_wire::JobRequest {
            spec: spec([1; 32], tag),
            namespace: Namespace::Main,
            deadline_ms: 100_000,
            local: LocalPolicy::Never,
            test_timeout_ms: None,
        },
        reservation(),
        &mut Vec::new(),
    )
    .unwrap()
    .execution
}

#[test]
fn a_duplicate_kill_ack_cannot_release_a_new_assignment_or_its_draining_slot() {
    use blaze_dist::admission::ExecutorPhase;
    for draining in [false, true] {
        for alive in [false, true] {
            let (mut jobs, slot) = registry();
            let a = submit(&mut jobs, 1);
            jobs.tick(0, &mut Vec::new());
            let request = done(jobs.execution(a).unwrap());
            let mut out = Vec::new();
            jobs.worker_events(1, &slot, &request, &mut out).unwrap();
            let token = out
                .iter()
                .find_map(|(_, fx)| match fx {
                    Effect::Kill { executor, assignment } if executor == &slot => Some(*assignment),
                    _ => None,
                })
                .unwrap();
            jobs.killed(2, &slot, token, true);
            jobs.killed(2, &slot, token, true); // duplicate before reuse is inert too
            let b = submit(&mut jobs, 2);
            jobs.tick(3, &mut Vec::new());
            assert_eq!(jobs.admission_view().executor(&slot).unwrap().busy, Some(b));
            if draining {
                assert!(jobs.admission().rebuild(4, &slot, reservation()).is_none());
            }
            let before = jobs.admission_view().executor(&slot).unwrap().clone();
            jobs.killed(5, &slot, token, alive);
            // Even the current token cannot acknowledge a stop that has not been requested.
            jobs.killed(5, &slot, jobs.execution(b).unwrap().assignment(), true);
            assert_eq!(jobs.admission_view().executor(&slot), Some(&before));
            let c = submit(&mut jobs, 3);
            jobs.tick(6, &mut Vec::new());
            assert_eq!(jobs.execution(c).unwrap().state(), ExecState::Queued);
            assert_eq!(jobs.admission_view().executors().count(), 1);
            assert_eq!(
                before.phase,
                if draining { ExecutorPhase::Draining } else { ExecutorPhase::Ready }
            );
            let request = done(jobs.execution(b).unwrap());
            let b_token = jobs.execution(b).unwrap().assignment();
            jobs.worker_events(7, &slot, &request, &mut Vec::new()).unwrap();
            jobs.killed(8, &slot, b_token, true);
            jobs.tick(9, &mut Vec::new());
            assert_eq!(jobs.execution(c).unwrap().state(), ExecState::Running);
            if draining {
                assert_eq!(
                    jobs.admission_view().executor(&slot).unwrap().phase,
                    ExecutorPhase::Stopped
                );
                assert_ne!(jobs.execution(c).unwrap().lead(), Some(slot.as_str()));
            }
        }
    }
}

#[test]
fn promoted_permanently_stalled_helper_is_cancelled_at_deadline_without_coverage() {
    use blaze_wire::{BatchResult, RootSet, Set, WorkJoinRequest, WorkLeaseRequest};
    let mut config = ExecutionConfig::default();
    config.heartbeat_ms = 5;
    config.work.progress_first_ms = 10;
    let mut e = Execution::new(
        0,
        ExecutionId(1),
        JobContent([1; 32]),
        Namespace::Main,
        subscriber(1, 50),
        config,
        &mut Vec::new(),
    );
    e.assign(0, "lead", &["stuck".to_owned(), "healthy".to_owned()], &mut Vec::new()).unwrap();
    let roots =
        RootSet { spec: [1; 32], roots: [2; 32], count: 1, index: [3; 32], inputs: [4; 32] };
    let heartbeat = |e: &Execution| WorkersEventsRequest {
        execution: e.id,
        attempt: e.attempt(),
        events: Vec::new(),
    };
    // The original lead disappears. The helper keeps heartbeating and is promoted.
    e.worker_events(16, "stuck", &heartbeat(&e), &mut Vec::new()).unwrap();
    let old = e.assignment();
    let mut out = Vec::new();
    e.tick(16, &mut out);
    assert!(out.contains(&Effect::Kill { executor: "lead".to_owned(), assignment: old }));
    assert!(out.contains(&Effect::NeedLead));
    e.assign(16, "stuck", &["healthy".to_owned()], &mut out).unwrap();
    e.join(
        16,
        "stuck",
        &WorkJoinRequest {
            execution: e.id,
            attempt: e.attempt(),
            roots,
            rejected: Set::new(Vec::new()),
        },
        &mut out,
    )
    .unwrap();
    let grant = e
        .lease(16, "stuck", &WorkLeaseRequest { execution: e.id, attempt: e.attempt() }, &mut out)
        .unwrap()
        .grant
        .unwrap();
    // Never deliver a Compiled event. Heartbeats continue, but progress revokes the lease.
    for now in [21, 26, 31, 36, 41, 46, 50] {
        e.worker_events(now, "stuck", &heartbeat(&e), &mut out).unwrap();
        e.tick(now, &mut out);
    }
    assert_eq!(e.state(), ExecState::Running);
    assert_eq!(e.lead(), Some("stuck"));
    assert!(!e.work().unwrap().is_done());
    let late = BatchResult {
        lease: grant.lease,
        seq: 1,
        items: Vec::new(),
        packs: Vec::new(),
        returned_roots: grant.roots,
        returned_names: Set::new(Vec::new()),
    };
    assert_eq!(e.result(50, "stuck", &late, &mut out), Err(blaze_dist::Refusal::DeadLease));
    let current = e.assignment();
    e.tick(51, &mut out);
    assert_eq!(e.state(), ExecState::Ended(Outcome::Cancelled));
    assert_eq!(e.work().unwrap().ended(), Some(EndWhy::Deadline));
    assert!(!e.work().unwrap().is_done());
    assert!(out.contains(&Effect::Kill { executor: "stuck".to_owned(), assignment: current }));
    assert!(out.contains(&Effect::Kill { executor: "healthy".to_owned(), assignment: current }));
    assert!(!out.iter().any(|fx| matches!(fx, Effect::Work(blaze_dist::work::Effect::Done))));
    assert_eq!(e.result(52, "stuck", &late, &mut out), Err(blaze_dist::Refusal::Ended));
}

fn spec(platform: Digest, i: u64) -> BuildSpec {
    let profile = ProfileSpec {
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
    };
    BuildSpec {
        source: SourceRef::Root(blaze_wire::RootRef { root: [i as u8; 32] }),
        lock: [1; 32],
        units: Vec::new(),
        asked: blaze_wire::Set::new(Vec::new()),
        profile,
        platform,
        rustflags: Vec::new(),
        config_env: blaze_wire::Set::new(Vec::new()),
        script_env: blaze_wire::Set::new(Vec::new()),
        cwd: String::new(),
        command: Command::Build,
        toolchain: StoreIdentity([2; 32]),
    }
}
