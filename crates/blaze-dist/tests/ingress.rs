//! Worker ingress and typed terminal policy, checked against the public stream.

#![no_std]
#![allow(missing_docs)]

use blaze_dist::Refusal;
use blaze_dist::jobs::{
    CompletionMode, Effect, ExecState, Execution, ExecutionConfig, LocalAction, Subscriber,
    SubscriberState, local_action,
};
use blaze_own::{Vec, vec_in};
use blaze_wire::job_terminal_v2::{TerminationReasonV2 as Reason, TerminationV2, decode_terminal};
use blaze_wire::{
    AssignedEvent, Attempt, BatchResult, DoneEvent, ExecutionId, JobContent, JobEvent, Limits,
    LocalPolicy, Namespace, Outcome, OutputEvent, RefusedEvent, RootSet, Set, Stage, StageEvent,
    WireAlloc, WireString, WireVec, WorkJoinRequest, WorkLeaseRequest, WorkersEventsRequest,
    encode,
};

fn heap() -> WireAlloc {
    blaze_own::testing::heap().shared()
}

fn execution(mode: CompletionMode, alloc: WireAlloc) -> Execution {
    let mut e = Execution::new(
        0,
        ExecutionId(1),
        JobContent([1; 32]),
        Namespace::Main,
        Subscriber {
            id: 1,
            deadline_ms: 100_000,
            policy: LocalPolicy::Fallback,
            state: SubscriberState::Attached,
        },
        ExecutionConfig { completion: mode, ..ExecutionConfig::default() },
        alloc,
        &mut Vec::new_in(alloc),
    );
    e.assign(0, "lead", &["helper"], &mut Vec::new_in(alloc)).unwrap();
    e
}

fn request(e: &Execution, events: WireVec<JobEvent>) -> WorkersEventsRequest {
    WorkersEventsRequest { execution: e.id, attempt: e.attempt(), events }
}

fn done(outcome: Outcome, alloc: WireAlloc) -> JobEvent {
    JobEvent::Done(DoneEvent { outcome, counters: Vec::new_in(alloc) })
}

fn stage(stage: Stage) -> JobEvent {
    JobEvent::Stage(StageEvent { stage, at_ms: 0 })
}

fn join(e: &mut Execution, holder: &str, count: u64, tag: u8, alloc: WireAlloc) {
    join_at(e, holder, count, tag, 0, alloc);
}

fn join_at(e: &mut Execution, holder: &str, count: u64, tag: u8, now: u64, alloc: WireAlloc) {
    let r = WorkJoinRequest {
        execution: e.id,
        attempt: e.attempt(),
        roots: RootSet { spec: [1; 32], roots: [tag; 32], count, index: [3; 32], inputs: [4; 32] },
        rejected: Set::empty_in(alloc),
    };
    e.join(now, holder, &r, &mut Vec::new_in(alloc)).unwrap();
}

fn rejects_without_public_effects(
    e: &mut Execution,
    from: &str,
    r: &WorkersEventsRequest,
    alloc: WireAlloc,
) {
    let before = Vec::from_slice_in(e.events(0), alloc);
    let stages = Vec::from_slice_in(e.stages(), alloc);
    let state = e.state();
    let mut effects = Vec::new_in(alloc);
    assert!(e.worker_events(1, from, r, &mut effects).is_err());
    assert!(effects.is_empty(), "rejected batch emitted {effects:?}");
    assert_eq!(e.events(0), before.as_slice(), "rejected batch changed public stream");
    assert_eq!(e.stages(), stages.as_slice());
    assert_eq!(e.state(), state);
}

#[test]
fn coordinator_stages_and_assignments_never_enter_from_workers() {
    let alloc = heap();
    for from in ["lead", "helper"] {
        for event in [
            stage(Stage::Compiled),
            stage(Stage::Done),
            stage(Stage::Queued),
            stage(Stage::Assigned),
            stage(Stage::Admitted),
            JobEvent::Assigned(AssignedEvent {
                executor: WireString::from_str_in("intruder", alloc),
                attempt: Attempt(2),
            }),
        ] {
            let mut e = execution(CompletionMode::WorkProtocol, alloc);
            let r = request(&e, vec_in![alloc; event.clone(), event]);
            rejects_without_public_effects(&mut e, from, &r, alloc);
        }
    }
    let mut e = execution(CompletionMode::WorkProtocol, alloc);
    let r = request(&e, vec_in![alloc; stage(Stage::Loaded)]);
    rejects_without_public_effects(&mut e, "helper", &r, alloc);
}

