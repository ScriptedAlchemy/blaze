use super::*;
use ide_db::RootDatabase;
use sha2::{Digest, Sha256};
use test_fixture::WithFixture;

const TEST: &str = "startup_tests::startup_shim_reuses_code_and_rebinds_native_calls";
const CHILD: &str = "BLAZE_STARTUP_CACHE_CHILD";

struct Symbols<'a> {
    db: &'a RootDatabase,
    identity: [u8; 32],
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
    let db = RootDatabase::with_files(source);
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
        let unit =
            CodegenUnit::compile(&db, &symbols, roots, &FxHashSet::default(), &mut known).unwrap();
        assert!(unit.skipped.is_empty(), "startup compile refused: {:?}", unit.skipped);
        store::Store::global().unwrap().seal_checked().unwrap();
        let compiled = compiled_functions() - before;
        assert_eq!(known.stats.compiled, compiled);
        (unit.object.unwrap(), compiled)
    })
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
