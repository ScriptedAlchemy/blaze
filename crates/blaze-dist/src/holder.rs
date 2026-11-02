//! An executor's side of the work calls, as a sans-IO machine: join, ask for a lease, compile
//! the grant as one batch, report it, and again; heartbeats beside.
//!
//! It holds one call at a time and sends it again, unchanged, until it is answered, so a result
//! whose answer was lost is replayed with the same `seq` and gets the same answer. It keeps its
//! findings: the names its committed batches found, which it may be granted later, until the
//! coordinator makes them terminal or the execution ends. A granted name it cannot resolve is
//! answered `Lost`, never left out.
//!
//! The compile itself is not here: [`Effect::Compile`] hands a [`Plan`] to the executor's
//! compile threads (D0's `compile_ahead` batch), and [`Event::Compiled`] brings back what they
//! did.

use std::collections::{BTreeMap, BTreeSet};

use blaze_wire::{
    Attempt, BatchResult, Digest, ExecutionId, Grant, ItemOutcome, ItemResult, PackRef, RootSet,
    Set, Why, WireError, WorkJoinAnswer, WorkJoinRequest, WorkLeaseAnswer, WorkLeaseRequest,
    WorkName, WorkResultAnswer, WorkersEventsAnswer, WorkersEventsRequest,
};

use crate::work::{LOST_FINDING, UNKNOWN_NAME};
use crate::{Millis, Ranges, Refusal};

/// The holder's timings, on its own clock.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HolderConfig {
    /// A heartbeat this often (section 4: every 5 s).
    pub heartbeat_ms: u64,
    /// A call not answered in this long is sent again.
    pub retry_ms: u64,
    /// After an empty lease answer, ask again this much later.
    pub idle_ms: u64,
}

impl Default for HolderConfig {
    fn default() -> HolderConfig {
        HolderConfig { heartbeat_ms: 5_000, retry_ms: 10_000, idle_ms: 2_000 }
    }
}

/// A call to the coordinator.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Call {
    /// `POST /v1/work/join`.
    Join(WorkJoinRequest),
    /// `POST /v1/work/lease`.
    Lease(WorkLeaseRequest),
    /// `POST /v1/work/result`.
    Result(BatchResult),
    /// `POST /v1/workers/events`, a heartbeat when its events are empty.
    Events(WorkersEventsRequest),
}

/// The coordinator's answer to a [`Call`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Reply {
    /// To a join.
    Join(Result<WorkJoinAnswer, WireError>),
    /// To a lease.
    Lease(Result<WorkLeaseAnswer, WireError>),
    /// To a result.
    Result(Result<WorkResultAnswer, WireError>),
    /// To events.
    Events(Result<WorkersEventsAnswer, WireError>),
}

/// What the compile threads are asked to do with a grant.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Plan {
    /// The grant.
    pub grant: Grant,
    /// The granted names the holder resolves, through a finding or its pre-index, in the
    /// grant's set order. The others are answered `Lost` by the holder.
    pub compile: Vec<WorkName>,
}

/// What the compile threads did with a [`Plan`]: a batch, exported, sealed and uploaded.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Compiled {
    /// The lease generation of the plan's grant.
    pub generation: u64,
    /// One item per processed root, in ascending index order.
    pub roots: Vec<ItemResult>,
    /// One item per processed name of the plan's `compile`, in its order.
    pub names: Vec<ItemResult>,
    /// The packs the batch's entries were sealed into, uploaded.
    pub packs: Vec<PackRef>,
    /// Granted roots the batch did not get to (u7's budget).
    pub returned_roots: Ranges,
    /// Names of the plan's `compile` the batch did not get to.
    pub returned_names: Vec<WorkName>,
}

/// Why a holder left the execution.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StopWhy {
    /// A heartbeat was answered "cancel": the execution ended, or a helper is no longer needed.
    Cancelled,
    /// The execution's attempt moved: the agent assigns again.
    AttemptMoved,
    /// Its front end differs from the lead's.
    FrontEndDiffers,
    /// The execution ended.
    Ended,
    /// An answer the protocol does not give a correct holder: a bug, named.
    Protocol(String),
}

/// What the holder asks its driver to do.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Effect {
    /// Send a call. A reply comes back as [`Event::Reply`] with the same `id`.
    Send {
        /// The call's id.
        id: u64,
        /// The call.
        call: Call,
    },
    /// Compile a batch; [`Event::Compiled`] brings it back.
    Compile(Plan),
    /// Findings the compiler need no longer retain.
    Release(Vec<WorkName>),
    /// The holder left the execution.
    Stop(StopWhy),
}

