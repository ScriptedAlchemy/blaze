//! Object-free export of the compiler's local requests. No Salsa identity is an artifact.

#[cfg(test)]
mod tests;

use super::*;
use serde_derive::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{collections::BTreeMap, path::Path};

/// An artifact identity. All 256 bits cross the future protocol boundary.
pub type Digest256 = [u8; 32];

/// Stable identity of a locally held request. Never a serialized Instance or Salsa id.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct WorkName {
    pub name: String,
    pub home: Digest256,
    pub tuple: Digest256,
}

/// Only local Rust callers can construct requests. Discovered instances are returned
/// intact to the holder, which must retain the database that owns their identities.
#[derive(Clone)]
pub struct LocalRequest {
    kind: LocalKind,
}

#[derive(Clone)]
enum LocalKind {
    Root(FunctionId),
    Instance(Instance, StoredParamEnvAndCrate),
}

impl LocalRequest {
    pub fn root(function: FunctionId) -> Self {
        Self { kind: LocalKind::Root(function) }
    }

    /// A generic function selected by the holder in this database. Shims and closures
    /// arrive through discovery, preserving their original asking environment.
    pub fn function_instance(
        function: FunctionId,
        arguments: StoredGenericArgs,
        environment: StoredParamEnvAndCrate,
        overflow_checks: bool,
    ) -> Self {
        Self {
            kind: LocalKind::Instance(
                Instance {
                    def: InstanceDef::Fn(function),
                    args: arguments,
                    checked: overflow_checks,
                },
                environment,
            ),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Declaration {
    Function,
    Static { thread_local: bool },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Provider {
    Native,
    Shipped,
    Runtime,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Import {
    pub declaration: Declaration,
    pub provider: Provider,
}

/// Resolved final-link policy. It is not inferred from a helper's object. The caller
/// supplies content digests of the source/unit closure and native/runtime toolchains.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Policy {
    pub closure: Digest256,
    pub toolchain: Digest256,
    pub native_policy: Digest256,
    pub program: Option<Digest256>,
    pub debug: bool,
    pub variables: bool,
    pub imports: BTreeMap<String, Import>,
}

/// Immutable per-build context; all fields are private after admission. Local crate and
/// function identities are resolved here, and never included in the artifact encoding.
pub struct ExportContext<'a, 'db> {
    db: &'db dyn HirDatabase,
    symbols: &'a dyn SymbolResolver,
    policy: Policy,
    owned: FxHashSet<Crate>,
    roots: FxHashMap<StableName, Option<FunctionId>>,
    names: stable::StableNames<'a, 'db>,
    digest: Digest256,
    settings: Vec<u8>,
}

fn digest(domain: &[u8], fields: &[&[u8]]) -> Digest256 {
    let mut hash = Sha256::new();
    for field in std::iter::once(domain).chain(fields.iter().copied()) {
        hash.update((field.len() as u64).to_le_bytes());
        hash.update(field);
    }
    hash.finalize().into()
}

fn settings() -> std::result::Result<Vec<u8>, String> {
    let backend = Backend::from_env()?;
    let target = Target::selected().map_err(|e| e.to_string())?;
    Ok(format!(
        "{}\0{}\0{}\0{}\0{}",
        target.triple(),
        backend,
        mir_opt_level().number(),
        backend::frame_pointers(),
        regalloc()
    )
    .into_bytes())
}

impl<'a, 'db> ExportContext<'a, 'db> {
    pub fn resolved(
        db: &'db dyn HirDatabase,
        symbols: &'a dyn SymbolResolver,
        owned: impl IntoIterator<Item = Crate>,
        roots: &[FunctionId],
        policy: Policy,
    ) -> std::result::Result<Self, String> {
        if symbols.onto_rustcs() || !symbols.nothing_of_rustc() {
            return Err(
                "batch export requires the owned compiler link policy, not a mixed object".into()
            );
        }
        if policy.variables && !policy.debug {
            return Err("variable debug requires debug information".into());
        }
        if policy.debug != symbols.debug_directory().is_some()
            || policy.variables != (policy.debug && symbols.describes_variables())
        {
            return Err("debug policy and local compiler resolver disagree".into());
        }
        if symbols.keeps_frame_pointers() != backend::frame_pointers() {
            return Err("frame pointer policy is not installed in the local compiler".into());
        }
        let owned: FxHashSet<_> = owned.into_iter().collect();
        if owned.iter().any(|&krate| symbols.rustc_built(krate)) {
            return Err("owned crate is marked as provided by a mixed compiler build".into());
        }
        let names = stable::StableNames::new(db, symbols, owned.clone(), true);
        let mut identities = Vec::new();
        for &krate in &owned {
            identities.push((
                names.content_id(krate).ok_or("owned crate has no closure identity")?.0,
                symbols.stable_crate_id(krate),
            ));
        }
        identities.sort();
        identities.dedup();
        let mut indexed = FxHashMap::default();
        for &root in roots {
            if !owned.contains(&root.krate(db)) {
                return Err("root is outside the resolved owned closure".into());
            }
            if !hir_def::hir::generics::GenericParams::of(db, root.into()).is_empty() {
                return Err(
                    "generic function must be selected as a local instance, not a root".into()
                );
            }
            let name =
                names.instance(&root_instance(db, root)).ok_or("root has no portable name")?;
            if indexed.insert(StableName::from(name), Some(root)).is_some() {
                return Err("root name is ambiguous in the pre-index".into());
            }
        }
        let mut root_names = Vec::new();
        for (name, function) in &indexed {
            let function = function.ok_or("ambiguous root")?;
            let symbol = symbols.function(function).ok_or("root has no resolved link name")?;
            if symbols.defines(&symbol) || !symbols.linked(&symbol) {
                return Err(
                    "owned root is bound as mixed or absent by the local link policy".into()
                );
            }
            root_names.push((name.as_ref().to_owned(), symbol));
        }
        root_names.sort();
        let settings = settings()?;
        let canonical =
            postcard::to_allocvec(&(&policy, identities, root_names)).map_err(|e| e.to_string())?;
        let digest = digest(b"blaze local export context 1", &[&canonical, &settings]);
        Ok(Self { db, symbols, policy, owned, roots: indexed, names, digest, settings })
    }

    pub fn digest(&self) -> Digest256 {
        self.digest
    }

    /// Work identity for the local registry. Nonempty caller bounds require the semantic
    /// environment-key query not present on this base; do not silently alias them to an
    /// empty environment. The original request remains compilable and locally held.
    pub fn work_name(&self, request: &LocalRequest) -> std::result::Result<WorkName, String> {
        let (instance, home) = match &request.kind {
            LocalKind::Root(root) => (root_instance(self.db, *root), root.krate(self.db)),
            LocalKind::Instance(instance, env) => {
                if !env.param_env(self.db).clauses().is_empty() {
                    return Err("caller bounds need a canonical semantic environment key".into());
                }
                (instance.clone(), env.krate)
            }
        };
        let name = self.names.instance(&instance).ok_or("request has no portable stable name")?;
        if !self.owned.contains(&home) {
            return Err("request home is outside the resolved owned closure".into());
        }
        if home::mentioned_crates(self.db, &instance)
            .is_none_or(|crates| !crates.is_subset(&self.owned))
        {
            return Err("request mentions types outside the resolved owned closure".into());
        }
        let home = self.names.content_id(home).ok_or("request home has no closure identity")?.0;
        let tuple =
            digest(b"blaze local request tuple 1", &[name.as_bytes(), &home, b"empty environment"]);
        Ok(WorkName { name, home, tuple })
    }

    fn discovered(&self, instance: &Instance, env: &StoredParamEnvAndCrate) -> LocalRequest {
        if let InstanceDef::Fn(func) = instance.def
            && instance.args.as_ref().is_empty()
            && env.param_env(self.db).clauses().is_empty()
            && env.krate == func.krate(self.db)
            && *instance == root_instance(self.db, func)
            && self
                .names
                .instance(instance)
                .is_some_and(|name| self.roots.get(name.as_str()) == Some(&Some(func)))
        {
            return LocalRequest::root(func);
        }
        LocalRequest { kind: LocalKind::Instance(instance.clone(), env.clone()) }
    }

    fn keeper<'k>(
        &'k self,
        stable_names: &'k mut FxHashMap<Instance, Option<StableName>>,
        source_paths: &'k mut FxHashMap<base_db::FileId, Option<String>>,
        kept: &'k mut Kept,
        told: &'k ToldTypes,
    ) -> later::Keeper<'k, 'db> {
        later::Keeper {
            db: self.db,
            symbols: self.symbols,
            names: &self.names,
            stable_names,
            source_paths,
            kept,
            root_names: &self.roots,
            own_crates: &self.owned,
            told,
            debug: self.policy.debug,
            variables: self.policy.variables,
            program: self.policy.program.map(Some),
            own: true,
            markable: true,
            settled: None,
        }
    }
}

impl Bindings for ExportContext<'_, '_> {
    fn function(
        &self,
        _: usize,
        request: &FuncRequest,
    ) -> std::result::Result<FunctionBinding, &'static str> {
        let symbol = match &request.target {
            FuncTarget::Asm(_) => return Ok(FunctionBinding::Defined),
            FuncTarget::Symbol(symbol) => Some(symbol.clone()),
            FuncTarget::Path(path) => {
                Some(self.symbols.path(path).ok_or("path has no resolved import")?)
            }
            FuncTarget::Instance(instance, _) => {
                if let InstanceDef::Fn(func) = instance.def {
                    if let Some(of) =
                        type_id_of(self.db, self.symbols, func, instance.args.as_ref())
                    {
                        let mut hash =
                            type_id::TypeIdHasher::new(self.db, self.symbols, &self.owned);
                        return hash
                            .type_id(of)
                            .map(FunctionBinding::TypeId)
                            .ok_or("request TypeId could not be hashed");
                    }
                    if let Some(symbol) = link_name(self.db, func) {
                        Some(symbol)
                    } else if instance.args.as_ref().iter().all(|it| it.region().is_some())
                        && as_its_crate_compiles(self.db, self.symbols, instance)
                        && self.owned.contains(&func.krate(self.db))
                    {
                        // The local object imports an owned root symbol while also
                        // requesting its definition. Shared classification yields Root.
                        let name = self
                            .names
                            .instance(instance)
                            .ok_or("owned root has no portable name")?;
                        if self.roots.get(name.as_str()) != Some(&Some(func)) {
                            return Err("owned root is absent from the immutable pre-index");
                        }
                        return self
                            .symbols
                            .function(func)
                            .map(FunctionBinding::Imported)
                            .ok_or("owned root has no link name");
                    } else if self.owned.contains(&func.krate(self.db)) {
                        return Ok(FunctionBinding::Defined);
                    } else {
                        self.symbols.function(func)
                    }
                } else {
                    return Ok(FunctionBinding::Defined);
                }
            }
        }
        .ok_or("function ownership is not resolved")?;
        match self.policy.imports.get(&symbol) {
            Some(Import { declaration: Declaration::Function, .. }) => {
                Ok(FunctionBinding::Imported(symbol))
            }
            _ => Err("function is not provided by the immutable link policy"),
        }
    }

    fn static_(&self, _: usize, of: StaticId) -> std::result::Result<StaticBinding, &'static str> {
        if static_link_name(self.db, of).is_none() && self.owned.contains(&of.krate(self.db)) {
            return Ok(StaticBinding::Defined);
        }
        let symbol = static_link_name(self.db, of)
            .or_else(|| self.symbols.static_(of))
            .ok_or("static has no resolved link name")?;
        match self.policy.imports.get(&symbol) {
            Some(Import { declaration: Declaration::Static { thread_local }, .. })
                if *thread_local == is_thread_local(self.db, of) =>
            {
                Ok(StaticBinding::Imported { symbol, thread_local: *thread_local })
            }
            _ => Err("static is not provided by the immutable link policy"),
        }
    }
}

/// A held callee with a stable identity, or an explicit reason it cannot yet be leased.
pub struct Discovered {
    pub request: LocalRequest,
    pub work: std::result::Result<WorkName, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Exported {
    pub work: WorkName,
    pub key: Digest256,
    pub entry_digest: Digest256,
}

pub enum Outcome {
    Exported(Exported),
    Local { work: std::result::Result<WorkName, String>, why: String },
    Failed { work: std::result::Result<WorkName, String>, why: String },
}

pub struct Batch {
    pub outcomes: Vec<Outcome>,
    pub discovered: Vec<Discovered>,
    pub receipt: Receipt,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Receipt {
    pub context: Digest256,
    pub producer: Digest256,
    pub entries: Vec<Exported>,
    pub packs: Vec<String>,
}

/// Selected query calls only. No object, resolve(), definitions, or callee compilation.
pub fn compile_batch(
    context: &ExportContext<'_, '_>,
    expected_context: Digest256,
    requests: &[LocalRequest],
    store: &store::Store,
) -> std::result::Result<Batch, String> {
    if expected_context != context.digest || settings()? != context.settings {
        return Err("export context or compiler settings mismatch".into());
    }
    let mut writer = cached::Writer::alone(
        store,
        context.symbols.cargo_home(),
        context.symbols.library_sources(),
    );
    let mut outcomes = Vec::new();
    let mut discovered = Vec::new();
    let mut entries = Vec::new();
    let mut key_tuples = FxHashMap::default();
    for request in requests {
        let canonical = match &request.kind {
            LocalKind::Instance(instance, env) => context.discovered(instance, env),
            LocalKind::Root(_) => request.clone(),
        };
        let request = &canonical;
        // Only serialized debug type blobs survive a request. The local object path
        // can share a ToldTypes graph; a helper need not keep that graph for its batch.
        let (mut stable_names, mut source_paths, mut kept) = Default::default();
        let told = ToldTypes::default();
        let mut keeper = context.keeper(&mut stable_names, &mut source_paths, &mut kept, &told);
        let work = context.work_name(request);
        let (compiled, instance, home, of) = match &request.kind {
            LocalKind::Root(root) => (
                compiled_root(context.db, *root),
                root_instance(context.db, *root),
                root.krate(context.db),
                later::Of::Root {
                    func: *root,
                    symbol: context.symbols.function(*root).ok_or("root has no link name")?,
                },
            ),
            LocalKind::Instance(instance, env) => (
                compiled_instance(context.db, instance.clone(), env.clone()),
                instance.clone(),
                env.krate,
                later::Of::Instance { instance: instance.clone(), env: env.clone() },
            ),
        };
        let compiled = match compiled {
            Ok(compiled) => compiled,
            Err(why) => {
                outcomes.push(Outcome::Failed { work, why: why.to_string() });
                continue;
            }
        };
        let mut unleaseable = None;
        for (at, callee) in compiled.requests.funcs.iter().enumerate() {
            if let FuncTarget::Instance(instance, env) = &callee.target {
                let request = context.discovered(instance, env);
                let work = context.work_name(&request);
                if let Err(why) = &work {
                    // Intrinsics and provided imports need no remotely leased callee.
                    // Still retain the original request for the local holder.
                    let needs_work = match context.function(at, callee) {
                        Ok(FunctionBinding::TypeId(_)) => false,
                        Ok(FunctionBinding::Imported(_)) => match instance.def {
                            InstanceDef::Fn(func) => {
                                context.owned.contains(&func.krate(context.db))
                                    && link_name(context.db, func).is_none()
                            }
                            _ => false,
                        },
                        _ => true,
                    };
                    if needs_work {
                        unleaseable = Some(why.clone());
                    }
                }
                discovered.push(Discovered { request, work });
            }
        }
        let work = match work {
            Ok(work) => work,
            Err(why) => {
                outcomes.push(Outcome::Local { work: Err(why.clone()), why });
                continue;
            }
        };
        if let Some(why) = unleaseable {
            outcomes.push(Outcome::Local { work: Ok(work), why });
            continue;
        }
        let debug = keeper.describe_function(&of, compiled, None, false);
        let described = debug.is_some();
        let Some(mut entry) = keeper.entry(&instance, compiled, context, debug) else {
            let mut reasons = keeper.kept.unkept.keys().copied().collect::<Vec<_>>();
            reasons.sort();
            let why = reasons.join("; ");
            outcomes.push(Outcome::Local { work: Ok(work), why });
            keeper.kept.unkept.clear();
            continue;
        };
        if stable::StableNames::of_checkout(&work.name) {
            match keeper.tracked(&instance, home, compiled, described) {
                Ok(tracked) => entry.tracked = Some(tracked),
                Err(why) => {
                    outcomes.push(Outcome::Local { work: Ok(work), why: why.into() });
                    continue;
                }
            }
        }
        let key = cached::Entry::key(
            &work.name,
            cached::Told::of(described, context.policy.variables),
            context.policy.program.as_ref(),
        )
        .ok_or("entry key settings are unavailable")?;
        if let Some(previous) = key_tuples.insert(key, work.tuple)
            && previous != work.tuple
        {
            return Err(
                "entry key cannot distinguish two request homes/environments on this base".into()
            );
        }
        let bytes = writer.encoded(&mut entry).ok_or("entry encoding or portability failed")?;
        if cached::Entry::decode(&bytes).is_none() {
            return Err("exported entry has invalid structural references".into());
        }
        if let Some(previous) = store.get_installed(&key)
            && previous != bytes
        {
            return Err(format!("entry `{}` resolved to conflicting compiled bytes", work.name));
        }
        store.insert(&key, &bytes);
        writer.release_types();
        let exported = Exported { work, key, entry_digest: Sha256::digest(&bytes).into() };
        entries.push(exported.clone());
        outcomes.push(Outcome::Exported(exported));
    }
    writer.finish_checked().map_err(|e| e.to_string())?;
    if settings()? != context.settings {
        return Err("compiler settings changed while the batch was compiling".into());
    }
    let packs = store.seal_checked().map_err(|e| e.to_string())?;
    let receipt = Receipt { context: context.digest, producer: store.producer(), entries, packs };
    validate_receipt(context, store, &receipt)?;
    Ok(Batch { outcomes, discovered, receipt })
}

fn validate_receipt(
    context: &ExportContext<'_, '_>,
    store: &store::Store,
    receipt: &Receipt,
) -> std::result::Result<(), String> {
    if context.digest != receipt.context || store.producer() != receipt.producer {
        return Err("receipt context or store producer mismatch".into());
    }
    let mut keys = FxHashSet::default();
    for name in &receipt.packs {
        let bytes = store.pack(name).map_err(|e| e.to_string())?;
        keys.extend(store::pack_keys(name, &bytes).map_err(str::to_owned)?);
    }
    let mut entries = FxHashMap::default();
    for exported in &receipt.entries {
        if !context.owned.iter().any(|&krate| {
            context.names.content_id(krate).is_some_and(|id| id.0 == exported.work.home)
        }) {
            return Err("receipt request home is outside the resolved closure".into());
        }
        let tuple = digest(
            b"blaze local request tuple 1",
            &[exported.work.name.as_bytes(), &exported.work.home, b"empty environment"],
        );
        if tuple != exported.work.tuple {
            return Err("receipt work tuple digest mismatch".into());
        }
        if let Some((tuple, bytes)) = entries.insert(exported.key, (tuple, exported.entry_digest))
            && (tuple != exported.work.tuple || bytes != exported.entry_digest)
        {
            return Err("receipt has conflicting results for one entry key".into());
        }
        let key = cached::Entry::key(
            &exported.work.name,
            cached::Told::of(context.policy.debug, context.policy.variables),
            context.policy.program.as_ref(),
        )
        .ok_or("entry key settings unavailable")?;
        if key != exported.key || !keys.contains(&store.disk_key(&key)) {
            return Err("receipt entry key is not in its sealed packs".into());
        }
        let bytes = store.get_installed(&key).ok_or("receipt entry missing")?;
        if Digest256::from(Sha256::digest(&bytes)) != exported.entry_digest {
            return Err("receipt entry digest mismatch".into());
        }
        let mut entry = cached::Entry::decode(&bytes).ok_or("invalid entry references")?;
        if entry.debug.is_some() != context.policy.debug {
            return Err("entry debug setting differs from the context".into());
        }
        if stable::StableNames::of_checkout(&exported.work.name) && entry.tracked.is_none() {
            return Err("checkout entry is missing patch tracking".into());
        }
        if let Some(debug) = &mut entry.debug {
            let mut types = Vec::new();
            for key in &debug.stored_types {
                if !keys.contains(&store.disk_key(key)) {
                    return Err("receipt debug type key is not in its sealed packs".into());
                }
                let bytes = store.get_installed(key).ok_or("debug type blob missing")?;
                if digest_types(&bytes) != *key {
                    return Err("debug type blob digest mismatch".into());
                }
                let set: Vec<debuginfo::DebugType> =
                    postcard::from_bytes(&bytes).map_err(|e| e.to_string())?;
                validate_types(&set)?;
                types.push(set.into());
            }
            debug.types = types;
            validate_debug(debug, entry.code.code().bytes.len())?;
        }
        for function in &entry.funcs {
            if let cached::CachedTarget::Import { symbol, .. } = &function.target {
                if !matches!(
                    context.policy.imports.get(symbol),
                    Some(Import { declaration: Declaration::Function, .. })
                ) {
                    return Err("entry imports a function absent from the link policy".into());
                }
            }
            if matches!(function.target, cached::CachedTarget::Copy { .. }) {
                return Err("mixed rustc-copy entry is not a batch artifact".into());
            }
        }
        for data in &entry.datas {
            if let cached::CachedData::Static { symbol, thread_local } = data {
                if !matches!(context.policy.imports.get(symbol), Some(Import { declaration: Declaration::Static { thread_local: tls }, .. }) if tls == thread_local)
                {
                    return Err("entry static/TLS import is absent from the link policy".into());
                }
            }
        }
        entry
            .arrived(
                context.symbols.cargo_home().as_deref(),
                context.symbols.library_sources().as_ref(),
            )
            .ok_or("entry paths cannot be bound here")?;
    }
    Ok(())
}

fn digest_types(bytes: &[u8]) -> Digest256 {
    let mut hash = Sha256::new();
    hash.update(b"ra-jit types 1");
    hash.update(bytes);
    hash.finalize().into()
}

fn validate_types(types: &[debuginfo::DebugType]) -> std::result::Result<(), String> {
    use debuginfo::DebugType;
    if types.is_empty() {
        return Err("empty debug type set".into());
    }
    let in_set = |at: u32| (at as usize) < types.len();
    let aggregate = |of: &debuginfo::Aggregate| {
        of.fields.iter().all(|it| in_set(it.ty)) && of.parameters.iter().all(|(_, ty)| in_set(*ty))
    };
    for ty in types {
        let valid = match ty {
            DebugType::Base { .. } | DebugType::Names { .. } => true,
            DebugType::Pointer { to, .. } => to.is_none_or(in_set),
            DebugType::Array { of, .. } => in_set(*of),
            DebugType::Struct(of) | DebugType::Union(of) => aggregate(of),
            DebugType::Enum { of, variants, .. } => {
                aggregate(of) && variants.iter().all(|it| it.fields.iter().all(|it| in_set(it.ty)))
            }
        };
        if !valid {
            return Err("debug type reference is outside its set".into());
        }
    }
    Ok(())
}

fn validate_debug(debug: &FunctionDebug, code_len: usize) -> std::result::Result<(), String> {
    let place = |at: &debuginfo::Place| (at.file as usize) < debug.files.len();
    let range = |&(start, end): &(u32, u32)| start <= end && end as usize <= code_len;
    if !debug.declared.as_ref().is_none_or(place)
        || !debug.rows.iter().all(|(offset, at)| *offset as usize <= code_len && place(at))
        || !debug.scopes.iter().all(|it| {
            it.parent.is_none_or(|at| (at as usize) < debug.scopes.len())
                && it.ranges.iter().all(range)
        })
        || !debug.variables.iter().all(|it| {
            (it.ty as usize) < debug.types.len()
                && it.scope.is_none_or(|at| (at as usize) < debug.scopes.len())
                && match &it.place {
                    debuginfo::VariablePlace::Frame(_) => true,
                    debuginfo::VariablePlace::Ranges(ranges) => {
                        ranges.iter().all(|(start, end, _)| range(&(*start, *end)))
                    }
                }
        })
    {
        return Err("debug location, scope, variable or range is invalid".into());
    }
    Ok(())
}

/// Admission in a fresh owned directory. No old negative Entries memo survives this
/// boundary, and nothing is exposed until every receipt reference is verified.
pub fn import_batch(
    context: &ExportContext<'_, '_>,
    receipt: &Receipt,
    directory: &Path,
    mut fetch: impl FnMut(&str) -> std::io::Result<Vec<u8>>,
) -> std::result::Result<Imported, String> {
    if directory.exists() {
        return Err("batch import requires a fresh staging directory".into());
    }
    // Own the directory before claiming the store. An exists()/claim() race must not
    // adopt or clean up another importer's directory.
    std::fs::create_dir(directory).map_err(|e| e.to_string())?;
    let store = match store::Store::owned(directory) {
        Ok(store) => store,
        Err(error) => {
            let cleanup = std::fs::remove_dir_all(directory);
            return Err(match cleanup {
                Ok(()) => error.to_string(),
                Err(e) => format!("{error}; staging cleanup: {e}"),
            });
        }
    };
    let admitted = (|| {
        if receipt.context != context.digest || receipt.producer != store.producer() {
            return Err("receipt context or producer mismatch".into());
        }
        for name in &receipt.packs {
            let bytes = fetch(name).map_err(|e| e.to_string())?;
            store.import_pack(name, &bytes).map_err(|e| e.to_string())?;
        }
        store.refresh();
        validate_receipt(context, &store, receipt)?;
        validate_closure(&store, receipt)
    })();
    if let Err(why) = admitted {
        drop(store);
        let cleanup = std::fs::remove_dir_all(directory);
        return Err(match cleanup {
            Ok(()) => why,
            Err(e) => format!("{why}; staging cleanup: {e}"),
        });
    }
    Ok(Imported { store: std::sync::Arc::new(store), receipt: receipt.clone() })
}

/// Raw batch receipts may name work held by another batch. Admission, unlike export,
/// requires a closed set: both Named and Root edges are checked, including cycles.
fn validate_closure(store: &store::Store, receipt: &Receipt) -> std::result::Result<(), String> {
    let names: FxHashSet<_> = receipt.entries.iter().map(|it| it.work.name.as_str()).collect();
    for exported in &receipt.entries {
        let bytes = store.get_installed(&exported.key).ok_or("closure entry missing")?;
        let entry = cached::Entry::decode(&bytes).ok_or("closure entry invalid")?;
        for function in &entry.funcs {
            match &function.target {
                cached::CachedTarget::Named(name) | cached::CachedTarget::Root(name)
                    if !names.contains(name.as_str()) =>
                {
                    return Err(format!("receipt closure lacks `{name}`"));
                }
                _ => {}
            }
        }
        if entry.datas.iter().any(|it| matches!(it, cached::CachedData::OwnStatic(_))) {
            return Err("receipt closure requires the owned-static batch exporter".into());
        }
    }
    Ok(())
}

pub struct Imported {
    store: std::sync::Arc<store::Store>,
    receipt: Receipt,
}

impl Imported {
    pub fn receipt(&self) -> &Receipt {
        &self.receipt
    }

