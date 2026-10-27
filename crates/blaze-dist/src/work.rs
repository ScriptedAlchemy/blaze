//! The coordinator's work set of one execution: section 4's claimed discovery, "(b′)".
//!
//! Every executor of an execution loads the same front end and posts a [`RootSet`]. The lead's is
//! authoritative; a helper that joins first is provisional and compiles speculatively. Work is
//! handed out as grants under leases: half-open ranges of the sorted roots, and the names a
//! holder found. A batch's [`BatchResult`] commits in one transition: its items' outcomes, the
//! names its items found, the requests without a stable name as terminal `Edge`s, its packs, and
//! the work it gives back. A result whose lease is not its holder's live one is refused whole.
//!
//! Compute is at least once after a holder dies; the accounting is exactly once: each root index,
//! name and edge ends terminal once, either from one holder's committed result or in the lead's
//! tail, where the lead compiles it. Nothing a holder was given is dropped: a lease that ends
//! returns its work to the pool, to another finder or to the tail.
//!
//! The machine has no clock, IO or threads of its own: every call takes `now`, answers with a
//! wire type or a [`Refusal`], and appends what happened to `out`, the log a driver keeps.

use std::collections::{BTreeMap, BTreeSet};

use blaze_wire::{
    Attempt, BatchResult, Digest, EdgeKey, EntryKey, ExecutionId, Found, Grant, ItemOutcome,
    ItemResult, Lease, PackRef, RootSet, Set, Why, WorkJoinAnswer, WorkJoinRequest, WorkKey,
    WorkLeaseAnswer, WorkLeaseRequest, WorkName, WorkResultAnswer, WorkersEventsAnswer,
};

use crate::{Millis, Ranges, Refusal};

/// The reason of a `Lost` outcome whose holder knows it held the finding and no longer does.
pub const LOST_FINDING: &str = "lost-finding";
/// The reason of a `Lost` outcome for a name in neither the holder's findings nor its
/// pre-index. Also counted as a conflict.
pub const UNKNOWN_NAME: &str = "unknown-name";
/// The reason the coordinator gives an item reported with requests that have no stable name:
/// its entry cannot name its callee, so it is `Local`.
pub const UNNAMED_REQUEST: &str = "unnamed request";

/// The work set's timings and sizes. The timings are section 4's starting values, to be
/// replaced by D1's measurement (**unverified**).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkConfig {
    /// A holder is lost when nothing was heard from it for this long; its lease ends with it.
    pub lease_ms: u64,
    /// The progress timeout of a lease before the execution has a median batch time.
    pub progress_first_ms: u64,
    /// The progress timeout is this many median batch times once there is a median.
    pub progress_factor: u64,
    /// The progress timeout is never shorter than this once there is a median.
    pub progress_floor_ms: u64,
    /// A root or name whose lease was revoked this many times goes to the lead's tail.
    pub revocations_to_tail: u32,
    /// The most root indexes one grant holds.
    pub grant_roots: u64,
    /// The most names one grant holds.
    pub grant_names: usize,
    /// The most roots a root set may count: a bound on what a join can make the coordinator
    /// allocate.
    pub max_roots: u64,
}

impl Default for WorkConfig {
    fn default() -> WorkConfig {
        WorkConfig {
            lease_ms: 60_000,
            progress_first_ms: 300_000,
            progress_factor: 10,
            progress_floor_ms: 120_000,
            revocations_to_tail: 3,
            grant_roots: 4_096,
            grant_names: 4_096,
            max_roots: 1 << 24,
        }
    }
}

/// Which accepted result a terminal item came from.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Commit {
    /// The holder whose result it was.
    pub holder: String,
    /// The lease generation it was committed under.
    pub generation: u64,
    /// The holder's batch number.
    pub seq: u64,
}

/// Why an item went to the lead's tail: the lead compiles it in its assembly.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum TailWhy {
    /// Its lease was revoked [`WorkConfig::revocations_to_tail`] times.
    Revoked,
    /// No finder that still holds its finding is left.
    NoFinder,
    /// A holder granted it could not resolve it (`unknown-name`); counted as a conflict.
    UnknownName,
    /// Two finders reported it with different `tuple`s: an identity conflict, counted as a
    /// determinism bug with both.
    Conflict,
    /// The lead's daemon rejects it.
    Rejected,
    /// It was outstanding when the lead was lost: findings of a dead attempt are invalid.
    LeadLost,
    /// A request with no stable name: a terminal `WorkKey::Edge` of its parent, with the reason.
    Unnamed(String),
    /// A root item answered `Lost`: a holder resolves roots by index, so it is not one the
    /// protocol makes, and unknown is `Local`.
    LostRoot,
}

/// How a work item ended.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Terminal {
    /// One holder's result, accepted under its live lease. A `Local` outcome here is the
    /// exporter's: compiled, not exportable; the lead compiles it and what calls it.
    Committed {
        /// The outcome, as committed.
        outcome: ItemOutcome,
        /// The result it came from.
        by: Commit,
    },
    /// The lead's tail, decided by the coordinator, with no result behind it.
    Tail(TailWhy),
}

impl Terminal {
    /// Whether the lead compiles it: a tail item, or one committed `Local`.
    pub fn is_local(&self) -> bool {
        matches!(
            self,
            Terminal::Tail(_) | Terminal::Committed { outcome: ItemOutcome::Local(_), .. }
        )
    }
}

/// Why a lease or a holder ended before its result was committed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum FenceWhy {
    /// Nothing was heard from the holder for [`WorkConfig::lease_ms`].
    Expired,
    /// Heartbeats arrived but no batch was committed within the progress timeout.
    Hung,
    /// The holder sent a result that does not follow the protocol's shape.
    Malformed,
    /// The execution's attempt moved: the lead was lost.
    AttemptMoved,
    /// The holder's root set differs from the lead's.
    RootsDisagree,
    /// The execution ended.
    Ended,
}

