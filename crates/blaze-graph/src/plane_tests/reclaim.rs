//! Real plane transitions on an injected clock against the independent row-based model.
//! Faults remove one admission/continuation safeguard; no compiler, worker or clock runs.

use hegel::{TestCase, generators as gs};

use super::*;
use crate::plane_tests::reference;

type Checked<T> = Result<T, String>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Plant {
    None,
    OrdinaryReclaim,
    ClampGrant,
    ClearHold,
    ForgetExclusive,
    IgnoreUnbudgetedHold,
    IgnoreRoom,
    EagerPark,
    CurrentParkToken,
    SkipShape,
}

fn require(ok: bool, why: &str) -> Checked<()> {
    // alloc: one simulation failure, kept by Hegel with its shrunk draws
    if ok { Ok(()) } else { Err(why.to_owned()) }
}

#[derive(Debug, PartialEq, Eq)]
struct Snapshot {
    granted: u64,
    held: u64,
    allowance: u64,
    room: u64,
    cap: Option<u64>,
    hold: Option<NodeId>,
    reclaim: Option<TaskHandle>,
    clock: u64,
    seq: u64,
    counts: Counts,
    slots: (usize, usize, usize),
    nodes: Vec<(NodeId, Option<NodeState>, Option<Parked>, Option<Admitted>)>,
}

struct Sim {
    real: Plane,
    model: reference::Plane,
    now: u64,
    nodes: Vec<NodeId>,
    requests: Vec<RequestId>,
    starts: Vec<NodeId>,
    plant: Plant,
}

impl Sim {
    fn new(ceiling: Option<u64>, work: usize, plant: Plant) -> Sim {
        let config = PlaneConfig {
            io: 1, work, starve_after: 1,
            // alloc: the simulated ceiling's error source, once per run
            budget: ceiling.map(|bytes| Budget { bytes, from: "simulation ceiling".into(), kept_back: 0 }),
        };
        Sim {
            real: Plane::new(config.clone()).unwrap(), model: reference::Plane::new(config).unwrap(),
            now: 0, nodes: Vec::new(), requests: Vec::new(), starts: Vec::new(), plant,
        }
    }

    fn at(&mut self) -> u64 {
        self.now += 1;
        self.now
    }

    fn snapshot(&self) -> Snapshot {
        Snapshot {
            granted: self.real.granted(), held: self.real.held(), allowance: self.real.allowance(),
            room: self.real.room(), cap: self.real.cap(), hold: self.real.holding(), reclaim: self.real.reclaim,
            clock: self.real.last_ns, seq: self.real.next_seq, counts: self.real.counts(), slots: self.real.slots(),
            // alloc: the states before a refused transition, for its atomicity oracle
            nodes: self.nodes.iter().map(|&id| {
                (id, self.real.state(id).cloned(), self.real.parked(id), self.real.admitted(id))
            }).collect(),
        }
    }
}

impl Sim {
    fn oracle(&self) -> Checked<()> {
        require(self.real.granted() == self.model.granted(), "a grant was lost, clamped or returned twice")?;
        require(self.real.holding() == self.model.holding(), "the held compile reservation was lost")?;
        require(self.real.counts() == self.model.counts(), "task accounting differs from the model")?;
        for &id in &self.nodes {
            require(self.real.state(id) == self.model.state(id), "a node's state differs from the model")?;
            require(self.real.parked(id) == self.model.parked(id), "a parked continuation lost its exact epoch")?;
        }
        for &id in &self.requests {
            require(self.real.stats(id) == self.model.stats(id), "request accounting differs from the model")?;
        }
        let (mut grants, mut positive, mut reclaims) = (0u64, 0usize, 0usize);
        for &node in self.real.index.values() {
            let me = node.index() as usize;
            if self.real.states[me] != NodeState::Running {
                continue;
            }
            let task = self.real.tasks[row_of(self.real.rows[me].task)];
            if task.runner as usize != me {
                continue;
            }
            grants += task.memory;
            positive += usize::from(task.memory > 0);
            if self.real.cold[me].flags & RECLAIM != 0 {
                reclaims += 1;
                require(task.memory > 0, "a running reclamation has no positive grant")?;
            }
        }
        require(grants == self.real.granted(), "the grants do not equal running task charges")?;
        require(reclaims == 0 || (reclaims == 1 && positive == 1), "reclamation overlaps positive-grant work")
    }

