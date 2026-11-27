//! The release tier's proof: a function LLVM lowered, optimized and emitted reads back as a
//! sealed code record, and the record runs and computes what the LIR says. The negative
//! control: a record the Reference tier made is never named release or folded work.

use blaze_code::{
    record::{CodeView, SiteKind, Space, Symbol, Target, TrapKind, TrapSource},
    trap::UNREACHABLE,
};
use blaze_lir::{
    BlockKind, Builder, Flags, Full, Ty,
    builder::{Param, Variadic},
    func::{Conv, FnAttrs, Inline},
};
use blaze_own::Alloc;

use super::{
    LlvmModule, Named, Names, Outside,
    emit::{Backend, ReleaseTask, emit_release, identity, identity_of},
    prepare,
};
use crate::RoutinePlaces;

/// The machine the tests emit for: the host's.
fn machine() -> blaze_tier::Machine {
    let arch = blaze_tier::Arch::Arm64;
    #[cfg(target_os = "macos")]
    let os = blaze_tier::Os::MacOs;
    #[cfg(target_os = "linux")]
    let os = blaze_tier::Os::Linux;
    blaze_tier::Machine { arch, os, features: 0 }
}

#[test]
fn native_objects_import_llvm_unwind_for_each_landing() {
    use blaze_code::unwind::Action;

    let alloc = blaze_own::testing::heap().alloc();
    for (kind, action) in [(BlockKind::Catch, Action::Catch), (BlockKind::Cleanup, Action::Cleanup), (BlockKind::Terminate, Action::Terminate)] {
        let mut b = Builder::<_, Full>::new_in(64, alloc);
        let ret = Param::of(Ty::I32);
        let entry_sig = b.sig(Conv::C, Variadic::No, &[], &[ret]);
        let call_sig = b.sig(Conv::C, Variadic::No, &[], &[]);
        b.set_signature(entry_sig);
        let entry = b.create_block(BlockKind::Normal);
        let normal = b.create_block(BlockKind::Normal);
        let landing = b.create_block(kind);
        let callee = b.reference(blaze_lir::func::RefKind::Func, 0);
        b.begin(entry);
        b.invoke(Flags::NONE, callee, call_sig, &[], normal, landing);
        b.begin(normal);
        b.call(Flags::NONE, callee, call_sig, &[]);
        let value = b.iconst(Ty::I32, 7);
        b.ret(&[value]);
        b.begin(landing);
        let exception = b.param(Ty::PTR);
        match kind {
            BlockKind::Catch => {
                let value = b.iconst(Ty::I32, 42);
                b.ret(&[value]);
            }
            BlockKind::Cleanup => {
                b.call(Flags::NONE, callee, call_sig, &[]);
                b.resume(exception);
            }
            BlockKind::Terminate => b.trap(123),
            _ => unreachable!(),
        }
        let function = b.finish_verified().unwrap();
        let module = prepare(Symbol([5; 32]), function.verified(), &ByPlace, alloc).unwrap();
        for (os, data_align) in [(blaze_tier::Os::Linux, -4), (blaze_tier::Os::MacOs, -8)] {
            let machine = blaze_tier::Machine { os, ..machine() };
            let emission = emit_release(ReleaseTask { module: &module, imports: &[], opt: 3, machine }, alloc).unwrap();
            let view = CodeView::read(&emission.record).unwrap();
            let unwind = blaze_code::unwind::read(view).unwrap();
            let cfi = unwind.cfi().expect("LLVM's original PC-dependent frame rules");
            assert!(unwind.has_lsda());
            assert_eq!(cfi.code_align(), 1);
            assert_eq!(cfi.data_align(), data_align);
            assert!(usize::try_from(cfi.code_len()).unwrap() < view.code().len(), "import veneers are outside the LLVM FDE");
            let landing = unwind.row(0);
            assert_eq!(landing.action, action);
            assert_eq!(landing.len, 4);
            assert!(landing.landing < cfi.code_len());
            let call = u32::from_le_bytes(view.code()[landing.start as usize..landing.start as usize + 4].try_into().unwrap());
            assert_eq!(call & 0xfc00_0000, 0x9400_0000, "the LSDA still covers the relocated BL");
        }
    }
}

#[test]
fn native_cleanup_calls_leave_gaps_without_losing_resume() {
    use blaze_code::{eh, trap, unwind::{self, Action}};

    let alloc = blaze_own::testing::heap().alloc();
    let mut b = Builder::<_, Full>::new_in(64, alloc);
    let entry_sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::PTR)], &[Param::of(Ty::I32)]);
    let call_sig = b.sig(Conv::C, Variadic::No, &[], &[Param::of(Ty::I32)]);
    let indirect_sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::I32)], &[]);
    b.set_signature(entry_sig);
    let entry = b.create_block(BlockKind::Normal);
    let normal = b.create_block(BlockKind::Normal);
    let landing = b.create_block(BlockKind::Cleanup);
    let drop = b.create_block(BlockKind::Normal);
    let callee = b.reference(blaze_lir::func::RefKind::Func, 0);
    b.begin(entry);
    let indirect = b.param(Ty::PTR);
    let first = b.invoke(Flags::NONE, callee, call_sig, &[], normal, landing);
    b.begin(normal);
    let second = b.call(Flags::NONE, callee, call_sig, &[]);
    let value = b.binary(blaze_lir::Op::Add, Flags::NONE, first, second);
    b.ret(&[value]);
    b.begin(landing);
    let exception = b.param(Ty::PTR);
    b.jump((drop, &[]));
    b.begin(drop);
    let value = b.call(Flags::NONE, callee, call_sig, &[]);
    b.call_indirect(Flags::NONE, indirect, indirect_sig, &[value]);
    b.resume(exception);
    let function = b.finish_verified().unwrap();
    let module = prepare(Symbol([5; 32]), function.verified(), &ByPlace, alloc).unwrap();
    for os in [blaze_tier::Os::Linux, blaze_tier::Os::MacOs] {
        let machine = blaze_tier::Machine { os, ..machine() };
        let emission = emit_release(ReleaseTask { module: &module, imports: &[], opt: 3, machine }, alloc).unwrap();
        let view = CodeView::read(&emission.record).unwrap();
        let unwind = unwind::read(view).unwrap();
        let cfi = unwind.cfi().expect("the original machine CFI is still present");
        assert!(unwind.has_lsda());
        let mut cleanup = 0;
        let mut continuing = 0;
        let mut direct_gap = 0;
        let mut indirect_gap = 0;
        let code = &view.code()[..cfi.code_len() as usize];
        for (i, &word) in code.as_chunks::<4>().0.iter().enumerate() {
            let word = u32::from_le_bytes(word);
            let direct = word & 0xfc00_0000 == 0x9400_0000;
            let indirect = trap::is_blr(word);
            assert_ne!(trap::brk(word), Some(super::control::CLEANUP_LANDING));
            if !direct && !indirect {
                continue;
            }
            let pc = (i * 4) as u32;
            let action = (0..unwind.rows())
                .map(|i| unwind.row(i))
                .find(|row| (row.start..row.start + row.len).contains(&pc))
                .map(|row| row.action);
            match (action, direct) {
                (Some(Action::Cleanup), true) => cleanup += 1,
                (Some(Action::Continue), true) => continuing += 1,
                (None, true) => direct_gap += 1,
                (None, false) => indirect_gap += 1,
                _ => panic!("unexpected unwind action {action:?} at {pc:#x} in {os:?}"),
            }
        }
        assert_eq!((cleanup, continuing, direct_gap, indirect_gap), (1, 2, 1, 1), "{os:?}");

        // The launcher includes the native exporter's exact LSDA bytes in its registered FDE.
        let mut registered = blaze_own::AVec::new_in(alloc);
        let entry = eh::entry(
            &unwind,
            eh::Placed { entry_at: 0x1000, code_at: 0x2000, personality: Some(0x3000) },
            &mut registered,
        ).unwrap();
        let mut exported = blaze_own::AVec::new_in(alloc);
        eh::lsda_table(&unwind, &mut exported).unwrap();
        assert_eq!(entry.code_len, cfi.code_len());
        assert_eq!(&registered[entry.lsda.unwrap()..], exported.as_slice());
    }
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
    caller_with_attrs(FnAttrs::default())
}

