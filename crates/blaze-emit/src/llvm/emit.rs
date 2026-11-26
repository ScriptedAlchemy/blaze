//! LLVM emission of a prepared module: the pipeline, the target machine, the object file.
//!
//! [`emit_release`] runs LLVM's own `O2` pipeline over the module the lowering lane's `prepare`
//! made and emits it with LLVM's AArch64 code generator to an object file, which
//! [`super::object::read`] turns into a sealed `bzcode07` record. [`emit_fold`] does the same
//! for a folded group: one body and one thunk per other member. Both mint an [`Emission`] whose
//! `backend` is `Some(..)`: the one place a record is named release or folded work, so a
//! `--release` export claims only what LLVM actually made.
//!
//! # What LLVM is asked for
//!
//! - The target is the host's, as `Machine` names it: the same machine the SOTA tier emits for,
//!   so a record of either runs on the same CPU.
//! - The pipeline is `default<O2>`: LLVM's full inlining, vectorization and loop passes, as
//!   rustc's `-C opt-level=2` runs them. `O3` is the task's to ask for.
//! - The code model is small and the relocation PIC: an exported file has no GOT of its own,
//!   so every outside reference is a `bl`/`adrp` the record names by symbol, which the export's
//!   layout resolves in place.

use core::ffi::CStr;
use core::ptr;

use blaze_code::record::Symbol;
use blaze_ids::{ContentId, Hasher};
use blaze_own::{AVec, Alloc, CollectIn};
use blaze_pins::{LLVM_COMMIT, LLVM_VERSION};
use blaze_tier::{Arch, Machine, Os};
use llvm_sys::{
    core::{LLVMContextCreate, LLVMContextDispose, LLVMCreateMemoryBufferWithMemoryRangeCopy, LLVMDisposeMemoryBuffer, LLVMDisposeMessage, LLVMDisposeModule, LLVMGetBufferStart, LLVMGetBufferSize},
    error::{LLVMDisposeErrorMessage, LLVMGetErrorMessage},
    ir_reader::LLVMParseIRInContext2,
    linker::LLVMLinkModules2,
    prelude::{LLVMContextRef, LLVMModuleRef},
    target_machine::LLVMTargetMachineRef,
    target::{LLVMInitializeAArch64Target, LLVMInitializeAArch64TargetInfo, LLVMInitializeAArch64TargetMC},
    target_machine::{
        LLVMCodeGenFileType, LLVMCodeGenOptLevel, LLVMCodeModel, LLVMCreateTargetMachine, LLVMDisposeTargetMachine, LLVMGetTargetFromTriple, LLVMRelocMode, LLVMTargetMachineEmitToMemoryBuffer,
    },
    transforms::pass_builder::{LLVMBlazeCreatePasses, LLVMBlazeDisposePasses, LLVMBlazePassesRef, LLVMBlazeRunPasses},
};

use super::LlvmModule;

/// Which backend made a record. `Some(..)` is minted only by [`emit_release`] and
/// [`emit_fold`]: a record is release or folded work only when LLVM made it here.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Backend {
    /// One release blob: the module optimized and emitted for the target machine.
    ReleaseBlob,
    /// A folded group's body or one of its member thunks.
    FoldedOutput,
}

/// One record LLVM made, sealed and ready for the store.
pub struct Emission<'a> {
    /// The sealed `bzcode07` record's bytes (`Sealed::encode`'s encoding).
    pub record: AVec<'a, u8>,
    /// The backend that made it; `Some(..)` only from this module.
    pub backend: Backend,
    /// The symbol the record is of.
    pub symbol: Symbol,
}

const IDENTITY_DOMAIN: &str = "blaze-ladder native LLVM identity 1";

/// What backend work is: the target triple, the CPU and features, the pinned LLVM (its
/// version and commit), the pipeline and its flags. Two emissions that differ in any of these
/// are different work, so the driver keys a recipe on this before any module exists.
pub fn identity(machine: Machine, opt: u8) -> ContentId {
    let (triple, cpu) = target_of(machine);
    let pipeline = pipeline_of(opt);
    let mut hash = Hasher::new(IDENTITY_DOMAIN);
    hash.part(triple.to_bytes()).part(cpu.to_bytes()).part(&machine.features.to_le_bytes());
    hash.part(LLVM_VERSION.as_bytes()).part(LLVM_COMMIT.as_bytes());
    hash.part(pipeline.to_bytes());
    // The flags the target machine is made with: the code model, the relocation and the
    // optimization level LLVM's own API is given.
    hash.part(c"small".to_bytes()).part(c"pic".to_bytes()).part(c"aggressive".to_bytes());
    hash.finish()
}

