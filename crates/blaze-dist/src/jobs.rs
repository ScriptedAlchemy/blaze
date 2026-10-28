//! Executions of jobs, as section 4's "Warm remote jobs" and unit E3 describe them.
//!
//! A job's content (`JobContent`, its `BuildSpec` and namespace) has at most one running
//! execution: an equal request joins it as another subscriber. Each subscriber has its own
//! deadline and `LocalPolicy`; the execution runs until its latest live subscriber's deadline,
//! and ends when its last subscriber leaves. A lead that misses three heartbeats loses the job:
//! it is reassigned once with a new `Attempt`, and the dead attempt's late events and work
//! calls are refused; a second loss ends it `Lost`. The work set of section 4 lives inside the
//! execution, so a reassignment fences its leases in the same transition.

use std::collections::{BTreeMap, BTreeSet};

use blaze_wire::job_terminal_v2::{
    SubscriberTerminalV2, TerminationReasonV2, TerminationV2, VERSION as TERMINAL_VERSION,
};
use blaze_wire::{
    AssignedEvent, Attempt, BatchResult, Digest, DoneEvent, EventItem, ExecutionId, JobAnswer,
    JobContent, JobContentInput, JobEvent, JobRequest, LocalPolicy, Namespace, Outcome,
    Reservation, SourceRef, Stage, StageEvent, WorkJoinAnswer, WorkJoinRequest, WorkLeaseAnswer,
    WorkLeaseRequest, WorkResultAnswer, WorkersEventsAnswer, WorkersEventsRequest,
};

use crate::admission::{Admission, Waiting};
use crate::work::{self, EndWhy, WorkConfig, WorkSet};
use crate::{Millis, Refusal};

mod ingress;

/// An execution's timings.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExecutionConfig {
    /// Executors heartbeat this often.
    pub heartbeat_ms: u64,
    /// A lead that misses this many heartbeats loses its job.
    pub missed_heartbeats: u64,
    /// The work set's.
    pub work: WorkConfig,
    /// Admission's explicit compile requirement. Never inferred from worker events.
    pub completion: CompletionMode,
}

/// Whether the admitted job needs distributed compile work.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CompletionMode {
    /// Success requires the current lead's agreed, completed work set.
    WorkProtocol,
    /// Admission established that no compile work is required (for example verified cached
    /// artifacts or an already-built test shard). The lead still reports the final verdict.
    /// This is a driver decision, not a worker claim or inference from an empty BuildSpec.
    NoCompileWork,
}

impl Default for ExecutionConfig {
    fn default() -> ExecutionConfig {
        ExecutionConfig {
            heartbeat_ms: 5_000,
            missed_heartbeats: 3,
            work: WorkConfig::default(),
            completion: CompletionMode::WorkProtocol,
        }
    }
}

/// One requester's interest in an execution.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Subscriber {
    /// Its id, the `JobAnswer`'s.
    pub id: u64,
    /// Its own deadline, ms since the epoch.
    pub deadline_ms: u64,
    /// What it does when the remote build does not answer in time.
    pub policy: LocalPolicy,
    /// Its test timeout.
    pub test_timeout_ms: Option<u64>,
    /// Where it is.
    pub state: SubscriberState,
}

/// A subscriber's state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SubscriberState {
    /// Waiting for the execution.
    Attached,
    /// It left; the execution runs on for the others.
    Left,
    /// Its own deadline passed: detached `TimedOut`, following its own `LocalPolicy`.
    TimedOut,
    /// The execution ended while it was attached.
    Ended,
}

/// Where an execution is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ExecState {
    /// Waiting for admission.
    Queued,
    /// Admitted and assigned, or waiting for a new lead after its lead was lost.
    Running,
    /// Ended, with its outcome.
    Ended(Outcome),
}

/// Identity of one assignment on an executor. A stop and its acknowledgment must name it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AssignmentToken {
    /// The execution assigned to the slot.
    pub execution: ExecutionId,
    /// Its attempt, including reassignments within the same execution.
    pub attempt: Attempt,
}

/// What an execution asks its driver to do, or tells it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Effect {
    /// An item was appended to the execution's event stream.
    Event(EventItem),
    /// The lead was lost: the driver assigns a new lead (admission's choice), or ends the
    /// execution `Lost` when there is none.
    NeedLead,
    /// Stop an executor's process group: the agent kills it.
    Kill {
        /// The executor to stop.
        executor: String,
        /// Stop only this assignment, never a newer one on the same executor.
        assignment: AssignmentToken,
    },
    /// A subscriber is done with the execution: its own outcome and policy.
    SubscriberEnded {
        /// The subscriber.
        subscriber: u64,
        /// Its outcome: the execution's, or `TimedOut` at its own deadline.
        outcome: Outcome,
        /// What it does next on its side.
        policy: LocalPolicy,
        /// The versioned delivery used by the terminal endpoint and client policy.
        terminal: SubscriberTerminalV2,
    },
    /// The work set's log.
    Work(work::Effect),
}

/// One execution of a `JobContent`.
#[derive(Clone, Debug)]
pub struct Execution {
    config: ExecutionConfig,
    /// The server's id of it.
    pub id: ExecutionId,
    /// What it builds, and for which namespace.
    pub content: JobContent,
    /// Its namespace.
    pub namespace: Namespace,
    attempt: Attempt,
    lead: Option<String>,
    helpers: BTreeSet<String>,
    lead_heard: Millis,
    heard: BTreeMap<String, Millis>,
    losses: u32,
    lost: BTreeSet<String>,
    work: Option<WorkSet>,
    subscribers: BTreeMap<u64, Subscriber>,
    events: Vec<EventItem>,
    stages: Vec<(Stage, Millis)>,
    compiled: Option<(Attempt, u64)>,
    terminals: BTreeMap<u64, SubscriberTerminalV2>,
    refused: bool,
    state: ExecState,
}