    fn take(&mut self, input: PlaneInput) -> Checked<Result<Vec<PlaneOutput>, Error>> {
        match &input {
            PlaneInput::Add { nodes, .. } | PlaneInput::Done { added: nodes, .. } => {
                self.nodes.extend(nodes.iter().map(|node| node.id));
            }
            PlaneInput::AddNodes { nodes, .. } | PlaneInput::Reclaim { nodes, .. } => {
                self.nodes.extend((0..nodes.len()).map(|at| nodes.id(at)));
            }
            PlaneInput::Open { request, .. } => self.requests.push(*request),
            _ => {}
        }
        self.nodes.sort_unstable();
        self.nodes.dedup();
        self.requests.sort_unstable();
        self.requests.dedup();
        let before = self.snapshot();
        let want = self.model.step(input.clone());
        let reclaim = matches!(input, PlaneInput::Reclaim { .. });
        let given = match (self.plant, input) {
            (Plant::OrdinaryReclaim, PlaneInput::Reclaim { at_ns, request, nodes }) => {
                PlaneInput::AddNodes { at_ns, request, nodes }
            }
            (Plant::EagerPark, PlaneInput::Park { at_ns, id }) => PlaneInput::Yielded { at_ns, id },
            (Plant::CurrentParkToken, PlaneInput::Resume { at_ns, parked }) => {
                PlaneInput::Resume { at_ns, parked: self.real.parked(parked.id).unwrap_or(parked) }
            }
            (_, input) => input,
        };
        if reclaim && self.plant == Plant::IgnoreRoom {
            self.real.room = u64::MAX;
        }
        let bypassed = (self.plant == Plant::IgnoreUnbudgetedHold
            && self.real.config.budget.is_none()).then_some(self.real.hold).flatten();
        if bypassed.is_some() {
            self.real.hold = None;
        }
        let got = self.apply(given);
        if bypassed.is_some() {
            // Preserve the reservation itself: only the admission turn ignored it.
            self.real.hold = bypassed;
        }
        if reclaim && self.plant == Plant::IgnoreRoom {
            self.real.room = before.room;
        }
        if reclaim && let Some(task) = self.real.reclaim {
            match self.plant {
                Plant::ClampGrant => {
                    let capped = self.real.budget_now().unwrap_or(u64::MAX);
                    let t = &mut self.real.tasks[row_of(task)];
                    let old = t.memory;
                    t.memory = old.min(capped);
                    self.real.granted -= old - t.memory;
                }
                Plant::ClearHold => self.real.hold = None,
                Plant::ForgetExclusive => self.real.reclaim = None,
                _ => {}
            }
        }
        if want.is_err() {
            require(self.snapshot() == before, "a refused input changed the plane before reporting its error")?;
        }
        // alloc: only a failed simulation's transcript, naming both transitions
        if got != want {
            return Err(format!("transition differs from the reference: got {got:?}, expected {want:?}"));
        }
        self.oracle()?;
        if let Ok(out) = &got {
            self.starts.extend(out.iter().filter_map(|it| match it {
                PlaneOutput::Run { id, .. } => Some(*id),
                _ => None,
            }));
        }
        Ok(got)
    }

    fn apply(&mut self, input: PlaneInput) -> Result<Vec<PlaneOutput>, Error> {
        if self.plant == Plant::SkipShape && let PlaneInput::Reclaim { at_ns, request, nodes } = input {
            // The real add/dispatch with only the shape guard removed.
            let (mut out, mut touched) = (Vec::new(), Vec::new());
            let at = at_ns.max(self.real.last_ns);
            self.real.add_nodes(at, request, nodes, true, &mut out, &mut touched)?;
            self.real.after(at, &mut out, &mut touched);
            Ok(out)
        } else {
            self.real.step(input)
        }
    }
}

impl Sim {
    fn step(&mut self, input: PlaneInput) -> Checked<Vec<PlaneOutput>> {
        self.take(input)?.map_err(|error| error.to_string())
    }

    fn refused(&mut self, input: PlaneInput, expected: Error) -> Checked<()> {
        require(self.take(input)? == Err(expected), "the transition did not give its typed refusal")
    }

    fn open(&mut self, request: u32) -> Checked<()> {
        let at_ns = self.at();
        self.step(PlaneInput::Open { at_ns, request: RequestId(request), weight: 1, keep_going: true, idle: false })?;
        Ok(())
    }

