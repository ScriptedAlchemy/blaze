//! Jobs and admission under a random load: executions, subscribers, attempts and capacity.
//!
//! Requests for `Main` and for pull requests arrive on one or two platforms, many of them for
//! the same content; subscribers leave and time out; pull requests push again and close;
//! leads die or stall and come back with a dead attempt's events; builders are rebuilt. The
//! checker holds after every step:
//!
//! - **capacity**: no machine's reservations sum past its capacity;
//! - **order**: a pull request's job is admitted only while no `Main` job waits on its
//!   platform, and every job only after the jobs of its class queued before it;
//! - **queue**: at most `Q` executions wait per platform, and `busy` is answered exactly then;
//! - **placement**: an execution runs on its platform and namespace within the executor's
//!   reservation; a live lead has exactly that assignment and cannot lead another execution;
//! - **attempts**: an execution is reassigned at most once, a second loss ends it `Lost`, and a
//!   dead attempt's events are refused;
//! - **subscribers**: equal content shares one execution while it runs; `TimedOut` only past a
//!   subscriber's own deadline and no other outcome after it; the last subscriber leaving
//!   ends it; a refusal never ends `Passed`;
//! - **stages**: coordinator-generated stage timestamps go forward in order. Arbitrary
//!   worker-supplied `Stage` events are outside this check;
//! - **progress**: every execution ends.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use blaze_wire::{
    Attempt, BuildSpec, Command, Digest, DoneEvent, ExecutionId, JobEvent, JobRequest, LocalPolicy,
    Namespace, Outcome, PrNamespace, ProfileSpec, RefusedEvent, Reservation, SourceRef, Stage,
    StoreIdentity, WorkersEventsRequest,
};

use super::{Rng, Violation};
use crate::admission::{Admission, AdmissionConfig, Class};
use crate::jobs::{self, ExecState, ExecutionConfig, Jobs};
use crate::{Millis, Refusal};

/// What one run did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Summary {
    /// The seed.
    pub seed: u64,
    /// Executions created.
    pub executions: u64,
    /// Requests answered `busy`.
    pub busy: u64,
    /// Requests that joined a running execution.
    pub joined: u64,
    /// Executions reassigned after a lead was lost.
    pub reassigned: u64,
    /// Outcomes, by `Outcome`'s discriminant.
    pub outcomes: BTreeMap<u64, u64>,
}

const STEP_MS: Millis = 1_000;
const HEARTBEAT_MS: Millis = 5_000;

#[derive(Clone, Debug)]
struct Lead {
    executor: String,
    attempt: blaze_wire::Attempt,
    finish_at: Millis,
    dead: bool,
    stall_until: Millis,
    next_beat: Millis,
    refuse: bool,
}

struct World {
    seed: u64,
    rng: Rng,
    now: Millis,
    jobs: Jobs,
    specs: Vec<BuildSpec>,
    queue_limit: usize,
    /// The checker's own queue: per platform and class, the executions waiting, in order.
    shadow: BTreeMap<(Digest, Class), VecDeque<ExecutionId>>,
    meta: BTreeMap<ExecutionId, (Digest, Namespace, Reservation)>,
    subscribers: BTreeMap<u64, (ExecutionId, u64)>,
    ended_subs: BTreeSet<u64>,
    left: BTreeSet<u64>,
    leads: BTreeMap<ExecutionId, Lead>,
    late: Vec<(ExecutionId, String, blaze_wire::Attempt)>,
    kills: Vec<(Millis, String, ExecutionId, Attempt)>,
    dead: BTreeSet<String>,
    summary: Summary,
}

impl World {
    fn violation(&self, invariant: &'static str, detail: String) -> Violation {
        Violation { seed: self.seed, invariant, at: self.now, detail }
    }