/// A release task: the root module, its imports in the planner's order (each linked into
/// the root's module before optimizing, so LLVM inlines across them), the optimization level
/// and the target machine.
pub struct ReleaseTask<'a> {
    pub module: &'a LlvmModule<'a>,
    /// The ordered import closure (`backend_plan::Release::imports`), each a prepared module.
    pub imports: &'a [&'a LlvmModule<'a>],
    /// `2` or `3`: LLVM's `O2`/`O3` pipeline.
    pub opt: u8,
    pub machine: Machine,
}

/// One member of a folded group: a prepared module and whether it is the body (the rewritten
/// body, whose code the thunks share) or a thunk (one per original member, which holds its
/// constants and jumps into the body). The planner (`backend_plan::Fold`) names each.
pub struct FoldMember<'a> {
    pub module: &'a LlvmModule<'a>,
    pub is_body: bool,
}

/// A folded group's task: its members (each a prepared module) and the target machine.
pub struct FoldTask<'a> {
    pub members: &'a [FoldMember<'a>],
    pub machine: Machine,
}

/// What a folded group made: its body and one thunk per other member.
pub struct FoldOutput<'a> {
    pub body: Emission<'a>,
    pub thunks: AVec<'a, Emission<'a>>,
}

/// Why LLVM could not make a record.
#[derive(Debug, PartialEq, Eq)]
pub enum EmitError {
    /// The IR did not parse.
    Parse { at: u32 },
    /// LLVM has no target for the machine's triple.
    NoTarget,
    /// An import's IR did not link into the root's module.
    Link,
    /// The pipeline refused the module.
    Pipeline,
    /// The code generator refused the module.
    Codegen,
    /// The object file could not be read back as a record.
    Read(super::object::ReadError),
}

impl core::fmt::Display for EmitError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            EmitError::Parse { at } => write!(f, "LLVM did not parse the module's IR near byte {at}"),
            EmitError::NoTarget => write!(f, "LLVM has no target for the machine"),
            EmitError::Link => write!(f, "an import did not link into the module"),
            EmitError::Pipeline => write!(f, "LLVM's pipeline refused the module"),
            EmitError::Codegen => write!(f, "LLVM's code generator refused the module"),
            EmitError::Read(why) => write!(f, "LLVM's object file: {why}"),
        }
    }
}

impl core::error::Error for EmitError {}

/// The LLVM context, module, target machine and passes of one emission, disposed together.
struct Session {
    context: LLVMContextRef,
    module: LLVMModuleRef,
    machine: LLVMTargetMachineRef,
    passes: LLVMBlazePassesRef,
}

/// The pipeline `opt` asks for.
fn pipeline_of(opt: u8) -> &'static CStr {
    if opt >= 3 { c"default<O3>" } else { c"default<O2>" }
}

/// The triple and CPU of `machine`, as LLVM names them: the host's, as the SOTA tier emits for.
fn target_of(machine: Machine) -> (&'static CStr, &'static CStr) {
    match (machine.arch, machine.os) {
        (Arch::Arm64, Os::MacOs) => (c"arm64-apple-macosx14", c"apple-m1"),
        (Arch::Arm64, Os::Linux) => (c"aarch64-unknown-linux-gnu", c"generic"),
        (Arch::X86_64, Os::MacOs) => (c"x86_64-apple-macosx14", c"apple-m1"),
        (Arch::X86_64, Os::Linux) => (c"x86_64-unknown-linux-gnu", c"x86-64"),
    }
}