impl Execution {
    /// A queued execution with its first subscriber.
    pub fn new(
        now: Millis,
        id: ExecutionId,
        content: JobContent,
        namespace: Namespace,
        first: Subscriber,
        config: ExecutionConfig,
        out: &mut Vec<Effect>,
    ) -> Execution {
        let mut execution = Execution {
            config,
            id,
            content,
            namespace,
            attempt: Attempt(1),
            lead: None,
            helpers: BTreeSet::new(),
            lead_heard: now,
            heard: BTreeMap::new(),
            losses: 0,
            lost: BTreeSet::new(),
            work: None,
            subscribers: BTreeMap::from([(first.id, first)]),
            events: Vec::new(),
            stages: Vec::new(),
            compiled: None,
            terminals: BTreeMap::new(),
            refused: false,
            state: ExecState::Queued,
        };
        execution.stage(now, Stage::Queued, out);
        execution
    }

    /// Where it is.
    pub fn state(&self) -> ExecState {
        self.state.clone()
    }

    /// The immutable typed terminal endpoint response for a subscriber, once it ended.
    pub fn terminal(&self, subscriber: u64) -> Option<&SubscriberTerminalV2> {
        self.terminals.get(&subscriber)
    }

    /// Its current attempt.
    pub fn attempt(&self) -> Attempt {
        self.attempt
    }

    /// The token used by the driver to fence assignment stops and acknowledgments.
    pub fn assignment(&self) -> AssignmentToken {
        AssignmentToken { execution: self.id, attempt: self.attempt }
    }

    /// The current attempt's lead.
    pub fn lead(&self) -> Option<&str> {
        self.lead.as_deref()
    }

    /// The executors of the current attempt.
    pub fn executors(&self) -> impl Iterator<Item = &str> {
        self.lead.iter().map(String::as_str).chain(self.helpers.iter().map(String::as_str))
    }

    /// How many times its lead was lost.
    pub fn losses(&self) -> u32 {
        self.losses
    }

    /// The work set, once assigned.
    pub fn work(&self) -> Option<&WorkSet> {
        self.work.as_ref()
    }

    /// Its stage timestamps, in the order recorded.
    pub fn stages(&self) -> &[(Stage, Millis)] {
        &self.stages
    }

    /// Its subscribers.
    pub fn subscribers(&self) -> impl Iterator<Item = &Subscriber> {
        self.subscribers.values()
    }

    /// The execution's deadline: the latest of its attached subscribers'.
    pub fn deadline(&self) -> Option<u64> {
        self.attached().map(|s| s.deadline_ms).max()
    }

    fn attached(&self) -> impl Iterator<Item = &Subscriber> {
        self.subscribers.values().filter(|s| s.state == SubscriberState::Attached)
    }

    /// Executors heard from within the lead-loss window: who a reassignment may pick.
    pub fn heard_recently(&self, now: Millis) -> Vec<String> {
        let window = self.config.heartbeat_ms * self.config.missed_heartbeats;
        self.heard
            .iter()
            .filter(|&(id, &at)| {
                now <= at + window && Some(id) != self.lead.as_ref() && !self.lost.contains(id)
            })
            .map(|(id, _)| id.clone())
            .collect()
    }

    /// Another subscriber of the same content.
    pub fn subscribe(&mut self, subscriber: Subscriber) -> Result<(), Refusal> {
        if let ExecState::Ended(_) = self.state {
            return Err(Refusal::Ended);
        }
        self.subscribers.insert(subscriber.id, subscriber);
        Ok(())
    }

    /// `DELETE /v1/jobs/<execution>/subscribers/<id>`: the execution is cancelled when its
    /// last subscriber leaves.
    pub fn unsubscribe(
        &mut self,
        now: Millis,
        id: u64,
        out: &mut Vec<Effect>,
    ) -> Result<(), Refusal> {
        let Some(subscriber) = self.subscribers.get_mut(&id) else {
            return Err(Refusal::NoSubscriber);
        };
        if subscriber.state == SubscriberState::Attached {
            subscriber.state = SubscriberState::Left;
            self.terminals.insert(
                id,
                SubscriberTerminalV2 {
                    execution: self.id,
                    attempt: self.attempt,
                    subscriber: id,
                    termination: TerminationV2 {
                        version: TERMINAL_VERSION,
                        outcome: Outcome::Cancelled,
                        reason: TerminationReasonV2::Cancelled,
                    },
                },
            );
        }
        if self.attached().next().is_none() && !matches!(self.state, ExecState::Ended(_)) {
            self.end(now, Outcome::Cancelled, EndWhy::Cancelled, out);
        }
        Ok(())
    }

    /// Admission let it run.
    pub fn admitted(&mut self, now: Millis, out: &mut Vec<Effect>) {
        self.stage(now, Stage::Admitted, out);
    }

