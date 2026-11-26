//! Script runs reachable from checked crates. A build dependency is compiled on the host,
//! so its script can be a producer without having a crate in the check world. These are the
//! resolved host units. D52's dependency compilation for WASI requires its own target
//! projection before its module can take an output receipt from this planner.

use blaze_ids::{ContentId, Hasher};
use blaze_manifest::lock::{Checksum, Lock};
use blaze_own::{AVec, Alloc};
use blaze_proc_macro::script_unit::{self, Consumer};
use blaze_unit_graph::{UnitGraph, UnitId};
use blaze_world::{CheckoutPath, PathKind, Side};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct RunId {
    pub(crate) unit: UnitId,
    pub(crate) side: Side,
    pub(crate) consumer: Consumer,
}

impl RunId {
    fn host(unit: UnitId) -> Self {
        Self { unit, side: Side::Target, consumer: Consumer::HostDependency }
    }

    pub(crate) fn slot(self) -> usize {
        match (self.consumer, self.side) {
            (Consumer::Checked, Side::Target) => 0,
            (Consumer::Checked, Side::Macro) => 1,
            (Consumer::HostDependency, _) => 2,
        }
    }
}

pub(crate) struct RunPlan<'a> {
    pub(crate) id: RunId,
    pub(crate) inputs: AVec<'a, RunId>,
}

/// Each root and its reachable host producers. A run's inputs include its `links` reads and
/// the scripts of the libraries compiled into it. Reading a build-dependency output does not
/// make it `DEP_*` metadata. That still follows the separate `links` projection.
pub(crate) fn runs<'a>(graph: &UnitGraph<'_>, roots: impl Iterator<Item = RunId>, alloc: Alloc<'a>) -> AVec<'a, RunPlan<'a>> {
    let mut plans = AVec::new_in(alloc);
    let mut pending = AVec::new_in(alloc);
    pending.extend(roots);
    let mut seen = AVec::new_in(alloc);
    seen.resize(graph.units.len(), 0u8);
    let mut libraries = AVec::new_in(alloc);
    let mut reached = AVec::new_in(alloc);
    reached.resize(graph.units.len(), 0usize);
    let mut walk = 0usize;
    while let Some(id) = pending.pop() {
        let mark = &mut seen[id.unit.index()];
        let bit = 1 << id.slot();
        if *mark & bit != 0 {
            continue;
        }
        *mark |= bit;
        let mut inputs = AVec::new_in(alloc);
        for (unit, _, _) in script_unit::links_closure_for(graph, id.unit, id.consumer) {
            inputs.push(RunId { unit, ..id });
        }
        // On a script unit the resolver leaves only build-dependency edges. On the libraries
        // reached from it there are only normal dependency edges. Follow each library once.
        walk += 1;
        libraries.extend(graph.units[id.unit.index()].deps.iter().map(|dep| dep.unit));
        while let Some(lib) = libraries.pop() {
            let mark = &mut reached[lib.index()];
            if *mark == walk {
                continue;
            }
            *mark = walk;
            let lib = &graph.units[lib.index()];
            if let Some(unit) = lib.build_script {
                inputs.push(RunId::host(unit));
            }
            libraries.extend(lib.deps.iter().map(|dep| dep.unit));
        }
        inputs.sort_unstable_by_key(|input| (input.unit, input.slot()));
        inputs.dedup();
        pending.extend(inputs.iter().copied().filter(|input| input.consumer == Consumer::HostDependency));
        plans.push(RunPlan { id, inputs });
    }
    plans
}

/// A standalone producer has no crate key. Its source covers the pinned package, the script
/// and its host configuration. Module bytes and the outputs of other scripts are task reads.
pub(crate) fn host_source(graph: &UnitGraph<'_>, lock: &Lock<'_>, paths: &[CheckoutPath<'_>], unit: UnitId) -> Option<ContentId> {
    let script = &graph.units[unit.index()];
    let consumer = &graph.units[script_unit::served_for(graph, unit, Consumer::HostDependency)?.index()];
    let package = lock.package(script.package);
    let mut hash = Hasher::new("blaze host build-dependency script source 1");
    hash.part(crate::graph::TRIPLE.as_bytes()).part(script.name.as_bytes()).part(script.version.as_bytes())
        .part(package.source_text.as_bytes()).part(script.root.as_bytes())
        .opt_part(script.links.as_ref().map(|links| links.as_bytes()));
    hash.opt_part(match &package.checksum {
        Checksum::Absent => None,
        Checksum::Sha256(sum) => Some(sum.as_bytes()),
    });
    if let Some(path) = paths.iter().find(|path| path.id == script.package) {
        hash.part(match path.kind { PathKind::Member => b"member", PathKind::External => b"external" }).part(path.dir.as_bytes());
    }
    hash.part(b"features");
    for feature in &script.features {
        hash.part(feature.as_bytes());
    }
    hash.part(b"host cfg");
    for atom in &consumer.cfg {
        hash.part(atom.name.as_bytes()).opt_part(atom.value.as_ref().map(|value| value.as_bytes()));
    }
    if let Some(meta) = graph.meta_of(script.package) {
        hash.part(b"package metadata");
        for (name, value) in meta.vars().pairs() {
            hash.part(name.as_bytes()).part(value.as_bytes());
        }
    }
    Some(hash.finish())
}

/// A distinct host run even if a checked crate's script has the same name and features.
pub(crate) fn host_name(source: ContentId) -> blaze_drive::Name {
    let subject = Hasher::new("blaze host build-dependency script subject 1").id(&source).finish();
    blaze_drive::Name::new(blaze_drive::Kind::Script, subject)
}