impl Session {
    /// Parses `ir`, links each of `imports` into it in order (so LLVM sees the whole closure
    /// as one module and can inline across it), and makes the target machine and passes for
    /// `machine`.
    fn new(ir: &[u8], imports: &[&[u8]], machine: Machine) -> Result<Session, EmitError> {
        // SAFETY: the AArch64 target is registered once per process; LLVM's own guards make
        // a second call a no-op.
        unsafe {
            LLVMInitializeAArch64TargetInfo();
            LLVMInitializeAArch64Target();
            LLVMInitializeAArch64TargetMC();
        }
        // SAFETY: a new context, disposed in `Drop`.
        let context = unsafe { LLVMContextCreate() };
        // SAFETY: `ir` is copied into the buffer, which outlives the parse and is disposed below.
        let buffer = unsafe { LLVMCreateMemoryBufferWithMemoryRangeCopy(ir.as_ptr().cast(), ir.len(), c"module".as_ptr()) };
        let mut module = ptr::null_mut();
        let mut parse_error = ptr::null_mut();
        // SAFETY: `buffer` and `context` are live; `module` and `parse_error` are out-parameters.
        let parsed = unsafe { LLVMParseIRInContext2(context, buffer, &mut module, &mut parse_error) };
        if parsed != 0 || module.is_null() {
            if !parse_error.is_null() {
                // SAFETY: LLVM allocated `parse_error`.
                unsafe { LLVMDisposeMessage(parse_error) };
            }
            // SAFETY: `buffer` and `context` were made above and are used no further.
            unsafe {
                LLVMDisposeMemoryBuffer(buffer);
                LLVMContextDispose(context);
            }
            return Err(EmitError::Parse { at: 0 });
        }
        // SAFETY: `buffer` is no longer needed once the module is parsed.
        unsafe { LLVMDisposeMemoryBuffer(buffer) };
        for imported in imports {
            // SAFETY: the import's bytes are copied into a buffer disposed after its parse.
            let buffer = unsafe { LLVMCreateMemoryBufferWithMemoryRangeCopy(imported.as_ptr().cast(), imported.len(), c"import".as_ptr()) };
            let mut other = ptr::null_mut();
            let mut link_error = ptr::null_mut();
            // SAFETY: `buffer` and `context` are live; `other` and `link_error` are out-parameters.
            let parsed = unsafe { LLVMParseIRInContext2(context, buffer, &mut other, &mut link_error) };
            if parsed != 0 || other.is_null() {
                if !link_error.is_null() {
                    // SAFETY: LLVM allocated `link_error`.
                    unsafe { LLVMDisposeMessage(link_error) };
                }
                // SAFETY: `buffer`, `module` and `context` were made above.
                unsafe {
                    LLVMDisposeMemoryBuffer(buffer);
                    LLVMDisposeModule(module);
                    LLVMContextDispose(context);
                }
                return Err(EmitError::Parse { at: 0 });
            }
            // SAFETY: `buffer` is no longer needed once `other` is parsed.
            unsafe { LLVMDisposeMemoryBuffer(buffer) };
            // SAFETY: `other` is consumed by the link whether it succeeds or not.
            let linked = unsafe { LLVMLinkModules2(module, other) };
            if linked != 0 {
                // SAFETY: `module` and `context` were made above.
                unsafe {
                    LLVMDisposeModule(module);
                    LLVMContextDispose(context);
                }
                return Err(EmitError::Link);
            }
        }
        let (triple, cpu) = target_of(machine);
        let mut target = ptr::null_mut();
        let mut target_error = ptr::null_mut();
        // SAFETY: `triple` is NUL-terminated; `target` and `target_error` are out-parameters.
        let found = unsafe { LLVMGetTargetFromTriple(triple.as_ptr().cast(), &mut target, &mut target_error) };
        if found != 0 || target.is_null() {
            if !target_error.is_null() {
                // SAFETY: LLVM allocated `target_error`.
                unsafe { LLVMDisposeMessage(target_error) };
            }
            // SAFETY: `module` and `context` were made above.
            unsafe {
                LLVMDisposeModule(module);
                LLVMContextDispose(context);
            }
            return Err(EmitError::NoTarget);
        }
        // SAFETY: `target` is a live target; the triple, CPU and features are NUL-terminated.
        let machine_ref = unsafe {
            LLVMCreateTargetMachine(
                target,
                triple.as_ptr(),
                cpu.as_ptr(),
                // An empty feature string, never null: LLVM assigns it to a `std::string`.
                c"".as_ptr(),
                LLVMCodeGenOptLevel::LLVMCodeGenLevelAggressive,
                LLVMRelocMode::LLVMRelocPIC,
                LLVMCodeModel::LLVMCodeModelSmall,
            )
        };
        // SAFETY: the passes are made for `machine_ref` and disposed with it.
        let passes = unsafe { LLVMBlazeCreatePasses(machine_ref) };
        Ok(Session { context, module, machine: machine_ref, passes })
    }