fn caller_with_attrs(attrs: FnAttrs) -> blaze_lir::VerifiedFunc<blaze_own::Alloc<'static>> {
    let mut b = Builder::<_, Full>::new_in(64, blaze_own::testing::heap().alloc());
    let p = Param::of(Ty::I32);
    let sig = b.sig(Conv::C, Variadic::No, &[p], &[p]);
    b.set_signature(sig);
    b.set_attrs(attrs);
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
    let cfi = view.cfi().expect("the leaf has an authoritative FDE too");
    assert!(!cfi.has_lsda());
    assert_eq!(cfi.code_len() as usize, view.code().len());
    assert_eq!(view.targets(), 0, "the control function names nothing outside");
    assert_eq!(view.sites(), 0);
    assert_eq!(view.words(), 0);
    assert!(view.code().len() >= 4 && view.code().len() % 4 == 0, "LLVM's code is whole instructions: {} bytes", view.code().len());
    assert!(view.code_align() >= 4, "the record's code is aligned at least 16: 2^{}", view.code_align());
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

/// An LLVM direct branch reaches a placed veneer that loads the callee's bound address.
#[test]
fn release_record_names_its_call() {
    let func = caller();
    let module = prepare(Symbol([2; 32]), func.verified(), &ByPlace, Alloc::global()).unwrap();
    let made = emit_release(ReleaseTask { module: &module, imports: &[], opt: 2, machine: machine() }, Alloc::global()).unwrap();
    let view = CodeView::read(&made.record).unwrap();
    assert_eq!(view.targets(), 1);
    assert_eq!(view.target(0), Target { space: Space::Func, symbol: Symbol([7; 32]) });
    assert_eq!(view.sites(), 2, "one original branch and its callable import veneer");
    assert_eq!(view.site(0).kind, SiteKind::Branch26);
    assert_eq!(view.site(1).kind, SiteKind::Load);
    let veneer = view.site(0).addend as usize;
    assert_eq!(u32::from_le_bytes(view.code()[veneer + 8..veneer + 12].try_into().unwrap()), 0xd61f_0200, "the veneer preserves the original branch's LR");
}

/// `fn(x: i32) -> i32 { x + 1 }`: the callee of [`caller`], prepared as its own module under
/// the symbol `ByPlace` names for `Outside::Func(0)`, so a link resolves the caller's `declare`
/// to this `define`.
fn callee() -> blaze_lir::VerifiedFunc<blaze_own::Alloc<'static>> {
    let mut b = Builder::<_, Full>::new_in(64, blaze_own::testing::heap().alloc());
    let p = Param::of(Ty::I32);
    let sig = b.sig(Conv::C, Variadic::No, &[p], &[p]);
    b.set_signature(sig);
    let entry = b.create_block(BlockKind::Normal);
    b.begin(entry);
    let x = b.param(Ty::I32);
    let one = b.iconst(Ty::I32, 1);
    let sum = b.binary(blaze_lir::Op::Add, Flags::NONE, x, one);
    b.ret(&[sum]);
    b.finish_verified().unwrap()
}