    fn add(&mut self, request: u32, nodes: Vec<Spec>) -> Checked<Vec<PlaneOutput>> {
        let at_ns = self.at();
        self.step(PlaneInput::Add { at_ns, request: RequestId(request), nodes })
    }

    fn close(&mut self, request: u32) -> Checked<()> {
        let at_ns = self.at();
        self.step(PlaneInput::Close { at_ns, request: RequestId(request) })?;
        Ok(())
    }

    fn reclaim(&mut self, request: u32, node: Spec) -> Checked<Vec<PlaneOutput>> {
        let at_ns = self.at();
        self.step(PlaneInput::Reclaim { at_ns, request: RequestId(request), nodes: columns(&[node]) })
    }

    fn done(&mut self, id: u32, outcome: Outcome) -> Checked<Vec<PlaneOutput>> {
        let at_ns = self.at();
        self.step(PlaneInput::Done { at_ns, id: NodeId(id), outcome, added: Vec::new(), group: None, peak: 0 })
    }

    fn cap(&mut self, bytes: Option<u64>) -> Checked<()> {
        let at_ns = self.at();
        self.step(PlaneInput::Cap { at_ns, bytes })?;
        Ok(())
    }

    fn held(&mut self, bytes: u64) -> Checked<()> {
        let at_ns = self.at();
        self.step(PlaneInput::Held { at_ns, bytes })?;
        Ok(())
    }

    fn allowance(&mut self, bytes: u64) -> Checked<()> {
        let at_ns = self.at();
        self.step(PlaneInput::Allowance { at_ns, bytes })?;
        Ok(())
    }

    fn cancel(&mut self, request: u32) -> Checked<()> {
        let at = self.at();
        let got = self.real.cancel(at, RequestId(request));
        let want = self.model.cancel(at, RequestId(request));
        require(got == want, "cancellation differs from the model")?;
        self.oracle()
    }

    fn release(&mut self, request: u32) -> Checked<()> {
        require(self.real.release(RequestId(request)) == self.model.release(RequestId(request)),
                "request release differs from the model")?;
        self.oracle()
    }

    fn running(&self, id: u32) -> bool {
        self.real.state(NodeId(id)) == Some(&NodeState::Running)
    }

    fn park(&mut self, id: u32) -> Checked<Parked> {
        let at_ns = self.at();
        self.step(PlaneInput::Park { at_ns, id: NodeId(id) })?;
        self.real.parked(NodeId(id)).ok_or_else(|| "a live unposted task has no continuation".into())
    }

    fn resume(&mut self, parked: Parked) -> Checked<()> {
        let at_ns = self.at();
        self.step(PlaneInput::Resume { at_ns, parked })?;
        Ok(())
    }

    fn refuse_parked(&mut self, parked: Parked, why: &str) -> Checked<()> {
        let at_ns = self.at();
        // alloc: one simulated compiler refusal carried to its exact parked node
        self.step(PlaneInput::RefuseParked { at_ns, parked, why: why.to_owned() })?;
        Ok(())
    }

    fn stale(&mut self, parked: Parked) -> Checked<()> {
        let at_ns = self.at() + 10_000;
        self.refused(PlaneInput::Resume { at_ns, parked }, Error::NotParked(parked.id))?;
        let at_ns = self.at() + 10_000;
        self.refused(PlaneInput::RefuseParked { at_ns, parked, why: "stale refusal".into() },
                     Error::NotParked(parked.id))
    }

    fn drain(&mut self, ceiling: u64) -> Checked<()> {
        self.held(0)?;
        self.cap(None)?;
        for _ in 0..=self.nodes.len() {
            self.allowance(ceiling)?;
            let running = self.nodes.iter().copied().find(|&id| self.real.state(id) == Some(&NodeState::Running));
            match running {
                Some(id) => { self.done(id.0, Outcome::Built)?; }
                None => return require(self.nodes.iter().all(|&id| self.real.state(id).is_none_or(NodeState::settled)),
                                       "finite ready work did not finish within its node bound"),
            }
        }
        Err("finite ready work exhausted its completion bound".into())
    }
}

fn node(id: u32, memory: u64) -> Spec {
    Spec {
        id: NodeId(id), kind: NodeKind::Codegen,
        // alloc: one simulated node's recorded name
        name: format!("unit {id}"), deps: Vec::new(), pool: Pool::Work, cost: 1, key: None, memory,
    }
}

