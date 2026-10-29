use ide_db::{RootDatabase, base_db::all_crates};
use test_fixture::WithFixture;

fn checked(fixture: &str) -> hir::compiler_acceptance::Acceptance {
    let db = RootDatabase::with_files(fixture);
    let krates = all_crates(&db);
    let krate = krates
        .iter()
        .copied()
        .find(|krate| {
            hir::Crate::from(*krate)
                .display_name(&db)
                .is_some_and(|name| name.canonical_name().as_str() == "main")
        })
        .unwrap_or_else(|| {
            krates.iter().copied().find(|krate| krate.data(&db).origin.is_local()).unwrap()
        });
    hir::attach_db(&db, || hir::compiler_acceptance::check(&db, krate).clone())
}

#[test]
fn source_acceptance_checks_uncalled_signatures() {
    let result = checked("fn never_called(_: NoSuchType) {}\n");
    assert!(result.errors.iter().any(|error| error.contains("unresolved type")), "{result:?}");
    let result = checked("fn accepts(x: u32) -> u32 { x }\n");
    assert_eq!(result, Default::default());
}

#[test]
fn source_acceptance_checks_private_paths() {
    let result = checked("mod hidden { fn private() {} } fn caller() { hidden::private(); }\n");
    assert!(result.errors.iter().any(|error| error.contains("private item")), "{result:?}");
    let result =
        checked("mod hidden { pub(crate) fn public() {} } fn caller() { hidden::public(); }\n");
    assert_eq!(result, Default::default());
}

#[test]
fn source_acceptance_checks_external_method_stability() {
    let fixture = r#"
//- /main.rs crate:main deps:library
fn caller(value: &library::Value) { value.METHOD(); }
//- /lib.rs crate:library
pub struct Value;
impl Value {
    #[unstable(feature = "value_unstable", issue = "none")]
    pub fn unstable_method(&self) {}
    pub fn stable_method(&self) {}
}
"#;
    let unstable = checked(&fixture.replace("METHOD", "unstable_method"));
    assert!(
        unstable.errors.iter().any(|error| error.contains("unstable library item")),
        "{unstable:?}"
    );
    let stable = checked(&fixture.replace("METHOD", "stable_method"));
    assert_eq!(stable, Default::default());
}

#[test]
fn source_acceptance_checks_dyn_signatures() {
    let result = checked("trait NotDyn { fn generic<T>(&self); } fn caller(_: &dyn NotDyn) {}\n");
    assert!(result.errors.iter().any(|error| error.contains("not dyn compatible")), "{result:?}");
}

#[test]
fn source_acceptance_checks_constant_calls() {
    for fixture in [
        "fn runtime() -> u8 { 1 } const X: u8 = runtime();\n",
        "fn runtime() -> usize { 1 } fn uncalled(_: [u8; runtime()]) {}\n",
        "fn runtime() -> u8 { 1 } fn caller() { let _ = const { runtime() }; }\n",
    ] {
        let result = checked(fixture);
        assert!(
            result.errors.iter().any(|error| error.contains("non-const function")),
            "{result:?}"
        );
    }
    let valid = checked("const fn constant() -> u8 { 1 } const X: u8 = constant();\n");
    assert_eq!(valid, Default::default());
}

#[test]
fn source_acceptance_respects_cfg_and_declaration_only_items() {
    let result = checked(
        r#"
#[cfg(any())] fn inactive(_: NoSuchType) {}
trait Declared { fn method(&self); const N: u8; }
enum Implicit { First, Second }
fn identity<T>(value: T) -> T { value }
"#,
    );
    assert_eq!(result, Default::default());
}

#[test]
fn source_acceptance_rejects_unreachable_constant_overflow() {
    for fixture in [
        "const NEVER_READ: u8 = 255u8 + 1u8;\n",
        "const fn increment(value: u8) -> u8 { value + 1 } const NEVER_READ: u8 = increment(255);\n",
    ] {
        let result = checked(fixture);
        assert!(!result.errors.is_empty(), "{result:?}");
    }
    let runtime = checked("fn increment(value: u8) -> u8 { value + 1 }\n");
    assert_eq!(runtime, Default::default());
}

#[test]
fn source_acceptance_checks_unused_private_imports() {
    let result = checked("mod hidden { fn private() {} } use hidden::private;\n");
    assert!(result.errors.iter().any(|error| error.contains("private item")), "{result:?}");
}

#[test]
fn source_acceptance_checks_macro_generated_signatures() {
    let result = checked(
        r#"
macro_rules! declare { () => { fn generated(_: NoSuchType) {} } }
declare!();
"#,
    );
    assert!(result.errors.iter().any(|error| error.contains("unresolved type")), "{result:?}");
}

#[test]
fn source_acceptance_checks_const_drops() {
    let result = checked(
        r#"
//- minicore: drop, sized
struct Owned;
impl Drop for Owned { fn drop(&mut self) {} }
const fn invalid(value: Owned) {}
"#,
    );
    assert!(result.errors.iter().any(|error| error.contains("destructor")), "{result:?}");
}