/// A release task with a real import closure: the callee's module is linked into the caller's
/// with `LLVMLinkModules2` before the pipeline runs, so LLVM sees the whole closure as one
/// module. The record is the caller's atom only (the callee's body is its own atom in
/// `__text`, which the reader leaves out), and it reads back as release work. Whether the
/// `Call` site survives is the optimizer's to decide (a one-instruction callee is inlined at
/// `O2`), so the test reports it rather than asserting it: both outcomes are real LLVM output.
#[test]
fn release_record_links_its_imports() {
    let callee_module = prepare(Symbol([7; 32]), callee().verified(), &ByPlace, Alloc::global()).unwrap();
    let caller_module = prepare(Symbol([2; 32]), caller().verified(), &ByPlace, Alloc::global()).unwrap();
    let imports: [&LlvmModule<'_>; 1] = [&callee_module];
    let emission = emit_release(ReleaseTask { module: &caller_module, imports: &imports, opt: 2, machine: machine() }, Alloc::global()).unwrap();
    assert_eq!(emission.backend, Backend::ReleaseBlob);
    assert_eq!(emission.symbol, Symbol([2; 32]));
    let view = CodeView::read(&emission.record).expect("the record reads");
    assert!(view.code().len() >= 4 && view.code().len() % 4 == 0, "LLVM's code is whole instructions: {} bytes", view.code().len());
    // The caller's reference to the callee is in its `references`; the callee's definition
    // is in the linked module. The record names at most that one target.
    assert!(view.targets() <= 1, "the caller names only its callee: {} targets", view.targets());
    println!("release record with imports: {} bytes of code, {} targets, {} sites", view.code().len(), view.targets(), view.sites());
}

#[test]
fn release_record_resolves_calls_introduced_by_inlining() {
    use super::{LinkName, emit::EmitError, object::ReadError};
    use blaze_code::record::OWN_CODE;

    struct Leaf(Named<'static>);
    impl Names for Leaf {
        fn name(&self, outside: Outside) -> Option<Named<'_>> {
            match outside {
                Outside::Func(0) => Some(self.0),
                _ => None,
            }
        }

        fn routines(&self) -> RoutinePlaces {
            RoutinePlaces::NONE
        }
    }

    let alloc = blaze_own::testing::heap().alloc();
    let root = prepare(Symbol([2; 32]), caller().verified(), &ByPlace, alloc).unwrap();
    let middle_symbol = Symbol([7; 32]);
    let middle_body = caller_with_attrs(FnAttrs::default().with_inline(Inline::Always));
    for os in [blaze_tier::Os::Linux, blaze_tier::Os::MacOs] {
        let machine = blaze_tier::Machine { os, ..machine() };
        let before = emit_release(ReleaseTask { module: &root, imports: &[], opt: 2, machine }, alloc).unwrap();
        let before = CodeView::read(&before.record).unwrap();
        assert_eq!(before.targets(), 1, "{os:?}");
        assert_eq!(before.target(0), Target { space: Space::Func, symbol: middle_symbol }, "{os:?}");

        for leaf in [Named::Symbol(Symbol([9; 32])), Named::C("blaze_test_inlined_callee")] {
            let (leaf_target, c_name) = match leaf {
                Named::Symbol(symbol) => (Target { space: Space::Func, symbol }, None),
                Named::C(name) => (Target { space: Space::Outside, symbol: Symbol::of_name(name) }, Some(name)),
            };
            let mut middle = prepare(middle_symbol, middle_body.verified(), &Leaf(leaf), alloc).unwrap();
            let after = emit_release(ReleaseTask { module: &root, imports: &[&middle], opt: 2, machine }, alloc).unwrap();
            let after = CodeView::read(&after.record).unwrap();
            assert_eq!(after.targets(), 1, "{os:?}, {leaf:?}");
            assert_eq!(after.target(0), leaf_target, "{os:?}, {leaf:?}");
            assert_eq!(after.name(leaf_target.symbol), c_name, "{os:?}, {leaf:?}");
            assert_eq!(after.sites(), 2, "{os:?}, {leaf:?}");
            let branch = after.site(0);
            let veneer = after.site(1);
            assert_eq!((branch.kind, branch.target), (SiteKind::Branch26, OWN_CODE));
            assert_eq!((veneer.at, veneer.kind, veneer.target), (branch.addend, SiteKind::Load, 0));

            // Keep the call in the linked IR and remove only its reference metadata.
            middle.references.clear();
            let error = emit_release(ReleaseTask { module: &root, imports: &[&middle], opt: 2, machine }, alloc).err().unwrap();
            let EmitError::Read(ReadError::UnknownSymbol { name }) = error else {
                panic!("{os:?}, {leaf:?}: unexpected emission error {error:?}");
            };
            if let Some(c_name) = c_name {
                assert_eq!(name.as_str(), c_name, "{os:?}");
            } else {
                let expected = blaze_own::format_in!(alloc, "{}", LinkName(leaf_target));
                assert_eq!(name.as_str(), expected.as_str(), "{os:?}");
            }
        }
    }
}

/// The linked record runs: `f(x) == x + 1` whether LLVM inlined the callee or left the call
/// (the callee's body is in the same object, so a `bl` resolves within it).
#[cfg(target_arch = "aarch64")]
#[test]
fn release_record_with_imports_runs() {
    use blaze_inproc::exec::Executable;
    let callee_module = prepare(Symbol([7; 32]), callee().verified(), &ByPlace, Alloc::global()).unwrap();
    let caller_module = prepare(Symbol([2; 32]), caller().verified(), &ByPlace, Alloc::global()).unwrap();
    let imports: [&LlvmModule<'_>; 1] = [&callee_module];
    let emission = emit_release(ReleaseTask { module: &caller_module, imports: &imports, opt: 2, machine: machine() }, Alloc::global()).unwrap();
    let view = CodeView::read(&emission.record).expect("the record reads");
    // The record must be self-contained to run: if the call survived, the record names the
    // callee by symbol and the test has no binder for it, so the run is only meaningful when
    // LLVM inlined (which `O2` does for a one-instruction callee).
    assert_eq!(view.targets(), 0, "O2 inlines the one-instruction callee; a surviving call would need a binder");
    assert_eq!(view.sites(), 0);
    let code = Executable::written(view.code().len(), 0, |code, _| code.copy_from_slice(view.code())).expect("the code maps");
    let f: unsafe extern "C" fn(i32) -> i32 = unsafe { code.as_fn() };
    assert_eq!(unsafe { f(0) }, 1);
    assert_eq!(unsafe { f(41) }, 42);
    assert_eq!(unsafe { f(-1) }, 0);
}

/// `fn() -> i64 { 0x0123_4567_89ab_cdef }`: a constant that takes the whole `mov` + 3 `movk`
/// sequence to materialize, so it cannot be folded away or mistaken for an immediate the
/// instruction encodes on its own. The record runs and returns exactly those bits: the
/// constant survives LLVM's pipeline and the object reader unchanged.
#[cfg(target_arch = "aarch64")]
#[test]
fn release_record_preserves_a_wide_constant() {
    use blaze_inproc::exec::Executable;
    let mut b = Builder::<_, Full>::new_in(64, blaze_own::testing::heap().alloc());
    let sig = b.sig(Conv::C, Variadic::No, &[], &[Param::of(Ty::I64)]);
    b.set_signature(sig);
    let entry = b.create_block(BlockKind::Normal);
    b.begin(entry);
    let value = b.iconst(Ty::I64, 0x0123_4567_89ab_cdef);
    b.ret(&[value]);
    let func = b.finish_verified().unwrap();
    let module = prepare(Symbol([5; 32]), func.verified(), &ByPlace, Alloc::global()).unwrap();
    let emission = emit_release(ReleaseTask { module: &module, imports: &[], opt: 2, machine: machine() }, Alloc::global()).unwrap();
    let view = CodeView::read(&emission.record).expect("the record reads");
    assert_eq!(view.targets(), 0);
    let code = Executable::written(view.code().len(), 0, |code, _| code.copy_from_slice(view.code())).expect("the code maps");
    let f: unsafe extern "C" fn() -> i64 = unsafe { code.as_fn() };
    assert_eq!(unsafe { f() }, 0x0123_4567_89ab_cdef);
}

#[cfg(target_arch = "aarch64")]
#[test]
fn release_record_stores_a_splat_to_every_lane() {
    use blaze_inproc::exec::Executable;
    use blaze_lir::{Op, Scalar, builder::MemAttrs};

    let alloc = blaze_own::testing::heap().alloc();
    let mut b = Builder::<_, Full>::new_in(64, alloc);
    let sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::PTR), Param::of(Ty::I32)], &[]);
    b.set_signature(sig);
    let entry = b.create_block(BlockKind::Normal);
    b.begin(entry);
    let output = b.param(Ty::PTR);
    let value = b.param(Ty::I32);
    let vector = b.convert(Op::Splat, Flags::NONE, Ty::vector(Scalar::I32, 4).unwrap(), value);
    b.store(Flags::NONE, vector, output, 0, MemAttrs::aligned(2));
    b.ret(&[]);
    let func = b.finish_verified().unwrap();
    let module = prepare(Symbol([15; 32]), func.verified(), &ByPlace, alloc).unwrap();
    let emission = emit_release(ReleaseTask { module: &module, imports: &[], opt: 2, machine: machine() }, alloc).unwrap();
    let view = CodeView::read(&emission.record).unwrap();
    assert_eq!(view.targets(), 0);
    assert_eq!(view.sites(), 0);
    let code = Executable::written(view.code().len(), 0, |code, _| code.copy_from_slice(view.code())).expect("the code maps");
    // SAFETY: the leaf has the declared C ABI and no relocations.
    let splat: unsafe extern "C" fn(*mut u32, u32) = unsafe { code.as_fn() };
    let mut lanes = [0; 6];
    for value in [0x1234_5678, 0xfedc_ba98] {
        // SAFETY: lanes[1..5] provides four writable u32 lanes with the declared alignment.
        unsafe { splat(lanes[1..5].as_mut_ptr(), value) };
        assert_eq!(lanes, [0, value, value, value, value, 0]);
    }
}

#[cfg(target_arch = "aarch64")]
fn place_leaf(view: &CodeView<'_>, symbol: Symbol) -> blaze_inproc::exec::Executable {
    use blaze_code::{image::ImageError, place::{At, Words, place}};
    use blaze_inproc::exec::Executable;

    struct NoImports;
    impl Words for NoImports {
        fn word(&mut self, target: Target, _: bool) -> Result<u64, ImageError> {
            Err(ImageError::Unbound { target })
        }
        fn value(&mut self, target: Target) -> Result<u64, ImageError> {
            Err(ImageError::Unbound { target })
        }
    }

    assert_eq!(view.targets(), 0);
    Executable::written(view.code().len(), view.relro().len(), |code, data| {
        let at = At { code: code.as_ptr().addr().try_into().unwrap(), relro: data.as_ptr().addr().try_into().unwrap() };
        place(view, symbol, at, &mut NoImports, code, data).expect("the leaf and its own data place");
    }).expect("the code maps")
}

#[cfg(target_arch = "aarch64")]
#[test]
fn release_record_shuffles_both_vectors_into_each_result_width() {
    use blaze_lir::{Scalar, builder::MemAttrs};

    let alloc = blaze_own::testing::heap().alloc();
    let mut b = Builder::<_, Full>::new_in(64, alloc);
    let ptr = Param::of(Ty::PTR);
    let sig = b.sig(Conv::C, Variadic::No, &[ptr, ptr, ptr], &[]);
    b.set_signature(sig);
    let entry = b.create_block(BlockKind::Normal);
    b.begin(entry);
    let first = b.param(Ty::PTR);
    let second = b.param(Ty::PTR);
    let output = b.param(Ty::PTR);
    let four = Ty::vector(Scalar::I32, 4).unwrap();
    let two = Ty::vector(Scalar::I32, 2).unwrap();
    let a = b.load(four, Flags::NONE, first, 0, MemAttrs::aligned(2));
    let z = b.load(four, Flags::NONE, second, 0, MemAttrs::aligned(2));
    let interleaved = b.shuffle(four, a, z, &[6, 1, 4, 3]);
    let narrowed = b.shuffle(two, a, z, &[7, 0]);
    b.store(Flags::NONE, interleaved, output, 0, MemAttrs::aligned(2));
    b.store(Flags::NONE, narrowed, output, 16, MemAttrs::aligned(2));
    b.ret(&[]);

    let func = b.finish_verified().unwrap();
    let symbol = Symbol([18; 32]);
    let module = prepare(symbol, func.verified(), &ByPlace, alloc).unwrap();
    let emission = emit_release(ReleaseTask { module: &module, imports: &[], opt: 2, machine: machine() }, alloc).unwrap();
    let view = CodeView::read(&emission.record).unwrap();
    let code = place_leaf(&view, symbol);
    // SAFETY: the placed leaf has the declared C ABI.
    let shuffle: unsafe extern "C" fn(*const u32, *const u32, *mut u32) = unsafe { code.as_fn() };
    let mut lanes = [0; 6];
    // SAFETY: each input holds four readable lanes and output holds six writable lanes.
    unsafe { shuffle([11, 12, 13, 14].as_ptr(), [21, 22, 23, 24].as_ptr(), lanes.as_mut_ptr()) };
    assert_eq!(lanes, [23, 12, 21, 14, 24, 11]);
}