#[test]
fn helper_roles_and_mixed_batches_are_prevalidated_atomically() {
    let alloc = heap();
    for event in [done(Outcome::Failed, alloc), stage(Stage::Compiled)] {
        let mut e = execution(CompletionMode::NoCompileWork, alloc);
        let r = request(
            &e,
            vec_in![
                alloc;
                JobEvent::Output(OutputEvent { stream: 1, bytes: vec_in![alloc; 1] }),
                JobEvent::Refused(RefusedEvent {
                    function: WireString::from_str_in("f", alloc),
                    why: WireString::from_str_in("bad", alloc),
                }),
                event,
            ],
        );
        rejects_without_public_effects(&mut e, "helper", &r, alloc);
        // Even the valid Refused prefix was not applied.
        let passed = request(&e, vec_in![alloc; done(Outcome::Passed, alloc)]);
        e.worker_events(2, "lead", &passed, &mut Vec::new_in(alloc)).unwrap();
        assert_eq!(e.state(), ExecState::Ended(Outcome::Passed));
    }
}

#[test]
fn stale_and_duplicate_worker_batches_cannot_forge_stages_or_multiple_verdicts() {
    let alloc = heap();
    let mut e = execution(CompletionMode::NoCompileWork, alloc);
    let old = request(&e, vec_in![alloc; stage(Stage::Compiled)]);
    e.assign(1, "helper", &[], &mut Vec::new_in(alloc)).unwrap();
    rejects_without_public_effects(&mut e, "lead", &old, alloc);
    let duplicate = request(
        &e,
        vec_in![alloc; done(Outcome::Passed, alloc), done(Outcome::Passed, alloc)],
    );
    rejects_without_public_effects(&mut e, "helper", &duplicate, alloc);
    let passed = request(&e, vec_in![alloc; done(Outcome::Passed, alloc)]);
    e.worker_events(2, "helper", &passed, &mut Vec::new_in(alloc)).unwrap();
    let before = Vec::from_slice_in(e.events(0), alloc);
    e.worker_events(3, "helper", &passed, &mut Vec::new_in(alloc)).unwrap();
    assert_eq!(e.events(0), before.as_slice());
    assert_eq!(before.iter().filter(|i| matches!(i.event, JobEvent::Done(_))).count(), 1);
}

#[test]
fn invalid_ingress_is_atomic_even_after_a_subscribers_deadline() {
    let alloc = heap();
    let mut e = execution(CompletionMode::NoCompileWork, alloc);
    let r = request(&e, vec_in![alloc; stage(Stage::Done)]);
    let before = Vec::from_slice_in(e.events(0), alloc);
    let mut effects = Vec::new_in(alloc);
    assert!(e.worker_events(100_001, "lead", &r, &mut effects).is_err());
    assert!(effects.is_empty());
    assert_eq!(e.events(0), before.as_slice());
    assert!(e.terminal(1).is_none());
    // The actual timer still expires it normally; the invalid request did not deliver.
    e.tick(100_001, &mut effects);
    assert_eq!(e.terminal(1).unwrap().termination.reason, Reason::SubscriberDeadline);
}

#[test]
fn worker_cannot_classify_infrastructure_failure_as_a_verdict() {
    let alloc = heap();
    for outcome in [Outcome::Lost, Outcome::Cancelled, Outcome::TimedOut] {
        let mut e = execution(CompletionMode::NoCompileWork, alloc);
        let r = request(&e, vec_in![alloc; done(outcome, alloc)]);
        rejects_without_public_effects(&mut e, "lead", &r, alloc);
    }
}

#[test]
fn success_requires_authoritative_completion_not_provisional_or_returned_work() {
    let alloc = heap();
    let mut e = execution(CompletionMode::WorkProtocol, alloc);
    let passed = request(&e, vec_in![alloc; done(Outcome::Passed, alloc)]);
    rejects_without_public_effects(&mut e, "lead", &passed, alloc);
    join(&mut e, "helper", 0, 2, alloc);
    let passed = request(&e, vec_in![alloc; done(Outcome::Passed, alloc)]);
    rejects_without_public_effects(&mut e, "lead", &passed, alloc);
    // A differing authoritative list reopens work; returned roots are not coverage.
    join(&mut e, "lead", 1, 3, alloc);
    let grant = e
        .lease(
            0,
            "lead",
            &WorkLeaseRequest { execution: e.id, attempt: e.attempt() },
            &mut Vec::new_in(alloc),
        )
        .unwrap()
        .grant
        .unwrap();
    e.result(
        0,
        "lead",
        &BatchResult {
            lease: grant.lease,
            seq: 1,
            items: Vec::new_in(alloc),
            packs: Vec::new_in(alloc),
            returned_roots: grant.roots,
            returned_names: grant.names,
        },
        &mut Vec::new_in(alloc),
    )
    .unwrap();
    let passed = request(&e, vec_in![alloc; done(Outcome::Passed, alloc)]);
    rejects_without_public_effects(&mut e, "lead", &passed, alloc);
}

