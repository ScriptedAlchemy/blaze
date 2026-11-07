//! Worker ingress and typed terminal policy, checked against the public stream.

use blaze_dist::Refusal;
use blaze_dist::jobs::{
    CompletionMode, Effect, ExecState, Execution, ExecutionConfig, LocalAction, Subscriber,
    SubscriberState, local_action,
};
use blaze_wire::job_terminal_v2::{TerminationReasonV2 as Reason, TerminationV2, decode_terminal};
use blaze_wire::{
    AssignedEvent, Attempt, BatchResult, DoneEvent, ExecutionId, JobContent, JobEvent, Limits,
    LocalPolicy, Namespace, Outcome, OutputEvent, ProgramEvent, PublishedEvent, RefusedEvent,
    RootSet, Set, Stage, StageEvent, WorkJoinRequest, WorkLeaseRequest, WorkersEventsRequest,
    encode,
};

fn execution(mode: CompletionMode) -> Execution {
    let mut e = Execution::new(
        0,
        ExecutionId(1),
        JobContent([1; 32]),
        Namespace::Main,
        Subscriber {
            id: 1,
            deadline_ms: 100_000,
            policy: LocalPolicy::Fallback,
            test_timeout_ms: None,
            state: SubscriberState::Attached,
        },
        ExecutionConfig { completion: mode, ..ExecutionConfig::default() },
        &mut Vec::new(),
    );
    e.assign(0, "lead", &["helper".to_owned()], &mut Vec::new()).unwrap();
    e
}

fn request(e: &Execution, events: Vec<JobEvent>) -> WorkersEventsRequest {
    WorkersEventsRequest { execution: e.id, attempt: e.attempt(), events }
}

fn done(outcome: Outcome) -> JobEvent {
    JobEvent::Done(DoneEvent { outcome, counters: Vec::new() })
}

fn stage(stage: Stage) -> JobEvent {
    JobEvent::Stage(StageEvent { stage, at_ms: 0 })
}

fn join(e: &mut Execution, holder: &str, count: u64, tag: u8) {
    join_at(e, holder, count, tag, 0);
}

fn join_at(e: &mut Execution, holder: &str, count: u64, tag: u8, now: u64) {
    let r = WorkJoinRequest {
        execution: e.id,
        attempt: e.attempt(),
        roots: RootSet { spec: [1; 32], roots: [tag; 32], count, index: [3; 32], inputs: [4; 32] },
        rejected: Set::new(Vec::new()),
    };
    e.join(now, holder, &r, &mut Vec::new()).unwrap();
}

fn rejects_without_public_effects(e: &mut Execution, from: &str, r: WorkersEventsRequest) {
    let before = e.events(0).to_vec();
    let stages = e.stages().to_vec();
    let state = e.state();
    let mut effects = Vec::new();
    assert!(e.worker_events(1, from, &r, &mut effects).is_err());
    assert!(effects.is_empty(), "rejected batch emitted {effects:?}");
    assert_eq!(e.events(0), before, "rejected batch changed public stream");
    assert_eq!(e.stages(), stages);
    assert_eq!(e.state(), state);
}

#[test]
fn coordinator_stages_and_assignments_never_enter_from_workers() {
    for from in ["lead", "helper"] {
        for event in [
            stage(Stage::Compiled),
            stage(Stage::Done),
            stage(Stage::Queued),
            stage(Stage::Assigned),
            stage(Stage::Admitted),
            JobEvent::Assigned(AssignedEvent {
                executor: "intruder".to_owned(),
                attempt: Attempt(2),
            }),
        ] {
            let mut e = execution(CompletionMode::WorkProtocol);
            let r = request(&e, vec![event.clone(), event]);
            rejects_without_public_effects(&mut e, from, r);
        }
    }
    let mut e = execution(CompletionMode::WorkProtocol);
    let r = request(&e, vec![stage(Stage::Loaded)]);
    rejects_without_public_effects(&mut e, "helper", r);
}

#[test]
fn helper_roles_and_mixed_batches_are_prevalidated_atomically() {
    for event in [
        done(Outcome::Failed),
        JobEvent::Verdict(<blaze_wire::TestVerdict as blaze_wire::Sample>::sample(
            &mut blaze_wire::Gen::new(1),
        )),
        JobEvent::Program(ProgramEvent { name: "program".to_owned(), blob: [1; 32] }),
        JobEvent::Published(PublishedEvent { records: Vec::new() }),
        stage(Stage::Compiled),
    ] {
        let mut e = execution(CompletionMode::NoCompileWork);
        let r = request(
            &e,
            vec![
                JobEvent::Output(OutputEvent { stream: 1, bytes: vec![1] }),
                JobEvent::Refused(RefusedEvent { function: "f".to_owned(), why: "bad".to_owned() }),
                event,
            ],
        );
        rejects_without_public_effects(&mut e, "helper", r);
        // Even the valid Refused prefix was not applied.
        let passed = request(&e, vec![done(Outcome::Passed)]);
        e.worker_events(2, "lead", &passed, &mut Vec::new()).unwrap();
        assert_eq!(e.state(), ExecState::Ended(Outcome::Passed));
    }
}