    fn new(seed: u64) -> World {
        let mut rng = Rng::new(seed ^ 0x5EED_0F_10B5);
        let queue_limit = rng.range(2, 8) as usize;
        let mut admission =
            Admission::new(AdmissionConfig { queue_limit, ..AdmissionConfig::default() });
        let platforms: Vec<Digest> = (0..rng.range(1, 2)).map(|_| rng.digest()).collect();
        for (p, platform) in platforms.iter().enumerate() {
            for m in 0..rng.range(1, 3) {
                let name = format!("m{p}.{m}");
                let capacity = Reservation {
                    memory: rng.range(16, 96),
                    cpus: rng.range(8, 48) as u32,
                    disk: rng.range(100, 600),
                };
                admission.add_machine(&name, *platform, capacity);
                for _ in 0..rng.range(0, 2) {
                    let r = Reservation {
                        memory: rng.range(8, 24),
                        cpus: rng.range(4, 16) as u32,
                        disk: rng.range(20, 80),
                    };
                    let _ = admission.add_builder(0, &name, r);
                }
            }
        }
        let specs = (0..rng.range(2, 6))
            .map(|i| spec(platforms[i as usize % platforms.len()], i))
            .collect();
        World {
            seed,
            rng,
            now: 0,
            jobs: Jobs::new(admission, ExecutionConfig::default()),
            specs,
            queue_limit,
            shadow: BTreeMap::new(),
            meta: BTreeMap::new(),
            subscribers: BTreeMap::new(),
            ended_subs: BTreeSet::new(),
            left: BTreeSet::new(),
            leads: BTreeMap::new(),
            late: Vec::new(),
            kills: Vec::new(),
            dead: BTreeSet::new(),
            summary: Summary {
                seed,
                executions: 0,
                busy: 0,
                joined: 0,
                reassigned: 0,
                outcomes: BTreeMap::new(),
            },
        }
    }

    fn run(mut self) -> Result<Summary, Violation> {
        let horizon = self.rng.range(60_000, 600_000);
        let end = horizon + 3_600_000;
        while self.now <= end {
            if self.now < horizon {
                self.random_event()?;
            }
            self.leads()?;
            self.agents();
            let mut out = Vec::new();
            self.jobs.tick(self.now, &mut out);
            self.effects(out)?;
            self.jobs.admission_view().check().map_err(|e| self.violation("capacity", e))?;
            self.assignments()?;
            if self.now >= horizon
                && self.jobs.executions().all(|e| matches!(e.state(), ExecState::Ended(_)))
                && self.kills.is_empty()
            {
                break;
            }
            self.now += STEP_MS;
        }
        for e in self.jobs.executions() {
            if !matches!(e.state(), ExecState::Ended(_)) {
                return Err(self.violation(
                    "progress",
                    format!("execution {:?} never ended: {:?}", e.id, e.state()),
                ));
            }
            stages(e).map_err(|d| self.violation("stages", d))?;
        }
        Ok(self.summary)
    }

    fn random_event(&mut self) -> Result<(), Violation> {
        match self.rng.below(100) {
            0..=11 => self.submit(),
            12..=14 => {
                let attached: Vec<u64> = self
                    .subscribers
                    .keys()
                    .filter(|s| !self.left.contains(s) && !self.ended_subs.contains(s))
                    .copied()
                    .collect();
                if let Some(&s) =
                    attached.get(self.rng.below(attached.len().max(1) as u64) as usize)
                {
                    self.unsubscribe(s)?;
                }
                Ok(())
            }
            15 => {
                let n = self.rng.range(1, 4);
                let gone = self.jobs.admission().close_pr(n);
                for (executor, busy) in gone {
                    self.dead.insert(executor.clone());
                    if let Some((execution, _)) = busy
                        && let Some(lead) = self.leads.get_mut(&execution)
                        && lead.executor == executor
                    {
                        lead.dead = true;
                    }
                }
                Ok(())
            }
            16..=17 => {
                // A lead dies, or stalls and comes back with its old attempt's events.
                let running: Vec<ExecutionId> = self.leads.keys().copied().collect();
                if let Some(&id) = running.get(self.rng.below(running.len().max(1) as u64) as usize)
                {
                    let stall = self.rng.chance(500_000);
                    let until = self.now + self.rng.range(10_000, 60_000);
                    let lead = self.leads.get_mut(&id).expect("running");
                    if stall {
                        lead.stall_until = until;
                    } else {
                        lead.dead = true;
                    }
                }
                Ok(())
            }
            18 => {
                let builders: Vec<String> = self
                    .jobs
                    .admission_view()
                    .executors()
                    .filter(|e| e.namespace == Namespace::Main)
                    .map(|e| e.id.clone())
                    .collect();
                if let Some(b) = builders.get(self.rng.below(builders.len().max(1) as u64) as usize)
                {
                    let r = Reservation {
                        memory: self.rng.range(8, 32),
                        cpus: self.rng.range(4, 16) as u32,
                        disk: self.rng.range(20, 80),
                    };
                    let _ = self.jobs.admission().rebuild(self.now, b, r);
                }
                Ok(())
            }
            19 => {
                let gone = self.jobs.admission().reap(self.now);
                self.dead.extend(gone);
                Ok(())
            }
            _ => Ok(()),
        }
    }

