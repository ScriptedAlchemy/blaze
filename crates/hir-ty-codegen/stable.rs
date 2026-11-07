//! Names for instances that are the same in every run of the daemon on the same sources, so
//! that code compiled by one run can be found by the next. A crate of the checkout is named
//! by what is written in it, so an edit makes other names of everything in it.

use std::{
    cell::RefCell,
    fmt::Write,
    sync::{Arc, PoisonError, RwLock},
};

use hir_def::{
    AdtId, AssocItemId, CallableDefId, DefWithBodyId, FunctionId, HasModule, Lookup, ModuleDefId,
    StaticId, TraitId, expr_store::Body, item_scope::ItemScope, nameres::crate_def_map,
};
use hir_expand::{HirFileId, MacroCallKind};
use rustc_ast_ir::Mutability;
use rustc_hash::{FxHashMap, FxHashSet};
use rustc_type_ir::{
    AliasTy, AliasTyKind, BoundVarIndexKind, ExistentialPredicate, GenericArgKind, RegionKind,
    inherent::{IntoKind, Safety as _, Term as _},
};
use salsa::plumbing::{AsId, FromId};
use span::ErasedFileAstId;

use base_db::Crate;

use super::{Instance, InstanceDef, InstanceIn, SymbolResolver};
use hir_ty::{
    ImplTraitId, InferBodyId,
    consteval::try_const_usize,
    db::{ClosureKey, HirDatabase, InternedClosure, InternedClosureId, InternedCoroutineId},
    next_solver::{GenericArgs, Region, TermId, Ty, TyKind},
};

/// Before the content of a crate of the checkout, where a crate of rustc's alone has its id.
const OF_CHECKOUT: char = '~';

/// After the name of an instance compiled with `-C overflow-checks`.
const CHECKED: &str = " checked";

const SEMANTIC_INPUTS: &str = " inputs4:";

pub(super) fn without_semantic_inputs(name: &str) -> &str {
    match name.rsplit_once(SEMANTIC_INPUTS) {
        Some((name, hash)) if hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_hexdigit()) => {
            name
        }
        _ => name,
    }
}

#[cfg(test)]
pub(super) fn has_semantic_inputs(name: &str) -> bool {
    without_semantic_inputs(name).len() != name.len()
}

pub(super) fn semantic_digest(name: &str) -> Option<&str> {
    let (base, digest) = name.rsplit_once(SEMANTIC_INPUTS)?;
    (without_semantic_inputs(name) == base).then_some(digest)
}

/// Before what stands for a crate from outside the checkout that rustc has no id for.
const OF_LIBRARY: char = '^';

/// After a crate's id, before what a debugger is told of its code (0 nothing, 1 lines, 2
/// variables), where that is not what it is told of the build's own code.
const DEBUG: &str = ".d";

/// What a debugger is told of the code `name` names, when the crate it is written in is told
/// otherwise than the build's own code ([`DEBUG`]); `None` when it is told what that is. The
/// kinds of instance whose code is written in a crate spell that crate first.
pub(super) fn told_in(name: &str) -> Option<super::Told> {
    let kinds = ["fn ", "closure ", "coroutine by move ", "coroutine ", "vtable shim "];
    let rest = kinds.iter().find_map(|kind| name.strip_prefix(kind))?;
    let (_, told) = rest[..rest.find(':')?].rsplit_once(DEBUG)?;
    match told {
        "0" => Some(super::Told::Nothing),
        "1" => Some(super::Told::Lines),
        "2" => Some(super::Told::Variables),
        _ => None,
    }
}

/// Before the hash that stands for a type written longer than [`SPELLED`]: a closure that
/// wraps closures has a type that holds each of theirs, and written out in full its name
/// doubles with each level.
const HASHED: char = '$';

/// The longest a type is written out in a name.
const SPELLED: usize = 1024;

enum Declared {
    Function(FunctionId),
    Static(StaticId),
}

/// Lowers what [`StableNames`] lists of `crates`, so that a build that lists them later
/// finds it done. `lower` is given the bodies of each round to lower at once.
pub fn lower_declared(db: &dyn HirDatabase, crates: &[Crate], lower: &dyn Fn(&[DefWithBodyId])) {
    declared(db, crates, lower, &mut |_| ());
}

/// The functions and statics `crates` declare, in their modules and in the blocks of their
/// bodies: a `static` is often written by a macro.
fn declared(
    db: &dyn HirDatabase,
    crates: &[Crate],
    lower: &dyn Fn(&[DefWithBodyId]),
    found: &mut dyn FnMut(Declared),
) {
    let mut bodies: Vec<DefWithBodyId> = Vec::new();
    let mut scope = |scope: &ItemScope, bodies: &mut Vec<DefWithBodyId>| {
        for def in scope.declarations() {
            match def {
                ModuleDefId::StaticId(id) => {
                    found(Declared::Static(id));
                    bodies.push(id.into());
                }
                ModuleDefId::FunctionId(id) => {
                    found(Declared::Function(id));
                    bodies.push(id.into());
                }
                ModuleDefId::ConstId(id) => bodies.push(id.into()),
                // A method a trait provides has blocks of its own.
                ModuleDefId::TraitId(id) => {
                    for &(_, item) in id.trait_items(db).items.iter() {
                        match item {
                            AssocItemId::FunctionId(id) => bodies.push(id.into()),
                            AssocItemId::ConstId(id) => bodies.push(id.into()),
                            AssocItemId::TypeAliasId(_) => {}
                        }
                    }
                }
                _ => {}
            }
        }
        bodies.extend(scope.unnamed_consts().map(DefWithBodyId::from));
        for imp in scope.impls() {
            for &(_, item) in imp.impl_items(db).items.iter() {
                match item {
                    AssocItemId::FunctionId(id) => {
                        found(Declared::Function(id));
                        bodies.push(id.into());
                    }
                    AssocItemId::ConstId(id) => bodies.push(id.into()),
                    AssocItemId::TypeAliasId(_) => {}
                }
            }
        }
    };
    for &krate in crates {
        for (_, module) in crate_def_map(db, krate).modules(db) {
            scope(module.scope(db), &mut bodies);
        }
    }
    while !bodies.is_empty() {
        let round = std::mem::take(&mut bodies);
        lower(&round);
        for body in round {
            for (_, def_map) in Body::of(db, body).blocks(db) {
                for (_, module) in def_map.modules(db) {
                    scope(module.scope(db), &mut bodies);
                }
            }
        }
    }
}

/// The crates by what a name starts with, and what the ones asked about declare, for every
/// [`StableNames`] of a build: to list a crate is to go through every body of it.
#[derive(Default, Clone)]
pub(super) struct Listings(std::sync::Arc<std::sync::Mutex<Listing>>);

#[derive(Default)]
struct Listing {
    crates: Option<FxHashMap<String, Vec<Crate>>>,
    listed: FxHashMap<String, Listed>,
}

/// The statics and the functions without type parameters that one build of a crate declares.
#[derive(Default)]
struct Listed {
    statics: FxHashMap<String, StaticId>,
    functions: FxHashMap<String, Vec<FunctionId>>,
}

/// One SCC of the semantic call graph: what its own bodies observed, and which SCCs they reach.
/// A certificate's transitive provenance is this graph and is never kept flattened: roots of
/// a shared call graph overlap, so a flattened copy per root is quadratic in the graph.
struct SemanticComponent {
    contextual: bool,
    constants: Box<[Arc<str>]>,
    sources: Box<[Arc<super::cached::SemanticSource>]>,
    dependencies: Box<[usize]>,
    /// The digest of the complete closure, once it is asked for; `None` for a closure that is
    /// not canonical, which has two names for one file.
    digest: std::sync::OnceLock<Option<Box<str>>>,
}

/// What decides how an instance is spelled, and so what its semantic node says.
#[derive(PartialEq, Eq)]
struct NamingOf {
    owned: FxHashSet<Crate>,
    by_content: bool,
    frozen: FxHashMap<Crate, super::ContentId>,
}

/// The semantic call graph of one build, for every [`StableNames`] that spells as it does.
///
/// Every thread of a build names roots and callees with a view of its own, and those closures
/// overlap: with a graph per view each thread walks the whole program and keeps its own copy of
/// what the program observed, which is what passed an 8 GiB bound with one function compiled.
/// A node is a pure function of its key and the [`NamingOf`], so one made by any thread is good
/// for all, and a certificate's digest is of what its closure holds and not of how the graph
/// was cut in components, so threads that race to the same closure agree. No lock is held while
/// a node is made or a closure is walked.
pub(super) struct SemanticGraph {
    naming: NamingOf,
    /// What is walked and not yet in a component; those are found by `certificates` first.
    nodes: RwLock<FxHashMap<InstanceIn, Arc<SemanticNode>>>,
    // Completed SCCs. Arena edges avoid recursive graph drops.
    components: RwLock<Vec<Arc<SemanticComponent>>>,
    certificates: RwLock<FxHashMap<InstanceIn, Result<usize, &'static str>>>,
}

impl SemanticGraph {
    fn new(naming: NamingOf) -> Arc<Self> {
        Arc::new(SemanticGraph {
            naming,
            nodes: RwLock::default(),
            components: RwLock::default(),
            certificates: RwLock::default(),
        })
    }

    fn certificate(&self, key: &InstanceIn) -> Option<Result<usize, &'static str>> {
        self.certificates.read().unwrap_or_else(PoisonError::into_inner).get(key).copied()
    }

    /// The first answer stands, for every view to go on with the same one.
    fn settle(&self, key: InstanceIn, answer: Result<usize, &'static str>) {
        self.certificates
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .entry(key)
            .or_insert(answer);
    }

    fn component(&self, at: usize) -> Arc<SemanticComponent> {
        self.components.read().unwrap_or_else(PoisonError::into_inner)[at].clone()
    }

    fn add_component(&self, component: SemanticComponent) -> usize {
        let mut arena = self.components.write().unwrap_or_else(PoisonError::into_inner);
        arena.push(Arc::new(component));
        arena.len() - 1
    }
}

/// The complete transitive provenance of an instance, by reference into the [`SemanticGraph`]
/// of the [`StableNames`] that made it, for as long as that graph lives. Its closure is
/// canonical. Ask that view for what is needed of it: [`StableNames::covers`], or [`StableNames::complete_inputs`] for the
/// owned value an entry carries to the store.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct SemanticCertificate(usize);

/// Asks whether one certificate covers others. What it has found of the parent's closure is
/// for one entry's callees, which are many, and goes with it: see [`StableNames::covers`].
pub(super) struct SemanticCoverage {
    parent: Option<SemanticCertificate>,
    reached: Option<FxHashMap<usize, Arc<SemanticComponent>>>,
}

impl SemanticCoverage {
    pub(super) fn of(parent: Option<SemanticCertificate>) -> Self {
        SemanticCoverage { parent, reached: None }
    }
}

struct SemanticNode {
    contextual: bool,
    constants: Vec<Arc<str>>,
    sources: Vec<Arc<super::cached::SemanticSource>>,
    children: Vec<InstanceIn>,
}

pub(super) struct StableNames<'a, 'db> {
    db: &'db dyn HirDatabase,
    symbols: &'a dyn SymbolResolver,
    /// The crates whose code changes under the daemon.
    owned: FxHashSet<Crate>,
    /// Whether what is of those crates has a name, by what is written in them now.
    by_content: bool,
    /// Where cargo keeps the sources it fetched on this machine.
    cargo_home: Option<String>,
    library: Option<super::LibrarySources>,
    /// The checkout's directory here, which [`super::CHECKOUT`] stands for.
    checkout: Option<String>,
    files: RefCell<FxHashMap<HirFileId, Option<String>>>,
    /// The crates by what a name starts with, and what the ones asked about declare.
    listings: Listings,
    /// How each crate and each type is written, or why it has no name.
    crate_names: RefCell<FxHashMap<Crate, Result<Box<str>, &'static str>>>,
    tys: RefCell<FxHashMap<Ty<'db>, Result<Box<str>, &'static str>>>,
    /// What each closure asked about is written as, by a hash of its source.
    written: RefCell<FxHashMap<InternedClosure<'db>, Option<Box<str>>>>,
    /// The ids crates of the checkout are named by in place of what is written in them now:
    /// those of a base, whose names an edit leaves to the code they are of.
    frozen: FxHashMap<Crate, super::ContentId>,
    /// Why the last name that could not be written could not.
    missed: std::cell::Cell<&'static str>,
    /// How often each thing kept an instance from having a name.
    pub(super) misses: RefCell<FxHashMap<&'static str, usize>>,
    /// The view's own, until it joins one with [`Self::sharing`].
    graph: Arc<SemanticGraph>,
    #[cfg(test)]
    semantic_visits: std::cell::Cell<usize>,
    #[cfg(test)]
    semantic_flatten_visits: std::cell::Cell<usize>,
}

