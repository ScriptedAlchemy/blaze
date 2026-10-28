//! Real query fixtures. The process fixture re-loads, not clones, its RootDatabase.

use super::*;
use ide_db::RootDatabase;
use test_fixture::WithFixture;

const SOURCE: &str = r#"
//- minicore: sized, fn, copy, concat, unsize, coerce_unsized, dispatch_from_dyn
//- /src/lib.rs crate:case
#[rustc_builtin_macro]
macro_rules! file {}
pub fn identity<T>(x: T) -> T { x }
pub fn number<const N: usize>() -> usize { N }
pub trait Value { fn value(self) -> i32; }
impl Value for i32 { fn value(self) -> i32 { self } }
pub fn bounded<T: Value>(x: T) -> i32 { x.value() }
pub fn helper() -> i32 { 7 }
pub fn root() -> i32 {
    let closure = |x: i32| x + 2;
    bounded(identity(helper())) + closure(number::<3>() as i32)
}
pub fn source_path() -> &'static str { concat!("literal:", file!(), ":tail") }
pub trait View { fn read(&self) -> i32; }
impl View for i32 { fn read(&self) -> i32 { *self } }
pub fn view(x: &dyn View) -> i32 { x.read() }
pub fn vtable_root() -> i32 { view(&9) }
pub fn other_closure() -> i32 { let f = |x: i32| x + 4; f(1) }
"#;

struct Symbols<'a> {
    db: &'a RootDatabase,
    debug: bool,
}

const TYPE_ID_SOURCE: &str = r#"
//- minicore: sized
//- /src/lib.rs crate:case
#![feature(lang_items, intrinsics)]
pub mod any {
    #[lang = "type_id"]
    pub struct TypeId { pub data: [*const (); 2] }
    impl TypeId {
        pub fn of<T: 'static>() -> TypeId {
            const { crate::intrinsics::type_id::<T>() }
        }
    }
}
pub mod intrinsics {
    #[rustc_intrinsic]
    pub const fn type_id<T>() -> crate::any::TypeId;
}
pub fn runtime() -> any::TypeId { any::TypeId::of::<u64>() }
pub const ID: any::TypeId = any::TypeId::of::<u64>();
pub fn constant() -> &'static any::TypeId { &ID }
"#;

impl SymbolResolver for Symbols<'_> {
    fn function(&self, func: FunctionId) -> Option<String> {
        Some(format!("case_{}", FunctionSignature::of(self.db, func).name.as_str()))
    }
    fn static_(&self, id: StaticId) -> Option<String> {
        Some(format!("case_{}", StaticSignature::of(self.db, id).name.as_str()))
    }
    fn path(&self, _: &str) -> Option<String> {
        None
    }
    fn item(&self, item: hir_def::ModuleDefId) -> Option<String> {
        match item {
            hir_def::ModuleDefId::AdtId(AdtId::StructId(id))
                if hir_def::signatures::StructSignature::of(self.db, id).name.as_str()
                    == "TypeId" =>
            {
                Some("core::any::TypeId".into())
            }
            _ => None,
        }
    }
    fn source_path(&self, _: base_db::FileId) -> Option<String> {
        Some("src/lib.rs".into())
    }
    fn stable_crate_id(&self, krate: Crate) -> Option<u64> {
        Some(if krate.data(self.db).origin.is_local() { 1 } else { 2 })
    }
    fn content_id(&self, krate: Crate) -> Option<ContentId> {
        Some(ContentId(digest(
            b"fixture source closure",
            &[
                SOURCE.as_bytes(),
                krate
                    .extra_data(self.db)
                    .display_name
                    .as_ref()?
                    .canonical_name()
                    .as_str()
                    .as_bytes(),
            ],
        )))
    }
    fn rustc_built(&self, _: Crate) -> bool {
        false
    }
    fn nothing_of_rustc(&self) -> bool {
        true
    }
    fn describes_variables(&self) -> bool {
        self.debug
    }
    fn debug_directory(&self) -> Option<String> {
        self.debug.then(|| "/fixture".into())
    }
}

fn policy(debug: bool) -> Policy {
    Policy {
        closure: Sha256::digest(SOURCE.as_bytes()).into(),
        toolchain: Sha256::digest(b"rust 1.99.0 fixture").into(),
        native_policy: Sha256::digest(b"no native fixture inputs").into(),
        program: None,
        debug,
        variables: debug,
        imports: ["memcpy", "memmove", "memset", "_Unwind_Resume"]
            .into_iter()
            .map(|symbol| {
                (
                    symbol.to_owned(),
                    Import { declaration: Declaration::Function, provider: Provider::Native },
                )
            })
            .collect(),
    }
}

