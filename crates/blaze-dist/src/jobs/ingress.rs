//! Worker reports cannot manufacture coordinator-owned transitions.
//!
//! Validate the entire batch before publishing any of it. A rejected suffix must not leave
//! an accepted prefix (or a sticky refusal) in the execution's public event stream.

use blaze_wire::{JobEvent, Stage};

use super::Execution;
use crate::Refusal;

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
        Stage::Tested => 8,
        Stage::Reported => 9,
        Stage::Done => 10,
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

    pub(super) fn validate_events(&self, lead: bool, events: &[JobEvent]) -> Result<(), Refusal> {
        let mut latest = self.reported_stage().map(order).unwrap_or(2);
        for (index, event) in events.iter().enumerate() {
            match event {
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
                        return Err(Refusal::Malformed("stage moved backwards".to_owned()));
                    }
                    if next > order(&Stage::Compiled)
                        && !self.work.as_ref().is_some_and(|work| {
                            work.lead_joined()
                                && work.is_done()
                                && self.compiled == Some((self.attempt, work.root_epoch()))
                        })
                    {
                        return Err(Refusal::Malformed(
                            "post-compile stage before authoritative work completion".to_owned(),
                        ));
                    }
                    latest = next;
                }
                JobEvent::Published(_) | JobEvent::Program(_) | JobEvent::Verdict(_) if !lead => {
                    return Err(Refusal::NotHolder);
                }
                JobEvent::Done(_) => {
                    if !lead {
                        return Err(Refusal::NotHolder);
                    }
                    if index + 1 != events.len() {
                        return Err(Refusal::Malformed("Done must be the last event".to_owned()));
                    }
                }
                _ => {}
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use blaze_wire::{
        AssignedEvent, DoneEvent, ExecutionId, JobContent, LocalPolicy, Namespace, Outcome,
        OutputEvent, RefusedEvent, RootSet, Set, StageEvent, WorkJoinRequest, WorkersEventsRequest,
    };

    use super::*;
    use crate::jobs::{Effect, ExecState, ExecutionConfig, LocalAction, Subscriber, SubscriberState};
    use crate::work::EndWhy;

    fn execution() -> Execution {
        let mut execution = Execution::new(
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
            ExecutionConfig::default(),
            &mut Vec::new(),
        );
        execution.assign(0, "lead", &["helper".to_owned()], &mut Vec::new()).unwrap();
        execution
    }

    fn stage(stage: Stage) -> JobEvent {
        // The coordinator must use its own time rather than this untrusted timestamp.
        JobEvent::Stage(StageEvent { stage, at_ms: u64::MAX })
    }

    fn send(execution: &mut Execution, from: &str, events: Vec<JobEvent>) -> Result<(), Refusal> {
        let request = WorkersEventsRequest {
            execution: execution.id,
            attempt: execution.attempt(),
            events,
        };
        execution.worker_events(1, from, &request, &mut Vec::new()).map(|_| ())
    }

    #[test]
    fn coordinator_stages_and_assignment_cannot_be_injected() {
        for from in ["lead", "helper"] {
            for reserved in [
                Stage::Queued,
                Stage::Admitted,
                Stage::Assigned,
                Stage::Compiled,
                Stage::Done,
            ] {
                let mut execution = execution();
                let before = execution.events(0).to_vec();
                assert_eq!(
                    send(&mut execution, from, vec![stage(reserved)]),
                    Err(Refusal::NotHolder)
                );
                assert_eq!(execution.events(0), before.as_slice());
            }
            let mut execution = execution();
            let assignment =
                AssignedEvent { executor: "other".into(), attempt: execution.attempt() };
            assert_eq!(
                send(&mut execution, from, vec![JobEvent::Assigned(assignment)]),
                Err(Refusal::NotHolder)
            );
        }
    }

    #[test]
    fn a_rejected_suffix_does_not_publish_its_prefix_or_poison_the_verdict() {
        let mut execution = execution();
        let before = execution.events(0).to_vec();
        let events = vec![
            JobEvent::Output(OutputEvent { stream: 1, bytes: b"not published".to_vec() }),
            JobEvent::Refused(RefusedEvent { function: "f".into(), why: "not committed".into() }),
            stage(Stage::Compiled),
        ];
        assert_eq!(send(&mut execution, "helper", events), Err(Refusal::NotHolder));
        assert_eq!(execution.events(0), before.as_slice());
        assert!(!execution.refused);
    }

    #[test]
    fn stages_are_monotone_deduplicated_and_recorded_in_both_views() {
        let mut execution = execution();
        send(&mut execution, "lead", vec![stage(Stage::Fetched), stage(Stage::Loaded)]).unwrap();
        let before = execution.events(0).to_vec();
        send(&mut execution, "lead", vec![stage(Stage::Loaded)]).unwrap();
        assert_eq!(execution.events(0), before.as_slice());
        assert!(send(&mut execution, "lead", vec![stage(Stage::Fetched)]).is_err());
        assert_eq!(execution.stages().last(), Some(&(Stage::Loaded, 1)));
        assert!(send(&mut execution, "helper", vec![stage(Stage::Loaded)]).is_err());
        assert!(send(&mut execution, "lead", vec![stage(Stage::Linked)]).is_err());

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
            rejected: Set::new(Vec::new()),
        };
        execution.join(1, "lead", &request, &mut Vec::new()).unwrap();
        send(&mut execution, "lead", vec![stage(Stage::Published), stage(Stage::Linked)]).unwrap();
        assert_eq!(execution.stages().last(), Some(&(Stage::Linked, 1)));
    }

    #[test]
    fn a_terminal_report_cannot_have_a_suffix_or_conflicting_second_verdict() {
        let mut execution = execution();
        let done = JobEvent::Done(DoneEvent { outcome: Outcome::Failed, counters: Vec::new() });
        let before = execution.events(0).to_vec();
        assert!(send(&mut execution, "lead", vec![done.clone(), done]).is_err());
        assert_eq!(execution.events(0), before.as_slice());
        assert_eq!(execution.state(), ExecState::Running);
    }

    #[test]
    fn no_lead_is_infrastructure_loss_in_subscriber_and_wire_delivery() {
        let mut execution = execution();
        let mut effects = Vec::new();
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
        let decoded: DoneEvent =
            blaze_wire::decode(&blaze_wire::encode(verdict), &blaze_wire::Limits::default())
                .unwrap();
        assert_eq!(decoded.outcome, Outcome::Lost);
        assert_eq!(
            super::super::local_action(&LocalPolicy::Fallback, &decoded.outcome),
            LocalAction::BuildLocally
        );
        assert_eq!(
            super::super::local_action(&LocalPolicy::Never, &decoded.outcome),
            LocalAction::Fail
        );
        assert_eq!(
            super::super::local_action(&LocalPolicy::Race, &decoded.outcome),
            LocalAction::BuildLocally
        );
        assert_eq!(
            super::super::local_action(&LocalPolicy::Fallback, &Outcome::Failed),
            LocalAction::Take
        );
    }
}