#[cfg(target_arch = "aarch64")]
#[test]
fn release_record_masks_each_vector_shift_amount() {
    use blaze_lir::{Op, Scalar, builder::MemAttrs};

    let alloc = blaze_own::testing::heap().alloc();
    let mut b = Builder::<_, Full>::new_in(64, alloc);
    let ptr = Param::of(Ty::PTR);
    let sig = b.sig(Conv::C, Variadic::No, &[ptr, ptr, ptr], &[]);
    b.set_signature(sig);
    let entry = b.create_block(BlockKind::Normal);
    b.begin(entry);
    let input = b.param(Ty::PTR);
    let amounts = b.param(Ty::PTR);
    let output = b.param(Ty::PTR);
    let vector = Ty::vector(Scalar::I32, 4).unwrap();
    let align = MemAttrs::aligned(2);
    let values = b.load(vector, Flags::NONE, input, 0, align);
    let amounts = b.load(vector, Flags::NONE, amounts, 0, align);
    for (op, offset) in [(Op::Shl, 0), (Op::Lshr, 16), (Op::Ashr, 32)] {
        let value = b.binary(op, Flags::NONE, values, amounts);
        b.store(Flags::NONE, value, output, offset, align);
    }
    b.ret(&[]);

    let func = b.finish_verified().unwrap();
    let symbol = Symbol([19; 32]);
    let module = prepare(symbol, func.verified(), &ByPlace, alloc).unwrap();
    let emission = emit_release(ReleaseTask { module: &module, imports: &[], opt: 2, machine: machine() }, alloc).unwrap();
    let view = CodeView::read(&emission.record).unwrap();
    let code = place_leaf(&view, symbol);
    // SAFETY: the placed leaf has the declared C ABI.
    let shift: unsafe extern "C" fn(*const u32, *const u32, *mut u32) = unsafe { code.as_fn() };
    let input = [0x8000_0003, 0x8000_0005, 0xffff_fff8, 0x22];
    let amounts = [0, 33, u32::MAX, 64];
    let mut output = [0; 12];
    // SAFETY: each input holds four readable lanes and output holds twelve writable lanes.
    unsafe { shift(input.as_ptr(), amounts.as_ptr(), output.as_mut_ptr()) };
    assert_eq!(output[..4], [0x8000_0003, 10, 0, 0x22]);
    assert_eq!(output[4..8], [0x8000_0003, 0x4000_0002, 1, 0x22]);
    assert_eq!(output[8..], [0x8000_0003, 0xc000_0002, u32::MAX, 0x22]);
}

#[cfg(target_arch = "aarch64")]
#[test]
fn release_record_packs_bitmask_lanes_low_to_high() {
    use blaze_lir::{IntCond, Op, Scalar, builder::MemAttrs};

    let alloc = blaze_own::testing::heap().alloc();
    let mut b = Builder::<_, Full>::new_in(64, alloc);
    let pointer = Param::of(Ty::PTR);
    let sig = b.sig(Conv::C, Variadic::No, &[pointer, pointer], &[]);
    b.set_signature(sig);
    let entry = b.create_block(BlockKind::Normal);
    b.begin(entry);
    let input = b.param(Ty::PTR);
    let output = b.param(Ty::PTR);
    let bytes = Ty::vector(Scalar::I8, 4).unwrap();
    let values = b.load(bytes, Flags::NONE, input, 0, MemAttrs::aligned(0));
    let zero = b.iconst(Ty::I8, 0);
    let zeros = b.convert(Op::Splat, Flags::NONE, bytes, zero);
    let lanes = b.icmp(IntCond::Ne, values, zeros);
    let widened = b.convert(Op::Bitmask, Flags::NONE, Ty::I128, lanes);
    b.store(Flags::NONE, widened, output, 0, MemAttrs::aligned(0));
    let bits = Ty::vector(Scalar::I1, 128).unwrap();
    let lanes = b.load(bits, Flags::NONE, input, 4, MemAttrs::aligned(0));
    let exact = b.convert(Op::Bitmask, Flags::NONE, Ty::I128, lanes);
    b.store(Flags::NONE, exact, output, 16, MemAttrs::aligned(0));
    b.ret(&[]);
    let func = b.finish_verified().unwrap();
    let symbol = Symbol([16; 32]);
    let module = prepare(symbol, func.verified(), &ByPlace, alloc).unwrap();
    let emission = emit_release(ReleaseTask { module: &module, imports: &[], opt: 2, machine: machine() }, alloc).unwrap();
    let view = CodeView::read(&emission.record).unwrap();
    let code = place_leaf(&view, symbol);
    // SAFETY: the placed leaf has the declared C ABI.
    let pack: unsafe extern "C" fn(*const u8, *mut u8) = unsafe { code.as_fn() };
    let mut input = [0xd5; 20];
    input[..4].copy_from_slice(&[0xff, 0, 0xff, 0xff]);
    let mut output = [0xee; 32];
    // SAFETY: the source provides 20 readable bytes, and the destination provides 32 writable bytes.
    unsafe { pack(input.as_ptr(), output.as_mut_ptr()) };
    assert_eq!(output[..16], 0x0du128.to_le_bytes());
    assert_eq!(output[16..], input[4..]);
}

#[cfg(target_arch = "aarch64")]
#[test]
fn release_record_reduces_integer_and_ordered_float_vectors() {
    use blaze_lir::{Op, Scalar, builder::MemAttrs};

    let alloc = blaze_own::testing::heap().alloc();
    let mut b = Builder::<_, Full>::new_in(64, alloc);
    let ptr = Param::of(Ty::PTR);
    let sig = b.sig(Conv::C, Variadic::No, &[ptr, ptr, ptr, Param::of(Ty::F32)], &[]);
    b.set_signature(sig);
    let entry = b.create_block(BlockKind::Normal);
    b.begin(entry);
    let integers = b.param(Ty::PTR);
    let floats = b.param(Ty::PTR);
    let output = b.param(Ty::PTR);
    let start = b.param(Ty::F32);
    let vector = b.load(Ty::vector(Scalar::I32, 4).unwrap(), Flags::NONE, integers, 0, MemAttrs::aligned(2));
    for (op, offset) in [
        (Op::ReduceAdd, 0), (Op::ReduceMul, 4), (Op::ReduceAnd, 8),
        (Op::ReduceOr, 12), (Op::ReduceXor, 16), (Op::ReduceSmin, 20),
        (Op::ReduceSmax, 24), (Op::ReduceUmin, 28), (Op::ReduceUmax, 32),
    ] {
        let result = b.convert(op, Flags::NONE, Ty::I32, vector);
        b.store(Flags::NONE, result, output, offset, MemAttrs::aligned(2));
    }
    let vector = b.load(Ty::vector(Scalar::F32, 4).unwrap(), Flags::NONE, floats, 0, MemAttrs::aligned(2));
    for (op, flags, offset) in [
        (Op::ReduceFadd, Flags::NONE, 36),
        (Op::ReduceFadd, Flags::REASSOC, 40),
        (Op::ReduceFmul, Flags::NONE, 44),
    ] {
        let result = b.reduce_ordered(op, flags, start, vector);
        b.store(Flags::NONE, result, output, offset, MemAttrs::aligned(2));
    }
    b.ret(&[]);

    let func = b.finish_verified().unwrap();
    let symbol = Symbol([17; 32]);
    let module = prepare(symbol, func.verified(), &ByPlace, alloc).unwrap();
    let ir = core::str::from_utf8(&module.ir).unwrap();
    assert!(ir.contains("call float @llvm.vector.reduce.fadd.v4f32"));
    assert!(ir.contains("call reassoc float @llvm.vector.reduce.fadd.v4f32"));
    let emission = emit_release(ReleaseTask { module: &module, imports: &[], opt: 2, machine: machine() }, alloc).unwrap();
    let view = CodeView::read(&emission.record).unwrap();
    let code = place_leaf(&view, symbol);
    // SAFETY: the placed leaf has the declared C ABI.
    let reduce: unsafe extern "C" fn(*const u32, *const f32, *mut u32, f32) = unsafe { code.as_fn() };
    let integers = [0xffff_fff1, 3, 0x8000_0005, 9];
    let mut output = [0; 12];
    // SAFETY: each input holds four readable lanes and output holds twelve writable words.
    unsafe { reduce(integers.as_ptr(), [-1e20, 1.0, 2.0, 3.0].as_ptr(), output.as_mut_ptr(), 1e20) };
    assert_eq!(output[..9], [0x8000_0002, 0x7fff_f817, 1, 0xffff_ffff, 0x7fff_fffe, 0x8000_0005, 9, 3, 0xffff_fff1]);
    // This is (((start + lane0) + lane1) + lane2) + lane3; summing lanes first loses the small values.
    assert_eq!(f32::from_bits(output[9]), 6.0);
    // SAFETY: same input and output lengths as above, with exactly representable float reductions.
    unsafe { reduce(integers.as_ptr(), [2.0, 3.0, 4.0, 5.0].as_ptr(), output.as_mut_ptr(), 1.0) };
    assert_eq!(output[9..], [15.0f32.to_bits(), 15.0f32.to_bits(), 120.0f32.to_bits()]);
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
    module.unreachable_operands.clear();
    module
}