    /// The first assignment, or the reassignment after a lead was lost: a new attempt whose
    /// lead is `lead`. Every lease of the old attempt is fenced.
    pub fn assign(
        &mut self,
        now: Millis,
        lead: &str,
        helpers: &[String],
        out: &mut Vec<Effect>,
    ) -> Result<(), Refusal> {
        if let ExecState::Ended(_) = self.state {
            return Err(Refusal::Ended);
        }
        let previous = self.assignment();
        if self.lead.is_some() || self.work.is_some() {
            self.attempt = Attempt(self.attempt.0 + 1);
        }
        for old in self.executors().map(str::to_owned).collect::<Vec<_>>() {
            if old != lead && !helpers.contains(&old) {
                out.push(Effect::Kill { executor: old, assignment: previous });
            }
        }
        self.lead = Some(lead.to_owned());
        self.helpers = helpers.iter().filter(|h| *h != lead).cloned().collect();
        self.lead_heard = now;
        self.heard.insert(lead.to_owned(), now);
        let mut log = Vec::new();
        match &mut self.work {
            None => {
                self.work =
                    Some(WorkSet::new(self.id, self.attempt, lead, self.config.work.clone()));
            }
            Some(work) => work.new_attempt(self.attempt, lead, &mut log),
        }
        out.extend(log.into_iter().map(Effect::Work));
        self.state = ExecState::Running;
        self.stage(now, Stage::Assigned, out);
        self.push(
            JobEvent::Assigned(AssignedEvent { executor: lead.to_owned(), attempt: self.attempt }),
            out,
        );
        Ok(())
    }

    /// Another helper for the current attempt: an idle executor of the right trust, platform
    /// and front end, as admission offers it (non-blocking: a lead never waits for one). Never
    /// a lead this execution lost.
    pub fn add_helper(&mut self, now: Millis, executor: &str) -> Result<Attempt, Refusal> {
        if let ExecState::Ended(_) = self.state {
            return Err(Refusal::Ended);
        }
        if self.lost.contains(executor) {
            return Err(Refusal::NotHolder);
        }
        if self.lead.as_deref() != Some(executor) {
            self.helpers.insert(executor.to_owned());
        }
        self.heard.insert(executor.to_owned(), now);
        Ok(self.attempt)
    }

    fn member(&self, from: &str) -> Result<(), Refusal> {
        if let ExecState::Ended(_) = self.state {
            return Err(Refusal::Ended);
        }
        if self.executors().any(|e| e == from) { Ok(()) } else { Err(Refusal::NotHolder) }
    }

    fn work_call<T>(
        &mut self,
        now: Millis,
        from: &str,
        attempt: Attempt,
        out: &mut Vec<Effect>,
        call: impl FnOnce(&mut WorkSet, &mut Vec<work::Effect>) -> Result<T, Refusal>,
    ) -> Result<T, Refusal> {
        // A capability names its attempt: a dead attempt's calls are refused as such before
        // anything else, so its executor stops rather than errs.
        if let ExecState::Ended(_) = self.state {
            return Err(Refusal::Ended);
        }
        if attempt != self.attempt {
            return Err(Refusal::AttemptMoved);
        }
        self.member(from)?;
        if self.config.completion == CompletionMode::NoCompileWork {
            return Err(Refusal::Malformed("work call in no-compile-work execution".to_owned()));
        }
        self.heard.insert(from.to_owned(), now);
        let work = self.work.as_mut().ok_or(Refusal::NotJoined)?;
        let mut log = Vec::new();
        let answer = call(work, &mut log);
        out.extend(log.into_iter().map(Effect::Work));
        self.after_work(now, out);
        answer
    }

    /// `POST /v1/work/join`.
    pub fn join(
        &mut self,
        now: Millis,
        from: &str,
        req: &WorkJoinRequest,
        out: &mut Vec<Effect>,
    ) -> Result<WorkJoinAnswer, Refusal> {
        self.work_call(now, from, req.attempt, out, |work, log| work.join(now, from, req, log))
    }

    /// `POST /v1/work/lease`.
    pub fn lease(
        &mut self,
        now: Millis,
        from: &str,
        req: &WorkLeaseRequest,
        out: &mut Vec<Effect>,
    ) -> Result<WorkLeaseAnswer, Refusal> {
        self.work_call(now, from, req.attempt, out, |work, log| work.lease(now, from, req, log))
    }

    /// `POST /v1/work/result`.
    pub fn result(
        &mut self,
        now: Millis,
        from: &str,
        req: &BatchResult,
        out: &mut Vec<Effect>,
    ) -> Result<WorkResultAnswer, Refusal> {
        self.work_call(now, from, req.lease.attempt, out, |work, log| {
            work.result(now, from, req, log)
        })
    }

