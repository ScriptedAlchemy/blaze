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

use blaze_wire::{
    AssignedEvent, Attempt, BatchResult, Digest, DoneEvent, EventItem, ExecutionId, JobAnswer,
    JobContent, JobContentInput, JobEvent, JobRequest, LocalPolicy, Namespace, Outcome,
    Reservation, Stage, StageEvent, WorkJoinAnswer, WorkJoinRequest, WorkLeaseAnswer,
    WorkLeaseRequest, WorkResultAnswer, WorkersEventsAnswer, WorkersEventsRequest,
};

use crate::admission::{Admission, Waiting};
use crate::work::{self, EndWhy, WorkConfig, WorkSet};
use crate::{Millis, Refusal};

/// An execution's timings.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExecutionConfig {
    /// Executors heartbeat this often.
    pub heartbeat_ms: u64,
    /// A lead that misses this many heartbeats loses its job.
    pub missed_heartbeats: u64,
    /// The work set's.
    pub work: WorkConfig,
}

impl Default for ExecutionConfig {
    fn default() -> ExecutionConfig {
        ExecutionConfig { heartbeat_ms: 5_000, missed_heartbeats: 3, work: WorkConfig::default() }
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
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExecState {
    /// Waiting for admission.
    Queued,
    /// Admitted and assigned, or waiting for a new lead after its lead was lost.
    Running,
    /// Ended, with its outcome.
    Ended(Outcome),
}

/// What an execution asks its driver to do, or tells it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Effect {
    /// An item was appended to the execution's event stream.
    Event(EventItem),
    /// The lead was lost: the driver assigns a new lead (admission's choice), or ends the
    /// execution `Failed` when there is none.
    NeedLead,
    /// Stop an executor's process group: the agent kills it.
    Kill(String),
    /// A subscriber is done with the execution: its own outcome and policy.
    SubscriberEnded {
        /// The subscriber.
        subscriber: u64,
        /// Its outcome: the execution's, or `TimedOut` at its own deadline.
        outcome: Outcome,
        /// What it does next on its side.
        policy: LocalPolicy,
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
            refused: false,
            state: ExecState::Queued,
        };
        execution.stage(now, Stage::Queued, out);
        execution
    }

    /// Where it is.
    pub fn state(&self) -> ExecState {
        self.state
    }