impl<'a, 'db> StableNames<'a, 'db> {
    pub(super) fn new(
        db: &'db dyn HirDatabase,
        symbols: &'a dyn SymbolResolver,
        owned: FxHashSet<Crate>,
        by_content: bool,
    ) -> Self {
        StableNames {
            db,
            symbols,
            graph: SemanticGraph::new(NamingOf {
                owned: owned.clone(),
                by_content,
                frozen: FxHashMap::default(),
            }),
            owned,
            by_content,
            cargo_home: symbols.cargo_home(),
            library: symbols.library_sources(),
            checkout: symbols.checkout(),
            files: RefCell::default(),
            listings: Listings::default(),
            crate_names: RefCell::default(),
            tys: RefCell::default(),
            written: RefCell::default(),
            frozen: FxHashMap::default(),
            missed: std::cell::Cell::new(""),
            misses: RefCell::default(),
            #[cfg(test)]
            semantic_visits: std::cell::Cell::new(0),
            #[cfg(test)]
            semantic_flatten_visits: std::cell::Cell::new(0),
        }
    }

    /// Lists the crates with `listings`, which other names of the build share.
    pub(super) fn listing(mut self, listings: Listings) -> Self {
        self.listings = listings;
        self
    }

    pub(super) fn listings(&self) -> Listings {
        self.listings.clone()
    }

    /// Names each crate of `frozen` by its id there, whatever is written in it now.
    pub(super) fn frozen(mut self, frozen: FxHashMap<Crate, super::ContentId>) -> Self {
        self.graph = SemanticGraph::new(NamingOf {
            owned: self.owned.clone(),
            by_content: self.by_content,
            frozen: frozen.clone(),
        });
        self.frozen = frozen;
        self
    }

    /// The graph of this view, for the other threads of a build to share with
    /// [`Self::sharing`].
    pub(super) fn semantic_graph(&self) -> Arc<SemanticGraph> {
        self.graph.clone()
    }

    /// Walks `graph` in place of a graph of its own, when it is of names spelled as this view
    /// spells them. Otherwise the view keeps its own: its nodes would not be the same.
    pub(super) fn sharing(mut self, graph: Arc<SemanticGraph>) -> Self {
        if graph.naming == self.graph.naming {
            self.graph = graph;
        }
        self
    }

    /// The id a crate of the checkout is named by.
    pub(super) fn content_id(&self, krate: Crate) -> Option<super::ContentId> {
        self.frozen.get(&krate).copied().or_else(|| self.symbols.content_id(krate))
    }

    /// The name of a file as every machine has it, as panic locations name it in an entry.
    pub(super) fn file_path(&self, file: base_db::FileId) -> Option<String> {
        self.stored(self.symbols.source_path(file)?)
    }

    /// Whether a name is of anything of a crate of the checkout, which is only what the
    /// name says while the checkout is as it was: rustc's build of it can be of another time.
    pub(super) fn of_checkout(name: &str) -> bool {
        name.contains(OF_CHECKOUT)
    }

    fn default_environment(
        &self,
        instance: &Instance,
    ) -> Option<hir_ty::traits::StoredParamEnvAndCrate> {
        let krate = match super::written_in(self.db, instance.def) {
            Some(krate) => krate,
            None => {
                let mut crates = super::home::mentioned_crates(self.db, instance)?
                    .into_iter()
                    .map(|krate| {
                        let mut name = String::new();
                        self.crate_(&mut name, krate)?;
                        Some((name, krate))
                    })
                    .collect::<Option<Vec<_>>>()?;
                crates.sort_by(|a, b| a.0.cmp(&b.0));
                crates.first()?.1
            }
        };
        let asked =
            hir_ty::ParamEnvAndCrate { param_env: hir_ty::next_solver::ParamEnv::empty(), krate }
                .store();
        Some(super::home::Homes::default().home(self.db, instance, &asked))
    }

    /// `None` for an instance that names anything this has no stable name for.
    pub(super) fn instance(&self, instance: &Instance) -> Option<String> {
        match self.default_environment(instance) {
            Some(environment) => self.instance_in(instance, &environment),
            None => self.raw_instance(instance),
        }
    }

    pub(super) fn instance_in(
        &self,
        instance: &Instance,
        environment: &hir_ty::traits::StoredParamEnvAndCrate,
    ) -> Option<String> {
        let mut name = self.raw_instance(instance)?;
        if let Some(certificate) = self.semantic_certificate(instance, environment)? {
            name.push_str(SEMANTIC_INPUTS);
            let component = self.graph.component(certificate.0);
            let digest = component.digest.get().and_then(Option::as_deref);
            name.push_str(digest.expect("a certificate's closure is canonical"));
        }
        Some(name)
    }

    /// `None` for an instance with no certificate, `Some(None)` for one whose closure depends
    /// on nothing of where it is compiled.
    pub(super) fn semantic_certificate(
        &self,
        instance: &Instance,
        environment: &hir_ty::traits::StoredParamEnvAndCrate,
    ) -> Option<Option<SemanticCertificate>> {
        self.db.unwind_if_revision_cancelled();
        let key = InstanceIn::new(instance.clone(), environment.clone());
        let cached = self.graph.certificate(&key);
        let result = cached.unwrap_or_else(|| {
            let result = self.semantic_closure(&key).ok_or_else(|| self.missed.get());
            self.graph.settle(key, result);
            result
        });
        match result {
            Ok(component) => {
                if !self.graph.component(component).contextual {
                    return Some(None);
                }
                self.seal_component(component)?;
                Some(Some(SemanticCertificate(component)))
            }
            Err(why) if cached.is_some() => self.miss(why),
            Err(_) => None,
        }
    }

    fn semantic_closure(&self, root: &InstanceIn) -> Option<usize> {
        let mut pending = vec![(root.clone(), false)];
        let mut active = FxHashMap::<InstanceDef, usize>::default();
        let mut index = FxHashMap::default();
        let mut keys = Vec::new();
        let mut nodes = Vec::new();
        while let Some((key, exiting)) = pending.pop() {
            self.db.unwind_if_revision_cancelled();
            if exiting {
                *active.get_mut(&key.instance.def).unwrap() -= 1;
                continue;
            }
            if let Some(cached) = self.graph.certificate(&key) {
                if let Err(why) = cached {
                    self.cache_semantic_failure(&key, &pending, why);
                    return self.miss(why);
                }
                continue;
            }
            if index.contains_key(&key) {
                continue;
            }
            let depth = active.entry(key.instance.def).or_default();
            *depth += 1;
            if *depth > crate_def_map(self.db, key.env.krate).recursion_limit() as usize {
                return self.miss("semantic instantiation recursion limit");
            }
            let node = match self.shared_node(&key) {
                Some(node) => node,
                None => {
                    self.cache_semantic_failure(&key, &pending, self.missed.get());
                    return None;
                }
            };
            index.insert(key.clone(), nodes.len());
            pending.push((key.clone(), true));
            pending.extend(node.children.iter().cloned().map(|child| (child, false)));
            keys.push(key);
            nodes.push(node);
        }
        let edges: Vec<Vec<usize>> = nodes
            .iter()
            .map(|node| node.children.iter().filter_map(|key| index.get(key).copied()).collect())
            .collect();
        let components = semantic_components(&edges);
        let mut component_of = vec![0; nodes.len()];
        for (component, members) in components.iter().enumerate() {
            for &member in members {
                component_of[member] = component;
            }
        }
        let mut complete: Vec<Option<usize>> = vec![None; components.len()];
        for (component, members) in components.iter().enumerate().rev() {
            self.db.unwind_if_revision_cancelled();
            let mut constants = Vec::new();
            let mut sources = Vec::new();
            let mut contextual = false;
            let mut dependencies = FxHashSet::default();
            let mut boundaries = FxHashSet::default();
            for &member in members {
                let node = &nodes[member];
                contextual |= node.contextual;
                constants.extend(node.constants.iter().cloned());
                sources.extend(node.sources.iter().cloned());
                for child in &node.children {
                    if let Some(&child) = index.get(child) {
                        let child = component_of[child];
                        if child != component {
                            dependencies.insert(child);
                        }
                    } else {
                        boundaries.insert(child.clone());
                    }
                }
            }
            let mut children = Vec::new();
            for dependency in dependencies {
                let child = complete[dependency].expect("semantic dependencies precede callers");
                contextual |= self.graph.component(child).contextual;
                children.push(child);
            }
            for child in boundaries {
                let cached = self.graph.certificate(&child).expect("discovered semantic boundary");
                let child = match cached {
                    Ok(child) => child,
                    Err(why) => return self.miss(why),
                };
                contextual |= self.graph.component(child).contextual;
                children.push(child);
            }
            constants.sort();
            constants.dedup();
            sources.sort();
            sources.dedup();
            children.sort_unstable();
            children.dedup();
            let certificate = self.graph.add_component(SemanticComponent {
                contextual,
                constants: constants.into_boxed_slice(),
                sources: sources.into_boxed_slice(),
                dependencies: children.into_boxed_slice(),
                digest: std::sync::OnceLock::new(),
            });
            for &member in members {
                self.graph.settle(keys[member].clone(), Ok(certificate));
            }
            let mut walked = self.graph.nodes.write().unwrap_or_else(PoisonError::into_inner);
            for &member in members {
                walked.remove(&keys[member]);
            }
            drop(walked);
            complete[component] = Some(certificate);
        }
        match self.graph.certificate(root)? {
            Ok(certificate) => Some(certificate),
            // Another thread walked to where this one's closure fails first.
            Err(why) => self.miss(why),
        }
    }

    /// The node of `key`, made here unless another thread of the build has made it.
    fn shared_node(&self, key: &InstanceIn) -> Option<Arc<SemanticNode>> {
        let nodes = &self.graph.nodes;
        if let Some(node) = nodes.read().unwrap_or_else(PoisonError::into_inner).get(key) {
            return Some(node.clone());
        }
        let node = Arc::new(self.semantic_node(key)?);
        let mut nodes = nodes.write().unwrap_or_else(PoisonError::into_inner);
        Some(nodes.entry(key.clone()).or_insert(node).clone())
    }

    fn cache_semantic_failure(
        &self,
        failed: &InstanceIn,
        pending: &[(InstanceIn, bool)],
        why: &'static str,
    ) {
        let ancestors = pending.iter().filter(|it| it.1).map(|it| &it.0);
        for key in ancestors.chain([failed]) {
            self.graph.nodes.write().unwrap_or_else(PoisonError::into_inner).remove(key);
            self.graph.settle(key.clone(), Err(why));
        }
    }

    /// Works out the digest of the complete closure of `root`, once for each SCC of this view.
    /// What it reads is borrowed from the arena and gone when this returns: only the digest is
    /// kept. `None` for a closure that is not canonical.
    fn seal_component(&self, root: usize) -> Option<()> {
        let component = self.graph.component(root);
        let digest = component.digest.get_or_init(|| {
            let closure = self.closure_components(root);
            let (constants, sources) = closure_values(&closure);
            super::cached::SemanticInputs::canonical_parts(
                constants.iter().copied(),
                sources.iter().copied(),
            )
            .then(|| {
                super::cached::SemanticInputs::digest_parts(
                    constants.iter().copied(),
                    sources.iter().copied(),
                )
                .into_boxed_str()
            })
        });
        match digest {
            Some(_) => Some(()),
            None => self.miss("ambiguous semantic source provenance"),
        }
    }

