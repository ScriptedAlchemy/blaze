use super::*;
use ide_db::RootDatabase;
use sha2::{Digest, Sha256};
use test_fixture::WithFixture;

const TEST: &str = "startup_tests::startup_shim_reuses_code_and_rebinds_native_calls";
const CHILD: &str = "BLAZE_STARTUP_CACHE_CHILD";

pub(super) struct Symbols<'a> {
    pub(super) db: &'a RootDatabase,
    pub(super) identity: [u8; 32],
}

impl SymbolResolver for Symbols<'_> {
    fn function(&self, func: FunctionId) -> Option<String> {
        link_name(self.db, func).or_else(|| {
            Some(format!("fixture_{}", FunctionSignature::of(self.db, func).name.as_str()))
        })
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

    fn source_path(&self, _: base_db::FileId) -> Option<String> {
        Some("src/main.rs".into())
    }

    fn stable_crate_id(&self, _: Crate) -> Option<u64> {
        None
    }

    fn content_id(&self, _: Crate) -> Option<ContentId> {
        Some(ContentId(self.identity))
    }

    fn rustc_built(&self, _: Crate) -> bool {
        false
    }

    fn nothing_of_rustc(&self) -> bool {
        true
    }
}

fn build(source: &str, entry: &str, start: bool, allocator: bool) -> (Vec<u8>, usize) {
    build_with_source_name(source, entry, start, allocator, None).unwrap()
}

fn build_with_source_name(
    source: &str,
    entry: &str,
    start: bool,
    allocator: bool,
    source_name: Option<&'static str>,
) -> std::result::Result<(Vec<u8>, usize), String> {
    let mut db = RootDatabase::with_files(source);
    if let Some(name) = source_name {
        hir_ty::mir::location::name_sources(&mut db, move |_, _| Some(name.to_owned()));
    }
    hir::attach_db(&db, || {
        let krate = *base_db::all_crates(&db)
            .iter()
            .find(|krate| krate.data(&db).origin.is_local())
            .unwrap();
        let main = hir_def::nameres::crate_def_map(&db, krate)
            .modules(&db)
            .flat_map(|(_, module)| module.scope(&db).declarations())
            .find_map(|item| match item {
                hir_def::ModuleDefId::FunctionId(id)
                    if FunctionSignature::of(&db, id).name.as_str() == entry =>
                {
                    Some(id)
                }
                _ => None,
            })
            .unwrap();
        let symbols = Symbols { db: &db, identity: Sha256::digest(source.as_bytes()).into() };
        let roots = Roots {
            entries: &[main],
            on_demand: &[],
            start: start.then_some(main),
            allocator: allocator.then_some(krate),
            shared: false,
            whole: true,
            kept: &[],
            offered: &[],
        };
        let mut known = Rejections::default();
        known.not_counted();
        let before = compiled_functions();
        let unit = CodegenUnit::compile(&db, &symbols, roots, &FxHashSet::default(), &mut known)
            .map_err(|error| error.to_string())?;
        if !unit.skipped.is_empty() {
            return Err(format!("fixture compile refused: {:?}", unit.skipped));
        }
        store::Store::global().unwrap().seal_checked().unwrap();
        let compiled = compiled_functions() - before;
        assert_eq!(known.stats.compiled, compiled);
        Ok((unit.object.unwrap(), compiled))
    })
}

