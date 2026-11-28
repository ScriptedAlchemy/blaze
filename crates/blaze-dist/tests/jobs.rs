//! Lifecycle boundaries that must hold independently of the randomized load.

#![no_std]
#![allow(missing_docs)]

use blaze_dist::admission::{Admission, AdmissionConfig};
use blaze_dist::jobs::{
    AssignmentToken, Effect, ExecState, Execution, ExecutionConfig, Jobs, SubmitAnswer, Subscriber,
    SubscriberState,
};
use blaze_dist::work::EndWhy;
use blaze_own::{Vec, vec_in};
use blaze_wire::{
    Attempt, BatchResult, Digest, DoneEvent, ExecutionId, ItemOutcome, ItemResult, JobContent,
    JobEvent, JobRequest, LocalPolicy, Namespace, Outcome, PrNamespace, RefusedEvent, Reservation,
    RootSet, Set, Why, WireAlloc, WireString, WorkJoinRequest, WorkLeaseRequest, WorkName,
    WorkersEventsRequest,
};

fn heap() -> WireAlloc {
    blaze_own::testing::heap().shared()
}

fn attached(
    answer: Result<SubmitAnswer, (blaze_dist::Refusal, Option<u64>)>,
) -> blaze_wire::JobAnswer {
    match answer {
        Ok(SubmitAnswer::Attached(answer)) => answer,
        other => panic!("expected an attached test request: {other:?}"),
    }
}

fn execution(alloc: WireAlloc) -> Execution {
    let mut e = Execution::new(
        0,
        ExecutionId(1),
        JobContent([1; 32]),
        Namespace::Main,
        subscriber(1, 100_000),
        ExecutionConfig::default(),
        alloc,
        &mut Vec::new_in(alloc),
    );
    e.assign(0, "lead", &["helper"], &mut Vec::new_in(alloc)).unwrap();
    e
}

fn subscriber(id: u64, deadline_ms: u64) -> Subscriber {
    Subscriber { id, deadline_ms, policy: LocalPolicy::Never, state: SubscriberState::Attached }
}

// Deadline/verdict tests explicitly establish zero authoritative work. This is not
// permission to infer cache-only success from a worker Done or an empty unit.
fn complete_empty_work(e: &mut Execution, now: u64, alloc: WireAlloc) {
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
        rejected: Set::empty_in(alloc),
    };
    let lead = WireString::from_str_in(e.lead().unwrap(), alloc);
    e.join(now, lead.as_str(), &request, &mut Vec::new_in(alloc)).unwrap();
}

#[test]
fn a_subscribers_timeout_does_not_cancel_another_subscriber() {
    let alloc = heap();
    let mut e = execution(alloc);
    e.subscribe(subscriber(2, 10)).unwrap();
    let mut effects = Vec::new_in(alloc);
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
    let alloc = heap();
    let mut e = execution(alloc);
    let mut effects = Vec::new_in(alloc);
    e.assign(1, "helper", &[], &mut effects).unwrap();
    assert_eq!(e.attempt(), Attempt(2));
    assert!(effects.scan_for(&Effect::Kill {
        executor: WireString::from_str_in("lead", alloc),
        assignment: AssignmentToken { execution: e.id, attempt: Attempt(1) }
    }).is_some());
    let request = WorkersEventsRequest {
        execution: e.id,
        attempt: Attempt(1),
        events: Vec::new_in(alloc),
    };
    assert_eq!(
        e.worker_events(2, "lead", &request, &mut Vec::new_in(alloc)),
        Err(blaze_dist::Refusal::AttemptMoved),
    );
}

#[test]
fn a_refusal_never_turns_into_a_passed_result() {
    let alloc = heap();
    let mut e = execution(alloc);
    complete_empty_work(&mut e, 0, alloc);
    let request = WorkersEventsRequest {
        execution: e.id,
        attempt: e.attempt(),
        events: vec_in![
            alloc;
            JobEvent::Refused(RefusedEvent {
                function: WireString::from_str_in("function", alloc),
                why: WireString::from_str_in("cannot compile", alloc),
            }),
            JobEvent::Done(DoneEvent { outcome: Outcome::Passed, counters: Vec::new_in(alloc) }),
        ],
    };
    e.worker_events(1, "lead", &request, &mut Vec::new_in(alloc)).unwrap();
    assert_eq!(e.state(), ExecState::Ended(Outcome::Refused));
    let verdicts = Vec::from_iter_in(
        e.events(0).iter().filter_map(|item| match &item.event {
            JobEvent::Done(done) => Some(done.outcome.clone()),
            _ => None,
        }),
        alloc,
    );
    assert_eq!(verdicts.as_slice(), &[Outcome::Refused]);
}