#[test]
fn explicit_no_work_and_authoritative_zero_roots_can_succeed() {
    let alloc = heap();
    for mode in [CompletionMode::WorkProtocol, CompletionMode::NoCompileWork] {
        let mut e = execution(mode, alloc);
        if mode == CompletionMode::WorkProtocol {
            join(&mut e, "lead", 0, 2, alloc);
        }
        let passed = request(&e, vec_in![alloc; done(Outcome::Passed, alloc)]);
        e.worker_events(1, "lead", &passed, &mut Vec::new_in(alloc)).unwrap();
        assert_eq!(e.state(), ExecState::Ended(Outcome::Passed));
    }
}

#[test]
fn retained_completion_requires_the_replacement_leads_agreement() {
    let alloc = heap();
    let mut e = execution(CompletionMode::WorkProtocol, alloc);
    join(&mut e, "lead", 0, 2, alloc);
    e.assign(1, "helper", &[], &mut Vec::new_in(alloc)).unwrap();
    let passed = request(&e, vec_in![alloc; done(Outcome::Passed, alloc)]);
    rejects_without_public_effects(&mut e, "helper", &passed, alloc);
    join(&mut e, "helper", 0, 2, alloc);
    let passed = request(&e, vec_in![alloc; done(Outcome::Passed, alloc)]);
    e.worker_events(2, "helper", &passed, &mut Vec::new_in(alloc)).unwrap();
    assert_eq!(e.state(), ExecState::Ended(Outcome::Passed));
}

#[test]
fn terminal_delivery_retains_reason_through_wire_and_client_policy() {
    let alloc = heap();
    for (reason, outcome) in [
        (Reason::Reported, Outcome::Passed),
        (Reason::Reported, Outcome::Failed),
        (Reason::Reported, Outcome::Refused),
        (Reason::NoLead, Outcome::Lost),
        (Reason::LeadLostTwice, Outcome::Lost),
        (Reason::Cancelled, Outcome::Cancelled),
        (Reason::Deadline, Outcome::Cancelled),
        (Reason::SubscriberDeadline, Outcome::TimedOut),
    ] {
        for policy in [LocalPolicy::Never, LocalPolicy::Fallback] {
            let value =
                TerminationV2 { version: 2, reason: reason.clone(), outcome: outcome.clone() };
            let expected = if reason == Reason::Reported {
                LocalAction::Take
            } else if policy == LocalPolicy::Never {
                LocalAction::Fail
            } else {
                LocalAction::BuildLocally
            };
            assert_eq!(local_action(&policy, &value, alloc).unwrap(), expected);
        }
    }
    let mut e = execution(CompletionMode::NoCompileWork, alloc);
    let failed = request(&e, vec_in![alloc; done(Outcome::Failed, alloc)]);
    let mut effects = Vec::new_in(alloc);
    e.worker_events(1, "lead", &failed, &mut effects).unwrap();
    let delivered = effects
        .iter()
        .find_map(|fx| match fx {
            Effect::SubscriberEnded { terminal, .. } => Some(terminal),
            _ => None,
        })
        .unwrap();
    let decoded =
        decode_terminal(&encode(delivered, alloc), &Limits::DOCUMENTED, alloc).unwrap();
    assert_eq!(e.terminal(1), Some(&decoded));
    assert_eq!(decoded.termination.reason, Reason::Reported);
    assert_eq!(
        local_action(&LocalPolicy::Fallback, &decoded.termination, alloc).unwrap(),
        LocalAction::Take
    );
    let invalid = TerminationV2 { version: 1, reason: Reason::NoLead, outcome: Outcome::Failed };
    assert!(matches!(
        local_action(&LocalPolicy::Fallback, &invalid, alloc),
        Err(Refusal::Malformed(_))
    ));
}