    fn submit(&mut self) -> Result<(), Violation> {
        let spec = self.specs[self.rng.below(self.specs.len() as u64) as usize].clone();
        let namespace = if self.rng.chance(350_000) {
            Namespace::Main
        } else {
            Namespace::Pr(PrNamespace { n: self.rng.range(1, 4) })
        };
        let req = JobRequest {
            spec: spec.clone(),
            namespace: namespace.clone(),
            deadline_ms: self.now + self.rng.range(20_000, 600_000),
            local: match self.rng.below(3) {
                0 => LocalPolicy::Never,
                1 => LocalPolicy::Fallback,
                _ => LocalPolicy::Race,
            },
            test_timeout_ms: None,
        };
        let need = Reservation {
            memory: self.rng.range(4, 32),
            cpus: self.rng.range(2, 16) as u32,
            disk: self.rng.range(10, 100),
        };
        let platform = spec.platform;
        let class = Class::of(&namespace).expect("a job's namespace");
        let waiting: usize = [Class::Main, Class::Pr]
            .iter()
            .map(|c| self.shadow.get(&(platform, *c)).map_or(0, VecDeque::len))
            .sum();
        let running: Option<ExecutionId> = self
            .jobs
            .executions()
            .find(|e| {
                !matches!(e.state(), ExecState::Ended(_))
                    && self.meta.get(&e.id).is_some_and(|(_, ns, _)| *ns == namespace)
                    && e.content
                        == blaze_wire::JobContent::of(&blaze_wire::JobContentInput {
                            spec: blaze_wire::digest_of(&spec),
                            namespace: namespace.clone(),
                        })
            })
            .map(|e| e.id);
        let mut out = Vec::new();
        match self.jobs.submit(self.now, &req, need.clone(), &mut out) {
            Ok(answer) => {
                self.subscribers.insert(answer.subscriber, (answer.execution, req.deadline_ms));
                match running {
                    Some(id) if id != answer.execution => {
                        return Err(self.violation(
                            "subscribers",
                            format!(
                                "equal content running as {id:?} started {:?}",
                                answer.execution
                            ),
                        ));
                    }
                    Some(_) => self.summary.joined += 1,
                    None => {
                        if waiting >= self.queue_limit {
                            return Err(self.violation(
                                "queue",
                                format!(
                                    "{waiting} waiting and a job queued past Q {}",
                                    self.queue_limit
                                ),
                            ));
                        }
                        self.summary.executions += 1;
                        self.meta.insert(answer.execution, (platform, namespace, need));
                        self.shadow
                            .entry((platform, class))
                            .or_default()
                            .push_back(answer.execution);
                    }
                }
            }
            Err((Refusal::Busy, _)) => {
                self.summary.busy += 1;
                if running.is_some() || waiting < self.queue_limit {
                    return Err(self.violation(
                        "queue",
                        format!("busy with {waiting} waiting under Q {}", self.queue_limit),
                    ));
                }
            }
            Err((other, _)) => {
                return Err(self.violation("queue", format!("a submit refused: {other:?}")));
            }
        }
        self.effects(out)
    }

    fn unsubscribe(&mut self, s: u64) -> Result<(), Violation> {
        let (execution, _) = self.subscribers[&s];
        self.left.insert(s);
        let mut out = Vec::new();
        let _ = self.jobs.unsubscribe(self.now, execution, s, &mut out);
        self.effects(out)?;
        let e = self.jobs.execution(execution).expect("known");
        let attached = e.subscribers().any(|x| x.state == jobs::SubscriberState::Attached);
        if !attached && !matches!(e.state(), ExecState::Ended(_)) {
            return Err(
                self.violation("subscribers", format!("{execution:?} runs with no subscriber"))
            );
        }
        Ok(())
    }

