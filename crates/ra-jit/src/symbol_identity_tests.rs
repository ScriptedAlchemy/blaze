use super::*;
use hir_def::{HasModule, Lookup};
use hir_ty_codegen::export::{self, ExportContext, Policy};
use object::{Object as _, ObjectSection as _};
use sha2::{Digest, Sha256};
use test_fixture::WithFixture;

const SOURCE: &str = r#"
pub fn first() -> u32 {
    static VALUE: u32 = 1;
    fn imp() -> u32 { 7 }
    imp()
}
pub fn second() -> u32 {
    static VALUE: u32 = 2;
    fn imp() -> u32 { 9 }
    imp()
}
pub fn repeated() -> u32 {
    { fn imp() -> u32 { 3 } imp(); }
    { fn imp() -> u32 { 4 } imp() }
}
"#;

struct Snapshot {
    names: Vec<String>,
    statics: Vec<String>,
    entries: Vec<([u8; 32], Vec<u8>)>,
    receipt: export::Receipt,
    packs: std::collections::BTreeMap<String, Vec<u8>>,
    source: PathBuf,
    object: Vec<u8>,
}

fn database(path: &Path, source: &str) -> (RootDatabase, vfs::FileId) {
    let (mut db, file) =
        RootDatabase::with_single_file(&format!("//- /src/lib.rs crate:sample library\n{source}"));
    let file = file.file_id(&db);
    let mut files = vfs::file_set::FileSet::default();
    files.insert(file, vfs::VfsPath::from(paths::AbsPathBuf::assert_utf8(path.to_owned())));
    let mut change = ide_db::ChangeWithProcMacros::default();
    change.set_roots(vec![ide_db::base_db::SourceRoot::new_library(files)]);
    db.apply_change(change);
    (db, file)
}

