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

/// rustc rejects a literal given where a type written out wants a scalar it cannot be; any
/// other mismatch rests on rust-analyzer's coercions and inference, which may be wrong where
/// rustc accepts, so it is undecided: a refusal, never a "no".
#[test]
fn source_acceptance_rejects_only_certain_type_mismatches() {
    for fixture in [
        "pub fn value() -> u32 { \"wrong\" }\n",
        "pub fn value() -> u32 { { (true) } }\n",
        "pub fn value() { let x: bool = 1; }\n",
        "pub fn value() -> u8 { return 1.5; }\n",
        "pub fn value() -> f32 { 1 }\n",
        "pub const X: u8 = 'c';\n",
        "pub static Y: char = 2u8;\n",
    ] {
        let result = checked(fixture);
        assert!(
            result.errors.iter().any(|error| error.contains("mismatched types")),
            "{fixture}: {result:?}"
        );
    }
    for fixture in [
        "fn takes(_: u64) {} pub fn value() { takes(\"x\"); }\n",
        "pub struct S; pub fn value() -> S { 1 }\n",
        "pub fn value() -> u32 { let s = \"x\"; s }\n",
        "pub fn value() -> u32 { let f = || -> u32 { \"x\" }; f() }\n",
        "pub fn value() -> u16 { 1u8 + 1u8 }\n",
    ] {
        let result = checked(fixture);
        assert!(result.errors.is_empty(), "{fixture}: {result:?}");
        assert!(
            result
                .unsupported
                .iter()
                .any(|it| it.contains("type mismatch rust-analyzer cannot prove")),
            "{fixture}: {result:?}"
        );
    }
    for fixture in [
        "pub fn value() -> u64 { 1 }\n",
        "pub fn value() -> f64 { 1.0 }\n",
        "pub fn value() -> u8 { b'a' }\n",
        "pub const X: i128 = 7;\n",
    ] {
        assert_eq!(checked(fixture), Default::default(), "{fixture}");
    }
}

/// A body the borrow checker did not check is no answer: rustc may accept it, or reject it
/// for what kept the check from being made, so the probe is undecided unless an error that
/// decides it was found.
#[test]
fn source_acceptance_leaves_an_unchecked_body_undecided() {
    let result = checked(
        "pub struct S { pub a: u8 }\n\
         pub fn f(s: S) -> u8 { let a; S { a, ..s } = S { a: 1 }; a }\n",
    );
    assert!(
        result.unsupported.iter().any(|it| it.contains("borrow checking not done")),
        "{result:?}"
    );
    let result = checked("pub fn value() -> u32 { let f = 1u8; f }\n");
    assert!(
        result.unsupported.iter().any(|it| it.contains("borrow checking not done")),
        "{result:?}"
    );
}

/// A function must have a body but in a trait or an `extern` block.
#[test]
fn source_acceptance_rejects_a_function_without_a_body() {
    for fixture in ["pub fn f();\n", "pub struct S;\nimpl S { pub fn f(); }\n"] {
        let result = checked(fixture);
        assert!(result.errors.iter().any(|it| it.contains("without a body")), "{result:?}");
    }
    let declared = checked("pub trait T { fn f(); }\nextern \"C\" { pub fn g(); }\n");
    assert_eq!(declared, Default::default());
}

/// A method of a trait's impl is written without `pub`, and is as visible as the trait.
#[test]
fn source_acceptance_takes_trait_impl_items_as_visible_as_their_trait() {
    let fixture = r#"
//- /main.rs crate:main deps:library
use library::Named;
pub fn caller() -> u8 { 1u8.name() + <u16 as Named>::NAME }
//- /lib.rs crate:library
pub trait Named { const NAME: u8; fn name(&self) -> u8; }
impl Named for u8 { const NAME: u8 = 1; fn name(&self) -> u8 { 1 } }
impl Named for u16 { const NAME: u8 = 2; fn name(&self) -> u8 { 2 } }
"#;
    assert_eq!(checked(fixture), Default::default());
    let bounded = r#"
//- /main.rs crate:main deps:library
pub fn total<T: library::Named>(v: &T) -> u8 { v.name() }
pub fn unknown<T: library::Absent>(_: T) {}
//- /lib.rs crate:library
pub trait Named { fn name(&self) -> u8; }
"#;
    let result = checked(bounded);
    assert_eq!(
        result.errors,
        ["unresolved trait `library::Absent`".to_owned()],
        "a bound names a trait, not a type: {result:?}"
    );
    let private = checked(
        "mod hidden { pub struct S; impl S { fn inherent(&self) {} } }\n\
         pub fn caller(s: hidden::S) { s.inherent(); }\n",
    );
    assert!(private.errors.iter().any(|error| error.contains("private item")), "{private:?}");
}