fn release_node(id: u32, memory: u64) -> Spec {
    Spec { kind: NodeKind::Store, ..node(id, memory) }
}

fn columns(nodes: &[Spec]) -> Nodes {
    let first = nodes.first().map_or(NodeId(20), |node| node.id);
    let mut out = Nodes::new(first);
    for node in nodes {
        out.push(NodeRef { kind: node.kind, name: &node.name, deps: &node.deps,
                           pool: node.pool, cost: node.cost, key: node.key, memory: node.memory });
    }
    out
}

fn reclamation(ceiling: u64, workers: usize, terminal: u8, noise: &[u8], plant: Plant) -> Checked<()> {
    let mut sim = Sim::new(Some(ceiling), workers, plant);
    for request in 1..=3 { sim.open(request)?; }
    sim.add(1, vec![node(1, ceiling / 2)])?;
    let big = Spec { cost: 100, ..node(2, 3 * ceiling / 4) };
    sim.add(1, vec![big, node(3, ceiling / 4)])?;
    require(sim.real.holding() == Some(NodeId(2)), "the fixture did not reserve its starved compile")?;
    sim.cap(Some(0))?;
    sim.held(2 * ceiling)?;
    sim.done(1, Outcome::Built)?;
    sim.done(3, Outcome::Built)?;
    require(sim.real.granted() == 0, "the fixture did not drain its running grants")?;

    // A Store label alone confers no exemption; only the dedicated input has ownership.
    sim.add(2, vec![release_node(4, 1)])?;
    require(!sim.running(4), "ordinary Store work bypassed pressure or the held compile")?;
    let need = ceiling / 8;
    sim.reclaim(2, release_node(5, need))?;
    require(sim.running(5) && sim.real.admitted(NodeId(5)).is_some_and(|it| it.granted == need),
            "reclamation did not start with its full positive grant under cap 0")?;
    let at_ns = sim.at();
    sim.refused(PlaneInput::Reclaim { at_ns, request: RequestId(2), nodes: columns(&[release_node(5, need)]) },
                Error::Twice(NodeId(5)))?;
    sim.reclaim(3, release_node(6, need))?;
    require(!sim.running(6), "a second reclamation overlapped the first")?;
    sim.add(1, vec![Spec { pool: Pool::Io, ..node(7, 0) }, node(8, 0)])?;
    require(sim.running(7) && sim.running(8), "zero-memory work lost its ordinary worker opportunity")?;
    for &draw in noise {
        // A deterministic sequence of independent ledger, pressure and footprint reports.
        sim.now += u64::from(draw % 5);
        match draw % 4 {
            0 => sim.cap(Some(u64::from(draw) * ceiling / 255))?,
            1 => sim.cap(None)?,
            2 => sim.held(u64::from(draw) * ceiling / 127)?,
            _ => sim.allowance(need + u64::from(draw) * (ceiling - need) / 255)?,
        }
        require(!sim.running(2) && !sim.running(4) && !sim.running(6),
                "positive work entered while reclamation held its grant")?;
    }
    sim.held(0)?;
    sim.cap(None)?;
    sim.allowance(ceiling)?;
    sim.done(8, Outcome::Built)?;
    require(!sim.running(2) && !sim.running(4) && !sim.running(6),
            "a free worker or cleared cap bypassed active reclamation")?;
    require(sim.real.holding() == Some(NodeId(2)), "cleanup consumed the compile's reservation")?;
    match terminal % 4 {
        0 => { sim.done(5, Outcome::Built)?; }
        1 => { sim.done(5, Outcome::Failed { why: "release refused".into() })?; }
        2 => {
            sim.cancel(2)?;
            require(sim.running(5) && sim.real.granted() == need, "cancellation returned an active grant early")?;
            sim.done(5, Outcome::Built)?;
        }
        _ => {
            sim.cancel(2)?;
            require(sim.running(5) && sim.real.granted() == need, "cancellation returned an active grant early")?;
            let at_ns = sim.at();
            sim.step(PlaneInput::Yielded { at_ns, id: NodeId(5) })?;
        }
    }
    sim.drain(ceiling)?;
    let held = sim.starts.iter().position(|id| *id == NodeId(2));
    require(held.is_some(), "the held compile never got its later opportunity")?;
    if let Some(smaller) = sim.starts.iter().position(|id| *id == NodeId(4)) {
        require(held.unwrap() < smaller, "a smaller ordinary task overtook the held compile")?;
    }
    Ok(())
}