#[test]
fn nested_native_names_and_debug_entries_survive_cargo_home_relocation() {
    ra_jit_store::compiles_against("1.99.0");
    let directory = tempfile::tempdir().unwrap();
    let mut first: Option<Snapshot> = None;
    for (leg, machine) in ["alice/src/co", "builder-2/x"].iter().enumerate() {
        let cargo_home = directory.path().join(machine).join("home/.cargo");
        let source = cargo_home
            .join("registry/src/index.crates.io-1949cf8c6b5b557f/sample-1.0.0/src/lib.rs");
        let checkout = directory.path().join(machine).join("checkout");
        std::fs::create_dir_all(source.parent().unwrap()).unwrap();
        std::fs::create_dir_all(&checkout).unwrap();
        std::fs::write(&source, SOURCE).unwrap();
        let source = source.canonicalize().unwrap();
        let cargo_home = cargo_home.canonicalize().unwrap();
        let checkout = checkout.canonicalize().unwrap();
        let (db, file) = database(&source, SOURCE);
        hir::attach_db(&db, || {
            let krate = ide_db::base_db::all_crates(&db)[0];
            let declared = declared_in(&db, krate.into());
            let table = SymbolTable::default();
            let contents = content::Contents::default();
            let listed = Listed::default();
            let mut known = Rejections::default();
            let mut args = Args::parse(
                ["project", "--crate", "sample", "--out", "sample.o"]
                    .into_iter()
                    .map(str::to_owned),
            )
            .unwrap();
            args.checkout = checkout;
            args.library = true;
            args.debug.level = plan::Level::Lines;
            let resolver = Resolver::of(&db, &table, &args, &known, &contents, &listed);
            assert_eq!(resolver.cargo_home().as_deref(), cargo_home.to_str());
            assert_eq!(resolver.source_path(file).as_deref(), source.to_str());
            let mut functions: Vec<_> = declared
                .functions
                .into_iter()
                .filter(|func| func.module(&db).block(&db).is_some())
                .collect();
            functions.sort_by_key(|func| func.lookup(&db).id.value.erase().into_raw());
            assert_eq!(functions.len(), 4);
            let names: Vec<_> =
                functions.iter().map(|&func| resolver.function(func).unwrap()).collect();
            assert_eq!(names.iter().collect::<std::collections::BTreeSet<_>>().len(), 4);
            let mut statics: Vec<_> =
                declared.statics.iter().map(|&id| resolver.own_static(id).unwrap()).collect();
            statics.sort();
            assert_eq!(statics.iter().collect::<std::collections::BTreeSet<_>>().len(), 2);
            let roots = Roots {
                entries: &functions,
                on_demand: &[],
                start: None,
                allocator: None,
                shared: false,
                whole: true,
                kept: &[],
                offered: &[],
            };
            let object =
                CodegenUnit::compile(&db, &resolver, roots, &Default::default(), &mut known)
                    .unwrap()
                    .object
                    .unwrap();
            let policy = Policy {
                closure: resolver.content_id(krate).unwrap().0,
                toolchain: Sha256::digest(b"1.99.0 symbol identity fixture").into(),
                native_policy: Sha256::digest(b"no native inputs").into(),
                program: None,
                debug: true,
                variables: false,
                imports: Default::default(),
            };
            let context =
                ExportContext::resolved(&db, &resolver, [krate], &functions, policy).unwrap();
            let store =
                ra_jit_store::Store::owned(&directory.path().join(format!("store-{leg}"))).unwrap();
            let requests: Vec<_> =
                functions.iter().copied().map(export::LocalRequest::root).collect();
            let batch =
                export::compile_batch(&context, context.digest(), &requests, &store).unwrap();
            for outcome in &batch.outcomes {
                assert!(
                    matches!(outcome, export::Outcome::Exported(_)),
                    "nested function was not exported"
                );
            }
            let entries: Vec<_> = batch
                .receipt
                .entries
                .iter()
                .map(|it| (it.key, store.get(&it.key).unwrap()))
                .collect();
            let parsed = object::File::parse(object.as_slice()).unwrap();
            assert!(
                parsed.sections().filter(|it| it.name().unwrap().contains("debug")).any(|it| it
                    .data()
                    .unwrap()
                    .windows(source.to_str().unwrap().len())
                    .any(|bytes| bytes == source.to_str().unwrap().as_bytes())),
                "debug information lost its actual source path"
            );
            assert_eq!(
                resolver.source_path(file).as_deref(),
                source.to_str(),
                "native identity naming changed the observable source path"
            );
            if let Some(old) = &first {
                assert_ne!(old.source, source);
                assert_eq!(old.names, names, "nested native function names depend on cargo home");
                assert_eq!(old.statics, statics, "nested static names depend on cargo home");
                assert_eq!(old.entries, entries, "portable nested function/debug entries differ");
                let imported = export::import_batch(
                    &context,
                    &old.receipt,
                    &directory.path().join("import"),
                    |name| Ok(old.packs[name].clone()),
                )
                .unwrap();
                for (key, bytes) in &entries {
                    assert_eq!(imported.entry_bytes(key).as_ref(), Some(bytes));
                    let rebound = imported.rebound_entry_bytes(&context, key).unwrap();
                    assert!(
                        rebound
                            .windows(source.to_str().unwrap().len())
                            .any(|it| it == source.to_str().unwrap().as_bytes())
                    );
                    assert!(
                        !rebound
                            .windows(old.source.to_str().unwrap().len())
                            .any(|it| it == old.source.to_str().unwrap().as_bytes())
                    );
                }
                let declarations: String = names
                    .iter()
                    .enumerate()
                    .map(|(nth, symbol)| {
                        let prefix = if cfg!(target_os = "macos") { "_" } else { "" };
                        format!("extern unsigned value{nth}(void) __asm__(\"{prefix}{symbol}\");\n")
                    })
                    .collect();
                let sum = (0..names.len())
                    .map(|nth| format!("value{nth}()"))
                    .collect::<Vec<_>>()
                    .join("+");
                std::fs::write(directory.path().join("old.o"), &old.object).unwrap();
                std::fs::write(directory.path().join("main.c"), format!("#include <stdio.h>\n{declarations}int rust_eh_personality(void) {{ return 0; }}\nint main(void) {{ printf(\"%u\\n\", {sum}); }}\n")).unwrap();
                let linked = std::process::Command::new("cc")
                    .args([directory.path().join("main.c"), directory.path().join("old.o")])
                    .arg("-o")
                    .arg(directory.path().join("program"))
                    .output()
                    .unwrap();
                assert!(linked.status.success(), "{}", String::from_utf8_lossy(&linked.stderr));
                let result =
                    std::process::Command::new(directory.path().join("program")).output().unwrap();
                assert!(result.status.success());
                assert_eq!(result.stdout, b"23\n");
            } else {
                let packs: std::collections::BTreeMap<String, Vec<u8>> = batch
                    .receipt
                    .packs
                    .iter()
                    .map(|name| (name.clone(), store.pack(name).unwrap()))
                    .collect();
                first = Some(Snapshot {
                    names,
                    statics,
                    entries,
                    receipt: batch.receipt,
                    packs,
                    source: source.clone(),
                    object,
                });
            }
        });
    }
}

