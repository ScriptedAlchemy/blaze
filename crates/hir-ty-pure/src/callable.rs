//! Reference callable-origin solver, not yet connected to MIR or effect proofs.
//!
//! Values carry known functions, addresses of storage, or opaque bits. Formal inputs are
//! empty ports until a call supplies its actual values. Storage writes, loads and returned
//! callbacks compose in one root's graph; memory-access facts are not origin evidence.
//! All graph and solution storage belongs to the caller's scratch allocator. Nothing is
//! cached or returned independently of the graph. This first language models whole values;
//! partial representations, vtables and implicit drop require further extraction rules.

use std::alloc::Allocator;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Node(usize);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Function(usize);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Cell(usize);

#[derive(Clone, Copy)]
pub(super) enum Atom {
    Function(Function),
    Address(Cell),
    Opaque,
}

struct Body<A: Allocator> {
    params: Vec<Node, A>,
    ret: Node,
    effects: u32,
}

pub(super) enum Op<A: Allocator> {
    Seed(Node, Atom),
    Copy {
        from: Node,
        into: Node,
    },
    Load {
        pointer: Node,
        into: Node,
    },
    Store {
        pointer: Node,
        value: Node,
    },
    Invoke {
        target: Node,
        args: Vec<Node, A>,
        into: Node,
    },
    /// A callable value leaves the proof boundary, without being invoked here.
    Export(Node),
}

struct Edge<A: Allocator> {
    owner: Function,
    op: Op<A>,
}

pub(super) struct Graph<A: Allocator> {
    alloc: A,
    nodes: usize,
    cells: Vec<Node, A>,
    bodies: Vec<Body<A>, A>,
    edges: Vec<Edge<A>, A>,
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum Incomplete {
    SizeOverflow,
    DidNotConverge,
}

impl<A: Allocator + Clone> Graph<A> {
    pub(super) fn new(alloc: A) -> Self {
        Self {
            nodes: 0,
            cells: Vec::new_in(alloc.clone()),
            bodies: Vec::new_in(alloc.clone()),
            edges: Vec::new_in(alloc.clone()),
            alloc,
        }
    }

    pub(super) fn node(&mut self) -> Node {
        let node = Node(self.nodes);
        self.nodes += 1;
        node
    }

    pub(super) fn cell(&mut self) -> Cell {
        let cell = Cell(self.cells.len());
        let node = self.node();
        self.cells.push(node);
        cell
    }

    pub(super) fn function(&mut self, arity: usize, effects: u32) -> Function {
        let function = Function(self.bodies.len());
        let mut params = Vec::with_capacity_in(arity, self.alloc.clone());
        for _ in 0..arity {
            params.push(self.node());
        }
        let ret = self.node();
        self.bodies.push(Body { params, ret, effects });
        function
    }

    pub(super) fn param(&self, function: Function, index: usize) -> Node {
        self.bodies[function.0].params[index]
    }

    pub(super) fn returned(&self, function: Function) -> Node {
        self.bodies[function.0].ret
    }

    pub(super) fn storage(&self, cell: Cell) -> Node {
        self.cells[cell.0]
    }

    pub(super) fn add(&mut self, owner: Function, op: Op<A>) {
        self.edges.push(Edge { owner, op });
    }

    pub(super) fn invoke(&mut self, owner: Function, target: Node, args: &[Node], into: Node) {
        let mut actuals = Vec::with_capacity_in(args.len(), self.alloc.clone());
        actuals.extend_from_slice(args);
        self.add(owner, Op::Invoke { target, args: actuals, into });
    }