impl FenceWhy {
    /// Whether the end counts against the work it held: a revocation.
    fn counts(self) -> bool {
        matches!(self, FenceWhy::Expired | FenceWhy::Hung | FenceWhy::Malformed)
    }
}

/// How an execution ended without its compile phase done.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum EndWhy {
    /// Its deadline passed: `Cancelled`, coverage not claimed.
    Deadline,
    /// Its last subscriber left.
    Cancelled,
    /// No compatible lead: `Failed`.
    NoLead,
    /// The lead was lost twice: `Lost`.
    LeadLostTwice,
    /// The lead reported the job's outcome before the compile phase was done: a refusal, or
    /// a failure the lead's assembly found.
    Reported,
}

/// What a call did, in the order it did it: the coordinator's log.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Effect {
    /// The root set the execution's work is counted against is fixed: the lead's.
    Fixed {
        /// The lead's root set.
        roots: RootSet,
    },
    /// The lead's root set differed from the one the work set held: everything committed under
    /// the old one is dropped and the work set starts again on the lead's.
    Reset {
        /// The root epoch that starts.
        epoch: u64,
        /// The lead's root set.
        roots: RootSet,
    },
    /// An executor's root set differs from the lead's: a determinism bug, with both digests.
    RootsDisagree {
        /// The executor.
        holder: String,
        /// The lead's.
        lead: RootSet,
        /// The executor's.
        other: RootSet,
    },
    /// A grant was made under a new lease generation.
    Granted {
        /// Its holder.
        holder: String,
        /// The new generation.
        generation: u64,
        /// The root indexes it holds.
        roots: Ranges,
        /// The names it holds, in set order.
        names: Vec<WorkName>,
    },
    /// A batch's result was committed.
    Committed {
        /// The result.
        by: Commit,
        /// The root epoch it was committed in.
        epoch: u64,
    },
    /// A result committed before was sent again and answered as before.
    Replayed {
        /// The result.
        by: Commit,
    },
    /// A call was refused.
    Refused {
        /// Who called.
        holder: String,
        /// Why.
        refusal: Refusal,
    },
    /// A lease ended without its result: its generation is dead.
    Fenced {
        /// Its holder.
        holder: String,
        /// Its generation.
        generation: u64,
        /// Why.
        why: FenceWhy,
    },
    /// A holder was dropped: its findings are no longer counted on.
    HolderLost {
        /// The holder.
        holder: String,
        /// Why.
        why: FenceWhy,
    },
    /// Two finders reported one name with different `tuple`s.
    Conflict {
        /// The name.
        name: WorkName,
        /// The first finder's tuple.
        first: Digest,
        /// The other's.
        other: Digest,
    },
    /// An entry exported under a name that became a conflict or was rejected by the lead:
    /// the lead bans it.
    ExportInvalidated {
        /// The name.
        name: WorkName,
        /// The entry.
        entry: EntryKey,
    },
    /// Every root, name and edge is terminal and no lease holds work.
    Done,
    /// The execution ended without its compile phase done.
    Ended {
        /// Why.
        why: EndWhy,
    },
}

/// What the work set counted.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Counters {
    /// Grants made.
    pub grants: u64,
    /// Results committed.
    pub commits: u64,
    /// Results answered from the replay record.
    pub replays: u64,
    /// Results refused because their lease was not the holder's live one.
    pub dead_leases: u64,
    /// Results refused as malformed.
    pub malformed: u64,
    /// Leases ended without a result, by expiry, progress timeout or malformation.
    pub revocations: u64,
    /// Holders dropped as lost.
    pub holders_lost: u64,
    /// Identity conflicts, `unknown-name` answers included.
    pub conflicts: u64,
    /// `unknown-name` answers.
    pub unknown_names: u64,
    /// `lost-finding` answers.
    pub lost_findings: u64,
    /// Executors whose root set differed from the lead's.
    pub root_disagreements: u64,
    /// Restarts of the work set on the lead's root list.
    pub resets: u64,
    /// Items committed `Exported` with unnamed requests, kept as `Local`.
    pub unnamed_forced_local: u64,
}

/// The state of every work item at one moment, for a driver's report and a checker.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Report {
    /// The root set the work is counted against, when the lead's has fixed it.
    pub fixed: Option<RootSet>,
    /// The root epoch: how many times the work set started again on the lead's root list.
    pub epoch: u64,
    /// Each root index's end, or `None` while it is outstanding.
    pub roots: Vec<Option<Terminal>>,
    /// Each name's first tuple and end.
    pub names: BTreeMap<WorkName, (Digest, Option<Terminal>)>,
    /// Each edge's parent, ordinal and end.
    pub edges: BTreeMap<WorkName, (WorkName, u32, Terminal)>,
    /// The packs of committed results.
    pub packs: BTreeSet<PackRef>,
    /// Whether the compile phase is done.
    pub done: bool,
}

/// One root index or name.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Slot {
    /// In no lease: a root in the pool, a name waiting for a finder's next grant.
    Waiting,
    /// In the live lease of this generation.
    Held(u64),
    /// Terminal.
    Done(Terminal),
}

#[derive(Clone, Debug)]
struct RootSlot {
    revoked: u32,
    state: Slot,
}

#[derive(Clone, Debug)]
struct NameSlot {
    /// The first finder's tuple: a finder with another makes the name a conflict.
    tuple: Digest,
    /// Holders that reported it with this tuple and still hold their finding.
    finders: BTreeSet<String>,
    /// The first finder, granted it first while it holds it.
    prefer: Option<String>,
    revoked: u32,
    state: Slot,
}

