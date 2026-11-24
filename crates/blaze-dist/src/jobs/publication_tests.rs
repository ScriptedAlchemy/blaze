//! The publication lifetime on a virtual clock: scheduling and failures are drawn deterministically.
//! The oracle observes real Execution transitions, not a second implementation of them.

use blaze_own::{Vec, format_in, vec_in};
use blaze_wire::{WireAlloc, WireString, WireVec};

use super::*;
use crate::sim::Rng;

fn heap() -> WireAlloc {
    blaze_own::testing::heap().shared()
}

#[derive(Clone, Copy)]
enum Plant {
    None,
    EarlyDone,
    LateVerdict,
}

fn execution(alloc: WireAlloc) -> Execution {
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
        alloc,
        &mut Vec::new_in(alloc),
    );
    e.assign(0, "lead0", &[], &mut Vec::new_in(alloc)).unwrap();
    join(&mut e, 0, alloc);
    e
}

fn join(e: &mut Execution, now: u64, alloc: WireAlloc) {
    let lead = WireString::from_str_in(e.lead().unwrap(), alloc);
    e.join(
        now,
        lead.as_str(),
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
            rejected: blaze_wire::Set::empty_in(alloc),
        },
        &mut Vec::new_in(alloc),
    )
    .unwrap();
}

fn packs(tag: u8, alloc: WireAlloc) -> WireVec<PackRef> {
    vec_in![alloc; PackRef { pack: [tag; 32], size: 1, keys: [tag; 32], packed: None, format: 1 }]
}

fn verdict(
    e: &mut Execution,
    now: u64,
    outcome: Outcome,
    plant: Plant,
    alloc: WireAlloc,
) -> Result<WorkersEventsAnswer, Refusal> {
    let completion = e.config.completion;
    if matches!(plant, Plant::EarlyDone) {
        // Plant the missing safeguard: work completion alone is treated as unit completion.
        e.config.completion = CompletionMode::WorkProtocol;
    }
    // Plant the missing publication fence while retaining the accepted record for
    // the oracle: a worker's later verdict must not supersede that full output.
    let publication = if matches!(plant, Plant::LateVerdict) { e.publication.take() } else { None };
    let lead = WireString::from_str_in(e.lead().unwrap(), alloc);
    let result = e.worker_events(
        now,
        lead.as_str(),
        &WorkersEventsRequest {
            execution: e.id,
            attempt: e.attempt(),
            events: vec_in![alloc; JobEvent::Done(DoneEvent { outcome, counters: Vec::new_in(alloc) })],
        },
        &mut Vec::new_in(alloc),
    );
    e.config.completion = completion;
    if let Some(publication) = publication {
        e.publication = Some(publication);
    }
    result
}