#[test]
fn stale_and_duplicate_worker_batches_cannot_forge_stages_or_multiple_verdicts() {
    let mut e = execution(CompletionMode::NoCompileWork);
    let old = request(&e, vec![stage(Stage::Compiled)]);
    e.assign(1, "helper", &[], &mut Vec::new()).unwrap();
    rejects_without_public_effects(&mut e, "lead", old);
    let duplicate = request(&e, vec![done(Outcome::Passed), done(Outcome::Passed)]);
    rejects_without_public_effects(&mut e, "helper", duplicate);
    let passed = request(&e, vec![done(Outcome::Passed)]);
    e.worker_events(2, "helper", &passed, &mut Vec::new()).unwrap();
    let before = e.events(0).to_vec();
    e.worker_events(3, "helper", &passed, &mut Vec::new()).unwrap();
    assert_eq!(e.events(0), before);
    assert_eq!(before.iter().filter(|i| matches!(i.event, JobEvent::Done(_))).count(), 1);
}

#[test]
fn invalid_ingress_is_atomic_even_after_a_subscribers_deadline() {
    let mut e = execution(CompletionMode::NoCompileWork);
    let r = request(&e, vec![stage(Stage::Done)]);
    let before = e.events(0).to_vec();
    let mut effects = Vec::new();
    assert!(e.worker_events(100_001, "lead", &r, &mut effects).is_err());
    assert!(effects.is_empty());
    assert_eq!(e.events(0), before);
    assert!(e.terminal(1).is_none());
    // The actual timer still expires it normally; the invalid request did not deliver.
    e.tick(100_001, &mut effects);
    assert_eq!(e.terminal(1).unwrap().termination.reason, Reason::SubscriberDeadline);
}

#[test]
fn worker_cannot_classify_infrastructure_failure_as_a_verdict() {
    for outcome in [Outcome::Lost, Outcome::Cancelled, Outcome::TimedOut] {
        let mut e = execution(CompletionMode::NoCompileWork);
        let r = request(&e, vec![done(outcome)]);
        rejects_without_public_effects(&mut e, "lead", r);
    }
}

#[test]
fn success_requires_authoritative_completion_not_provisional_or_returned_work() {
    let mut e = execution(CompletionMode::WorkProtocol);
    let passed = request(&e, vec![done(Outcome::Passed)]);
    rejects_without_public_effects(&mut e, "lead", passed);
    join(&mut e, "helper", 0, 2);
    let passed = request(&e, vec![done(Outcome::Passed)]);
    rejects_without_public_effects(&mut e, "lead", passed);
    // A differing authoritative list reopens work; returned roots are not coverage.
    join(&mut e, "lead", 1, 3);
    let grant = e
        .lease(
            0,
            "lead",
            &WorkLeaseRequest { execution: e.id, attempt: e.attempt() },
            &mut Vec::new(),
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
            items: Vec::new(),
            packs: Vec::new(),
            returned_roots: grant.roots,
            returned_names: grant.names,
        },
        &mut Vec::new(),
    )
    .unwrap();
    let passed = request(&e, vec![done(Outcome::Passed)]);
    rejects_without_public_effects(&mut e, "lead", passed);
}

#[test]
fn explicit_no_work_and_authoritative_zero_roots_can_succeed() {
    for mode in [CompletionMode::WorkProtocol, CompletionMode::NoCompileWork] {
        let mut e = execution(mode);
        if mode == CompletionMode::WorkProtocol {
            join(&mut e, "lead", 0, 2);
        }
        let passed = request(&e, vec![done(Outcome::Passed)]);
        e.worker_events(1, "lead", &passed, &mut Vec::new()).unwrap();
        assert_eq!(e.state(), ExecState::Ended(Outcome::Passed));
    }
}

#[test]
fn retained_completion_requires_the_replacement_leads_agreement() {
    let mut e = execution(CompletionMode::WorkProtocol);
    join(&mut e, "lead", 0, 2);
    e.assign(1, "helper", &[], &mut Vec::new()).unwrap();
    let passed = request(&e, vec![done(Outcome::Passed)]);
    rejects_without_public_effects(&mut e, "helper", passed);
    join(&mut e, "helper", 0, 2);
    let passed = request(&e, vec![done(Outcome::Passed)]);
    e.worker_events(2, "helper", &passed, &mut Vec::new()).unwrap();
    assert_eq!(e.state(), ExecState::Ended(Outcome::Passed));
}