    /// `POST /v1/workers/events`: a heartbeat, and events for the stream. A dead attempt's are
    /// refused. A `Done` from the lead ends the execution with its outcome; a `Refused` before
    /// it makes that outcome `Refused`, whatever the lead says, since a job is never turned
    /// green past a refusal. The entire event batch is validated before any report is published.
    pub fn worker_events(
        &mut self,
        now: Millis,
        from: &str,
        req: &WorkersEventsRequest,
        out: &mut Vec<Effect>,
    ) -> Result<WorkersEventsAnswer, Refusal> {
        if req.execution != self.id {
            return Err(Refusal::NoExecution);
        }
        if let ExecState::Ended(_) = self.state {
            return Ok(WorkersEventsAnswer::CANCEL);
        }
        if req.attempt != self.attempt {
            return Err(Refusal::AttemptMoved);
        }
        self.member(from)?;
        // Validate the entire batch before it changes liveness, refusal state or the stream.
        // A rejected tail must not leave an accepted prefix in the public event log.
        let lead = self.lead.as_deref() == Some(from);
        let done = self.validate_events(now, lead, &req.events)?;
        self.expire_subscribers(now, out);
        if self.attached().next().is_none() {
            self.end(now, Outcome::Cancelled, EndWhy::Deadline, out);
            return Ok(WorkersEventsAnswer::CANCEL);
        }
        self.heard.insert(from.to_owned(), now);
        if lead {
            self.lead_heard = now;
        }
        for event in &req.events {
            match event {
                JobEvent::Refused(_) => self.refused = true,
                JobEvent::Stage(event) => {
                    // Use the coordinator's clock and update the same stage view checked by
                    // the model. Retransmission of the latest stage does not append it twice.
                    if self.reported_stage() != Some(&event.stage) {
                        self.stage(now, event.stage.clone(), out);
                    }
                    continue;
                }
                JobEvent::Done(_) => {
                    // Publish only the coordinator's final verdict, once. A raw Passed
                    // here could otherwise escape into replay before refusal normalization.
                    continue;
                }
                _ => {}
            }
            self.push(event.clone(), out);
        }
        if let Some(outcome) = done {
            let outcome =
                if self.refused && outcome == Outcome::Passed { Outcome::Refused } else { outcome };
            self.end(now, outcome, EndWhy::Reported, out);
            return Ok(WorkersEventsAnswer::CANCEL);
        }
        let mut log = Vec::new();
        let answer = match &mut self.work {
            Some(work) => work.heartbeat(now, from, req.attempt, &mut log),
            None => WorkersEventsAnswer::CONTINUE,
        };
        out.extend(log.into_iter().map(Effect::Work));
        self.after_work(now, out);
        Ok(answer)
    }

    /// Current-attempt/root-epoch authority, not a worker's Compiled notification.
    fn success_ready(&self) -> bool {
        self.config.completion == CompletionMode::NoCompileWork
            || self.work.as_ref().is_some_and(|work| {
                work.is_done()
                    && work.lead_joined()
                    && self.compiled == Some((self.attempt, work.root_epoch()))
            })
    }

    /// `GET /v1/jobs/<execution>/events?from=N`: the items after sequence `from`.
    pub fn events(&self, from: u64) -> &[EventItem] {
        let start = usize::try_from(from).unwrap_or(usize::MAX).min(self.events.len());
        &self.events[start..]
    }

    /// Ends what has run out of time: subscribers past their deadline, the execution past the
    /// latest, a lead that missed its heartbeats, and the work set's leases.
    pub fn tick(&mut self, now: Millis, out: &mut Vec<Effect>) {
        if let ExecState::Ended(_) = self.state {
            return;
        }
        self.expire_subscribers(now, out);
        if self.attached().next().is_none() {
            // The latest deadline passed: the execution ends, its coverage not claimed.
            return self.end(now, Outcome::Cancelled, EndWhy::Deadline, out);
        }
        if self.state != ExecState::Running {
            return;
        }
        let window = self.config.heartbeat_ms * self.config.missed_heartbeats;
        if self.lead.is_some() && now > self.lead_heard + window {
            let lost = self.lead.take().expect("lead");
            self.lost.insert(lost.clone());
            out.push(Effect::Kill { executor: lost, assignment: self.assignment() });
            self.losses += 1;
            if self.losses >= 2 {
                return self.end(now, Outcome::Lost, EndWhy::LeadLostTwice, out);
            }
            out.push(Effect::NeedLead);
            return;
        }
        if let Some(work) = &mut self.work {
            let mut log = Vec::new();
            work.tick(now, &mut log);
            out.extend(log.into_iter().map(Effect::Work));
        }
        self.after_work(now, out);
    }

    /// When the compile phase is done, the stream says so: the lead reads it and assembles.
    fn after_work(&mut self, now: Millis, out: &mut Vec<Effect>) {
        if let Some(work) = &self.work
            && work.is_done()
            && work.lead_joined()
            && self.compiled != Some((self.attempt, work.root_epoch()))
        {
            // A replacement lead must first check the retained root set. It then gets its
            // own notification, including when a changed root list reopened completed work.
            self.compiled = Some((self.attempt, work.root_epoch()));
            self.stage(now, Stage::Compiled, out);
        }
    }

    /// Ends the execution: the work set drops its leases, every executor is stopped, and every
    /// attached subscriber gets the outcome. Deadlines apply even when a completion arrives
    /// before the driver's next timer tick.
    pub(crate) fn end(
        &mut self,
        now: Millis,
        mut outcome: Outcome,
        mut why: EndWhy,
        out: &mut Vec<Effect>,
    ) {
        if let ExecState::Ended(_) = self.state {
            return;
        }
        // No available replacement lead is infrastructure loss, not a compiler/test verdict.
        // Preserve that distinction in both subscriber effects and the serialized Done event.
        if matches!(&why, EndWhy::NoLead) {
            outcome = Outcome::Lost;
        }
        if self.expire_subscribers(now, out) && self.attached().next().is_none() {
            outcome = Outcome::Cancelled;
            why = EndWhy::Deadline;
        }
        self.state = ExecState::Ended(outcome.clone());
        if let Some(work) = &mut self.work {
            let mut log = Vec::new();
            work.end(why, &mut log);
            out.extend(log.into_iter().map(Effect::Work));
        }
        self.finish(now, outcome, why, out);
    }

