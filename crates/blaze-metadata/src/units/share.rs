//! The second pass over the graph, cargo 1.99.0's `rebuild_unit_graph_shared`: a unit for the
//! target that is the host's own is the host's, so that it is shared with the same unit a build
//! script or a proc macro needs; `strip` is lowered where no unit has debuginfo; and debuginfo is
//! weakened in a unit only the host's build needs. A unit's identity then includes its
//! dependencies (cargo's `dep_hash`), as two units that differ only there are two.

use blaze_resolve::InternedString;
use rustc_hash::FxHashMap;

use super::deps::Graph;
use super::model::{Env, Inner, Kind};

/// An edge of the shared graph.
pub(super) struct Dep {
    pub unit: usize,
    pub name: InternedString,
}

/// The graph after sharing, its units in cargo's order.
pub(super) struct Shared {
    pub units: Vec<Inner>,
    pub deps: Vec<Vec<Dep>>,
    pub roots: Vec<usize>,
}

type Key =
    (blaze_resolve::PackageId, usize, Kind, super::model::CMode, Vec<InternedString>, Vec<usize>);

struct Sharing<'g> {
    graph: &'g Graph,
    /// The shared unit of each unit of the graph, as first computed.
    memo: FxHashMap<usize, usize>,
    units: Vec<Inner>,
    deps: Vec<Vec<Dep>>,
    index: FxHashMap<Key, Vec<usize>>,
}

pub(super) fn share(env: &Env<'_>, graph: &Graph, roots: &[usize]) -> Shared {
    let mut sharing = Sharing {
        graph,
        memo: FxHashMap::default(),
        units: Vec::new(),
        deps: Vec::new(),
        index: FxHashMap::default(),
    };
    let roots: Vec<usize> = roots.iter().map(|&root| sharing.traverse(root, false)).collect();
    let Sharing { units, deps, .. } = sharing;

    // The units in cargo's order; those that differ only in their dependencies in the order
    // they were met.
    let mut order: Vec<usize> = (0..units.len()).collect();
    order.sort_by(|&a, &b| env.cmp_units(&units[a], &units[b]).then(a.cmp(&b)));
    let mut rank = vec![0; units.len()];
    for (at, &old) in order.iter().enumerate() {
        rank[old] = at;
    }
    let mut units: Vec<Option<Inner>> = units.into_iter().map(Some).collect();
    let mut deps: Vec<Option<Vec<Dep>>> = deps.into_iter().map(Some).collect();
    Shared {
        units: order.iter().filter_map(|&old| units[old].take()).collect(),
        deps: order
            .iter()
            .filter_map(|&old| deps[old].take())
            .map(|list| {
                list.into_iter().map(|d| Dep { unit: rank[d.unit], name: d.name }).collect()
            })
            .collect(),
        roots: roots.into_iter().map(|root| rank[root]).collect(),
    }
}

impl Sharing<'_> {
    fn traverse(&mut self, unit: usize, is_for_host: bool) -> usize {
        if let Some(&shared) = self.memo.get(&unit) {
            return shared;
        }
        let graph = self.graph;
        let mut new_deps: Vec<Dep> = Vec::new();
        for edge in &graph.deps[unit] {
            let to = self.traverse(edge.unit, edge.unit_for.is_for_host());
            new_deps.push(Dep { unit: to, name: edge.name });
        }
        let source = &graph.arena.units[unit];
        // The target's own kind, which is the host's when nothing is built for another target.
        let kind = match source.kind {
            Kind::Target => Kind::Host,
            Kind::Host => Kind::Host,
        };

        let mut profile = source.profile.clone();
        // `strip` is lowered to debuginfo where nothing has debuginfo: the standard library's
        // is what is left.
        if profile.strip.is_deferred()
            && !profile.debuginfo.is_turned_on()
            && new_deps.iter().all(|d| !self.units[d.unit].profile.debuginfo.is_turned_on())
        {
            profile.strip = profile.strip.clone().strip_debuginfo();
        }
        // A build dependency not shared with the target's units has its debuginfo weakened.
        if is_for_host && profile.debuginfo.is_deferred() {
            // A probe: whether there is a unit with the debuginfo the target would give it.
            let canonical = profile.debuginfo.finalize();
            let mut probe = Inner { kind: Kind::Target, ..source.clone() };
            probe.profile = profile.clone();
            probe.profile.debuginfo = canonical;
            // A proc macro is already Host before sharing. Cargo's explicit target probe
            // only matches a runtime unit, not the deferred host unit itself.
            profile.debuginfo =
                if graph.arena.find(&probe).is_some() { canonical } else { canonical.weaken() };
        }

        let list: Vec<usize> = new_deps.iter().map(|d| d.unit).collect();
        let key: Key =
            (source.pkg, source.target, kind, source.mode, source.features.clone(), list);
        let bucket = self.index.entry(key).or_default();
        let found = bucket.iter().copied().find(|&at| self.units[at].profile == profile);
        let shared = match found {
            Some(at) => at,
            None => {
                let at = self.units.len();
                bucket.push(at);
                self.units.push(Inner { profile, kind, ..source.clone() });
                self.deps.push(new_deps);
                at
            }
        };
        self.memo.insert(unit, shared);
        shared
    }
}