fn bounds_and_shapes(ceiling: u64, plant: Plant) -> Checked<()> {
    let mut sim = Sim::new(Some(ceiling), 2, plant);
    sim.open(1)?;
    sim.cap(Some(0))?;
    sim.held(ceiling)?;
    sim.allowance(ceiling - 1)?;
    sim.reclaim(1, release_node(1, ceiling))?;
    require(!sim.running(1) && sim.real.granted() == 0, "reclamation exceeded the machine allowance")?;
    sim.cap(None)?;
    require(!sim.running(1), "reclamation took an unmeasured compile's partial-grant exception")?;
    sim.cap(Some(0))?;
    sim.allowance(ceiling)?;
    require(sim.running(1) && sim.real.granted() == ceiling, "the actual allowance did not admit the full prediction")?;
    sim.done(1, Outcome::Built)?;

    // The machine's allowance is large but its last locked turn reserved no bytes for us.
    sim.held(0)?;
    sim.allowance(ceiling)?;
    require(sim.real.room() == 0, "the fixture did not return the ledger reservation")?;
    sim.reclaim(1, release_node(2, ceiling / 8))?;
    require(!sim.running(2), "reclamation spent bytes reserved for another daemon")?;
    sim.allowance(ceiling)?;
    require(sim.running(2), "a new locked allowance did not admit waiting reclamation")?;
    sim.done(2, Outcome::Built)?;
    sim.reclaim(1, release_node(3, ceiling + 1))?;
    require(matches!(sim.real.state(NodeId(3)), Some(NodeState::Failed { why }) if why.contains("unit 3") && why.contains("memory")),
            "a reclamation over the configured ceiling was not refused by name")?;

    let id = 20;
    let valid = release_node(id, ceiling / 8);
    let bad = [
        (vec![valid.clone(), release_node(id + 1, 1)], "exactly one node is required"),
        (Vec::new(), "exactly one node is required"),
        (vec![node(id, 1)], "the node must be Store work"),
        (vec![Spec { pool: Pool::Io, ..valid.clone() }], "the node must be Store work"),
        (vec![Spec { key: Some(Key([7; 32])), ..valid.clone() }], "the node must be unkeyed and dependency-free"),
        (vec![Spec { deps: vec![NodeId(1)], ..valid.clone() }], "the node must be unkeyed and dependency-free"),
        (vec![release_node(id, 0)], "the memory prediction must be positive"),
    ];
    for (nodes, why) in bad {
        let at_ns = sim.at() + 10_000;
        sim.refused(PlaneInput::Reclaim { at_ns, request: RequestId(1), nodes: columns(&nodes) },
                    Error::InvalidReclaim { request: 1, first: NodeId(id), why })?;
    }
    // The same IDs remain usable; no rejected prefix entered the index, events or clock.
    sim.reclaim(1, valid)?;
    sim.drain(ceiling)?;
    Ok(())
}