    /// Cancel an execution administratively, preserving a typed cancellation for subscribers.
    pub fn cancel(&mut self, now: Millis, out: &mut Vec<Effect>) {
        self.end(now, Outcome::Cancelled, EndWhy::Cancelled, out);
    }

    /// Detaches subscribers whose own deadlines have passed, once, before either a timer or
    /// a final result can report an outcome to them.
    fn expire_subscribers(&mut self, now: Millis, out: &mut Vec<Effect>) -> bool {
        let late: Vec<u64> =
            self.attached().filter(|s| now > s.deadline_ms).map(|s| s.id).collect();
        let expired = !late.is_empty();
        for id in late {
            let subscriber = self.subscribers.get_mut(&id).expect("attached");
            subscriber.state = SubscriberState::TimedOut;
            let terminal = SubscriberTerminalV2 {
                execution: self.id,
                attempt: self.attempt,
                subscriber: id,
                termination: TerminationV2 {
                    version: TERMINAL_VERSION,
                    outcome: Outcome::TimedOut,
                    reason: TerminationReasonV2::SubscriberDeadline,
                },
            };
            self.terminals.insert(id, terminal.clone());
            out.push(Effect::SubscriberEnded {
                subscriber: id,
                outcome: Outcome::TimedOut,
                policy: subscriber.policy.clone(),
                terminal,
            });
        }
        expired
    }

    fn finish(&mut self, now: Millis, outcome: Outcome, why: EndWhy, out: &mut Vec<Effect>) {
        for executor in self.executors().map(str::to_owned).collect::<Vec<_>>() {
            out.push(Effect::Kill { executor, assignment: self.assignment() });
        }
        for subscriber in self.subscribers.values_mut() {
            if subscriber.state == SubscriberState::Attached {
                subscriber.state = SubscriberState::Ended;
                let terminal = SubscriberTerminalV2 {
                    execution: self.id,
                    attempt: self.attempt,
                    subscriber: subscriber.id,
                    termination: TerminationV2 {
                        version: TERMINAL_VERSION,
                        outcome: outcome.clone(),
                        reason: match why {
                            EndWhy::Reported => TerminationReasonV2::Reported,
                            EndWhy::NoLead => TerminationReasonV2::NoLead,
                            EndWhy::LeadLostTwice => TerminationReasonV2::LeadLostTwice,
                            EndWhy::Cancelled => TerminationReasonV2::Cancelled,
                            EndWhy::Deadline => TerminationReasonV2::Deadline,
                        },
                    },
                };
                self.terminals.insert(subscriber.id, terminal.clone());
                out.push(Effect::SubscriberEnded {
                    subscriber: subscriber.id,
                    outcome: outcome.clone(),
                    policy: subscriber.policy.clone(),
                    terminal,
                });
            }
        }
        self.stage(now, Stage::Done, out);
        self.push(JobEvent::Done(DoneEvent { outcome, counters: Vec::new() }), out);
    }

    fn stage(&mut self, now: Millis, stage: Stage, out: &mut Vec<Effect>) {
        self.stages.push((stage.clone(), now));
        self.push(JobEvent::Stage(StageEvent { stage, at_ms: now }), out);
    }

    fn push(&mut self, event: JobEvent, out: &mut Vec<Effect>) {
        let item =
            EventItem { sequence: self.events.len() as u64 + 1, attempt: self.attempt, event };
        self.events.push(item.clone());
        out.push(Effect::Event(item));
    }
}

/// What a requester does when its subscription ends, by its `LocalPolicy`: section 4's
/// "Failure" paragraph, the client's side of it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LocalAction {
    /// Take the remote outcome.
    Take,
    /// Build locally, inside the same no-rustc boundary.
    BuildLocally,
    /// Fail the job, naming the remote outcome.
    Fail,
}

/// The action for a versioned, classified terminal delivery under `policy`. `Passed`, `Failed`
/// and `Refused` are taken as verdicts; other outcomes fail under `Never` and build locally
/// under `Fallback`. `Race` was building locally from the start, and the first verdict wins,
/// a build error or failed test included.
///
/// Unknown versions or inconsistent outcome/reason pairs are refused, never guessed from
/// the v1 stream. `NoLead` is delivered as `Lost` in both v1 and v2, with its explicit reason
/// retained in v2 rather than guessed from that outcome.
pub fn local_action(policy: &LocalPolicy, remote: &TerminationV2) -> Result<LocalAction, Refusal> {
    if !remote.is_valid() {
        return Err(Refusal::Malformed("invalid terminal contract".to_owned()));
    }
    let verdict = remote.reason == TerminationReasonV2::Reported;
    Ok(match policy {
        LocalPolicy::Never => {
            if verdict {
                LocalAction::Take
            } else {
                LocalAction::Fail
            }
        }
        LocalPolicy::Fallback => {
            if verdict {
                LocalAction::Take
            } else {
                LocalAction::BuildLocally
            }
        }
        LocalPolicy::Race => {
            if verdict {
                LocalAction::Take
            } else {
                // The local build is already running; the race is its to win.
                LocalAction::BuildLocally
            }
        }
    })
}

/// The server's executions: one per running `JobContent`, admitted in section 5's order.
#[derive(Clone, Debug)]
pub struct Jobs {
    config: ExecutionConfig,
    admission: Admission,
    executions: BTreeMap<ExecutionId, Execution>,
    running: BTreeMap<JobContent, ExecutionId>,
    platforms: BTreeMap<ExecutionId, (Digest, Reservation)>,
    pr_jobs: BTreeMap<u64, PrJobs>,
    assignments: BTreeMap<String, (AssignmentToken, bool)>,
    next_execution: u64,
    next_subscriber: u64,
}