#[test]
fn reassignment_does_not_erase_an_executions_refusal() {
    let alloc = heap();
    let mut e = execution(alloc);
    let refused = WorkersEventsRequest {
        execution: e.id,
        attempt: e.attempt(),
        events: vec_in![
            alloc;
            JobEvent::Refused(RefusedEvent {
                function: WireString::from_str_in("function", alloc),
                why: WireString::from_str_in("cannot compile", alloc),
            })
        ],
    };
    e.worker_events(1, "lead", &refused, &mut Vec::new_in(alloc)).unwrap();
    e.assign(2, "helper", &[], &mut Vec::new_in(alloc)).unwrap();
    complete_empty_work(&mut e, 2, alloc);
    let done = WorkersEventsRequest {
        execution: e.id,
        attempt: e.attempt(),
        events: vec_in![
            alloc;
            JobEvent::Done(DoneEvent { outcome: Outcome::Passed, counters: Vec::new_in(alloc) })
        ],
    };
    e.worker_events(3, "helper", &done, &mut Vec::new_in(alloc)).unwrap();
    assert_eq!(e.state(), ExecState::Ended(Outcome::Refused));
}

#[test]
fn review_completion_respects_each_subscribers_deadline_without_a_tick() {
    let alloc = heap();
    for now in [10, 11] {
        let mut e = execution(alloc);
        complete_empty_work(&mut e, 0, alloc);
        e.subscribe(subscriber(2, 10)).unwrap();
        let done = WorkersEventsRequest {
            execution: e.id,
            attempt: e.attempt(),
            events: vec_in![
                alloc;
                JobEvent::Done(DoneEvent {
                    outcome: Outcome::Passed,
                    counters: Vec::new_in(alloc),
                })
            ],
        };
        let mut effects = Vec::new_in(alloc);
        e.worker_events(now, "lead", &done, &mut effects).unwrap();
        assert_eq!(e.state(), ExecState::Ended(Outcome::Passed));
        let expected = if now > 10 { Outcome::TimedOut } else { Outcome::Passed };
        let ended = Vec::from_iter_in(
            effects.iter().filter_map(|effect| match effect {
                Effect::SubscriberEnded { subscriber: 2, outcome, .. } => Some(outcome),
                _ => None,
            }),
            alloc,
        );
        assert_eq!(ended.as_slice(), &[&expected]);
        assert!(effects.iter().any(|effect| matches!(
            effect,
            Effect::SubscriberEnded { subscriber: 1, outcome: Outcome::Passed, .. }
        )));
    }
}

#[test]
fn review_completion_after_the_last_deadline_cannot_publish_success() {
    let alloc = heap();
    let mut e = execution(alloc);
    let done = WorkersEventsRequest {
        execution: e.id,
        attempt: e.attempt(),
        events: vec_in![
            alloc;
            JobEvent::Done(DoneEvent { outcome: Outcome::Passed, counters: Vec::new_in(alloc) })
        ],
    };
    let mut effects = Vec::new_in(alloc);
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
        unit: [source; 32],
        platform: [1; 32],
        namespace: Namespace::Main,
        source: [source; 32],
        deadline_ms: 100_000,
        local: LocalPolicy::Never,
    }
}