fn fixture() -> (RootDatabase, Vec<FunctionId>, Crate) {
    let (db, _) = RootDatabase::with_single_file(SOURCE);
    let (roots, krate) = hir::attach_db(&db, || {
        let krate =
            *base_db::all_crates(&db).iter().find(|it| it.data(&db).origin.is_local()).unwrap();
        let mut roots = Vec::new();
        for (_, module) in hir_def::nameres::crate_def_map(&db, krate).modules(&db) {
            for def in module.scope(&db).declarations() {
                if let hir_def::ModuleDefId::FunctionId(id) = def {
                    if ["root", "helper", "source_path", "view", "vtable_root", "other_closure"]
                        .contains(&FunctionSignature::of(&db, id).name.as_str())
                    {
                        roots.push(id);
                    }
                }
            }
            for impl_ in module.scope(&db).impls() {
                for &(_, item) in impl_.impl_items(&db).items.iter() {
                    if let hir_def::AssocItemId::FunctionId(id) = item {
                        roots.push(id);
                    }
                }
            }
        }
        (roots, krate)
    });
    (db, roots, krate)
}

fn scratch(tag: &str) -> std::path::PathBuf {
    static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    std::env::temp_dir().join(format!(
        "blaze-d0-{tag}-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ))
}

fn require_exported(batch: &Batch, count: usize) {
    for outcome in &batch.outcomes {
        match outcome {
            Outcome::Exported(_) => {}
            Outcome::Local { work, why } | Outcome::Failed { work, why } => {
                panic!("{work:?}: {why}")
            }
        }
    }
    assert_eq!(batch.receipt.entries.len(), count);
}

fn all_requests(
    context: &ExportContext<'_, '_>,
    roots: &[FunctionId],
    store: &store::Store,
) -> Vec<LocalRequest> {
    let mut requests: Vec<_> = roots.iter().copied().map(LocalRequest::root).collect();
    let mut seen: Vec<LocalKind> = requests.iter().map(|it| it.kind.clone()).collect();
    let mut at = 0;
    while at < requests.len() {
        let batch = compile_batch(context, context.digest(), &requests[at..at + 1], store).unwrap();
        require_exported(&batch, 1);
        for callee in batch.discovered {
            let duplicate = seen.iter().any(|it| match (it, &callee.request.kind) {
                (LocalKind::Root(a), LocalKind::Root(b)) => a == b,
                (LocalKind::Instance(a, env), LocalKind::Instance(b, other)) => {
                    a == b && env == other
                }
                // Roots and no-argument instances can deliberately share an entry.
                _ => false,
            });
            if !duplicate {
                assert!(callee.work.is_ok(), "callee has no total work name");
                seen.push(callee.request.kind.clone());
                requests.push(callee.request);
            }
        }
        at += 1;
        assert!(at < 32, "fixture unexpectedly discovered an unbounded closure");
    }
    requests
}

#[test]
fn real_queries_export_roots_generics_consts_bounds_closure_paths_and_debug() {
    store::compiles_against("1.99.0");
    for debug in [false, true] {
        let (db, roots, _) = fixture();
        hir::attach_db(&db, || {
            let symbols = Symbols { db: &db, debug };
            let context = ExportContext::resolved(
                &db,
                &symbols,
                base_db::all_crates(&db).iter().copied(),
                &roots,
                policy(debug),
            )
            .unwrap();
            let directory = scratch("query");
            let store = store::Store::owned(&directory).unwrap();
            let requests = all_requests(&context, &roots, &store);
            assert!(requests.len() > roots.len(), "no generic/closure queries discovered");
            let batch = compile_batch(&context, context.digest(), &requests, &store).unwrap();
            require_exported(&batch, requests.len());
            assert!(batch.receipt.entries.iter().any(|it| it.work.name.contains("closure")));
            let labels: Vec<_> = batch
                .receipt
                .entries
                .iter()
                .map(|it| cached::Entry::decode(&store.get(&it.key).unwrap()).unwrap().label)
                .collect();
            assert!(labels.iter().any(|it| it == "number"));
            assert!(labels.iter().any(|it| it == "bounded"));
            let path = batch
                .receipt
                .entries
                .iter()
                .find(|it| {
                    cached::Entry::decode(&store.get(&it.key).unwrap()).unwrap().label
                        == "source_path"
                })
                .unwrap();
            let entry = cached::Entry::decode(&store.get(&path.key).unwrap()).unwrap();
            assert!(entry.datas.iter().any(|it| matches!(it, cached::CachedData::Text(parts)
            if parts == &[
                cached::CachedTextPart::Text("literal:".into()),
                cached::CachedTextPart::FilePath("src/lib.rs".into()),
                cached::CachedTextPart::Text(":tail".into()),
            ])));
            assert_eq!(entry.debug.is_some(), debug);
            assert!(
                batch.receipt.entries.iter().any(|it| {
                    cached::Entry::decode(&store.get(&it.key).unwrap())
                        .unwrap()
                        .datas
                        .iter()
                        .any(|it| matches!(it, cached::CachedData::Vtable { .. }))
                }),
                "real compiled requests contained no vtable"
            );
            if debug {
                assert!(
                    batch.receipt.entries.iter().any(|it| {
                        cached::Entry::decode(&store.get(&it.key).unwrap())
                            .unwrap()
                            .debug
                            .is_some_and(|it| !it.stored_types.is_empty() && !it.rows.is_empty())
                    }),
                    "real debug variables, types and rows were not exported"
                );
            }
            let imported_dir = scratch("import");
            let mut incomplete = batch.receipt.clone();
            incomplete.entries.retain(|it| {
                cached::Entry::decode(&store.get(&it.key).unwrap()).unwrap().label != "helper"
            });
            let refused_dir = scratch("root-closure");
            let refused =
                import_batch(&context, &incomplete, &refused_dir, |name| store.pack(name))
                    .err()
                    .unwrap();
            assert!(refused.contains("closure lacks"), "{refused}");
            assert!(!refused_dir.exists());
            let imported =
                import_batch(&context, &batch.receipt, &imported_dir, |name| store.pack(name))
                    .unwrap();
            assert_eq!(imported.entry_bytes(&path.key), store.get(&path.key));
            drop(imported);
            std::fs::remove_dir_all(imported_dir).unwrap();
            drop(store);
            std::fs::remove_dir_all(directory).unwrap();
        });
    }
}

#[test]
fn real_object_bindings_and_batch_bindings_construct_identical_entries() {
    store::compiles_against("1.99.0");
    let (db, roots, krate) = fixture();
    hir::attach_db(&db, || {
        let symbols = Symbols { db: &db, debug: true };
        let context = ExportContext::resolved(
            &db,
            &symbols,
            base_db::all_crates(&db).iter().copied(),
            &roots,
            policy(true),
        )
        .unwrap();
        let rejected = FxHashMap::default();
        let on_demand = roots.iter().copied().collect();
        let owned = OwnStatics::new(
            &db,
            &symbols,
            [krate].into_iter().collect(),
            Default::default(),
            Default::default(),
            true,
        );
        let mut object =
            ObjectCodegen::new(&db, &symbols, &rejected, owned, &on_demand, None, true).unwrap();
        object.root_names = context.roots.clone();
        for root in &roots {
            object.define_function(*root).unwrap();
        }
        let refused = object.define_instances().expect("object fixture failed to define instances");
        assert!(refused.is_empty(), "object fixture refused instances");
        let laters = std::mem::take(&mut object.later);
        assert!(laters.len() > roots.len());
        for later in &laters {
            let instance = match &later.of {
                later::Of::Root { func, .. } => root_instance(&db, *func),
                later::Of::Instance { instance, .. } => instance.clone(),
            };
            let bindings =
                ObjectBindings::new(object.module.declarations(), &object.type_ids, later);
            let mut keeper = object.keeper(None);
            let debug = keeper.describe_function(&later.of, later.compiled, None, false);
            let local = keeper
                .entry(&instance, later.compiled, &bindings, debug.clone())
                .unwrap_or_else(|| panic!("object entry: {:?}", keeper.kept.unkept));
            let exported =
                keeper.entry(&instance, later.compiled, &context, debug).unwrap_or_else(|| {
                    for request in &later.compiled.requests.funcs {
                        let kind = match &request.target {
                            FuncTarget::Symbol(symbol) | FuncTarget::Path(symbol) => symbol.clone(),
                            FuncTarget::Instance(instance, _) => {
                                callee_name(&db, instance.def).to_owned()
                            }
                            FuncTarget::Asm(text) => text.clone(),
                        };
                        eprintln!("requested {kind}");
                    }
                    panic!("batch entry: {:?}", keeper.kept.unkept)
                });
            assert_eq!(
                local.encode(),
                exported.encode(),
                "{}",
                context.names.instance(&instance).unwrap()
            );
        }
    });
}

#[test]
fn imported_receipts_refuse_changed_compiler_settings() {
    const TEST: &str = "export::tests::imported_receipts_refuse_changed_compiler_settings";
    const CHILD: &str = "BLAZE_D0_SETTINGS_CHILD";
    // Frame-pointer policy is process-wide and may change when the next build starts.
    // Isolate that transition from other tests' compiler contexts.
    if std::env::var_os(CHILD).is_none() {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", TEST, "--nocapture"])
            .env(CHILD, "1")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }
    store::compiles_against("1.99.0");
    backend::keep_frame_pointers(false);
    let (db, roots, _) = fixture();
    hir::attach_db(&db, || {
        let symbols = Symbols { db: &db, debug: false };
        let context = ExportContext::resolved(
            &db,
            &symbols,
            base_db::all_crates(&db).iter().copied(),
            &roots,
            policy(false),
        )
        .unwrap();
        let root = roots
            .iter()
            .find(|it| FunctionSignature::of(&db, **it).name.as_str() == "helper")
            .unwrap();
        let directory = scratch("settings-source");
        let store = store::Store::owned(&directory).unwrap();
        let batch = compile_batch(&context, context.digest(), &[LocalRequest::root(*root)], &store)
            .unwrap();
        require_exported(&batch, 1);
        let staging = scratch("settings-import");
        let imported =
            import_batch(&context, &batch.receipt, &staging, |name| store.pack(name)).unwrap();
        let key = batch.receipt.entries[0].key;
        assert!(imported.rebound_entry_bytes(&context, &key).is_ok());

        backend::keep_frame_pointers(true);
        assert!(imported.entries(&context).err().unwrap().contains("compiler settings"));
        assert!(
            imported.rebound_entry_bytes(&context, &key).unwrap_err().contains("compiler settings")
        );
        let publication = directory.join("stale.receipt");
        assert!(batch.receipt.publish(&context, &store, &publication).is_err());
        assert!(!publication.exists());
        let refused = scratch("settings-refused");
        assert!(
            import_batch(&context, &batch.receipt, &refused, |_| {
                panic!("stale compiler context must fail before fetching packs")
            })
            .err()
            .unwrap()
            .contains("compiler settings")
        );
        assert!(!refused.exists());
        backend::keep_frame_pointers(false);
        assert!(imported.rebound_entry_bytes(&context, &key).is_ok());
        drop(imported);
        drop(store);
        std::fs::remove_dir_all(staging).unwrap();
        std::fs::remove_dir_all(directory).unwrap();
    });
}

#[test]
fn mismatches_corruption_and_missing_types_never_admit_a_receipt() {
    store::compiles_against("1.99.0");
    let (db, roots, _) = fixture();
    hir::attach_db(&db, || {
        let symbols = Symbols { db: &db, debug: true };
        let context = ExportContext::resolved(
            &db,
            &symbols,
            base_db::all_crates(&db).iter().copied(),
            &roots,
            policy(true),
        )
        .unwrap();
        let directory = scratch("negative");
        let store = store::Store::owned(&directory).unwrap();
        let root = roots
            .iter()
            .find(|it| FunctionSignature::of(&db, **it).name.as_str() == "helper")
            .unwrap();
        assert!(compile_batch(&context, [0; 32], &[LocalRequest::root(*root)], &store).is_err());
        let batch = compile_batch(&context, context.digest(), &[LocalRequest::root(*root)], &store)
            .unwrap();
        require_exported(&batch, 1);
        for change in 0..4 {
            let mut receipt = batch.receipt.clone();
            match change {
                0 => receipt.context[31] ^= 1,
                1 => receipt.producer[31] ^= 1,
                2 => receipt.entries[0].entry_digest[31] ^= 1,
                3 => receipt.packs.clear(),
                _ => unreachable!(),
            }
            let staging = scratch("refused");
            assert!(import_batch(&context, &receipt, &staging, |name| store.pack(name)).is_err());
            assert!(!staging.exists());
        }
        let staging = scratch("corrupt");
        assert!(
            import_batch(&context, &batch.receipt, &staging, |name| {
                let mut bytes = store.pack(name)?;
                let last = bytes.len() - 1;
                bytes[last] ^= 1;
                Ok(bytes)
            })
            .is_err()
        );
        assert!(!staging.exists());
        assert!(validate_types(&[debuginfo::DebugType::Array { of: 1, len: 3 }]).is_err());
        drop(store);
        std::fs::remove_dir_all(directory).unwrap();
    });
}

#[test]
fn caller_environment_and_unknown_import_are_explicitly_local() {
    store::compiles_against("1.99.0");
    let (db, roots, krate) = fixture();
    hir::attach_db(&db, || {
        use hir_ty::next_solver::{Binder, Clauses, ParamEnv, Predicate, PredicateKind};
        let symbols = Symbols { db: &db, debug: false };
        let context = ExportContext::resolved(
            &db,
            &symbols,
            base_db::all_crates(&db).iter().copied(),
            &roots,
            policy(false),
        )
        .unwrap();
        let root = roots
            .iter()
            .find(|it| FunctionSignature::of(&db, **it).name.as_str() == "helper")
            .unwrap();
        let interner = DbInterner::new_with(&db, krate);
        let ty = Ty::new_int(interner, rustc_type_ir::IntTy::I32);
        let clause = Predicate::new(
            interner,
            Binder::dummy(PredicateKind::Clause(rustc_type_ir::ClauseKind::WellFormed(ty.into()))),
        )
        .as_clause()
        .unwrap();
        let environment = ParamEnvAndCrate {
            param_env: ParamEnv::new(Clauses::new_from_slice(&[clause])),
            krate,
        }
        .store();
        let request = LocalRequest::function_instance(
            *root,
            GenericArgs::new_from_slice(&[]).store(),
            environment,
            false,
        );
        assert!(context.work_name(&request).unwrap_err().contains("caller bounds"));
        let directory = scratch("environment");
        let store = store::Store::owned(&directory).unwrap();
        let batch = compile_batch(&context, context.digest(), &[request], &store).unwrap();
        assert!(
            matches!(&batch.outcomes[0], Outcome::Local { why, .. } if why.contains("caller bounds"))
        );
        assert!(batch.receipt.entries.is_empty());
        let request = FuncRequest {
            target: FuncTarget::Symbol("not-in-policy".into()),
            sig: Signature::new(isa::CallConv::SystemV),
        };
        assert!(context.function(0, &request).is_err());
        drop(store);
        std::fs::remove_dir_all(directory).unwrap();
    });
}

#[test]
fn real_type_id_request_and_constant_blob_derive_the_same_full_value_as_the_object() {
    store::compiles_against("1.99.0");
    let (db, _) = RootDatabase::with_single_file(TYPE_ID_SOURCE);
    hir::attach_db(&db, || {
        let krate =
            *base_db::all_crates(&db).iter().find(|it| it.data(&db).origin.is_local()).unwrap();
        let mut roots = Vec::new();
        for (_, module) in hir_def::nameres::crate_def_map(&db, krate).modules(&db) {
            for def in module.scope(&db).declarations() {
                if let hir_def::ModuleDefId::FunctionId(id) = def
                    && ["runtime", "constant"]
                        .contains(&FunctionSignature::of(&db, id).name.as_str())
                {
                    roots.push(id);
                }
            }
        }
        assert_eq!(roots.len(), 2);
        let symbols = Symbols { db: &db, debug: false };
        let mut policy = policy(false);
        policy.closure = Sha256::digest(TYPE_ID_SOURCE.as_bytes()).into();
        let context = ExportContext::resolved(
            &db,
            &symbols,
            base_db::all_crates(&db).iter().copied(),
            &roots,
            policy,
        )
        .unwrap();
        let directory = scratch("type-id");
        let store = store::Store::owned(&directory).unwrap();
        let batch = compile_batch(
            &context,
            context.digest(),
            &roots.iter().copied().map(LocalRequest::root).collect::<Vec<_>>(),
            &store,
        )
        .unwrap();
        require_exported(&batch, 2);
        let expected = type_id::TypeIdHasher::new(&db, &symbols, &context.owned)
            .type_id(Ty::new_uint(DbInterner::new_with(&db, krate), UintTy::U64))
            .unwrap();
        assert_ne!(expected >> 64, 0, "fixture must exercise both TypeId words");
        let entries: Vec<_> = batch
            .receipt
            .entries
            .iter()
            .map(|it| cached::Entry::decode(&store.get(&it.key).unwrap()).unwrap())
            .collect();
        let constant = roots
            .iter()
            .find(|&&root| FunctionSignature::of(&db, root).name.as_str() == "constant")
            .unwrap();
        let compiled = compiled_root(&db, *constant).as_ref().unwrap();
        let mut altered = CompiledFunction {
            requests: Requests {
                funcs: compiled.requests.funcs.clone(),
                datas: compiled.requests.datas.clone(),
            },
            code: cached::CachedCode::from(&compiled.code).code(),
            source: compiled.source.clone(),
            closure: compiled.closure,
            variables: None,
            layouts: compiled.layouts.clone(),
        };
        let mut altered_placeholders = 0;
        for data in &mut altered.requests.datas {
            if let DataRequest::Blob { blob, .. } = data {
                for (offset, _) in &blob.type_ids {
                    blob.bytes[*offset as usize..*offset as usize + 16].fill(0xee);
                    altered_placeholders += 1;
                }
            }
        }
        assert!(altered_placeholders > 0, "must mutate real const-eval TypeId placeholders");
        assert_eq!(
            patch::stable_mark(&db, &context.names, compiled, false),
            patch::stable_mark(&db, &context.names, &altered, false),
            "tracking must not hash a const-eval table id",
        );
        assert!(entries.iter().any(|entry| entry.funcs.iter().any(
            |it| matches!(it.target, cached::CachedTarget::TypeId(value) if value == expected)
        )));
        assert!(
            entries.iter().any(|entry| entry.datas.iter().any(|it| match it {
                cached::CachedData::Blob { blob, .. } =>
                    blob.bytes.windows(16).any(|it| it == expected.to_le_bytes()),
                _ => false,
            })),
            "constant blob lost its request-derived TypeId: expected {expected:032x}; entries {entries:#?}"
        );
        let rejected = FxHashMap::default();
        let on_demand = roots.iter().copied().collect();
        let owned = OwnStatics::new(
            &db,
            &symbols,
            [krate].into_iter().collect(),
            Default::default(),
            Default::default(),
            true,
        );
        let mut object =
            ObjectCodegen::new(&db, &symbols, &rejected, owned, &on_demand, None, true).unwrap();
        object.root_names = context.roots.clone();
        for root in &roots {
            object.define_function(*root).unwrap();
        }
        let laters = std::mem::take(&mut object.later);
        for later in &laters {
            let later::Of::Root { func, .. } = later.of else { continue };
            let bindings =
                ObjectBindings::new(object.module.declarations(), &object.type_ids, later);
            let mut keeper = object.keeper(None);
            let instance = root_instance(&db, func);
            let local = keeper.entry(&instance, later.compiled, &bindings, None).unwrap();
            let exported = keeper.entry(&instance, later.compiled, &context, None).unwrap();
            assert_eq!(local.encode(), exported.encode());
        }
        drop(store);
        std::fs::remove_dir_all(directory).unwrap();
    });
}

#[test]
fn a_real_entry_missing_its_debug_blob_cannot_be_published_or_imported() {
    store::compiles_against("1.99.0");
    let (db, roots, _) = fixture();
    hir::attach_db(&db, || {
        let symbols = Symbols { db: &db, debug: true };
        let context = ExportContext::resolved(
            &db,
            &symbols,
            base_db::all_crates(&db).iter().copied(),
            &roots,
            policy(true),
        )
        .unwrap();
        let directory = scratch("types");
        let store = store::Store::owned(&directory).unwrap();
        let root = roots
            .iter()
            .find(|it| FunctionSignature::of(&db, **it).name.as_str() == "root")
            .unwrap();
        let batch = compile_batch(&context, context.digest(), &[LocalRequest::root(*root)], &store)
            .unwrap();
        require_exported(&batch, 1);
        let original = &batch.receipt.entries[0];
        let bytes = store.get(&original.key).unwrap();
        let entry = cached::Entry::decode(&bytes).unwrap();
        assert!(
            !entry.debug.unwrap().stored_types.is_empty(),
            "test must reference real debug type blobs"
        );
        let partial_dir = scratch("partial");
        let partial = store::Store::owned(&partial_dir).unwrap();
        // The real compiler entry, unchanged, but none of its separately stored types.
        partial.insert(&original.key, &bytes);
        let mut receipt = batch.receipt.clone();
        receipt.packs = partial.seal_checked().unwrap();
        let publication = partial_dir.join("receipt");
        assert!(receipt.publish(&context, &partial, &publication).is_err());
        assert!(!publication.exists());
        let staging = scratch("partial-import");
        let why =
            import_batch(&context, &receipt, &staging, |name| partial.pack(name)).err().unwrap();
        assert!(why.contains("debug type key"), "{why}");
        assert!(!staging.exists());
        drop(partial);
        drop(store);
        std::fs::remove_dir_all(partial_dir).unwrap();
        std::fs::remove_dir_all(directory).unwrap();
    });
}

/// Invoked by the independent-process test; no external CLI or whole-build remote job.
#[test]
#[ignore]
fn process_child() {
    store::compiles_against("1.99.0");
    let directory = std::path::PathBuf::from(std::env::var_os("BLAZE_D0_PROCESS_DIR").unwrap());
    let reverse = std::env::var_os("BLAZE_D0_REVERSE").is_some();
    let (db, mut roots, _) = fixture();
    hir::attach_db(&db, || {
        // Ask inference and codegen in different orders in separate databases/processes.
        if reverse {
            roots.reverse();
        }
        for root in &roots {
            let _ = InferenceResult::of(&db, DefWithBodyId::from(*root));
        }
        let symbols = Symbols { db: &db, debug: true };
        let context = ExportContext::resolved(
            &db,
            &symbols,
            base_db::all_crates(&db).iter().copied(),
            &roots,
            policy(true),
        )
        .unwrap();
        let store = store::Store::owned(&directory.join("store")).unwrap();
        let requests = all_requests(&context, &roots, &store);
        let mut batch = compile_batch(&context, context.digest(), &requests, &store).unwrap();
        require_exported(&batch, requests.len());
        batch.receipt.entries.sort_by(|a, b| a.work.cmp(&b.work));
        let mut local_ids: Vec<_> = requests
            .iter()
            .filter_map(|request| {
                let LocalKind::Instance(instance, _) = &request.kind else { return None };
                let InstanceDef::Closure(id) = instance.def else { return None };
                Some((context.work_name(request).unwrap().name, format!("{id:?}")))
            })
            .collect();
        local_ids.sort();
        std::fs::write(directory.join("local-closure-order.txt"), format!("{local_ids:?}"))
            .unwrap();
        batch.receipt.publish(&context, &store, &directory.join("receipt")).unwrap();
    });
}

#[test]
fn independently_loaded_processes_produce_identical_entry_digests_and_import() {
    let one = scratch("process-one");
    let two = scratch("process-two");
    for (directory, reverse) in [(&one, false), (&two, true)] {
        let mut child = std::process::Command::new(std::env::current_exe().unwrap());
        child
            .args(["--exact", "export::tests::process_child", "--ignored", "--nocapture"])
            .env("BLAZE_D0_PROCESS_DIR", directory);
        if reverse {
            child.env("BLAZE_D0_REVERSE", "1");
        }
        assert!(child.status().unwrap().success(), "independent compiler process failed");
    }
    let receipt = |dir: &Path| {
        postcard::from_bytes::<Receipt>(&std::fs::read(dir.join("receipt")).unwrap()).unwrap()
    };
    let (a, b) = (receipt(&one), receipt(&two));
    assert_eq!(a.context, b.context);
    assert_eq!(a.producer, b.producer);
    assert_ne!(
        std::fs::read(one.join("local-closure-order.txt")).unwrap(),
        std::fs::read(two.join("local-closure-order.txt")).unwrap(),
        "fixture must exercise different local closure interner orders",
    );
    assert_eq!(
        postcard::to_allocvec(&a.entries).unwrap(),
        postcard::to_allocvec(&b.entries).unwrap()
    );
    store::compiles_against("1.99.0");
    let (db, roots, krate) = fixture();
    hir::attach_db(&db, || {
        let symbols = Symbols { db: &db, debug: true };
        let context = ExportContext::resolved(
            &db,
            &symbols,
            base_db::all_crates(&db).iter().copied(),
            &roots,
            policy(true),
        )
        .unwrap();
        let staging = scratch("process-import");
        let imported = import_batch(&context, &a, &staging, |name| {
            std::fs::read(one.join("store").join(name))
        })
        .unwrap();
        assert!(a.entries.iter().all(|it| imported.entry_bytes(&it.key).is_some()));
        // A fresh real object consumes only the imported generation. No old negative
        // Entries view, patch base or helper ObjectModule crosses this boundary.
        let rejected = FxHashMap::default();
        let on_demand = roots.iter().copied().collect();
        let owned = OwnStatics::new(
            &db,
            &symbols,
            [krate].into_iter().collect(),
            Default::default(),
            Default::default(),
            true,
        );
        let mut object =
            ObjectCodegen::new(&db, &symbols, &rejected, owned, &on_demand, None, true).unwrap();
        object.entries = Some(std::sync::Arc::new(imported.entries(&context).unwrap()));
        object.root_names = context.roots.clone();
        for root in &roots {
            let work = context.work_name(&LocalRequest::root(*root)).unwrap();
            object.stored_roots.insert(*root, Some(StableName::from(work.name)));
            object.define_function(*root).unwrap();
        }
        assert!(
            object
                .define_instances()
                .expect("imported object fixture failed to define instances")
                .is_empty()
        );
        assert!(object.later.is_empty(), "imported assembly unexpectedly compiled a function");
        let (bytes, _) = object.finish().unwrap();
        std::fs::write(staging.join("imported.o"), bytes).unwrap();
        std::fs::write(
            staging.join("main.c"),
            r#"
#include <stdio.h>
extern int case_root(void);
extern int case_vtable_root(void);
int rust_eh_personality(void) { return 0; }
int main(void) { printf("%d %d\n", case_root(), case_vtable_root()); }
"#,
        )
        .unwrap();
        let linked = std::process::Command::new("cc")
            .args([staging.join("main.c"), staging.join("imported.o")])
            .arg("-o")
            .arg(staging.join("imported-program"))
            .output()
            .unwrap();
        assert!(linked.status.success(), "{}", String::from_utf8_lossy(&linked.stderr));
        let program =
            std::process::Command::new(staging.join("imported-program")).output().unwrap();
        assert!(program.status.success());
        assert_eq!(program.stdout, b"12 9\n");
        // The same source rustc builds, for `harness/d0export.py` to compile and run as the
        // differential oracle. This crate starts no rustc (docs/no-rustc.md): the reference
        // build is the harness's, and it compares the two programs' output, error and status.
        let source = SOURCE.replace("#[rustc_builtin_macro]\nmacro_rules! file {}\n", "");
        std::fs::write(
            staging.join("oracle.rs"),
            format!("{source}\nfn main() {{ println!(\"{{}} {{}}\", root(), vtable_root()); }}\n"),
        )
        .unwrap();
        std::fs::write(staging.join("program.status"), program.status.code().unwrap().to_string())
            .unwrap();
        if let Some(evidence) = std::env::var_os("BLAZE_D0_EVIDENCE") {
            let evidence = std::path::PathBuf::from(evidence);
            std::fs::create_dir_all(&evidence).unwrap();
            std::fs::write(evidence.join("fixture.rs"), SOURCE).unwrap();
            for (tag, dir, receipt) in [("one", &one, &a), ("two", &two, &b)] {
                let destination = evidence.join(tag);
                std::fs::create_dir_all(&destination).unwrap();
                std::fs::copy(dir.join("receipt"), destination.join("receipt.postcard")).unwrap();
                std::fs::copy(
                    dir.join("local-closure-order.txt"),
                    destination.join("local-closure-order.txt"),
                )
                .unwrap();
                for name in &receipt.packs {
                    std::fs::copy(dir.join("store").join(name), destination.join(name)).unwrap();
                }
                std::fs::write(destination.join("receipt.txt"), format!("{receipt:#?}")).unwrap();
            }
            for name in ["imported.o", "main.c", "oracle.rs", "program.status"] {
                std::fs::copy(staging.join(name), evidence.join(name)).unwrap();
            }
            std::fs::write(evidence.join("program.stdout"), &program.stdout).unwrap();
            std::fs::write(evidence.join("program.stderr"), &program.stderr).unwrap();
        }
        drop(imported);
        std::fs::remove_dir_all(staging).unwrap();
    });
    std::fs::remove_dir_all(one).unwrap();
    std::fs::remove_dir_all(two).unwrap();
}
