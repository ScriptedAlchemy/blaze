//! Lifecycle boundaries that must hold independently of the randomized load.

use blaze_dist::jobs::{
    Effect, ExecState, Execution, ExecutionConfig, Subscriber, SubscriberState,
};
use blaze_dist::work::EndWhy;
use blaze_wire::{
    Attempt, DoneEvent, ExecutionId, JobContent, JobEvent, LocalPolicy, Namespace, Outcome,
    RefusedEvent, WorkersEventsRequest,
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