#[derive(Clone, Debug)]
struct PrJobs {
    /// The source of the most recently submitted revision; platforms and profiles may differ.
    source: SourceRef,
    /// Live executions of this pull request, including older revisions kept by subscribers.
    executions: BTreeMap<ExecutionId, SourceRef>,
}

impl Jobs {
    /// No execution yet, over `admission`'s machines.
    pub fn new(admission: Admission, config: ExecutionConfig) -> Jobs {
        Jobs {
            config,
            admission,
            executions: BTreeMap::new(),
            running: BTreeMap::new(),
            platforms: BTreeMap::new(),
            pr_jobs: BTreeMap::new(),
            assignments: BTreeMap::new(),
            next_execution: 1,
            next_subscriber: 1,
        }
    }

    /// The registry's compile bounds, for a carrier's frame-size preflight.
    pub fn config(&self) -> &ExecutionConfig {
        &self.config
    }

    /// Admission, for its operator calls: machines, builders, rebuilds, closed pull requests.
    pub fn admission(&mut self) -> &mut Admission {
        &mut self.admission
    }

    /// Admission, to read.
    pub fn admission_view(&self) -> &Admission {
        &self.admission
    }

    /// One execution.
    pub fn execution(&self, id: ExecutionId) -> Option<&Execution> {
        self.executions.get(&id)
    }

    /// Typed response for the additive v2 subscriber-terminal endpoint. A v1 Done event is
    /// not enough to synthesize it: the outcome omits the explicit termination reason.
    pub fn terminal(
        &self,
        execution: ExecutionId,
        subscriber: u64,
    ) -> Option<&SubscriberTerminalV2> {
        self.executions.get(&execution)?.terminal(subscriber)
    }

    /// Every execution.
    pub fn executions(&self) -> impl Iterator<Item = &Execution> {
        self.executions.values()
    }

    /// Attach another subscriber to this exact running execution, not a new build.
    pub fn subscribe(
        &mut self,
        now: Millis,
        execution: ExecutionId,
        content: JobContent,
        deadline_ms: Millis,
        policy: LocalPolicy,
        out: &mut Vec<(ExecutionId, Effect)>,
    ) -> Result<JobAnswer, Refusal> {
        let e = self.executions.get(&execution).ok_or(Refusal::NoExecution)?;
        if e.content != content {
            return Err(Refusal::NoExecution);
        }
        self.tick(now, out);
        let e = self.executions.get_mut(&execution).ok_or(Refusal::NoExecution)?;
        if e.content != content {
            return Err(Refusal::NoExecution);
        }
        let id = self.next_subscriber;
        e.subscribe(Subscriber {
            id,
            deadline_ms,
            policy,
            test_timeout_ms: None,
            state: SubscriberState::Attached,
        })?;
        self.next_subscriber += 1;
        Ok(JobAnswer { content, execution, subscriber: id })
    }

    fn work_route<T>(
        &mut self,
        now: Millis,
        execution: ExecutionId,
        out: &mut Vec<(ExecutionId, Effect)>,
        call: impl FnOnce(&mut Execution, &mut Vec<Effect>) -> Result<T, Refusal>,
    ) -> Result<T, Refusal> {
        let e = self.executions.get_mut(&execution).ok_or(Refusal::NoExecution)?;
        let mut log = Vec::new();
        let result = call(e, &mut log);
        self.after(now, execution, log, out);
        result
    }

    /// Route the actual work-join call through the registry's lifecycle/effect tracking.
    pub fn join(
        &mut self,
        now: Millis,
        from: &str,
        req: &WorkJoinRequest,
        out: &mut Vec<(ExecutionId, Effect)>,
    ) -> Result<WorkJoinAnswer, Refusal> {
        self.work_route(now, req.execution, out, |e, log| e.join(now, from, req, log))
    }

    /// Route the actual work-lease call through the registry.
    pub fn lease(
        &mut self,
        now: Millis,
        from: &str,
        req: &WorkLeaseRequest,
        out: &mut Vec<(ExecutionId, Effect)>,
    ) -> Result<WorkLeaseAnswer, Refusal> {
        self.work_route(now, req.execution, out, |e, log| e.lease(now, from, req, log))
    }

    /// Route an uploaded batch's result through the registry.
    pub fn result(
        &mut self,
        now: Millis,
        from: &str,
        req: &BatchResult,
        out: &mut Vec<(ExecutionId, Effect)>,
    ) -> Result<WorkResultAnswer, Refusal> {
        self.work_route(now, req.lease.execution, out, |e, log| e.result(now, from, req, log))
    }

    /// Forget a terminal execution only once all assignment-stop acknowledgments arrived.
    /// A transport supplies its own retention time and calls this after it elapsed.
    pub fn forget_terminal(&mut self, execution: ExecutionId) -> bool {
        if self.assignments.values().any(|(token, _)| token.execution == execution)
            || self
                .executions
                .get(&execution)
                .is_none_or(|e| !matches!(e.state(), ExecState::Ended(_)))
        {
            return false;
        }
        self.executions.remove(&execution);
        self.platforms.remove(&execution);
        true
    }