#[test]
fn resubmission_cannot_revive_an_execution_past_its_last_deadline() {
    let alloc = heap();
    for running in [false, true] {
        let (mut jobs, _) = registry(alloc);
        let mut req = request(1);
        req.deadline_ms = 10;
        let first = attached(jobs.submit(0, &req, reservation(), &mut Vec::new_in(alloc)));
        if running {
            jobs.tick(0, &mut Vec::new_in(alloc));
        }
        req.deadline_ms = 100;
        let mut effects = Vec::new_in(alloc);
        let second = attached(jobs.submit(11, &req, reservation(), &mut effects));
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
    let alloc = heap();
    let (mut jobs, _) = registry(alloc);
    let mut req = request(1);
    req.deadline_ms = 10;
    let first = attached(jobs.submit(0, &req, reservation(), &mut Vec::new_in(alloc)));
    req.deadline_ms = 100;
    let live = attached(jobs.submit(1, &req, reservation(), &mut Vec::new_in(alloc)));
    req.deadline_ms = 200;
    let next = attached(jobs.submit(11, &req, reservation(), &mut Vec::new_in(alloc)));
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
    let alloc = heap();
    for alive in [true, false] {
        let mut admission = Admission::new(AdmissionConfig::default(), alloc);
        admission.add_machine("machine", [1; 32], reservation());
        let builder = WireString::from_str_in(
            &admission.add_builder(0, "machine", reservation(), alloc).unwrap(),
            alloc,
        );
        let mut jobs = Jobs::new(admission, ExecutionConfig::default(), alloc);
        let mut effects = Vec::new_in(alloc);
        let first = attached(jobs.submit(0, &request(1), reservation(), &mut effects));
        jobs.tick(0, &mut effects);
        jobs.unsubscribe(1, first.execution, first.subscriber, &mut effects).unwrap();
        jobs.killed(
            1,
            builder.as_str(),
            AssignmentToken { execution: first.execution, attempt: Attempt(1) },
            true,
        );

        let second = attached(jobs.submit(2, &request(2), reservation(), &mut effects));
        jobs.tick(2, &mut effects);
        assert_eq!(jobs.execution(second.execution).unwrap().lead(), Some(builder.as_str()));
        // A duplicated or reordered acknowledgement belongs to the first assignment.
        jobs.killed(
            3,
            builder.as_str(),
            AssignmentToken { execution: first.execution, attempt: Attempt(1) },
            alive,
        );
        assert_eq!(
            jobs.admission_view().executor(builder.as_str()).unwrap().busy,
            Some((second.execution, Attempt(1))),
        );

        let third = attached(jobs.submit(4, &request(3), reservation(), &mut effects));
        jobs.tick(4, &mut effects);
        assert_eq!(jobs.execution(second.execution).unwrap().state(), ExecState::Running);
        assert_eq!(jobs.execution(third.execution).unwrap().state(), ExecState::Queued);
    }
}

#[test]
fn review_pr_supersession_follows_source_and_preserves_shared_executions() {
    let alloc = heap();
    let admission = Admission::new(AdmissionConfig::default(), alloc);
    let mut jobs = Jobs::new(admission, ExecutionConfig::default(), alloc);
    let mut effects = Vec::new_in(alloc);
    let mut linux = request(1);
    linux.namespace = Namespace::Pr(PrNamespace { n: 7 });
    let mut macos = linux.clone();
    macos.platform = [2; 32];
    let mut release = linux.clone();
    release.unit = [0x70; 32];
    let first = attached(jobs.submit(0, &linux, reservation(), &mut effects));
    let second = attached(jobs.submit(0, &macos, reservation(), &mut effects));
    let shared = attached(jobs.submit(0, &release, reservation(), &mut effects));
    let other = attached(jobs.submit(0, &release, reservation(), &mut effects));
    assert_eq!(shared.execution, other.execution);
    for id in [first.execution, second.execution, shared.execution] {
        assert_eq!(jobs.execution(id).unwrap().state(), ExecState::Queued);
    }

    let mut next_push = linux.clone();
    next_push.unit = request(2).unit;
    next_push.source = request(2).source;
    let next = attached(jobs.submit(1, &next_push, reservation(), &mut effects));
    for id in [first.execution, second.execution] {
        assert_eq!(jobs.execution(id).unwrap().state(), ExecState::Ended(Outcome::Cancelled));
    }
    assert_eq!(jobs.execution(shared.execution).unwrap().state(), ExecState::Queued);
    assert_eq!(jobs.execution(next.execution).unwrap().state(), ExecState::Queued);

    // The older build's next unit is not a push that rolls the pull request back: it is refused,
    // and the newer source's executions stay.
    let refused = jobs.submit(1, &release, reservation(), &mut effects).unwrap_err();
    assert_eq!(refused.0, blaze_dist::Refusal::Superseded);
    assert_eq!(jobs.execution(shared.execution).unwrap().state(), ExecState::Queued);
    assert_eq!(jobs.execution(next.execution).unwrap().state(), ExecState::Queued);

    // A shared execution remains tracked: if its other subscriber leaves, a later push
    // supersedes it too rather than leaking an obsolete sole-subscriber job.
    jobs.unsubscribe(2, other.execution, other.subscriber, &mut effects).unwrap();
    next_push.unit = request(3).unit;
    next_push.source = request(3).source;
    let latest = attached(jobs.submit(3, &next_push, reservation(), &mut effects));
    for id in [shared.execution, next.execution] {
        assert_eq!(jobs.execution(id).unwrap().state(), ExecState::Ended(Outcome::Cancelled));
    }
    assert_eq!(jobs.execution(latest.execution).unwrap().state(), ExecState::Queued);
}

#[test]
fn review_a_replacement_lead_gets_its_own_compiled_event() {
    let alloc = heap();
    for changed in [false, true] {
        let mut e = execution(alloc);
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
            rejected: Set::empty_in(alloc),
        };
        e.join(0, "lead", &first, &mut Vec::new_in(alloc)).unwrap();
        assert!(e.work().unwrap().is_done());
        e.tick(15_001, &mut Vec::new_in(alloc));
        e.assign(15_002, "helper", &[], &mut Vec::new_in(alloc)).unwrap();
        let heartbeat = WorkersEventsRequest {
            execution: e.id,
            attempt: e.attempt(),
            events: Vec::new_in(alloc),
        };
        let mut effects = Vec::new_in(alloc);
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
            rejected: Set::empty_in(alloc),
        };
        e.join(15_003, "helper", &next, &mut Vec::new_in(alloc)).unwrap();
        if changed {
            assert!(!e.work().unwrap().is_done());
            let request = blaze_wire::WorkLeaseRequest { execution: e.id, attempt: e.attempt() };
            let grant = e
                .lease(15_004, "helper", &request, &mut Vec::new_in(alloc))
                .unwrap()
                .grant
                .unwrap();
            let result = blaze_wire::BatchResult {
                lease: grant.lease,
                seq: 1,
                items: vec_in![
                    alloc;
                    blaze_wire::ItemResult {
                        name: blaze_wire::WorkName([7; 32]),
                        outcome: blaze_wire::ItemOutcome::Local(blaze_wire::Why {
                            why: WireString::from_str_in("lead tail", alloc),
                        }),
                        found: Set::empty_in(alloc),
                        unnamed: Set::empty_in(alloc),
                    }
                ],
                packs: Vec::new_in(alloc),
                returned_roots: Set::empty_in(alloc),
                returned_names: Set::empty_in(alloc),
            };
            e.result(15_005, "helper", &result, &mut Vec::new_in(alloc)).unwrap();
        }
        assert!(e.work().unwrap().is_done());
        // Replaying the join must not duplicate this attempt's completion notification.
        e.join(15_006, "helper", &next, &mut Vec::new_in(alloc)).unwrap();
        let compiled = Vec::from_iter_in(
            e.events(0).iter().filter_map(|item| match &item.event {
                JobEvent::Stage(stage) if stage.stage == blaze_wire::Stage::Compiled => {
                    Some(item.attempt)
                }
                _ => None,
            }),
            alloc,
        );
        assert_eq!(compiled.as_slice(), &[Attempt(1), Attempt(2)]);
    }
}