#[test]
fn actual_lead_loss_cancel_and_subscriber_timeout_deliver_distinct_reasons() {
    let alloc = heap();
    let mut e = execution(CompletionMode::NoCompileWork, alloc);
    e.tick(15_001, &mut Vec::new_in(alloc));
    e.assign(15_002, "helper", &[], &mut Vec::new_in(alloc)).unwrap();
    e.tick(30_003, &mut Vec::new_in(alloc));
    assert_eq!(e.terminal(1).unwrap().termination.reason, Reason::LeadLostTwice);
    let mut e = execution(CompletionMode::WorkProtocol, alloc);
    e.cancel(1, &mut Vec::new_in(alloc));
    assert_eq!(e.terminal(1).unwrap().termination.reason, Reason::Cancelled);
    let mut e = execution(CompletionMode::WorkProtocol, alloc);
    e.tick(100_001, &mut Vec::new_in(alloc));
    assert_eq!(e.terminal(1).unwrap().termination.reason, Reason::SubscriberDeadline);
}

#[test]
fn no_lead_lost_delivery_is_not_a_completed_failed_verdict() {
    let alloc = heap();
    let mut admission =
        blaze_dist::admission::Admission::new(blaze_dist::admission::AdmissionConfig::default(), alloc);
    let reservation = blaze_wire::Reservation { memory: 4, cpus: 4, disk: 4 };
    admission.add_machine("machine", [1; 32], reservation.clone());
    admission.add_builder(0, "machine", reservation.clone(), alloc).unwrap();
    let mut jobs = blaze_dist::jobs::Jobs::new(admission, ExecutionConfig::default(), alloc);
    let answer = jobs
        .submit(
            0,
            &blaze_wire::JobRequest {
                unit: [9; 32],
                platform: [1; 32],
                namespace: Namespace::Main,
                source: [0; 32],
                deadline_ms: 100_000,
                local: LocalPolicy::Fallback,
            },
            reservation,
            &mut Vec::new_in(alloc),
        )
        .unwrap();
    jobs.tick(0, &mut Vec::new_in(alloc));
    let mut effects = Vec::new_in(alloc);
    jobs.tick(15_001, &mut effects);
    let terminal = jobs.terminal(answer.execution, answer.subscriber).unwrap();
    assert_eq!(terminal.termination.reason, Reason::NoLead);
    assert_eq!(terminal.termination.outcome, Outcome::Lost);
    let decoded =
        decode_terminal(&encode(terminal, alloc), &Limits::DOCUMENTED, alloc).unwrap();
    assert_eq!(
        local_action(&LocalPolicy::Fallback, &decoded.termination, alloc).unwrap(),
        LocalAction::BuildLocally
    );
}

#[test]
fn no_compile_mode_rejects_work_and_failed_ingress_does_not_renew_lead_liveness() {
    let alloc = heap();
    let mut e = execution(CompletionMode::NoCompileWork, alloc);
    let r = WorkLeaseRequest { execution: e.id, attempt: e.attempt() };
    assert!(e.lease(0, "lead", &r, &mut Vec::new_in(alloc)).is_err());
    let bad = request(&e, vec_in![alloc; stage(Stage::Compiled)]);
    assert!(e.worker_events(14_999, "lead", &bad, &mut Vec::new_in(alloc)).is_err());
    let mut effects = Vec::new_in(alloc);
    e.tick(15_001, &mut effects);
    assert!(effects.scan_for(&Effect::NeedLead).is_some());
}

#[test]
fn changed_root_epoch_blocks_success_until_its_work_is_done() {
    let alloc = heap();
    let mut e = execution(CompletionMode::WorkProtocol, alloc);
    join(&mut e, "lead", 0, 2, alloc);
    e.assign(1, "helper", &[], &mut Vec::new_in(alloc)).unwrap();
    join(&mut e, "helper", 1, 3, alloc);
    let passed = request(&e, vec_in![alloc; done(Outcome::Passed, alloc)]);
    rejects_without_public_effects(&mut e, "helper", &passed, alloc);
    let grant = e
        .lease(
            0,
            "helper",
            &WorkLeaseRequest { execution: e.id, attempt: e.attempt() },
            &mut Vec::new_in(alloc),
        )
        .unwrap()
        .grant
        .unwrap();
    e.result(
        1,
        "helper",
        &BatchResult {
            lease: grant.lease,
            seq: 1,
            items: vec_in![
                alloc;
                blaze_wire::ItemResult {
                    name: blaze_wire::WorkName([9; 32]),
                    outcome: blaze_wire::ItemOutcome::Local(blaze_wire::Why {
                        why: WireString::from_str_in("not exportable", alloc),
                    }),
                    found: Set::empty_in(alloc),
                    unnamed: Set::empty_in(alloc),
                }
            ],
            packs: Vec::new_in(alloc),
            returned_roots: Set::empty_in(alloc),
            returned_names: Set::empty_in(alloc),
        },
        &mut Vec::new_in(alloc),
    )
    .unwrap();
    let passed = request(&e, vec_in![alloc; done(Outcome::Passed, alloc)]);
    e.worker_events(2, "helper", &passed, &mut Vec::new_in(alloc)).unwrap();
    assert_eq!(e.state(), ExecState::Ended(Outcome::Passed));
}