    /// `POST /v1/jobs`, with the reservation the server measured for its lead. An equal
    /// `JobContent` running already is joined, not started again. A pull request's new push
    /// cancels earlier-source executions when nothing else subscribes to them. Platform,
    /// command and profile variants of the same source run together.
    pub fn submit(
        &mut self,
        now: Millis,
        req: &JobRequest,
        need: Reservation,
        out: &mut Vec<(ExecutionId, Effect)>,
    ) -> Result<JobAnswer, (Refusal, Option<u64>)> {
        let content = JobContent::of(&JobContentInput {
            spec: blaze_wire::digest_of(&req.spec),
            namespace: req.namespace.clone(),
        });
        let subscriber = Subscriber {
            id: self.next_subscriber,
            deadline_ms: req.deadline_ms,
            policy: req.local.clone(),
            test_timeout_ms: req.test_timeout_ms,
            state: SubscriberState::Attached,
        };
        if let Some(&id) = self.running.get(&content) {
            self.executions
                .get_mut(&id)
                .expect("running")
                .subscribe(subscriber)
                .map_err(|r| (r, None))?;
            self.next_subscriber += 1;
            return Ok(JobAnswer { content, execution: id, subscriber: self.next_subscriber - 1 });
        }
        let id = ExecutionId(self.next_execution);
        let waiting = Waiting {
            execution: id,
            namespace: req.namespace.clone(),
            platform: req.spec.platform,
            need: need.clone(),
            at: now,
        };
        self.admission.enqueue(waiting).map_err(|(r, retry)| (r, Some(retry)))?;
        self.next_execution += 1;
        self.next_subscriber += 1;
        let mut log = Vec::new();
        let execution = Execution::new(
            now,
            id,
            content,
            req.namespace.clone(),
            subscriber.clone(),
            self.config.clone(),
            &mut log,
        );
        out.extend(log.into_iter().map(|e| (id, e)));
        self.executions.insert(id, execution);
        self.running.insert(content, id);
        self.platforms.insert(id, (req.spec.platform, need));
        self.pr_submitted(now, req, id, out);
        Ok(JobAnswer { content, execution: id, subscriber: subscriber.id })
    }

    fn pr_submitted(
        &mut self,
        now: Millis,
        req: &JobRequest,
        id: ExecutionId,
        out: &mut Vec<(ExecutionId, Effect)>,
    ) {
        let Namespace::Pr(pr) = &req.namespace else { return };
        let jobs = self.pr_jobs.entry(pr.n).or_insert_with(|| PrJobs {
            source: req.spec.source.clone(),
            executions: BTreeMap::new(),
        });
        jobs.executions.insert(id, req.spec.source.clone());
        if jobs.source == req.spec.source {
            return;
        }
        jobs.source = req.spec.source.clone();
        let previous: Vec<_> = jobs
            .executions
            .iter()
            .filter(|(_, source)| **source != req.spec.source)
            .map(|(&id, _)| id)
            .collect();
        for previous in previous {
            if let Some(old) = self.executions.get(&previous)
                && !matches!(old.state(), ExecState::Ended(_))
                && old.attached().count() == 1
            {
                let only = old.attached().next().expect("one").id;
                let _ = self.unsubscribe(now, previous, only, out);
            }
        }
    }

    /// `DELETE /v1/jobs/<execution>/subscribers/<id>`.
    pub fn unsubscribe(
        &mut self,
        now: Millis,
        execution: ExecutionId,
        subscriber: u64,
        out: &mut Vec<(ExecutionId, Effect)>,
    ) -> Result<(), Refusal> {
        let e = self.executions.get_mut(&execution).ok_or(Refusal::NoExecution)?;
        let mut log = Vec::new();
        let answer = e.unsubscribe(now, subscriber, &mut log);
        self.after(now, execution, log, out);
        answer
    }

    /// `POST /v1/workers/events` for `execution`.
    pub fn worker_events(
        &mut self,
        now: Millis,
        from: &str,
        req: &WorkersEventsRequest,
        out: &mut Vec<(ExecutionId, Effect)>,
    ) -> Result<WorkersEventsAnswer, Refusal> {
        let e = self.executions.get_mut(&req.execution).ok_or(Refusal::NoExecution)?;
        let mut log = Vec::new();
        let answer = e.worker_events(now, from, req, &mut log);
        self.after(now, req.execution, log, out);
        answer
    }

    /// The agent stopped `executor`'s job processes after a kill: `alive` says whether the
    /// executor itself is still there (an execution ended) or gone (it had died). Only now is
    /// its reservation returned, so a lead presumed lost but still running holds its share.
    /// The acknowledgement names the killed assignment: a duplicate or late one cannot free
    /// a newer execution or attempt on the same executor.
    pub fn killed(
        &mut self,
        now: Millis,
        executor: &str,
        assignment: AssignmentToken,
        alive: bool,
    ) -> bool {
        // An acknowledgment is meaningful only for a stop we requested on this assignment.
        if self.assignments.get(executor) != Some(&(assignment, true)) {
            return false;
        }
        self.assignments.remove(executor);
        if self
            .admission
            .executor(executor)
            .is_none_or(|slot| slot.busy != Some((assignment.execution, assignment.attempt)))
        {
            return false;
        }
        if alive {
            self.admission.finished(now, executor, assignment.execution, assignment.attempt);
        } else {
            self.admission.lost(now, executor);
        }
        true
    }