fn parked_cycle(ceiling: u64, budgeted: bool, waits: u8, plant: Plant) -> Checked<()> {
    let mut sim = Sim::new(budgeted.then_some(ceiling), 1, plant);
    sim.open(1)?;
    sim.open(2)?;
    let need = ceiling / 8;
    sim.add(1, vec![node(1, ceiling / 2)])?;
    require(sim.running(1) && sim.real.granted() == ceiling / 2,
            "the unposted successor did not own its worker and full grant")?;
    let first = sim.park(1)?;
    require(sim.real.granted() == 0 && sim.real.state(NodeId(1)) == Some(&NodeState::Waiting),
            "parking did not return the grant and suspend the successor")?;
    let at_ns = sim.at();
    sim.refused(PlaneInput::Park { at_ns, id: NodeId(1) }, Error::NotRunning(NodeId(1)))?;
    let at_ns = sim.at();
    sim.refused(PlaneInput::Done { at_ns, id: NodeId(1), outcome: Outcome::Built,
                                  added: Vec::new(), group: None, peak: 0 }, Error::NotRunning(NodeId(1)))?;
    sim.cap(Some(0))?;
    sim.held(2 * ceiling)?;
    for _ in 0..waits {
        sim.allowance(ceiling)?;
        require(sim.real.parked(NodeId(1)) == Some(first) && sim.real.granted() == 0,
                "an unrelated ledger turn resumed a parked successor")?;
    }
    sim.reclaim(2, release_node(2, need))?;
    require(sim.running(2) && sim.real.granted() == need,
            "the parked successor left a one-worker reclamation cycle")?;
    // This free IO worker would admit positive work through the old no-budget fast path.
    sim.add(2, vec![Spec { pool: Pool::Io, ..node(3, need) }])?;
    require(!sim.running(3), "positive work overlapped reclamation on another pool")?;
    sim.cap(None)?;
    sim.held(0)?;
    sim.allowance(ceiling)?;
    require(sim.real.parked(NodeId(1)) == Some(first) && !sim.running(3),
            "clearing pressure resumed unposted work or overlapped its cleanup")?;
    sim.done(2, Outcome::Built)?;
    sim.allowance(ceiling)?;
    require(sim.running(3), "completion did not release reclamation's positive-grant exclusion")?;
    sim.done(3, Outcome::Built)?;
    require(sim.real.parked(NodeId(1)) == Some(first) && sim.real.granted() == 0,
            "a completed cleanup resumed work without its saved continuation")?;
    sim.resume(first)?;
    sim.allowance(ceiling)?;
    require(sim.running(1), "a valid continuation did not return to ordinary admission")?;
    let second = sim.park(1)?;
    require(first.task == second.task && first.epoch != second.epoch,
            "parking the same task again did not issue a new continuation epoch")?;
    sim.stale(first)?;
    sim.refuse_parked(second, "front input failed")?;
    require(matches!(sim.real.state(NodeId(1)), Some(NodeState::Failed { why }) if why == "front input failed"),
            "refusing parked work lost its node-specific cause")?;
    require(sim.real.granted() == 0, "refusing parked work returned its already-returned grant again")?;
    sim.stale(second)?;
    sim.close(1)?;
    sim.release(1)?;
    parked_reuse(&mut sim, ceiling, first, second)?;
    sim.close(2)?;
    sim.release(2)?;
    Ok(())
}

fn parked_reuse(sim: &mut Sim, ceiling: u64, first: Parked, second: Parked) -> Checked<()> {
    // Reuse the external IDs and the freed task slot, while its generation and park epoch advance.
    sim.open(1)?;
    sim.add(1, vec![node(1, ceiling / 2)])?;
    sim.allowance(ceiling)?;
    let current = sim.park(1)?;
    require(current.id == first.id && current.task as u32 == first.task as u32 && current.task != first.task,
            "the fixture did not reuse its node ID and task slot with a new generation")?;
    sim.stale(first)?;
    sim.stale(second)?;
    sim.stale(Parked { task: first.task, ..current })?;
    sim.stale(Parked { epoch: second.epoch, ..current })?;
    sim.stale(Parked { id: NodeId(99), ..current })?;
    sim.cancel(1)?;
    require(matches!(sim.real.state(NodeId(1)), Some(NodeState::Skipped { .. }))
                && sim.real.parked(NodeId(1)).is_none() && sim.real.granted() == 0,
            "cancelling parked work kept a continuation or returned its grant twice")?;
    sim.stale(current)?;
    sim.release(1)?;

    // A Post may already be queued when cancellation wins. Park retires that unposted
    // runner successfully, and the caller must accept that no continuation remains.
    sim.open(1)?;
    sim.add(1, vec![node(1, ceiling / 2)])?;
    sim.allowance(ceiling)?;
    sim.cancel(1)?;
    require(sim.running(1) && sim.real.granted() == ceiling / 2,
            "cancellation returned a running unposted task's grant before it stopped")?;
    let at_ns = sim.at();
    sim.step(PlaneInput::Park { at_ns, id: NodeId(1) })?;
    require(matches!(sim.real.state(NodeId(1)), Some(NodeState::Skipped { .. }))
                && sim.real.parked(NodeId(1)).is_none() && sim.real.granted() == 0,
            "parking canceled unposted work did not retire it without a continuation")?;
    sim.stale(current)?;
    sim.release(1)?;
    Ok(())
}