    pub(super) fn solve(&self, root: Function) -> Result<Solution<'_, A>, Incomplete> {
        let width = self
            .bodies
            .len()
            .checked_add(self.cells.len())
            .and_then(|n| n.checked_add(1))
            .ok_or(Incomplete::SizeOverflow)?;
        let bits = self.nodes.checked_mul(width).ok_or(Incomplete::SizeOverflow)?;
        let changes = bits.checked_add(self.bodies.len()).ok_or(Incomplete::SizeOverflow)?;
        let mut solution = Solution {
            graph: self,
            width,
            bits: Vec::new_in(self.alloc.clone()),
            active: Vec::new_in(self.alloc.clone()),
            unresolved_reads: false,
        };
        solution.bits.resize(bits, false);
        solution.active.resize(self.bodies.len(), false);
        solution.active[root.0] = true;
        // A non-final sweep adds at least one bit. There are `bits` value memberships
        // and one activation per body, so this bound follows from the finite domain.
        for _ in 0..=changes {
            let mut changed = false;
            for edge in &self.edges {
                if solution.active[edge.owner.0] {
                    changed |= solution.step(&edge.op);
                }
            }
            if !changed {
                // Empty ports are bottom while actuals, stores and returns propagate.
                // Once those settle, a read still lacking an origin is unknown, not
                // evidence that another known value accounts for every possibility.
                for edge in &self.edges {
                    if solution.active[edge.owner.0] {
                        changed |= solution.complete_reads(&edge.op);
                    }
                }
                if !changed {
                    return Ok(solution);
                }
            }
        }
        Err(Incomplete::DidNotConverge)
    }
}

pub(super) struct Solution<'g, A: Allocator> {
    graph: &'g Graph<A>,
    width: usize,
    bits: Vec<bool, A>,
    active: Vec<bool, A>,
    unresolved_reads: bool,
}

impl<A: Allocator> Solution<'_, A> {
    fn atom(&self, atom: Atom) -> usize {
        match atom {
            Atom::Function(function) => function.0,
            Atom::Address(cell) => self.graph.bodies.len() + cell.0,
            Atom::Opaque => self.width - 1,
        }
    }

    fn has(&self, node: Node, atom: Atom) -> bool {
        self.bits[node.0 * self.width + self.atom(atom)]
    }

    fn insert(&mut self, node: Node, atom: Atom) -> bool {
        let index = node.0 * self.width + self.atom(atom);
        let changed = !self.bits[index];
        self.bits[index] = true;
        changed
    }

    fn copy(&mut self, from: Node, into: Node) -> bool {
        let mut changed = false;
        for atom in 0..self.width {
            if self.bits[from.0 * self.width + atom] {
                let index = into.0 * self.width + atom;
                changed |= !self.bits[index];
                self.bits[index] = true;
            }
        }
        changed
    }

    fn complete_read(&mut self, node: Node) -> bool {
        let start = node.0 * self.width;
        if self.bits[start..start + self.width].iter().any(|bit| *bit) {
            false
        } else {
            self.unresolved_reads = true;
            self.insert(node, Atom::Opaque)
        }
    }

    fn complete_reads(&mut self, op: &Op<A>) -> bool {
        match op {
            &Op::Copy { from, .. } => self.complete_read(from),
            &Op::Load { pointer, .. } => {
                let mut changed = self.complete_read(pointer);
                for c in 0..self.graph.cells.len() {
                    if self.has(pointer, Atom::Address(Cell(c))) {
                        changed |= self.complete_read(self.graph.cells[c]);
                    }
                }
                changed
            }
            &Op::Store { pointer, value } => {
                self.complete_read(pointer) | self.complete_read(value)
            }
            Op::Invoke { target, args, .. } => {
                let mut changed = self.complete_read(*target);
                for &arg in args {
                    changed |= self.complete_read(arg);
                }
                changed
            }
            &Op::Export(node) => self.complete_read(node),
            Op::Seed(..) => false,
        }
    }

    fn havoc(&mut self) -> bool {
        let mut changed = false;
        for index in 0..self.graph.cells.len() {
            changed |= self.insert(self.graph.cells[index], Atom::Opaque);
        }
        changed
    }

    fn unknown_pointer(&self, node: Node) -> bool {
        self.has(node, Atom::Opaque)
            || (0..self.graph.bodies.len()).any(|f| self.has(node, Atom::Function(Function(f))))
    }

    fn unknown_callable(&self, node: Node) -> bool {
        self.has(node, Atom::Opaque)
            || (0..self.graph.cells.len()).any(|c| self.has(node, Atom::Address(Cell(c))))
    }

    fn step(&mut self, op: &Op<A>) -> bool {
        match op {
            &Op::Seed(node, atom) => self.insert(node, atom),
            &Op::Copy { from, into } => self.copy(from, into),
            &Op::Load { pointer, into } => {
                let mut changed = false;
                for c in 0..self.graph.cells.len() {
                    if self.has(pointer, Atom::Address(Cell(c))) {
                        changed |= self.copy(self.graph.cells[c], into);
                    }
                }
                if self.unknown_pointer(pointer) {
                    changed |= self.insert(into, Atom::Opaque);
                }
                changed
            }
            &Op::Store { pointer, value } => {
                let mut changed = false;
                for c in 0..self.graph.cells.len() {
                    if self.has(pointer, Atom::Address(Cell(c))) {
                        changed |= self.copy(value, self.graph.cells[c]);
                    }
                }
                if self.unknown_pointer(pointer) {
                    changed |= self.havoc();
                }
                changed
            }
            Op::Invoke { target, args, into } => {
                let mut changed = false;
                let mut unknown = self.unknown_callable(*target);
                for f in 0..self.graph.bodies.len() {
                    if !self.has(*target, Atom::Function(Function(f))) {
                        continue;
                    }
                    let body = &self.graph.bodies[f];
                    if body.params.len() != args.len() {
                        unknown = true;
                        continue;
                    }
                    changed |= !self.active[f];
                    self.active[f] = true;
                    for (i, &actual) in args.iter().enumerate() {
                        changed |= self.copy(actual, self.graph.bodies[f].params[i]);
                    }
                    changed |= self.copy(self.graph.bodies[f].ret, *into);
                }
                if unknown {
                    changed |= self.insert(*into, Atom::Opaque);
                    changed |= self.havoc();
                }
                changed
            }
            Op::Export(_) => false,
        }
    }

    /// No unresolved reads, and coverage of explicit callable invocations/exports.
    /// This small language says nothing about MIR, implicit drop or an effect proof.
    pub(super) fn closed(&self) -> bool {
        if self.unresolved_reads {
            return false;
        }
        self.graph.edges.iter().filter(|edge| self.active[edge.owner.0]).all(|edge| {
            let (node, arity) = match &edge.op {
                Op::Invoke { target, args, .. } => (*target, Some(args.len())),
                Op::Export(node) => (*node, None),
                _ => return true,
            };
            !self.unknown_callable(node)
                && (0..self.graph.bodies.len()).any(|f| self.has(node, Atom::Function(Function(f))))
                && (0..self.graph.bodies.len()).all(|f| {
                    !self.has(node, Atom::Function(Function(f)))
                        || arity.is_none_or(|n| self.graph.bodies[f].params.len() == n)
                })
        })
    }

    pub(super) fn effects(&self) -> u32 {
        self.graph
            .bodies
            .iter()
            .zip(&self.active)
            .filter(|(_, active)| **active)
            .fold(0, |all, (body, _)| all | body.effects)
    }
}

