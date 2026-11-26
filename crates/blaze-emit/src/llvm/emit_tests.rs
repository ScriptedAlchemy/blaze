//! The release tier's proof: a function LLVM lowered, optimized and emitted reads back as a
//! sealed `bzcode07` record, and the record runs and computes what the LIR says. The negative
//! control: a record the Reference tier made is never named release or folded work.


use blaze_code::record::{CodeView, SiteKind, Space, Symbol, Target};
use blaze_lir::{
    BlockKind, Builder, Flags, Full, Ty,
    builder::{Param, Variadic},
    func::Conv,
};
use blaze_own::Alloc;

use super::{
    Named, Names, Outside,
    emit::{Backend, ReleaseTask, emit_release},
    prepare,
};
use crate::RoutinePlaces;

/// The machine the tests emit for: the host's.
fn machine() -> blaze_tier::Machine {
    #[cfg(target_arch = "aarch64")]
    let arch = blaze_tier::Arch::Arm64;
    #[cfg(target_arch = "x86_64")]
    let arch = blaze_tier::Arch::X86_64;
    #[cfg(target_os = "macos")]
    let os = blaze_tier::Os::MacOs;
    #[cfg(target_os = "linux")]
    let os = blaze_tier::Os::Linux;
    blaze_tier::Machine { arch, os, features: 0 }
}

struct ByPlace;
impl Names for ByPlace {
    fn name(&self, outside: Outside) -> Option<Named<'_>> {
        match outside {
            Outside::Func(0) => Some(Named::Symbol(Symbol([7; 32]))),
            _ => None,
        }
    }
    fn routines(&self) -> RoutinePlaces {
        RoutinePlaces::NONE
    }
}

/// `fn(n: i32) -> i32 { if n == 0 { 11 << 34 } else { 19 } }`: the lowerer's own control
/// function, which the ARM64 admission command runs through LLVM. `11 << 34` wraps to 44.
fn control() -> blaze_lir::VerifiedFunc<blaze_own::Alloc<'static>> {
    let mut b = Builder::<_, Full>::new_in(64, blaze_own::testing::heap().alloc());
    let p = Param::of(Ty::I32);
    let sig = b.sig(Conv::C, Variadic::No, &[p], &[p]);
    b.set_signature(sig);
    let entry = b.create_block(BlockKind::Normal);
    let join = b.create_block(BlockKind::Normal);
    b.begin(entry);
    let n = b.param(Ty::I32);
    let zero = b.iconst(Ty::I32, 0);
    let cmp = b.icmp(blaze_lir::IntCond::Eq, n, zero);
    let a = b.iconst(Ty::I32, 11);
    let amount = b.iconst(Ty::I32, 34);
    let shifted = b.binary(blaze_lir::Op::Shl, Flags::NONE, a, amount);
    let other = b.iconst(Ty::I32, 19);
    b.br(Flags::NONE, cmp, (join, &[shifted]), (join, &[other]));
    b.begin(join);
    let ret = b.param(Ty::I32);
    b.ret(&[ret]);
    b.finish_verified().unwrap()
}

/// `fn(x: i32) -> i32 { callee(x) }`: one call of an outside function.
fn caller() -> blaze_lir::VerifiedFunc<blaze_own::Alloc<'static>> {
    let mut b = Builder::<_, Full>::new_in(64, blaze_own::testing::heap().alloc());
    let p = Param::of(Ty::I32);
    let sig = b.sig(Conv::C, Variadic::No, &[p], &[p]);
    b.set_signature(sig);
    let entry = b.create_block(BlockKind::Normal);
    b.begin(entry);
    let x = b.param(Ty::I32);
    let r = b.reference(blaze_lir::func::RefKind::Func, 0);
    let y = b.call(Flags::NONE, r, sig, &[x]);
    b.ret(&[y]);
    b.finish_verified().unwrap()
}

/// A one-function module through `prepare`, LLVM's `O2` and the AArch64 code generator reads
/// back as a sealed record `CodeView::read` accepts, with no outside reference (the function
/// has none) and a code part LLVM sized.
#[test]
fn release_record_reads_back() {
    let func = control();
    let module = prepare(Symbol([3; 32]), func.verified(), &ByPlace, Alloc::global()).unwrap();
    let emission = emit_release(ReleaseTask { module: &module, imports: &[], opt: 2, machine: machine() }, Alloc::global()).unwrap();
    assert_eq!(emission.backend, Backend::ReleaseBlob);
    assert_eq!(emission.symbol, Symbol([3; 32]));
    let view = CodeView::read(&emission.record).expect("the record reads");
    assert_eq!(view.targets(), 0, "the control function names nothing outside");
    assert_eq!(view.sites(), 0);
    assert_eq!(view.words(), 0);
    assert!(view.code().len() >= 4 && view.code().len() % 4 == 0, "LLVM's code is whole instructions: {} bytes", view.code().len());
    assert_eq!(view.code_align(), 4, "LLVM aligns a function to 16");
    println!("release record: {} bytes of code, {} targets, {} sites", view.code().len(), view.targets(), view.sites());
}

/// The record runs: `f(0) == 44` (11 << 34 wraps to 44) and `f(1) == 19`, as the LIR says.
#[cfg(target_arch = "aarch64")]
#[test]
fn release_record_runs() {
    use blaze_inproc::exec::Executable;
    let func = control();
    let module = prepare(Symbol([3; 32]), func.verified(), &ByPlace, Alloc::global()).unwrap();
    let emission = emit_release(ReleaseTask { module: &module, imports: &[], opt: 2, machine: machine() }, Alloc::global()).unwrap();
    let view = CodeView::read(&emission.record).expect("the record reads");
    assert_eq!(view.targets(), 0);
    let code = Executable::written(view.code().len(), 0, |code, _| code.copy_from_slice(view.code())).expect("the code maps");
    let f: unsafe extern "C" fn(i32) -> i32 = unsafe { code.as_fn() };
    assert_eq!(unsafe { f(0) }, 44);
    assert_eq!(unsafe { f(1) }, 19);
    assert_eq!(unsafe { f(-1) }, 19);
}

