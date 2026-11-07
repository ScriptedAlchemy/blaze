use super::*;
use base_db::SourceDatabase;

const DEP: &str = "pub fn mono() -> usize { 7 }\npub fn identity<T>(value: T) -> T { value }\npub fn width<T>() -> usize { core::mem::size_of::<T>() }\n";
const APP: &str = "pub struct Local { pub bytes: [u8; 1] }\npub fn caller() -> usize { dep::identity(1usize) }\npub fn layout() -> usize { dep::width::<Local>() }\n";

struct SourceSymbols<'a>(&'a RootDatabase);

impl SymbolResolver for SourceSymbols<'_> {
    fn function(&self, function: FunctionId) -> Option<String> {
        Some(format!(
            "tracking_{}_{}",
            fncache::crate_name(self.0, function.krate(self.0)),
            FunctionSignature::of(self.0, function).name.as_str()
        ))
    }
    fn static_(&self, _: StaticId) -> Option<String> {
        None
    }
    fn path(&self, _: &str) -> Option<String> {
        None
    }
    fn item(&self, _: hir_def::ModuleDefId) -> Option<String> {
        None
    }
    fn source_path(&self, file: base_db::FileId) -> Option<String> {
        let krate = base_db::all_crates(self.0)
            .iter()
            .copied()
            .find(|it| it.data(self.0).root_file_id == file)?;
        Some(format!("{}/lib.rs", fncache::crate_name(self.0, krate)))
    }
    fn stable_crate_id(&self, krate: Crate) -> Option<u64> {
        Some(u64::from_le_bytes(self.content_id(krate)?.0[..8].try_into().unwrap()))
    }
    fn content_id(&self, krate: Crate) -> Option<ContentId> {
        let data = krate.data(self.0);
        let mut hash = Sha256::new();
        for field in [
            fncache::crate_name(self.0, krate).as_bytes(),
            self.0.file_text(data.root_file_id).text(self.0).as_bytes(),
            format!("{:?}", data.codegen_flags).as_bytes(),
        ] {
            hash.update((field.len() as u64).to_le_bytes());
            hash.update(field);
        }
        let mut dependencies: Vec<_> = data.dependencies.iter().collect();
        dependencies.sort_by(|a, b| a.name.as_str().cmp(b.name.as_str()));
        for dependency in dependencies {
            hash.update(self.content_id(dependency.crate_id)?.0);
        }
        Some(ContentId(hash.finalize().into()))
    }
    fn rustc_built(&self, _: Crate) -> bool {
        false
    }
    fn nothing_of_rustc(&self) -> bool {
        true
    }
}

fn fixture(app: &str) -> RootDatabase {
    RootDatabase::with_files(&format!(
        "//- minicore: sized, size_of\n//- /dep/lib.rs crate:dep\n{DEP}\n//- /app/lib.rs crate:app deps:dep\n{app}"
    ))
}

fn krate(db: &RootDatabase, name: &str) -> Crate {
    *base_db::all_crates(db).iter().find(|&&it| fncache::crate_name(db, it) == name).unwrap()
}

fn function(db: &RootDatabase, owner: &str, name: &str) -> FunctionId {
    hir_def::nameres::crate_def_map(db, krate(db, owner))
        .modules(db)
        .flat_map(|(_, module)| module.scope(db).declarations())
        .find_map(|item| match item {
            hir_def::ModuleDefId::FunctionId(id)
                if FunctionSignature::of(db, id).name.as_str() == name =>
            {
                Some(id)
            }
            _ => None,
        })
        .unwrap()
}

fn context<'a>(
    db: &'a RootDatabase,
    symbols: &'a SourceSymbols<'a>,
    roots: &[FunctionId],
) -> ExportContext<'a, 'a> {
    let mut policy = policy(false);
    policy.closure = symbols.content_id(krate(db, "app")).unwrap().0;
    ExportContext::resolved(db, symbols, base_db::all_crates(db).iter().copied(), roots, policy)
        .unwrap()
}

