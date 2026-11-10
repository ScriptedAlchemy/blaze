//! The publication lifetime on a virtual clock: scheduling and failures are drawn by Hegel.
//! The oracle observes real Execution transitions, not a second implementation of them.

use hegel::generators as gs;

use super::*;

#[derive(Clone, Copy)]
enum Plant {
    None,
    EarlyDone,
}

fn execution() -> Execution {
    let mut e = Execution::new(
        0,
        ExecutionId(1),
        JobContent([1; 32]),
        Namespace::Main,
        Subscriber {
            id: 1,
            deadline_ms: 1_000_000,
            policy: LocalPolicy::Never,
            state: SubscriberState::Attached,
        },
        ExecutionConfig {
            heartbeat_ms: 5,
            completion: CompletionMode::PublishedWork,
            ..Default::default()
        },
        &mut Vec::new(),
    );
    e.assign(0, "lead0", &[], &mut Vec::new()).unwrap();
    join(&mut e, 0);
    e
}

fn join(e: &mut Execution, now: u64) {
    let lead = e.lead().unwrap().to_owned();
    e.join(
        now,
        &lead,
        &WorkJoinRequest {
            execution: e.id,
            attempt: e.attempt(),
            roots: blaze_wire::RootSet {
                spec: [1; 32],
                roots: [2; 32],
                count: 0,
                index: [3; 32],
                inputs: [4; 32],
            },
            rejected: blaze_wire::Set::new(Vec::new()),
        },
        &mut Vec::new(),
    )
    .unwrap();
}

fn packs(tag: u8) -> Vec<PackRef> {
    vec![PackRef { pack: [tag; 32], size: 1, keys: [tag; 32], packed: None, format: 1 }]
}

fn done(e: &mut Execution, now: u64, plant: Plant) -> Result<WorkersEventsAnswer, Refusal> {
    let completion = e.config.completion;
    if matches!(plant, Plant::EarlyDone) {
        // Plant the missing safeguard: work completion alone is treated as unit completion.
        e.config.completion = CompletionMode::WorkProtocol;
    }
    let lead = e.lead().unwrap().to_owned();
    let result = e.worker_events(
        now,
        &lead,
        &WorkersEventsRequest {
            execution: e.id,
            attempt: e.attempt(),
            events: vec![JobEvent::Done(DoneEvent {
                outcome: Outcome::Passed,
                counters: Vec::new(),
            })],
        },
        &mut Vec::new(),
    );
    e.config.completion = completion;
    result
}

fn scenario(ops: &[u8], plant: Plant) -> Result<(), String> {
    let mut e = execution();
    let mut now = 0;
    let mut uploaded = false;
    let mut accepted = None;
    let mut delivered = false;
    let mut next_subscriber = 2;
    for op in ops {
        now += 1;
        match op % 10 {
            0 if e.lead().is_some() && e.state() == ExecState::Running => {
                let _ = done(&mut e, now, plant);
            }
            1 => uploaded = true,
            2 if uploaded && e.lead().is_some() && e.state() == ExecState::Running => {
                let lead = e.lead().unwrap().to_owned();
                let record = packs(7);
                if e.commit_publication(now, &lead, e.attempt(), &record, &mut Vec::new()).is_ok() {
                    accepted = Some(record);
                }
            }
            3 if accepted.is_some() => {
                if e.deliver_publication(now, accepted.as_ref().unwrap(), &mut Vec::new()).is_ok() {
                    delivered = true;
                }
            }
            4 if e.lead().is_some() => {
                let lead = e.lead().unwrap().to_owned();
                let request = WorkersEventsRequest {
                    execution: e.id,
                    attempt: e.attempt(),
                    events: Vec::new(),
                };
                let _ = e.worker_events(now, &lead, &request, &mut Vec::new());
            }
            5 => {
                now += 20;
                e.tick(now, &mut Vec::new());
                if e.state() == ExecState::Running && e.lead().is_none() {
                    let lead = format!("lead{}", e.attempt().0);
                    e.assign(now, &lead, &[], &mut Vec::new()).unwrap();
                    join(&mut e, now);
                }
            }
            6 if e.attempt() != Attempt(1) => {
                assert!(
                    e.commit_publication(now, "lead0", Attempt(1), &packs(9), &mut Vec::new())
                        .is_err()
                );
            }
            7 if e.state() == ExecState::Running => {
                e.subscribe(Subscriber {
                    id: next_subscriber,
                    deadline_ms: 1_000_000,
                    policy: LocalPolicy::Fallback,
                    state: SubscriberState::Attached,
                })
                .unwrap();
                next_subscriber += 1;
            }
            8 => {
                // Cache mirror failure or a lost response changes no accepted record.
                if accepted.is_some() && !delivered && e.state() == ExecState::Running {
                    assert_ne!(e.state(), ExecState::Ended(Outcome::Passed));
                }
            }
            9 if accepted.is_some() && e.lead().is_some() => {
                let lead = e.lead().unwrap().to_owned();
                assert!(
                    e.commit_publication(now, &lead, e.attempt(), &packs(9), &mut Vec::new())
                        .is_err()
                );
            }
            _ => {}
        }
        if e.publication() != accepted.as_deref() {
            return Err(format!("the accepted record changed at {now}, op {op}"));
        }
        if e.state() == ExecState::Ended(Outcome::Passed)
            && (!uploaded || accepted.is_none() || !delivered)
        {
            return Err(format!(
                "success preceded uploaded, accepted and delivered full output at {now}, op {op}"
            ));
        }
    }
    Ok(())
}