fn parking_guards(ceiling: u64) -> Checked<()> {
    let mut sim = Sim::new(Some(ceiling), 2, Plant::None);
    sim.open(1)?;
    let keyed = Spec { key: Some(Key([1; 32])), ..node(1, ceiling / 4) };
    sim.add(1, vec![keyed.clone()])?;
    let at_ns = sim.at();
    sim.refused(PlaneInput::Park { at_ns, id: NodeId(1) }, Error::NotParkable(NodeId(1)))?;
    sim.add(1, vec![Spec { id: NodeId(2), ..keyed }])?;
    let at_ns = sim.at();
    sim.refused(PlaneInput::Park { at_ns, id: NodeId(1) }, Error::NotParkable(NodeId(1)))?;
    let at_ns = sim.at();
    sim.refused(PlaneInput::Park { at_ns, id: NodeId(2) }, Error::NotRunning(NodeId(2)))?;
    sim.add(1, vec![Spec { pool: Pool::Io, ..node(3, ceiling / 4) }])?;
    let at_ns = sim.at();
    sim.refused(PlaneInput::Park { at_ns, id: NodeId(3) }, Error::NotParkable(NodeId(3)))?;
    sim.done(1, Outcome::Built)?;
    sim.done(3, Outcome::Built)?;

    let at_ns = sim.at();
    sim.step(PlaneInput::Open { at_ns, request: RequestId(2), weight: 1, keep_going: false, idle: false })?;
    sim.add(2, vec![node(4, ceiling / 4), node(5, ceiling / 4)])?;
    sim.done(4, Outcome::Failed { why: "another compile failed".into() })?;
    require(sim.running(5), "the fail-fast fixture stopped its unposted runner prematurely")?;
    let at_ns = sim.at();
    sim.step(PlaneInput::Park { at_ns, id: NodeId(5) })?;
    require(matches!(sim.real.state(NodeId(5)), Some(NodeState::Skipped { because: 4 }))
                && sim.real.parked(NodeId(5)).is_none() && sim.real.granted() == 0,
            "parking fail-fast unposted work kept a dead continuation or grant")?;
    Ok(())
}

fn unbudgeted_hold(plant: Plant) -> Checked<()> {
    let mut sim = Sim::new(None, 3, plant);
    sim.open(1)?;
    sim.open(2)?;
    sim.add(1, vec![node(1, 64)])?;
    sim.reclaim(1, Spec { cost: 100, ..release_node(2, 16) })?;
    sim.add(1, vec![node(3, 16)])?;
    require(sim.running(1) && sim.running(3) && sim.real.holding() == Some(NodeId(2)),
            "the fixture did not reserve a passed reclamation without a configured budget")?;
    sim.add(2, vec![node(4, 16), node(5, 0)])?;
    require(!sim.running(4) && sim.running(5) && sim.real.holding() == Some(NodeId(2)),
            "the no-budget fast path bypassed reclamation's reserved turn")?;
    sim.done(1, Outcome::Built)?;
    require(!sim.running(2) && !sim.running(4), "a positive grant was ignored to start reserved work")?;
    sim.done(3, Outcome::Built)?;
    require(sim.running(2) && !sim.running(4), "the reserved reclamation lost its first idle opportunity")?;
    sim.done(2, Outcome::Built)?;
    require(sim.running(4), "finishing reclamation did not release its reserved turn")?;
    sim.done(4, Outcome::Built)?;
    sim.done(5, Outcome::Built)?;
    require(sim.real.granted() == 0, "the no-budget sequence leaked a grant")
}

/// The invariant: reclamation bypasses temporary pressure only with a full positive grant,
/// preserves a held compile's later opportunity, and excludes every other positive grant
/// until its actual completion or yield. Cancellation alone returns no running capacity.
#[hegel::test(test_cases = 64)]
fn reclamation_preserves_full_grants_reservations_and_worker_limits(tc: TestCase) {
    let ceiling = 64 + 8 * u64::from(tc.draw(gs::integers::<u8>().max_value(120)));
    let workers = 2 + usize::from(tc.draw(gs::integers::<u8>().max_value(2)));
    let terminal = tc.draw(gs::integers::<u8>().max_value(3));
    let count = usize::from(tc.draw(gs::integers::<u8>().max_value(24)));
    // alloc: the bounded, shrinkable sequence of ledger and pressure inputs for this run
    let noise: Vec<u8> = (0..count).map(|_| tc.draw(gs::integers::<u8>())).collect();
    reclamation(ceiling, workers, terminal, &noise, Plant::None).unwrap();
}