#[derive(Clone, Debug)]
struct EdgeSlot {
    parent: WorkName,
    ordinal: u32,
    why: String,
}

#[derive(Clone, Debug)]
struct HolderSlot {
    roots: RootSet,
    heard: Millis,
    lease: Option<u64>,
    /// The names it is a finder of, this attempt.
    findings: BTreeSet<WorkName>,
}

#[derive(Clone, Debug)]
struct Issued {
    holder: String,
    roots: Ranges,
    names: Vec<WorkName>,
    granted_at: Millis,
    live: bool,
    grant: Grant,
}

/// Where the execution's compile phase is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    Open,
    Done,
    Ended(EndWhy),
}

/// The work set of one execution.
#[derive(Clone, Debug)]
pub struct WorkSet {
    config: WorkConfig,
    execution: ExecutionId,
    attempt: Attempt,
    lead: String,
    phase: Phase,
    fixed: Option<RootSet>,
    provisional: Option<RootSet>,
    epoch: u64,
    rejected: BTreeSet<WorkName>,
    roots: Vec<RootSlot>,
    pool: Ranges,
    names: BTreeMap<WorkName, NameSlot>,
    waiting: BTreeSet<WorkName>,
    edges: BTreeMap<WorkName, EdgeSlot>,
    holders: BTreeMap<String, HolderSlot>,
    /// Executors refused this attempt for a root set that differs from the lead's.
    refused: BTreeMap<String, RootSet>,
    leases: BTreeMap<u64, Issued>,
    live: BTreeSet<u64>,
    next_generation: u64,
    replies: BTreeMap<(String, u64, u64), (Digest, WorkResultAnswer)>,
    /// Committed batches' times from grant to commit, sorted, for the median.
    batch_ms: Vec<u64>,
    packs: BTreeSet<PackRef>,
    counters: Counters,
}

impl WorkSet {
    /// The work set of `execution` at `attempt`, whose lead is `lead`.
    pub fn new(execution: ExecutionId, attempt: Attempt, lead: &str, config: WorkConfig) -> Self {
        WorkSet {
            config,
            execution,
            attempt,
            lead: lead.to_owned(),
            phase: Phase::Open,
            fixed: None,
            provisional: None,
            epoch: 0,
            rejected: BTreeSet::new(),
            roots: Vec::new(),
            pool: Ranges::new(),
            names: BTreeMap::new(),
            waiting: BTreeSet::new(),
            edges: BTreeMap::new(),
            holders: BTreeMap::new(),
            refused: BTreeMap::new(),
            leases: BTreeMap::new(),
            live: BTreeSet::new(),
            next_generation: 1,
            replies: BTreeMap::new(),
            batch_ms: Vec::new(),
            packs: BTreeSet::new(),
            counters: Counters::default(),
        }
    }

    /// The execution's current attempt.
    pub fn attempt(&self) -> Attempt {
        self.attempt
    }

    /// The current attempt's lead.
    pub fn lead(&self) -> &str {
        &self.lead
    }

    /// Whether the compile phase is done: every root, name and edge terminal, no lease live.
    pub fn is_done(&self) -> bool {
        self.phase == Phase::Done
    }

    /// The root-list epoch, advanced when a lead replaces the list the work was counted on.
    pub fn root_epoch(&self) -> u64 {
        self.epoch
    }

    /// Whether this attempt's lead has joined with its own root set.
    pub fn lead_joined(&self) -> bool {
        self.holders.contains_key(&self.lead)
    }

    /// How the execution ended, if it ended before its compile phase was done.
    pub fn ended(&self) -> Option<EndWhy> {
        match self.phase {
            Phase::Ended(why) => Some(why),
            _ => None,
        }
    }

    /// What the work set counted.
    pub fn counters(&self) -> &Counters {
        &self.counters
    }

    /// When the next lease ends or times out, for a driver's timer.
    pub fn next_deadline(&self) -> Option<Millis> {
        if self.phase != Phase::Open {
            return None;
        }
        let heard = self.holders.values().map(|h| h.heard + self.config.lease_ms + 1);
        let timeout = self.progress_timeout();
        let progress = self.live.iter().map(|g| self.leases[g].granted_at + timeout + 1);
        heard.chain(progress).min()
    }

    /// `POST /v1/work/join` from `from`, the executor the capability names.
    pub fn join(
        &mut self,
        now: Millis,
        from: &str,
        req: &WorkJoinRequest,
        out: &mut Vec<Effect>,
    ) -> Result<WorkJoinAnswer, Refusal> {
        self.tick(now, out);
        let answer = self.join_inner(now, from, req, out);
        if let Err(refusal) = &answer {
            out.push(Effect::Refused { holder: from.to_owned(), refusal: refusal.clone() });
        }
        answer
    }