#[test]
fn randomized_ingress_checks_public_stream_for_10000_seeds() {
    let alloc = heap();
    let mut g = blaze_wire::vectors::Gen::new(0x175);
    for seed in 0..10_000u64 {
        let mut e = execution(CompletionMode::WorkProtocol, alloc);
        let event = match g.below(5) {
            0 => stage(Stage::Compiled),
            1 => stage(Stage::Done),
            2 => stage(Stage::Queued),
            3 => done(Outcome::Passed, alloc),
            _ => JobEvent::Assigned(AssignedEvent {
                executor: WireString::from_str_in("forged", alloc),
                attempt: Attempt(2),
            }),
        };
        let mut r = request(
            &e,
            vec_in![
                alloc;
                JobEvent::Output(OutputEvent {
                    stream: 1,
                    bytes: vec_in![alloc; u8::try_from(seed & 0xff).unwrap()],
                }),
                event.clone(),
                event,
            ],
        );
        let from = if g.below(2) == 0 { "lead" } else { "helper" };
        if g.below(4) == 0 {
            e.assign(1, "helper", &[], &mut Vec::new_in(alloc)).unwrap();
        } else {
            r.attempt = e.attempt();
        }
        rejects_without_public_effects(&mut e, from, &r, alloc);
    }
}

#[test]
fn randomized_lead_progress_is_forward_deduplicated_and_coordinator_timed() {
    let alloc = heap();
    let mut g = blaze_wire::Gen::new(0x176);
    for seed in 0..10_000 {
        let mode = if g.below(2) == 0 {
            CompletionMode::NoCompileWork
        } else {
            CompletionMode::WorkProtocol
        };
        let mut e = execution(mode, alloc);
        let progress =
            request(&e, vec_in![alloc; stage(Stage::Fetched), stage(Stage::Loaded)]);
        e.worker_events(1, "lead", &progress, &mut Vec::new_in(alloc)).unwrap();
        let duplicate =
            request(&e, vec_in![alloc; stage(Stage::Loaded), stage(Stage::Loaded)]);
        let before = Vec::from_slice_in(e.events(0), alloc);
        e.worker_events(2, "lead", &duplicate, &mut Vec::new_in(alloc)).unwrap();
        assert_eq!(e.events(0), before.as_slice(), "duplicate progress seed {seed}");
        let backwards = request(&e, vec_in![alloc; stage(Stage::Fetched)]);
        rejects_without_public_effects(&mut e, "lead", &backwards, alloc);
        if mode == CompletionMode::WorkProtocol {
            let early = request(&e, vec_in![alloc; stage(Stage::Linked)]);
            rejects_without_public_effects(&mut e, "lead", &early, alloc);
            join_at(&mut e, "lead", 0, 2, 2, alloc);
        }
        let progress = request(
            &e,
            vec_in![
                alloc;
                stage(Stage::Published),
                stage(Stage::Linked),
                stage(Stage::Reported),
            ],
        );
        e.worker_events(3, "lead", &progress, &mut Vec::new_in(alloc)).unwrap();
        let public = Vec::from_iter_in(
            e.events(0).iter().filter_map(|item| match &item.event {
                JobEvent::Stage(s) => Some((s.stage.clone(), s.at_ms)),
                _ => None,
            }),
            alloc,
        );
        assert_eq!(public.as_slice(), e.stages(), "public/bookkeeping seed {seed}");
        assert_eq!(public.last(), Some(&(Stage::Reported, 3)));
        let old = request(&e, vec_in![alloc; stage(Stage::Reported)]);
        e.assign(4, "helper", &[], &mut Vec::new_in(alloc)).unwrap();
        rejects_without_public_effects(&mut e, "lead", &old, alloc);
    }
}