    /// Runs the `O2`/`O3` pipeline over the module and emits it to an object file.
    fn emit(self, opt: u8, alloc: Alloc<'_>) -> Result<AVec<'_, u8>, EmitError> {
        let pipeline = pipeline_of(opt);
        // SAFETY: `self.passes` is live for `self.module`; the pipeline is NUL-terminated.
        let error = unsafe { LLVMBlazeRunPasses(self.passes, self.module, pipeline.as_ptr(), pipeline.to_bytes().len(), 1) };
        if !error.is_null() {
            // SAFETY: `error` is LLVM's; its message is read then disposed.
            let message = unsafe { LLVMGetErrorMessage(error) };
            // SAFETY: LLVM allocated `message`.
            unsafe { LLVMDisposeErrorMessage(message) };
            return Err(EmitError::Pipeline);
        }
        let mut out = ptr::null_mut();
        let mut message = ptr::null_mut();
        // SAFETY: `self.machine` and `self.module` are live; `out` and `message` are out-parameters.
        let failed = unsafe { LLVMTargetMachineEmitToMemoryBuffer(self.machine, self.module, LLVMCodeGenFileType::LLVMObjectFile, &mut message, &mut out) };
        if failed != 0 || out.is_null() {
            return Err(EmitError::Codegen);
        }
        // SAFETY: `out` is a live memory buffer; its bytes are copied before it is disposed.
        let bytes: AVec<'_, u8> = unsafe {
            let start = LLVMGetBufferStart(out);
            let size = LLVMGetBufferSize(out);
            AVec::from_slice_in(core::slice::from_raw_parts(start.cast::<u8>(), size), alloc)
        };
        // SAFETY: `out` is disposed once, after its bytes are copied.
        unsafe { LLVMDisposeMemoryBuffer(out) };
        Ok(bytes)
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        // SAFETY: each was made in `new` and is disposed once here.
        unsafe {
            LLVMBlazeDisposePasses(self.passes);
            LLVMDisposeTargetMachine(self.machine);
            LLVMDisposeModule(self.module);
            LLVMContextDispose(self.context);
        }
    }
}

/// Emits one release blob: the module optimized at `task.opt` and emitted for `task.machine`.
pub fn emit_release<'a>(task: ReleaseTask<'_>, alloc: Alloc<'a>) -> Result<Emission<'a>, EmitError> {
    let imports: AVec<'_, &[u8]> = task.imports.iter().map(|m| m.ir.as_slice()).collect_in(alloc);
    let session = Session::new(task.module.ir.as_slice(), &imports, task.machine)?;
    let object = session.emit(task.opt, alloc)?;
    let record = super::object::read(&object, task.module, alloc).map_err(EmitError::Read)?;
    Ok(Emission { record, backend: Backend::ReleaseBlob, symbol: task.module.symbol })
}

/// Emits a folded group: its body and one thunk per other member.
pub fn emit_fold<'a>(task: FoldTask<'_>, alloc: Alloc<'a>) -> Result<FoldOutput<'a>, EmitError> {
    let mut body: Option<Emission<'a>> = None;
    let mut thunks: AVec<'a, Emission<'a>> = AVec::new_in(alloc);
    for member in task.members {
        let session = Session::new(member.module.ir.as_slice(), &[], task.machine)?;
        let object = session.emit(2, alloc)?;
        let record = super::object::read(&object, member.module, alloc).map_err(EmitError::Read)?;
        let emission = Emission { record, backend: Backend::FoldedOutput, symbol: member.module.symbol };
        if member.is_body {
            body = Some(emission);
        } else {
            thunks.push(emission);
        }
    }
    body.map(|body| FoldOutput { body, thunks }).ok_or(EmitError::Codegen)
}