    /// The leads of running executions: heartbeats, and the end of the job.
    fn leads(&mut self) -> Result<(), Violation> {
        let ids: Vec<ExecutionId> = self.leads.keys().copied().collect();
        for id in ids {
            let lead = self.leads[&id].clone();
            if lead.dead || self.now < lead.stall_until || self.now < lead.next_beat {
                continue;
            }
            let mut events = Vec::new();
            if self.now >= lead.finish_at {
                if lead.refuse {
                    events.push(JobEvent::Refused(RefusedEvent {
                        function: "a build script blaze cannot run".to_owned(),
                        why: "it needs rustc".to_owned(),
                    }));
                }
                let outcome =
                    if self.rng.chance(800_000) { Outcome::Passed } else { Outcome::Failed };
                events.push(JobEvent::Done(DoneEvent { outcome, counters: Vec::new() }));
            }
            let req = WorkersEventsRequest { execution: id, attempt: lead.attempt, events };
            let mut out = Vec::new();
            let answer = self.jobs.worker_events(self.now, &lead.executor, &req, &mut out);
            let current = self.jobs.execution(id).map(|e| e.attempt());
            if current != Some(lead.attempt) && answer != Err(Refusal::AttemptMoved) {
                let ended = self
                    .jobs
                    .execution(id)
                    .is_some_and(|e| matches!(e.state(), ExecState::Ended(_)));
                if !ended {
                    return Err(self.violation(
                        "attempts",
                        format!("a dead attempt's events answered {answer:?}"),
                    ));
                }
            }
            if let Some(l) = self.leads.get_mut(&id) {
                l.next_beat = self.now + HEARTBEAT_MS;
            }
            self.effects(out)?;
        }
        // Late events of dead attempts, sent again: every one refused.
        for (id, executor, attempt) in std::mem::take(&mut self.late) {
            let req = WorkersEventsRequest { execution: id, attempt, events: Vec::new() };
            let mut out = Vec::new();
            let answer = self.jobs.worker_events(self.now, &executor, &req, &mut out);
            let ended =
                self.jobs.execution(id).is_some_and(|e| matches!(e.state(), ExecState::Ended(_)));
            if !ended && answer != Err(Refusal::AttemptMoved) {
                return Err(self.violation(
                    "attempts",
                    format!("a late event of {attempt:?} answered {answer:?}"),
                ));
            }
            self.effects(out)?;
        }
        Ok(())
    }

    /// The agents: kill acknowledgements may be delayed and duplicated. Each one names its
    /// assignment, so replay after the executor starts another job cannot free that job.
    fn agents(&mut self) {
        let now = self.now;
        let (due, later): (Vec<_>, Vec<_>) =
            std::mem::take(&mut self.kills).into_iter().partition(|(at, ..)| *at <= now);
        self.kills = later;
        for (_, executor, execution, attempt) in due {
            let alive = !self.dead.contains(&executor);
            self.jobs.killed(now, &executor, execution, attempt, alive);
        }
    }

    fn effects(&mut self, out: Vec<(ExecutionId, jobs::Effect)>) -> Result<(), Violation> {
        for (id, effect) in out {
            match effect {
                jobs::Effect::Event(item) => self.event(id, &item.event, item.attempt)?,
                jobs::Effect::Kill { executor, attempt } => {
                    if let Some(lead) = self.leads.get(&id)
                        && lead.executor == executor
                        && lead.dead
                    {
                        self.dead.insert(executor.clone());
                    }
                    let at = self.now + self.rng.range(0, 10_000);
                    self.kills.push((at, executor.clone(), id, attempt));
                    if self.rng.chance(250_000) {
                        let replay_at = at + self.rng.range(10_000, 30_000);
                        self.kills.push((replay_at, executor, id, attempt));
                    }
                }
                jobs::Effect::SubscriberEnded { subscriber, outcome, .. } => {
                    if self.left.contains(&subscriber) {
                        return Err(self.violation(
                            "subscribers",
                            format!("subscriber {subscriber} ended after it left"),
                        ));
                    }
                    if !self.ended_subs.insert(subscriber) {
                        return Err(self.violation(
                            "subscribers",
                            format!("subscriber {subscriber} ended twice"),
                        ));
                    }
                    let (_, deadline) = self.subscribers[&subscriber];
                    if outcome == Outcome::TimedOut && self.now <= deadline {
                        return Err(self.violation(
                            "subscribers",
                            format!("subscriber {subscriber} timed out before its deadline"),
                        ));
                    }
                    if self.now > deadline && outcome != Outcome::TimedOut {
                        return Err(self.violation(
                            "subscribers",
                            format!("subscriber {subscriber} got {outcome:?} past its deadline"),
                        ));
                    }
                }
                jobs::Effect::NeedLead => self.summary.reassigned += 1,
                jobs::Effect::Work(_) => {}
            }
        }
        Ok(())
    }