#[test]
fn optimized_import_keeps_its_trap_source_when_the_roots_trap_is_deleted() {
    let alloc = blaze_own::testing::heap().alloc();
    let make = |is_root: bool| {
        let mut b = Builder::<_, Full>::new_in(64, alloc);
        let p = Param::of(Ty::I32);
        let sig = b.sig(Conv::C, Variadic::No, &[p], &[p]);
        b.set_signature(sig);
        if !is_root {
            b.set_attrs(FnAttrs::default().with_inline(Inline::Always));
        }
        let entry = b.create_block(BlockKind::Normal);
        let die = b.create_block(BlockKind::Normal);
        let good = b.create_block(BlockKind::Normal);
        b.begin(entry);
        let x = b.param(Ty::I32);
        let condition = if is_root {
            b.iconst(Ty::I1, 0)
        } else {
            let zero = b.iconst(Ty::I32, 0);
            b.icmp(blaze_lir::IntCond::Eq, x, zero)
        };
        b.br(Flags::NONE, condition, (die, &[]), (good, &[]));
        b.begin(die);
        b.unreachable();
        b.begin(good);
        let value = if is_root {
            let reference = b.reference(blaze_lir::func::RefKind::Func, 0);
            b.call(Flags::NONE, reference, sig, &[x])
        } else {
            x
        };
        b.ret(&[value]);
        b.finish_verified().unwrap()
    };
    let before_opt = make(true);
    let imported = make(false);
    let mut root = prepare(Symbol([2; 32]), before_opt.verified(), &ByPlace, alloc).unwrap();
    let mut callee = prepare(Symbol([7; 32]), imported.verified(), &ByPlace, alloc).unwrap();
    root.traps.push(TrapSource { ordinal: 0, kind: TrapKind::BodyEnd, site: Some((2, 10)), subject: None });
    callee.traps.push(TrapSource { ordinal: 0, kind: TrapKind::Intrinsic, site: Some((7, 20)), subject: None });
    let record = emit_release(ReleaseTask { module: &root, imports: &[&callee], opt: 2, machine: machine() }, alloc).unwrap();
    let view = CodeView::read(&record.record).unwrap();
    assert_eq!(view.targets(), 0, "the trap must survive in the root after inlining");
    assert_eq!(view.traps(), 1, "only the imported trap survives optimization");
    let trap = view.trap(0);
    assert_eq!((trap.kind, trap.site), (TrapKind::Intrinsic, Some((7, 20))));
    let at = usize::try_from(trap.at).unwrap();
    let brk = u32::from_le_bytes(view.code()[at..at + 4].try_into().unwrap());
    assert_eq!(brk, 0xd420_0000 | UNREACHABLE << 5, "only the semantic BRK reaches the record");
}

#[test]
fn explicit_lir_traps_cannot_consume_the_llvm_provenance_range() {
    let alloc = blaze_own::testing::heap().alloc();
    for reserved in [UNREACHABLE + 1, u32::from(u16::MAX)] {
        let mut b = Builder::<_, Full>::new_in(64, alloc);
        let sig = b.sig(Conv::C, Variadic::No, &[], &[]);
        b.set_signature(sig);
        let entry = b.create_block(BlockKind::Normal);
        b.begin(entry);
        b.trap(reserved);
        let body = b.finish_verified().unwrap();
        let error = prepare(Symbol([5; 32]), body.verified(), &ByPlace, alloc).err().unwrap();
        assert_eq!(error.reason, "a LIR trap in the emitter's reserved BRK range");
    }
}

#[test]
fn a_c_link_name_cannot_call_the_break_intrinsic() {
    struct NamedBreak;
    impl Names for NamedBreak {
        fn name(&self, _: Outside) -> Option<Named<'_>> {
            Some(Named::C("llvm.aarch64.break"))
        }

        fn routines(&self) -> RoutinePlaces {
            RoutinePlaces::NONE
        }
    }
    let body = caller();
    let error = prepare(Symbol([5; 32]), body.verified(), &NamedBreak, Alloc::global()).err().unwrap();
    assert_eq!(error.reason, "a C link name in LLVM's intrinsic namespace");
}

#[test]
fn object_reader_refuses_an_unassigned_trap_id() {
    use super::{emit::EmitError, object::ReadError};
    let ir = "declare void @llvm.aarch64.break(i32)\ndefine void @entry() uwtable {\n\
              call void @llvm.aarch64.break(i32 65535)\n  unreachable\n}\n";
    let module = object_fixture(ir);
    let error = emit_release(ReleaseTask { module: &module, imports: &[], opt: 2, machine: machine() }, Alloc::global()).err().unwrap();
    assert!(matches!(error, EmitError::Read(ReadError::TrapTag { immediate: 65535, .. })));
}