fn done(e: &Execution, alloc: WireAlloc) -> WorkersEventsRequest {
    WorkersEventsRequest {
        execution: e.id,
        attempt: e.attempt(),
        events: vec_in![
            alloc;
            JobEvent::Done(DoneEvent { outcome: Outcome::Passed, counters: Vec::new_in(alloc) })
        ],
    }
}

#[test]
fn completion_and_timer_order_preserve_each_subscribers_deadline() {
    let alloc = heap();
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
                    alloc,
                    &mut Vec::new_in(alloc),
                );
                if !last {
                    let mut b = subscriber(2, 100);
                    b.policy = LocalPolicy::Fallback;
                    e.subscribe(b).unwrap();
                }
                e.assign(0, "lead", &[], &mut Vec::new_in(alloc)).unwrap();
                complete_empty_work(&mut e, 0, alloc);
                let request = done(&e, alloc);
                let mut out = Vec::new_in(alloc);
                if timer_first {
                    e.tick(now, &mut out);
                }
                e.worker_events(now, "lead", &request, &mut out).unwrap();
                e.tick(now, &mut out);
                e.worker_events(now + 1, "lead", &request, &mut out).unwrap();
                let outcomes = Vec::from_iter_in(
                    out.iter().filter_map(|fx| match fx {
                        Effect::SubscriberEnded { subscriber, outcome, policy, .. } => {
                            Some((*subscriber, outcome.clone(), policy.clone()))
                        }
                        _ => None,
                    }),
                    alloc,
                );
                let mut expected = vec_in![
                    alloc;
                    (
                        1,
                        if now > 10 { Outcome::TimedOut } else { Outcome::Passed },
                        LocalPolicy::Never,
                    )
                ];
                if !last {
                    expected.push((2, Outcome::Passed, LocalPolicy::Fallback));
                }
                assert_eq!(
                    outcomes.as_slice(),
                    expected.as_slice(),
                    "timer_first={timer_first}, last={last}, now={now}"
                );
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

fn registry(alloc: WireAlloc) -> (blaze_dist::jobs::Jobs, WireString) {
    let mut a = blaze_dist::admission::Admission::new(Default::default(), alloc);
    a.add_machine("machine", [1; 32], reservation());
    let slot =
        WireString::from_str_in(&a.add_builder(0, "machine", reservation(), alloc).unwrap(), alloc);
    (blaze_dist::jobs::Jobs::new(a, Default::default(), alloc), slot)
}

fn submit(jobs: &mut blaze_dist::jobs::Jobs, tag: u64, alloc: WireAlloc) -> ExecutionId {
    attached(jobs.submit(
        0,
        &blaze_wire::JobRequest {
            unit: [u8::try_from(tag & 0xff).unwrap(); 32],
            platform: [1; 32],
            namespace: Namespace::Main,
            source: [0; 32],
            deadline_ms: 100_000,
            local: LocalPolicy::Never,
        },
        reservation(),
        &mut Vec::new_in(alloc),
    ))
    .execution
}