    fn event(
        &mut self,
        id: ExecutionId,
        event: &JobEvent,
        attempt: blaze_wire::Attempt,
    ) -> Result<(), Violation> {
        let (platform, namespace, need) = self.meta.get(&id).cloned().expect("submitted");
        match event {
            JobEvent::Stage(stage) if stage.stage == Stage::Admitted => {
                let class = Class::of(&namespace).expect("a job's");
                let queue = self.shadow.entry((platform, class)).or_default();
                if queue.front() != Some(&id) {
                    let first = queue.front().copied();
                    return Err(self.violation(
                        "order",
                        format!("{id:?} admitted before {first:?} of its class"),
                    ));
                }
                queue.pop_front();
                if class == Class::Pr
                    && let Some(main) =
                        self.shadow.get(&(platform, Class::Main)).and_then(|q| q.front())
                {
                    return Err(self.violation(
                        "order",
                        format!("pull request {id:?} admitted while Main {main:?} waits"),
                    ));
                }
            }
            JobEvent::Assigned(assigned) => {
                let executor = self.jobs.admission_view().executor(&assigned.executor).cloned();
                match executor {
                    Some(e)
                        if self.jobs.admission_view().platform(&e.machine) == Some(platform)
                            && e.namespace == namespace
                            && e.busy == Some((id, attempt))
                            && e.reservation.memory >= need.memory
                            && e.reservation.cpus >= need.cpus
                            && e.reservation.disk >= need.disk => {}
                    other => {
                        return Err(self.violation(
                            "placement",
                            format!("{id:?} of {namespace:?} assigned to {other:?}"),
                        ));
                    }
                }
                if assigned.attempt.0 > 2 {
                    return Err(self.violation(
                        "attempts",
                        format!("{id:?} at attempt {}", assigned.attempt.0),
                    ));
                }
                if let Some(old) = self.leads.remove(&id)
                    && !old.dead
                {
                    self.late.push((id, old.executor, old.attempt));
                }
                let finish_at = self.now + self.rng.range(5_000, 120_000);
                let refuse = self.rng.chance(100_000);
                self.leads.insert(
                    id,
                    Lead {
                        executor: assigned.executor.clone(),
                        attempt,
                        finish_at,
                        dead: false,
                        stall_until: 0,
                        next_beat: self.now + HEARTBEAT_MS,
                        refuse,
                    },
                );
            }
            JobEvent::Done(done) => {
                let e = self.jobs.execution(id).expect("known");
                if e.losses() >= 2 && done.outcome != Outcome::Lost {
                    return Err(self.violation(
                        "attempts",
                        format!("{id:?} lost twice ended {:?}", done.outcome),
                    ));
                }
                if let Some(lead) = self.leads.remove(&id)
                    && lead.refuse
                    && done.outcome == Outcome::Passed
                    && lead.finish_at <= self.now
                {
                    return Err(self
                        .violation("subscribers", format!("{id:?} ended Passed after a refusal")));
                }
                for queue in self.shadow.values_mut() {
                    queue.retain(|x| *x != id);
                }
                *self.summary.outcomes.entry(done.outcome.discriminant()).or_default() += 1;
            }
            _ => {}
        }
        Ok(())
    }

    /// Capacity sums alone cannot detect two executions sharing one executor reservation.
    fn assignments(&self) -> Result<(), Violation> {
        let mut owners = BTreeMap::new();
        for execution in self.jobs.executions() {
            if execution.state() != ExecState::Running {
                continue;
            }
            let Some(lead) = execution.lead() else { continue };
            if let Some(other) = owners.insert(lead, execution.id) {
                return Err(self.violation(
                    "placement",
                    format!("{lead} leads both {other:?} and {:?}", execution.id),
                ));
            }
            let expected = Some((execution.id, execution.attempt()));
            if !self.dead.contains(lead)
                && self
                    .jobs
                    .admission_view()
                    .executor(lead)
                    .is_none_or(|slot| slot.busy != expected)
            {
                return Err(self.violation(
                    "placement",
                    format!("{lead} lost the reservation of {:?}", execution.id),
                ));
            }
        }
        Ok(())
    }
}

/// Stage timestamps go forward, each stage after the one before it.
fn stages(e: &jobs::Execution) -> Result<(), String> {
    let mut last: Option<(u64, Millis)> = None;
    for (stage, at) in e.stages() {
        let rank = stage.discriminant();
        if let Some((r, t)) = last
            && (*at < t || (rank < r && *stage != Stage::Assigned))
        {
            return Err(format!("{:?}: stage {stage:?} at {at} after {r} at {t}", e.id));
        }
        last = Some((rank, *at));
    }
    Ok(())
}

fn spec(platform: Digest, i: u64) -> BuildSpec {
    let profile = ProfileSpec {
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
    };
    BuildSpec {
        source: SourceRef::Root(blaze_wire::RootRef { root: [i as u8; 32] }),
        lock: [1; 32],
        units: Vec::new(),
        asked: blaze_wire::Set::new(Vec::new()),
        profile,
        platform,
        rustflags: Vec::new(),
        config_env: blaze_wire::Set::new(Vec::new()),
        script_env: blaze_wire::Set::new(Vec::new()),
        cwd: String::new(),
        command: Command::Build,
        toolchain: StoreIdentity([2; 32]),
    }
}

/// Runs the jobs simulation of `seed`.
pub fn run(seed: u64) -> Result<Summary, Violation> {
    World::new(seed).run()
}