fn scenario(ops: &[u8], plant: Plant, alloc: WireAlloc) -> Result<(), WireString> {
    let mut e = execution(alloc);
    let mut now = 0;
    let mut uploaded = false;
    let mut accepted: Option<WireVec<PackRef>> = None;
    let mut delivered = false;
    let mut next_subscriber = 2;
    for op in ops {
        now += 1;
        match op % 12 {
            0 if e.lead().is_some() && e.state() == ExecState::Running => {
                let _ = verdict(&mut e, now, Outcome::Passed, plant, alloc);
            }
            1 => uploaded = true,
            2 if uploaded && e.lead().is_some() && e.state() == ExecState::Running => {
                let lead = WireString::from_str_in(e.lead().unwrap(), alloc);
                let record = packs(7, alloc);
                if e.commit_publication(
                    now,
                    lead.as_str(),
                    e.attempt(),
                    &record,
                    &mut Vec::new_in(alloc),
                )
                .is_ok()
                {
                    accepted = Some(record);
                }
            }
            3 if accepted.is_some() => {
                if e.deliver_publication(now, accepted.as_ref().unwrap(), &mut Vec::new_in(alloc))
                    .is_ok()
                {
                    delivered = true;
                }
            }
            4 if e.lead().is_some() => {
                let lead = WireString::from_str_in(e.lead().unwrap(), alloc);
                let request = WorkersEventsRequest {
                    execution: e.id,
                    attempt: e.attempt(),
                    events: Vec::new_in(alloc),
                };
                let _ = e.worker_events(now, lead.as_str(), &request, &mut Vec::new_in(alloc));
            }
            5 => {
                now += 20;
                e.tick(now, &mut Vec::new_in(alloc));
                if e.state() == ExecState::Running && e.lead().is_none() {
                    let lead = format_in!(alloc, "lead{}", e.attempt().0);
                    e.assign(now, lead.as_str(), &[], &mut Vec::new_in(alloc)).unwrap();
                    join(&mut e, now, alloc);
                }
            }
            6 if e.attempt() != Attempt(1) => {
                assert!(
                    e.commit_publication(
                        now,
                        "lead0",
                        Attempt(1),
                        &packs(9, alloc),
                        &mut Vec::new_in(alloc)
                    )
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
                let lead = WireString::from_str_in(e.lead().unwrap(), alloc);
                assert!(
                    e.commit_publication(
                        now,
                        lead.as_str(),
                        e.attempt(),
                        &packs(9, alloc),
                        &mut Vec::new_in(alloc)
                    )
                    .is_err()
                );
            }
            10 | 11
                if accepted.is_some()
                    && e.lead().is_some()
                    && e.state() == ExecState::Running =>
            {
                let outcome = if op % 12 == 10 { Outcome::Failed } else { Outcome::Refused };
                let _ = verdict(&mut e, now, outcome, plant, alloc);
            }
            _ => {}
        }
        if e.publication() != accepted.as_deref() {
            return Err(format_in!(alloc, "the accepted record changed at {now}, op {op}"));
        }
        if e.state() == ExecState::Ended(Outcome::Passed)
            && (!uploaded || accepted.is_none() || !delivered)
        {
            return Err(format_in!(
                alloc,
                "success preceded uploaded, accepted and delivered full output at {now}, op {op}"
            ));
        }
        if accepted.is_some()
            && matches!(e.state(), ExecState::Ended(Outcome::Failed | Outcome::Refused))
        {
            return Err(format_in!(
                alloc,
                "accepted full output was superseded by a later worker verdict at {now}, op {op}"
            ));
        }
    }
    Ok(())
}

/// The invariant: a published-work execution reports success only after full output
/// is uploaded, its immutable manifest is accepted unchanged, and delivery to
/// subscribers completes; a later attempt cannot replace the accepted record and
/// a later worker verdict cannot supersede it.
#[test]
fn every_success_has_one_immutable_full_manifest() {
    let alloc = heap();
    for seed in 0..128u64 {
        let mut rng = Rng::new(seed ^ 0x90B1_CAFE);
        let len = rng.range_usize(1, 128);
        let mut ops = Vec::with_capacity_in(len, alloc);
        for _ in 0..len {
            ops.push(u8::try_from(rng.below(12)).unwrap());
        }
        scenario(&ops, Plant::None, alloc).unwrap();
    }
}

#[test]
fn the_oracle_catches_an_early_done_plant() {
    let alloc = heap();
    assert!(scenario(&[0], Plant::EarlyDone, alloc).unwrap_err().contains("success preceded"));
}

#[test]
fn the_oracle_catches_a_late_worker_verdict_plant() {
    let alloc = heap();
    for verdict in [10, 11] {
        assert!(
            scenario(&[1, 2, verdict], Plant::LateVerdict, alloc)
                .unwrap_err()
                .contains("superseded by a later worker verdict")
        );
    }
}

#[test]
fn phase_completion_and_manifest_commit_do_not_end_a_unit_before_delivery() {
    let alloc = heap();
    let mut e = execution(alloc);
    assert!(verdict(&mut e, 1, Outcome::Passed, Plant::None, alloc).is_err());
    e.commit_publication(2, "lead0", Attempt(1), &packs(7, alloc), &mut Vec::new_in(alloc))
        .unwrap();
    assert!(verdict(&mut e, 3, Outcome::Passed, Plant::None, alloc).is_err());
    assert_eq!(e.state(), ExecState::Running);
    e.subscribe(Subscriber {
        id: 2,
        deadline_ms: 1_000,
        policy: LocalPolicy::Never,
        state: SubscriberState::Attached,
    })
    .unwrap();
    assert_eq!(e.publication(), Some(packs(7, alloc).as_slice()));
}

#[test]
fn a_lost_commit_reply_and_a_dead_lead_cannot_change_the_accepted_record() {
    let alloc = heap();
    let mut e = execution(alloc);
    let record = packs(7, alloc);
    e.commit_publication(1, "lead0", Attempt(1), &record, &mut Vec::new_in(alloc)).unwrap();
    assert_eq!(e.publication_attempt(), Some(Attempt(1)));
    // A lost answer repeats the same transition, and publishes no second stage/event.
    let events = e.events(0).len();
    e.commit_publication(2, "lead0", Attempt(1), &record, &mut Vec::new_in(alloc)).unwrap();
    assert_eq!(e.events(0).len(), events);
    e.tick(18, &mut Vec::new_in(alloc));
    e.assign(18, "replacement", &[], &mut Vec::new_in(alloc)).unwrap();
    assert_eq!(
        e.publication_attempt(),
        Some(Attempt(1)),
        "acceptance provenance survives reassignment"
    );
    assert_eq!(
        e.commit_publication(19, "lead0", Attempt(1), &packs(9, alloc), &mut Vec::new_in(alloc)),
        Err(Refusal::AttemptMoved)
    );
    assert!(
        e.commit_publication(
            19,
            "replacement",
            e.attempt(),
            &packs(9, alloc),
            &mut Vec::new_in(alloc)
        )
        .is_err()
    );
    assert_eq!(e.publication(), Some(record.as_slice()));
    // Mirror failure has no transition. A subscriber can retry delivery without rerunning code.
    assert_eq!(e.state(), ExecState::Running);
    e.deliver_publication(20, &record, &mut Vec::new_in(alloc)).unwrap();
    let events = e.events(0).len();
    e.deliver_publication(21, &record, &mut Vec::new_in(alloc)).unwrap();
    assert_eq!(e.events(0).len(), events);
    assert_eq!(e.state(), ExecState::Ended(Outcome::Passed));
}

#[test]
fn an_accepted_refusal_cannot_publish_success_after_reassignment() {
    let alloc = heap();
    for replace in [false, true] {
        for fallback in [false, true] {
            let policy = if fallback { LocalPolicy::Fallback } else { LocalPolicy::Never };
            let mut e = execution(alloc);
            e.subscribers.get_mut(&1).unwrap().policy = policy.clone();
            e.worker_events(
                1,
                "lead0",
                &WorkersEventsRequest {
                    execution: e.id,
                    attempt: e.attempt(),
                    events: vec_in![
                        alloc;
                        JobEvent::Refused(blaze_wire::RefusedEvent {
                            function: WireString::from_str_in("fixture", alloc),
                            why: WireString::from_str_in("refused", alloc),
                        })
                    ],
                },
                &mut Vec::new_in(alloc),
            )
            .unwrap();
            let now = if replace {
                e.tick(18, &mut Vec::new_in(alloc));
                e.assign(18, "replacement", &[], &mut Vec::new_in(alloc)).unwrap();
                join(&mut e, 18, alloc);
                19
            } else {
                2
            };
            let lead = WireString::from_str_in(e.lead().unwrap(), alloc);
            assert_eq!(
                e.commit_publication(
                    now,
                    lead.as_str(),
                    e.attempt(),
                    &packs(7, alloc),
                    &mut Vec::new_in(alloc)
                ),
                Err(Refusal::Ended)
            );
            assert_eq!(e.state(), ExecState::Ended(Outcome::Refused));
            assert!(e.publication().is_none(), "a refused unit must not leak a cache manifest");
            let terminal = e.terminals.get(&1).unwrap();
            assert_eq!(terminal.termination.outcome, Outcome::Refused);
            assert_eq!(
                local_action(&policy, &terminal.termination, alloc).unwrap(),
                LocalAction::Take
            );
            assert!(
                e.deliver_publication(now, &packs(7, alloc), &mut Vec::new_in(alloc)).is_err()
            );
            assert_eq!(e.state(), ExecState::Ended(Outcome::Refused));
        }
    }
}

#[test]
fn a_late_worker_verdict_cannot_rewrite_an_accepted_manifest() {
    let alloc = heap();
    for kind in 0..=2u8 {
        let mut e = execution(alloc);
        e.commit_publication(1, "lead0", e.attempt(), &packs(7, alloc), &mut Vec::new_in(alloc))
            .unwrap();
        let before = e.events(0).len();
        let event = match kind {
            0 => JobEvent::Refused(blaze_wire::RefusedEvent {
                function: WireString::from_str_in("late", alloc),
                why: WireString::from_str_in("late", alloc),
            }),
            _ => JobEvent::Done(DoneEvent {
                outcome: if kind == 1 { Outcome::Failed } else { Outcome::Refused },
                counters: Vec::new_in(alloc),
            }),
        };
        assert!(
            e.worker_events(
                2,
                "lead0",
                &WorkersEventsRequest {
                    execution: e.id,
                    attempt: e.attempt(),
                    events: vec_in![
                        alloc;
                        JobEvent::Output(blaze_wire::OutputEvent {
                            stream: 1,
                            bytes: Vec::from_slice_in(b"unaccepted prefix", alloc),
                        }),
                        event,
                    ],
                },
                &mut Vec::new_in(alloc),
            )
            .is_err()
        );
        assert_eq!(e.events(0).len(), before, "the rejected batch has no accepted prefix");
        e.deliver_publication(3, &packs(7, alloc), &mut Vec::new_in(alloc)).unwrap();
        assert_eq!(e.state(), ExecState::Ended(Outcome::Passed));
    }
}