#[test]
fn source_acceptance_checks_constant_casts_without_treating_closures_as_constants() {
    let invalid = checked("const fn address(value: *const u8) -> usize { value as usize }\n");
    assert!(invalid.errors.iter().any(|error| error.contains("pointer")), "{invalid:?}");
    let valid = checked("fn runtime() {} const fn make_closure() { let _ = || runtime(); }\n");
    assert_eq!(valid, Default::default());
}

#[test]
fn source_acceptance_defers_unused_inline_constant_evaluation() {
    let result = checked(
        "const fn increment(value: u8) -> u8 { value + 1 } fn uncalled() { let _ = const { increment(255) }; }\n",
    );
    assert_eq!(result, Default::default());
}

#[test]
fn source_acceptance_checks_discarded_unresolved_expression_paths() {
    for fixture in [
        "mod known {} fn caller() { let _ = known::missing::VALUE; }\n",
        "fn caller() { let _ = missing; }\n",
        r#"
//- /main.rs crate:main deps:library
pub fn caller() { let _ = library::missing::VALUE; }
//- /lib.rs crate:library
pub const VALUE: u8 = 1;
"#,
    ] {
        let result = checked(fixture);
        assert!(
            result.errors.iter().any(|error| error.contains("unresolved expression")),
            "{result:?}"
        );
    }
    for fixture in [
        "mod known { pub const VALUE: u8 = 1; } fn caller() { let _ = known::VALUE; }\n",
        "trait HasValue { const VALUE: u8; } fn caller<T: HasValue>() { let _ = T::VALUE; }\n",
        "fn caller(value: u8) { let _ = value; }\n",
    ] {
        assert_eq!(checked(fixture), Default::default());
    }
}

#[test]
fn source_acceptance_rejects_a_name_bound_twice_in_one_namespace() {
    let library =
        "//- /lib.rs crate:library\npub struct Value { pub field: u32 }\npub struct Unit;\n";
    let probe =
        |main: &str| checked(&format!("//- /main.rs crate:main deps:library\n{main}\n{library}"));
    for twice in [
        "use library::Value;\nuse library::Value;",
        "use library::{Value, Value};",
        "use library::Value;\nstruct Value;",
        "use library::Unit;\nfn Unit() {}",
        "mod m {}\nmod m {}",
        "fn f() {}\nfn f() {}",
        "struct S;\nfn S() {}",
        "extern crate library;\nextern crate library;",
        "extern crate library;\nuse library::Value as library;",
        "mod inner { use library::Value; pub struct Value; }",
    ] {
        let result = probe(twice);
        assert!(
            result.errors.iter().any(|error| error.contains("defined multiple times")),
            "{twice}: {result:?}"
        );
    }
    for once in [
        "use library::Value;",
        "use library::Value;\nfn Value() {}",
        "struct S { x: u32 }\nfn S() {}",
        "use library::Value as _;\nuse library::Value as _;",
        "use library::*;\nstruct Value;",
        "macro_rules! m { () => {} }\nmacro_rules! m { () => {} }",
        "use library::{self as lib, Value};",
        "mod a { pub struct Value; }\nmod b { pub struct Value; }",
    ] {
        let result = probe(once);
        assert!(
            !result.errors.iter().any(|error| error.contains("defined multiple times")),
            "{once}: {result:?}"
        );
    }
}

#[test]
fn source_acceptance_normalizes_raw_imports_and_aliases_in_each_namespace() {
    let library = "//- /lib.rs crate:library\npub struct Value { pub field: u32 }\npub struct Unit;\n#[macro_export] macro_rules! make { () => {} }\n";
    let probe =
        |main: &str| checked(&format!("//- /main.rs crate:main deps:library\n{main}\n{library}"));
    for twice in [
        "struct Value { x: u8 } use self::r#Value;",
        "struct r#Value { x: u8 } use self::Value;",
        "struct r#Value { x: u8 } use self::r#Value;",
        "use library::{Value, r#Value};",
        "use library::{r#Value, Value};",
        "use library::Value as r#Alias; use library::Value as Alias;",
        "use library::r#Value as Alias; use library::Value as r#Alias;",
        "use library::Value as r#Alias; struct Alias { x: u8 }",
        "mod value {} use self::r#value::{self};",
        "mod value {} use self::value::{self as r#value};",
        "mod r#value {} use self::r#value::{self as value};",
        "use library::{Unit, r#Unit};",
        "use library::r#Unit; fn Unit() {}",
        "use library::{make, r#make};",
    ] {
        let result = probe(twice);
        assert!(
            result.errors.iter().any(|error| error.contains("defined multiple times")),
            "{twice}: {result:?}"
        );
    }
    for once in [
        "use library::r#Value; fn Value() {}",
        "use library::Value as r#Alias; fn Alias() {}",
        "use library::Value as r#Alias; use library::Value as Other;",
        "use library::r#Value as _; use library::Value as _;",
        "use library::*; struct r#Value;",
        "use library::make; use library::Value as r#make;",
        "macro_rules! r#make { () => {} } macro_rules! make { () => {} }",
        "use library::{self as r#lib, r#Value};",
    ] {
        let result = probe(once);
        assert_eq!(result, Default::default(), "{once}: {result:?}");
    }
}