    /// The SCCs of `root`'s complete closure. Each is held for as long as the answer is, and
    /// the arena is not: a closure can be big, and other threads are adding to the arena.
    fn closure_components(&self, root: usize) -> FxHashMap<usize, Arc<SemanticComponent>> {
        let mut pending = vec![root];
        let mut seen = FxHashMap::default();
        while let Some(at) = pending.pop() {
            self.db.unwind_if_revision_cancelled();
            if seen.contains_key(&at) {
                continue;
            }
            #[cfg(test)]
            self.semantic_flatten_visits.set(self.semantic_flatten_visits.get() + 1);
            let component = self.graph.component(at);
            pending.extend(&component.dependencies);
            seen.insert(at, component);
        }
        seen
    }

    /// Whether everything `child`'s complete closure observed is in the closure of the parent
    /// of `coverage`. A closure that reaches `child` has it all, so that is asked first, of
    /// the components alone. One that does not can still have it all, by the same names.
    pub(super) fn covers(
        &self,
        coverage: &mut SemanticCoverage,
        child: SemanticCertificate,
    ) -> bool {
        let Some(parent) = coverage.parent else { return false };
        let reached = coverage.reached.get_or_insert_with(|| self.closure_components(parent.0));
        if reached.contains_key(&child.0) {
            return true;
        }
        let (constants, sources) = closure_values(reached);
        self.closure_components(child.0).iter().filter(|(at, _)| !reached.contains_key(at)).all(
            |(_, component)| {
                component.constants.iter().all(|it| constants.binary_search(&&**it).is_ok())
                    && component.sources.iter().all(|it| sources.binary_search(&&**it).is_ok())
            },
        )
    }

    /// The certificate as the owned value the store is given, for the one that gives it: it
    /// is as big as the closure, and it is what must not be kept for each root.
    pub(super) fn complete_inputs(
        &self,
        certificate: SemanticCertificate,
    ) -> super::cached::SemanticInputs {
        let closure = self.closure_components(certificate.0);
        let (constants, sources) = closure_values(&closure);
        super::cached::SemanticInputs::new(
            constants.into_iter().map(str::to_owned).collect(),
            sources.into_iter().cloned().collect(),
        )
    }

    fn semantic_node(&self, key: &InstanceIn) -> Option<SemanticNode> {
        use hir_ty::consteval::inputs::{BodyInput, BodyKind, prepare_constants};
        use hir_ty::db::GeneralConstId;
        #[cfg(test)]
        self.semantic_visits.set(self.semantic_visits.get() + 1);
        let instance = &key.instance;
        let environment = &key.env;
        if !environment.param_env(self.db).clauses().is_empty() {
            return self.miss("semantic preparation with caller bounds");
        }
        let contextual = self
            .default_environment(instance)
            .as_ref()
            .is_some_and(|default| default != environment);
        let mut body = self.raw_instance(instance)?;
        body.push_str(" environment ");
        self.crate_(&mut body, environment.krate)?;
        let mut node = SemanticNode {
            contextual,
            constants: vec![Arc::from(format!("body {body}"))],
            sources: Vec::new(),
            children: Vec::new(),
        };
        let kind = match instance.def {
            InstanceDef::Fn(function) => {
                if !hir_def::signatures::FunctionSignature::of(self.db, function).has_body()
                    || hir_def::signatures::FunctionSignature::is_intrinsic(self.db, function)
                {
                    return Some(node);
                }
                BodyKind::Body(function.into())
            }
            InstanceDef::Closure(id) => BodyKind::Closure(InternedClosureId::from_id(id)),
            InstanceDef::Coroutine(id) => BodyKind::Coroutine(InternedCoroutineId::from_id(id)),
            InstanceDef::CoroutineByMove(id) => {
                BodyKind::CoroutineByMove(InternedCoroutineId::from_id(id))
            }
            _ => return Some(node),
        };
        let input = BodyInput::new(self.db, kind, instance.args.clone(), environment.clone());
        let prepared = prepare_constants(self.db, input);
        if prepared.result.is_err() {
            return self.miss("concrete constant preparation failed");
        }
        for selection in &prepared.selections {
            let mut name = format!("{body} constant ");
            match selection.definition {
                GeneralConstId::ConstId(id) => {
                    let at = id.lookup(self.db).id;
                    self.item(&mut name, id.krate(self.db), at.file_id, at.value.erase())?;
                }
                GeneralConstId::StaticId(id) => {
                    let at = id.lookup(self.db).id;
                    self.item(&mut name, id.krate(self.db), at.file_id, at.value.erase())?;
                }
                GeneralConstId::AnonConstId(id) => {
                    let loc = id.loc(self.db);
                    let (_, map) =
                        hir_def::expr_store::ExpressionStore::with_source_map(self.db, loc.owner);
                    let source = map.expr_syntax(loc.expr).ok()?;
                    self.file(&mut name, source.file_id)?;
                    write!(name, "@{:?}", source.value.syntax_node_ptr().text_range()).ok()?;
                }
            }
            self.args(&mut name, selection.arguments.as_ref())?;
            node.constants.push(Arc::from(name));
        }
        for read in &prepared.source_names {
            node.sources.push(Arc::new(super::cached::SemanticSource {
                body: body.clone(),
                file: self.file_path(read.file)?,
                name: read.name.clone()?,
            }));
        }
        node.contextual |= !node.sources.is_empty();
        let mut homes = super::home::Homes::default();
        for (kind, arguments) in &prepared.callees {
            let def = match kind {
                BodyKind::Body(hir_ty::InferBodyId::DefWithBodyId(
                    hir_def::DefWithBodyId::FunctionId(id),
                )) => InstanceDef::Fn(*id),
                BodyKind::Closure(id) => InstanceDef::Closure(id.as_id()),
                BodyKind::Coroutine(id) => InstanceDef::Coroutine(id.as_id()),
                BodyKind::CoroutineByMove(id) => InstanceDef::CoroutineByMove(id.as_id()),
                _ => return self.miss("semantic callee is not a function body"),
            };
            let callee = Instance { def, args: arguments.clone(), checked: instance.checked };
            let environment = homes.home(self.db, &callee, environment);
            let mut edge = format!("{body} callee {} environment ", self.raw_instance(&callee)?);
            self.crate_(&mut edge, environment.krate)?;
            node.constants.push(Arc::from(edge));
            node.children.push(InstanceIn::new(callee, environment));
        }
        Some(node)
    }

    pub(super) fn semantic_sources(&self) -> super::cached::SemanticSources {
        let mut values = super::cached::SemanticSources::default();
        let mut seen = FxHashSet::default();
        for &krate in base_db::all_crates(self.db).iter() {
            let root =
                self.db.file_source_root(krate.data(self.db).root_file_id).source_root_id(self.db);
            for file in self.db.source_root(root).source_root(self.db).iter() {
                if seen.insert(file)
                    && let Some(path) = self.file_path(file)
                {
                    values.insert(path, hir_ty::mir::location::source_name(self.db, file));
                }
            }
        }
        values
    }

    fn raw_instance(&self, instance: &Instance) -> Option<String> {
        let mut out = String::new();
        match instance.def {
            InstanceDef::Fn(func) => {
                out.push_str("fn ");
                self.function(&mut out, func)?;
            }
            InstanceDef::Closure(id) => {
                out.push_str("closure ");
                self.closure(&mut out, InternedClosureId::from_id(id).loc(self.db))?;
            }
            InstanceDef::Coroutine(id) => {
                out.push_str("coroutine ");
                self.closure(&mut out, InternedCoroutineId::from_id(id).loc(self.db))?;
            }
            InstanceDef::CoroutineByMove(id) => {
                out.push_str("coroutine by move ");
                self.closure(&mut out, InternedCoroutineId::from_id(id).loc(self.db))?;
            }
            InstanceDef::DropGlue => out.push_str("drop"),
            InstanceDef::CallOnceShim => out.push_str("call_once"),
            InstanceDef::VtableShim(func) => {
                out.push_str("vtable shim ");
                self.function(&mut out, func)?;
            }
            InstanceDef::FnPointerShim => out.push_str("fn_pointer"),
            InstanceDef::ReifyShim { receiver } => write!(out, "reify {receiver}").ok()?,
        }
        self.args(&mut out, instance.args.as_ref())?;
        if instance.checked {
            out.push_str(CHECKED);
        }
        Some(out)
    }

    /// A portable type identity for backend metadata. Unlike StoredTy's Hash this
    /// contains no process-local interner key. Long types retain all SHA-256 bits.
    pub(super) fn type_name(&self, ty: Ty<'db>) -> Option<String> {
        let mut name = String::new();
        self.ty(&mut name, ty)?;
        Some(name)
    }