#[cfg(test)]
#[path = "callable/fixtures.rs"]
mod fixtures;

#[cfg(test)]
mod tests {
    use std::alloc::Global;

    use super::*;

    // alloc: the graph and solution are test-owned scratch, never query outputs.
    fn graph() -> Graph<Global> {
        Graph::new(Global)
    }

    #[test]
    fn an_indirect_factory_returns_a_known_callback_without_access_facts() {
        let mut g = graph();
        let root = g.function(0, 0);
        let factory = g.function(0, 0);
        let known = g.function(0, 1);
        let dormant = g.function(0, 2);
        let f = g.node();
        let callback = g.node();
        let unused = g.node();
        g.add(root, Op::Seed(f, Atom::Function(factory)));
        g.add(root, Op::Seed(unused, Atom::Function(dormant)));
        g.add(factory, Op::Seed(g.bodies[factory.0].ret, Atom::Function(known)));
        g.invoke(root, f, &[], callback);
        g.invoke(root, callback, &[], g.bodies[root.0].ret);
        let s = g.solve(root).unwrap();
        assert!(s.closed());
        assert!(s.has(callback, Atom::Function(known)));
        assert_eq!(s.effects(), 1);
        assert!(!s.active[dormant.0]);
    }

    #[test]
    fn stores_and_returns_compose_across_helpers_and_full_value_views() {
        let mut g = graph();
        let root = g.function(0, 0);
        let put = g.function(2, 0);
        let get = g.function(1, 0);
        let callback = g.function(0, 4);
        let cell = g.cell();
        let address = g.node();
        let bytes = g.node();
        let target = g.node();
        let setter = g.node();
        let getter = g.node();
        let loaded = g.node();
        let ignored = g.node();
        g.add(root, Op::Seed(address, Atom::Address(cell)));
        g.add(root, Op::Copy { from: address, into: bytes });
        g.add(root, Op::Seed(target, Atom::Function(callback)));
        g.add(root, Op::Seed(setter, Atom::Function(put)));
        g.add(root, Op::Seed(getter, Atom::Function(get)));
        g.add(
            put,
            Op::Store { pointer: g.bodies[put.0].params[0], value: g.bodies[put.0].params[1] },
        );
        g.add(get, Op::Load { pointer: g.bodies[get.0].params[0], into: g.bodies[get.0].ret });
        g.invoke(root, setter, &[bytes, target], ignored);
        g.invoke(root, getter, &[bytes], loaded);
        g.invoke(root, loaded, &[], ignored);
        let s = g.solve(root).unwrap();
        assert!(s.closed());
        assert_eq!(s.effects(), 4);
        assert!(s.has(loaded, Atom::Function(callback)));
        drop(s);
        g.edges.reverse();
        let reordered = g.solve(root).unwrap();
        assert!(reordered.closed());
        assert_eq!(reordered.effects(), 4);
        assert!(reordered.has(loaded, Atom::Function(callback)));
    }