/// The invariant: even cleanup fits the configured ceiling, the current machine allowance
/// and its recorded reservation. A malformed reclamation changes no state, event or clock.
#[hegel::test(test_cases = 64)]
fn reclamation_refuses_invalid_shapes_and_respects_actual_machine_bounds(tc: TestCase) {
    let ceiling = 64 + 8 * u64::from(tc.draw(gs::integers::<u8>().max_value(120)));
    bounds_and_shapes(ceiling, Plant::None).unwrap();
}

/// The refinement: an admitted but unposted successor gives its exact grant and worker
/// back once while its front input is reclaimed. Only its saved task generation and park
/// epoch can resume or refuse it; a missing, repeated or stale continuation cannot run it.
#[hegel::test(test_cases = 64)]
fn parked_unposted_work_releases_the_lane_without_losing_its_continuation(tc: TestCase) {
    let ceiling = 64 + 8 * u64::from(tc.draw(gs::integers::<u8>().max_value(120)));
    let waits = tc.draw(gs::integers::<u8>().max_value(8));
    for budgeted in [false, true] {
        parked_cycle(ceiling, budgeted, waits, Plant::None).unwrap();
    }
    parking_guards(ceiling).unwrap();
}

#[test]
fn parked_work_unblocks_reclamation_with_one_worker_and_no_budget() {
    parked_cycle(128, false, 3, Plant::None).unwrap();
}

#[test]
fn a_reclamation_hold_is_respected_without_a_configured_budget() {
    unbudgeted_hold(Plant::None).unwrap();
}

#[test]
fn the_oracle_catches_reclamation_treated_as_ordinary_work() {
    reclamation(128, 3, 0, &[0, 1, 3], Plant::None).unwrap();
    assert!(reclamation(128, 3, 0, &[0, 1, 3], Plant::OrdinaryReclaim).unwrap_err()
        .contains("transition differs from the reference"));
}

#[test]
fn the_oracle_catches_a_reclamation_grant_clamped_to_the_pressure_cap() {
    reclamation(128, 3, 0, &[], Plant::None).unwrap();
    assert!(reclamation(128, 3, 0, &[], Plant::ClampGrant).unwrap_err()
        .contains("a grant was lost, clamped or returned twice"));
}

#[test]
fn the_oracle_catches_a_lost_held_compile_reservation() {
    reclamation(128, 3, 0, &[], Plant::None).unwrap();
    assert!(reclamation(128, 3, 0, &[], Plant::ClearHold).unwrap_err()
        .contains("the held compile reservation was lost"));
}

#[test]
fn the_oracle_catches_positive_work_overlapping_reclamation() {
    reclamation(128, 3, 0, &[], Plant::None).unwrap();
    assert!(reclamation(128, 3, 0, &[], Plant::ForgetExclusive).unwrap_err()
        .contains("transition differs from the reference"));
}

#[test]
fn the_oracle_catches_a_reclamation_hold_bypassed_without_a_budget() {
    unbudgeted_hold(Plant::None).unwrap();
    assert!(unbudgeted_hold(Plant::IgnoreUnbudgetedHold).unwrap_err()
        .contains("transition differs from the reference"));
}

#[test]
fn the_oracle_catches_reclamation_spending_unreserved_machine_room() {
    bounds_and_shapes(128, Plant::None).unwrap();
    assert!(bounds_and_shapes(128, Plant::IgnoreRoom).unwrap_err()
        .contains("transition differs from the reference"));
}

#[test]
fn the_oracle_catches_a_park_that_eagerly_readmits_unposted_work() {
    parked_cycle(128, true, 3, Plant::None).unwrap();
    assert!(parked_cycle(128, true, 3, Plant::EagerPark).unwrap_err()
        .contains("transition differs from the reference"));
}

#[test]
fn the_oracle_catches_a_stale_continuation_replaced_with_the_current_token() {
    parked_cycle(128, true, 3, Plant::None).unwrap();
    assert!(parked_cycle(128, true, 3, Plant::CurrentParkToken).unwrap_err()
        .contains("a refused input changed the plane before reporting its error"));
}

#[test]
fn the_oracle_catches_a_rejected_reclamation_prefix_entering_the_plane() {
    bounds_and_shapes(128, Plant::None).unwrap();
    assert!(bounds_and_shapes(128, Plant::SkipShape).unwrap_err()
        .contains("a refused input changed the plane before reporting its error"));
}