    /// The vtable of a type for a trait object, or the one with no methods.
    pub(super) fn vtable(
        &self,
        ty: Ty<'db>,
        trait_ref: Option<(TraitId, GenericArgs<'db>)>,
    ) -> Option<String> {
        let mut out = String::from("vtable ");
        self.ty(&mut out, ty)?;
        if let Some((trait_, args)) = trait_ref {
            out.push_str(" as ");
            self.trait_(&mut out, trait_)?;
            self.args(&mut out, args)?;
        }
        Some(out)
    }

    /// A static that [`Self::static_named`] finds again.
    pub(super) fn static_(&self, id: StaticId) -> Option<String> {
        let name = self.static_name(id)?;
        self.static_named(&name).is_some().then_some(name)
    }

    pub(super) fn static_named(&self, name: &str) -> Option<StaticId> {
        self.listed(name, |it| it.statics.get(name).copied())
    }

    /// The functions an instance without arguments can be of: one for each crate that the
    /// one build of its crate is here.
    pub(super) fn functions_named(&self, name: &str) -> Vec<FunctionId> {
        let name = without_semantic_inputs(name);
        let name = name.strip_suffix(CHECKED).unwrap_or(name);
        // An instance of a library's function with a type of the checkout is asked about
        // too, and no function without type parameters is named so: its crate is not listed.
        let of_checkout = name.split_once(' ').is_some_and(|it| it.1.starts_with(OF_CHECKOUT));
        if !of_checkout {
            return Vec::new();
        }
        // A function with lifetime parameters alone is one function whatever they are: its
        // instance has them all erased.
        let erased = name.rfind('<').filter(|&at| {
            let args = name[at + 1..].strip_suffix('>');
            args.is_some_and(|it| it.split(',').all(|arg| arg.is_empty() || arg == "'_"))
        });
        let name = match erased {
            Some(at) => std::borrow::Cow::Owned(format!("{}<>", &name[..at])),
            None => std::borrow::Cow::Borrowed(name),
        };
        self.listed(&name, |it| it.functions.get(&*name).cloned()).unwrap_or_default()
    }

    /// What the crate that `name` is of declares: a name starts with the kind of the item
    /// and the crate it is written in.
    fn listed<T>(&self, name: &str, of: impl FnOnce(&Listed) -> Option<T>) -> Option<T> {
        let (_, rest) = name.split_once(' ')?;
        let krate = &rest[..=rest.find(':')?];
        let mut listing = self.listings.0.lock().unwrap_or_else(|e| e.into_inner());
        let Listing { crates, listed } = &mut *listing;
        if !listed.contains_key(krate) {
            // One build of a crate can be several crates here, which differ in what the
            // daemon is told of them and not in what they declare.
            let crates = crates.get_or_insert_with(|| {
                let mut crates = FxHashMap::<String, Vec<Crate>>::default();
                // What is built names only what it depends on, and to name a crate of the
                // checkout is to go through every body of it: not of one that is not built.
                let named: FxHashSet<Crate> =
                    self.owned.iter().flat_map(|it| it.transitive_deps(self.db)).collect();
                let all = base_db::all_crates(self.db);
                for &krate in all.iter().filter(|it| named.contains(it)) {
                    let mut name = String::new();
                    if self.crate_(&mut name, krate).is_some() {
                        crates.entry(name).or_default().push(krate);
                    }
                }
                crates
            });
            let found = self.list(crates.get(krate)?);
            listed.insert(krate.to_owned(), found);
        }
        of(&listed[krate])
    }

    fn list(&self, crates: &[Crate]) -> Listed {
        let mut found = Listed::default();
        // Lowering a body is most of the listing, and the bodies do not wait on each other.
        let lower = |round: &[DefWithBodyId]| {
            self.symbols.in_parallel(round.len(), &|db, _, at| _ = Body::of(db, round[at]));
        };
        declared(self.db, crates, &lower, &mut |declared| match declared {
            Declared::Function(id) => {
                let mut name = String::from("fn ");
                if self.function(&mut name, id).is_some() {
                    name.push_str("<>");
                    found.functions.entry(name).or_default().push(id);
                }
            }
            Declared::Static(id) => {
                if let Some(name) = self.static_name(id) {
                    found.statics.entry(name).or_insert(id);
                }
            }
        });
        found
    }

    /// The name of a static whether or not [`Self::static_named`] finds it again.
    pub(super) fn static_name(&self, id: StaticId) -> Option<String> {
        let mut out = String::from("static ");
        let at = id.lookup(self.db).id;
        self.item(&mut out, id.krate(self.db), at.file_id, at.value.erase())?;
        Some(out)
    }

    /// A name of a static that stays the same across edits of the checkout while the
    /// daemon runs, for [`super::LASTING_STATIC`]: [`Self::static_name`] names a crate of the
    /// checkout by its content and a macro call by its place among the calls of its file,
    /// which an edit above it changes. Not for the store.
    pub(super) fn static_across_edits(&self, id: StaticId) -> Option<String> {
        let krate = id.krate(self.db);
        if !self.of_the_checkout(krate) {
            return self.static_name(id);
        }
        let at = id.lookup(self.db).id;
        let root = krate.data(self.db).root_file_id;
        let mut out = format!("static {OF_CHECKOUT}{root:?}:");
        file_across_edits(self.db, &mut out, at.file_id)?;
        write!(out, "#{}", at.value.erase().into_raw()).ok()?;
        Some(out)
    }

    /// The name of a file as every machine has it: one of the standard library is where it
    /// was built, one that cargo fetched is under [`super::CARGO_HOME`] whoever fetched it, and
    /// one below the checkout or beside it is under [`super::CHECKOUT`]. `None` for a file that
    /// is already named so and is not one of those.
    fn stored(&self, path: String) -> Option<String> {
        if let Some(built) = self.library.as_ref().and_then(|it| it.stored(&path)) {
            return Some(built);
        }
        super::relocate::stored_path(&path, self.cargo_home.as_deref(), self.checkout.as_deref())
    }

    fn miss<T>(&self, why: &'static str) -> Option<T> {
        *self.misses.borrow_mut().entry(why).or_default() += 1;
        self.missed.set(why);
        None
    }

    /// Writes what `spell` writes of `of`, once for each `of`: the second time a type is asked
    /// for, the first answer is copied, and its miss counted again.
    fn memo<K: std::hash::Hash + Eq + Copy>(
        &self,
        out: &mut String,
        memo: &RefCell<FxHashMap<K, Result<Box<str>, &'static str>>>,
        of: K,
        spell: impl FnOnce(&mut String) -> Option<()>,
    ) -> Option<()> {
        if let Some(known) = memo.borrow().get(&of) {
            return match known {
                Ok(it) => {
                    out.push_str(it);
                    Some(())
                }
                Err(why) => self.miss(why),
            };
        }
        let mut spelled = String::new();
        let found = match spell(&mut spelled) {
            Some(()) => Ok(spelled.into_boxed_str()),
            None => Err(self.missed.get()),
        };
        if let Ok(it) = &found {
            out.push_str(it);
        }
        let named = found.as_ref().ok().map(|_| ());
        memo.borrow_mut().insert(of, found);
        named
    }

    /// An item by where it is written: names do not tell two `impl`s apart.
    fn item(
        &self,
        out: &mut String,
        krate: Crate,
        file: HirFileId,
        id: ErasedFileAstId,
    ) -> Option<()> {
        self.crate_(out, krate)?;
        self.ast(out, file, id)
    }

    fn crate_(&self, out: &mut String, krate: Crate) -> Option<()> {
        self.memo(out, &self.crate_names, krate, |out| self.spell_crate(out, krate))
    }

    /// Whether the crate is named by its content: its source changes under the daemon.
    pub(super) fn of_the_checkout(&self, krate: Crate) -> bool {
        self.owned.contains(&krate) || krate.data(self.db).origin.is_local()
    }

    fn spell_crate(&self, out: &mut String, krate: Crate) -> Option<()> {
        // The source of a crate of the workspace changes too, built by the daemon or not.
        if self.of_the_checkout(krate) {
            if !self.by_content {
                return self.miss("of the checkout, in a patch");
            }
            let Some(id) = self.content_id(krate) else {
                return self.miss("no content id");
            };
            write!(out, "{OF_CHECKOUT}{id}:").ok()?;
        } else {
            match self.symbols.stable_crate_id(krate) {
                Some(id) => {
                    use std::hash::{Hash as _, Hasher as _};
                    let flags = &krate.data(self.db).codegen_flags;
                    let id = if flags.is_empty() {
                        id
                    } else {
                        let mut hash = rustc_hash::FxHasher::default();
                        id.hash(&mut hash);
                        flags.hash(&mut hash);
                        hash.finish()
                    };
                    write!(out, "{id:x}:").ok()?;
                }
                None => match self.symbols.content_id(krate) {
                    Some(id) => write!(out, "{OF_LIBRARY}{id}:").ok()?,
                    None => return self.miss("no crate id"),
                },
            }
        }
        // The code of a crate a debugger is told of otherwise than of the build's own is not
        // the code of the crate built as the build is: the store keeps an entry under what is
        // told of the build's own (`later::Keeper`), and what is told of this crate is in the
        // names of its code. A build that tells of every crate alike names them as it did.
        let told = super::told_of(self.symbols, Some(krate));
        if told != super::told_of(self.symbols, None) {
            out.pop();
            write!(out, "{DEBUG}{}:", told as u8).ok()?;
        }
        Some(())
    }

    fn ast(&self, out: &mut String, file: HirFileId, id: ErasedFileAstId) -> Option<()> {
        if let Some(known) = self.files.borrow().get(&file) {
            out.push_str(known.as_deref()?);
        } else {
            let mut name = String::new();
            let named = self.file(&mut name, file);
            let name = named.map(|()| name);
            out.push_str(name.as_deref().unwrap_or_default());
            self.files.borrow_mut().insert(file, name);
            named?;
        }
        write!(out, "#{}", id.into_raw()).ok()
    }

    fn file(&self, out: &mut String, file: HirFileId) -> Option<()> {
        let named = |path: Option<String>| match path {
            Some(path) => self
                .stored(path)
                .or_else(|| self.miss("a file under what stands for cargo's directory")),
            None => self.miss("no source path"),
        };
        file_name(self.db, self.symbols, out, file, &named)
    }

    fn function(&self, out: &mut String, func: FunctionId) -> Option<()> {
        let id = func.lookup(self.db).id;
        self.item(out, func.krate(self.db), id.file_id, id.value.erase())
    }

    fn adt(&self, out: &mut String, adt: AdtId) -> Option<()> {
        let db = self.db;
        let (file, id) = match adt {
            AdtId::StructId(it) => {
                let id = it.lookup(db).id;
                (id.file_id, id.value.erase())
            }
            AdtId::UnionId(it) => {
                let id = it.lookup(db).id;
                (id.file_id, id.value.erase())
            }
            AdtId::EnumId(it) => {
                let id = it.lookup(db).id;
                (id.file_id, id.value.erase())
            }
        };
        self.item(out, adt.krate(db), file, id)
    }

    fn trait_(&self, out: &mut String, trait_: TraitId) -> Option<()> {
        let id = trait_.lookup(self.db).id;
        self.item(out, trait_.krate(self.db), id.file_id, id.value.erase())
    }

    /// A closure or an `async` block by the body it is written in, its place among the
    /// closures there, as it is interned, and what is written of it: an edit of the body
    /// before it leaves its name, and the names of what is generic over it, as they were.
    /// What calls a closure does not say what it takes, so one that takes the place of another
    /// has to be named apart from it. One the body has no source for, as the coroutine of an
    /// `async fn`, is named by the place of its expression in the body.
    fn closure(&self, out: &mut String, closure: InternedClosure<'db>) -> Option<()> {
        let db = self.db;
        let InferBodyId::DefWithBodyId(owner) = closure.owner else {
            return self.miss("closure in an anonymous constant");
        };
        match owner {
            DefWithBodyId::FunctionId(it) => self.function(out, it)?,
            DefWithBodyId::ConstId(it) => {
                let id = it.lookup(db).id;
                self.item(out, it.krate(db), id.file_id, id.value.erase())?
            }
            DefWithBodyId::StaticId(it) => {
                let id = it.lookup(db).id;
                self.item(out, it.krate(db), id.file_id, id.value.erase())?
            }
            DefWithBodyId::VariantId(it) => {
                let id = it.lookup(db).id;
                self.item(out, it.krate(db), id.file_id, id.value.erase())?
            }
        }
        match self.written(owner, closure) {
            Some(written) => write!(out, "@{}:{written}", ClosureKey::nth(db, closure)).ok(),
            None => write!(out, "@x{}", closure.expr.into_raw().into_u32()).ok(),
        }
    }

    /// A hash of the source of a closure, `None` when the body has none for it.
    fn written(&self, owner: DefWithBodyId, closure: InternedClosure<'db>) -> Option<Box<str>> {
        if let Some(written) = self.written.borrow().get(&closure) {
            return written.clone();
        }
        let source = Body::with_source_map(self.db, owner).1.expr_syntax(closure.expr).ok();
        let written = source.map(|source| {
            use sha2::{Digest, Sha256};
            let root = source.file_id.parse_or_expand(self.db);
            let text = source.value.syntax_node_ptr().to_node(&root).text().to_string();
            let digest = Sha256::digest(text.as_bytes());
            digest.iter().map(|byte| format!("{byte:02x}")).collect::<String>().into()
        });
        self.written.borrow_mut().insert(closure, written.clone());
        written
    }

    fn args(&self, out: &mut String, args: GenericArgs<'db>) -> Option<()> {
        out.push('<');
        for arg in args.iter() {
            match arg.kind() {
                GenericArgKind::Lifetime(region) => self.region(out, region)?,
                GenericArgKind::Type(ty) => self.ty(out, ty)?,
                GenericArgKind::Const(konst) => {
                    if let Some(value) = try_const_usize(self.db, konst) {
                        write!(out, "{value}").ok()?
                    } else if let Some(value) = konst.try_to_value()
                        && let Some(leaf) = value.try_to_leaf()
                    {
                        self.ty(out, value.ty)?;
                        write!(out, ":{leaf:?}").ok()?
                    } else {
                        return self.miss("constant argument");
                    }
                }
            }
            out.push(',');
        }
        out.push('>');
        Some(())
    }

    /// Erased, unless a binder in the type binds it.
    fn region(&self, out: &mut String, region: Region<'db>) -> Option<()> {
        match region.kind() {
            RegionKind::ReBound(BoundVarIndexKind::Bound(depth), bound) => {
                write!(out, "'{}.{}", depth.as_u32(), bound.var.as_u32()).ok()
            }
            RegionKind::ReBound(..) => self.miss("bound region"),
            _ => {
                out.push_str("'_");
                Some(())
            }
        }
    }

    fn tys(&self, out: &mut String, tys: impl IntoIterator<Item = Ty<'db>>) -> Option<()> {
        for ty in tys {
            self.ty(out, ty)?;
            out.push(',');
        }
        Some(())
    }

    /// A type written out, or the hash of that when it is longer than [`SPELLED`]: what holds
    /// a type of the checkout says so in its hash too, for [`Self::of_checkout`].
    fn ty(&self, out: &mut String, ty: Ty<'db>) -> Option<()> {
        self.memo(out, &self.tys, ty, |out| {
            self.spell_ty(out, ty)?;
            if out.len() > SPELLED {
                *out = hashed(out);
            }
            Some(())
        })
    }

    fn spell_ty(&self, out: &mut String, ty: Ty<'db>) -> Option<()> {
        let db = self.db;
        match ty.kind() {
            TyKind::Bool => out.push_str("bool"),
            TyKind::Char => out.push_str("char"),
            TyKind::Str => out.push_str("str"),
            TyKind::Never => out.push('!'),
            TyKind::Int(it) => out.push_str(it.name_str()),
            TyKind::Uint(it) => out.push_str(it.name_str()),
            TyKind::Float(it) => out.push_str(it.name_str()),
            TyKind::Ref(region, pointee, mutability) => {
                out.push('&');
                self.region(out, region)?;
                out.push_str(match mutability {
                    Mutability::Not => " ",
                    Mutability::Mut => " mut ",
                });
                self.ty(out, pointee)?;
            }
            TyKind::RawPtr(pointee, mutability) => {
                out.push_str(match mutability {
                    Mutability::Not => "*const ",
                    Mutability::Mut => "*mut ",
                });
                self.ty(out, pointee)?;
            }
            TyKind::Slice(element) => {
                out.push('[');
                self.ty(out, element)?;
                out.push(']');
            }
            TyKind::Array(element, len) => {
                out.push('[');
                self.ty(out, element)?;
                let Some(len) = try_const_usize(db, len) else {
                    return self.miss("array length");
                };
                write!(out, ";{len}]").ok()?;
            }
            TyKind::Tuple(tys) => {
                out.push('(');
                self.tys(out, tys.iter())?;
                out.push(')');
            }
            TyKind::Adt(adt, args) => {
                out.push_str("adt ");
                self.adt(out, adt.def_id())?;
                self.args(out, args)?;
            }
            TyKind::FnDef(def, args) => {
                match def.0 {
                    CallableDefId::FunctionId(it) => {
                        out.push_str("fn ");
                        self.function(out, it)?;
                    }
                    CallableDefId::StructId(it) => {
                        out.push_str("ctor ");
                        self.adt(out, it.into())?;
                    }
                    CallableDefId::EnumVariantId(it) => {
                        out.push_str("ctor ");
                        let id = it.lookup(db).id;
                        self.item(out, it.krate(db), id.file_id, id.value.erase())?;
                    }
                }
                self.args(out, args)?;
            }
            TyKind::FnPtr(signature, header) => {
                write!(
                    out,
                    "fnptr {:?} {} {} (",
                    header.abi(),
                    header.safety().is_safe(),
                    header.c_variadic()
                )
                .ok()?;
                self.tys(out, signature.skip_binder().inputs_and_output.iter())?;
                out.push(')');
            }
            TyKind::Closure(id, args) => {
                out.push_str("closure ");
                self.closure(out, id.0.loc(db))?;
                self.args(out, args)?;
            }
            TyKind::CoroutineClosure(id, args) => {
                out.push_str("async closure ");
                self.closure(out, id.0.loc(db))?;
                self.args(out, args)?;
            }
            TyKind::Coroutine(id, args) => {
                out.push_str("coroutine ");
                self.closure(out, id.0.loc(db))?;
                self.args(out, args)?;
            }
            TyKind::Dynamic(predicates, region) => {
                // In an order of their own: the list is sorted by ids of this run.
                let mut written = Vec::new();
                for predicate in predicates.iter() {
                    let mut out = String::new();
                    match predicate.skip_binder() {
                        ExistentialPredicate::Trait(it) => {
                            out.push_str("trait ");
                            self.trait_(&mut out, it.def_id.0)?;
                            self.args(&mut out, it.args)?;
                        }
                        ExistentialPredicate::Projection(it) => {
                            let TermId::TypeAliasId(alias) = it.def_id.0 else {
                                return self.miss("projection");
                            };
                            out.push_str("type ");
                            let id = alias.lookup(db).id;
                            self.item(&mut out, alias.krate(db), id.file_id, id.value.erase())?;
                            self.args(&mut out, it.args)?;
                            out.push('=');
                            self.ty(&mut out, it.term.as_type()?)?;
                        }
                        ExistentialPredicate::AutoTrait(it) => {
                            out.push_str("auto ");
                            self.trait_(&mut out, it.0)?;
                        }
                    }
                    written.push(out);
                }
                written.sort();
                out.push_str("dyn (");
                for it in written {
                    out.push_str(&it);
                    out.push('+');
                }
                self.region(out, region)?;
                out.push(')');
            }
            // What was left of a projection that no `impl` in reach gives a type to.
            TyKind::Alias(AliasTy { kind: AliasTyKind::Projection { def_id }, args, .. }) => {
                let at = def_id.0.lookup(db).id;
                out.push_str("alias ");
                self.item(out, def_id.0.krate(db), at.file_id, at.value.erase())?;
                self.args(out, args)?;
            }
            // An `impl Trait` nothing has told the type of, as in the drop glue of what holds one.
            TyKind::Alias(AliasTy { kind: AliasTyKind::Opaque { def_id }, args, .. }) => {
                let (at, index) = match def_id.0.loc(db) {
                    ImplTraitId::ReturnTypeImplTrait(of, index) => {
                        out.push_str("opaque fn ");
                        self.function(out, of)?;
                        (None, index)
                    }
                    ImplTraitId::TypeAliasImplTrait(of, index) => {
                        out.push_str("opaque alias ");
                        (Some(of), index)
                    }
                };
                if let Some(of) = at {
                    let at = of.lookup(db).id;
                    self.item(out, of.krate(db), at.file_id, at.value.erase())?;
                }
                write!(out, " {}", index.into_raw().into_u32()).ok()?;
                self.args(out, args)?;
            }
            TyKind::Alias(..) => return self.miss("alias type"),
            TyKind::Param(..) => return self.miss("type parameter"),
            TyKind::CoroutineWitness(..) => return self.miss("coroutine witness"),
            TyKind::Foreign(def_id) => {
                let at = def_id.0.lookup(db).id;
                out.push_str("foreign ");
                self.item(out, def_id.0.krate(db), at.file_id, at.value.erase())?;
            }
            TyKind::Pat(..) => return self.miss("pattern type"),
            _ => return self.miss("other type"),
        }
        Some(())
    }
}

/// A file by its path as `named` has it, or for what a macro wrote by the call that wrote it.
fn file_name(
    db: &dyn HirDatabase,
    symbols: &dyn SymbolResolver,
    out: &mut String,
    file: HirFileId,
    named: &dyn Fn(Option<String>) -> Option<String>,
) -> Option<()> {
    let written_at = |out: &mut String, file: HirFileId, id: ErasedFileAstId| {
        file_name(db, symbols, out, file, named)?;
        write!(out, "#{}", id.into_raw()).ok()
    };
    match file {
        HirFileId::FileId(file) => out.push_str(&named(symbols.source_path(file.file_id(db)))?),
        HirFileId::MacroFile(call) => match &call.loc(db).kind {
            MacroCallKind::FnLike { ast_id, .. } => {
                written_at(out, ast_id.file_id, ast_id.value.erase())?;
                out.push('!');
            }
            MacroCallKind::Derive { ast_id, derive_attr_index, derive_index, .. } => {
                written_at(out, ast_id.file_id, ast_id.value.erase())?;
                write!(out, "!derive {derive_attr_index:?} {derive_index}").ok()?;
            }
            MacroCallKind::Attr { ast_id, censored_attr_ids, .. } => {
                written_at(out, ast_id.file_id, ast_id.value.erase())?;
                write!(out, "!attr {censored_attr_ids:?}").ok()?;
            }
        },
    }
    Some(())
}

/// [`file_name`] for [`StableNames::static_across_edits`]: a file by its id, and a call of a
/// macro written as one (`name!(...)`) by its text, the same wherever the call moves in its
/// file. A derive's or an attribute's call is the item's, which its name stands for.
fn file_across_edits(db: &dyn HirDatabase, out: &mut String, file: HirFileId) -> Option<()> {
    match file {
        HirFileId::FileId(file) => write!(out, "{:?}", file.file_id(db)).ok(),
        HirFileId::MacroFile(call) => {
            let loc = call.loc(db);
            match &loc.kind {
                MacroCallKind::FnLike { ast_id, .. } => {
                    file_across_edits(db, out, ast_id.file_id)?;
                    let text = loc.to_node(db).value.text().to_string();
                    write!(out, "#{:032x}!", super::print_of(&text)).ok()
                }
                MacroCallKind::Derive { ast_id, derive_attr_index, derive_index, .. } => {
                    file_across_edits(db, out, ast_id.file_id)?;
                    let at = ast_id.value.erase().into_raw();
                    write!(out, "#{at}!derive {derive_attr_index:?} {derive_index}").ok()
                }
                MacroCallKind::Attr { ast_id, censored_attr_ids, .. } => {
                    file_across_edits(db, out, ast_id.file_id)?;
                    let at = ast_id.value.erase().into_raw();
                    write!(out, "#{at}!attr {censored_attr_ids:?}").ok()
                }
            }
        }
    }
}

fn hashed(spelled: &str) -> String {
    use sha2::{Digest, Sha256};
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(66);
    out.push(HASHED);
    if spelled.contains(OF_CHECKOUT) {
        out.push(OF_CHECKOUT);
    }
    for byte in Sha256::digest(spelled.as_bytes()) {
        out.push(HEX[usize::from(byte >> 4)] as char);
        out.push(HEX[usize::from(byte & 15)] as char);
    }
    out
}

/// The identity of where a function inside a block is written, which its path does not say:
/// two blocks of one function, or two `const _` of one module, can each hold an item of the same name.
/// `None` for a function outside of any block.
pub fn block_place(
    db: &dyn HirDatabase,
    symbols: &dyn SymbolResolver,
    func: FunctionId,
) -> Option<u64> {
    func.module(db).block(db)?;
    let id = func.lookup(db).id;
    native_place(db, symbols, id.file_id, id.value.erase())
}

/// Where a static inside a block is written, as [`block_place`] has it of a function.
pub fn static_place(
    db: &dyn HirDatabase,
    symbols: &dyn SymbolResolver,
    id: hir_def::StaticId,
) -> Option<u64> {
    id.module(db).block(db)?;
    let id = id.lookup(db).id;
    native_place(db, symbols, id.file_id, id.value.erase())
}

fn native_place(
    db: &dyn HirDatabase,
    symbols: &dyn SymbolResolver,
    file: HirFileId,
    item: ErasedFileAstId,
) -> Option<u64> {
    use sha2::{Digest, Sha256};
    let names = StableNames::new(db, symbols, FxHashSet::default(), false);
    let literal = std::cell::Cell::new(false);
    let mut file_name_text = String::new();
    file_name(db, symbols, &mut file_name_text, file, &|path| {
        let path = path?;
        Some(names.stored(path.clone()).unwrap_or_else(|| {
            // Reserved stored-path spellings can also be real source paths. They
            // remain native inputs even when no portable cache name can represent them.
            literal.set(true);
            path
        }))
    })?;
    let encoded = postcard::to_allocvec(&(
        b"ra-jit native place 1".as_slice(),
        literal.get(),
        file_name_text,
        item.into_raw(),
    ))
    .ok()?;
    let hash = Sha256::digest(encoded);
    Some(u64::from_le_bytes(hash[..8].try_into().expect("a hash is 32 bytes")))
}

/// What a complete closure observed, sorted and without repeats, by reference.
fn closure_values(
    closure: &FxHashMap<usize, Arc<SemanticComponent>>,
) -> (Vec<&str>, Vec<&super::cached::SemanticSource>) {
    let mut constants = Vec::new();
    let mut sources = Vec::new();
    for component in closure.values() {
        constants.extend(component.constants.iter().map(|it| &**it));
        sources.extend(component.sources.iter().map(|it| &**it));
    }
    constants.sort_unstable();
    constants.dedup();
    sources.sort_unstable();
    sources.dedup();
    (constants, sources)
}

/// Strongly connected components in caller-before-dependency order, without recursive DFS.
fn semantic_components(edges: &[Vec<usize>]) -> Vec<Vec<usize>> {
    let mut reverse = vec![Vec::new(); edges.len()];
    for (from, children) in edges.iter().enumerate() {
        for &child in children {
            reverse[child].push(from);
        }
    }
    let mut seen = vec![false; edges.len()];
    let mut finish = Vec::new();
    for root in 0..edges.len() {
        if seen[root] {
            continue;
        }
        seen[root] = true;
        let mut stack = vec![(root, 0)];
        while let Some((node, next)) = stack.last_mut() {
            if let Some(&child) = edges[*node].get(*next) {
                *next += 1;
                if !seen[child] {
                    seen[child] = true;
                    stack.push((child, 0));
                }
            } else {
                let (node, _) = stack.pop().unwrap();
                finish.push(node);
            }
        }
    }
    seen.fill(false);
    let mut components = Vec::new();
    for root in finish.into_iter().rev() {
        if seen[root] {
            continue;
        }
        let mut members = Vec::new();
        let mut stack = vec![root];
        seen[root] = true;
        while let Some(node) = stack.pop() {
            members.push(node);
            for &parent in &reverse[node] {
                if !seen[parent] {
                    seen[parent] = true;
                    stack.push(parent);
                }
            }
        }
        components.push(members);
    }
    components
}

#[cfg(test)]
mod semantic_tests {
    use super::*;
    use ide_db::RootDatabase;
    use rustc_type_ir::inherent::Ty as _;
    use sha2::{Digest, Sha256};
    use test_fixture::WithFixture;