#[test]
fn terminal_delivery_retains_reason_through_wire_and_client_policy() {
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
        for policy in [LocalPolicy::Never, LocalPolicy::Fallback, LocalPolicy::Race] {
            let value =
                TerminationV2 { version: 2, reason: reason.clone(), outcome: outcome.clone() };
            let expected = if reason == Reason::Reported {
                LocalAction::Take
            } else if policy == LocalPolicy::Never {
                LocalAction::Fail
            } else {
                LocalAction::BuildLocally
            };
            assert_eq!(local_action(&policy, &value).unwrap(), expected);
        }
    }
    let mut e = execution(CompletionMode::NoCompileWork);
    let failed = request(&e, vec![done(Outcome::Failed)]);
    let mut effects = Vec::new();
    e.worker_events(1, "lead", &failed, &mut effects).unwrap();
    let delivered = effects
        .iter()
        .find_map(|fx| match fx {
            Effect::SubscriberEnded { terminal, .. } => Some(terminal),
            _ => None,
        })
        .unwrap();
    let decoded = decode_terminal(&encode(delivered), &Limits::DOCUMENTED).unwrap();
    assert_eq!(e.terminal(1), Some(&decoded));
    assert_eq!(decoded.termination.reason, Reason::Reported);
    assert_eq!(
        local_action(&LocalPolicy::Fallback, &decoded.termination).unwrap(),
        LocalAction::Take
    );
    let invalid = TerminationV2 { version: 1, reason: Reason::NoLead, outcome: Outcome::Failed };
    assert!(matches!(local_action(&LocalPolicy::Fallback, &invalid), Err(Refusal::Malformed(_))));
}

#[test]
fn actual_lead_loss_cancel_and_subscriber_timeout_deliver_distinct_reasons() {
    let mut e = execution(CompletionMode::NoCompileWork);
    e.tick(15_001, &mut Vec::new());
    e.assign(15_002, "helper", &[], &mut Vec::new()).unwrap();
    e.tick(30_003, &mut Vec::new());
    assert_eq!(e.terminal(1).unwrap().termination.reason, Reason::LeadLostTwice);
    let mut e = execution(CompletionMode::WorkProtocol);
    e.cancel(1, &mut Vec::new());
    assert_eq!(e.terminal(1).unwrap().termination.reason, Reason::Cancelled);
    let mut e = execution(CompletionMode::WorkProtocol);
    e.tick(100_001, &mut Vec::new());
    assert_eq!(e.terminal(1).unwrap().termination.reason, Reason::SubscriberDeadline);
}

#[test]
fn no_lead_lost_delivery_is_not_a_completed_failed_verdict() {
    let mut admission =
        blaze_dist::admission::Admission::new(blaze_dist::admission::AdmissionConfig::default());
    let reservation = blaze_wire::Reservation { memory: 4, cpus: 4, disk: 4 };
    admission.add_machine("machine", [1; 32], reservation.clone());
    admission.add_builder(0, "machine", reservation.clone()).unwrap();
    let mut jobs = blaze_dist::jobs::Jobs::new(admission, ExecutionConfig::default());
    let mut spec =
        <blaze_wire::BuildSpec as blaze_wire::Sample>::sample(&mut blaze_wire::Gen::new(1));
    spec.platform = [1; 32];
    let answer = jobs
        .submit(
            0,
            &blaze_wire::JobRequest {
                spec,
                namespace: Namespace::Main,
                deadline_ms: 100_000,
                local: LocalPolicy::Fallback,
                test_timeout_ms: None,
            },
            reservation,
            &mut Vec::new(),
        )
        .unwrap();
    jobs.tick(0, &mut Vec::new());
    let mut effects = Vec::new();
    jobs.tick(15_001, &mut effects);
    let terminal = jobs.terminal(answer.execution, answer.subscriber).unwrap();
    assert_eq!(terminal.termination.reason, Reason::NoLead);
    assert_eq!(terminal.termination.outcome, Outcome::Lost);
    let decoded = decode_terminal(&encode(terminal), &Limits::DOCUMENTED).unwrap();
    assert_eq!(
        local_action(&LocalPolicy::Fallback, &decoded.termination).unwrap(),
        LocalAction::BuildLocally
    );
}

#[test]
fn no_compile_mode_rejects_work_and_failed_ingress_does_not_renew_lead_liveness() {
    let mut e = execution(CompletionMode::NoCompileWork);
    let r = WorkLeaseRequest { execution: e.id, attempt: e.attempt() };
    assert!(e.lease(0, "lead", &r, &mut Vec::new()).is_err());
    let bad = request(&e, vec![stage(Stage::Compiled)]);
    assert!(e.worker_events(14_999, "lead", &bad, &mut Vec::new()).is_err());
    let mut effects = Vec::new();
    e.tick(15_001, &mut effects);
    assert!(effects.contains(&Effect::NeedLead));
}