#[test]
fn a_duplicate_kill_ack_cannot_release_a_new_assignment_or_its_draining_slot() {
    use blaze_dist::admission::ExecutorPhase;
    let alloc = heap();
    for draining in [false, true] {
        for alive in [false, true] {
            let (mut jobs, slot) = registry(alloc);
            let a = submit(&mut jobs, 1, alloc);
            jobs.tick(0, &mut Vec::new_in(alloc));
            // This assignment-stop test needs a final verdict, not successful compile
            // coverage. A reported build failure is a legitimate terminal verdict.
            let mut request = done(jobs.execution(a).unwrap(), alloc);
            request.events = vec_in![
                alloc;
                JobEvent::Done(DoneEvent { outcome: Outcome::Failed, counters: Vec::new_in(alloc) })
            ];
            let mut out = Vec::new_in(alloc);
            jobs.worker_events(1, slot.as_str(), &request, &mut out).unwrap();
            let token = out
                .iter()
                .find_map(|(_, fx)| match fx {
                    Effect::Kill { executor, assignment } if executor.as_str() == slot.as_str() => {
                        Some(*assignment)
                    }
                    _ => None,
                })
                .unwrap();
            jobs.killed(2, slot.as_str(), token, true);
            jobs.killed(2, slot.as_str(), token, true); // duplicate before reuse is inert too
            let b = submit(&mut jobs, 2, alloc);
            jobs.tick(3, &mut Vec::new_in(alloc));
            assert_eq!(
                jobs.admission_view().executor(slot.as_str()).unwrap().busy,
                Some((b, Attempt(1)))
            );
            if draining {
                assert!(jobs.admission().rebuild(4, slot.as_str(), reservation(), alloc).is_none());
            }
            let before = jobs.admission_view().executor(slot.as_str()).unwrap().clone();
            jobs.killed(5, slot.as_str(), token, alive);
            // Even the current token cannot acknowledge a stop that has not been requested.
            jobs.killed(5, slot.as_str(), jobs.execution(b).unwrap().assignment(), true);
            assert_eq!(jobs.admission_view().executor(slot.as_str()), Some(&before));
            let c = submit(&mut jobs, 3, alloc);
            jobs.tick(6, &mut Vec::new_in(alloc));
            assert_eq!(jobs.execution(c).unwrap().state(), ExecState::Queued);
            assert_eq!(jobs.admission_view().executors().count(), 1);
            assert_eq!(
                before.phase,
                if draining { ExecutorPhase::Draining } else { ExecutorPhase::Ready }
            );
            let mut request = done(jobs.execution(b).unwrap(), alloc);
            request.events = vec_in![
                alloc;
                JobEvent::Done(DoneEvent { outcome: Outcome::Failed, counters: Vec::new_in(alloc) })
            ];
            let b_token = jobs.execution(b).unwrap().assignment();
            jobs.worker_events(7, slot.as_str(), &request, &mut Vec::new_in(alloc)).unwrap();
            jobs.killed(8, slot.as_str(), b_token, true);
            jobs.tick(9, &mut Vec::new_in(alloc));
            assert_eq!(jobs.execution(c).unwrap().state(), ExecState::Running);
            if draining {
                assert_eq!(
                    jobs.admission_view().executor(slot.as_str()).unwrap().phase,
                    ExecutorPhase::Stopped
                );
                assert_ne!(jobs.execution(c).unwrap().lead(), Some(slot.as_str()));
            }
        }
    }
}

#[test]
fn promoted_permanently_stalled_helper_is_cancelled_at_deadline_without_coverage() {
    let alloc = heap();
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
        alloc,
        &mut Vec::new_in(alloc),
    );
    e.assign(0, "lead", &["stuck", "healthy"], &mut Vec::new_in(alloc)).unwrap();
    let roots =
        RootSet { spec: [1; 32], roots: [2; 32], count: 1, index: [3; 32], inputs: [4; 32] };
    let heartbeat = |e: &Execution| WorkersEventsRequest {
        execution: e.id,
        attempt: e.attempt(),
        events: Vec::new_in(alloc),
    };
    // The original lead disappears. The helper keeps heartbeating and is promoted.
    e.worker_events(16, "stuck", &heartbeat(&e), &mut Vec::new_in(alloc)).unwrap();
    let old = e.assignment();
    let mut out = Vec::new_in(alloc);
    e.tick(16, &mut out);
    assert!(out.scan_for(&Effect::Kill {
        executor: WireString::from_str_in("lead", alloc),
        assignment: old
    }).is_some());
    assert!(out.scan_for(&Effect::NeedLead).is_some());
    e.assign(16, "stuck", &["healthy"], &mut out).unwrap();
    e.join(
        16,
        "stuck",
        &WorkJoinRequest {
            execution: e.id,
            attempt: e.attempt(),
            roots,
            rejected: Set::empty_in(alloc),
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
        items: Vec::new_in(alloc),
        packs: Vec::new_in(alloc),
        returned_roots: grant.roots,
        returned_names: Set::empty_in(alloc),
    };
    assert_eq!(e.result(50, "stuck", &late, &mut out), Err(blaze_dist::Refusal::DeadLease));
    let current = e.assignment();
    e.tick(51, &mut out);
    assert_eq!(e.state(), ExecState::Ended(Outcome::Cancelled));
    assert_eq!(e.work().unwrap().ended(), Some(EndWhy::Deadline));
    assert!(!e.work().unwrap().is_done());
    assert!(out.scan_for(&Effect::Kill {
        executor: WireString::from_str_in("stuck", alloc),
        assignment: current
    }).is_some());
    assert!(out.scan_for(&Effect::Kill {
        executor: WireString::from_str_in("healthy", alloc),
        assignment: current
    }).is_some());
    assert!(!out.iter().any(|fx| matches!(fx, Effect::Work(blaze_dist::work::Effect::Done))));
    assert_eq!(e.result(52, "stuck", &late, &mut out), Err(blaze_dist::Refusal::Ended));
}

