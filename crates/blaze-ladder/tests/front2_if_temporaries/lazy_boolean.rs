//! Lazy Boolean operands are temporary scopes (Reference, "Destructors", "Temporary
//! scopes"). PR 85's source protocol exposed a shared opaque-iterator temporary kept until
//! an entire `if` ended, falsely conflicting with the next operand's two-phase mutation.
//! Check the real frontend's borrow verdicts and execute the independent Drop-order oracle.

use blaze_ladder::crates::{Inputs, UserCrate};
use blaze_ladder::front2::project::{BodyRef, ProjectWorld};
use blaze_ladder::refusal::SourceFile;
use blaze_ladder::run::{FrontChoice, NewFront, RunError, run_with};
use blaze_ladder::trap::TrapCode;
use blaze_own::Vec;
use blaze_toolchain::Shelf;

const SOURCE: &str = r#"
struct State(u8);
struct Guard<'a>(&'a u8);
impl Drop for Guard<'_> { fn drop(&mut self) { assert_eq!(*self.0, 0); } }
impl Guard<'_> {
    fn test(&self) -> bool { *self.0 == 255 }
    fn get(&self) -> Option<u8> { Some(*self.0) }
}
impl State {
    fn view(&self) -> Guard<'_> { Guard(&self.0) }
    fn bump(&mut self) -> bool { self.0 += 1; true }
}
fn boolean_value(s: &mut State) -> bool { false || s.view().test() || s.bump() }
fn boolean_condition(s: &mut State) -> bool {
    if false || s.view().test() || s.bump() { true } else { false }
}
fn and_operands(s: &mut State) -> bool { true && !s.view().test() && s.bump() }
fn let_chain_boolean_link(s: &mut State) -> u8 {
    if let Some(value) = Some(1u8) && !s.view().test() && s.bump() { value } else { 0 }
}
fn while_boolean_link(s: &mut State) {
    while !s.view().test() && s.bump() { break; }
}

struct Jobs { value: u8 }
impl Jobs {
    fn committed(&self) -> Result<Option<impl Iterator<Item = &u8>>, ()> {
        Ok(Some(core::iter::once(&self.value)))
    }
    fn lease(&mut self, out: &mut bool) -> Result<(), ()> {
        self.value += 1;
        *out = true;
        Ok(())
    }
}
struct Client { jobs: Jobs, out: bool }
impl Client { fn join(&mut self) -> bool { false } }
fn opaque_query_then_lease(c: &mut Client) -> bool {
    if c.join() || c.jobs.committed().unwrap().is_some() || c.jobs.lease(&mut c.out).is_ok() {
        true
    } else {
        false
    }
}

fn named_guard_stays_live(s: &mut State) -> bool {
    let held = s.view();
    let answer = false || held.test() || s.bump();
    drop(held);
    answer
}
fn accept(a: bool, b: bool) -> bool { a || b }
fn eager_arguments_keep_the_guard(s: &mut State) -> bool {
    accept(s.view().test(), s.bump())
}
fn pattern_scrutinee_stays_live(s: &mut State) -> u8 {
    if let Some(value) = s.view().get() && s.bump() { value } else { 0 }
}
fn main() {}
"#;