    fn functions(db: &RootDatabase, krate: Crate) -> FxHashMap<String, FunctionId> {
        crate_def_map(db, krate)
            .modules(db)
            .flat_map(|(_, module)| module.scope(db).declarations())
            .filter_map(|item| match item {
                ModuleDefId::FunctionId(id) => Some((
                    hir_def::signatures::FunctionSignature::of(db, id).name.as_str().to_owned(),
                    id,
                )),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn pure_graph_keeps_only_local_records() {
        const N: usize = 1024;
        let mut source = "//- minicore: sized\n//- /src/main.rs crate:pure_graph\n".to_owned();
        for i in 0..N {
            let value = if i + 1 == N { "17".to_owned() } else { format!("f{}::<T>()", i + 1) };
            writeln!(source, "pub fn f{i}<T>() -> usize {{ {value} }}").unwrap();
        }
        let db = RootDatabase::with_files(&source);
        hir::attach_db(&db, || {
            let krate = *base_db::all_crates(&db)
                .iter()
                .find(|krate| krate.data(&db).origin.is_local())
                .unwrap();
            let functions = functions(&db, krate);
            let symbols = crate::startup_tests::Symbols {
                db: &db,
                identity: Sha256::digest(source.as_bytes()).into(),
            };
            let names = StableNames::new(
                &db,
                &symbols,
                base_db::all_crates(&db).iter().copied().collect(),
                true,
            );
            let environment = hir_ty::ParamEnvAndCrate {
                param_env: hir_ty::next_solver::ParamEnv::empty(),
                krate,
            }
            .store();
            let args = GenericArgs::new_from_slice(&[Ty::new_unit(
                hir_ty::next_solver::DbInterner::new_with(&db, krate),
            )
            .into()])
            .store();
            for i in 0..N {
                let instance = Instance {
                    def: InstanceDef::Fn(functions[&format!("f{i}")]),
                    args: args.clone(),
                    checked: false,
                };
                assert!(!has_semantic_inputs(&names.instance_in(&instance, &environment).unwrap()));
            }
            assert_eq!(names.semantic_visits.get(), N);
            assert_eq!(names.semantic_flatten_visits.get(), 0);
            let arena = names.graph.components.read().unwrap();
            assert_eq!(arena.len(), N);
            assert!(arena.iter().all(|component| component.digest.get().is_none()));
            assert_eq!(
                arena.iter().map(|component| component.dependencies.len()).sum::<usize>(),
                N - 1
            );
            assert!(
                arena.iter().map(|component| component.constants.len()).sum::<usize>() <= 3 * N
            );
        });
    }

    #[test]
    fn recursive_bodies_share_one_complete_certificate_and_honor_cancellation() {
        semantically_shared_view();
    }

    fn semantically_shared_view() {
        use salsa::Database;
        let source = "//- minicore: sized\n//- /src/main.rs crate:semantic_cycle\n#[rustc_builtin_macro] macro_rules! file {}\n#[rustc_intrinsic] const unsafe fn size_of_val<T: ?Sized>(value: *const T) -> usize;\nconst WIDTH: usize = unsafe { size_of_val(file!() as *const str) };\npub fn f<T>() -> usize { g::<T>() + WIDTH }\npub fn g<T>() -> usize { f::<T>() + WIDTH }\n";
        let mut db = RootDatabase::with_files(source);
        hir_ty::mir::location::name_sources(&mut db, |_, _| Some("cycle.rs".into()));
        hir::attach_db(&db, || {
            let krate = *base_db::all_crates(&db)
                .iter()
                .find(|krate| krate.data(&db).origin.is_local())
                .unwrap();
            let functions = functions(&db, krate);
            let symbols = crate::startup_tests::Symbols {
                db: &db,
                identity: Sha256::digest(source.as_bytes()).into(),
            };
            let names = StableNames::new(
                &db,
                &symbols,
                base_db::all_crates(&db).iter().copied().collect(),
                true,
            );
            let environment = hir_ty::ParamEnvAndCrate {
                param_env: hir_ty::next_solver::ParamEnv::empty(),
                krate,
            }
            .store();
            let args = GenericArgs::new_from_slice(&[Ty::new_unit(
                hir_ty::next_solver::DbInterner::new_with(&db, krate),
            )
            .into()])
            .store();
            let f = Instance {
                def: InstanceDef::Fn(functions["f"]),
                args: args.clone(),
                checked: false,
            };
            let g = Instance { def: InstanceDef::Fn(functions["g"]), args, checked: false };
            let a = names.semantic_certificate(&f, &environment).unwrap().unwrap();
            let b = names.semantic_certificate(&g, &environment).unwrap().unwrap();
            assert_eq!(a, b);
            assert_eq!(names.semantic_visits.get(), 2);
            assert_eq!(names.semantic_flatten_visits.get(), 1);
            assert_ne!(names.instance_in(&f, &environment), names.instance_in(&g, &environment));
            let cancelled = salsa::Cancelled::catch(std::panic::AssertUnwindSafe(|| {
                db.cancellation_token().cancel();
                names.semantic_certificate(&f, &environment)
            }));
            assert!(matches!(cancelled, Err(salsa::Cancelled::Local)), "{cancelled:?}");
            // What walks the graph is cancelled as what builds it is.
            let cancelled = salsa::Cancelled::catch(std::panic::AssertUnwindSafe(|| {
                names.complete_inputs(a);
            }));
            assert!(matches!(cancelled, Err(salsa::Cancelled::Local)), "{cancelled:?}");
            let cancelled = salsa::Cancelled::catch(std::panic::AssertUnwindSafe(|| {
                names.covers(&mut SemanticCoverage::of(Some(a)), b)
            }));
            assert!(matches!(cancelled, Err(salsa::Cancelled::Local)), "{cancelled:?}");
        });
    }

    #[test]
    fn naming_views_retract_failed_and_changed_source_observations() {
        let source = "//- minicore: sized\n//- /src/main.rs crate:semantic_views\n#[rustc_builtin_macro] macro_rules! file {}\n#[rustc_intrinsic] const unsafe fn size_of_val<T: ?Sized>(value: *const T) -> usize;\nconst WIDTH: usize = unsafe { size_of_val(file!() as *const str) };\npub fn touched<T>() -> usize { WIDTH }\npub fn pure<T>() -> usize { 17 }\n";
        let mut db = RootDatabase::with_files(source);
        let mut results = Vec::new();
        for provider in [Some("first.rs"), Some("other.rs"), None, Some("first.rs")] {
            hir_ty::mir::location::name_sources(&mut db, move |_, _| provider.map(Into::into));
            results.push(hir::attach_db(&db, || {
                let krate = *base_db::all_crates(&db)
                    .iter()
                    .find(|krate| krate.data(&db).origin.is_local())
                    .unwrap();
                let functions = functions(&db, krate);
                let symbols = crate::startup_tests::Symbols {
                    db: &db,
                    identity: Sha256::digest(source.as_bytes()).into(),
                };
                let names = StableNames::new(
                    &db,
                    &symbols,
                    base_db::all_crates(&db).iter().copied().collect(),
                    true,
                );
                let environment = hir_ty::ParamEnvAndCrate {
                    param_env: hir_ty::next_solver::ParamEnv::empty(),
                    krate,
                }
                .store();
                let args = GenericArgs::new_from_slice(&[Ty::new_unit(
                    hir_ty::next_solver::DbInterner::new_with(&db, krate),
                )
                .into()])
                .store();
                let touched = Instance {
                    def: InstanceDef::Fn(functions["touched"]),
                    args: args.clone(),
                    checked: false,
                };
                let pure =
                    Instance { def: InstanceDef::Fn(functions["pure"]), args, checked: false };
                let first = names.instance_in(&touched, &environment);
                let visits = names.semantic_visits.get();
                assert_eq!(first, names.instance_in(&touched, &environment));
                assert_eq!(names.semantic_visits.get(), visits);
                let pure = names.instance_in(&pure, &environment).unwrap();
                assert!(!has_semantic_inputs(&pure));
                (first, pure)
            }));
        }
        assert!(results[0].0.is_some());
        assert_ne!(results[0].0, results[1].0);
        assert!(results[2].0.is_none());
        assert_eq!(results[0], results[3]);
        assert!(results.iter().all(|result| result.1 == results[0].1));
    }

    #[test]
    fn shared_semantic_graph_is_rendered_once_per_naming_view() {
        const N: usize = 128;
        let mut source = "//- minicore: sized\n//- /src/main.rs crate:semantic_graph\n#[rustc_builtin_macro] macro_rules! file {}\n#[rustc_intrinsic] const unsafe fn size_of_val<T: ?Sized>(value: *const T) -> usize;\nconst WIDTH: usize = unsafe { size_of_val(file!() as *const str) };\n".to_owned();
        for i in 0..N {
            let value = if i + 1 == N { "WIDTH".to_owned() } else { format!("f{}::<T>()", i + 1) };
            writeln!(source, "pub fn f{i}<T>() -> usize {{ {value} }}").unwrap();
        }
        let mut db = RootDatabase::with_files(&source);
        hir_ty::mir::location::name_sources(&mut db, |_, _| Some("graph.rs".into()));
        hir::attach_db(&db, || {
            let krate = *base_db::all_crates(&db)
                .iter()
                .find(|krate| krate.data(&db).origin.is_local())
                .unwrap();
            let mut functions: Vec<_> = crate_def_map(&db, krate)
                .modules(&db)
                .flat_map(|(_, module)| module.scope(&db).declarations())
                .filter_map(|item| match item {
                    ModuleDefId::FunctionId(id) => {
                        hir_def::signatures::FunctionSignature::of(&db, id)
                            .name
                            .as_str()
                            .strip_prefix('f')
                            .and_then(|name| name.parse::<usize>().ok())
                            .map(|index| (index, id))
                    }
                    _ => None,
                })
                .collect();
            functions.sort_by_key(|item| item.0);
            assert_eq!(functions.len(), N);
            let symbols = crate::startup_tests::Symbols {
                db: &db,
                identity: Sha256::digest(source.as_bytes()).into(),
            };
            let names = StableNames::new(
                &db,
                &symbols,
                base_db::all_crates(&db).iter().copied().collect(),
                true,
            );
            let environment = hir_ty::ParamEnvAndCrate {
                param_env: hir_ty::next_solver::ParamEnv::empty(),
                krate,
            }
            .store();
            let interner = hir_ty::next_solver::DbInterner::new_with(&db, krate);
            let args = GenericArgs::new_from_slice(&[Ty::new_unit(interner).into()]).store();
            let began = std::time::Instant::now();
            let instances: Vec<_> = functions
                .into_iter()
                .map(|(_, function)| Instance {
                    def: InstanceDef::Fn(function),
                    args: args.clone(),
                    checked: false,
                })
                .collect();
            let first: Vec<_> = instances
                .iter()
                .map(|instance| names.instance_in(instance, &environment).unwrap())
                .collect();
            let flattened = names.semantic_flatten_visits.get();
            let second: Vec<_> = instances
                .iter()
                .map(|instance| names.instance_in(instance, &environment).unwrap())
                .collect();
            assert_eq!(first, second);
            assert_eq!(
                names.semantic_flatten_visits.get(),
                flattened,
                "repeated names must reuse canonical certificates"
            );
            eprintln!(
                "semantic graph roots={N} visits={} flatten={} elapsed={:?} first={} last={}",
                names.semantic_visits.get(),
                names.semantic_flatten_visits.get(),
                began.elapsed(),
                first[0],
                first[N - 1]
            );
            assert!(
                names.semantic_visits.get() <= N,
                "shared closure rendering must not revisit every descendant for every name: {}",
                names.semantic_visits.get()
            );
        });
    }

    /// `N` roots of a call graph where each calls the next two, so that every root's complete
    /// closure is every root after it, and only the last reads a file name. The same source
    /// as a real generic call chain: nothing here is a mock of a certificate.
    fn shared_call_graph(n: usize) -> String {
        let mut source = "//- minicore: sized\n//- /src/main.rs crate:shared_graph\n#[rustc_builtin_macro] macro_rules! file {}\n#[rustc_intrinsic] const unsafe fn size_of_val<T: ?Sized>(value: *const T) -> usize;\nconst WIDTH: usize = unsafe { size_of_val(file!() as *const str) };\n".to_owned();
        for i in 0..n {
            let value = match n - i {
                1 => "WIDTH".to_owned(),
                2 => format!("f{}::<T>()", i + 1),
                _ => format!("f{}::<T>() + f{}::<T>()", i + 1, i + 2),
            };
            writeln!(source, "pub fn f{i}<T>() -> usize {{ {value} }}").unwrap();
        }
        source
    }

    /// `body` with a fresh naming view of `source` where every file is named `provider`, and
    /// the instance of a function by its name with `()` for its one type parameter.
    fn in_view<R>(
        source: &str,
        provider: Option<&'static str>,
        body: impl FnOnce(
            &dyn Fn(&mut dyn FnMut(&StableNames<'_, '_>)),
            &dyn Fn(&str) -> Instance,
            &hir_ty::traits::StoredParamEnvAndCrate,
        ) -> R,
    ) -> R {
        let mut db = RootDatabase::with_files(source);
        hir_ty::mir::location::name_sources(&mut db, move |_, _| provider.map(Into::into));
        hir::attach_db(&db, || {
            let krate = *base_db::all_crates(&db)
                .iter()
                .find(|krate| krate.data(&db).origin.is_local())
                .unwrap();
            let functions = functions(&db, krate);
            let symbols = crate::startup_tests::Symbols {
                db: &db,
                identity: Sha256::digest(source.as_bytes()).into(),
            };
            let view = |body: &mut dyn FnMut(&StableNames<'_, '_>)| {
                let owned = base_db::all_crates(&db).iter().copied().collect();
                body(&StableNames::new(&db, &symbols, owned, true));
            };
            let environment = hir_ty::ParamEnvAndCrate {
                param_env: hir_ty::next_solver::ParamEnv::empty(),
                krate,
            }
            .store();
            let args = GenericArgs::new_from_slice(&[Ty::new_unit(
                hir_ty::next_solver::DbInterner::new_with(&db, krate),
            )
            .into()])
            .store();
            let instance = |name: &str| Instance {
                def: InstanceDef::Fn(functions[name]),
                args: args.clone(),
                checked: false,
            };
            body(&view, &instance, &environment)
        })
    }

    /// What a view of `n` roots holds once each of them is named, past what the database keeps.
    fn view_bytes(n: usize) -> isize {
        in_view(&shared_call_graph(n), Some("graph.rs"), |view, instance, environment| {
            let roots: Vec<_> = (0..n).map(|i| instance(&format!("f{i}"))).collect();
            let name_all = |names: &StableNames<'_, '_>| {
                for root in &roots {
                    names.instance_in(root, environment).unwrap();
                }
            };
            // The queries and the interners of the first view stay in the database.
            view(&mut |names| name_all(names));
            let mut held = 0;
            let before = ra_jit_fuzz::live_bytes();
            view(&mut |names| {
                name_all(names);
                held = ra_jit_fuzz::live_bytes() - before;
            });
            held
        })
    }

    #[test]
    fn many_roots_of_a_shared_call_graph_do_not_each_retain_its_closure() {
        // Four times the roots hold four times the components. A view that kept each root's
        // flattened closure holds sixteen times what it did, as the failed bootstrap's 3.9 GiB
        // of certificates beside 458 MiB of components did.
        let (small, large) = (view_bytes(128), view_bytes(512));
        eprintln!("view of 128 roots holds {small} bytes, of 512 roots {large} bytes");
        assert!(large < 6 * small, "{large} bytes of 512 roots, {small} of 128");
    }

    /// A root's digest and the sealed bytes of its complete certificate, from the view's value.
    fn sealed(
        names: &StableNames<'_, '_>,
        instance: &Instance,
        environment: &hir_ty::traits::StoredParamEnvAndCrate,
    ) -> (String, String, usize) {
        let certificate = names.semantic_certificate(instance, environment).unwrap().unwrap();
        let inputs = names.complete_inputs(certificate);
        let wire = postcard::to_allocvec(&Some(&inputs)).unwrap();
        let name = names.instance_in(instance, environment).unwrap();
        let digest = semantic_digest(&name).unwrap().to_owned();
        // What a reader works out of the bytes is what the name was sealed with.
        let read: Option<crate::cached::SemanticInputs> = postcard::from_bytes(&wire).unwrap();
        let read = read.unwrap();
        assert_eq!(read.digest(), digest);
        assert!(read.canonical());
        (digest, hex(&Sha256::digest(&wire)), wire.len())
    }

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|byte| format!("{byte:02x}")).collect()
    }

    /// Goldens taken from the flattening implementation these replaced, at `inputs4`.
    #[test]
    fn digests_and_wire_bytes_are_those_of_the_flattened_certificate() {
        const CYCLE: &str = "//- minicore: sized\n//- /src/main.rs crate:semantic_cycle\n#[rustc_builtin_macro] macro_rules! file {}\n#[rustc_intrinsic] const unsafe fn size_of_val<T: ?Sized>(value: *const T) -> usize;\nconst WIDTH: usize = unsafe { size_of_val(file!() as *const str) };\npub fn f<T>() -> usize { g::<T>() + WIDTH }\npub fn g<T>() -> usize { f::<T>() + WIDTH }\n";
        let graph = shared_call_graph(32);
        let mut got = Vec::new();
        for (source, provider, roots) in [
            (graph.as_str(), "graph.rs", &["f0", "f7", "f30", "f31"][..]),
            (graph.as_str(), "other.rs", &["f0", "f31"][..]),
            (CYCLE, "cycle.rs", &["f", "g"][..]),
        ] {
            in_view(source, Some(provider), |view, instance, environment| {
                view(&mut |names| {
                    for root in roots {
                        let (digest, wire, len) = sealed(names, &instance(root), environment);
                        got.push(format!("{provider} {root} {digest} {wire} {len}"));
                    }
                });
            });
        }
        let got = got.join("\n");
        eprintln!("{got}");
        assert_eq!(got, GOLDEN.trim());
    }

    const GOLDEN: &str = "\
graph.rs f0 3596b3b126f309c7501c0cbe1310ad1df2cb7d6bfee008635b1118b0a5d4ba6d ef56cc8fbd83d03824166abc99983fcf53e87845e2c2b54f9416d6c73cf24bc0 28262\n\
graph.rs f7 d383350c824f5c6f8ee4800601735d1f8b3fd29d7ad63e18bc611468f77949e6 e384387e820177af158eb89419a1208416e74ba1f4c5a96bfb3fc0d2dce4847a 21948\n\
graph.rs f30 54ea71c3c8e92b2354c4dff09e47a82fc15e57f2c5428872a465300627b27cf9 3236028237396798672714cefd68732499d0ba173db3f2da6d53557729b68ead 1202\n\
graph.rs f31 f2f5505cdce27d39f56e4cebb65439ef02b718e2e8e9a1b544d853d99f961118 48e4461ded267ebb7bcb70e5124e1a5d0c66873645884c22885ef28dfbcb4268 660\n\
other.rs f0 b243ea88cc56aedc9186b4fb920ae4f67e5272b4adfa08754da870acd863212b 59117c203445c127e8ac3ed40d5f08ee75ef56a62dd5a093ad86a2e20788c39a 28262\n\
other.rs f31 d580bb8eef8bc1f3ff2e725aaef336d3b62b034763a0f75a9fde2742c5642c20 036cc60a1475a9aaa0719aaab2321b09d994d6fed104c15264e6bba73c37064a 660\n\
cycle.rs f 621069e58ee8a80a483aa7ea81b552696e20aa21240c699398e6db242df013e9 036b8a7dce7bc80ed75fb49e4a91348d3670e07b14161307927280283574069f 2037\n\
cycle.rs g 621069e58ee8a80a483aa7ea81b552696e20aa21240c699398e6db242df013e9 036b8a7dce7bc80ed75fb49e4a91348d3670e07b14161307927280283574069f 2037\n\
";

    /// Every answer of the graph is the answer of the flattened sets.
    #[test]
    fn coverage_by_graph_is_coverage_of_the_complete_values() {
        const N: usize = 16;
        in_view(&shared_call_graph(N), Some("graph.rs"), |view, instance, environment| {
            view(&mut |names| {
                let certificates: Vec<_> = (0..N)
                    .map(|i| {
                        names
                            .semantic_certificate(&instance(&format!("f{i}")), environment)
                            .unwrap()
                            .unwrap()
                    })
                    .collect();
                let values: Vec<_> =
                    certificates.iter().map(|it| names.complete_inputs(*it)).collect();
                for (i, &parent) in certificates.iter().enumerate() {
                    let mut coverage = SemanticCoverage::of(Some(parent));
                    for (j, &child) in certificates.iter().enumerate() {
                        let flat = values[i].covers(&values[j]);
                        assert_eq!(names.covers(&mut coverage, child), flat, "{i} {j}");
                        assert_eq!(flat, i <= j, "{i} {j}");
                    }
                }
                assert!(!names.covers(&mut SemanticCoverage::of(None), certificates[0]));
            });
        });
    }

    /// A closure that does not reach a component can still hold what it observed, and a child
    /// with one more constant or one other source name is not covered.
    #[test]
    fn coverage_of_a_component_not_reached_is_by_its_values() {
        fn source(file: &str, name: &str) -> crate::cached::SemanticSource {
            crate::cached::SemanticSource {
                body: "body".into(),
                file: file.into(),
                name: name.into(),
            }
        }
        in_view(&shared_call_graph(1), Some("graph.rs"), |view, _, _| {
            view(&mut |names| {
                let add = |constants: &[&str], sources: Vec<_>, dependencies: &[usize]| {
                    SemanticCertificate(names.graph.add_component(SemanticComponent {
                        contextual: true,
                        constants: constants.iter().map(|it| Arc::from(*it)).collect(),
                        sources: sources.into_iter().map(Arc::new).collect(),
                        dependencies: dependencies.into(),
                        digest: Default::default(),
                    }))
                };
                let leaf = add(&["a", "b"], vec![source("f.rs", "/f.rs")], &[]);
                let parent = add(&["c"], Vec::new(), &[leaf.0]);
                let same = add(&["a", "c"], vec![source("f.rs", "/f.rs")], &[]);
                let more = add(&["a", "z"], Vec::new(), &[]);
                let other = add(&["a"], vec![source("f.rs", "/other.rs")], &[]);
                let deep = add(&["b"], Vec::new(), &[more.0]);
                let mut coverage = SemanticCoverage::of(Some(parent));
                assert!(names.covers(&mut coverage, leaf));
                assert!(names.covers(&mut coverage, same));
                assert!(!names.covers(&mut coverage, more));
                assert!(!names.covers(&mut coverage, other));
                assert!(!names.covers(&mut coverage, deep), "what it depends on is asked too");
            });
        });
    }

    /// Two names for one file are refused however deep they are, and by the digest's guard
    /// and no other: a root over the ambiguity has no certificate.
    #[test]
    fn an_ambiguous_closure_has_no_digest() {
        in_view(&shared_call_graph(1), Some("graph.rs"), |view, _, _| {
            view(&mut |names| {
                let source = |name: &str| crate::cached::SemanticSource {
                    body: "body".into(),
                    file: "f.rs".into(),
                    name: name.into(),
                };
                for (constant, name, dependencies) in
                    [("a", "/one.rs", &[][..]), ("b", "/two.rs", &[0][..]), ("c", "/one.rs", &[])]
                {
                    names.graph.add_component(SemanticComponent {
                        contextual: true,
                        constants: Box::new([Arc::from(constant)]),
                        sources: Box::new([Arc::new(source(name))]),
                        dependencies: dependencies.into(),
                        digest: Default::default(),
                    });
                }
                assert!(names.seal_component(2).is_some());
                assert!(names.seal_component(0).is_some());
                assert!(names.seal_component(1).is_none());
            });
        });
    }

    /// `body` with what makes the views of one build over `shared_call_graph(n)`, and its roots.
    fn in_build<R>(
        n: usize,
        provider: Option<&'static str>,
        body: impl FnOnce(
            &RootDatabase,
            &[Instance],
            &hir_ty::traits::StoredParamEnvAndCrate,
            [u8; 32],
        ) -> R,
    ) -> R {
        let source = shared_call_graph(n);
        let mut db = RootDatabase::with_files(&source);
        hir_ty::mir::location::name_sources(&mut db, move |_, _| provider.map(Into::into));
        hir::attach_db(&db, || {
            let krate = *base_db::all_crates(&db)
                .iter()
                .find(|krate| krate.data(&db).origin.is_local())
                .unwrap();
            let functions = functions(&db, krate);
            let environment = hir_ty::ParamEnvAndCrate {
                param_env: hir_ty::next_solver::ParamEnv::empty(),
                krate,
            }
            .store();
            let args = GenericArgs::new_from_slice(&[Ty::new_unit(
                hir_ty::next_solver::DbInterner::new_with(&db, krate),
            )
            .into()])
            .store();
            let roots: Vec<_> = (0..n)
                .map(|i| Instance {
                    def: InstanceDef::Fn(functions[&format!("f{i}")]),
                    args: args.clone(),
                    checked: false,
                })
                .collect();
            body(&db, &roots, &environment, Sha256::digest(source.as_bytes()).into())
        })
    }

    fn name_all(
        names: &StableNames<'_, '_>,
        roots: &[Instance],
        environment: &hir_ty::traits::StoredParamEnvAndCrate,
    ) -> Vec<Option<String>> {
        roots.iter().map(|root| names.instance_in(root, environment)).collect()
    }

    /// Every thread of a build names with a view of its own. Walking the graph once is what
    /// keeps twenty threads from holding twenty copies of a whole program's observations, as
    /// the bootstrap's roots did when they passed 8 GiB with one function compiled.
    #[test]
    fn the_views_of_a_build_walk_one_graph_and_name_as_a_view_alone_does() {
        const N: usize = 64;
        in_build(N, Some("graph.rs"), |db, roots, environment, identity| {
            let symbols = crate::startup_tests::Symbols { db, identity };
            let owned: FxHashSet<Crate> = base_db::all_crates(db).iter().copied().collect();
            let new = || StableNames::new(db, &symbols, owned.clone(), true);
            // The control: a view alone makes every node, as each of two views does.
            let alone = new();
            let expected = name_all(&alone, roots, environment);
            assert!(expected.iter().all(|it| it.as_deref().is_some_and(has_semantic_inputs)));
            assert_eq!(alone.semantic_visits.get(), N);
            let other = new();
            assert_eq!(name_all(&other, roots, environment), expected);
            assert_eq!(other.semantic_visits.get(), N);
            // Together: the first makes the graph, and the others make nothing of it.
            let first = new();
            let graph = first.semantic_graph();
            assert_eq!(name_all(&first, roots, environment), expected);
            let components = graph.components.read().unwrap().len();
            for _ in 0..4 {
                let view = new().sharing(graph.clone());
                assert!(Arc::ptr_eq(&view.graph, &graph));
                assert_eq!(name_all(&view, roots, environment), expected);
                assert_eq!(view.semantic_visits.get(), 0, "a node made again");
                assert_eq!(view.semantic_flatten_visits.get(), 0, "a digest worked out again");
                assert_eq!(graph.components.read().unwrap().len(), components);
                // The bytes for the store are the same from any view of the graph.
                assert_eq!(
                    sealed(&view, &roots[0], environment),
                    sealed(&alone, &roots[0], environment)
                );
                let mut coverage = SemanticCoverage::of(
                    view.semantic_certificate(&roots[0], environment).unwrap(),
                );
                let child = first.semantic_certificate(&roots[N - 1], environment).unwrap();
                assert!(view.covers(&mut coverage, child.unwrap()));
            }
            assert!(graph.nodes.read().unwrap().is_empty(), "nodes are held by components alone");
        });
    }

    /// A node says how its names are spelled: a view that spells otherwise must not read it.
    #[test]
    fn only_a_view_spelled_alike_joins_a_graph() {
        in_build(4, Some("graph.rs"), |db, roots, environment, identity| {
            let symbols = crate::startup_tests::Symbols { db, identity };
            let owned: FxHashSet<Crate> = base_db::all_crates(db).iter().copied().collect();
            let new =
                |owned: FxHashSet<Crate>, content| StableNames::new(db, &symbols, owned, content);
            let first = new(owned.clone(), true);
            let graph = first.semantic_graph();
            name_all(&first, roots, environment);
            let frozen =
                FxHashMap::from_iter(owned.iter().map(|&krate| (krate, crate::ContentId([7; 32]))));
            for view in [
                new(owned.clone(), false).sharing(graph.clone()),
                new(FxHashSet::default(), true).sharing(graph.clone()),
                new(owned.clone(), true).frozen(frozen).sharing(graph.clone()),
            ] {
                assert!(!Arc::ptr_eq(&view.graph, &graph));
            }
            let alike = new(owned.clone(), true).sharing(graph.clone());
            assert!(Arc::ptr_eq(&alike.graph, &graph));
            // Frozen by the same ids, the graph is for those who are frozen by them.
            let ids =
                FxHashMap::from_iter(owned.iter().map(|&krate| (krate, crate::ContentId([9; 32]))));
            let frozen = new(owned.clone(), true).frozen(ids.clone());
            let joined = new(owned.clone(), true).frozen(ids).sharing(frozen.semantic_graph());
            assert!(Arc::ptr_eq(&joined.graph, &frozen.graph));
        });
    }

    /// What one view finds has no certificate, the others of the build are told, without
    /// walking to it, and are still told why.
    #[test]
    fn a_failure_found_by_one_view_is_a_failure_for_the_views_it_shares_with() {
        in_build(8, None, |db, roots, environment, identity| {
            let symbols = crate::startup_tests::Symbols { db, identity };
            let owned: FxHashSet<Crate> = base_db::all_crates(db).iter().copied().collect();
            let alone = StableNames::new(db, &symbols, owned.clone(), true);
            let expected = name_all(&alone, roots, environment);
            assert!(expected.iter().all(Option::is_none));
            let first = StableNames::new(db, &symbols, owned.clone(), true);
            let graph = first.semantic_graph();
            assert_eq!(name_all(&first, roots, environment), expected);
            let view = StableNames::new(db, &symbols, owned, true).sharing(graph.clone());
            assert_eq!(name_all(&view, roots, environment), expected);
            assert_eq!(view.semantic_visits.get(), 0);
            assert!(!view.misses.borrow().is_empty(), "a view is told why");
            assert!(graph.nodes.read().unwrap().is_empty(), "what failed is not held");
        });
    }

    /// Threads racing over the roots in different orders name each as a view alone does: a
    /// digest is of what a closure holds, however the threads cut it into components.
    #[test]
    fn threads_racing_over_one_graph_name_as_a_view_alone_does() {
        const N: usize = 96;
        const THREADS: usize = 8;
        for provider in [Some("graph.rs"), None] {
            in_build(N, provider, |db, roots, environment, identity| {
                let owned: FxHashSet<Crate> = base_db::all_crates(db).iter().copied().collect();
                let symbols = crate::startup_tests::Symbols { db, identity };
                let alone = StableNames::new(db, &symbols, owned.clone(), true);
                let expected = name_all(&alone, roots, environment);
                let first = StableNames::new(db, &symbols, owned.clone(), true);
                let graph = first.semantic_graph();
                let start = std::sync::Barrier::new(THREADS);
                // Not a `solving::Handle`, which is of the crate above and can't reach a view.
                // What it is for is a thread that outlives its clone and keeps its solver
                // cache; these end with this scope, and nothing here collects types.
                let root = db;
                std::thread::scope(|scope| {
                    for thread in 0..THREADS {
                        let (db, owned, graph) = (root.clone(), owned.clone(), graph.clone());
                        let (start, expected) = (&start, &expected);
                        scope.spawn(move || {
                            hir::attach_db(&db, || {
                                let symbols = crate::startup_tests::Symbols { db: &db, identity };
                                let view =
                                    StableNames::new(&db, &symbols, owned, true).sharing(graph);
                                start.wait();
                                // From the leaves, from the middle and from the top.
                                let order: Vec<usize> =
                                    (0..N).map(|i| (i + thread * N / THREADS) % N).collect();
                                for at in order {
                                    assert_eq!(
                                        view.instance_in(&roots[at], environment),
                                        expected[at],
                                        "root {at} on thread {thread}"
                                    );
                                }
                            });
                        });
                    }
                });
                assert_eq!(name_all(&first, roots, environment), expected);
                assert!(graph.nodes.read().unwrap().is_empty());
            });
        }
    }

    /// What the views of a build hold past the database, once each has named every root.
    fn views_bytes(n: usize, views: usize, shared: bool) -> isize {
        in_build(n, Some("graph.rs"), |db, roots, environment, identity| {
            let symbols = crate::startup_tests::Symbols { db, identity };
            let owned: FxHashSet<Crate> = base_db::all_crates(db).iter().copied().collect();
            let new = || StableNames::new(db, &symbols, owned.clone(), true);
            // The queries and the interners of the first view stay in the database.
            name_all(&new(), roots, environment);
            let before = ra_jit_fuzz::live_bytes();
            let graph = new().semantic_graph();
            let held: Vec<_> = (0..views)
                .map(|_| {
                    let view = if shared { new().sharing(graph.clone()) } else { new() };
                    name_all(&view, roots, environment);
                    view
                })
                .collect();
            let bytes = ra_jit_fuzz::live_bytes() - before;
            drop(held);
            bytes
        })
    }

    /// The bootstrap's bound was passed by what each thread held of the same graph.
    #[test]
    fn a_graph_held_by_many_views_is_held_once() {
        let one = views_bytes(256, 1, true);
        let (shared, apart) = (views_bytes(256, 8, true), views_bytes(256, 8, false));
        eprintln!("one view holds {one} bytes, eight together {shared}, eight apart {apart}");
        assert!(apart > 6 * one, "the control: {apart} bytes apart, {one} of one view");
        assert!(shared < 3 * one, "{shared} bytes of eight together, {one} of one view");
    }

    /// A fresh view after the provider changed or lost a file sees that in the complete value,
    /// through the closure's deepest body: nothing of the graph outlives its view.
    #[test]
    fn a_fresh_view_seals_what_the_provider_says_now() {
        const N: usize = 8;
        let graph = shared_call_graph(N);
        let seal = |provider| {
            in_view(&graph, provider, |view, instance, environment| {
                let mut sealed = None;
                view(&mut |names| {
                    sealed = names.instance_in(&instance("f0"), environment).map(|name| {
                        let certificate = names
                            .semantic_certificate(&instance("f0"), environment)
                            .unwrap()
                            .unwrap();
                        (name, names.complete_inputs(certificate))
                    });
                });
                sealed
            })
        };
        let (first, first_inputs) = seal(Some("first.rs")).unwrap();
        let (other, other_inputs) = seal(Some("other.rs")).unwrap();
        assert_ne!(first, other);
        assert_ne!(first_inputs.digest(), other_inputs.digest());
        assert_eq!(semantic_digest(&first), Some(first_inputs.digest()));
        assert_eq!(semantic_digest(&other), Some(other_inputs.digest()));
        assert!(seal(None).is_none(), "a file with no name now has no certificate");
        let (again, _) = seal(Some("first.rs")).unwrap();
        assert_eq!(first, again);
    }
}