    fn join_inner(
        &mut self,
        now: Millis,
        from: &str,
        req: &WorkJoinRequest,
        out: &mut Vec<Effect>,
    ) -> Result<WorkJoinAnswer, Refusal> {
        if req.execution != self.execution {
            return Err(Refusal::NoExecution);
        }
        if let Phase::Ended(_) = self.phase {
            return Err(Refusal::Ended);
        }
        if req.attempt != self.attempt {
            return Err(Refusal::AttemptMoved);
        }
        if req.roots.count > self.config.max_roots {
            return Err(Refusal::Malformed(format!(
                "a root set of {} roots, past {}",
                req.roots.count, self.config.max_roots
            )));
        }
        if let Some(holder) = self.holders.get_mut(from) {
            // A join sent again is answered again.
            if holder.roots == req.roots {
                holder.heard = now;
                return Ok(self.join_answer());
            }
            // A front end that changed under findings held is a new process: a source change
            // is a new execution, not a reload, so what it held is dropped.
            self.lose(from, FenceWhy::RootsDisagree, out);
        }
        if from == self.lead {
            self.join_lead(req, out);
        } else {
            if self.refused.contains_key(from) {
                return Err(Refusal::FrontEndDiffers);
            }
            match (&self.fixed, &self.provisional) {
                (Some(fixed), _) if *fixed != req.roots => {
                    self.counters.root_disagreements += 1;
                    out.push(Effect::RootsDisagree {
                        holder: from.to_owned(),
                        lead: fixed.clone(),
                        other: req.roots.clone(),
                    });
                    self.refused.insert(from.to_owned(), req.roots.clone());
                    return Err(Refusal::FrontEndDiffers);
                }
                (Some(_), _) => {}
                (None, Some(provisional)) if *provisional != req.roots => {
                    return Err(Refusal::NotYet);
                }
                (None, Some(_)) => {}
                (None, None) => {
                    // The first helper's root set is provisional: it compiles speculatively
                    // until the lead's arrives.
                    self.provisional = Some(req.roots.clone());
                    self.start_roots(req.roots.count);
                }
            }
        }
        self.holders.insert(
            from.to_owned(),
            HolderSlot {
                roots: req.roots.clone(),
                heard: now,
                lease: None,
                findings: BTreeSet::new(),
            },
        );
        Ok(self.join_answer())
    }

    /// The lead's join: its root set is the execution's, whatever the work set held.
    fn join_lead(&mut self, req: &WorkJoinRequest, out: &mut Vec<Effect>) {
        self.rejected = req.rejected.iter().copied().collect();
        let held = self.fixed.clone().or_else(|| self.provisional.clone());
        match held {
            Some(held) if held == req.roots => {
                if self.fixed.is_none() {
                    self.fixed = Some(held);
                    self.provisional = None;
                    out.push(Effect::Fixed { roots: req.roots.clone() });
                }
            }
            Some(_) => self.reset(&req.roots, out),
            None => {
                self.start_roots(req.roots.count);
                self.fixed = Some(req.roots.clone());
                out.push(Effect::Fixed { roots: req.roots.clone() });
            }
        }
        // Helpers may discover and even finish names before the lead loads. Its rejection
        // list applies to those provisional findings too, including entries already sealed.
        for name in &self.rejected {
            let Some(slot) = self.names.get_mut(name) else { continue };
            if let Slot::Done(Terminal::Committed {
                outcome: ItemOutcome::Exported(entry), ..
            }) = &slot.state
            {
                out.push(Effect::ExportInvalidated { name: *name, entry: entry.entry });
            }
            self.waiting.remove(name);
            slot.state = Slot::Done(Terminal::Tail(TailWhy::Rejected));
        }
        self.check_done(out);
    }

    /// Starts again on the lead's root list: every executor that joined with the other root
    /// set is fenced and refused, what was committed under it is dropped, and the difference
    /// is counted as a determinism bug.
    fn reset(&mut self, roots: &RootSet, out: &mut Vec<Effect>) {
        let holders: Vec<String> = self.holders.keys().cloned().collect();
        for holder in holders {
            let theirs = self.holders[&holder].roots.clone();
            if holder != self.lead {
                self.counters.root_disagreements += 1;
                out.push(Effect::RootsDisagree {
                    holder: holder.clone(),
                    lead: roots.clone(),
                    other: theirs.clone(),
                });
                self.refused.insert(holder.clone(), theirs);
            }
            self.lose(&holder, FenceWhy::RootsDisagree, out);
        }
        let live: Vec<u64> = self.live.iter().copied().collect();
        for generation in live {
            self.fence(generation, FenceWhy::RootsDisagree, out);
        }
        self.names.clear();
        self.waiting.clear();
        self.edges.clear();
        self.replies.clear();
        self.packs.clear();
        self.batch_ms.clear();
        self.epoch += 1;
        self.counters.resets += 1;
        // A new root list is new work, even after the old one was done.
        self.phase = Phase::Open;
        self.start_roots(roots.count);
        self.fixed = Some(roots.clone());
        self.provisional = None;
        out.push(Effect::Reset { epoch: self.epoch, roots: roots.clone() });
    }

    fn start_roots(&mut self, count: u64) {
        let len = usize::try_from(count).expect("bounded by max_roots");
        self.roots = vec![RootSlot { revoked: 0, state: Slot::Waiting }; len];
        self.pool = Ranges::full(count);
    }

    fn join_answer(&self) -> WorkJoinAnswer {
        let roots = self.fixed.clone().or_else(|| self.provisional.clone()).expect("joined");
        WorkJoinAnswer { roots, fixed: self.fixed.is_some() }
    }

    /// `POST /v1/work/lease` from `from`: its live lease's grant if it has one, else a new
    /// grant, names first, or nothing.
    pub fn lease(
        &mut self,
        now: Millis,
        from: &str,
        req: &WorkLeaseRequest,
        out: &mut Vec<Effect>,
    ) -> Result<WorkLeaseAnswer, Refusal> {
        self.tick(now, out);
        let answer = self.lease_inner(now, from, req, out);
        if let Err(refusal) = &answer {
            out.push(Effect::Refused { holder: from.to_owned(), refusal: refusal.clone() });
        }
        answer
    }