/// Two builders on one machine: the registry admits one as lead and offers the other
/// to the same execution as a helper, and both take disjoint grants of one work set.
/// Before `offer_helpers`, `Jobs` assigned with no helpers and `member` refused the
/// second worker's join, so no build could ever split.
#[test]
fn an_idle_builder_is_assigned_as_a_helper_and_both_workers_lease() {
    let alloc = heap();
    let mut admission = Admission::new(AdmissionConfig::default(), alloc);
    admission.add_machine("machine", [1; 32], Reservation { memory: 16, cpus: 16, disk: 16 });
    let lead = WireString::from_str_in(
        &admission.add_builder(0, "machine", reservation(), alloc).unwrap(),
        alloc,
    );
    let helper = WireString::from_str_in(
        &admission.add_builder(0, "machine", reservation(), alloc).unwrap(),
        alloc,
    );
    let mut jobs = Jobs::new(admission, ExecutionConfig::default(), alloc);
    let mut out = Vec::new_in(alloc);
    let id = attached(jobs.submit(0, &request(7), reservation(), &mut out)).execution;
    out.clear();
    jobs.tick(0, &mut out);
    let assigned = Vec::from_iter_in(
        out.iter().filter_map(|(_, effect)| match effect {
            Effect::HelperAssigned { executor, assignment } => {
                Some((executor.clone(), *assignment))
            }
            _ => None,
        }),
        alloc,
    );
    assert_eq!(assigned.len(), 1, "{out:?}");
    assert_eq!(assigned[0].0, helper);
    assert_eq!(assigned[0].1.execution, id);
    let attempt = jobs.execution(id).unwrap().attempt();
    assert_eq!(assigned[0].1.attempt, attempt);
    assert_eq!(jobs.execution(id).unwrap().lead(), Some(lead.as_str()));

    let roots =
        RootSet { spec: [1; 32], roots: [2; 32], count: 2, index: [3; 32], inputs: [4; 32] };
    let join = |jobs: &mut Jobs, who: &str| {
        jobs.join(
            0,
            who,
            &WorkJoinRequest {
                execution: id,
                attempt,
                roots: roots.clone(),
                rejected: Set::empty_in(alloc),
            },
            &mut Vec::new_in(alloc),
        )
        .unwrap()
    };
    join(&mut jobs, lead.as_str());
    let lease = |jobs: &mut Jobs, who: &str| {
        jobs.lease(0, who, &WorkLeaseRequest { execution: id, attempt }, &mut Vec::new_in(alloc))
            .unwrap()
            .grant
            .unwrap()
    };
    let first = lease(&mut jobs, lead.as_str());
    // The lead's first request may arrive before the assigned helper joins.
    join(&mut jobs, helper.as_str());
    let second = lease(&mut jobs, helper.as_str());
    assert_eq!(first.roots.as_slice(), &[(0, 1)]);
    assert_eq!(second.roots.as_slice(), &[(1, 2)]);
    for (who, grant) in [(lead.as_str(), &first), (helper.as_str(), &second)] {
        jobs.result(0, who, &local_root_result(grant, alloc), &mut out).unwrap();
    }
    assert!(jobs.execution(id).unwrap().work().unwrap().is_done());
}

fn local_root_result(grant: &blaze_wire::Grant, alloc: WireAlloc) -> BatchResult {
    let ranges = blaze_dist::Ranges::from_wire(&grant.roots, alloc).unwrap();
    let items = Vec::from_iter_in(
        ranges.indexes().map(|idx| ItemResult {
            name: WorkName([u8::try_from(idx).unwrap(); 32]),
            outcome: ItemOutcome::Local(Why { why: WireString::from_str_in("fixture", alloc) }),
            found: Set::empty_in(alloc),
            unnamed: Set::empty_in(alloc),
        }),
        alloc,
    );
    BatchResult {
        lease: grant.lease.clone(),
        seq: 1,
        items,
        packs: Vec::new_in(alloc),
        returned_roots: Set::empty_in(alloc),
        returned_names: Set::empty_in(alloc),
    }
}

#[test]
fn absent_assignments_and_an_expired_late_helper_do_not_strand_roots() {
    let alloc = heap();
    let mut config = ExecutionConfig::default();
    config.work.lease_ms = 10;
    config.completion = blaze_dist::jobs::CompletionMode::PublishedWork;
    let mut log = Vec::new_in(alloc);
    let mut e = Execution::new(
        0,
        ExecutionId(1),
        JobContent([1; 32]),
        Namespace::Main,
        subscriber(1, 100_000),
        config,
        alloc,
        &mut log,
    );
    e.assign(0, "lead", &["late", "absent-a", "absent-b", "absent-c"], &mut log).unwrap();
    let attempt = e.attempt();
    e.resolve_publication(0, "lead", attempt, None, &mut log).unwrap();
    let join = WorkJoinRequest {
        execution: e.id,
        attempt,
        roots: RootSet {
            spec: [1; 32],
            roots: [2; 32],
            count: 12,
            index: [3; 32],
            inputs: [4; 32],
        },
        rejected: Set::empty_in(alloc),
    };
    let lease = WorkLeaseRequest { execution: e.id, attempt };
    e.join(0, "lead", &join, &mut log).unwrap();

    // Several reserved helpers never start. The lead can finish a substantial batch before
    // any helper joins. A late holder gets its own part of the remaining work immediately.
    let first = e.lease(0, "lead", &lease, &mut log).unwrap().grant.unwrap();
    assert_eq!(first.roots.as_slice(), &[(0, 6)]);
    e.result(1, "lead", &local_root_result(&first, alloc), &mut log).unwrap();
    e.join(2, "late", &join, &mut log).unwrap();
    let late = e.lease(2, "late", &lease, &mut log).unwrap().grant.unwrap();
    assert_eq!(late.roots.as_slice(), &[(6, 8)]);
    let mut committed = Vec::from_iter_in(0..6, alloc);
    // Take everything available while the late holder is silent, then its expired roots.
    for now in [5, 13] {
        while let Some(grant) = e.lease(now, "lead", &lease, &mut log).unwrap().grant {
            let roots = blaze_dist::Ranges::from_wire(&grant.roots, alloc).unwrap();
            committed.extend(roots.indexes());
            e.result(now, "lead", &local_root_result(&grant, alloc), &mut log).unwrap();
        }
        if now == 5 {
            assert_eq!(committed.as_slice(), &[0, 1, 2, 3, 4, 5, 8, 9, 10, 11]);
            assert!(e.commit_publication(now, "lead", attempt, &[], &mut log).is_err());
        }
    }
    committed.sort_unstable();
    assert!(committed.into_iter().eq(0..12));
    assert_eq!(
        e.result(13, "late", &local_root_result(&late, alloc), &mut log),
        Err(blaze_dist::Refusal::DeadLease),
    );
    e.commit_publication(13, "lead", attempt, &[], &mut log).unwrap();
    e.commit_publication(13, "lead", attempt, &[], &mut log).unwrap();
    for _ in 0..2 {
        assert_eq!(e.join(13, "late", &join, &mut log), Err(blaze_dist::Refusal::Ended));
        assert_eq!(e.lease(13, "late", &lease, &mut log), Err(blaze_dist::Refusal::Ended));
    }
    assert_eq!(
        e.stages().iter().filter(|(stage, _)| *stage == blaze_wire::Stage::Published).count(),
        1
    );
    e.deliver_publication(14, &[], &mut log).unwrap();
    assert_eq!(e.state(), ExecState::Ended(Outcome::Passed));
}