/// What happened to the holder.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    /// A reply to the call of `id`.
    Reply {
        /// The call's id.
        id: u64,
        /// The reply.
        reply: Reply,
    },
    /// The compile threads finished a batch.
    Compiled(Compiled),
    /// The front end is loaded and its root set computed: the holder joins.
    Loaded,
    /// Time passed.
    Tick,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Phase {
    Loading,
    Joining,
    Idle { until: Millis, then_join: bool },
    Leasing,
    Compiling { grant: Grant, lost: BTreeMap<WorkName, ItemResult> },
    Reporting { result: BatchResult, root_items: usize },
    Stopped,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Outstanding {
    id: u64,
    call: Call,
    sent_at: Millis,
}

/// One executor's part in one execution.
#[derive(Clone, Debug)]
pub struct Holder {
    config: HolderConfig,
    id: String,
    execution: ExecutionId,
    attempt: Attempt,
    roots: RootSet,
    rejected: BTreeSet<WorkName>,
    index: BTreeSet<WorkName>,
    findings: BTreeMap<WorkName, Digest>,
    lost: BTreeSet<WorkName>,
    phase: Phase,
    seq: u64,
    next_id: u64,
    outstanding: Option<Outstanding>,
    beat: Option<u64>,
    next_beat: Millis,
}

impl Holder {
    /// The holder `id` of `execution` at `attempt`, with the root set its front end computed,
    /// the names its daemon rejects, and its pre-index: the names it resolves with no finding.
    pub fn new(
        id: &str,
        execution: ExecutionId,
        attempt: Attempt,
        roots: RootSet,
        rejected: Set<WorkName>,
        index: BTreeSet<WorkName>,
        config: HolderConfig,
    ) -> Holder {
        Holder {
            config,
            id: id.to_owned(),
            execution,
            attempt,
            roots,
            rejected: rejected.into_vec().into_iter().collect(),
            index,
            findings: BTreeMap::new(),
            lost: BTreeSet::new(),
            phase: Phase::Loading,
            seq: 0,
            next_id: 1,
            outstanding: None,
            beat: None,
            next_beat: 0,
        }
    }

    /// The holder's id.
    pub fn id(&self) -> &str {
        &self.id
    }

    /// Whether it left the execution.
    pub fn is_stopped(&self) -> bool {
        self.phase == Phase::Stopped
    }

    /// The findings it holds: name and tuple.
    pub fn findings(&self) -> &BTreeMap<WorkName, Digest> {
        &self.findings
    }

    /// Starts: heartbeats from now, while the front end loads; [`Event::Loaded`] joins. No
    /// executor waits for the lead to load, and a lead that is loading is not a lost one.
    pub fn start(&mut self, now: Millis, out: &mut Vec<Effect>) {
        self.next_beat = now;
        self.tick(now, out);
    }

    /// A fault: the finding of `name` is gone, as a batch eviction or a type GC that broke the
    /// retention contract would leave it. `knows` says whether the holder still knows it had
    /// it (`lost-finding`) or not (`unknown-name`).
    pub fn drop_finding(&mut self, name: &WorkName, knows: bool) {
        if self.findings.remove(name).is_some() && knows {
            self.lost.insert(*name);
        }
    }

    /// One step.
    pub fn step(&mut self, now: Millis, event: Event, out: &mut Vec<Effect>) {
        if self.phase == Phase::Stopped {
            // Cancellation can race a compile already in flight. Its discoveries were
            // never reported, but the compiler retained them before returning this batch.
            if let Event::Compiled(batch) = event {
                self.release(
                    batch
                        .roots
                        .iter()
                        .chain(&batch.names)
                        .flat_map(|item| item.found.iter().map(|found| found.name)),
                    out,
                );
            }
            return;
        }
        match event {
            Event::Tick => self.tick(now, out),
            Event::Compiled(batch) => self.compiled(now, batch, out),
            Event::Loaded => {
                if self.phase == Phase::Loading {
                    self.join(now, out);
                }
            }
            Event::Reply { id, reply: Reply::Events(answer) } => {
                if Some(id) == self.beat {
                    self.beat = None;
                    if answer == Ok(WorkersEventsAnswer::CANCEL) {
                        self.stop(StopWhy::Cancelled, out);
                    }
                }
            }
            Event::Reply { id, reply } => {
                if self.outstanding.as_ref().map(|o| o.id) != Some(id) {
                    // A reply to a call answered before, or one the network repeated.
                    return;
                }
                self.outstanding = None;
                self.reply(now, reply, out);
            }
        }
    }

    fn tick(&mut self, now: Millis, out: &mut Vec<Effect>) {
        if now >= self.next_beat {
            self.next_beat = now + self.config.heartbeat_ms;
            // A slow or lost heartbeat response must not become obsolete merely
            // because another heartbeat interval passed. Retry the same local
            // request id until its reply arrives: a late CANCEL must still fence
            // this holder and release its retained compiler findings.
            let id = match self.beat {
                Some(id) => id,
                None => {
                    let id = self.take_id();
                    self.beat = Some(id);
                    id
                }
            };
            let call = Call::Events(WorkersEventsRequest {
                execution: self.execution,
                attempt: self.attempt,
                events: Vec::new(),
            });
            out.push(Effect::Send { id, call });
        }
        if let Some(outstanding) = &mut self.outstanding
            && now >= outstanding.sent_at + self.config.retry_ms
        {
            // Sent again unchanged: a join, a lease and a result are all answered alike twice.
            outstanding.sent_at = now;
            out.push(Effect::Send { id: outstanding.id, call: outstanding.call.clone() });
        }
        if let Phase::Idle { until, then_join } = self.phase
            && now >= until
        {
            if then_join {
                self.join(now, out);
            } else {
                self.lease(now, out);
            }
        }
    }

    fn reply(&mut self, now: Millis, reply: Reply, out: &mut Vec<Effect>) {
        match (std::mem::replace(&mut self.phase, Phase::Leasing), reply) {
            (Phase::Joining, Reply::Join(Ok(answer))) => {
                if answer.roots != self.roots {
                    return self.stop(StopWhy::FrontEndDiffers, out);
                }
                self.lease(now, out);
            }
            (Phase::Joining, Reply::Join(Err(error))) => match Refusal::of(&error) {
                Some(Refusal::NotYet) => {
                    let wait = error.retry_after_ms.unwrap_or(self.config.retry_ms);
                    self.phase = Phase::Idle { until: now + wait, then_join: true };
                }
                Some(Refusal::FrontEndDiffers) => self.stop(StopWhy::FrontEndDiffers, out),
                other => self.refused(other, &error, out),
            },
            (Phase::Leasing, Reply::Lease(Ok(answer))) => match answer.grant {
                Some(grant) => self.plan(grant, out),
                None => {
                    self.phase = Phase::Idle { until: now + self.config.idle_ms, then_join: false };
                }
            },
            (Phase::Leasing, Reply::Lease(Err(error))) => match Refusal::of(&error) {
                Some(Refusal::NotJoined) => self.rejoin(now, out),
                other => self.refused(other, &error, out),
            },
            (Phase::Reporting { result, root_items }, Reply::Result(Ok(answer))) => {
                self.committed(&result, root_items, &answer, out);
                self.lease(now, out);
            }
            (Phase::Reporting { result, .. }, Reply::Result(Err(error))) => {
                // Refused whole: nothing it found was registered.
                let found: Vec<WorkName> = result
                    .items
                    .iter()
                    .flat_map(|item| item.found.iter().map(|f| f.name))
                    .filter(|name| !self.findings.contains_key(name))
                    .collect();
                if !found.is_empty() {
                    out.push(Effect::Release(found));
                }
                match Refusal::of(&error) {
                    Some(Refusal::DeadLease) => self.lease(now, out),
                    other => self.refused(other, &error, out),
                }
            }
            (phase, reply) => {
                self.phase = phase;
                self.stop(StopWhy::Protocol(format!("{reply:?} in {:?}", self.phase)), out);
            }
        }
    }

    fn refused(&mut self, refusal: Option<Refusal>, error: &WireError, out: &mut Vec<Effect>) {
        let why = match refusal {
            Some(Refusal::AttemptMoved) => StopWhy::AttemptMoved,
            Some(Refusal::Ended | Refusal::NoExecution) => StopWhy::Ended,
            Some(Refusal::FrontEndDiffers) => StopWhy::FrontEndDiffers,
            _ => StopWhy::Protocol(error.message.clone()),
        };
        self.stop(why, out);
    }

    /// The coordinator dropped this holder: what it found is no longer counted on, so it is
    /// released, and the holder joins as new.
    fn rejoin(&mut self, now: Millis, out: &mut Vec<Effect>) {
        let held: Vec<WorkName> = self.findings.keys().copied().collect();
        if !held.is_empty() {
            out.push(Effect::Release(held));
        }
        self.findings.clear();
        self.lost.clear();
        self.join(now, out);
    }

    fn plan(&mut self, grant: Grant, out: &mut Vec<Effect>) {
        if grant.lease.holder != self.id
            || grant.lease.attempt != self.attempt
            || grant.lease.execution != self.execution
        {
            return self
                .stop(StopWhy::Protocol(format!("a grant of another lease: {grant:?}")), out);
        }
        let mut compile = Vec::new();
        let mut lost = BTreeMap::new();
        for name in grant.names.iter() {
            let why = if self.findings.contains_key(name) || self.index.contains(name) {
                compile.push(*name);
                continue;
            } else if self.lost.contains(name) {
                LOST_FINDING
            } else {
                UNKNOWN_NAME
            };
            // Never left out: a name it cannot resolve is answered, and it stops being a finder.
            lost.insert(
                *name,
                ItemResult {
                    name: *name,
                    outcome: ItemOutcome::Lost(Why { why: why.to_owned() }),
                    found: Set::new(Vec::new()),
                    unnamed: Set::new(Vec::new()),
                },
            );
        }
        self.phase = Phase::Compiling { grant: grant.clone(), lost };
        out.push(Effect::Compile(Plan { grant, compile }));
    }

    fn compiled(&mut self, now: Millis, batch: Compiled, out: &mut Vec<Effect>) {
        let Phase::Compiling { grant, lost } = &self.phase else { return };
        if batch.generation != grant.lease.generation {
            return;
        }
        let (grant, lost) = (grant.clone(), lost.clone());
        let returned: BTreeSet<WorkName> = batch.returned_names.iter().copied().collect();
        let mut compiled = batch.names.into_iter();
        let root_items = batch.roots.len();
        let mut items = batch.roots;
        // Items are the processed roots' in index order, then the processed names' in the
        // grant's order: the shape the coordinator checks.
        for name in grant.names.iter() {
            if let Some(item) = lost.get(name) {
                items.push(item.clone());
            } else if !returned.contains(name)
                && let Some(item) = compiled.next()
            {
                items.push(item);
            }
        }
        self.seq += 1;
        let result = BatchResult {
            lease: grant.lease.clone(),
            seq: self.seq,
            items,
            packs: batch.packs,
            returned_roots: batch.returned_roots.to_wire(),
            returned_names: Set::new(batch.returned_names),
        };
        self.phase = Phase::Reporting { result: result.clone(), root_items };
        self.send(now, Call::Result(result), out);
    }

    /// A result committed: what it found is held, the names it finished are released, and so
    /// are the conflicts, which nobody compiles but the lead.
    fn committed(
        &mut self,
        result: &BatchResult,
        root_items: usize,
        answer: &WorkResultAnswer,
        out: &mut Vec<Effect>,
    ) {
        let conflicts: BTreeSet<WorkName> = answer.conflicts.iter().copied().collect();
        let mut release = conflicts.clone();
        for item in &result.items {
            for found in item.found.iter() {
                if !conflicts.contains(&found.name) && !self.rejected.contains(&found.name) {
                    self.findings.insert(found.name, found.tuple);
                } else {
                    // A newly discovered request is already retained by the compiler,
                    // even though it never enters this map of accepted findings.
                    release.insert(found.name);
                }
            }
        }
        release.extend(result.items[root_items..].iter().map(|item| item.name));
        self.release(release, out);
    }

    fn join(&mut self, now: Millis, out: &mut Vec<Effect>) {
        self.phase = Phase::Joining;
        let call = Call::Join(WorkJoinRequest {
            execution: self.execution,
            attempt: self.attempt,
            roots: self.roots.clone(),
            rejected: Set::new(self.rejected.iter().copied().collect()),
        });
        self.send(now, call, out);
    }

    fn lease(&mut self, now: Millis, out: &mut Vec<Effect>) {
        self.phase = Phase::Leasing;
        let call =
            Call::Lease(WorkLeaseRequest { execution: self.execution, attempt: self.attempt });
        self.send(now, call, out);
    }

    fn send(&mut self, now: Millis, call: Call, out: &mut Vec<Effect>) {
        let id = self.take_id();
        self.outstanding = Some(Outstanding { id, call: call.clone(), sent_at: now });
        out.push(Effect::Send { id, call });
    }

    fn take_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    fn stop(&mut self, why: StopWhy, out: &mut Vec<Effect>) {
        let mut held: BTreeSet<WorkName> = self.findings.keys().copied().collect();
        if let Phase::Reporting { result, .. } = &self.phase {
            held.extend(result.items.iter().flat_map(|item| item.found.iter().map(|f| f.name)));
        }
        self.phase = Phase::Stopped;
        self.outstanding = None;
        self.release(held, out);
        self.lost.clear();
        out.push(Effect::Stop(why));
    }

    fn release(&mut self, names: impl IntoIterator<Item = WorkName>, out: &mut Vec<Effect>) {
        let names: BTreeSet<WorkName> = names.into_iter().collect();
        for name in &names {
            self.findings.remove(name);
            self.lost.remove(name);
        }
        if !names.is_empty() {
            out.push(Effect::Release(names.into_iter().collect()));
        }
    }
}