    fn lease_inner(
        &mut self,
        now: Millis,
        from: &str,
        req: &WorkLeaseRequest,
        out: &mut Vec<Effect>,
    ) -> Result<WorkLeaseAnswer, Refusal> {
        if req.execution != self.execution {
            return Err(Refusal::NoExecution);
        }
        if let Phase::Ended(_) = self.phase {
            return Err(Refusal::Ended);
        }
        if req.attempt != self.attempt {
            return Err(Refusal::AttemptMoved);
        }
        let Some(holder) = self.holders.get_mut(from) else {
            return Err(Refusal::NotJoined);
        };
        holder.heard = now;
        if let Some(generation) = holder.lease {
            // A lease asked for again, or by a request the network repeated: the same grant.
            return Ok(WorkLeaseAnswer { grant: Some(self.leases[&generation].grant.clone()) });
        }
        if self.phase == Phase::Done {
            return Ok(WorkLeaseAnswer { grant: None });
        }
        Ok(WorkLeaseAnswer { grant: self.grant(now, from, out) })
    }

    fn grant(&mut self, now: Millis, holder: &str, out: &mut Vec<Effect>) -> Option<Grant> {
        // Names come before roots, so a finder compiles what it found while its database holds
        // it. A name goes to its first finder while that one holds it, else to any finder.
        let names: Vec<WorkName> = self.holders[holder]
            .findings
            .iter()
            .filter(|name| self.waiting.contains(*name))
            .filter(|name| {
                let slot = &self.names[*name];
                slot.prefer.is_none() || slot.prefer.as_deref() == Some(holder)
            })
            .take(self.config.grant_names)
            .copied()
            .collect();
        let roots = self.pool.take_front(self.config.grant_roots);
        if names.is_empty() && roots.is_empty() {
            return None;
        }
        let generation = self.next_generation;
        self.next_generation += 1;
        for index in roots.indexes() {
            self.roots[index as usize].state = Slot::Held(generation);
        }
        for name in &names {
            self.waiting.remove(name);
            self.names.get_mut(name).expect("waiting").state = Slot::Held(generation);
        }
        let names = Set::new(names);
        let grant = Grant {
            lease: Lease {
                execution: self.execution,
                attempt: self.attempt,
                holder: holder.to_owned(),
                generation,
            },
            roots: roots.to_wire(),
            names: names.clone(),
        };
        self.leases.insert(
            generation,
            Issued {
                holder: holder.to_owned(),
                roots: roots.clone(),
                names: names.iter().copied().collect(),
                granted_at: now,
                live: true,
                grant: grant.clone(),
            },
        );
        self.live.insert(generation);
        self.holders.get_mut(holder).expect("joined").lease = Some(generation);
        self.counters.grants += 1;
        out.push(Effect::Granted {
            holder: holder.to_owned(),
            generation,
            roots,
            names: names.into_vec(),
        });
        Some(grant)
    }

    /// `POST /v1/work/result` from `from`: commits the batch in one transition, or refuses it
    /// whole.
    pub fn result(
        &mut self,
        now: Millis,
        from: &str,
        req: &BatchResult,
        out: &mut Vec<Effect>,
    ) -> Result<WorkResultAnswer, Refusal> {
        self.tick(now, out);
        let answer = self.result_inner(now, from, req, out);
        if let Err(refusal) = &answer {
            out.push(Effect::Refused { holder: from.to_owned(), refusal: refusal.clone() });
        }
        answer
    }

    fn result_inner(
        &mut self,
        now: Millis,
        from: &str,
        req: &BatchResult,
        out: &mut Vec<Effect>,
    ) -> Result<WorkResultAnswer, Refusal> {
        if req.lease.execution != self.execution {
            return Err(Refusal::NoExecution);
        }
        if req.lease.holder != from {
            return Err(Refusal::NotHolder);
        }
        if req.lease.attempt != self.attempt {
            return Err(Refusal::AttemptMoved);
        }
        let generation = req.lease.generation;
        let by = Commit { holder: from.to_owned(), generation, seq: req.seq };
        let key = (from.to_owned(), generation, req.seq);
        let digest = blaze_wire::digest_of(req);
        // A result committed in this attempt gets the same answer after its lease ended.
        // An earlier attempt must not replay or keep a newly joined holder alive.
        if let Some((committed, answer)) = self.replies.get(&key) {
            if *committed != digest {
                return Err(Refusal::SeqReused);
            }
            let answer = answer.clone();
            self.counters.replays += 1;
            if let Some(holder) = self.holders.get_mut(from) {
                holder.heard = now;
            }
            out.push(Effect::Replayed { by });
            return Ok(answer);
        }
        if let Phase::Ended(_) = self.phase {
            return Err(Refusal::Ended);
        }
        // The fence: only the holder's live lease commits. A result of a dead generation is
        // refused whole, `found` and `unnamed` included, so it registers no names.
        let live = self.holders.get(from).and_then(|h| h.lease) == Some(generation)
            && self.leases.get(&generation).is_some_and(|issued| issued.live);
        if !live {
            self.counters.dead_leases += 1;
            return Err(Refusal::DeadLease);
        }
        if let Some(holder) = self.holders.get_mut(from) {
            holder.heard = now;
        }
        let issued = self.leases[&generation].clone();
        let (processed_roots, processed_names, returned_roots, returned_names) =
            match shape(&issued, req) {
                Ok(shape) => shape,
                Err(what) => {
                    self.counters.malformed += 1;
                    self.fence(generation, FenceWhy::Malformed, out);
                    self.check_done(out);
                    return Err(Refusal::Malformed(what));
                }
            };
        let roots_done = processed_roots.len() as usize;
        let mut granted = BTreeSet::new();
        let mut conflicts = BTreeSet::new();
        for (item, index) in req.items[..roots_done].iter().zip(processed_roots.indexes()) {
            let terminal = match &item.outcome {
                ItemOutcome::Lost(_) => Terminal::Tail(TailWhy::LostRoot),
                _ => self.committed(item, &by),
            };
            if let Some(slot) = self.roots.get_mut(index as usize) {
                slot.state = Slot::Done(terminal);
            }
        }
        for (item, name) in req.items[roots_done..].iter().zip(&processed_names) {
            self.commit_name(from, item, *name, &by, &mut conflicts, out);
        }
        for item in &req.items {
            if matches!(item.outcome, ItemOutcome::Lost(_)) {
                continue;
            }
            for (ordinal, why) in item.unnamed.iter() {
                let edge =
                    WorkName::of(&WorkKey::Edge(EdgeKey { parent: item.name, ordinal: *ordinal }));
                self.edges.entry(edge).or_insert(EdgeSlot {
                    parent: item.name,
                    ordinal: *ordinal,
                    why: why.clone(),
                });
            }
            for found in item.found.iter() {
                self.register(from, found, &mut granted, &mut conflicts, out);
            }
        }
        for (start, end) in returned_roots.iter() {
            for index in start..end {
                if let Some(slot) = self.roots.get_mut(index as usize) {
                    slot.state = Slot::Waiting;
                }
            }
            self.pool.insert(start, end);
        }
        for name in &returned_names {
            if let Some(slot) = self.names.get_mut(name)
                && slot.state == Slot::Held(generation)
            {
                // Given back unprocessed: its holder still holds the finding.
                slot.state = Slot::Waiting;
                self.waiting.insert(*name);
            }
        }
        self.packs.extend(req.packs.iter().cloned());
        let issued = self.leases.get_mut(&generation).expect("live");
        issued.live = false;
        self.live.remove(&generation);
        if let Some(holder) = self.holders.get_mut(from)
            && holder.lease == Some(generation)
        {
            holder.lease = None;
        }
        let took = now.saturating_sub(issued.granted_at);
        let at = self.batch_ms.partition_point(|&ms| ms <= took);
        self.batch_ms.insert(at, took);
        self.counters.commits += 1;
        out.push(Effect::Committed { by, epoch: self.epoch });
        let answer = WorkResultAnswer {
            granted: Set::new(granted.into_iter().collect()),
            conflicts: Set::new(conflicts.into_iter().collect()),
        };
        self.replies.insert(key, (digest, answer.clone()));
        self.check_done(out);
        Ok(answer)
    }

