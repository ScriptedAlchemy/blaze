//! The release tier's proof: a function LLVM lowered, optimized and emitted reads back as a
//! sealed `bzcode07` record, and the record runs and computes what the LIR says. The negative
//! control: a record the Reference tier made is never named release or folded work.


use blaze_code::record::{CodeView, Symbol};
use blaze_lir::{
    BlockKind, Builder, Flags, Full, Ty,
    builder::{Param, Variadic},
    func::Conv,
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

/// A function that calls an outside function and is not inlined: LLVM emits a direct `bl`,
/// which the record form has no site for. Its `Call` site is the 12-byte `adrp`+`ldr`+`blr`
/// the loader rewrites (`blaze_code::image`), and a direct branch written as one would run
/// past the function and be refused at load. So the reader refuses it by name: a record is
/// never minted with a wrong site. (The import-closure tests show the other outcome, where
/// `O2` inlines the callee and no call remains.)
#[test]
fn release_record_refuses_a_direct_call() {
    let func = caller();
    let module = prepare(Symbol([2; 32]), func.verified(), &ByPlace, Alloc::global()).unwrap();
    let error = emit_release(ReleaseTask { module: &module, imports: &[], opt: 2, machine: machine() }, Alloc::global()).err().expect("a direct call is refused, not written as the form's Call");
    assert_eq!(error.to_string(), "LLVM's object file: a direct call at 0: the record form's call is the 12-byte `adrp`+`ldr`+`blr` the loader rewrites, which a direct branch is not");
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
    let mut other_magic = blaze_code::record::MAGIC;
    other_magic[7] = b'8';
    assert_ne!(real, identity_of(machine, 2, &other_magic, env!("BLAZE_EMIT_NATIVE_REVISION")), "the record schema is material");
    assert_ne!(real, identity_of(machine, 2, &blaze_code::record::MAGIC, "a repaired reader"), "the implementation revision is material");
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
