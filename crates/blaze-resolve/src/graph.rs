//! A directed graph with ordered nodes and edges: `src/util/graph.rs` of cargo 1.99.0
//! (5f94df4789). Iteration is in node order, which is what lock files and `cargo metadata`
//! are written in.

use std::borrow::Borrow;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

#[derive(Clone, PartialEq, Eq)]
pub struct Graph<N: Ord, E> {
    nodes: BTreeMap<N, BTreeMap<N, E>>,
}

impl<N: Ord + Clone, E: Default + Clone> Graph<N, E> {
    pub fn new() -> Graph<N, E> {
        Graph { nodes: BTreeMap::new() }
    }

    pub fn add(&mut self, node: N) {
        self.nodes.entry(node).or_default();
    }

    /// The edge from `node` to `child`, made (with its default value) if there is none.
    pub fn link(&mut self, node: N, child: N) -> &mut E {
        self.nodes.entry(node).or_default().entry(child).or_default()
    }

    pub fn contains<Q>(&self, k: &Q) -> bool
    where
        N: Borrow<Q>,
        Q: Ord + Eq + ?Sized,
    {
        self.nodes.contains_key(k)
    }

    pub fn edge(&self, from: &N, to: &N) -> Option<&E> {
        self.nodes.get(from)?.get(to)
    }

    pub fn edges(&self, from: &N) -> impl Iterator<Item = (&N, &E)> + use<'_, N, E> {
        self.nodes.get(from).into_iter().flat_map(|x| x.iter())
    }

    /// Every node after the nodes it reaches: dependencies first.
    pub fn sort(&self) -> Vec<N> {
        let mut ret = Vec::new();
        let mut marks = BTreeSet::new();
        for node in self.nodes.keys() {
            self.sort_inner_visit(node, &mut ret, &mut marks);
        }
        ret
    }

    fn sort_inner_visit(&self, node: &N, dst: &mut Vec<N>, marks: &mut BTreeSet<N>) {
        if !marks.insert(node.clone()) {
            return;
        }
        for child in self.nodes[node].keys() {
            self.sort_inner_visit(child, dst, marks);
        }
        dst.push(node.clone());
    }

    pub fn iter(&self) -> impl Iterator<Item = &N> {
        self.nodes.keys()
    }

    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    pub fn is_path_from_to(&self, from: &N, to: &N) -> bool {
        let mut stack = vec![from];
        let mut seen = BTreeSet::new();
        seen.insert(from);
        while let Some(iter) = stack.pop().and_then(|p| self.nodes.get(p)) {
            for p in iter.keys() {
                if p == to {
                    return true;
                }
                if seen.insert(p) {
                    stack.push(p);
                }
            }
        }
        false
    }
}

impl<N: Ord + Clone, E: Default + Clone> Default for Graph<N, E> {
    fn default() -> Graph<N, E> {
        Graph::new()
    }
}

impl<N: Ord + fmt::Display, E> fmt::Debug for Graph<N, E> {
    fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(fmt, "Graph {{")?;
        for (n, e) in &self.nodes {
            writeln!(fmt, "  - {}", n)?;
            for n in e.keys() {
                writeln!(fmt, "    - {}", n)?;
            }
        }
        write!(fmt, "}}")?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::Graph;

    #[test]
    fn sort_puts_dependencies_first() {
        let mut g: Graph<u32, ()> = Graph::new();
        g.link(0, 3);
        g.link(1, 0);
        g.link(2, 0);
        g.link(2, 1);
        g.add(3);
        assert_eq!(g.sort(), [3, 0, 1, 2]);
        assert!(g.is_path_from_to(&2, &3));
        assert!(!g.is_path_from_to(&3, &2));
        assert_eq!(g.edges(&2).map(|(n, _)| *n).collect::<Vec<_>>(), [0, 1]);
    }
}