    /// The terminal state of an item a live lease's result committed.
    fn committed(&mut self, item: &ItemResult, by: &Commit) -> Terminal {
        let outcome = match &item.outcome {
            ItemOutcome::Exported(_) | ItemOutcome::FromStore(_) if !item.unnamed.is_empty() => {
                // Its entry could not name its callee: the lead compiles it.
                self.counters.unnamed_forced_local += 1;
                ItemOutcome::Local(Why { why: UNNAMED_REQUEST.to_owned() })
            }
            outcome => outcome.clone(),
        };
        Terminal::Committed { outcome, by: by.clone() }
    }

    fn commit_name(
        &mut self,
        from: &str,
        item: &ItemResult,
        name: WorkName,
        by: &Commit,
        conflicts: &mut BTreeSet<WorkName>,
        out: &mut Vec<Effect>,
    ) {
        let Some(state) = self.names.get(&name).map(|slot| slot.state.clone()) else { return };
        match state {
            Slot::Held(generation) if generation == by.generation => {}
            Slot::Done(Terminal::Tail(TailWhy::Conflict | TailWhy::Rejected)) => {
                // It became a conflict or was rejected while held: the outcome is not
                // taken, and an entry exported under it is banned. Acknowledge that the
                // name must be dropped even when this result could not resolve it.
                conflicts.insert(name);
                if let ItemOutcome::Exported(entry) = &item.outcome {
                    out.push(Effect::ExportInvalidated { name, entry: entry.entry });
                }
                return;
            }
            // A processed name of a live lease is held by it; anything else is not this
            // result's to change. The checker holds a coordinator that gets here wrong.
            _ => return,
        }
        let terminal = match &item.outcome {
            ItemOutcome::Lost(why) => {
                self.drop_finder(from, name);
                let slot = &self.names[&name];
                if why.why == LOST_FINDING {
                    self.counters.lost_findings += 1;
                    if slot.finders.is_empty() {
                        Slot::Done(Terminal::Tail(TailWhy::NoFinder))
                    } else {
                        self.waiting.insert(name);
                        Slot::Waiting
                    }
                } else {
                    // A name the holder cannot resolve at all: never dropped, the lead's.
                    self.counters.unknown_names += 1;
                    self.counters.conflicts += 1;
                    conflicts.insert(name);
                    Slot::Done(Terminal::Tail(TailWhy::UnknownName))
                }
            }
            _ => Slot::Done(self.committed(item, by)),
        };
        self.names.get_mut(&name).expect("held").state = terminal;
    }

