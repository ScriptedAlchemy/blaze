//! Names for instances that are the same in every run of the daemon on the same sources, so
//! that code compiled by one run can be found by the next. A crate of the checkout is named
//! by what is written in it, so an edit makes other names of everything in it.

use std::{cell::RefCell, fmt::Write, sync::Arc};

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

struct SemanticComponent {
    contextual: bool,
    constants: Vec<String>,
    sources: Vec<super::cached::SemanticSource>,
    dependencies: Vec<usize>,
    inputs: std::sync::OnceLock<Option<Arc<super::cached::SemanticInputs>>>,
}

struct SemanticNode {
    contextual: bool,
    constants: Vec<String>,
    sources: Vec<super::cached::SemanticSource>,
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
    // Completed SCCs belong to this naming view. Arena edges avoid recursive graph drops.
    components: RefCell<Vec<SemanticComponent>>,
    certificates: RefCell<FxHashMap<InstanceIn, Result<usize, &'static str>>>,
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
            components: RefCell::default(),
            certificates: RefCell::default(),
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
        self.frozen = frozen;
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
        if let Some(inputs) = self.semantic_inputs(instance, environment)? {
            name.push_str(SEMANTIC_INPUTS);
            name.push_str(inputs.digest());
        }
        Some(name)
    }

    pub(super) fn semantic_inputs(
        &self,
        instance: &Instance,
        environment: &hir_ty::traits::StoredParamEnvAndCrate,
    ) -> Option<Option<Arc<super::cached::SemanticInputs>>> {
        self.db.unwind_if_revision_cancelled();
        let key = InstanceIn::new(instance.clone(), environment.clone());
        let cached = self.certificates.borrow().get(&key).copied();
        let result = cached.unwrap_or_else(|| {
            let result = self.semantic_closure(&key).ok_or_else(|| self.missed.get());
            self.certificates.borrow_mut().insert(key, result);
            result
        });
        match result {
            Ok(component) => {
                if !self.components.borrow()[component].contextual {
                    return Some(None);
                }
                Some(Some(self.semantic_component_inputs(component)?))
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
            let cached = self.certificates.borrow().get(&key).copied();
            if let Some(cached) = cached {
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
            let node = match self.semantic_node(&key) {
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
                contextual |= self.components.borrow()[child].contextual;
                children.push(child);
            }
            for child in boundaries {
                let cached = self
                    .certificates
                    .borrow()
                    .get(&child)
                    .copied()
                    .expect("discovered semantic boundary");
                let child = match cached {
                    Ok(child) => child,
                    Err(why) => return self.miss(why),
                };
                contextual |= self.components.borrow()[child].contextual;
                children.push(child);
            }
            constants.sort();
            constants.dedup();
            sources.sort();
            sources.dedup();
            children.sort_unstable();
            children.dedup();
            let certificate = {
                let mut arena = self.components.borrow_mut();
                let at = arena.len();
                arena.push(SemanticComponent {
                    contextual,
                    constants,
                    sources,
                    dependencies: children,
                    inputs: std::sync::OnceLock::new(),
                });
                at
            };
            for &member in members {
                self.certificates.borrow_mut().insert(keys[member].clone(), Ok(certificate));
            }
            complete[component] = Some(certificate);
        }
        self.certificates.borrow().get(root).copied()?.ok()
    }

    fn cache_semantic_failure(
        &self,
        failed: &InstanceIn,
        pending: &[(InstanceIn, bool)],
        why: &'static str,
    ) {
        let mut cache = self.certificates.borrow_mut();
        cache.insert(failed.clone(), Err(why));
        for (ancestor, exiting) in pending {
            if *exiting {
                cache.insert(ancestor.clone(), Err(why));
            }
        }
    }

    fn semantic_component_inputs(&self, root: usize) -> Option<Arc<super::cached::SemanticInputs>> {
        let arena = self.components.borrow();
        arena[root]
            .inputs
            .get_or_init(|| {
                let mut pending = vec![root];
                let mut seen = FxHashSet::default();
                let mut constants = Vec::new();
                let mut sources = Vec::new();
                while let Some(component) = pending.pop() {
                    self.db.unwind_if_revision_cancelled();
                    if !seen.insert(component) {
                        continue;
                    }
                    #[cfg(test)]
                    self.semantic_flatten_visits.set(self.semantic_flatten_visits.get() + 1);
                    let component = &arena[component];
                    constants.extend(component.constants.iter().cloned());
                    sources.extend(component.sources.iter().cloned());
                    pending.extend(&component.dependencies);
                }
                constants.sort();
                constants.dedup();
                sources.sort();
                sources.dedup();
                let inputs = super::cached::SemanticInputs::new(constants, sources);
                inputs.canonical().then(|| Arc::new(inputs))
            })
            .clone()
            .or_else(|| self.miss("ambiguous semantic source provenance"))
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
            constants: vec![format!("body {body}")],
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
            node.constants.push(name);
        }
        for read in &prepared.source_names {
            node.sources.push(super::cached::SemanticSource {
                body: body.clone(),
                file: self.file_path(read.file)?,
                name: read.name.clone()?,
            });
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
            node.constants.push(edge);
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
            let arena = names.components.borrow();
            assert_eq!(arena.len(), N);
            assert!(arena.iter().all(|component| component.inputs.get().is_none()));
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
            let a = names.semantic_inputs(&f, &environment).unwrap().unwrap();
            let b = names.semantic_inputs(&g, &environment).unwrap().unwrap();
            assert!(Arc::ptr_eq(&a, &b));
            assert_eq!(names.semantic_visits.get(), 2);
            assert_eq!(names.semantic_flatten_visits.get(), 1);
            assert_ne!(names.instance_in(&f, &environment), names.instance_in(&g, &environment));
            let cancelled = salsa::Cancelled::catch(std::panic::AssertUnwindSafe(|| {
                db.cancellation_token().cancel();
                names.semantic_inputs(&f, &environment)
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
}
