//! The work protocol under faults: one execution's coordinator and two to four executors.
//!
//! The coordinator is a [`jobs::Execution`](crate::jobs::Execution), so lead loss and
//! reassignment run through the code E3 drives, with the work set inside it. Each executor runs
//! a [`Holder`] on its own clock and compiles grants of a [`Program`]. Until the fault horizon
//! the network loses, duplicates and delays messages and partitions executors; executors die at
//! any effect; one may be hung, one may have a slow clock, one may have another front end, and
//! findings are dropped. One executor, the survivor, never dies, so the build can always finish.
//!
//! The checker holds, from the coordinator's log and the calls it sees:
//!
//! - **exclusive**: no root index or name is in two live leases at once;
//! - **fenced**: no result is committed for a lease that was fenced, expired by the checker's
//!   own clock, or of another attempt, and none after Done;
//! - **once**: every root index, name and edge ends terminal, from exactly one accepted
//!   result whose item is what the lead's front end compiles, or in the lead's tail;
//! - **unknown**: a name a holder could not resolve ends a conflict or `Local`, never dropped;
//! - **progress**: the compile phase ends `Done` unless the lead was lost twice or no lead was
//!   left; a run that reaches its deadline did not make progress;
//! - **protocol**: a correct holder is never told its calls are malformed.

use core::alloc::Allocator;
use core::fmt;

use blaze_own::{BTreeMap, BTreeSet, Vec, format_in, vec_in};
use blaze_wire::{
    Attempt, BatchResult, ExecutionId, ItemOutcome, ItemResult, JobContent, LocalPolicy, Namespace,
    PackRef, WireAlloc, WireString, WireVec, WorkKey, WorkName, WorkResultAnswer,
};

use super::program::{Divergence, FrontEnd, Program};
use super::{Rng, Violation};
use crate::holder::{self, Call, Compiled, Holder, HolderConfig, Plan, Reply, StopWhy};
use crate::jobs::{self, ExecState, Execution, ExecutionConfig, Subscriber, SubscriberState};
use crate::work::{self, EndWhy, FenceWhy, Terminal, UNKNOWN_NAME, WorkConfig};
use crate::{Millis, Ranges, Refusal};

/// What one run did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Summary {
    /// The seed.
    pub seed: u64,
    /// Whether the compile phase ended `Done`.
    pub done: bool,
    /// How the execution ended otherwise.
    pub ended: Option<EndWhy>,
    /// Events the scheduler ran.
    pub events: u64,
    /// Simulated time at the end.
    pub sim_ms: Millis,
    /// The work set's counters.
    pub counters: work::Counters,
    /// Leads lost.
    pub lead_losses: u32,
    /// Executors that died.
    pub deaths: u32,
}

/// The faults one run injects, drawn from its seed.
#[derive(Clone, Debug)]
pub struct Profile {
    nodes: usize,
    loss_ppm: u32,
    dup_ppm: u32,
    max_delay_ms: u64,
    horizon_ms: Millis,
    survivor: usize,
    divergent: Option<(usize, Divergence)>,
    hung: Option<usize>,
    slow_clock: Option<usize>,
    death_ppm: u32,
    drop_ppm: u32,
    budget_ppm: u32,
    conflicts: bool,
    lead_load_ms: Millis,
}

impl Profile {
    fn draw(rng: &mut Rng) -> Profile {
        let nodes = rng.range_usize(2, 4);
        let survivor = rng.below_usize(nodes);
        // A helper, never the survivor or the lead, gets each of the odd faults.
        let odd = |rng: &mut Rng, ppm: u32| {
            let at = rng.range_usize(1, nodes.saturating_sub(1));
            (rng.chance(ppm) && at != survivor).then_some(at)
        };
        let divergent = odd(rng, 250_000).map(|at| {
            let d = match rng.below(3) {
                0 => Divergence::SwappedTie,
                1 => Divergence::OtherRoot,
                _ => Divergence::Impure,
            };
            (at, d)
        });
        let hung = odd(rng, 100_000);
        let slow_clock = odd(rng, 100_000);
        let calm = rng.chance(100_000);
        Profile {
            nodes,
            loss_ppm: if calm { 0 } else { rng.range_u32(0, 250_000) },
            dup_ppm: if calm { 0 } else { rng.range_u32(0, 150_000) },
            max_delay_ms: rng.range(1, 3_000),
            horizon_ms: rng.range(10_000, 240_000),
            survivor,
            divergent,
            hung,
            slow_clock,
            death_ppm: if calm { 0 } else { rng.range_u32(0, 20_000) },
            drop_ppm: if calm { 0 } else { rng.range_u32(0, 30_000) },
            budget_ppm: rng.range_u32(0, 300_000),
            conflicts: rng.chance(300_000),
            lead_load_ms: rng.range(0, 60_000),
        }
    }
}

const TICK_MS: Millis = 1_000;
const COORD_TICK_MS: Millis = 2_000;
const DRAIN_MS: Millis = 180_000;
const DEADLINE_MS: Millis = 4 * 3_600_000;
const EVENT_CAP: u64 = 2_000_000;
/// How often an idle agent long-polls for an assignment.
const POLL_MS: Millis = 10_000;

struct Node {
    id: WireString,
    alive: bool,
    front: FrontEnd,
    holder: Option<Holder<WireAlloc>>,
    attempt: Attempt,
    lead: bool,
    offset: Millis,
    rate_ppm: u64,
    death_ppm: u32,
    hung: bool,
    partitions: WireVec<(Millis, Millis)>,
    /// Refused for a front end that differs: its agent asks for no more of this execution.
    refused: bool,
    /// When its agent next asks for an assignment, while it has none.
    poll_at: Millis,
}

