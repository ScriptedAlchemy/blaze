//! Lifecycle boundaries that must hold independently of the randomized load.

use blaze_dist::admission::{Admission, AdmissionConfig};
use blaze_dist::jobs::{
    AssignmentToken, Effect, ExecState, Execution, ExecutionConfig, Jobs, Subscriber,
    SubscriberState,
};
use blaze_dist::work::EndWhy;
use blaze_wire::{
    Attempt, BuildSpec, Command, Digest, DoneEvent, ExecutionId, JobContent, JobEvent, JobRequest,
    LocalPolicy, Namespace, Outcome, PrNamespace, ProfileSpec, RefusedEvent, Reservation, RootRef,
    Set, SourcePath, SourceRef, StoreIdentity, WorkersEventsRequest,
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

// Deadline/verdict tests explicitly establish zero authoritative work. This is not
// permission to infer cache-only success from a worker Done or an empty BuildSpec.
fn complete_empty_work(e: &mut Execution, now: u64) {
    let request = blaze_wire::WorkJoinRequest {
        execution: e.id,
        attempt: e.attempt(),
        roots: blaze_wire::RootSet {
            spec: [1; 32],
            roots: [2; 32],
            count: 0,
            index: [3; 32],
            inputs: [4; 32],
        },
        rejected: Set::new(Vec::new()),
    };
    let lead = e.lead().unwrap().to_owned();
    e.join(now, &lead, &request, &mut Vec::new()).unwrap();
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
    assert!(effects.contains(&Effect::Kill {
        executor: "lead".to_owned(),
        assignment: AssignmentToken { execution: e.id, attempt: Attempt(1) }
    }));
    let request = WorkersEventsRequest { execution: e.id, attempt: Attempt(1), events: Vec::new() };
    assert_eq!(
        e.worker_events(2, "lead", &request, &mut Vec::new()),
        Err(blaze_dist::Refusal::AttemptMoved),
    );
}

#[test]
fn a_refusal_never_turns_into_a_passed_result() {
    let mut e = execution();
    complete_empty_work(&mut e, 0);
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
    complete_empty_work(&mut e, 2);
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
        complete_empty_work(&mut e, 0);
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
                backend_opt: 0,
                backend_emit: 0,
            },
            platform: [1; 32],
            rustflags: Vec::new(),
            config_env: Set::new(Vec::new()),
            script_env: Set::new(Vec::new()),
            cwd: SourcePath::new(".".into()).unwrap(),
            command: Command::Build,
            toolchain: StoreIdentity([2; 32]),
            workspace_manifest: SourcePath::new("Cargo.toml".into()).unwrap(),
        },
        namespace: Namespace::Main,
        deadline_ms: 100_000,
        local: LocalPolicy::Never,
        test_timeout_ms: None,
    }
}

#[test]
fn resubmission_cannot_revive_an_execution_past_its_last_deadline() {
    for running in [false, true] {
        let (mut jobs, _) = registry();
        let mut req = request(1);
        req.deadline_ms = 10;
        let first = jobs.submit(0, &req, reservation(), &mut Vec::new()).unwrap();
        if running {
            jobs.tick(0, &mut Vec::new());
        }
        req.deadline_ms = 100;
        let mut effects = Vec::new();
        let second = jobs.submit(11, &req, reservation(), &mut effects).unwrap();
        assert_ne!(second.execution, first.execution);
        assert_eq!(
            jobs.execution(first.execution).unwrap().state(),
            ExecState::Ended(Outcome::Cancelled)
        );
        assert_eq!(
            jobs.terminal(first.execution, first.subscriber).unwrap().termination.outcome,
            Outcome::TimedOut
        );
        assert!(effects.iter().any(|(id, effect)| *id == first.execution
            && matches!(effect, Effect::SubscriberEnded { outcome: Outcome::TimedOut, .. })));
        assert_eq!(jobs.execution(second.execution).unwrap().deadline(), Some(100));
    }
}

#[test]
fn resubmission_still_coalesces_when_an_existing_subscriber_is_live() {
    let (mut jobs, _) = registry();
    let mut req = request(1);
    req.deadline_ms = 10;
    let first = jobs.submit(0, &req, reservation(), &mut Vec::new()).unwrap();
    req.deadline_ms = 100;
    let live = jobs.submit(1, &req, reservation(), &mut Vec::new()).unwrap();
    req.deadline_ms = 200;
    let next = jobs.submit(11, &req, reservation(), &mut Vec::new()).unwrap();
    assert_eq!(first.execution, live.execution);
    assert_eq!(next.execution, live.execution);
    assert_eq!(
        jobs.terminal(first.execution, first.subscriber).unwrap().termination.outcome,
        Outcome::TimedOut
    );
    assert!(jobs.terminal(live.execution, live.subscriber).is_none());
    assert_eq!(jobs.execution(next.execution).unwrap().deadline(), Some(200));
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
        jobs.killed(
            1,
            &builder,
            AssignmentToken { execution: first.execution, attempt: Attempt(1) },
            true,
        );

        let second = jobs.submit(2, &request(2), reservation(), &mut effects).unwrap();
        jobs.tick(2, &mut effects);
        assert_eq!(jobs.execution(second.execution).unwrap().lead(), Some(builder.as_str()));
        // A duplicated or reordered acknowledgement belongs to the first assignment.
        jobs.killed(
            3,
            &builder,
            AssignmentToken { execution: first.execution, attempt: Attempt(1) },
            alive,
        );
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
                complete_empty_work(&mut e, 0);
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
                        Effect::SubscriberEnded { subscriber, outcome, policy, .. } => {
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
            // This assignment-stop test needs a final verdict, not successful compile
            // coverage. A reported build failure is a legitimate terminal verdict.
            let mut request = done(jobs.execution(a).unwrap());
            request.events =
                vec![JobEvent::Done(DoneEvent { outcome: Outcome::Failed, counters: Vec::new() })];
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
            assert_eq!(jobs.admission_view().executor(&slot).unwrap().busy, Some((b, Attempt(1))));
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
            let mut request = done(jobs.execution(b).unwrap());
            request.events =
                vec![JobEvent::Done(DoneEvent { outcome: Outcome::Failed, counters: Vec::new() })];
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
        backend_opt: 0,
        backend_emit: 0,
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
        cwd: SourcePath::new(".".into()).unwrap(),
        command: Command::Build,
        toolchain: StoreIdentity([2; 32]),
        workspace_manifest: SourcePath::new("Cargo.toml".into()).unwrap(),
    }
}
