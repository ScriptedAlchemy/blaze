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

use std::collections::{BTreeMap, BTreeSet};

use blaze_wire::{
    Attempt, BatchResult, ExecutionId, ItemOutcome, ItemResult, JobContent, LocalPolicy, Namespace,
    PackRef, WorkKey, WorkName, WorkResultAnswer,
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
        let nodes = rng.range(2, 4) as usize;
        let survivor = rng.below(nodes as u64) as usize;
        // A helper, never the survivor or the lead, gets each of the odd faults.
        let odd = |rng: &mut Rng, ppm: u32| {
            let at = rng.range(1, nodes as u64 - 1) as usize;
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
            loss_ppm: if calm { 0 } else { rng.range(0, 250_000) as u32 },
            dup_ppm: if calm { 0 } else { rng.range(0, 150_000) as u32 },
            max_delay_ms: rng.range(1, 3_000),
            horizon_ms: rng.range(10_000, 240_000),
            survivor,
            divergent,
            hung,
            slow_clock,
            death_ppm: if calm { 0 } else { rng.range(0, 20_000) as u32 },
            drop_ppm: if calm { 0 } else { rng.range(0, 30_000) as u32 },
            budget_ppm: rng.range(0, 300_000) as u32,
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
    id: String,
    alive: bool,
    front: FrontEnd,
    holder: Option<Holder>,
    attempt: Attempt,
    lead: bool,
    offset: Millis,
    rate_ppm: u64,
    death_ppm: u32,
    hung: bool,
    partitions: Vec<(Millis, Millis)>,
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
    holder: String,
    roots: Ranges,
    names: Vec<WorkName>,
    heard: Millis,
}

/// The checker: what the coordinator must never do, from what it can see.
struct Checker {
    lease_ms: u64,
    live: BTreeMap<u64, Shadow>,
    dead: BTreeSet<u64>,
    held_roots: BTreeMap<u64, u64>,
    held_names: BTreeMap<WorkName, u64>,
    /// Accepted results of the current root epoch, and their answers.
    accepted: BTreeMap<(String, u64, u64), (BatchResult, WorkResultAnswer, Ranges, Vec<WorkName>)>,
    root_claims: BTreeMap<u64, (String, u64, u64)>,
    name_claims: BTreeMap<WorkName, (String, u64, u64)>,
    done_at: Option<Millis>,
    attempt: Attempt,
}

/// A checker's verdict: fine, or the invariant broken and what was seen.
type Check = Result<(), (&'static str, String)>;

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
            return Err(("exclusive", format!("a grant {g} to {holder} after Done")));
        }
        for index in roots.indexes() {
            if let Some(other) = self.held_roots.insert(index, g) {
                return Err(("exclusive", format!("root {index} in live leases {other} and {g}")));
            }
        }
        for name in names {
            if let Some(other) = self.held_names.insert(*name, g) {
                return Err((
                    "exclusive",
                    format!("name {:?} in live leases {other} and {g}", short(name)),
                ));
            }
        }
        self.live.insert(
            g,
            Shadow {
                holder: holder.to_owned(),
                roots: roots.clone(),
                names: names.to_vec(),
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
            if shadow.holder == holder {
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
            return Err(("fenced", format!("result {by:?} committed at {now} after Done at {at}")));
        }
        if self.dead.contains(&g) {
            return Err(("fenced", format!("result {by:?} of a fenced lease committed")));
        }
        let Some(shadow) = self.live.get(&g) else {
            return Err(("fenced", format!("result {by:?} of a lease never granted committed")));
        };
        if now > shadow.heard + self.lease_ms {
            return Err((
                "fenced",
                format!(
                    "result {by:?} committed at {now}, its lease expired at {}",
                    shadow.heard + self.lease_ms
                ),
            ));
        }
        let Some((req, answer)) = result else {
            return Err(("fenced", format!("a commit {by:?} with no result")));
        };
        if req.lease.attempt != self.attempt {
            return Err((
                "fenced",
                format!(
                    "result {by:?} of attempt {:?} committed in {:?}",
                    req.lease.attempt, self.attempt
                ),
            ));
        }
        let shadow = self.release(g).expect("live");
        self.dead.insert(g);
        let returned_roots = Ranges::from_wire(&req.returned_roots).unwrap_or_default();
        let returned: BTreeSet<WorkName> = req.returned_names.iter().copied().collect();
        let roots = shadow.roots.minus(&returned_roots);
        let names: Vec<WorkName> =
            shadow.names.iter().filter(|n| !returned.contains(n)).copied().collect();
        let key = (by.holder.clone(), g, by.seq);
        for index in roots.indexes() {
            if let Some(other) = self.root_claims.insert(index, key.clone()) {
                return Err(("once", format!("root {index} accounted by {other:?} and {key:?}")));
            }
        }
        let k = roots.len() as usize;
        for (item, name) in req.items[k..].iter().zip(&names) {
            // Lost is a returned finding, not accepted compute. Another finder may
            // legitimately commit this name later under a new lease.
            if matches!(item.outcome, ItemOutcome::Lost(_)) {
                continue;
            }
            if let Some(other) = self.name_claims.insert(*name, key.clone()) {
                return Err((
                    "once",
                    format!("name {} accounted by {other:?} and {key:?}", short(name)),
                ));
            }
        }
        for (item, name) in req.items[k..].iter().zip(&names) {
            if let ItemOutcome::Lost(why) = &item.outcome
                && why.why == UNKNOWN_NAME
                && !answer.conflicts.iter().any(|c| c == name)
            {
                return Err((
                    "unknown",
                    format!("unknown name {} not answered as a conflict", short(name)),
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
                    self.granted(now, holder, *generation, roots, names)?;
                }
                work::Effect::Fenced { generation, .. } => {
                    self.release(*generation);
                    self.dead.insert(*generation);
                }
                work::Effect::Committed { by, .. } => self.committed(now, by, result)?,
                work::Effect::Reset { .. } => {
                    if !self.live.is_empty() {
                        return Err(("exclusive", "a reset with leases live".to_owned()));
                    }
                    self.accepted.clear();
                    self.root_claims.clear();
                    self.name_claims.clear();
                    self.done_at = None;
                }
                work::Effect::Done => {
                    if !self.live.is_empty() {
                        return Err(("once", format!("Done with {} leases live", self.live.len())));
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
            return Err(("once", "the fixed root set is not the lead's".to_owned()));
        }
        if report.roots.len() != program.lead.roots.len() {
            return Err((
                "once",
                format!("{} roots for the lead's {}", report.roots.len(), program.lead.roots.len()),
            ));
        }
        let mut found = BTreeMap::new();
        let mut edges = BTreeSet::new();
        let mut items: BTreeMap<(String, u64, u64), BTreeMap<RootOrName, &ItemResult>> =
            BTreeMap::new();
        for (key, (req, _, roots, names)) in &self.accepted {
            let k = roots.len() as usize;
            let mut by = BTreeMap::new();
            for (item, index) in req.items[..k].iter().zip(roots.indexes()) {
                by.insert(RootOrName::Root(index), item);
            }
            for (item, name) in req.items[k..].iter().zip(names) {
                by.insert(RootOrName::Name(*name), item);
            }
            for item in &req.items {
                if matches!(item.outcome, ItemOutcome::Lost(_)) {
                    continue;
                }
                for f in item.found.iter() {
                    found.entry(f.name).or_insert(key.clone());
                }
                for (ordinal, _) in item.unnamed.iter() {
                    edges.insert((item.name, *ordinal));
                }
            }
            items.insert(key.clone(), by);
        }
        let committed_item = |by: &work::Commit, what: RootOrName| {
            items
                .get(&(by.holder.clone(), by.generation, by.seq))
                .and_then(|m| m.get(&what).copied())
        };
        for (index, end) in report.roots.iter().enumerate() {
            let index = index as u64;
            match end {
                None => return Err(("once", format!("root {index} not terminal at Done"))),
                Some(Terminal::Tail(_)) => {}
                Some(Terminal::Committed { by, .. }) => {
                    let claim = self.root_claims.get(&index);
                    if claim != Some(&(by.holder.clone(), by.generation, by.seq)) {
                        return Err((
                            "once",
                            format!("root {index} done by {by:?}, claimed by {claim:?}"),
                        ));
                    }
                    let item = committed_item(by, RootOrName::Root(index)).expect("claimed");
                    let truth = program.lead.roots[index as usize].result(program.lead.salt);
                    if *item != truth {
                        return Err((
                            "once",
                            format!(
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
                return Err(("once", format!("name {} not terminal at Done", short(name))));
            };
            if let Terminal::Committed { by, .. } = end {
                let claim = self.name_claims.get(name);
                if claim != Some(&(by.holder.clone(), by.generation, by.seq)) {
                    return Err((
                        "once",
                        format!("name {} done by {by:?}, claimed by {claim:?}", short(name)),
                    ));
                }
                let item = committed_item(by, RootOrName::Name(*name)).expect("claimed");
                if let ItemOutcome::Lost(why) = &item.outcome {
                    let invariant = if why.why == UNKNOWN_NAME { "unknown" } else { "once" };
                    return Err((
                        invariant,
                        format!(
                            "name {} taken as done from a Lost answer ({})",
                            short(name),
                            why.why
                        ),
                    ));
                }
                let truth = program.instances[name].result(program.lead.salt);
                if *item != truth {
                    return Err((
                        "once",
                        format!("name {} done with another item than the lead's", short(name)),
                    ));
                }
            }
        }
        for (name, key) in &found {
            if !report.names.contains_key(name) {
                return Err((
                    "once",
                    format!("name {} found by {key:?} is missing from the work set", short(name)),
                ));
            }
        }
        for (key, (req, _, _, _)) in &self.accepted {
            for item in &req.items {
                if let ItemOutcome::Lost(why) = &item.outcome
                    && why.why == UNKNOWN_NAME
                {
                    match report.names.get(&item.name) {
                        Some((_, Some(Terminal::Tail(_)))) => {}
                        other => {
                            return Err((
                                "unknown",
                                format!(
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
            let edge = WorkName::of(&WorkKey::Edge(blaze_wire::EdgeKey {
                parent: *parent,
                ordinal: *ordinal,
            }));
            match report.edges.get(&edge) {
                Some((p, o, t)) if p == parent && o == ordinal && t.is_local() => {}
                other => {
                    return Err((
                        "once",
                        format!("edge {ordinal} of {} is {other:?}", short(parent)),
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

fn short(name: &WorkName) -> String {
    name.0[..4].iter().map(|b| format!("{b:02x}")).collect()
}

struct World {
    seed: u64,
    rng: Rng,
    profile: Profile,
    program: Program,
    now: Millis,
    queue: BTreeMap<(Millis, u64), Kind>,
    next: u64,
    nodes: Vec<Node>,
    execution: Execution,
    checker: Checker,
    deaths: u32,
}

fn wire(refusal: Refusal) -> blaze_wire::WireError {
    let retry = (refusal == Refusal::NotYet).then_some(3_000);
    refusal.to_wire(retry)
}

impl World {
    fn new(seed: u64) -> World {
        let mut rng = Rng::new(seed);
        let profile = Profile::draw(&mut rng);
        let program = Program::generate(&mut rng.fork(), profile.conflicts);
        let horizon = profile.horizon_ms;
        let mut nodes = Vec::new();
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
            let mut partitions = Vec::new();
            if !survivor || rng.chance(300_000) {
                for _ in 0..rng.below(3) {
                    let start = rng.below(horizon);
                    let end = (start + rng.range(1_000, 120_000)).min(horizon);
                    partitions.push((start, end));
                }
            }
            nodes.push(Node {
                id: format!("exec:{i}"),
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
                grant_names: rng.range(1, 12) as usize,
                ..WorkConfig::default()
            },
            ..ExecutionConfig::default()
        };
        let mut checker = Checker {
            lease_ms: config.work.lease_ms,
            attempt: Attempt(1),
            live: BTreeMap::new(),
            dead: BTreeSet::new(),
            held_roots: BTreeMap::new(),
            held_names: BTreeMap::new(),
            accepted: BTreeMap::new(),
            root_claims: BTreeMap::new(),
            name_claims: BTreeMap::new(),
            done_at: None,
        };
        let subscriber = Subscriber {
            id: 1,
            deadline_ms: DEADLINE_MS,
            policy: LocalPolicy::Never,
            test_timeout_ms: None,
            state: SubscriberState::Attached,
        };
        let mut log = Vec::new();
        let mut execution = Execution::new(
            0,
            ExecutionId(seed),
            JobContent([7; 32]),
            Namespace::Main,
            subscriber,
            config,
            &mut log,
        );
        execution.admitted(0, &mut log);
        let helpers: Vec<String> = nodes.iter().skip(1).map(|n| n.id.clone()).collect();
        execution.assign(0, &nodes[0].id, &helpers, &mut log).expect("queued");
        checker.observe(0, &log, None).expect("no work yet");
        let mut world = World {
            seed,
            rng,
            profile,
            program,
            now: 0,
            queue: BTreeMap::new(),
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
        world
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
            if self.nodes[node].partitioned(self.now).is_some()
                || self.rng.chance(self.profile.loss_ppm)
            {
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
        let at = match self.nodes[node].partitioned(self.now) {
            Some(end) if self.faulty() => end + delay,
            _ => self.now + delay,
        };
        self.push(at, kind);
    }

    fn violation(&self, (invariant, detail): (&'static str, String)) -> Violation {
        Violation { seed: self.seed, invariant, at: self.now, detail }
    }

    fn run(mut self, mut trace: Option<&mut Vec<String>>) -> Result<Summary, Violation> {
        let mut events = 0u64;
        while let Some(((at, _), kind)) = self.queue.pop_first() {
            self.now = at;
            events += 1;
            if events > EVENT_CAP {
                return Err(
                    self.violation(("progress", format!("{EVENT_CAP} events without an end")))
                );
            }
            if let Some(trace) = trace.as_mut() {
                trace.push(format!("{at:>9} {kind:?}"));
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
        let work = self.execution.work().expect("assigned");
        let report = work.report();
        if report.done {
            self.checker.coverage(&self.program, &report).map_err(|v| self.violation(v))?;
        } else {
            match work.ended() {
                Some(EndWhy::LeadLostTwice | EndWhy::NoLead) => {}
                other => {
                    return Err(self.violation((
                        "progress",
                        format!(
                            "the compile phase did not end Done: {other:?}, {:?}",
                            work.counters()
                        ),
                    )));
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
                let mut fx = Vec::new();
                self.execution.tick(self.now, &mut fx);
                self.effects(fx, None)?;
                self.push(self.now + COORD_TICK_MS, Kind::CoordTick);
            }
            Kind::ToCoord { from, id, call } => self.call(from, id, call)?,
            Kind::ToNode { to, attempt, id, reply } => {
                if self.nodes[to].attempt == attempt {
                    self.step(to, holder::Event::Reply { id, reply })?;
                }
            }
            Kind::Compiled { node, attempt, batch } => {
                if self.nodes[node].attempt == attempt {
                    self.step(node, holder::Event::Compiled(batch))?;
                }
            }
            Kind::NodeTick { node } => {
                if !self.nodes[node].alive {
                    return Ok(());
                }
                if self.faulty() && self.rng.chance(self.profile.drop_ppm) {
                    let knows = self.rng.chance(500_000);
                    if let Some(h) = &mut self.nodes[node].holder
                        && let Some((&name, _)) = h.findings().iter().next()
                    {
                        h.drop_finding(&name, knows);
                    }
                }
                self.step(node, holder::Event::Tick)?;
                self.poll(node);
                self.push(self.now + TICK_MS, Kind::NodeTick { node });
            }
            Kind::Loaded { to, attempt } => {
                if self.nodes[to].attempt == attempt {
                    self.step(to, holder::Event::Loaded)?;
                }
            }
            Kind::Assign { to, attempt, lead, load_ms } => {
                let node = &mut self.nodes[to];
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
                    blaze_wire::Set::new(self.program.rejected.iter().copied().collect())
                } else {
                    blaze_wire::Set::new(Vec::new())
                };
                let mut h = Holder::new(
                    &node.id,
                    self.execution.id,
                    attempt,
                    node.front.rootset.clone(),
                    rejected,
                    self.program.index.clone(),
                    HolderConfig::default(),
                );
                let local = node.local(self.now);
                let mut out = Vec::new();
                h.start(local, &mut out);
                node.holder = Some(h);
                self.apply(to, out)?;
                self.push(self.now + load_ms, Kind::Loaded { to, attempt });
            }
            Kind::Kill { to, attempt } => {
                let node = &mut self.nodes[to];
                if node.attempt <= attempt {
                    node.holder = None;
                }
            }
        }
        Ok(())
    }

    fn step(&mut self, node: usize, event: holder::Event) -> Result<(), Violation> {
        let n = &mut self.nodes[node];
        if !n.alive {
            return Ok(());
        }
        let local = n.local(self.now);
        let Some(h) = &mut n.holder else { return Ok(()) };
        let mut out = Vec::new();
        h.step(local, event, &mut out);
        self.apply(node, out)
    }

    fn apply(&mut self, node: usize, effects: Vec<holder::Effect>) -> Result<(), Violation> {
        for effect in effects {
            if !self.nodes[node].alive {
                break;
            }
            // Death at any step: before the effect or just after it.
            if self.faulty() && self.rng.chance(self.nodes[node].death_ppm) {
                if self.rng.chance(500_000) {
                    self.effect(node, effect)?;
                }
                self.nodes[node].alive = false;
                self.nodes[node].holder = None;
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
                let (batch, took) = self.compile(node, &plan);
                let attempt = self.nodes[node].attempt;
                // A hung batch exceeds the progress timeout, but the fault is finite.
                // Its eventual completion exercises fencing; later batches run normally.
                let at = if self.nodes[node].hung && self.faulty() {
                    self.profile.horizon_ms + WorkConfig::default().progress_first_ms + took
                } else {
                    self.now + took
                };
                self.push(at, Kind::Compiled { node, attempt, batch });
            }
            holder::Effect::Release(_) => {}
            holder::Effect::Stop(why) => {
                if let StopWhy::Protocol(what) = &why {
                    return Err(self.violation((
                        "protocol",
                        format!("{} stopped: {what}", self.nodes[node].id),
                    )));
                }
                if why == StopWhy::FrontEndDiffers {
                    self.nodes[node].refused = true;
                }
                self.nodes[node].holder = None;
            }
        }
        Ok(())
    }

    /// The executor's compile threads over a plan: the roots of its own front end, the names
    /// from the program, stopped early at a budget now and then.
    fn compile(&mut self, node: usize, plan: &Plan) -> (Compiled, Millis) {
        let front = &self.nodes[node].front;
        let granted = Ranges::from_wire(&plan.grant.roots).expect("the coordinator's ranges");
        let total = granted.len() as usize + plan.compile.len();
        let stop = if self.rng.chance(self.profile.budget_ppm) {
            self.rng.below(total as u64 + 1) as usize
        } else {
            total
        };
        let mut done = 0;
        let mut roots = Vec::new();
        let mut returned_roots = Ranges::new();
        for index in granted.indexes() {
            if done < stop {
                roots.push(front.roots[index as usize].result(front.salt));
                done += 1;
            } else {
                returned_roots.insert(index, index + 1);
            }
        }
        let mut names = Vec::new();
        let mut returned_names = Vec::new();
        for name in &plan.compile {
            if done < stop {
                names.push(self.program.instances[name].result(front.salt));
                done += 1;
            } else {
                returned_names.push(*name);
            }
        }
        let packs = if done > 0 {
            vec![PackRef {
                pack: self.rng.digest(),
                size: done as u64,
                keys: [0; 32],
                packed: None,
                format: 1,
            }]
        } else {
            Vec::new()
        };
        let took = 20 + 30 * done as u64 + self.rng.below(2_000);
        let batch = Compiled {
            generation: plan.grant.lease.generation,
            roots,
            names,
            packs,
            returned_roots,
            returned_names,
        };
        (batch, took)
    }

    fn call(&mut self, from: usize, id: u64, call: Call) -> Result<(), Violation> {
        let holder_id = self.nodes[from].id.clone();
        let now = self.now;
        let mut fx = Vec::new();
        let attempt = match &call {
            Call::Join(r) => r.attempt,
            Call::Lease(r) => r.attempt,
            Call::Result(r) => r.lease.attempt,
            Call::Events(r) => r.attempt,
        };
        let reply = match &call {
            Call::Join(req) => {
                Reply::Join(self.execution.join(now, &holder_id, req, &mut fx).map_err(wire))
            }
            Call::Lease(req) => {
                Reply::Lease(self.execution.lease(now, &holder_id, req, &mut fx).map_err(wire))
            }
            Call::Result(req) => {
                let answer = self.execution.result(now, &holder_id, req, &mut fx);
                if let Err(
                    refusal @ (Refusal::Malformed(_) | Refusal::SeqReused | Refusal::NotHolder),
                ) = &answer
                {
                    return Err(self.violation((
                        "protocol",
                        format!("{holder_id}'s result refused: {refusal:?}"),
                    )));
                }
                let ok = answer.as_ref().ok().cloned();
                self.effects(fx, ok.as_ref().map(|a| (req, a)))?;
                self.checker.heard(now, &holder_id);
                let reply = Reply::Result(answer.map_err(wire));
                self.send(from, Kind::ToNode { to: from, attempt, id, reply });
                return Ok(());
            }
            Call::Events(req) => Reply::Events(
                self.execution.worker_events(now, &holder_id, req, &mut fx).map_err(wire),
            ),
        };
        if let Reply::Join(Err(e)) | Reply::Lease(Err(e)) = &reply
            && let Some(Refusal::Malformed(what)) = Refusal::of(e)
        {
            return Err(self.violation((
                "protocol",
                format!("{holder_id}'s call refused as malformed: {what}"),
            )));
        }
        self.effects(fx, None)?;
        self.checker.heard(now, &holder_id);
        self.send(from, Kind::ToNode { to: from, attempt, id, reply });
        Ok(())
    }

    fn effects(
        &mut self,
        fx: Vec<jobs::Effect>,
        result: Option<(&BatchResult, &WorkResultAnswer)>,
    ) -> Result<(), Violation> {
        self.checker.observe(self.now, &fx, result).map_err(|v| self.violation(v))?;
        for effect in fx {
            match effect {
                jobs::Effect::NeedLead => self.reassign()?,
                jobs::Effect::Kill { executor, attempt } => {
                    if let Some(at) = self.nodes.iter().position(|n| n.id == executor) {
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
        let n = &self.nodes[node];
        if n.holder.is_some() || n.refused || !n.alive || self.now < n.poll_at {
            return;
        }
        self.nodes[node].poll_at = self.now + POLL_MS;
        let running = self.execution.state() == ExecState::Running
            && self.execution.lead().is_some()
            && !self.execution.work().is_some_and(|w| w.is_done());
        if !running {
            return;
        }
        let id = self.nodes[node].id.clone();
        if self.execution.lead() == Some(id.as_str()) {
            // Its own assignment is on its way.
            return;
        }
        if let Ok(attempt) = self.execution.add_helper(self.now, &id) {
            let load_ms = self.rng.below(2_000);
            self.carry(node, Kind::Assign { to: node, attempt, lead: false, load_ms });
        }
    }

    /// The lead was lost: a new lead from the executors the execution heard from recently,
    /// as admission would choose, never the one whose front end differs (the truth the
    /// checker holds results to is the first lead's front end).
    fn reassign(&mut self) -> Result<(), Violation> {
        let heard = self.execution.heard_recently(self.now);
        let candidates: Vec<usize> = (0..self.nodes.len())
            .filter(|&i| heard.contains(&self.nodes[i].id))
            .filter(|&i| self.profile.divergent.is_none_or(|(at, _)| at != i))
            .collect();
        let mut fx = Vec::new();
        if candidates.is_empty() {
            self.execution.end(self.now, blaze_wire::Outcome::Failed, EndWhy::NoLead, &mut fx);
        } else {
            let lead = candidates[self.rng.below(candidates.len() as u64) as usize];
            let helpers: Vec<String> = candidates
                .iter()
                .filter(|&&i| i != lead)
                .map(|&i| self.nodes[i].id.clone())
                .collect();
            let lead_id = self.nodes[lead].id.clone();
            self.execution.assign(self.now, &lead_id, &helpers, &mut fx).expect("running");
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
pub fn run(seed: u64) -> Result<Summary, Violation> {
    World::new(seed).run(None)
}

/// Runs the simulation of `seed`, appending every event to the caller's trace.
pub fn trace(seed: u64, events: &mut Vec<String>) -> Result<Summary, Violation> {
    World::new(seed).run(Some(events))
}