    /// Time passed: executions' deadlines and lead losses, reassignments, then admission.
    pub fn tick(&mut self, now: Millis, out: &mut Vec<(ExecutionId, Effect)>) {
        let ids: Vec<ExecutionId> = self.executions.keys().copied().collect();
        for id in ids {
            let e = self.executions.get_mut(&id).expect("present");
            if matches!(e.state(), ExecState::Ended(_)) {
                continue;
            }
            let mut log = Vec::new();
            e.tick(now, &mut log);
            self.after(now, id, log, out);
        }
        for admitted in self.admission.admit(now) {
            let e = self.executions.get_mut(&admitted.execution).expect("queued");
            let mut log = Vec::new();
            e.admitted(now, &mut log);
            if e.assign(now, &admitted.executor, &[], &mut log).is_ok() {
                self.assignments.insert(admitted.executor.clone(), (e.assignment(), false));
            }
            self.after(now, admitted.execution, log, out);
        }
    }

    fn track_stop(&mut self, effect: &Effect) {
        if let Effect::Kill { executor, assignment } = effect
            && let Some((current, stopping)) = self.assignments.get_mut(executor)
            && current == assignment
        {
            *stopping = true;
        }
    }

    fn after(
        &mut self,
        now: Millis,
        id: ExecutionId,
        log: Vec<Effect>,
        out: &mut Vec<(ExecutionId, Effect)>,
    ) {
        for effect in log {
            if effect == Effect::NeedLead {
                let e = &self.executions[&id];
                let except: Vec<String> = e.lost.iter().cloned().collect();
                let (platform, need) = self.platforms[&id].clone();
                let namespace = e.namespace.clone();
                let mut more = Vec::new();
                let e = self.executions.get_mut(&id).expect("present");
                match self.admission.idle_for(&namespace, &platform, &need, &except) {
                    Some(lead) => {
                        if e.assign(now, &lead, &[], &mut more).is_ok() {
                            self.admission.take(&lead, id, e.attempt());
                            self.assignments.insert(lead, (e.assignment(), false));
                        }
                    }
                    None => e.end(now, Outcome::Lost, EndWhy::NoLead, &mut more),
                }
                out.push((id, effect));
                for effect in more {
                    self.track_stop(&effect);
                    out.push((id, effect));
                }
                continue;
            }
            self.track_stop(&effect);
            out.push((id, effect));
        }
        let e = &self.executions[&id];
        if let ExecState::Ended(_) = e.state() {
            if self.running.get(&e.content) == Some(&id) {
                self.running.remove(&e.content);
            }
            self.admission.dequeue(id);
            if let Namespace::Pr(pr) = &e.namespace
                && let Some(jobs) = self.pr_jobs.get_mut(&pr.n)
            {
                jobs.executions.remove(&id);
                // Supersession only needs the source while a PR still has live work.
                // Retaining empty entries makes bounded execution retention leak one
                // source record for every PR ever served.
                if jobs.executions.is_empty() {
                    self.pr_jobs.remove(&pr.n);
                }
            }
        }
    }
}

#[cfg(test)]
mod retention_tests {
    use super::*;
    use crate::admission::AdmissionConfig;
    use blaze_wire::{BuildSpec, Command, PrNamespace, ProfileSpec, RootRef, Set, StoreIdentity};

    fn request(pr: u64, source: u8) -> JobRequest {
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
                },
                platform: [1; 32],
                rustflags: Vec::new(),
                config_env: Set::new(Vec::new()),
                script_env: Set::new(Vec::new()),
                cwd: String::new(),
                command: Command::Build,
                toolchain: StoreIdentity([2; 32]),
            },
            namespace: Namespace::Pr(PrNamespace { n: pr }),
            deadline_ms: 100_000,
            local: LocalPolicy::Never,
            test_timeout_ms: None,
        }
    }

    #[test]
    fn ending_pr_executions_releases_the_source_index_and_preserves_live_siblings() {
        let mut jobs =
            Jobs::new(Admission::new(AdmissionConfig::default()), ExecutionConfig::default());
        let need = Reservation { memory: 4, cpus: 4, disk: 4 };
        for pr in 1..=128 {
            let mut effects = Vec::new();
            let first = jobs.submit(0, &request(pr, 1), need.clone(), &mut effects).unwrap();
            let mut other_platform = request(pr, 1);
            other_platform.spec.platform = [3; 32];
            let sibling = jobs.submit(0, &other_platform, need.clone(), &mut effects).unwrap();
            jobs.unsubscribe(1, first.execution, first.subscriber, &mut effects).unwrap();
            assert_eq!(jobs.pr_jobs[&pr].executions.len(), 1);
            assert!(jobs.pr_jobs[&pr].executions.contains_key(&sibling.execution));

            let next = jobs.submit(2, &request(pr, 2), need.clone(), &mut effects).unwrap();
            assert_eq!(
                jobs.execution(sibling.execution).unwrap().state(),
                ExecState::Ended(Outcome::Cancelled)
            );
            assert_eq!(jobs.pr_jobs[&pr].executions.len(), 1);
            assert!(jobs.pr_jobs[&pr].executions.contains_key(&next.execution));
            jobs.unsubscribe(3, next.execution, next.subscriber, &mut effects).unwrap();
            for execution in [first.execution, sibling.execution, next.execution] {
                assert!(jobs.forget_terminal(execution));
            }
            assert_eq!(jobs.executions().count(), 0);
            assert!(jobs.pr_jobs.is_empty(), "finished PR {pr} retained its source index");
        }
    }
}