    /// A name an item found: a new one is granted to its finder, an equal one adds a finder,
    /// another `tuple` makes it a conflict.
    fn register(
        &mut self,
        from: &str,
        found: &Found,
        granted: &mut BTreeSet<WorkName>,
        conflicts: &mut BTreeSet<WorkName>,
        out: &mut Vec<Effect>,
    ) {
        let name = found.name;
        let Some(slot) = self.names.get_mut(&name) else {
            let rejected = self.rejected.contains(&name);
            let state = if rejected {
                Slot::Done(Terminal::Tail(TailWhy::Rejected))
            } else {
                self.waiting.insert(name);
                granted.insert(name);
                if let Some(holder) = self.holders.get_mut(from) {
                    holder.findings.insert(name);
                }
                Slot::Waiting
            };
            let finders =
                if rejected { BTreeSet::new() } else { BTreeSet::from([from.to_owned()]) };
            self.names.insert(
                name,
                NameSlot {
                    tuple: found.tuple,
                    finders,
                    prefer: (!rejected).then(|| from.to_owned()),
                    revoked: 0,
                    state,
                },
            );
            return;
        };
        if slot.tuple == found.tuple {
            if !matches!(slot.state, Slot::Done(_)) {
                slot.finders.insert(from.to_owned());
                if let Some(holder) = self.holders.get_mut(from) {
                    holder.findings.insert(name);
                }
            }
            return;
        }
        conflicts.insert(name);
        if slot.state == Slot::Done(Terminal::Tail(TailWhy::Conflict)) {
            return;
        }
        self.counters.conflicts += 1;
        out.push(Effect::Conflict { name, first: slot.tuple, other: found.tuple });
        if let Slot::Done(Terminal::Committed { outcome: ItemOutcome::Exported(entry), .. }) =
            &slot.state
        {
            out.push(Effect::ExportInvalidated { name, entry: entry.entry });
        }
        if slot.state == Slot::Waiting {
            self.waiting.remove(&name);
        }
        slot.state = Slot::Done(Terminal::Tail(TailWhy::Conflict));
    }

    /// `holder` no longer holds its finding of `name`.
    fn drop_finder(&mut self, holder: &str, name: WorkName) {
        let slot = self.names.get_mut(&name).expect("registered");
        slot.finders.remove(holder);
        if slot.prefer.as_deref() == Some(holder) {
            slot.prefer = None;
        }
        if let Some(h) = self.holders.get_mut(holder) {
            h.findings.remove(&name);
        }
    }

    /// A heartbeat on `POST /v1/workers/events`: the holder was heard. The answer stops a
    /// helper once the compile phase is done, and every executor of a dead attempt or an ended
    /// execution.
    pub fn heartbeat(
        &mut self,
        now: Millis,
        from: &str,
        attempt: Attempt,
        out: &mut Vec<Effect>,
    ) -> WorkersEventsAnswer {
        self.tick(now, out);
        if matches!(self.phase, Phase::Ended(_)) || attempt != self.attempt {
            return WorkersEventsAnswer::CANCEL;
        }
        if let Some(holder) = self.holders.get_mut(from) {
            holder.heard = now;
        }
        if self.phase == Phase::Done && from != self.lead {
            return WorkersEventsAnswer::CANCEL;
        }
        WorkersEventsAnswer::CONTINUE
    }

    /// Ends what has run out of time: holders not heard from for the lease time, and leases
    /// past their progress timeout. Every call runs it first, so a lease that ended is dead
    /// whether or not a timer fired.
    pub fn tick(&mut self, now: Millis, out: &mut Vec<Effect>) {
        if self.phase != Phase::Open {
            return;
        }
        let silent: Vec<String> = self
            .holders
            .iter()
            .filter(|(_, h)| now > h.heard + self.config.lease_ms)
            .map(|(id, _)| id.clone())
            .collect();
        for holder in silent {
            self.lose(&holder, FenceWhy::Expired, out);
        }
        let timeout = self.progress_timeout();
        let hung: Vec<String> = self
            .live
            .iter()
            .map(|g| &self.leases[g])
            .filter(|issued| now > issued.granted_at + timeout)
            .map(|issued| issued.holder.clone())
            .collect();
        for holder in hung {
            self.lose(&holder, FenceWhy::Hung, out);
        }
        self.check_done(out);
    }

    /// 300 s from the grant until the execution has a median batch time, then ten medians,
    /// at least 120 s.
    fn progress_timeout(&self) -> u64 {
        match self.batch_ms.get(self.batch_ms.len() / 2) {
            None => self.config.progress_first_ms,
            Some(&median) => {
                (median * self.config.progress_factor).max(self.config.progress_floor_ms)
            }
        }
    }

    /// Drops a holder: its live lease is fenced, and the names it found go to other finders or
    /// to the lead's tail.
    fn lose(&mut self, holder: &str, why: FenceWhy, out: &mut Vec<Effect>) {
        let Some(slot) = self.holders.get(holder) else { return };
        if let Some(generation) = slot.lease {
            self.fence(generation, why, out);
        }
        let slot = self.holders.remove(holder).expect("present");
        for name in slot.findings {
            let Some(n) = self.names.get_mut(&name) else { continue };
            n.finders.remove(holder);
            if n.prefer.as_deref() == Some(holder) {
                n.prefer = None;
            }
            if n.state == Slot::Waiting && n.finders.is_empty() {
                self.waiting.remove(&name);
                n.state = Slot::Done(Terminal::Tail(TailWhy::NoFinder));
            }
        }
        self.counters.holders_lost += 1;
        out.push(Effect::HolderLost { holder: holder.to_owned(), why });
    }