    #[test]
    fn opaque_bits_cannot_be_cleared_by_a_known_target_or_a_scalar_view() {
        let mut g = graph();
        let root = g.function(0, 0);
        let known = g.function(0, 0);
        let scalar = g.node();
        let bytes = g.node();
        let loaded = g.node();
        let out = g.node();
        g.add(root, Op::Seed(scalar, Atom::Opaque));
        g.add(root, Op::Copy { from: scalar, into: bytes });
        g.add(root, Op::Load { pointer: bytes, into: loaded });
        g.add(root, Op::Seed(loaded, Atom::Function(known)));
        assert!(g.solve(root).unwrap().closed(), "ordinary opaque data is not an invocation");
        g.invoke(root, loaded, &[], out);
        let s = g.solve(root).unwrap();
        assert!(!s.closed());
        assert!(s.has(loaded, Atom::Opaque));
        assert!(s.has(loaded, Atom::Function(known)));
    }

    #[test]
    fn a_slot_address_is_not_the_function_stored_in_it() {
        let mut g = graph();
        let root = g.function(0, 0);
        let known = g.function(0, 0);
        let cell = g.cell();
        let address = g.node();
        let out = g.node();
        g.add(root, Op::Seed(g.cells[cell.0], Atom::Function(known)));
        g.add(root, Op::Seed(address, Atom::Address(cell)));
        g.invoke(root, address, &[], out);
        assert!(!g.solve(root).unwrap().closed());
    }

    #[test]
    fn an_unknown_call_havocs_storage_and_keeps_its_return_unknown() {
        let mut g = graph();
        let root = g.function(0, 0);
        let known = g.function(0, 0);
        let cell = g.cell();
        let unknown = g.node();
        let address = g.node();
        let loaded = g.node();
        let out = g.node();
        g.add(root, Op::Seed(unknown, Atom::Opaque));
        g.add(root, Op::Seed(address, Atom::Address(cell)));
        g.add(root, Op::Seed(g.cells[cell.0], Atom::Function(known)));
        g.invoke(root, unknown, &[], out);
        g.add(root, Op::Load { pointer: address, into: loaded });
        let s = g.solve(root).unwrap();
        assert!(!s.closed());
        assert!(s.has(loaded, Atom::Opaque));
        assert!(s.has(out, Atom::Opaque));
    }