#[test]
fn changed_root_epoch_blocks_success_until_its_work_is_done() {
    let mut e = execution(CompletionMode::WorkProtocol);
    join(&mut e, "lead", 0, 2);
    e.assign(1, "helper", &[], &mut Vec::new()).unwrap();
    join(&mut e, "helper", 1, 3);
    let passed = request(&e, vec![done(Outcome::Passed)]);
    rejects_without_public_effects(&mut e, "helper", passed);
    let grant = e
        .lease(
            0,
            "helper",
            &WorkLeaseRequest { execution: e.id, attempt: e.attempt() },
            &mut Vec::new(),
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
            items: vec![blaze_wire::ItemResult {
                name: blaze_wire::WorkName([9; 32]),
                outcome: blaze_wire::ItemOutcome::Local(blaze_wire::Why {
                    why: "not exportable".to_owned(),
                }),
                found: Set::new(Vec::new()),
                unnamed: Set::new(Vec::new()),
            }],
            packs: Vec::new(),
            returned_roots: Set::new(Vec::new()),
            returned_names: Set::new(Vec::new()),
        },
        &mut Vec::new(),
    )
    .unwrap();
    let passed = request(&e, vec![done(Outcome::Passed)]);
    e.worker_events(2, "helper", &passed, &mut Vec::new()).unwrap();
    assert_eq!(e.state(), ExecState::Ended(Outcome::Passed));
}

#[test]
fn randomized_ingress_checks_public_stream_for_10000_seeds() {
    let mut g = blaze_wire::vectors::Gen::new(0x175);
    for seed in 0..10_000 {
        let mut e = execution(CompletionMode::WorkProtocol);
        let event = match g.below(5) {
            0 => stage(Stage::Compiled),
            1 => stage(Stage::Done),
            2 => stage(Stage::Queued),
            3 => done(Outcome::Passed),
            _ => JobEvent::Assigned(AssignedEvent {
                executor: "forged".to_owned(),
                attempt: Attempt(2),
            }),
        };
        let mut r = request(
            &e,
            vec![
                JobEvent::Output(OutputEvent { stream: 1, bytes: vec![seed as u8] }),
                event.clone(),
                event,
            ],
        );
        let from = if g.below(2) == 0 { "lead" } else { "helper" };
        if g.below(4) == 0 {
            e.assign(1, "helper", &[], &mut Vec::new()).unwrap();
        } else {
            r.attempt = e.attempt();
        }
        rejects_without_public_effects(&mut e, from, r);
    }
}

#[test]
fn randomized_lead_progress_is_forward_deduplicated_and_coordinator_timed() {
    let mut g = blaze_wire::Gen::new(0x176);
    for seed in 0..10_000 {
        let mode = if g.below(2) == 0 {
            CompletionMode::NoCompileWork
        } else {
            CompletionMode::WorkProtocol
        };
        let mut e = execution(mode);
        let progress = request(&e, vec![stage(Stage::Fetched), stage(Stage::Loaded)]);
        e.worker_events(1, "lead", &progress, &mut Vec::new()).unwrap();
        let duplicate = request(&e, vec![stage(Stage::Loaded), stage(Stage::Loaded)]);
        let before = e.events(0).to_vec();
        e.worker_events(2, "lead", &duplicate, &mut Vec::new()).unwrap();
        assert_eq!(e.events(0), before, "duplicate progress seed {seed}");
        let backwards = request(&e, vec![stage(Stage::Fetched)]);
        rejects_without_public_effects(&mut e, "lead", backwards);
        if mode == CompletionMode::WorkProtocol {
            let early = request(&e, vec![stage(Stage::Linked)]);
            rejects_without_public_effects(&mut e, "lead", early);
            join_at(&mut e, "lead", 0, 2, 2);
        }
        let progress = request(
            &e,
            vec![
                stage(Stage::Published),
                stage(Stage::Linked),
                stage(Stage::Tested),
                stage(Stage::Reported),
            ],
        );
        e.worker_events(3, "lead", &progress, &mut Vec::new()).unwrap();
        let public: Vec<_> = e
            .events(0)
            .iter()
            .filter_map(|item| match &item.event {
                JobEvent::Stage(s) => Some((s.stage.clone(), s.at_ms)),
                _ => None,
            })
            .collect();
        assert_eq!(public, e.stages(), "public/bookkeeping seed {seed}");
        assert_eq!(public.last(), Some(&(Stage::Reported, 3)));
        let old = request(&e, vec![stage(Stage::Reported)]);
        e.assign(4, "helper", &[], &mut Vec::new()).unwrap();
        rejects_without_public_effects(&mut e, "lead", old);
    }
}