    /// Its current attempt.
    pub fn attempt(&self) -> Attempt {
        self.attempt
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
        if self.lead.is_some() || self.work.is_some() {
            self.attempt = Attempt(self.attempt.0 + 1);
        }
        for old in self.executors().map(str::to_owned).collect::<Vec<_>>() {
            if old != lead && !helpers.contains(&old) {
                out.push(Effect::Kill(old));
            }
        }
        self.lead = Some(lead.to_owned());
        self.helpers = helpers.iter().filter(|h| *h != lead).cloned().collect();
        self.lead_heard = now;
        self.heard.insert(lead.to_owned(), now);
        self.refused = false;
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
    /// green past a refusal.
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
        self.heard.insert(from.to_owned(), now);
        let lead = self.lead.as_deref() == Some(from);
        if lead {
            self.lead_heard = now;
        }
        let mut done = None;
        for event in &req.events {
            match event {
                JobEvent::Refused(_) => self.refused = true,
                JobEvent::Done(d) if lead => done = Some(d.outcome.clone()),
                JobEvent::Done(_) => return Err(Refusal::NotHolder),
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
        let late: Vec<u64> =
            self.attached().filter(|s| now > s.deadline_ms).map(|s| s.id).collect();
        for id in late {
            let subscriber = self.subscribers.get_mut(&id).expect("attached");
            subscriber.state = SubscriberState::TimedOut;
            out.push(Effect::SubscriberEnded {
                subscriber: id,
                outcome: Outcome::TimedOut,
                policy: subscriber.policy.clone(),
            });
        }
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
            out.push(Effect::Kill(lost));
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
        if self.work.as_ref().is_some_and(WorkSet::is_done)
            && !self.stages.iter().any(|&(s, _)| s == Stage::Compiled)
        {
            self.stage(now, Stage::Compiled, out);
        }
    }

    /// Ends the execution: the work set drops its leases, every executor is stopped, and every
    /// attached subscriber gets the outcome.
    pub fn end(&mut self, now: Millis, outcome: Outcome, why: EndWhy, out: &mut Vec<Effect>) {
        if let ExecState::Ended(_) = self.state {
            return;
        }
        self.state = ExecState::Ended(outcome.clone());
        if let Some(work) = &mut self.work {
            let mut log = Vec::new();
            work.end(why, &mut log);
            out.extend(log.into_iter().map(Effect::Work));
        }
        self.finish(now, outcome, out);
    }

    fn finish(&mut self, now: Millis, outcome: Outcome, out: &mut Vec<Effect>) {
        for executor in self.executors().map(str::to_owned).collect::<Vec<_>>() {
            out.push(Effect::Kill(executor));
        }
        for subscriber in self.subscribers.values_mut() {
            if subscriber.state == SubscriberState::Attached {
                subscriber.state = SubscriberState::Ended;
                out.push(Effect::SubscriberEnded {
                    subscriber: subscriber.id,
                    outcome: outcome.clone(),
                    policy: subscriber.policy.clone(),
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

/// The action for a remote outcome under `policy`. `Never` fails on anything but a remote
/// verdict; `Fallback` builds locally after the deadline or a remote failure that is not a
/// verdict (a build error or a failed test is a result, not retried); `Race` was building
/// locally from the start, and the first to finish wins, a failure included.
pub fn local_action(policy: &LocalPolicy, remote: &Outcome) -> LocalAction {
    let verdict = matches!(remote, Outcome::Passed | Outcome::Failed | Outcome::Refused);
    match policy {
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
    }
}

/// The server's executions: one per running `JobContent`, admitted in section 5's order.
#[derive(Clone, Debug)]
pub struct Jobs {
    config: ExecutionConfig,
    admission: Admission,
    executions: BTreeMap<ExecutionId, Execution>,
    running: BTreeMap<JobContent, ExecutionId>,
    platforms: BTreeMap<ExecutionId, (Digest, Reservation)>,
    pr_latest: BTreeMap<u64, ExecutionId>,
    next_execution: u64,
    next_subscriber: u64,
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
            pr_latest: BTreeMap::new(),
            next_execution: 1,
            next_subscriber: 1,
        }
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

    /// Every execution.
    pub fn executions(&self) -> impl Iterator<Item = &Execution> {
        self.executions.values()
    }

    /// `POST /v1/jobs`, with the reservation the server measured for its lead. An equal
    /// `JobContent` running already is joined, not started again. A pull request's new push
    /// cancels its previous execution when nothing else subscribes to it.
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
        if let Namespace::Pr(pr) = &req.namespace {
            if let Some(previous) = self.pr_latest.insert(pr.n, id)
                && let Some(old) = self.executions.get(&previous)
                && !matches!(old.state(), ExecState::Ended(_))
                && old.attached().count() == 1
            {
                let only = old.attached().next().expect("one").id;
                let _ = self.unsubscribe(now, previous, only, out);
            }
        }
        Ok(JobAnswer { content, execution: id, subscriber: subscriber.id })
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
    pub fn killed(&mut self, now: Millis, executor: &str, alive: bool) {
        if alive {
            self.admission.finished(now, executor);
        } else {
            self.admission.lost(now, executor);
        }
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
            let _ = e.assign(now, &admitted.executor, &[], &mut log);
            self.after(now, admitted.execution, log, out);
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
                        self.admission.take(&lead, id);
                        let _ = e.assign(now, &lead, &[], &mut more);
                    }
                    None => e.end(now, Outcome::Failed, EndWhy::NoLead, &mut more),
                }
                out.push((id, effect));
                out.extend(more.into_iter().map(|e| (id, e)));
                continue;
            }
            out.push((id, effect));
        }
        let e = &self.executions[&id];
        if let ExecState::Ended(_) = e.state() {
            if self.running.get(&e.content) == Some(&id) {
                self.running.remove(&e.content);
            }
            self.admission.dequeue(id);
        }
    }
}
