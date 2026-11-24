//! Worker reports cannot manufacture coordinator-owned transitions.
//!
//! Validate the entire batch before publishing any of it. A rejected suffix must not leave
//! an accepted prefix (or a sticky refusal) in the execution's public event stream.

use blaze_wire::{JobEvent, Outcome, Stage, WireString};

use super::Execution;
use crate::{Millis, Refusal};

fn order(stage: &Stage) -> u8 {
    match stage {
        Stage::Queued => 0,
        Stage::Admitted => 1,
        Stage::Assigned => 2,
        Stage::Fetched => 3,
        Stage::Loaded => 4,
        Stage::Compiled => 5,
        Stage::Published => 6,
        Stage::Linked => 7,
        Stage::Reported => 8,
        Stage::Done => 9,
    }
}

impl Execution {
    /// The latest stage of this attempt, not a stage belonging to a previous lead.
    pub(super) fn reported_stage(&self) -> Option<&Stage> {
        self.events.iter().rev().find_map(|item| {
            if item.attempt != self.attempt {
                return None;
            }
            match &item.event {
                JobEvent::Stage(event) => Some(&event.stage),
                _ => None,
            }
        })
    }

    pub(super) fn validate_events(
        &self,
        now: Millis,
        lead: bool,
        events: &[JobEvent],
    ) -> Result<Option<Outcome>, Refusal> {
        let mut latest = self.reported_stage().map(order).unwrap_or(2);
        let mut done = None;
        for (index, event) in events.iter().enumerate() {
            match event {
                JobEvent::Refused(_) | JobEvent::Done(_) if self.publication.is_some() => {
                    return Err(Refusal::Malformed(WireString::from_str_in(
                        "worker verdict after full publication was accepted",
                        self.alloc,
                    )));
                }
                JobEvent::Assigned(_) => return Err(Refusal::NotHolder),
                JobEvent::Stage(event) => {
                    if !lead
                        || matches!(
                            event.stage,
                            Stage::Queued
                                | Stage::Admitted
                                | Stage::Assigned
                                | Stage::Compiled
                                | Stage::Done
                        )
                    {
                        return Err(Refusal::NotHolder);
                    }
                    let next = order(&event.stage);
                    if next < latest {
                        return Err(Refusal::Malformed(WireString::from_str_in(
                            "stage moved backwards",
                            self.alloc,
                        )));
                    }
                    if next > order(&Stage::Compiled) && !self.success_ready() {
                        return Err(Refusal::Malformed(WireString::from_str_in(
                            "post-compile stage before authoritative work completion",
                            self.alloc,
                        )));
                    }
                    latest = next;
                }
                JobEvent::Done(event) => {
                    if !lead {
                        return Err(Refusal::NotHolder);
                    }
                    if done.is_some() || index + 1 != events.len() {
                        return Err(Refusal::Malformed(WireString::from_str_in(
                            "Done must be the last event",
                            self.alloc,
                        )));
                    }
                    if !matches!(
                        event.outcome,
                        Outcome::Passed | Outcome::Failed | Outcome::Refused
                    ) {
                        return Err(Refusal::Malformed(WireString::from_str_in(
                            "invalid worker verdict batch",
                            self.alloc,
                        )));
                    }
                    if event.outcome == Outcome::Passed
                        && self.attached().any(|s| now <= s.deadline_ms)
                        && !self.success_ready()
                    {
                        return Err(Refusal::Malformed(WireString::from_str_in(
                            "success before authoritative completion",
                            self.alloc,
                        )));
                    }
                    done = Some(event.outcome.clone());
                }
                _ => {}
            }
        }
        Ok(done)
    }
}

#[cfg(test)]
mod tests {
    use blaze_own::{Vec, vec_in};
    use blaze_wire::{
        AssignedEvent, DoneEvent, ExecutionId, JobContent, LocalPolicy, Namespace, Outcome,
        OutputEvent, RefusedEvent, RootSet, Set, StageEvent, WireAlloc, WireString, WireVec,
        WorkJoinRequest, WorkersEventsRequest,
    };

    use super::*;
    use crate::jobs::{
        Effect, ExecState, ExecutionConfig, LocalAction, Subscriber, SubscriberState,
    };
    use crate::work::EndWhy;

    fn heap() -> WireAlloc {
        blaze_own::testing::heap().shared()
    }

    fn execution(alloc: WireAlloc) -> Execution {
        let mut execution = Execution::new(
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
            ExecutionConfig::default(),
            alloc,
            &mut Vec::new_in(alloc),
        );
        execution.assign(0, "lead", &["helper"], &mut Vec::new_in(alloc)).unwrap();
        execution
    }

    fn stage(stage: Stage) -> JobEvent {
        // The coordinator must use its own time rather than this untrusted timestamp.
        JobEvent::Stage(StageEvent { stage, at_ms: u64::MAX })
    }

    fn send(
        execution: &mut Execution,
        from: &str,
        events: WireVec<JobEvent>,
        alloc: WireAlloc,
    ) -> Result<(), Refusal> {
        let request =
            WorkersEventsRequest { execution: execution.id, attempt: execution.attempt(), events };
        execution.worker_events(1, from, &request, &mut Vec::new_in(alloc)).map(|_| ())
    }

    #[test]
    fn coordinator_stages_and_assignment_cannot_be_injected() {
        let alloc = heap();
        for from in ["lead", "helper"] {
            for reserved in
                [Stage::Queued, Stage::Admitted, Stage::Assigned, Stage::Compiled, Stage::Done]
            {
                let mut execution = execution(alloc);
                let before = Vec::from_slice_in(execution.events(0), alloc);
                assert_eq!(
                    send(&mut execution, from, vec_in![alloc; stage(reserved)], alloc),
                    Err(Refusal::NotHolder)
                );
                assert_eq!(execution.events(0), before.as_slice());
            }
            let mut execution = execution(alloc);
            let assignment = AssignedEvent {
                executor: WireString::from_str_in("other", alloc),
                attempt: execution.attempt(),
            };
            assert_eq!(
                send(&mut execution, from, vec_in![alloc; JobEvent::Assigned(assignment)], alloc),
                Err(Refusal::NotHolder)
            );
        }
    }