    #[test]
    fn a_known_join_cannot_hide_an_unresolved_load() {
        for from_empty_cell in [false, true] {
            let mut g = graph();
            let root = g.function(0, 0);
            let known = g.function(0, 1);
            let cell = g.cell();
            let pointer = g.node();
            let target = g.node();
            let out = g.node();
            if from_empty_cell {
                g.add(root, Op::Seed(pointer, Atom::Address(cell)));
            }
            g.add(root, Op::Load { pointer, into: target });
            g.add(root, Op::Seed(target, Atom::Function(known)));
            g.invoke(root, target, &[], out);
            let s = g.solve(root).unwrap();
            assert!(!s.closed());
            assert!(s.has(target, Atom::Function(known)));
            assert!(s.has(target, Atom::Opaque));
        }
    }

    #[test]
    fn an_empty_storage_read_is_incomplete_even_when_the_result_is_not_called() {
        let mut g = graph();
        let root = g.function(0, 0);
        let cell = g.cell();
        let pointer = g.node();
        let ret = g.bodies[root.0].ret;
        g.add(root, Op::Seed(pointer, Atom::Address(cell)));
        g.add(root, Op::Load { pointer, into: ret });
        let s = g.solve(root).unwrap();
        assert!(s.has(ret, Atom::Opaque));
        assert!(!s.closed());
    }

    #[test]
    fn a_known_join_cannot_hide_a_copy_of_an_unbound_value() {
        let mut g = graph();
        let root = g.function(1, 0);
        let known = g.function(0, 1);
        let target = g.node();
        let out = g.node();
        g.add(root, Op::Copy { from: g.bodies[root.0].params[0], into: target });
        g.add(root, Op::Seed(target, Atom::Function(known)));
        g.invoke(root, target, &[], out);
        let s = g.solve(root).unwrap();
        assert!(!s.closed());
        assert!(s.has(target, Atom::Function(known)));
        assert!(s.has(target, Atom::Opaque));
    }

    #[test]
    fn a_trampoline_binds_all_actuals_and_an_unbound_input_stays_unproved() {
        let mut g = graph();
        let root = g.function(0, 0);
        let trampoline = g.function(9, 0);
        let known = g.function(0, 8);
        let actual = g.node();
        let target = g.node();
        let out = g.node();
        g.add(root, Op::Seed(actual, Atom::Function(known)));
        g.add(root, Op::Seed(target, Atom::Function(trampoline)));
        g.invoke(trampoline, g.bodies[trampoline.0].params[8], &[], g.bodies[trampoline.0].ret);
        g.invoke(root, target, &[actual; 9], out);
        let s = g.solve(root).unwrap();
        assert!(s.closed());
        assert_eq!(s.effects(), 8);
        assert!(!g.solve(trampoline).unwrap().closed());
        assert!(g.solve(root).unwrap().closed(), "a standalone solve cannot poison actuals");
        g.invoke(root, target, &[actual; 8], out);
        assert!(
            !g.solve(root).unwrap().closed(),
            "a mismatched call cannot silently zip arguments"
        );
    }

    #[test]
    fn an_export_obligation_keeps_unknown_callback_results_visible() {
        let mut g = graph();
        let root = g.function(0, 0);
        let ret = g.bodies[root.0].ret;
        g.add(root, Op::Seed(ret, Atom::Opaque));
        g.add(root, Op::Export(ret));
        assert!(!g.solve(root).unwrap().closed());
    }

    #[test]
    fn recursive_ports_settle_without_claiming_termination() {
        let mut g = graph();
        let root = g.function(0, 0);
        let recursive = g.function(1, 16);
        let known = g.function(0, 0);
        let target = g.node();
        let input = g.node();
        let again = g.node();
        let ret = g.bodies[recursive.0].ret;
        let param = g.bodies[recursive.0].params[0];
        g.add(root, Op::Seed(target, Atom::Function(recursive)));
        g.add(root, Op::Seed(input, Atom::Function(known)));
        g.add(recursive, Op::Seed(again, Atom::Function(recursive)));
        g.add(recursive, Op::Copy { from: param, into: ret });
        g.invoke(recursive, again, &[param], ret);
        g.invoke(root, target, &[input], g.bodies[root.0].ret);
        let s = g.solve(root).unwrap();
        assert!(s.closed());
        assert!(s.has(g.bodies[root.0].ret, Atom::Function(known)));
        assert_eq!(s.effects(), 16);
    }
}