#[test]
fn cache_hit_observes_aggregate_filename_and_assertion_inputs() {
    const TEST: &str = "startup_tests::cache_hit_observes_aggregate_filename_and_assertion_inputs";
    if std::env::var_os(CHILD).is_none() {
        let directory =
            std::env::temp_dir().join(format!("blaze-semantic-cache-{}", std::process::id()));
        std::fs::create_dir(&directory).unwrap();
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", TEST, "--nocapture"])
            .env(CHILD, &directory)
            .env("RA_JIT_CACHE", directory.join("store"))
            .env("RA_JIT_CACHE_SEAL_AT", "1")
            .env("RUST_BACKTRACE", "0")
            .env_remove("RA_JIT_CACHE_CHECK")
            .output()
            .unwrap();
        std::fs::remove_dir_all(directory).unwrap();
        assert!(
            output.status.success(),
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }
    store::compiles_against("1.99.0");
    let source = r#"
//- minicore: sized, assert, panic
//- /src/main.rs crate:semantics
#[rustc_builtin_macro] macro_rules! file {}
#[rustc_intrinsic] const unsafe fn size_of_val<T: ?Sized>(value: *const T) -> usize;
struct Aggregate { width: usize }
struct Block<T>([T; 0]);
impl<T> Block<T> {
    const VALUE: Aggregate = {
        let width = unsafe { size_of_val(file!() as *const str) };
        core::assert!(width < 64, "width overflow");
        Aggregate { width }
    };
}
pub fn root() -> usize { Block::<u8>::VALUE.width }
"#;
    for transitive in [false, true] {
        let source = if transitive {
            source.replace("pub fn root() -> usize { Block::<u8>::VALUE.width }", "fn helper<T>() -> usize { Block::<T>::VALUE.width }\npub fn root() -> usize { helper::<u8>() }")
        } else {
            source.to_owned()
        };
        const LONG: &str =
            "source/this-filename-is-deliberately-too-long-for-the-constant-assertion-to-accept.rs";
        let cold = build_with_source_name(&source, "root", false, false, Some(LONG)).unwrap_err();
        assert!(
            cold.contains("width overflow"),
            "the cold failure must reach the real assertion: {cold}"
        );
        let (first, compiled) =
            build_with_source_name(&source, "root", false, false, Some("short.rs")).unwrap();
        assert!(compiled > 0);
        let (_, compiled) =
            build_with_source_name(&source, "root", false, false, Some("short.rs")).unwrap();
        assert_eq!(compiled, 0);
        let warm = build_with_source_name(&source, "root", false, false, Some(LONG));
        assert!(warm.is_err(), "a cache hit must not bypass a newly failing concrete constant");
        assert!(warm.unwrap_err().contains("width overflow"));
        let (changed, compiled) =
            build_with_source_name(&source, "root", false, false, Some("a-longer-source.rs"))
                .unwrap();
        assert!(compiled > 0, "changed folded filenames need distinct cached code");
        let (same_length, compiled) =
            build_with_source_name(&source, "root", false, false, Some("other.rs")).unwrap();
        assert!(compiled > 0, "equal allocation bytes must not backdate different observations");
        let directory = std::path::PathBuf::from(std::env::var_os(CHILD).unwrap());
        for (name, bytes, expected) in [
            ("first", first, "short.rs".len()),
            ("changed", changed, "a-longer-source.rs".len()),
            ("same-length", same_length, "other.rs".len()),
        ] {
            assert_eq!(native_usize(&directory, name, &bytes), expected);
        }
    }
}

fn native_usize(directory: &std::path::Path, name: &str, bytes: &[u8]) -> usize {
    let object = directory.join(format!("{name}.o"));
    let driver = directory.join(format!("{name}.c"));
    let program = directory.join(name);
    std::fs::write(&object, bytes).unwrap();
    std::fs::write(&driver, "#include <stdint.h>\n#include <stdio.h>\nextern uintptr_t fixture_root(void);\nint main(void) { printf(\"%zu\\n\", (size_t)fixture_root()); return 0; }\nint rust_eh_personality(void) { return 0; }\n").unwrap();
    let linked = std::process::Command::new("cc")
        .arg(&object)
        .arg(&driver)
        .arg("-o")
        .arg(&program)
        .output()
        .unwrap();
    assert!(linked.status.success(), "{}", String::from_utf8_lossy(&linked.stderr));
    let output = std::process::Command::new(program).output().unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert!(output.stderr.is_empty());
    let stdout = String::from_utf8(output.stdout).unwrap();
    let value = stdout.trim().parse().unwrap();
    assert_eq!(stdout, format!("{value}\n"));
    value
}

#[test]
fn cached_layout_observes_anonymous_type_filename_inputs() {
    const TEST: &str = "startup_tests::cached_layout_observes_anonymous_type_filename_inputs";
    if std::env::var_os(CHILD).is_none() {
        let directory =
            std::env::temp_dir().join(format!("blaze-type-filename-cache-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&directory);
        std::fs::create_dir_all(&directory).unwrap();
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", TEST, "--nocapture"])
            .env(CHILD, &directory)
            .env("RA_JIT_CACHE", directory.join("store"))
            .env("RA_JIT_CACHE_SEAL_AT", "1")
            .env("RUST_BACKTRACE", "0")
            .env_remove("RA_JIT_CACHE_CHECK")
            .output()
            .unwrap();
        std::fs::remove_dir_all(directory).unwrap();
        assert!(
            output.status.success(),
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }
    store::compiles_against("1.99.0");
    let source = r#"
//- minicore: sized
//- /src/main.rs crate:type_inputs
#[rustc_builtin_macro] macro_rules! file {}
#[rustc_intrinsic] const unsafe fn size_of_val<T: ?Sized>(value: *const T) -> usize;
#[rustc_intrinsic] const fn size_of<T>() -> usize;
struct Block<T> { field: [u8; unsafe { size_of_val(file!() as *const str) }], value: [T; 0] }
pub fn root() -> usize { size_of::<Block<u8>>() }
"#;
    for repeat in [false, true] {
        let source = if repeat {
            source.replace("pub fn root() -> usize { size_of::<Block<u8>>() }", "pub fn root() -> usize { let value = [0u8; unsafe { size_of_val(file!() as *const str) }]; unsafe { size_of_val(&value as *const _) } }")
        } else {
            source.to_owned()
        };
        let directory = std::path::PathBuf::from(std::env::var_os(CHILD).unwrap());
        let (first, cold) =
            build_with_source_name(&source, "root", false, false, Some("short.rs")).unwrap();
        assert!(cold > 0);
        assert_eq!(native_usize(&directory, "cold", &first), "short.rs".len());
        let (_, compiled) =
            build_with_source_name(&source, "root", false, false, Some("short.rs")).unwrap();
        assert_eq!(compiled, 0);
        let (changed, compiled) =
            build_with_source_name(&source, "root", false, false, Some("a-longer-source.rs"))
                .unwrap();
        assert_eq!(
            native_usize(&directory, "changed", &changed),
            "a-longer-source.rs".len(),
            "a warm native layout must reflect its actual current anonymous type constant"
        );
        assert!(compiled > 0);
        let (same_length, compiled) =
            build_with_source_name(&source, "root", false, false, Some("other.rs")).unwrap();
        assert_eq!(native_usize(&directory, "same-length", &same_length), "other.rs".len());
        assert!(compiled > 0, "equal layout bytes must still preserve the current observation set");
        assert!(
            build_with_source_name(&source, "root", false, false, None).is_err(),
            "missing type filename inputs must not hit a previous cache entry"
        );
    }
}

#[test]
fn concrete_preparation_replays_and_retracts_current_filename_reads() {
    use hir_ty::consteval::inputs::{BodyInput, BodyKind, evaluate_selection, prepare_constants};
    use salsa::plumbing::AsId;
    let source = r#"
//- minicore: sized
//- /src/main.rs crate:semantics
#[rustc_builtin_macro] macro_rules! file {}
#[rustc_intrinsic] const unsafe fn size_of_val<T: ?Sized>(value: *const T) -> usize;
struct Aggregate { width: usize }
struct Block<T>([T; 0]);
impl<T> Block<T> { const VALUE: Aggregate = Aggregate { width: unsafe { size_of_val(file!() as *const str) } }; }
pub fn root() -> usize { Block::<u8>::VALUE.width }
"#;
    let mut db = RootDatabase::with_files(source);
    let krate =
        *base_db::all_crates(&db).iter().find(|krate| krate.data(&db).origin.is_local()).unwrap();
    let root = hir_def::nameres::crate_def_map(&db, krate)
        .modules(&db)
        .flat_map(|(_, module)| module.scope(&db).declarations())
        .find_map(|item| match item {
            hir_def::ModuleDefId::FunctionId(id)
                if FunctionSignature::of(&db, id).name.as_str() == "root" =>
            {
                Some(id)
            }
            _ => None,
        })
        .unwrap();
    let environment =
        ParamEnvAndCrate { param_env: hir_ty::next_solver::ParamEnv::empty(), krate }.store();
    let mut observed = Vec::new();
    for name in [Some("short.rs"), Some("other.rs"), None, Some("short.rs")] {
        hir_ty::mir::location::name_sources(&mut db, move |_, _| name.map(str::to_owned));
        hir::attach_db(&db, || {
            let input = BodyInput::new(
                &db,
                BodyKind::Body(root.into()),
                GenericArgs::empty().store(),
                environment.clone(),
            );
            let prepared = prepare_constants(&db, input);
            assert_eq!(prepared.result.is_ok(), name.is_some());
            assert!(!prepared.source_names.is_empty());
            assert!(prepared.source_names.iter().all(|read| read.name.as_deref() == name));
            if let Some(name) = name {
                let allocation =
                    evaluate_selection(&db, &prepared.selections[0], &environment).value.unwrap();
                assert_eq!(
                    usize::from_le_bytes(allocation.memory[..].try_into().unwrap()),
                    name.len()
                );
            }
            observed.push((input.as_id(), prepared.source_names.clone()));
        });
    }
    assert!(observed.iter().all(|entry| entry.0 == observed[0].0));
    assert_ne!(observed[0].1, observed[1].1);
    assert_eq!(observed[0].1, observed[3].1);
}

/// Preparing an instance reads its body once and what it says is what is kept: nothing else
/// reads a monomorphized body from the database, and a body of each instance of a program was
/// the largest thing an 8 GiB bootstrap held. What is prepared is of the filled-in body.
#[test]
fn preparing_an_instance_keeps_no_monomorphized_body() {
    use hir_ty::{
        consteval::inputs::{BodyInput, BodyKind, prepare_constants},
        db::HirDatabase,
    };
    use salsa::Database;
    let source = r#"
//- minicore: sized
//- /src/main.rs crate:semantics
trait Width { const WIDTH: usize; fn width() -> usize; }
impl Width for u8 { const WIDTH: usize = 1; fn width() -> usize { 1 } }
pub fn root<T: Width>() -> usize { T::WIDTH + T::width() }
"#;
    let mut db = RootDatabase::with_files(source);
    hir_ty::mir::location::name_sources(&mut db, |_, _| Some("main.rs".to_owned()));
    let krate =
        *base_db::all_crates(&db).iter().find(|krate| krate.data(&db).origin.is_local()).unwrap();
    let root = hir_def::nameres::crate_def_map(&db, krate)
        .modules(&db)
        .flat_map(|(_, module)| module.scope(&db).declarations())
        .find_map(|item| match item {
            hir_def::ModuleDefId::FunctionId(id)
                if FunctionSignature::of(&db, id).name.as_str() == "root" =>
            {
                Some(id)
            }
            _ => None,
        })
        .unwrap();
    let environment =
        ParamEnvAndCrate { param_env: hir_ty::next_solver::ParamEnv::empty(), krate }.store();
    let args = hir::attach_db(&db, || {
        let interner = hir_ty::next_solver::DbInterner::new_with(&db, krate);
        GenericArgs::new_from_slice(&[hir_ty::next_solver::Ty::new_uint(
            interner,
            rustc_type_ir::UintTy::U8,
        )
        .into()])
        .store()
    });
    hir::attach_db(&db, || {
        let input =
            BodyInput::new(&db, BodyKind::Body(root.into()), args.clone(), environment.clone());
        let prepared = prepare_constants(&db, input);
        assert!(prepared.result.is_ok());
        // Of the instance and not the generic body: `T::width` is `u8`'s, and `T::WIDTH` is one.
        let [(BodyKind::Body(callee), _)] = prepared.callees[..] else {
            panic!("{} callees", prepared.callees.len())
        };
        assert_ne!(callee, hir_ty::InferBodyId::from(root));
        assert_eq!(prepared.selections.len(), 1);
    });
    const QUERY: &str = "monomorphized_runtime_mir_body_query";
    let kept = |db: &mut RootDatabase| {
        let db: &mut dyn Database = db;
        db.memory_usage().queries.get(QUERY).map_or(0, |it| it.count())
    };
    // `WIDTH`'s body, which evaluating it reads, and which the interpreter asks for again. Not
    // the instance's.
    assert_eq!(kept(&mut db), 1, "a body of the instance was kept");
    // The control: the count is of what the database keeps of this query.
    hir::attach_db(&db, || {
        db.monomorphized_runtime_mir_body(root.into(), args.clone(), environment.clone()).unwrap();
    });
    assert_eq!(kept(&mut db), 2);
}

#[test]
fn startup_shim_reuses_code_and_rebinds_native_calls() {
    if std::env::var_os(CHILD).is_none() {
        let directory = std::env::temp_dir().join(format!("blaze-startup-{}", std::process::id()));
        std::fs::create_dir(&directory).unwrap();
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", TEST, "--nocapture"])
            .env(CHILD, &directory)
            .env("RA_JIT_CACHE", directory.join("store"))
            .env("RA_JIT_CACHE_SEAL_AT", "1")
            .env("RUST_BACKTRACE", "0")
            .env_remove("RA_JIT_CACHE_CHECK")
            .output()
            .unwrap();
        std::fs::remove_dir_all(directory).unwrap();
        assert!(
            output.status.success(),
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }
    store::compiles_against("1.99.0");
    let source = r#"
//- minicore: sized, fn, copy
//- /src/main.rs crate:startup
#![feature(lang_items)]
#[lang = "start"]
pub fn lang_start<T>(main: fn() -> T, _: isize, _: *const *const u8, _: u8) -> isize {
    main();
    unsafe { startup_result() }
}
unsafe extern "C" { fn startup_result() -> isize; }
pub fn main() {}
"#;
    let (first, compiled) = build(source, "main", true, false);
    assert!(compiled >= 3, "the first build must compile main, startup and lang_start");
    let (reused, compiled) = build(source, "main", true, false);
    assert_eq!(compiled, 0, "an equivalent fresh database must reuse the startup shim");
    let (changed, compiled) =
        build(&source.replace("main();", "main(); main();"), "main", true, false);
    assert!(compiled >= 3, "a changed required source identity must invalidate startup code");

    let directory = std::path::PathBuf::from(std::env::var_os(CHILD).unwrap());
    for (name, bytes, result) in
        [("first", first, 17), ("reused", reused, 29), ("changed", changed, 31)]
    {
        let object = directory.join(format!("{name}.o"));
        let native = directory.join(format!("{name}.c"));
        let program = directory.join(name);
        std::fs::write(&object, bytes).unwrap();
        std::fs::write(
            &native,
            format!("#include <stdint.h>\nintptr_t startup_result(void) {{ return {result}; }}\nint rust_eh_personality(void) {{ return 0; }}\n"),
        )
        .unwrap();
        let linked = std::process::Command::new("cc")
            .arg(&object)
            .arg(&native)
            .arg("-o")
            .arg(&program)
            .output()
            .unwrap();
        assert!(linked.status.success(), "{}", String::from_utf8_lossy(&linked.stderr));
        assert_eq!(std::process::Command::new(program).status().unwrap().code(), Some(result));
    }
}

#[test]
fn allocator_shim_calls_remain_native_bindings_in_cached_code() {
    const TEST: &str = "startup_tests::allocator_shim_calls_remain_native_bindings_in_cached_code";
    if std::env::var_os(CHILD).is_none() {
        let directory =
            std::env::temp_dir().join(format!("blaze-allocator-cache-{}", std::process::id()));
        std::fs::create_dir(&directory).unwrap();
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", TEST, "--nocapture"])
            .env(CHILD, &directory)
            .env("RA_JIT_CACHE", directory.join("store"))
            .env("RA_JIT_CACHE_SEAL_AT", "1")
            .env("RA_JIT_KEYS_LOG", directory.join("keys.log"))
            .env("RUST_BACKTRACE", "0")
            .env_remove("RA_JIT_CACHE_CHECK")
            .output()
            .unwrap();
        std::fs::remove_dir_all(directory).unwrap();
        assert!(
            output.status.success(),
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }
    store::compiles_against("1.99.0");
    let source = r#"
//- minicore: sized
//- /src/main.rs crate:allocator deps:runtime
#![feature(lang_items)]
#[lang = "owned_box"]
pub struct Box<T>(T);
pub extern "C" fn root() -> usize { unsafe { *runtime::__rust_alloc(16, 8) as usize } }
//- /runtime.rs crate:runtime library
unsafe extern "C" { pub fn __rust_alloc(size: usize, align: usize) -> *mut u8; }
"#;
    let (first, compiled) = build(source, "root", false, true);
    assert_eq!(compiled, 1);
    let (reused, compiled) = build(source, "root", false, true);
    assert_eq!(compiled, 0, "a locally defined native shim has no Rust body entry to require");
    let directory = std::path::PathBuf::from(std::env::var_os(CHILD).unwrap());
    for (name, bytes, result) in [("first", first, 17), ("reused", reused, 29)] {
        let object = directory.join(format!("{name}.o"));
        let native = directory.join(format!("{name}.c"));
        let program = directory.join(name);
        std::fs::write(&object, bytes).unwrap();
        std::fs::write(
            &native,
            format!(
                "#include <stdint.h>\n#include <stdlib.h>\n\
                 _Alignas(16) static unsigned char allocation[16] = {{ {result} }};\n\
                 void *__rdl_alloc(uintptr_t size, uintptr_t align) {{ return allocation; }}\n\
                 void __rdl_dealloc(void *ptr, uintptr_t size, uintptr_t align) {{ }}\n\
                 void *__rdl_realloc(void *ptr, uintptr_t size, uintptr_t align, uintptr_t new_size) {{ return ptr; }}\n\
                 void *__rdl_alloc_zeroed(uintptr_t size, uintptr_t align) {{ return 0; }}\n\
                 void __rdl_alloc_error_handler(uintptr_t size, uintptr_t align) {{ abort(); }}\n\
                 int rust_eh_personality(void) {{ return 0; }}\n\
                 extern uintptr_t fixture_root(void);\n\
                 int main(void) {{ return (int)fixture_root(); }}\n"
            ),
        )
        .unwrap();
        let linked = std::process::Command::new("cc")
            .arg(&object)
            .arg(&native)
            .arg("-o")
            .arg(&program)
            .output()
            .unwrap();
        assert!(linked.status.success(), "{}", String::from_utf8_lossy(&linked.stderr));
        assert_eq!(std::process::Command::new(program).status().unwrap().code(), Some(result));
    }
    let missing = directory.join("missing.c");
    std::fs::write(
        &missing,
        "#include <stdint.h>\nextern uintptr_t fixture_root(void);\nint main(void) { return (int)fixture_root(); }\n",
    )
    .unwrap();
    let linked = std::process::Command::new("cc")
        .arg(directory.join("reused.o"))
        .arg(missing)
        .arg("-o")
        .arg(directory.join("missing"))
        .output()
        .unwrap();
    assert!(!linked.status.success(), "cached code still requires its native definitions");
    assert!(String::from_utf8_lossy(&linked.stderr).contains("__rdl_alloc"));
}