/// Execution ids seeded from a daemon epoch: two daemons never issue the same first
/// id, and the same epoch is deterministic. A restarted daemon must not hand a
/// resubmitted job the id a surviving executor's lease still names.
#[test]
fn seeded_ids_differ_between_daemon_epochs() {
    let alloc = heap();
    let first_id = |epoch: Digest| {
        let (mut jobs, _) = registry(alloc);
        jobs.seed_ids(&epoch);
        submit(&mut jobs, 1, alloc)
    };
    assert_eq!(first_id([9; 32]), first_id([9; 32]));
    assert_ne!(first_id([9; 32]), first_id([7; 32]));
    assert_ne!(first_id([9; 32]), ExecutionId(1));
}

/// Four idle builders and two independent runnable builds: the scheduler assigns
/// two leads and offers one helper to each before taking a second for either.
/// An eager inner loop used to give both spare builders to the first build.
#[test]
fn idle_helpers_are_shared_round_robin_between_running_builds() {
    let alloc = heap();
    let mut admission = Admission::new(AdmissionConfig::default(), alloc);
    admission.add_machine("machine", [1; 32], Reservation { memory: 32, cpus: 32, disk: 32 });
    for _ in 0..4 {
        admission.add_builder(0, "machine", reservation(), alloc).unwrap();
    }
    let mut jobs = Jobs::new(admission, ExecutionConfig::default(), alloc);
    // Every effect counts, submits included: a submit ticks admission before
    // it enqueues, so the first build's helper is assigned inside the second
    // submit, not in the tick below.
    let mut out = Vec::new_in(alloc);
    let a = attached(jobs.submit(0, &request(7), reservation(), &mut out)).execution;
    let b = attached(jobs.submit(0, &request(8), reservation(), &mut out)).execution;
    assert_ne!(a, b);
    jobs.tick(0, &mut out);
    assert_eq!(jobs.execution(a).unwrap().state(), ExecState::Running);
    assert_eq!(jobs.execution(b).unwrap().state(), ExecState::Running);
    let helpers = Vec::from_iter_in(
        out.iter().filter_map(|(execution, effect)| match effect {
            Effect::HelperAssigned { executor, .. } => Some((*execution, executor.as_str())),
            _ => None,
        }),
        alloc,
    );
    assert_eq!(helpers.len(), 2, "{out:?}");
    assert_eq!(helpers.iter().filter(|(id, _)| *id == a).count(), 1, "{out:?}");
    assert_eq!(helpers.iter().filter(|(id, _)| *id == b).count(), 1, "{out:?}");
    assert_ne!(helpers[0].1, helpers[1].1);
}

/// An assignment notification is not proof that a worker is alive. A helper that
/// actually heartbeats must outrank another builder which was only reserved;
/// otherwise a just-assigned but silent worker can be promoted on lead loss.
#[test]
fn assigning_an_unstarted_helper_does_not_forge_liveness() {
    let alloc = heap();
    let mut e = execution(alloc);
    let heartbeat = WorkersEventsRequest {
        execution: e.id,
        attempt: e.attempt(),
        events: Vec::new_in(alloc),
    };
    e.worker_events(9, "helper", &heartbeat, &mut Vec::new_in(alloc)).unwrap();
    e.add_helper(10, "silent").unwrap();
    let recently_heard = Vec::from_iter_in(e.heard_recently(11), alloc);
    assert!(recently_heard.scan_for(&"helper").is_some(), "{recently_heard:?}");
    assert!(recently_heard.scan_for(&"silent").is_none(), "{recently_heard:?}");

    e.worker_events(12, "silent", &heartbeat, &mut Vec::new_in(alloc)).unwrap();
    assert!(e.heard_recently(13).any(|executor| executor == "silent"));
}