fn dependency_request(db: &RootDatabase, root: FunctionId, label: &str) -> LocalRequest {
    let compiled = compiled_root(db, root).as_ref().unwrap();
    compiled
        .requests
        .funcs
        .iter()
        .find_map(|request| match &request.target {
            FuncTarget::Instance(instance, env) if callee_name(db, instance.def) == label => {
                Some(LocalRequest { kind: LocalKind::Instance(instance.clone(), env.clone()) })
            }
            _ => None,
        })
        .unwrap()
}

fn own(db: &RootDatabase, symbols: &dyn SymbolResolver) -> OwnStatics {
    OwnStatics::new(
        db,
        symbols,
        [krate(db, "app")].into_iter().collect(),
        Default::default(),
        Default::default(),
        true,
    )
}

fn execute(bytes: &[u8], directory: &Path, symbol: &str) -> String {
    let symbol = if cfg!(target_os = "macos") { format!("_{symbol}") } else { symbol.to_owned() };
    // Clang quotes punctuated labels itself; GCC passes the spelling to GAS.
    let native_label = r#"
#ifdef __clang__
#define NATIVE_LABEL(name) name
#else
#define NATIVE_LABEL(name) "\"" name "\""
#endif
"#;
    std::fs::create_dir_all(directory).unwrap();
    std::fs::write(directory.join("code.o"), bytes).unwrap();
    std::fs::write(directory.join("main.c"), format!("#include <stdio.h>\n#include <stddef.h>\n{native_label}extern size_t result(void) __asm__(NATIVE_LABEL(\"{symbol}\"));\nint rust_eh_personality(void) {{ return 0; }}\nint main(void) {{ printf(\"%zu\\n\", result()); }}\n")).unwrap();
    let linked = std::process::Command::new("cc")
        .args([directory.join("main.c"), directory.join("code.o")])
        .arg("-o")
        .arg(directory.join("program"))
        .output()
        .unwrap();
    assert!(linked.status.success(), "{}", String::from_utf8_lossy(&linked.stderr));
    let output = std::process::Command::new(directory.join("program")).output().unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    String::from_utf8(output.stdout).unwrap()
}

#[test]
fn root_and_requester_instance_have_identical_portable_tracking() {
    store::compiles_against("1.99.0");
    let db = fixture(APP);
    hir::attach_db(&db, || {
        let symbols = SourceSymbols(&db);
        let mono = function(&db, "dep", "mono");
        let context = context(&db, &symbols, &[mono]);
        let instance = root_instance(&db, mono);
        let env = ParamEnvAndCrate {
            param_env: hir_ty::next_solver::ParamEnv::empty(),
            krate: krate(&db, "app"),
        }
        .store();
        let requests = [
            LocalRequest::root(mono),
            LocalRequest::function_instance(mono, instance.args, env, instance.checked),
        ];
        let mut answers = Vec::new();
        for request in &requests {
            let directory = scratch("tracking-form");
            let store = store::Store::owned(&directory).unwrap();
            let batch =
                compile_batch(&context, context.digest(), &[request.clone()], &store).unwrap();
            require_exported(&batch, 1);
            let result = &batch.receipt.entries[0];
            answers.push((result.key, store.get(&result.key).unwrap(), result.work.clone()));
            drop(store);
            std::fs::remove_dir_all(directory).unwrap();
        }
        assert_eq!(answers[0].0, answers[1].0);
        assert_ne!(answers[0].2.home, answers[1].2.home);
        assert_eq!(
            cached::Entry::decode(&answers[0].1).unwrap().tracked,
            cached::Entry::decode(&answers[1].1).unwrap().tracked
        );
        assert_eq!(answers[0].1, answers[1].1);
        assert_eq!(cached::Entry::decode(&answers[0].1).unwrap().tracked.unwrap().home, None);
    });
}