    /// Only admitted entries are exposed, never arbitrary keys in a received pack.
    pub fn entry_bytes(&self, key: &Digest256) -> Option<Vec<u8>> {
        self.receipt
            .entries
            .iter()
            .any(|it| &it.key == key)
            .then(|| self.store.get_installed(key))
            .flatten()
    }

    /// A validated, locally rebound entry. This exposes the same entry bytes as the
    /// object stitcher reads, but not arbitrary records from received packs.
    pub fn rebound_entry_bytes(
        &self,
        context: &ExportContext<'_, '_>,
        key: &Digest256,
    ) -> std::result::Result<Vec<u8>, String> {
        if context.digest != self.receipt.context {
            return Err("imported entry requested under a different context".into());
        }
        let exported = self
            .receipt
            .entries
            .iter()
            .find(|it| &it.key == key)
            .ok_or("entry is not in the admitted set")?;
        let entry = self
            .entries(context)
            .get(&exported.work.name)
            .ok_or("admitted entry cannot be rebound")?;
        entry.encode().ok_or("rebound entry encoding failed".into())
    }

    pub(super) fn entries(&self, context: &ExportContext<'_, '_>) -> cached::Entries {
        cached::Entries::owned(
            self.store.clone(),
            self.receipt.entries.iter().map(|it| it.work.name.clone()).collect(),
            cached::Told::of(context.policy.debug, context.policy.variables),
            context.policy.program,
            context.symbols.cargo_home(),
            context.symbols.library_sources(),
        )
    }
}

impl Receipt {
    /// Publish only the checked result, atomically, using the store's whole-file writer.
    /// This is a local artifact encoding, not a new distributed CBOR schema.
    pub fn publish(
        &self,
        context: &ExportContext<'_, '_>,
        store: &store::Store,
        path: &Path,
    ) -> std::io::Result<()> {
        validate_receipt(context, store, self).map_err(std::io::Error::other)?;
        let bytes = postcard::to_allocvec(self).map_err(std::io::Error::other)?;
        store::write_file_whole(path, &bytes)
    }
}

/// How the final link binds one function request, before any object is defined.
pub(super) enum FunctionBinding {
    Defined,
    Imported(String),
    TypeId(u128),
}

pub(super) enum StaticBinding {
    Defined,
    Imported { symbol: String, thread_local: bool },
}

/// Answers binding questions only. Entry construction is shared by object and batch callers.
pub(super) trait Bindings {
    fn function(
        &self,
        at: usize,
        request: &FuncRequest,
    ) -> std::result::Result<FunctionBinding, &'static str>;
    fn static_(&self, at: usize, of: StaticId) -> std::result::Result<StaticBinding, &'static str>;
}

/// The existing mixed/local object path's binding evidence. Never supplied to a helper.
pub(super) struct ObjectBindings {
    funcs: Vec<FunctionBinding>,
    datas: Vec<StaticBinding>,
}

impl ObjectBindings {
    pub(super) fn new(
        declarations: &cranelift_module::ModuleDeclarations,
        type_ids: &FxHashMap<FuncId, u128>,
        later: &later::Later<'_>,
    ) -> Self {
        let funcs = later
            .ids
            .funcs
            .iter()
            .zip(&later.imported.0)
            .map(|(&id, &imported)| {
                if let Some(&hash) = type_ids.get(&id) {
                    FunctionBinding::TypeId(hash)
                } else if imported {
                    FunctionBinding::Imported(
                        declarations.get_function_decl(id).linkage_name(id).into_owned(),
                    )
                } else {
                    FunctionBinding::Defined
                }
            })
            .collect();
        let datas = later
            .ids
            .datas
            .iter()
            .zip(&later.imported.1)
            .map(|(&id, &imported)| {
                if imported {
                    let decl = declarations.get_data_decl(id);
                    StaticBinding::Imported {
                        symbol: decl.linkage_name(id).into_owned(),
                        thread_local: decl.tls,
                    }
                } else {
                    StaticBinding::Defined
                }
            })
            .collect();
        Self { funcs, datas }
    }
}

impl Bindings for ObjectBindings {
    fn function(
        &self,
        at: usize,
        _: &FuncRequest,
    ) -> std::result::Result<FunctionBinding, &'static str> {
        match self.funcs.get(at) {
            Some(FunctionBinding::TypeId(hash)) => Ok(FunctionBinding::TypeId(*hash)),
            Some(FunctionBinding::Imported(symbol)) => {
                Ok(FunctionBinding::Imported(symbol.clone()))
            }
            Some(FunctionBinding::Defined) => Ok(FunctionBinding::Defined),
            None => Err("missing function binding"),
        }
    }

    fn static_(&self, at: usize, _: StaticId) -> std::result::Result<StaticBinding, &'static str> {
        match self.datas.get(at) {
            Some(StaticBinding::Imported { symbol, thread_local }) => {
                Ok(StaticBinding::Imported { symbol: symbol.clone(), thread_local: *thread_local })
            }
            Some(StaticBinding::Defined) => Ok(StaticBinding::Defined),
            None => Err("missing static binding"),
        }
    }
}
