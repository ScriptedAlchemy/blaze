use blaze_ladder::refusal::SourceFile;
use blaze_ladder::run::{FrontChoice, NewFront, run_with};

const PRELUDE: &str = include_str!("../../blaze-cli/tests/ladder/front2/x0-async-01/src/prelude.rs");

fn run(source: &str) -> Result<u8, String> {
    let text = format!("#![feature(no_core, lang_items, intrinsics, rustc_attrs, unboxed_closures, auto_traits)]\n#![no_core]\n#![no_main]\nmod prelude; use prelude::*;\n{source}");
    let files = [SourceFile { path: "src/main.rs", text: &text }, SourceFile { path: "src/prelude.rs", text: PRELUDE }];
    let front = NewFront { package: "ladder", edition: "2024", shelf: None, overflow: blaze_build::OverflowChecks::On };
    run_with(&files, "src/main.rs", None, FrontChoice::New(front), blaze_own::testing::heap().shared()).map_err(|e| format!("{e}"))
}

#[test]
fn thin_and_wide_marker_coercions_retain_the_concrete_layout() {
    let source = r#"
auto trait Mark {}
auto trait Extra {}
trait Trait {}
impl Trait for u64 {}
#[rustc_intrinsic] pub const unsafe fn size_of_val<T: PointeeSized>(p: *const T) -> usize;
#[rustc_intrinsic] pub const unsafe fn align_of_val<T: PointeeSized>(p: *const T) -> usize;
fn size(m: &dyn Mark) -> usize { unsafe { size_of_val(m) } }
fn alignment(m: &dyn Mark) -> usize { unsafe { align_of_val(m) } }
#[unsafe(no_mangle)] pub extern "C" fn main() -> i32 {
    let small: u16 = 1;
    let large: u64 = 2;
    let markers: &(dyn Mark + Extra) = &small;
    let principal: &(dyn Trait + Mark) = &large;
    (size(markers) + alignment(markers) + size(principal) + alignment(principal)) as i32
}
"#;
    assert_eq!(run(source), Ok(20));
}

#[test]
fn assignment_after_a_direct_variant_move_drops_each_field_once() {
    let source = r#"
struct Count { out: *mut u32 }
impl Drop for Count { fn drop(&mut self) { unsafe { *self.out += 1; } } }
enum E { Empty, Pair(Count, Count) }
#[unsafe(no_mangle)] pub extern "C" fn main() -> i32 {
    let mut first = 0u32;
    let mut second = 0u32;
    {
        let mut e = E::Empty;
        let mut i = 0u32;
        loop {
            e = E::Pair(Count { out: &mut first }, Count { out: &mut second });
            if let E::Pair(x, _) = e { drop(x); }
            i += 1;
            if i == 2 { break; }
        }
    }
    (10 * first + second) as i32
}
"#;
    assert_eq!(run(source), Ok(22));
}