fn check_verdicts(cases: &[(&str, bool)]) {
    let heap = blaze_own::testing::heap();
    let alloc = heap.alloc();
    let tc = super::toolchain();
    let root = "/lazy/src/main.rs";
    let files = [(root.to_owned(), SOURCE.to_owned())];
    let user = UserCrate {
        name: "lazy",
        dir: "/lazy/",
        root,
        files: &files,
        edition: edition::PackageEdition::of_manifest("2024").expect("Rust 2024"),
        panic: blaze_build::Panic::Unwind,
    };
    let inputs = Inputs::new(&user, &tc).expect("fixture inputs");
    let crates = inputs.crates();
    let names = inputs.resolve(&crates, blaze_own::testing::bump()).expect("fixture names");
    let sources = [SourceFile { path: "src/main.rs", text: SOURCE }];
    let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("checker frame");
    let mut world = ProjectWorld::new(&names, &frame, &sources, alloc).expect("body checker");
    let mut verdicts = Vec::new_in(alloc);
    for &(name, want_conflict) in cases {
        let key = names.items(0).find_map(|(key, _, _)| (names.item_text(key)?.name == Some(name)).then_some(key)).expect("fixture body");
        let (types, borrows) = world.check_borrows(BodyRef::Free(key), alloc);
        println!("{name}: types clean {}, {borrows:?}", types.is_clean());
        assert!(types.is_clean(), "{name}'s types: {types:?}");
        verdicts.push((name, want_conflict, borrows));
    }
    for (name, want_conflict, borrows) in verdicts {
        assert!(borrows.refused.is_none(), "{name} was not checked: {borrows:?}");
        if want_conflict {
            assert!(borrows.findings.iter().any(|d| d.code == "E0502"), "{name} must retain the live shared-loan conflict: {borrows:?}");
        } else {
            assert!(borrows.findings.is_empty(), "{name}'s operand temporary must end before the next operand: {borrows:?}");
        }
    }
}

#[test]
fn lazy_boolean_operand_borrows_end_before_the_next_operand() {
    check_verdicts(&[
        ("boolean_value", false),
        ("boolean_condition", false),
        ("and_operands", false),
        ("let_chain_boolean_link", false),
        ("while_boolean_link", false),
        ("opaque_query_then_lease", false),
    ]);
}

#[test]
fn lazy_boolean_operand_scopes_preserve_genuinely_live_borrows() {
    check_verdicts(&[
        ("named_guard_stays_live", true),
        ("eager_arguments_keep_the_guard", true),
        ("pattern_scrutinee_stays_live", true),
    ]);
}

/// This executes generated ARM64 code, as the other `run_with` ladder tests do. Both this
/// wrapper and the hosted `blaze test` fixture use the installed core's `Cell` and `Drop`.
/// Checked overflow under the same wrapper must reach its panic handler's abort trap.
#[test]
fn lazy_boolean_operand_destructors_run_at_their_scope_boundaries() {
    const MAIN: &str = "#![feature(core_intrinsics)]
#![no_std]
#![no_main]
use core::ops::Drop;
mod case;
#[panic_handler]
fn panic(_: &core::panic::PanicInfo<'_>) -> ! {
    core::intrinsics::abort_immediate()
}
#[unsafe(no_mangle)]
pub extern \"C\" fn main() -> i32 { case::check() as i32 }
";
    const CASE: &str = include_str!("../../../blaze-cli/tests/ladder/front2/x-drop-13-lazy-booleans/src/case.rs");
    let files = [
        SourceFile { path: "src/main.rs", text: MAIN },
        SourceFile { path: "src/case.rs", text: CASE },
    ];
    let shelf = super::shelf();
    let new = NewFront { package: "lazy", edition: "2024", shelf: Some(shelf.place()), overflow: blaze_build::OverflowChecks::On };
    let got = run_with(&files, "src/main.rs", None, FrontChoice::New(new), blaze_own::testing::heap().shared());
    assert!(matches!(got, Ok(0)), "executed lazy-Boolean Drop oracle: {got:?}");

    // The same checked boundary must fail through core's panic entry, not return a wrapped
    // zero or accept a frontend refusal as evidence that the panic handler ran.
    const OVERFLOW: &str = "fn overflow(value: u8) -> u8 { value + 1 }
pub fn check() -> u8 { overflow(255) }
";
    let overflow_files = [
        SourceFile { path: "src/main.rs", text: MAIN },
        SourceFile { path: "src/case.rs", text: OVERFLOW },
    ];
    let ended = run_with(&overflow_files, "src/main.rs", None, FrontChoice::New(new), blaze_own::testing::heap().shared());
    assert!(
        matches!(
            &ended,
            Err(RunError::Trapped { trap })
                if matches!(trap.code(), TrapCode::Lir(code) if u32::from(code) == blaze_build::intrinsics::ABORT_TRAP)
        ),
        "checked u8 overflow must reach the wrapper's abort trap: {ended:?}",
    );
}