#[test]
fn edited_requester_reuses_dependency_bytes_under_a_fresh_receipt() {
    use blaze_dist::holder::Plan;
    use blaze_wire::{Attempt, ExecutionId, Grant, ItemOutcome, Lease, Set};
    store::compiles_against("1.99.0");
    let directory = scratch("edited-requester");
    std::fs::create_dir(&directory).unwrap();
    let mut earlier = None;
    let mut snapshots = Vec::new();
    for (leg, app) in
        [APP.to_owned(), APP.replace("identity(1usize)", "identity(2usize)")].iter().enumerate()
    {
        let db = fixture(app);
        hir::attach_db(&db, || {
            let symbols = SourceSymbols(&db);
            let caller = function(&db, "app", "caller");
            let context = context(&db, &symbols, &[caller]);
            let request = dependency_request(&db, caller, "identity");
            let store = store::Store::owned(&directory.join(format!("solo-{leg}"))).unwrap();
            let batch =
                compile_batch(&context, context.digest(), &[request.clone()], &store).unwrap();
            require_exported(&batch, 1);
            let exported = batch.receipt.entries[0].clone();
            let bytes = store.get(&exported.key).unwrap();
            snapshots.push((
                symbols.content_id(krate(&db, "app")).unwrap(),
                symbols.content_id(krate(&db, "dep")).unwrap(),
                exported.clone(),
                bytes.clone(),
            ));
            if leg == 0 {
                let packs: BTreeMap<_, _> = batch
                    .receipt
                    .packs
                    .iter()
                    .map(|pack| (pack.clone(), store.pack(pack).unwrap()))
                    .collect();
                earlier = Some((batch.receipt, packs));
                return;
            }
            let (old_receipt, packs) = earlier.as_ref().unwrap();
            assert_ne!(old_receipt.context, context.digest());
            assert!(
                import_batch(&context, old_receipt, &directory.join("old-context"), |_| panic!(
                    "old context fetched bytes"
                ))
                .err()
                .unwrap()
                .contains("context")
            );
            let reused = store::Store::owned(&directory.join("reused")).unwrap();
            for (pack, bytes) in packs {
                reused.import_pack(pack, bytes).unwrap();
            }
            reused.refresh();
            let mut worker = dist::Worker::new(
                &context,
                &reused,
                vec![request],
                context.digest(),
                [0; 32],
                |_, _| Ok(()),
            )
            .unwrap();
            let plan = Plan {
                grant: Grant {
                    lease: Lease {
                        execution: ExecutionId(2),
                        attempt: Attempt(1),
                        holder: "edited-requester".into(),
                        generation: 1,
                    },
                    roots: Set::new(vec![(0, 1)]),
                    names: Set::new(vec![]),
                },
                compile: vec![],
            };
            let answer = worker.compile(&plan).unwrap();
            assert!(matches!(answer.roots[0].outcome, ItemOutcome::FromStore(_)));
            let receipt = dist::merged_receipt(worker.receipts()).unwrap();
            drop(worker);
            assert_eq!(receipt.entries[0].work, exported.work);
            assert_eq!(
                receipt.entries[0].entry_digest,
                Digest256::from(Sha256::digest(&snapshots[0].3))
            );
            let imported = import_batch(&context, &receipt, &directory.join("import"), |pack| {
                Ok(packs[pack].clone())
            })
            .unwrap();
            assert_eq!(imported.entry_bytes(&exported.key).unwrap(), snapshots[0].3);
            assert_eq!(
                imported.rebound_entry_bytes(&context, &exported.key).unwrap(),
                snapshots[0].3
            );
            let rejected = FxHashMap::default();
            let on_demand = [caller].into_iter().collect();
            let mut object = ObjectCodegen::new(
                &db,
                &symbols,
                &rejected,
                own(&db, &symbols),
                &on_demand,
                None,
                true,
            )
            .unwrap();
            object.entries = Some(std::sync::Arc::new(imported.entries(&context).unwrap()));
            object.root_names = context.roots.clone();
            object.define_function(caller).unwrap();
            assert!(object.define_instances().unwrap().is_empty());
            let bytes = object.finish().unwrap().0;
            assert_eq!(execute(&bytes, &directory.join("run"), "tracking_app_caller"), "2\n");
        });
    }
    std::fs::remove_dir_all(directory).unwrap();
    assert_ne!(snapshots[0].0.0, snapshots[1].0.0);
    assert_eq!(snapshots[0].1.0, snapshots[1].1.0);
    assert_eq!(snapshots[0].2.key, snapshots[1].2.key);
    assert_ne!(snapshots[0].2.work.home, snapshots[1].2.work.home);
    assert_eq!(snapshots[0].3, snapshots[1].3);
}