    #[test]
    fn a_rejected_suffix_does_not_publish_its_prefix_or_poison_the_verdict() {
        let alloc = heap();
        let mut execution = execution(alloc);
        let before = Vec::from_slice_in(execution.events(0), alloc);
        let events = vec_in![
            alloc;
            JobEvent::Output(OutputEvent {
                stream: 1,
                bytes: Vec::from_slice_in(b"not published", alloc),
            }),
            JobEvent::Refused(RefusedEvent {
                function: WireString::from_str_in("f", alloc),
                why: WireString::from_str_in("not committed", alloc),
            }),
            stage(Stage::Compiled),
        ];
        assert_eq!(send(&mut execution, "helper", events, alloc), Err(Refusal::NotHolder));
        assert_eq!(execution.events(0), before.as_slice());
        assert!(!execution.refused);
    }

    #[test]
    fn stages_are_monotone_deduplicated_and_recorded_in_both_views() {
        let alloc = heap();
        let mut execution = execution(alloc);
        send(
            &mut execution,
            "lead",
            vec_in![alloc; stage(Stage::Fetched), stage(Stage::Loaded)],
            alloc,
        )
        .unwrap();
        let before = Vec::from_slice_in(execution.events(0), alloc);
        send(&mut execution, "lead", vec_in![alloc; stage(Stage::Loaded)], alloc).unwrap();
        assert_eq!(execution.events(0), before.as_slice());
        assert!(send(&mut execution, "lead", vec_in![alloc; stage(Stage::Fetched)], alloc).is_err());
        assert_eq!(execution.stages().last(), Some(&(Stage::Loaded, 1)));
        assert!(
            send(&mut execution, "helper", vec_in![alloc; stage(Stage::Loaded)], alloc).is_err()
        );
        assert!(send(&mut execution, "lead", vec_in![alloc; stage(Stage::Linked)], alloc).is_err());

        let request = WorkJoinRequest {
            execution: execution.id,
            attempt: execution.attempt(),
            roots: RootSet {
                spec: [1; 32],
                roots: [2; 32],
                count: 0,
                index: [3; 32],
                inputs: [4; 32],
            },
            rejected: Set::empty_in(alloc),
        };
        execution.join(1, "lead", &request, &mut Vec::new_in(alloc)).unwrap();
        send(
            &mut execution,
            "lead",
            vec_in![alloc; stage(Stage::Published), stage(Stage::Linked)],
            alloc,
        )
        .unwrap();
        assert_eq!(execution.stages().last(), Some(&(Stage::Linked, 1)));
    }

    #[test]
    fn a_terminal_report_cannot_have_a_suffix_or_conflicting_second_verdict() {
        let alloc = heap();
        let mut execution = execution(alloc);
        let done =
            JobEvent::Done(DoneEvent { outcome: Outcome::Failed, counters: Vec::new_in(alloc) });
        let before = Vec::from_slice_in(execution.events(0), alloc);
        assert!(send(&mut execution, "lead", vec_in![alloc; done.clone(), done], alloc).is_err());
        assert_eq!(execution.events(0), before.as_slice());
        assert_eq!(execution.state(), ExecState::Running);
    }

    #[test]
    fn no_lead_is_infrastructure_loss_in_subscriber_and_wire_delivery() {
        let alloc = heap();
        let mut execution = execution(alloc);
        let mut effects = Vec::new_in(alloc);
        execution.end(1, Outcome::Failed, EndWhy::NoLead, &mut effects);
        assert_eq!(execution.state(), ExecState::Ended(Outcome::Lost));
        assert!(effects.iter().any(|effect| matches!(
            effect,
            Effect::SubscriberEnded { outcome: Outcome::Lost, .. }
        )));
        let verdict = execution
            .events(0)
            .iter()
            .find_map(|item| match &item.event {
                JobEvent::Done(done) => Some(done),
                _ => None,
            })
            .unwrap();
        let decoded: DoneEvent = blaze_wire::decode(
            &blaze_wire::encode(verdict, alloc),
            &blaze_wire::Limits::default(),
            alloc,
        )
        .unwrap();
        assert_eq!(decoded.outcome, Outcome::Lost);
        let terminal = execution.terminal(1).unwrap();
        let typed = blaze_wire::job_terminal_v2::decode_terminal(
            &blaze_wire::encode(terminal, alloc),
            &blaze_wire::Limits::default(),
            alloc,
        )
        .unwrap();
        assert_eq!(typed.termination.outcome, decoded.outcome);
        assert_eq!(
            typed.termination.reason,
            blaze_wire::job_terminal_v2::TerminationReasonV2::NoLead
        );
        assert_eq!(
            super::super::local_action(&LocalPolicy::Fallback, &typed.termination, alloc).unwrap(),
            LocalAction::BuildLocally
        );
        assert_eq!(
            super::super::local_action(&LocalPolicy::Never, &typed.termination, alloc).unwrap(),
            LocalAction::Fail
        );
        assert_eq!(
            super::super::local_action(
                &LocalPolicy::Fallback,
                &blaze_wire::job_terminal_v2::TerminationV2 {
                    version: 2,
                    outcome: Outcome::Failed,
                    reason: blaze_wire::job_terminal_v2::TerminationReasonV2::Reported,
                },
                alloc,
            )
            .unwrap(),
            LocalAction::Take
        );
    }
}