impl Node {
    fn local(&self, now: Millis) -> Millis {
        self.offset + now * self.rate_ppm / 1_000_000
    }

    fn partitioned(&self, now: Millis) -> Option<Millis> {
        self.partitions.iter().find(|&&(s, e)| now >= s && now < e).map(|&(_, e)| e)
    }
}

#[derive(Clone, Debug)]
enum Kind {
    ToCoord { from: usize, id: u64, call: Call },
    ToNode { to: usize, attempt: Attempt, id: u64, reply: Reply },
    Assign { to: usize, attempt: Attempt, lead: bool, load_ms: Millis },
    Loaded { to: usize, attempt: Attempt },
    Kill { to: usize, attempt: Attempt },
    NodeTick { node: usize },
    CoordTick,
    Compiled { node: usize, attempt: Attempt, batch: Compiled },
}

/// One live lease, as the checker sees it.
#[derive(Clone, Debug)]
struct Shadow {
    holder: WireString,
    roots: Ranges,
    names: WireVec<WorkName>,
    heard: Millis,
}

/// The checker: what the coordinator must never do, from what it can see.
struct Checker {
    alloc: WireAlloc,
    lease_ms: u64,
    live: BTreeMap<u64, Shadow, WireAlloc>,
    dead: BTreeSet<u64, WireAlloc>,
    held_roots: BTreeMap<u64, u64, WireAlloc>,
    held_names: BTreeMap<WorkName, u64, WireAlloc>,
    /// Accepted results of the current root epoch, and their answers.
    accepted: BTreeMap<
        (WireString, u64, u64),
        (BatchResult, WorkResultAnswer, Ranges, WireVec<WorkName>),
        WireAlloc,
    >,
    root_claims: BTreeMap<u64, (WireString, u64, u64), WireAlloc>,
    name_claims: BTreeMap<WorkName, (WireString, u64, u64), WireAlloc>,
    done_at: Option<Millis>,
    attempt: Attempt,
}