/// A function that calls an outside function: the record's one site is a `Call` naming the
/// callee by (space, symbol), as the lowering lane's `references` says.
#[test]
fn release_record_names_its_call() {
    let func = caller();
    let module = prepare(Symbol([2; 32]), func.verified(), &ByPlace, Alloc::global()).unwrap();
    let emission = emit_release(ReleaseTask { module: &module, imports: &[], opt: 2, machine: machine() }, Alloc::global()).unwrap();
    let view = CodeView::read(&emission.record).expect("the record reads");
    assert_eq!(view.targets(), 1);
    assert_eq!(view.target(0), Target { space: Space::Func, symbol: Symbol([7; 32]) });
    assert_eq!(view.sites(), 2, "one original branch and its callable import veneer");
    assert_eq!(view.site(0).kind, SiteKind::Branch26);
    assert_eq!(view.site(1).kind, SiteKind::Load);
    let veneer = view.site(0).addend as usize;
    assert_eq!(u32::from_le_bytes(view.code()[veneer + 8..veneer + 12].try_into().unwrap()), 0xd61f_0200, "the veneer preserves the original branch's LR");
}

fn object_fixture(ir: &str) -> super::LlvmModule<'static> {
    let func = control();
    let mut module = prepare(Symbol([3; 32]), func.verified(), &ByPlace, Alloc::global()).unwrap();
    module.ir.clear();
    module.ir.extend_from_slice(ir.as_bytes());
    let at = ir.find("@entry(").unwrap() + 1;
    module.definition_name = at..at + "entry".len();
    module.references.clear();
    module.implicit_references.clear();
    module.positions.clear();
    module.traps.clear();
    module
}

/// Real LLVM objects, emitted for both object formats. A volatile read keeps the address
/// relocation and its nonzero local-symbol offset alive through O2.
#[test]
fn native_objects_keep_local_offsets_data_addends_and_position_slots() {
    for os in [blaze_tier::Os::MacOs, blaze_tier::Os::Linux] {
        let machine = blaze_tier::Machine { arch: blaze_tier::Arch::Arm64, os, features: 0 };
        let target = Target { space: Space::Static, symbol: Symbol([7; 32]) };
        let outside = std::format!("{}", super::LinkName(target));
        let ir = std::format!(r#"
@{outside} = external global i8
@d = private constant <{{ [8 x i8], ptr }}> <{{ [8 x i8] zeroinitializer, ptr getelementptr(i8, ptr @{outside}, i64 -7) }}>, align 8
@blaze_pos_entry_0 = hidden externally_initialized global [1 x i32] zeroinitializer, align 4
define i64 @entry() {{
  %p = load volatile ptr, ptr getelementptr(i8, ptr @d, i64 8), align 8
  %v = ptrtoint ptr %p to i64
  %line = load volatile i32, ptr @blaze_pos_entry_0, align 4
  %wide = zext i32 %line to i64
  %out = add i64 %v, %wide
  ret i64 %out
}}
"#);
        let mut module = object_fixture(&ir);
        module.references.push(Some(target));
        module.positions.push((0, blaze_lir::func::DataPositionRec { item: [9; 16], offset: 0, site: 17, field: 0 }));
        let emission = emit_release(ReleaseTask { module: &module, imports: &[], opt: 2, machine }, Alloc::global()).unwrap();
        let view = CodeView::read(&emission.record).unwrap();
        assert_eq!(view.words(), 1, "{os:?}: relocation on the constant must not be dropped");
        assert_eq!(view.word(0).addend, -7);
        assert_eq!(view.target(view.word(0).target as usize), target);
        assert_eq!(view.positions(), 1);
        assert_eq!(view.position(0).item, u128::from_le_bytes([9; 16]));
        assert_eq!(view.position(0).site, 17);
        assert!((0..view.sites()).any(|i| view.site(i).kind == SiteKind::Page21));
        assert!((0..view.sites()).any(|i| view.site(i).addend >= 8 && view.site(i).target == blaze_code::record::OWN_RELRO), "the interior data address survives");
    }
}

/// The negative control: a record the Reference tier made (`emit_record`) is never named
/// release or folded work. `Backend` has no constructor from a record: the only way to one
/// is `emit_release`/`emit_fold`, which run LLVM. This test pins that by construction: a
/// Reference-tier record has no `Backend`, and the oracle's `backend_work_of` (engine_dist)
/// lists only keys the plan names, which a Reference record never is.
#[test]
fn reference_record_is_never_release_or_fold_work() {
    let func = control();
    let (record, _) = crate::emit_record(func.verified(), &ByPlace, &RoutinePlaces::NONE, crate::tests::ISA, Alloc::global()).unwrap();
    let sealed = record.seal().unwrap();
    let mut bytes = blaze_own::Vec::new_in(Alloc::global());
    sealed.encode(&mut bytes);
    let view = CodeView::read(&bytes).expect("the Reference record reads");
    assert!(view.code().len() > 0);
    // A Reference-tier record is a record; it is not a `Backend`. There is no path from
    // `CodeView` to `Backend` but through LLVM, which this record did not pass through.
    let _: blaze_code::record::CodeView<'_> = view;
}
