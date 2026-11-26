//! alloc's Vec::extend_trusted moves a guard into a closure via its whole method receiver.
//! Its counter must be published by Drop after calls. A direct Copy-field capture is
//! intentionally disjoint and leaves the guard outside (Rust 2021); preserve that control.
use blaze_ladder::refusal::SourceFile;
use blaze_ladder::run::{FrontChoice, NewFront, run_with};

const PRELUDE: &str = include_str!("../../blaze-cli/tests/ladder/front2/x0-async-01/src/prelude.rs");

fn run(body: &str) -> Result<u8, String> {
    let text = format!(r#"#![feature(no_core, lang_items, intrinsics, rustc_attrs, unboxed_closures)]
#![no_core]
#![no_main]
mod prelude;
use prelude::*;
struct Guard {{ output: *mut u32, count: u32 }}
impl Drop for Guard {{ fn drop(&mut self) {{ unsafe {{ *self.output = self.count; }} }} }}
impl Guard {{ fn increment(&mut self) {{ self.count += 1; }} }}
fn twice<F: FnMut()>(mut f: F) {{ f(); f(); }}
#[unsafe(no_mangle)]
pub extern "C" fn main() -> i32 {{ {body} }}
"#);
    let files = [SourceFile { path: "src/main.rs", text: &text }, SourceFile { path: "src/prelude.rs", text: PRELUDE }];
    let new = NewFront { package: "ladder", edition: "2024", shelf: None, overflow: blaze_build::OverflowChecks::On };
    run_with(&files, "src/main.rs", None, FrontChoice::New(new), blaze_own::testing::heap().shared()).map_err(|e| format!("{e}"))
}

#[test]
fn move_closure_publishes_modified_whole_receiver_through_ancestor_drop() {
    assert_eq!(run("let mut out = 0u32; { let mut g = Guard { output: &mut out as *mut u32, count: 0 }; twice(move || g.increment()); } out as i32"), Ok(2));
}

#[test]
fn borrowed_copy_field_keeps_ancestor_in_its_original_scope() {
    assert_eq!(run("let mut out = 0u32; { let mut g = Guard { output: &mut out as *mut u32, count: 0 }; twice(|| g.count += 1); } out as i32"), Ok(2));
}

#[test]
fn copy_field_of_an_ancestor_without_drop_stays_a_disjoint_capture() {
    assert_eq!(run("struct Plain { count: u32, other: u32 } let mut g = Plain { count: 0, other: 7 }; twice(move || g.count += 1); g.other as i32"), Ok(7));
}

#[test]
fn direct_copy_field_capture_does_not_move_its_drop_ancestor() {
    assert_eq!(run("let mut out = 0u32; { let mut g = Guard { output: &mut out as *mut u32, count: 0 }; let f = move || g.count += 1; g.count = 7; twice(f); } out as i32"), Ok(7));
}