/// A checker's verdict: fine, or the invariant broken and what was seen.
type Check = Result<(), (&'static str, WireString)>;

impl Checker {
    fn granted(
        &mut self,
        now: Millis,
        holder: &str,
        g: u64,
        roots: &Ranges,
        names: &[WorkName],
    ) -> Check {
        if self.done_at.is_some() {
            return Err((
                "exclusive",
                format_in!(self.alloc, "a grant {g} to {holder} after Done"),
            ));
        }
        for index in roots.indexes() {
            if let Some(other) = self.held_roots.insert(index, g) {
                return Err((
                    "exclusive",
                    format_in!(self.alloc, "root {index} in live leases {other} and {g}"),
                ));
            }
        }
        for name in names {
            if let Some(other) = self.held_names.insert(*name, g) {
                return Err((
                    "exclusive",
                    format_in!(
                        self.alloc,
                        "name {:?} in live leases {other} and {g}",
                        short(name)
                    ),
                ));
            }
        }
        self.live.insert(
            g,
            Shadow {
                holder: WireString::from_str_in(holder, self.alloc),
                roots: roots.clone(),
                names: Vec::from_slice_in(names, self.alloc),
                heard: now,
            },
        );
        Ok(())
    }

    fn release(&mut self, g: u64) -> Option<Shadow> {
        let shadow = self.live.remove(&g)?;
        for index in shadow.roots.indexes() {
            if self.held_roots.get(&index) == Some(&g) {
                self.held_roots.remove(&index);
            }
        }
        for name in &shadow.names {
            if self.held_names.get(name) == Some(&g) {
                self.held_names.remove(name);
            }
        }
        Some(shadow)
    }

    /// A call from `holder` arrived: its live leases were heard from.
    fn heard(&mut self, now: Millis, holder: &str) {
        for shadow in self.live.values_mut() {
            if shadow.holder.as_str() == holder {
                shadow.heard = now;
            }
        }
    }

    fn committed(
        &mut self,
        now: Millis,
        by: &work::Commit,
        result: Option<(&BatchResult, &WorkResultAnswer)>,
    ) -> Check {
        let g = by.generation;
        if let Some(at) = self.done_at {
            return Err((
                "fenced",
                format_in!(self.alloc, "result {by:?} committed at {now} after Done at {at}"),
            ));
        }
        if self.dead.contains(&g) {
            return Err((
                "fenced",
                format_in!(self.alloc, "result {by:?} of a fenced lease committed"),
            ));
        }
        let Some(shadow) = self.live.get(&g) else {
            return Err((
                "fenced",
                format_in!(self.alloc, "result {by:?} of a lease never granted committed"),
            ));
        };
        if now > shadow.heard + self.lease_ms {
            return Err((
                "fenced",
                format_in!(
                    self.alloc,
                    "result {by:?} committed at {now}, its lease expired at {}",
                    shadow.heard + self.lease_ms
                ),
            ));
        }
        let Some((req, answer)) = result else {
            return Err(("fenced", format_in!(self.alloc, "a commit {by:?} with no result")));
        };
        if req.lease.attempt != self.attempt {
            return Err((
                "fenced",
                format_in!(
                    self.alloc,
                    "result {by:?} of attempt {:?} committed in {:?}",
                    req.lease.attempt,
                    self.attempt
                ),
            ));
        }
        let Some(shadow) = self.release(g) else {
            return Err((
                "fenced",
                format_in!(self.alloc, "result {by:?} lease disappeared during release"),
            ));
        };
        self.dead.insert(g);
        let returned_roots = Ranges::from_wire(&req.returned_roots, self.alloc)
            .unwrap_or_else(|| Ranges::new(self.alloc));
        let mut returned = BTreeSet::new_in(self.alloc);
        returned.extend(req.returned_names.iter().copied());
        let roots = shadow.roots.minus(&returned_roots);
        let names = Vec::from_iter_in(
            shadow.names.iter().filter(|n| !returned.contains(n)).copied(),
            self.alloc,
        );
        let key = (WireString::from_str_in(by.holder.as_str(), self.alloc), g, by.seq);
        for index in roots.indexes() {
            if let Some(other) = self.root_claims.insert(index, key.clone()) {
                return Err((
                    "once",
                    format_in!(self.alloc, "root {index} accounted by {other:?} and {key:?}"),
                ));
            }
        }
        let k = usize::try_from(roots.len()).unwrap_or(req.items.len());
        let named_items = req.items.get(k..).unwrap_or(&[]);
        for (item, name) in named_items.iter().zip(&names) {
            // Lost is a returned finding, not accepted compute. Another finder may
            // legitimately commit this name later under a new lease.
            if matches!(item.outcome, ItemOutcome::Lost(_)) {
                continue;
            }
            if let Some(other) = self.name_claims.insert(*name, key.clone()) {
                return Err((
                    "once",
                    format_in!(
                        self.alloc,
                        "name {} accounted by {other:?} and {key:?}",
                        short(name)
                    ),
                ));
            }
        }
        for (item, name) in named_items.iter().zip(&names) {
            if let ItemOutcome::Lost(why) = &item.outcome
                && why.why.as_str() == UNKNOWN_NAME
                && !answer.conflicts.iter().any(|c| c == name)
            {
                return Err((
                    "unknown",
                    format_in!(
                        self.alloc,
                        "unknown name {} not answered as a conflict",
                        short(name)
                    ),
                ));
            }
        }
        self.accepted.insert(key, (req.clone(), answer.clone(), roots, names));
        Ok(())
    }

    fn observe(
        &mut self,
        now: Millis,
        effects: &[jobs::Effect],
        result: Option<(&BatchResult, &WorkResultAnswer)>,
    ) -> Check {
        for effect in effects {
            let jobs::Effect::Work(effect) = effect else { continue };
            match effect {
                work::Effect::Granted { holder, generation, roots, names } => {
                    self.granted(now, holder.as_str(), *generation, roots, names)?;
                }
                work::Effect::Fenced { generation, .. } => {
                    self.release(*generation);
                    self.dead.insert(*generation);
                }
                work::Effect::Committed { by, .. } => self.committed(now, by, result)?,
                work::Effect::Reset { .. } => {
                    if !self.live.is_empty() {
                        return Err((
                            "exclusive",
                            WireString::from_str_in("a reset with leases live", self.alloc),
                        ));
                    }
                    self.accepted.clear();
                    self.root_claims.clear();
                    self.name_claims.clear();
                    self.done_at = None;
                }
                work::Effect::Done => {
                    if !self.live.is_empty() {
                        return Err((
                            "once",
                            format_in!(self.alloc, "Done with {} leases live", self.live.len()),
                        ));
                    }
                    self.done_at = Some(now);
                }
                _ => {}
            }
        }
        Ok(())
    }

    /// At Done: every root, every name an accepted item found and every edge is terminal,
    /// from the one result that claimed it with the lead's item, or in the tail.
    fn coverage(&self, program: &Program, report: &work::Report) -> Check {
        if report.fixed.as_ref() != Some(&program.lead.rootset) {
            return Err((
                "once",
                WireString::from_str_in("the fixed root set is not the lead's", self.alloc),
            ));
        }
        if report.roots.len() != program.lead.roots.len() {
            return Err((
                "once",
                format_in!(
                    self.alloc,
                    "{} roots for the lead's {}",
                    report.roots.len(),
                    program.lead.roots.len()
                ),
            ));
        }
        let mut found = BTreeMap::new_in(self.alloc);
        let mut edges = BTreeSet::new_in(self.alloc);
        let mut items: BTreeMap<
            (WireString, u64, u64),
            BTreeMap<RootOrName, &ItemResult, WireAlloc>,
            WireAlloc,
        > = BTreeMap::new_in(self.alloc);
        for (key, (req, _, roots, names)) in &self.accepted {
            let k = usize::try_from(roots.len()).unwrap_or(req.items.len());
            let mut by = BTreeMap::new_in(self.alloc);
            if let Some((root_slice, name_slice)) = req.items.split_at_checked(k) {
                for (item, index) in root_slice.iter().zip(roots.indexes()) {
                    by.insert(RootOrName::Root(index), item);
                }
                for (item, name) in name_slice.iter().zip(names) {
                    by.insert(RootOrName::Name(*name), item);
                }
            }
            for item in &req.items {
                if matches!(item.outcome, ItemOutcome::Lost(_)) {
                    continue;
                }
                for f in item.found.iter() {
                    found.entry(f.name).or_insert_with(|| key.clone());
                }
                for (ordinal, _) in item.unnamed.iter() {
                    edges.insert((item.name, *ordinal));
                }
            }
            items.insert(key.clone(), by);
        }
        let committed_item = |by: &work::Commit, what: RootOrName| {
            items
                .get(&(WireString::from_str_in(by.holder.as_str(), self.alloc), by.generation, by.seq))
                .and_then(|m| m.get(&what).copied())
        };
        for (idx, end) in report.roots.iter().enumerate() {
            let index = u64::try_from(idx).unwrap_or(u64::MAX);
            match end {
                None => {
                    return Err((
                        "once",
                        format_in!(self.alloc, "root {index} not terminal at Done"),
                    ));
                }
                Some(Terminal::Tail(_)) => {}
                Some(Terminal::Committed { by, .. }) => {
                    let claim = self.root_claims.get(&index);
                    let expected_key =
                        (WireString::from_str_in(by.holder.as_str(), self.alloc), by.generation, by.seq);
                    if claim != Some(&expected_key) {
                        return Err((
                            "once",
                            format_in!(
                                self.alloc,
                                "root {index} done by {by:?}, claimed by {claim:?}"
                            ),
                        ));
                    }
                    let Some(item) = committed_item(by, RootOrName::Root(index)) else {
                        return Err((
                            "once",
                            format_in!(self.alloc, "root {index} claimed item missing for {by:?}"),
                        ));
                    };
                    let Some(lead_root) = program.lead.roots.get(idx) else {
                        return Err((
                            "once",
                            format_in!(self.alloc, "root {index} out of lead root list"),
                        ));
                    };
                    let truth = lead_root.result(program.lead.salt);
                    if *item != truth {
                        return Err((
                            "once",
                            format_in!(
                                self.alloc,
                                "root {index} done by {by:?} with {} where the lead compiles {}",
                                short(&item.name),
                                short(&truth.name)
                            ),
                        ));
                    }
                }
            }
        }
        for (name, (_, end)) in &report.names {
            let Some(end) = end else {
                return Err((
                    "once",
                    format_in!(self.alloc, "name {} not terminal at Done", short(name)),
                ));
            };
            if let Terminal::Committed { by, .. } = end {
                let claim = self.name_claims.get(name);
                let expected_key =
                    (WireString::from_str_in(by.holder.as_str(), self.alloc), by.generation, by.seq);
                if claim != Some(&expected_key) {
                    return Err((
                        "once",
                        format_in!(
                            self.alloc,
                            "name {} done by {by:?}, claimed by {claim:?}",
                            short(name)
                        ),
                    ));
                }
                let Some(item) = committed_item(by, RootOrName::Name(*name)) else {
                    return Err((
                        "once",
                        format_in!(
                            self.alloc,
                            "name {} claimed item missing for {by:?}",
                            short(name)
                        ),
                    ));
                };
                if let ItemOutcome::Lost(why) = &item.outcome {
                    let invariant = if why.why.as_str() == UNKNOWN_NAME { "unknown" } else { "once" };
                    return Err((
                        invariant,
                        format_in!(
                            self.alloc,
                            "name {} taken as done from a Lost answer ({})",
                            short(name),
                            why.why
                        ),
                    ));
                }
                let Some(instance) = program.instances.get(name) else {
                    return Err((
                        "once",
                        format_in!(
                            self.alloc,
                            "name {} missing from program instances",
                            short(name)
                        ),
                    ));
                };
                let truth = instance.result(program.lead.salt);
                if *item != truth {
                    return Err((
                        "once",
                        format_in!(
                            self.alloc,
                            "name {} done with another item than the lead's",
                            short(name)
                        ),
                    ));
                }
            }
        }
        for (name, key) in &found {
            if !report.names.contains_key(name) {
                return Err((
                    "once",
                    format_in!(
                        self.alloc,
                        "name {} found by {key:?} is missing from the work set",
                        short(name)
                    ),
                ));
            }
        }
        for (key, (req, _, _, _)) in &self.accepted {
            for item in &req.items {
                if let ItemOutcome::Lost(why) = &item.outcome
                    && why.why.as_str() == UNKNOWN_NAME
                {
                    match report.names.get(&item.name) {
                        Some((_, Some(Terminal::Tail(_)))) => {}
                        other => {
                            return Err((
                                "unknown",
                                format_in!(
                                    self.alloc,
                                    "unknown name {} from {key:?} ended {other:?}, not a conflict or Local",
                                    short(&item.name)
                                ),
                            ));
                        }
                    }
                }
            }
        }
        for (parent, ordinal) in &edges {
            let edge = WorkName::of(
                &WorkKey::Edge(blaze_wire::EdgeKey { parent: *parent, ordinal: *ordinal }),
                self.alloc,
            );
            match report.edges.get(&edge) {
                Some((p, o, t)) if p == parent && o == ordinal && t.is_local() => {}
                other => {
                    return Err((
                        "once",
                        format_in!(self.alloc, "edge {ordinal} of {} is {other:?}", short(parent)),
                    ));
                }
            }
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum RootOrName {
    Root(u64),
    Name(WorkName),
}

fn short(name: &WorkName) -> blaze_wire::json::Hex<'_> {
    blaze_wire::json::hex(name.0.get(..4).unwrap_or(&[]))
}

struct World {
    alloc: WireAlloc,
    seed: u64,
    rng: Rng,
    profile: Profile,
    program: Program,
    now: Millis,
    queue: BTreeMap<(Millis, u64), Kind, WireAlloc>,
    next: u64,
    nodes: WireVec<Node>,
    execution: Execution,
    checker: Checker,
    deaths: u32,
}

#[expect(
    clippy::needless_pass_by_value,
    reason = "a refusal is consumed by the wire form it stands for"
)]
fn wire(refusal: Refusal, alloc: WireAlloc) -> blaze_wire::WireError {
    let retry = (refusal == Refusal::NotYet).then_some(3_000);
    refusal.to_wire(retry, alloc)
}

impl World {
    fn new(seed: u64, alloc: WireAlloc) -> Result<World, Violation> {
        let mut rng = Rng::new(seed);
        let profile = Profile::draw(&mut rng);
        let program = Program::generate(&mut rng.fork(), profile.conflicts, alloc);
        let horizon = profile.horizon_ms;
        let mut nodes = Vec::with_capacity_in(profile.nodes, alloc);
        for i in 0..profile.nodes {
            let front = match profile.divergent {
                Some((at, d)) if at == i => program.diverge(&mut rng, d),
                _ => program.lead.clone(),
            };
            let survivor = i == profile.survivor;
            let rate_ppm = if profile.slow_clock == Some(i) {
                rng.range(50_000, 200_000)
            } else {
                rng.range(800_000, 1_250_000)
            };
            let mut partitions = Vec::new_in(alloc);
            if !survivor || rng.chance(300_000) {
                for _ in 0..rng.below(3) {
                    let start = rng.below(horizon);
                    let end = (start + rng.range(1_000, 120_000)).min(horizon);
                    partitions.push((start, end));
                }
            }
            nodes.push(Node {
                id: format_in!(alloc, "exec:{i}"),
                alive: true,
                front,
                holder: None,
                attempt: Attempt(0),
                lead: false,
                offset: rng.below(3_600_000),
                rate_ppm,
                death_ppm: if survivor { 0 } else { profile.death_ppm },
                hung: profile.hung == Some(i),
                partitions,
                refused: false,
                poll_at: POLL_MS,
            });
        }
        let config = ExecutionConfig {
            work: WorkConfig {
                grant_roots: rng.range(1, 12),
                grant_names: rng.range_usize(1, 12),
                ..WorkConfig::default()
            },
            ..ExecutionConfig::default()
        };
        let mut checker = Checker {
            alloc,
            lease_ms: config.work.lease_ms,
            attempt: Attempt(1),
            live: BTreeMap::new_in(alloc),
            dead: BTreeSet::new_in(alloc),
            held_roots: BTreeMap::new_in(alloc),
            held_names: BTreeMap::new_in(alloc),
            accepted: BTreeMap::new_in(alloc),
            root_claims: BTreeMap::new_in(alloc),
            name_claims: BTreeMap::new_in(alloc),
            done_at: None,
        };
        let subscriber = Subscriber {
            id: 1,
            deadline_ms: DEADLINE_MS,
            policy: LocalPolicy::Never,
            state: SubscriberState::Attached,
        };
        let mut log = Vec::new_in(alloc);
        let mut execution = Execution::new(
            0,
            ExecutionId(seed),
            JobContent([7; 32]),
            Namespace::Main,
            subscriber,
            config,
            alloc,
            &mut log,
        );
        execution.admitted(0, &mut log);
        let helpers = Vec::from_iter_in(nodes.iter().skip(1).map(|n| n.id.as_str()), alloc);
        let Some(lead_node) = nodes.first() else {
            return Err(Violation::new(seed, 0, "protocol", format_args!("no lead node"), alloc));
        };
        if let Err(err) = execution.assign(0, lead_node.id.as_str(), &helpers, &mut log) {
            return Err(Violation::new(
                seed,
                0,
                "protocol",
                format_args!("initial assign failed: {err:?}"),
                alloc,
            ));
        }
        if let Err((invariant, detail)) = checker.observe(0, &log, None) {
            return Err(Violation { seed, invariant, at: 0, detail });
        }
        let mut world = World {
            alloc,
            seed,
            rng,
            profile,
            program,
            now: 0,
            queue: BTreeMap::new_in(alloc),
            next: 0,
            nodes,
            execution,
            checker,
            deaths: 0,
        };
        for i in 0..world.nodes.len() {
            // Every executor heartbeats from its assignment; a cold lead joins only once its
            // front end is loaded, so warm helpers join first.
            let at = world.rng.below(2_000);
            let load_ms = if i == 0 { world.profile.lead_load_ms } else { world.rng.below(5_000) };
            world.push(at, Kind::Assign { to: i, attempt: Attempt(1), lead: i == 0, load_ms });
            let tick = at + 1 + world.rng.below(TICK_MS);
            world.push(tick, Kind::NodeTick { node: i });
        }
        world.push(COORD_TICK_MS, Kind::CoordTick);
        Ok(world)
    }

    fn push(&mut self, at: Millis, kind: Kind) {
        self.next += 1;
        self.queue.insert((at, self.next), kind);
    }

    fn faulty(&self) -> bool {
        self.now < self.profile.horizon_ms
    }

    /// A message over the network: lost, duplicated, delayed or held by a partition until the
    /// horizon; delayed only after it.
    fn send(&mut self, node: usize, kind: Kind) {
        let delay = self.rng.range(1, self.profile.max_delay_ms);
        if self.faulty() {
            let partitioned =
                self.nodes.get(node).is_some_and(|n| n.partitioned(self.now).is_some());
            if partitioned || self.rng.chance(self.profile.loss_ppm) {
                return;
            }
            if self.rng.chance(self.profile.dup_ppm) {
                let again = self.rng.range(1, 2 * self.profile.max_delay_ms);
                self.push(self.now + again, kind.clone());
            }
        }
        self.push(self.now + delay, kind);
    }

    /// What the agents carry (assignments and kills): never lost, held by a partition.
    fn carry(&mut self, node: usize, kind: Kind) {
        let delay = self.rng.range(1, self.profile.max_delay_ms);
        let at = match self.nodes.get(node).and_then(|n| n.partitioned(self.now)) {
            Some(end) if self.faulty() => end + delay,
            _ => self.now + delay,
        };
        self.push(at, kind);
    }

    fn violation(&self, invariant: &'static str, detail: fmt::Arguments<'_>) -> Violation {
        Violation::new(self.seed, self.now, invariant, detail, self.alloc)
    }

    fn run(
        mut self,
        mut trace: Option<&mut dyn FnMut(fmt::Arguments<'_>)>,
    ) -> Result<Summary, Violation> {
        let mut events = 0u64;
        while let Some(((at, _), kind)) = self.queue.pop_first() {
            self.now = at;
            events += 1;
            if events > EVENT_CAP {
                return Err(
                    self.violation("progress", format_args!("{EVENT_CAP} events without an end"))
                );
            }
            if let Some(trace) = trace.as_mut() {
                trace(format_args!("{at:>9} {kind:?}"));
            }
            self.event(kind)?;
            if let ExecState::Ended(_) = self.execution.state() {
                break;
            }
            if let Some(done) = self.checker.done_at
                && self.now > done + DRAIN_MS
            {
                break;
            }
        }
        let Some(work) = self.execution.work() else {
            return Err(self.violation("progress", format_args!("execution had no work set")));
        };
        let report = work.report();
        if report.done {
            self.checker.coverage(&self.program, &report).map_err(|(invariant, detail)| {
                Violation { seed: self.seed, invariant, at: self.now, detail }
            })?;
        } else {
            match work.ended() {
                Some(EndWhy::LeadLostTwice | EndWhy::NoLead) => {}
                other => {
                    return Err(self.violation(
                        "progress",
                        format_args!(
                            "the compile phase did not end Done: {other:?}, {:?}",
                            work.counters()
                        ),
                    ));
                }
            }
        }
        Ok(Summary {
            seed: self.seed,
            done: report.done,
            ended: work.ended(),
            events,
            sim_ms: self.now,
            counters: work.counters().clone(),
            lead_losses: self.execution.losses(),
            deaths: self.deaths,
        })
    }

    fn event(&mut self, kind: Kind) -> Result<(), Violation> {
        match kind {
            Kind::CoordTick => {
                let mut fx = Vec::new_in(self.alloc);
                self.execution.tick(self.now, &mut fx);
                self.effects(fx, None)?;
                self.push(self.now + COORD_TICK_MS, Kind::CoordTick);
            }
            Kind::ToCoord { from, id, call } => self.call(from, id, call)?,
            Kind::ToNode { to, attempt, id, reply } => {
                if self.nodes.get(to).is_some_and(|n| n.attempt == attempt) {
                    self.step(to, holder::Event::Reply { id, reply })?;
                }
            }
            Kind::Compiled { node, attempt, batch } => {
                if self.nodes.get(node).is_some_and(|n| n.attempt == attempt) {
                    self.step(node, holder::Event::Compiled(batch))?;
                }
            }
            Kind::NodeTick { node } => {
                if !self.nodes.get(node).is_some_and(|n| n.alive) {
                    return Ok(());
                }
                if self.faulty() && self.rng.chance(self.profile.drop_ppm) {
                    let knows = self.rng.chance(500_000);
                    if let Some(h) = self.nodes.get_mut(node).and_then(|n| n.holder.as_mut()) {
                        let name = h.findings().next().map(|(name, _)| *name);
                        if let Some(name) = name {
                            h.drop_finding(&name, knows);
                        }
                    }
                }
                self.step(node, holder::Event::Tick)?;
                self.poll(node);
                self.push(self.now + TICK_MS, Kind::NodeTick { node });
            }
            Kind::Loaded { to, attempt } => {
                if self.nodes.get(to).is_some_and(|n| n.attempt == attempt) {
                    self.step(to, holder::Event::Loaded)?;
                }
            }
            Kind::Assign { to, attempt, lead, load_ms } => {
                let Some(node) = self.nodes.get_mut(to) else { return Ok(()) };
                if !node.alive
                    || attempt < node.attempt
                    || (attempt == node.attempt && node.holder.is_some())
                    || attempt != self.execution.attempt()
                {
                    return Ok(());
                }
                node.attempt = attempt;
                node.lead = lead;
                let rejected = if lead {
                    blaze_wire::Set::new(Vec::from_iter_in(
                        self.program.rejected.iter().copied(),
                        self.alloc,
                    ))
                } else {
                    blaze_wire::Set::empty_in(self.alloc)
                };
                let mut h = Holder::new(
                    node.id.as_str(),
                    self.execution.id,
                    attempt,
                    node.front.rootset.clone(),
                    rejected,
                    self.program.index.clone(),
                    HolderConfig::default(),
                    self.alloc,
                );
                let local = node.local(self.now);
                let mut out = Vec::new_in(self.alloc);
                h.start(local, &mut out);
                node.holder = Some(h);
                self.apply(to, out)?;
                self.push(self.now + load_ms, Kind::Loaded { to, attempt });
            }
            Kind::Kill { to, attempt } => {
                if let Some(node) = self.nodes.get_mut(to)
                    && node.attempt <= attempt
                {
                    node.holder = None;
                }
            }
        }
        Ok(())
    }

    fn step(&mut self, node: usize, event: holder::Event) -> Result<(), Violation> {
        let Some(n) = self.nodes.get_mut(node) else { return Ok(()) };
        if !n.alive {
            return Ok(());
        }
        let local = n.local(self.now);
        let Some(h) = &mut n.holder else { return Ok(()) };
        let mut out = Vec::new_in(self.alloc);
        h.step(local, event, &mut out);
        self.apply(node, out)
    }

    fn apply(
        &mut self,
        node: usize,
        effects: Vec<holder::Effect, impl Allocator>,
    ) -> Result<(), Violation> {
        for effect in effects {
            let Some((alive, death_ppm)) = self.nodes.get(node).map(|n| (n.alive, n.death_ppm))
            else {
                break;
            };
            if !alive {
                break;
            }
            // Death at any step: before the effect or just after it.
            if self.faulty() && self.rng.chance(death_ppm) {
                if self.rng.chance(500_000) {
                    self.effect(node, effect)?;
                }
                if let Some(n) = self.nodes.get_mut(node) {
                    n.alive = false;
                    n.holder = None;
                }
                self.deaths += 1;
                break;
            }
            self.effect(node, effect)?;
        }
        Ok(())
    }

    fn effect(&mut self, node: usize, effect: holder::Effect) -> Result<(), Violation> {
        match effect {
            holder::Effect::Send { id, call } => {
                self.send(node, Kind::ToCoord { from: node, id, call });
            }
            holder::Effect::Compile(plan) => {
                let (batch, took) = self.compile(node, &plan)?;
                let Some((attempt, hung)) = self.nodes.get(node).map(|n| (n.attempt, n.hung))
                else {
                    return Ok(());
                };
                // A hung batch exceeds the progress timeout, but the fault is finite.
                // Its eventual completion exercises fencing; later batches run normally.
                let at = if hung && self.faulty() {
                    self.profile.horizon_ms + WorkConfig::default().progress_first_ms + took
                } else {
                    self.now + took
                };
                self.push(at, Kind::Compiled { node, attempt, batch });
            }
            holder::Effect::Release(_) => {}
            holder::Effect::Stop(why) => {
                if let StopWhy::Protocol(what) = &why {
                    let id = self.nodes.get(node).map_or("<missing>", |n| n.id.as_str());
                    return Err(self.violation("protocol", format_args!("{id} stopped: {what}")));
                }
                if let Some(n) = self.nodes.get_mut(node) {
                    if why == StopWhy::FrontEndDiffers {
                        n.refused = true;
                    }
                    n.holder = None;
                }
            }
        }
        Ok(())
    }

    /// The executor's compile threads over a plan: the roots of its own front end, the names
    /// from the program, stopped early at a budget now and then.
    fn compile(&mut self, node: usize, plan: &Plan) -> Result<(Compiled, Millis), Violation> {
        let Some(front) = self.nodes.get(node).map(|n| &n.front) else {
            return Err(self.violation("protocol", format_args!("missing node {node}")));
        };
        let Some(granted) = Ranges::from_wire(&plan.grant.roots, self.alloc) else {
            return Err(
                self.violation("protocol", format_args!("invalid root ranges in compile plan"))
            );
        };
        let total = usize::try_from(granted.len()).unwrap_or(0).saturating_add(plan.compile.len());
        let stop = if self.rng.chance(self.profile.budget_ppm) {
            self.rng.below_usize(total.saturating_add(1))
        } else {
            total
        };
        let mut done: usize = 0;
        let mut roots = Vec::new_in(self.alloc);
        let mut returned_roots = Ranges::new(self.alloc);
        for index in granted.indexes() {
            if done < stop {
                if let Ok(idx) = usize::try_from(index)
                    && let Some(item) = front.roots.get(idx)
                {
                    roots.push(item.result(front.salt));
                }
                done += 1;
            } else {
                returned_roots.insert(index, index + 1);
            }
        }
        let mut names = Vec::new_in(self.alloc);
        let mut returned_names = Vec::new_in(self.alloc);
        for name in &plan.compile {
            if done < stop {
                if let Some(item) = self.program.instances.get(name) {
                    names.push(item.result(front.salt));
                }
                done += 1;
            } else {
                returned_names.push(*name);
            }
        }
        let done_u64 = u64::try_from(done).unwrap_or(u64::MAX);
        let packs = if done > 0 {
            vec_in![
                self.alloc;
                PackRef {
                    pack: self.rng.digest(),
                    size: done_u64,
                    keys: [0; 32],
                    packed: None,
                    format: 1,
                }
            ]
        } else {
            Vec::new_in(self.alloc)
        };
        let took = 20 + 30 * done_u64 + self.rng.below(2_000);
        let batch = Compiled {
            generation: plan.grant.lease.generation,
            roots,
            names,
            packs,
            returned_roots,
            returned_names,
        };
        Ok((batch, took))
    }

    #[expect(
        clippy::needless_pass_by_value,
        reason = "a call is what the simulated network delivers, once"
    )]
    fn call(&mut self, from: usize, id: u64, call: Call) -> Result<(), Violation> {
        let Some(holder_id) =
            self.nodes.get(from).map(|n| WireString::from_str_in(n.id.as_str(), self.alloc))
        else {
            return Ok(());
        };
        let now = self.now;
        let alloc = self.alloc;
        let mut fx = Vec::new_in(alloc);
        let attempt = match &call {
            Call::Join(r) => r.attempt,
            Call::Lease(r) => r.attempt,
            Call::Result(r) => r.lease.attempt,
            Call::Events(r) => r.attempt,
        };
        let reply = match &call {
            Call::Join(req) => Reply::Join(
                self.execution
                    .join(now, holder_id.as_str(), req, &mut fx)
                    .map_err(|r| wire(r, alloc)),
            ),
            Call::Lease(req) => Reply::Lease(
                self.execution
                    .lease(now, holder_id.as_str(), req, &mut fx)
                    .map_err(|r| wire(r, alloc)),
            ),
            Call::Result(req) => {
                let answer = self.execution.result(now, holder_id.as_str(), req, &mut fx);
                if let Err(
                    refusal @ (Refusal::Malformed(_) | Refusal::SeqReused | Refusal::NotHolder),
                ) = &answer
                {
                    return Err(self.violation(
                        "protocol",
                        format_args!("{holder_id}'s result refused: {refusal:?}"),
                    ));
                }
                let ok = answer.as_ref().ok().cloned();
                self.effects(fx, ok.as_ref().map(|a| (req, a)))?;
                self.checker.heard(now, holder_id.as_str());
                let reply = Reply::Result(answer.map_err(|r| wire(r, alloc)));
                self.send(from, Kind::ToNode { to: from, attempt, id, reply });
                return Ok(());
            }
            Call::Events(req) => Reply::Events(
                self.execution
                    .worker_events(now, holder_id.as_str(), req, &mut fx)
                    .map_err(|r| wire(r, alloc)),
            ),
        };
        if let Reply::Join(Err(e)) | Reply::Lease(Err(e)) = &reply
            && let Some(Refusal::Malformed(what)) = Refusal::of(e)
        {
            return Err(self.violation(
                "protocol",
                format_args!("{holder_id}'s call refused as malformed: {what}"),
            ));
        }
        self.effects(fx, None)?;
        self.checker.heard(now, holder_id.as_str());
        self.send(from, Kind::ToNode { to: from, attempt, id, reply });
        Ok(())
    }

    fn effects(
        &mut self,
        fx: Vec<jobs::Effect, impl Allocator>,
        result: Option<(&BatchResult, &WorkResultAnswer)>,
    ) -> Result<(), Violation> {
        self.checker.observe(self.now, &fx, result).map_err(|(invariant, detail)| Violation {
            seed: self.seed,
            invariant,
            at: self.now,
            detail,
        })?;
        for effect in fx {
            match effect {
                jobs::Effect::NeedLead => self.reassign()?,
                jobs::Effect::Kill { executor, assignment } => {
                    if let Some(at) = self.nodes.iter().position(|n| n.id == executor) {
                        let attempt = assignment.attempt;
                        self.carry(at, Kind::Kill { to: at, attempt });
                    }
                }
                jobs::Effect::Work(work::Effect::Fenced {
                    why: FenceWhy::AttemptMoved, ..
                }) => {}
                _ => {}
            }
        }
        Ok(())
    }

    /// An idle agent's long poll: an executor with no assignment, alive and not refused, is
    /// offered to the running execution as a helper of its current attempt.
    fn poll(&mut self, node: usize) {
        let Some(n) = self.nodes.get(node) else { return };
        if n.holder.is_some() || n.refused || !n.alive || self.now < n.poll_at {
            return;
        }
        let id = WireString::from_str_in(n.id.as_str(), self.alloc);
        if let Some(n) = self.nodes.get_mut(node) {
            n.poll_at = self.now + POLL_MS;
        }
        let running = self.execution.state() == ExecState::Running
            && self.execution.lead().is_some()
            && !self.execution.work().is_some_and(|w| w.is_done());
        if !running {
            return;
        }
        if self.execution.lead() == Some(id.as_str()) {
            // Its own assignment is on its way.
            return;
        }
        if let Ok(attempt) = self.execution.add_helper(self.now, id.as_str()) {
            let load_ms = self.rng.below(2_000);
            self.carry(node, Kind::Assign { to: node, attempt, lead: false, load_ms });
        }
    }

    /// The lead was lost: a new lead from the executors the execution heard from recently,
    /// as admission would choose, never the one whose front end differs (the truth the
    /// checker holds results to is the first lead's front end).
    fn reassign(&mut self) -> Result<(), Violation> {
        let heard = Vec::from_iter_in(self.execution.heard_recently(self.now), self.alloc);
        let candidates = Vec::from_iter_in(
            (0..self.nodes.len())
                .filter(|&i| self.nodes.get(i).is_some_and(|n| heard.scan_for(&n.id.as_str()).is_some()))
                .filter(|&i| self.profile.divergent.is_none_or(|(at, _)| at != i)),
            self.alloc,
        );
        let mut fx = Vec::new_in(self.alloc);
        if candidates.is_empty() {
            self.execution.end(self.now, blaze_wire::Outcome::Failed, EndWhy::NoLead, &mut fx);
        } else {
            let Some(&lead) = candidates.get(self.rng.below_usize(candidates.len())) else {
                return Err(self.violation("protocol", format_args!("no candidate selected")));
            };
            let Some(lead_id) = self.nodes.get(lead).map(|n| n.id.as_str()) else {
                return Err(self.violation("protocol", format_args!("missing lead node {lead}")));
            };
            let helpers = Vec::from_iter_in(
                candidates
                    .iter()
                    .filter(|&&i| i != lead)
                    .filter_map(|&i| self.nodes.get(i).map(|n| n.id.as_str())),
                self.alloc,
            );
            if let Err(err) = self.execution.assign(self.now, lead_id, &helpers, &mut fx) {
                return Err(self.violation("protocol", format_args!("reassign failed: {err:?}")));
            }
            self.checker.attempt = self.execution.attempt();
            let attempt = self.execution.attempt();
            for &i in &candidates {
                self.carry(i, Kind::Assign { to: i, attempt, lead: i == lead, load_ms: 0 });
            }
        }
        self.effects(fx, None)
    }
}

/// Runs the simulation of `seed`.
pub fn run(seed: u64, alloc: WireAlloc) -> Result<Summary, Violation> {
    World::new(seed, alloc)?.run(None)
}

/// Runs the simulation of `seed`, presenting each event to the caller's synchronous writer.
/// Formatting borrows the live event; the caller chooses its output buffer and allocator.
pub fn trace(
    seed: u64,
    alloc: WireAlloc,
    events: &mut impl FnMut(fmt::Arguments<'_>),
) -> Result<Summary, Violation> {
    World::new(seed, alloc)?.run(Some(events))
}