    /// Ends a live lease without its result: its roots go back to the pool and its names to
    /// another finder, each counted as revoked when the end is the holder's; past the bound,
    /// to the lead's tail.
    fn fence(&mut self, generation: u64, why: FenceWhy, out: &mut Vec<Effect>) {
        let Some(issued) = self.leases.get_mut(&generation) else { return };
        if !issued.live {
            return;
        }
        issued.live = false;
        let issued = issued.clone();
        self.live.remove(&generation);
        if let Some(h) = self.holders.get_mut(&issued.holder)
            && h.lease == Some(generation)
        {
            h.lease = None;
        }
        if why.counts() {
            self.counters.revocations += 1;
        }
        out.push(Effect::Fenced { holder: issued.holder.clone(), generation, why });
        let bound = self.config.revocations_to_tail;
        for index in issued.roots.indexes() {
            let slot = &mut self.roots[index as usize];
            if slot.state != Slot::Held(generation) {
                continue;
            }
            if why.counts() {
                slot.revoked += 1;
            }
            if slot.revoked >= bound {
                slot.state = Slot::Done(Terminal::Tail(TailWhy::Revoked));
            } else {
                slot.state = Slot::Waiting;
                self.pool.insert(index, index + 1);
            }
        }
        for name in &issued.names {
            if self.names[name].state != Slot::Held(generation) {
                continue;
            }
            self.drop_finder(&issued.holder, *name);
            let slot = self.names.get_mut(name).expect("held");
            if why.counts() {
                slot.revoked += 1;
            }
            if slot.revoked >= bound {
                slot.state = Slot::Done(Terminal::Tail(TailWhy::Revoked));
            } else if slot.finders.is_empty() {
                slot.state = Slot::Done(Terminal::Tail(TailWhy::NoFinder));
            } else {
                slot.state = Slot::Waiting;
                self.waiting.insert(*name);
            }
        }
    }

    /// The lead was lost and the execution reassigned at `attempt` with `lead`: every lease of
    /// the old attempt is dead, outstanding roots return to the pool, and every outstanding
    /// name goes to the new lead's tail, since findings of the dead attempt are invalid.
    /// Committed outcomes stay. Every executor joins again under the new attempt.
    pub fn new_attempt(&mut self, attempt: Attempt, lead: &str, out: &mut Vec<Effect>) {
        for slot in self.names.values_mut() {
            slot.finders.clear();
            slot.prefer = None;
            if !matches!(slot.state, Slot::Done(_)) {
                slot.state = Slot::Done(Terminal::Tail(TailWhy::LeadLost));
            }
        }
        self.waiting.clear();
        let live: Vec<u64> = self.live.iter().copied().collect();
        for generation in live {
            self.fence(generation, FenceWhy::AttemptMoved, out);
        }
        for holder in std::mem::take(&mut self.holders).into_keys() {
            self.counters.holders_lost += 1;
            out.push(Effect::HolderLost { holder, why: FenceWhy::AttemptMoved });
        }
        self.refused.clear();
        self.attempt = attempt;
        self.lead = lead.to_owned();
        self.check_done(out);
    }

    /// The execution ended: its deadline, its last subscriber, no lead. Every lease dies and
    /// no coverage is claimed.
    pub fn end(&mut self, why: EndWhy, out: &mut Vec<Effect>) {
        // A compile phase that was done stays done: what ends after it is the job's.
        if self.phase != Phase::Open {
            return;
        }
        let live: Vec<u64> = self.live.iter().copied().collect();
        for generation in live {
            self.fence(generation, FenceWhy::Ended, out);
        }
        self.phase = Phase::Ended(why);
        out.push(Effect::Ended { why });
    }

    fn check_done(&mut self, out: &mut Vec<Effect>) {
        // The root list's length is fixed only by the lead's root set, so an empty work set
        // before it is in does not signal done.
        if self.phase == Phase::Open
            && self.fixed.is_some()
            && self.live.is_empty()
            && self.waiting.is_empty()
            && self.pool.is_empty()
        {
            debug_assert!(self.roots.iter().all(|r| matches!(r.state, Slot::Done(_))));
            debug_assert!(self.names.values().all(|n| matches!(n.state, Slot::Done(_))));
            self.phase = Phase::Done;
            out.push(Effect::Done);
        }
    }

    /// Every work item's state now.
    pub fn report(&self) -> Report {
        let terminal = |slot: &Slot| match slot {
            Slot::Done(t) => Some(t.clone()),
            _ => None,
        };
        Report {
            fixed: self.fixed.clone(),
            epoch: self.epoch,
            roots: self.roots.iter().map(|r| terminal(&r.state)).collect(),
            names: self.names.iter().map(|(n, s)| (*n, (s.tuple, terminal(&s.state)))).collect(),
            edges: self
                .edges
                .iter()
                .map(|(e, s)| {
                    (*e, (s.parent, s.ordinal, Terminal::Tail(TailWhy::Unnamed(s.why.clone()))))
                })
                .collect(),
            packs: self.packs.clone(),
            done: self.phase == Phase::Done,
        }
    }
}

/// The shape of a result against its grant: the processed and returned roots and names, or
/// what is wrong. Items are the processed roots' in ascending index order, then the processed
/// names' in the grant's set order, one each.
#[allow(clippy::type_complexity)]
fn shape(
    issued: &Issued,
    req: &BatchResult,
) -> Result<(Ranges, Vec<WorkName>, Ranges, BTreeSet<WorkName>), String> {
    let returned_roots = Ranges::from_wire(&req.returned_roots)
        .ok_or_else(|| "returned roots overlap or are empty".to_owned())?;
    if !issued.roots.contains_all(&returned_roots) {
        return Err("returned roots that were not granted".to_owned());
    }
    let returned_names: BTreeSet<WorkName> = req.returned_names.iter().copied().collect();
    if !returned_names.iter().all(|n| issued.names.contains(n)) {
        return Err("returned names that were not granted".to_owned());
    }
    let processed_roots = issued.roots.minus(&returned_roots);
    let processed_names: Vec<WorkName> =
        issued.names.iter().filter(|n| !returned_names.contains(n)).copied().collect();
    let want = processed_roots.len() as usize + processed_names.len();
    if req.items.len() != want {
        return Err(format!("{} items for {want} processed", req.items.len()));
    }
    let roots_done = processed_roots.len() as usize;
    for (item, name) in req.items[roots_done..].iter().zip(&processed_names) {
        if item.name != *name {
            return Err("name items out of the grant's order".to_owned());
        }
    }
    Ok((processed_roots, processed_names, returned_roots, returned_names))
}
