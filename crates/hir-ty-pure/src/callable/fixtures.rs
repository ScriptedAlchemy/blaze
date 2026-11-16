//! Concrete witnesses for the whole-token graph, with coverage and refusal controls.

use super::*;
use std::alloc::Global;

#[derive(Clone, Copy)]
enum FixtureAtom {
    Function(usize),
    Address(usize),
    Opaque,
}
impl FixtureAtom {
    fn atom(self) -> Atom {
        match self {
            Self::Function(f) => Atom::Function(Function(f)),
            Self::Address(c) => Atom::Address(Cell(c)),
            Self::Opaque => Atom::Opaque,
        }
    }
}
enum FixtureOp {
    Seed(usize, FixtureAtom),
    Copy(usize, usize),
    Load(usize, usize),
    Store(usize, usize),
    Invoke(usize, &'static [usize], usize),
    Export(usize),
}
struct FixtureEdge(usize, FixtureOp);
struct FixtureFunction {
    name: &'static str,
    arity: usize,
    effects: u32,
}
enum Closure {
    Closed,
    Open,
    Either,
}
struct Fixture {
    name: &'static str,
    root: usize,
    nodes: usize,
    cells: usize,
    functions: &'static [FixtureFunction],
    edges: &'static [FixtureEdge],
    targets: &'static [usize],
    effects: u32,
    closure: Closure,
    status: &'static str,
    failure: &'static str,
}
impl Fixture {
    fn graph(&self) -> Graph<Global> {
        let mut graph = Graph::new(Global); // alloc: fixture-owned scratch, not persisted.
        let mut nodes = Vec::with_capacity_in(self.nodes, Global); // alloc: fixture-owned node lookup.
        for (index, function) in self.functions.iter().enumerate() {
            let id = graph.function(function.arity, function.effects);
            assert_eq!(id.0, index, "{}: {} function index", self.name, function.name);
            nodes.extend_from_slice(&graph.bodies[id.0].params);
            nodes.push(graph.bodies[id.0].ret);
        }
        for _ in 0..self.cells {
            let cell = graph.cell();
            nodes.push(graph.cells[cell.0]);
        }
        for _ in nodes.len()..self.nodes {
            nodes.push(graph.node());
        }
        for FixtureEdge(owner, op) in self.edges {
            let owner = Function(*owner);
            match op {
                FixtureOp::Seed(into, atom) => {
                    graph.add(owner, Op::Seed(nodes[*into], atom.atom()))
                }
                FixtureOp::Copy(from, into) => {
                    graph.add(owner, Op::Copy { from: nodes[*from], into: nodes[*into] })
                }
                FixtureOp::Load(pointer, into) => {
                    graph.add(owner, Op::Load { pointer: nodes[*pointer], into: nodes[*into] })
                }
                FixtureOp::Store(pointer, value) => {
                    graph.add(owner, Op::Store { pointer: nodes[*pointer], value: nodes[*value] })
                }
                FixtureOp::Invoke(target, args, into) => {
                    let mut actuals = Vec::with_capacity_in(args.len(), Global); // alloc: invocation scratch.
                    for arg in *args {
                        actuals.push(nodes[*arg]);
                    }
                    graph.invoke(owner, nodes[*target], &actuals, nodes[*into]);
                }
                FixtureOp::Export(node) => graph.add(owner, Op::Export(nodes[*node])),
            }
        }
        graph
    }

    fn accepts(&self, closed: bool, active: &[bool], effects: u32) -> bool {
        let closure_ok = match self.closure {
            Closure::Closed => closed,
            Closure::Open => !closed,
            Closure::Either => true,
        };
        closure_ok
            && (!closed
                || (self.targets.iter().all(|&f| active.get(f) == Some(&true))
                    && effects & self.effects == self.effects))
    }
}

/// The refinement: a closed whole-token graph covers every recorded concrete invoked
/// target and effect; known positive cases close, selected unsupported cases do not.
/// Failed/incomplete concrete records witness prefixes only, never successful execution.
#[test]
fn closed_graphs_cover_the_recorded_callable_witnesses() {
    assert_eq!(FIXTURES.len(), 20);
    assert_eq!(FIXTURES.iter().filter(|f| matches!(f.closure, Closure::Closed)).count(), 11);
    for fixture in FIXTURES {
        let graph = fixture.graph();
        let solution = graph.solve(Function(fixture.root)).expect("finite fixture graph converges");
        assert!(
            fixture.accepts(solution.closed(), &solution.active, solution.effects()),
            "{}: closure/coverage failed; concrete status={}, failure={}; closed={}, active={:?}, effects={}",
            fixture.name,
            fixture.status,
            fixture.failure,
            solution.closed(),
            solution.active,
            solution.effects()
        );
    }
}

#[test]
fn the_oracle_catches_missing_callable_targets_and_effects() {
    let fixture = FIXTURES.iter().find(|f| f.name == "byte_full_token_alias").unwrap();
    let graph = fixture.graph();
    let solution = graph.solve(Function(fixture.root)).unwrap();
    assert!(fixture.accepts(solution.closed(), &solution.active, solution.effects()));
    let target = *fixture.targets.iter().find(|&&f| f != fixture.root).unwrap();
    let mut missing_target = Vec::with_capacity_in(solution.active.len(), Global); // alloc: planted-control scratch.
    missing_target.extend_from_slice(&solution.active);
    missing_target[target] = false;
    assert!(
        !fixture.accepts(true, &missing_target, solution.effects()),
        "missing target must fail coverage"
    );
    let missing_effect = solution.effects() & !fixture.effects;
    assert!(
        !fixture.accepts(true, &solution.active, missing_effect),
        "missing effect must fail coverage"
    );
    assert!(
        !FIXTURES.iter().all(|f| f.accepts(false, &[], 0)),
        "all-incomplete must fail positive requirement"
    );
}

include!("fixtures_data.rs");