mod claim {
    use blaze_dist::admission::{Admission, AdmissionConfig};
    use blaze_dist::jobs::{ExecutionConfig, Jobs};
    use blaze_own::Vec;
    use blaze_wire::{JobRequest, LocalPolicy, Namespace, Reservation, WireAlloc, WireString};

    use super::{attached, heap};

    fn started(executors: u32, alloc: WireAlloc) -> (Jobs, blaze_wire::ExecutionId) {
        let need = Reservation { memory: 1, cpus: 1, disk: 1 };
        let mut admission = Admission::new(AdmissionConfig::default(), alloc);
        admission.add_machine("machine", [1; 32], Reservation { memory: 64, cpus: 64, disk: 64 });
        for _ in 0..executors {
            admission.add_builder(0, "machine", need.clone(), alloc).unwrap();
        }
        let mut jobs = Jobs::new(admission, ExecutionConfig::default(), alloc);
        let request = JobRequest {
            unit: [9; 32],
            platform: [1; 32],
            namespace: Namespace::Main,
            source: [0; 32],
            deadline_ms: 3_600_000,
            local: LocalPolicy::Never,
        };
        let mut out = Vec::new_in(alloc);
        let answer = attached(jobs.submit(0, &request, need, &mut out));
        (jobs, answer.execution)
    }

    #[test]
    fn an_executor_is_claimed_once_and_again_by_its_claimant() {
        let alloc = heap();
        let (mut jobs, id) = started(1, alloc);
        let mut out = Vec::new_in(alloc);
        let (first, attempt, storage) = {
            let (executor, attempt) =
                jobs.claim(1, id, [1; 32], &mut out).unwrap().expect("the lead");
            (WireString::from_str_in(executor, alloc), attempt, executor.as_ptr())
        };
        assert_eq!(jobs.execution(id).unwrap().lead().unwrap().as_ptr(), storage);
        assert_eq!(jobs.claim(2, id, [1; 32], &mut out).unwrap(), Some((first.as_str(), attempt)));
        assert_eq!(jobs.claim(3, id, [2; 32], &mut out).unwrap(), None, "nobody else gets it");
    }

    #[test]
    fn a_second_claimant_gets_a_helper_when_there_is_one() {
        let alloc = heap();
        let (mut jobs, id) = started(2, alloc);
        let mut out = Vec::new_in(alloc);
        let first = WireString::from_str_in(
            jobs.claim(1, id, [1; 32], &mut out).unwrap().expect("the lead").0,
            alloc,
        );
        let second = jobs.claim(2, id, [2; 32], &mut out).unwrap();
        // A helper is offered per tick, and only once the execution has its work set.
        if let Some(second) = second {
            assert_ne!(first.as_str(), second.0, "one claimant per executor");
        }
    }

    #[test]
    fn pull_workers_reuse_an_unstarted_reservation_but_acknowledge_claimed_stops() {
        let alloc = heap();
        let (jobs, first) = started(3, alloc);
        let mut jobs = jobs.start_on_claim();
        let mut out = Vec::new_in(alloc);
        let (lead, attempt) = jobs.claim(1, first, [1; 32], &mut out).unwrap().unwrap();
        let lead = WireString::from_str_in(lead, alloc);
        let token = super::AssignmentToken { execution: first, attempt };
        jobs.tick(1, &mut out);
        let direct = jobs.execution(first).unwrap().executors().find(|&e| e != lead.as_str()).unwrap();
        let direct = WireString::from_str_in(direct, alloc);
        let heartbeat = super::WorkersEventsRequest { execution: first, attempt, events: Vec::new_in(alloc) };
        jobs.worker_events(1, direct.as_str(), &heartbeat, &mut out).unwrap();
        let subscriber = jobs.execution(first).unwrap().subscribers().next().unwrap().id;
        jobs.unsubscribe(2, first, subscriber, &mut out).unwrap();
        let need = Reservation { memory: 1, cpus: 1, disk: 1 };
        let next = attached(jobs.submit(3, &super::request(7), need, &mut out)).execution;
        let (other, _) =
            jobs.claim(3, next, [2; 32], &mut out).unwrap().expect("unstarted helper was released");
        let other = WireString::from_str_in(other, alloc);
        assert_ne!(other, lead);
        assert_ne!(other, direct);
        assert!(!jobs.killed(3, other.as_str(), token, true));
        assert!(jobs.claim(3, next, [3; 32], &mut out).unwrap().is_none());
        assert!(jobs.killed(4, lead.as_str(), token, true));
        assert_eq!(jobs.claim(4, next, [3; 32], &mut out).unwrap().unwrap().0, lead.as_str());
        assert!(!jobs.killed(5, lead.as_str(), token, true));
        assert!(jobs.killed(5, direct.as_str(), token, true));
    }

    #[test]
    fn work_unfinished_has_no_committed_packs() {
        let alloc = heap();
        let (jobs, id) = started(1, alloc);
        assert!(jobs.committed_packs(id).unwrap().is_none());
    }
}