/// The invariant: a published-work execution reports success only after full output
/// is uploaded, its immutable manifest is accepted unchanged, and delivery to
/// subscribers completes; a later attempt cannot replace the accepted record.
#[hegel::test(test_cases = 128)]
fn every_success_has_one_immutable_full_manifest(tc: hegel::TestCase) {
    let ops = tc.draw(gs::vecs(gs::integers::<u8>().max_value(9)).min_size(1).max_size(128));
    scenario(&ops, Plant::None).unwrap();
}

#[test]
fn the_oracle_catches_an_early_done_plant() {
    assert!(scenario(&[0], Plant::EarlyDone).unwrap_err().contains("success preceded"));
}

#[test]
fn phase_completion_and_manifest_commit_do_not_end_a_unit_before_delivery() {
    let mut e = execution();
    assert!(done(&mut e, 1, Plant::None).is_err());
    e.commit_publication(2, "lead0", Attempt(1), &packs(7), &mut Vec::new()).unwrap();
    assert!(done(&mut e, 3, Plant::None).is_err());
    assert_eq!(e.state(), ExecState::Running);
    e.subscribe(Subscriber {
        id: 2,
        deadline_ms: 1_000,
        policy: LocalPolicy::Never,
        state: SubscriberState::Attached,
    })
    .unwrap();
    assert_eq!(e.publication(), Some(packs(7).as_slice()));
}

#[test]
fn a_lost_commit_reply_and_a_dead_lead_cannot_change_the_accepted_record() {
    let mut e = execution();
    let record = packs(7);
    e.commit_publication(1, "lead0", Attempt(1), &record, &mut Vec::new()).unwrap();
    assert_eq!(e.publication_attempt(), Some(Attempt(1)));
    // A lost answer repeats the same transition, and publishes no second stage/event.
    let events = e.events(0).len();
    e.commit_publication(2, "lead0", Attempt(1), &record, &mut Vec::new()).unwrap();
    assert_eq!(e.events(0).len(), events);
    e.tick(18, &mut Vec::new());
    e.assign(18, "replacement", &[], &mut Vec::new()).unwrap();
    assert_eq!(
        e.publication_attempt(),
        Some(Attempt(1)),
        "acceptance provenance survives reassignment"
    );
    assert_eq!(
        e.commit_publication(19, "lead0", Attempt(1), &packs(9), &mut Vec::new()),
        Err(Refusal::AttemptMoved)
    );
    assert!(
        e.commit_publication(19, "replacement", e.attempt(), &packs(9), &mut Vec::new()).is_err()
    );
    assert_eq!(e.publication(), Some(record.as_slice()));
    // Mirror failure has no transition. A subscriber can retry delivery without rerunning code.
    assert_eq!(e.state(), ExecState::Running);
    e.deliver_publication(20, &record, &mut Vec::new()).unwrap();
    let events = e.events(0).len();
    e.deliver_publication(21, &record, &mut Vec::new()).unwrap();
    assert_eq!(e.events(0).len(), events);
    assert_eq!(e.state(), ExecState::Ended(Outcome::Passed));
}

#[hegel::test(test_cases = 16)]
fn an_accepted_refusal_cannot_publish_success_after_reassignment(tc: hegel::TestCase) {
    let replace = tc.draw(gs::integers::<u8>().max_value(1)) != 0;
    let fallback = tc.draw(gs::integers::<u8>().max_value(1)) != 0;
    let policy = if fallback { LocalPolicy::Fallback } else { LocalPolicy::Never };
    let mut e = execution();
    e.subscribers.get_mut(&1).unwrap().policy = policy.clone();
    e.worker_events(
        1,
        "lead0",
        &WorkersEventsRequest {
            execution: e.id,
            attempt: e.attempt(),
            events: vec![JobEvent::Refused(blaze_wire::RefusedEvent {
                function: "fixture".into(),
                why: "refused".into(),
            })],
        },
        &mut Vec::new(),
    )
    .unwrap();
    let now = if replace {
        e.tick(18, &mut Vec::new());
        e.assign(18, "replacement", &[], &mut Vec::new()).unwrap();
        join(&mut e, 18);
        19
    } else {
        2
    };
    let lead = e.lead().unwrap().to_owned();
    assert_eq!(
        e.commit_publication(now, &lead, e.attempt(), &packs(7), &mut Vec::new()),
        Err(Refusal::Ended)
    );
    assert_eq!(e.state(), ExecState::Ended(Outcome::Refused));
    assert!(e.publication().is_none(), "a refused unit must not leak a cache manifest");
    let terminal = e.terminals.get(&1).unwrap();
    assert_eq!(terminal.termination.outcome, Outcome::Refused);
    assert_eq!(local_action(&policy, &terminal.termination).unwrap(), LocalAction::Take);
    assert!(e.deliver_publication(now, &packs(7), &mut Vec::new()).is_err());
    assert_eq!(e.state(), ExecState::Ended(Outcome::Refused));
}

#[test]
fn a_late_refusal_batch_cannot_rewrite_an_accepted_manifest() {
    let mut e = execution();
    e.commit_publication(1, "lead0", e.attempt(), &packs(7), &mut Vec::new()).unwrap();
    let before = e.events(0).len();
    assert!(
        e.worker_events(
            2,
            "lead0",
            &WorkersEventsRequest {
                execution: e.id,
                attempt: e.attempt(),
                events: vec![JobEvent::Refused(blaze_wire::RefusedEvent {
                    function: "late".into(),
                    why: "late".into()
                })],
            },
            &mut Vec::new()
        )
        .is_err()
    );
    assert_eq!(e.events(0).len(), before, "the rejected batch has no accepted prefix");
    e.deliver_publication(3, &packs(7), &mut Vec::new()).unwrap();
    assert_eq!(e.state(), ExecState::Ended(Outcome::Passed));
}