#[test]
fn frozen_layout_and_root_edits_reconsider_hydrated_code() {
    store::compiles_against("1.99.0");
    for layout_edit in [true, false] {
        let directory = scratch("tracking-live-edit");
        let mut db = fixture(APP);
        let (mut base, symbol, width) = hir::attach_db(&db, || {
            let symbols = SourceSymbols(&db);
            let root = function(&db, "app", "layout");
            let context = context(&db, &symbols, &[root]);
            let width = context.work_name(&dependency_request(&db, root, "width")).unwrap().name;
            let store = store::Store::owned(&directory).unwrap();
            let requests = all_requests(&context, &[root], &store);
            let batch = compile_batch(&context, context.digest(), &requests, &store).unwrap();
            require_exported(&batch, requests.len());
            let entries = admitted_closure(&context, &store, &batch.receipt).unwrap();
            let store = std::sync::Arc::new(store);
            let rejected = FxHashMap::default();
            let on_demand = [root].into_iter().collect();
            let mut object = ObjectCodegen::new(
                &db,
                &symbols,
                &rejected,
                own(&db, &symbols),
                &on_demand,
                None,
                true,
            )
            .unwrap();
            object.entries = Some(std::sync::Arc::new(cached::Entries::owned(
                store,
                entries.into_iter().collect(),
                cached::Told::Nothing,
                None,
                None,
                None,
            )));
            object.root_names = context.roots.clone();
            let root_symbol = object.define_function(root).unwrap();
            assert!(object.define_instances().unwrap().is_empty());
            let id = object
                .found
                .iter()
                .find_map(|(name, id)| (name.as_ref() == width).then_some(*id))
                .unwrap();
            let symbol =
                object.module.declarations().get_function_decl(id).linkage_name(id).into_owned();
            let base =
                patch::Base::record(&db, &object, &[root], &[(root, root_symbol)], &[]).unwrap();
            let bytes = object.finish().unwrap().0;
            assert_eq!(execute(&bytes, &directory.join("before"), "tracking_app_layout"), "1\n");
            (base, symbol, width)
        });
        let app = hir::attach_db(&db, || krate(&db, "app"));
        let file = app.data(&db).root_file_id;
        base.touch(&db, Some(&[app]));
        db.set_file_text(
            file,
            &if layout_edit {
                APP.replace("[u8; 1]", "[u8; 2]")
            } else {
                APP.replace("dep::width::<Local>()", "dep::width::<Local>() + 1")
            },
        );
        hir::attach_db(&db, || {
            let symbols = SourceSymbols(&db);
            let root = function(&db, "app", "layout");
            let request = dependency_request(&db, root, "width");
            let LocalKind::Instance(instance, _) = &request.kind else { panic!("not an instance") };
            let frozen = stable::StableNames::new(
                &db,
                &symbols,
                base_db::all_crates(&db).iter().copied().collect(),
                true,
            )
            .frozen(base.frozen_ids().unwrap().clone());
            assert_eq!(frozen.instance(instance).unwrap(), width);
            assert_ne!(symbols.content_id(app).unwrap().0, base.frozen_ids().unwrap()[&app].0);
            let stale = base.stale(&db, Some(&symbols), 0, 1);
            assert!(stale.steps.instances_asked >= 1);
            assert!(stale.steps.roots_asked >= 1);
            let roots = Roots {
                entries: &[root],
                on_demand: &[],
                start: None,
                allocator: None,
                shared: false,
                whole: true,
                kept: &[],
                offered: &[],
            };
            let patch = base
                .patch(
                    &db,
                    &symbols,
                    roots,
                    &FxHashSet::default(),
                    &FxHashSet::default(),
                    &Rejections::default(),
                    own(&db, &symbols),
                    vec![stale],
                )
                .unwrap()
                .unwrap();
            if layout_edit {
                assert_eq!(execute(&patch.object, &directory.join("run"), &symbol), "2\n");
            } else {
                use object::{Object as _, ObjectSymbol as _};
                let parsed = object::File::parse(&*patch.object).unwrap();
                assert!(parsed.symbols().any(|symbol| symbol.is_definition()
                    && symbol.name().unwrap_or("").contains("tracking_app_layout")));
            }
        });
        std::fs::remove_dir_all(directory).unwrap();
    }
}