#[test]
fn release_record_names_synthesized_libcalls_and_requires_metadata_for_input_externs() {
    use super::{emit::EmitError, object::ReadError};
    use blaze_lir::builder::MemAttrs;

    let alloc = blaze_own::testing::heap().alloc();
    let mut b = Builder::<_, Full>::new_in(64, alloc);
    let ptr = Param::of(Ty::PTR);
    let sig = b.sig(Conv::C, Variadic::No, &[ptr, ptr, Param::of(Ty::I64)], &[Param::of(Ty::I1)]);
    b.set_signature(sig);
    let entry = b.create_block(BlockKind::Normal);
    b.begin(entry);
    let left = b.param(Ty::PTR);
    let right = b.param(Ty::PTR);
    let len = b.param(Ty::I64);
    let compared = b.memcmp(left, right, len, MemAttrs::aligned(0));
    let zero = b.iconst(Ty::I32, 0);
    let equal = b.icmp(blaze_lir::IntCond::Eq, compared, zero);
    b.ret(&[equal]);
    let body = b.finish_verified().unwrap();
    let module = prepare(Symbol([19; 32]), body.verified(), &ByPlace, alloc).unwrap();
    let memcmp = Target { space: Space::Outside, symbol: Symbol::of_name("memcmp") };
    assert_eq!(module.implicit_references.as_slice(), &[memcmp]);

    let copy = object_fixture("declare void @llvm.memcpy.p0.p0.i64(ptr, ptr, i64, i1 immarg)\n\
        define void @entry(ptr %dst, ptr %src, i64 %len) uwtable {\n\
          %thread = call i64 asm sideeffect \"mrs $0, tpidrro_el0\", \"=r\"()\n\
          call void asm sideeffect \"yield\", \"\"()\n\
          call void @llvm.memcpy.p0.p0.i64(ptr %dst, ptr %src, i64 %len, i1 false)\n\
          ret void\n\
        }\n");
    // Explicit, ABI-compatible input declarations still require metadata. The signed
    // memcmp result remains observable, so LLVM cannot replace it with bcmp.
    let input = object_fixture("declare i32 @memcmp(ptr, ptr, i64)\n\
        define i32 @entry(ptr %a, ptr %b, i64 %n) uwtable {\n\
          %result = call i32 @memcmp(ptr %a, ptr %b, i64 %n)\n\
          ret i32 %result\n\
        }\n");

    for (os, equality_name) in [(blaze_tier::Os::Linux, "bcmp"), (blaze_tier::Os::MacOs, "memcmp")] {
        let machine = blaze_tier::Machine { os, ..machine() };
        let emit = |module| emit_release(ReleaseTask { module, imports: &[], opt: 2, machine }, alloc);
        // The target library table offers bcmp on Linux. Darwin keeps memcmp.
        for (module, name) in [(&module, equality_name), (&copy, "memcpy")] {
            let emission = emit(module).unwrap_or_else(|error| panic!("{os:?}: {error}"));
            let view = CodeView::read(&emission.record).unwrap();
            let target = Target { space: Space::Outside, symbol: Symbol::of_name(name) };
            assert_eq!(view.targets(), 1, "{os:?}, {name}");
            assert_eq!(view.target(0), target, "{os:?}, {name}");
            assert_eq!(view.name(target.symbol), Some(name));
            assert!((0..view.sites()).any(|i| view.site(i).kind == SiteKind::Branch26));
        }

        let error = emit(&input).err().expect("input extern has no metadata");
        assert!(matches!(error, EmitError::Read(ReadError::UnknownSymbol { name }) if name.as_str() == "memcmp"), "{os:?}");

        // The assembler makes a real branch relocation with no corresponding LLVM global.
        let asm_symbol = if os == blaze_tier::Os::MacOs { "_memcmp" } else { "memcmp" };
        let raw = blaze_own::format_in!(alloc, "define void @entry() uwtable {{\n\
          call void asm sideeffect \"b {asm_symbol}\", \"\"()\n\
          ret void\n\
        }}\n");
        let raw = object_fixture(&raw);
        let error = emit_release(ReleaseTask { module: &raw, imports: &[], opt: 2, machine }, alloc)
            .err()
            .expect("assembly target has no metadata");
        assert!(matches!(error, EmitError::Read(ReadError::UnknownSymbol { name }) if name.as_str() == "memcmp"), "{os:?}");
    }

    let remainder = object_fixture("define fp128 @entry(fp128 %left, fp128 %right) uwtable {\n\
          %result = frem fp128 %left, %right\n\
          ret fp128 %result\n\
        }\n");
    let machine = blaze_tier::Machine { os: blaze_tier::Os::Linux, ..machine() };
    let error = emit_release(ReleaseTask { module: &remainder, imports: &[], opt: 2, machine }, alloc)
        .err().expect("libm is outside the synthesized system ABI");
    assert!(matches!(error, EmitError::Read(ReadError::UnknownSymbol { name }) if matches!(name.as_str(), "fmodl" | "fmodf128")));
}

#[test]
fn release_record_binds_the_runtime_helpers_for_wide_division() {
    use super::{emit::EmitError, object::ReadError};
    use blaze_lir::Op;

    #[cfg(target_arch = "aarch64")]
    extern "C" fn divide(a: u128, b: u128) -> u128 {
        a / b
    }

    let alloc = blaze_own::testing::heap().alloc();
    let host = machine();
    for (op, mask, name) in [
        (Op::Udiv, None, "__udivti3"), (Op::Urem, None, "__umodti3"),
        (Op::Sdiv, None, "__divti3"), (Op::Srem, None, "__modti3"),
        (Op::Sdiv, Some((1u128 << 127) - 1), "__udivti3"),
    ] {
        let mut b = Builder::<_, Full>::new_in(64, alloc);
        let p = Param::of(Ty::I128);
        let sig = b.sig(Conv::C, Variadic::No, &[p, p], &[p]);
        b.set_signature(sig);
        let entry = b.create_block(BlockKind::Normal);
        b.begin(entry);
        let mut a = b.param(Ty::I128);
        let mut divisor = b.param(Ty::I128);
        if let Some(mask) = mask {
            let nonnegative = b.iconst(Ty::I128, mask);
            a = b.binary(Op::And, Flags::NONE, a, nonnegative);
            divisor = b.binary(Op::And, Flags::NONE, divisor, nonnegative);
        }
        let result = b.binary(op, Flags::NONE, a, divisor);
        b.ret(&[result]);
        let body = b.finish_verified().unwrap();
        let mut module = prepare(Symbol([21; 32]), body.verified(), &ByPlace, alloc).unwrap();
        let target = Target { space: Space::Outside, symbol: Symbol::of_name(name) };
        assert!(module.implicit_references.contains(&target));

        for os in [blaze_tier::Os::Linux, blaze_tier::Os::MacOs] {
            let machine = blaze_tier::Machine { os, ..host };
            let made = emit_release(ReleaseTask { module: &module, imports: &[], opt: 2, machine }, alloc)
                .unwrap_or_else(|error| panic!("{os:?}, {name}: {error}"));
            let view = CodeView::read(&made.record).unwrap();
            assert_eq!(view.targets(), 1, "{os:?}, {name}");
            assert_eq!(view.target(0), target, "{os:?}, {name}");
            assert_eq!(view.name(target.symbol), Some(name));

            #[cfg(target_arch = "aarch64")]
            if os == host.os && op == Op::Udiv {
                let placed = place_with_callable(&view, module.symbol, target, divide as *const () as u64);
                // SAFETY: the emitted C ABI and bound helper both take and return u128.
                let quotient: unsafe extern "C" fn(u128, u128) -> u128 = unsafe { placed.as_fn() };
                let numerator = (1u128 << 127) + (3u128 << 64) + 123;
                for divisor in [3, (1u128 << 65) + 17] {
                    // SAFETY: the executable is live and both divisors are nonzero.
                    assert_eq!(unsafe { quotient(numerator, divisor) }, numerator / divisor);
                }
            }
        }

        module.implicit_references.clear();
        let error = emit_release(ReleaseTask { module: &module, imports: &[], opt: 2, machine: host }, alloc)
            .err().expect("compiler-runtime calls require typed input metadata");
        assert!(matches!(error, EmitError::Read(ReadError::UnknownSymbol { name: actual }) if actual.as_str() == name));
    }
}

#[cfg(target_arch = "aarch64")]
fn place_with_callable(
    view: &CodeView<'_>,
    symbol: Symbol,
    target: Target,
    callable: u64,
) -> blaze_inproc::exec::Executable {
    use blaze_code::image::ImageError;
    use blaze_code::place::{At, Words, place};
    use blaze_inproc::exec::Executable;
    struct Import {
        slot: u64,
        target: Target,
        callable: u64,
    }
    impl Words for Import {
        fn word(&mut self, target: Target, call: bool) -> Result<u64, ImageError> {
            assert_eq!(target, self.target);
            assert!(!call, "the veneer loads a callable entry, then tail branches");
            Ok(self.slot)
        }
        fn value(&mut self, target: Target) -> Result<u64, ImageError> {
            assert_eq!(target, self.target);
            Ok(self.callable)
        }
    }
    Executable::written(view.code().len(), 8 + view.relro().len(), |code, data| {
        let slot = data.as_ptr() as usize as u64;
        data[..8].copy_from_slice(&callable.to_le_bytes());
        let at = At { code: code.as_ptr() as usize as u64, relro: slot + 8 };
        place(view, symbol, at, &mut Import { slot, target, callable }, code, &mut data[8..]).unwrap();
    }).unwrap()
}