#[test]
fn unmapped_reserved_sources_remain_native_without_aliasing_mapped_sources() {
    let directory = tempfile::tempdir().unwrap();
    let mut fingerprints = Vec::new();
    for mapped in [false, true] {
        let (db, _) = if mapped {
            let (mut db, files) = RootDatabase::with_many_files(&format!(
                "//- /src/lib.rs crate:sample library\n{SOURCE}\n\
                 //- /probe/lib.rs crate:probe library new_source_root:library\n\
                 pub fn probe() {{}}\n"
            ));
            let mut change = ide_db::ChangeWithProcMacros::default();
            change.set_roots(
                files
                    .iter()
                    .zip([
                        "/cargo/unmapped/sample.rs",
                        "/cargo/registry/src/index/probe-1.0.0/src/lib.rs",
                    ])
                    .map(|(file, path)| {
                        let mut root = vfs::file_set::FileSet::default();
                        root.insert(
                            file.file_id(&db),
                            vfs::VfsPath::from(paths::AbsPathBuf::assert_utf8(path.into())),
                        );
                        ide_db::base_db::SourceRoot::new_library(root)
                    })
                    .collect(),
            );
            db.apply_change(change);
            let file = files[0].file_id(&db);
            (db, file)
        } else {
            database(Path::new("/cargo/unmapped/sample.rs"), SOURCE)
        };
        hir::attach_db(&db, || {
            let krate = *ide_db::base_db::all_crates(&db)
                .iter()
                .find(|it| {
                    it.extra_data(&db).display_name.as_ref().unwrap().canonical_name().as_str()
                        == "sample"
                })
                .unwrap();
            let table = SymbolTable::default();
            let contents = content::Contents::default();
            let listed = Listed::default();
            let mut known = Rejections::default();
            let mut args = Args::parse(
                ["project", "--crate", "sample", "--out", "sample.o"]
                    .into_iter()
                    .map(str::to_owned),
            )
            .unwrap();
            args.checkout = directory.path().to_owned();
            args.library = true;
            let resolver = Resolver::of(&db, &table, &args, &known, &contents, &listed);
            assert_eq!(resolver.cargo_home().as_deref(), mapped.then_some("/cargo"));
            let mut functions: Vec<_> = declared_in(&db, krate.into())
                .functions
                .into_iter()
                .filter(|func| func.module(&db).block(&db).is_some())
                .collect();
            functions.sort_by_key(|func| func.lookup(&db).id.value.erase().into_raw());
            assert_eq!(functions.len(), 4);
            assert!(functions.iter().all(|&func| resolver.function(func).is_some()));
            fingerprints.push(hir_ty_codegen::block_place(&db, &resolver, functions[0]).unwrap());
            if !mapped {
                let roots = Roots {
                    entries: &functions,
                    on_demand: &[],
                    start: None,
                    allocator: None,
                    shared: false,
                    whole: true,
                    kept: &[],
                    offered: &[],
                };
                let unit =
                    CodegenUnit::compile(&db, &resolver, roots, &Default::default(), &mut known)
                        .unwrap();
                assert!(unit.skipped.is_empty());
                assert!(!unit.object.unwrap().is_empty());
                let policy = Policy {
                    closure: [1; 32],
                    toolchain: [2; 32],
                    native_policy: [3; 32],
                    program: None,
                    debug: false,
                    variables: false,
                    imports: Default::default(),
                };
                let refused = ExportContext::resolved(&db, &resolver, [krate], &functions, policy)
                    .err()
                    .unwrap();
                assert!(refused.contains("portable name"), "{refused}");
            }
        });
    }
    assert_ne!(fingerprints[0], fingerprints[1], "literal and mapped identity domains alias");
}