#[cfg(target_arch = "aarch64")]
#[test]
fn native_call_and_tail_branch_execute_with_the_original_return_address() {
    extern "C" fn twice(n: i64) -> i64 { n * 2 }
    let target = Target { space: Space::Func, symbol: Symbol([7; 32]) };
    let outside = std::format!("{}", super::LinkName(target));
    for tail in [false, true] {
        let ending = if tail { "ret i64 %v" } else { "%sum = add i64 %v, 7\n ret i64 %sum" };
        let ir = std::format!("declare i64 @{outside}(i64)\ndefine i64 @entry(i64 %n) uwtable {{\n %v = tail call i64 @{outside}(i64 %n)\n {ending}\n}}\n");
        let mut module = object_fixture(&ir);
        module.references.push(Some(target));
        let made = emit_release(ReleaseTask { module: &module, imports: &[], opt: 2, machine: machine() }, Alloc::global()).unwrap();
        let view = CodeView::read(&made.record).unwrap();
        let executable = place_with_callable(&view, module.symbol, target, twice as *const () as usize as u64);
        // SAFETY: this LLVM definition and the Rust callee both have C ABI i64 -> i64;
        // production placement has filled every relocation before executable publication.
        let call: unsafe extern "C" fn(i64) -> i64 = unsafe { executable.as_fn() };
        assert_eq!(unsafe { call(13) }, if tail { 26 } else { 33 });
    }
}

#[cfg(target_arch = "aarch64")]
#[test]
fn folded_body_and_each_member_thunk_execute_their_own_constants() {
    use super::{
        emit::{FoldMember, FoldTask, emit_fold},
        folding::{Role, module},
    };
    use blaze_inproc::exec::Executable;

    let alloc = blaze_own::testing::heap().alloc();
    let prepared = |symbol: u8, constant: u128| {
        let mut b = Builder::<_, Full>::new_in(64, alloc);
        let p = Param::of(Ty::I32);
        let sig = b.sig(Conv::C, Variadic::No, &[p], &[p]);
        b.set_signature(sig);
        let entry = b.create_block(BlockKind::Normal);
        b.begin(entry);
        let x = b.param(Ty::I32);
        let value = b.iconst(Ty::I32, constant);
        let add = b.binary(blaze_lir::Op::Add, Flags::NONE, x, value);
        b.ret(&[add]);
        let body = b.finish_verified().unwrap();
        prepare(Symbol([symbol; 32]), body.verified(), &ByPlace, alloc).unwrap()
    };
    let first = prepared(7, 7);
    let second = prepared(9, 9);
    let body_symbol = Symbol([11; 32]);
    let target = Target { space: Space::Func, symbol: body_symbol };
    let rewrite = |source, role| module(source, body_symbol, role, [(0, 0)].into_iter(), alloc).unwrap();
    let body = rewrite(&first, Role::Body);
    let first_thunk = rewrite(&first, Role::Thunk);
    let second_thunk = rewrite(&second, Role::Thunk);
    let members = [
        FoldMember { module: &body, is_body: true },
        FoldMember { module: &first_thunk, is_body: false },
        FoldMember { module: &second_thunk, is_body: false },
    ];
    let folded = emit_fold(FoldTask { members: &members, machine: machine() }, alloc).unwrap();
    assert_eq!(folded.body.backend, Backend::FoldedOutput);
    assert_eq!(folded.body.symbol, body_symbol);
    assert_eq!(folded.thunks.len(), 2);
    let view = CodeView::read(&folded.body.record).unwrap();
    assert_eq!(view.targets(), 0);
    assert_eq!(view.sites(), 0);
    let shared = Executable::written(view.code().len(), 0, |code, _| code.copy_from_slice(view.code())).unwrap();
    // SAFETY: the emitted shared body adds one i32 argument to the original C ABI.
    let entry: unsafe extern "C" fn(i32, i32) -> i32 = unsafe { shared.as_fn() };
    assert_eq!(unsafe { entry(13, 26) }, 39);
    for (thunk, (symbol, expected)) in folded.thunks.iter().zip([(first.symbol, 20), (second.symbol, 22)]) {
        assert_eq!(thunk.backend, Backend::FoldedOutput);
        assert_eq!(thunk.symbol, symbol);
        let view = CodeView::read(&thunk.record).unwrap();
        let placed = place_with_callable(&view, symbol, target, entry as usize as u64);
        // SAFETY: each placed thunk has the original i32 -> i32 C ABI and binds the live shared body.
        let call: unsafe extern "C" fn(i32) -> i32 = unsafe { placed.as_fn() };
        assert_eq!(unsafe { call(13) }, expected);
    }
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
define i64 @entry() uwtable {{
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

        let unrelated_ir = ir
            .replace("define i64 @entry()", "@ordinary_zero = hidden externally_initialized global [1 x i32] zeroinitializer, align 4\ndefine i64 @entry()")
            .replace("%wide = zext i32 %line to i64", "%other = load volatile i32, ptr @ordinary_zero, align 4\n  %sum = add i32 %line, %other\n  %wide = zext i32 %sum to i64");
        let mut unrelated = object_fixture(&unrelated_ir);
        unrelated.references.push(Some(target));
        unrelated.positions.extend_from_slice(&module.positions);
        let refused = emit_release(ReleaseTask { module: &unrelated, imports: &[], opt: 2, machine }, Alloc::global());
        assert!(matches!(refused, Err(super::emit::EmitError::Read(super::object::ReadError::Writable { .. }))), "{os:?}: a known position slot cannot admit another writable global");
    }
}

#[test]
fn release_record_preserves_an_exact_one_past_relro_address() {
    use super::{emit::EmitError, object::ReadError};
    use blaze_code::record::OWN_RELRO;

    const DATA: &str = "@bytes = hidden constant [443 x i8] zeroinitializer, align 1\n";
    let alloc = blaze_own::testing::heap().alloc();
    let end = object_fixture(&blaze_own::format_in!(alloc, r#"
{DATA}define ptr @entry() uwtable {{
  ret ptr getelementptr inbounds ([443 x i8], ptr @bytes, i64 0, i64 443)
}}
"#));
    for os in [blaze_tier::Os::MacOs, blaze_tier::Os::Linux] {
        let machine = blaze_tier::Machine { os, ..machine() };
        let emission = emit_release(ReleaseTask { module: &end, imports: &[], opt: 2, machine }, alloc)
            .unwrap_or_else(|error| panic!("{os:?}: {error}"));
        let view = CodeView::read(&emission.record).unwrap();
        assert_eq!(view.targets(), 0, "{os:?}");
        assert_eq!(view.relro().len(), 443, "{os:?}");
        assert_eq!(view.sites(), 2, "{os:?}");
        assert_eq!([view.site(0).kind, view.site(1).kind], [SiteKind::Page21, SiteKind::AddLo12], "{os:?}");
        for site in [view.site(0), view.site(1)] {
            assert_eq!((site.target, site.addend), (OWN_RELRO, 443), "{os:?}");
        }
    }

    // Force the ELF relocations in the two refusals. LLVM's normal instruction selector
    // keeps an object offset beyond its end as separate arithmetic.
    let machine = blaze_tier::Machine { os: blaze_tier::Os::Linux, ..machine() };
    for (asm, kind, expected) in [
        (r"adrp $0, bytes+444", SiteKind::Page21, 444),
        (r"adrp $0, bytes+443\0Aldrb ${0:w}, [$0, :lo12:bytes+443]", SiteKind::LdstLo12_1, 443),
    ] {
        let module = object_fixture(&blaze_own::format_in!(alloc, r#"
{DATA}define i64 @entry() uwtable {{
  %result = call i64 asm sideeffect "{asm}", "=r"()
  ret i64 %result
}}
"#));
        let error = emit_release(ReleaseTask { module: &module, imports: &[], opt: 2, machine }, alloc)
            .err().expect("the object relocation must be refused");
        assert!(matches!(error, EmitError::Read(ReadError::LocalRelroBounds {
            kind: actual, offset, section_len: 443, relro_at, relro_len: 443, ..
        }) if (actual, offset, relro_at) == (kind, expected, expected)), "unexpected refusal {error:?}");
    }
}

#[test]
fn release_record_returns_the_address_of_zero_sized_data() {
    use super::{emit::EmitError, object::ReadError};
    use blaze_code::record::OWN_RELRO;

    const DATA: &str = "@empty = hidden constant [0 x i8] zeroinitializer, align 64\n";
    let alloc = blaze_own::testing::heap().alloc();
    let module = object_fixture(&blaze_own::format_in!(alloc, r#"
{DATA}define ptr @entry() uwtable {{
  ret ptr getelementptr inbounds ([0 x i8], ptr @empty, i64 0, i64 0)
}}
"#));
    let host = machine();
    for os in [blaze_tier::Os::MacOs, blaze_tier::Os::Linux] {
        let machine = blaze_tier::Machine { os, ..host };
        let emission = emit_release(ReleaseTask { module: &module, imports: &[], opt: 2, machine }, alloc)
            .unwrap_or_else(|error| panic!("{os:?}: {error}"));
        let view = CodeView::read(&emission.record).unwrap();
        // LLVM's Mach-O printer emits one byte so two symbols cannot share this atom's address.
        let expected: &[u8] = if os == blaze_tier::Os::MacOs { &[0] } else { &[] };
        assert_eq!(view.relro(), expected, "{os:?}");
        assert!(view.relro_align() >= 6, "{os:?}");
        assert_eq!(view.sites(), 2, "{os:?}");
        assert_eq!([view.site(0).kind, view.site(1).kind], [SiteKind::Page21, SiteKind::AddLo12], "{os:?}");
        for site in [view.site(0), view.site(1)] {
            assert_eq!((site.target, site.addend), (OWN_RELRO, 0), "{os:?}");
        }

        #[cfg(target_arch = "aarch64")]
        if os == host.os {
            let placed = place_leaf(&view, module.symbol);
            let addr = usize::try_from(placed.data_address()).unwrap();
            assert_ne!(addr, 0);
            assert_eq!(addr % 64, 0);
            // SAFETY: this leaf has the declared C ABI and returns its placed data address.
            let entry: unsafe extern "C" fn() -> *const u8 = unsafe { placed.as_fn() };
            // SAFETY: the placed executable is live; the returned pointer is never dereferenced.
            assert_eq!(unsafe { entry() }.addr(), addr);
        }
    }

    let machine = blaze_tier::Machine { os: blaze_tier::Os::Linux, ..host };
    for (asm, kind, expected) in [
        (r"adrp $0, empty+1", SiteKind::Page21, 1),
        (r"adrp $0, empty\0Aldrb ${0:w}, [$0, :lo12:empty]", SiteKind::LdstLo12_1, 0),
    ] {
        let module = object_fixture(&blaze_own::format_in!(alloc, r#"
{DATA}define i64 @entry() uwtable {{
  %result = call i64 asm sideeffect "{asm}", "=r"()
  ret i64 %result
}}
"#));
        let error = emit_release(ReleaseTask { module: &module, imports: &[], opt: 2, machine }, alloc)
            .err().expect("the zero-byte section has no loadable bytes");
        assert!(matches!(error, EmitError::Read(ReadError::LocalRelroBounds {
            kind: actual, offset, section_len: 0, relro_at, relro_len: 0, ..
        }) if (actual, offset, relro_at) == (kind, expected, expected)), "unexpected refusal {error:?}");
    }
}

#[test]
fn release_record_keeps_source_positions_from_an_inlined_import() {
    use super::{emit::EmitError, object::ReadError};
    use blaze_code::record::{OWN_RELRO, PositionField, PositionPart};
    use blaze_lir::builder::{DataPosition, MemAttrs};

    const ITEM: u128 = 0x1aba_2c23_3e25_429e;
    const SITE: u32 = 17;
    let alloc = blaze_own::testing::heap().alloc();
    let root = prepare(Symbol([2; 32]), caller().verified(), &ByPlace, alloc).unwrap();
    let middle_symbol = Symbol([7; 32]);
    let mut b = Builder::<_, Full>::new_in(64, alloc);
    let p = Param::of(Ty::I32);
    let sig = b.sig(Conv::C, Variadic::No, &[p], &[p]);
    b.set_signature(sig);
    b.set_attrs(FnAttrs::default().with_inline(Inline::Always));
    let fields = [
        DataPosition { offset: 0, item: ITEM, site: SITE, field: blaze_lir::PositionField::Line },
        DataPosition { offset: 4, item: ITEM, site: SITE, field: blaze_lir::PositionField::Col },
    ];
    let data = b.data_with_positions(&[0; 8], 2, &[], &fields);
    let entry = b.create_block(BlockKind::Normal);
    b.begin(entry);
    b.param(Ty::I32);
    let address = b.data_addr(data);
    let line = b.load(Ty::I32, Flags::VOLATILE, address, 0, MemAttrs::aligned(2));
    let col = b.load(Ty::I32, Flags::VOLATILE, address, 4, MemAttrs::aligned(2));
    let sum = b.binary(blaze_lir::Op::Add, Flags::NONE, line, col);
    b.ret(&[sum]);
    let body = b.finish_verified().unwrap();

    for os in [blaze_tier::Os::Linux, blaze_tier::Os::MacOs] {
        let machine = blaze_tier::Machine { os, ..machine() };
        let before = emit_release(ReleaseTask { module: &root, imports: &[], opt: 2, machine }, alloc).unwrap();
        let before = CodeView::read(&before.record).unwrap();
        assert_eq!(before.positions(), 0, "{os:?}");
        assert_eq!(before.targets(), 1, "{os:?}");
        assert_eq!(before.target(0), Target { space: Space::Func, symbol: middle_symbol }, "{os:?}");

        let mut middle = prepare(middle_symbol, body.verified(), &ByPlace, alloc).unwrap();
        let made = emit_release(ReleaseTask { module: &root, imports: &[&middle], opt: 2, machine }, alloc).unwrap();
        let view = CodeView::read(&made.record).unwrap();
        assert_eq!(view.targets(), 0, "{os:?}: the call was inlined");
        assert_eq!(view.positions(), 2, "{os:?}");
        let line = view.position(0);
        let col = view.position(1);
        assert_eq!((line.part, line.item, line.site, line.field), (PositionPart::Relro, ITEM, SITE, PositionField::Line));
        assert_eq!((col.part, col.item, col.site, col.field), (PositionPart::Relro, ITEM, SITE, PositionField::Col));
        assert_eq!(col.at, line.at + 4, "{os:?}");
        assert_eq!(&view.relro()[line.at as usize..col.at as usize + 4], &[0; 8], "{os:?}");
        assert!((0..view.sites()).any(|i| {
            let site = view.site(i);
            site.kind == SiteKind::Page21 && site.target == OWN_RELRO && site.addend == line.at
        }), "{os:?}: the inlined body reads its source-position data");

        // LLVM sees identical IR, but the reader can no longer identify its writable global.
        middle.positions.clear();
        let refused = emit_release(ReleaseTask { module: &root, imports: &[&middle], opt: 2, machine }, alloc);
        assert!(matches!(refused, Err(EmitError::Read(ReadError::Writable { .. }))), "{os:?}");
    }
}

/// The backend's identity names the record schema and the lowering/reader's revision: both
/// are material to the digest, so a schema change or a reader repair is different work and a
/// recipe keyed on the old identity is not reused. The revision is build.rs's BLAKE3 over the
/// implementation's source files, so it is never empty.
#[test]
fn identity_names_the_schema_and_the_implementation_revision() {
    let machine = machine();
    let real = identity(machine, 2);
    assert_eq!(real, identity(machine, 2), "identity is a pure function of its inputs");
    assert_ne!(real, identity(machine, 3), "the pipeline is material");
    let magic = &blaze_code::record::CFI_MAGIC;
    let revision = env!("BLAZE_EMIT_NATIVE_REVISION");
    assert_eq!(real, identity_of(machine, 2, magic, revision));
    assert_ne!(real, identity_of(machine, 2, &blaze_code::record::MAGIC, revision), "the record schema is material");
    assert_ne!(real, identity_of(machine, 2, magic, "a repaired reader"), "the implementation revision is material");
    assert_eq!(env!("BLAZE_EMIT_NATIVE_REVISION").len(), 64, "build.rs hashes the implementation to 64 hex digits");
    assert!(env!("BLAZE_EMIT_NATIVE_REVISION").bytes().all(|b| b.is_ascii_hexdigit()));
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
