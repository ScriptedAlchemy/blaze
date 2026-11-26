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
use blaze_own::{AVec, Alloc, CollectIn, FixedString};
use blaze_pins::{LLVM_COMMIT, LLVM_VERSION};
use blaze_tier::{Arch, Machine, Os};
use blaze_llvm_abi::{
    LLVMBlazeCreatePasses, LLVMBlazeDisposePasses, LLVMBlazePassesRef, LLVMBlazeRunPasses,
    LLVMCodeGenFileType, LLVMCodeGenOptLevel, LLVMCodeModel, LLVMContextCreate, LLVMContextDispose,
    LLVMContextRef, LLVMCopyStringRepOfTargetData, LLVMCreateMemoryBufferWithMemoryRangeCopy,
    LLVMCreateTargetDataLayout, LLVMCreateTargetMachine, LLVMDisposeErrorMessage,
    LLVMDisposeMemoryBuffer, LLVMDisposeMessage, LLVMDisposeModule, LLVMDisposeTargetData,
    LLVMDisposeTargetMachine, LLVMErrorRef, LLVMGetBufferSize, LLVMGetBufferStart,
    LLVMGetErrorMessage, LLVMGetTargetFromTriple, LLVMInitializeAArch64AsmPrinter,
    LLVMInitializeAArch64Target, LLVMInitializeAArch64TargetInfo, LLVMInitializeAArch64TargetMC,
    LLVMLinkModules2, LLVMModuleRef, LLVMParseIRInContext2, LLVMRelocMode, LLVMSetDataLayout,
    LLVMSetTarget, LLVMTargetMachineEmitToMemoryBuffer, LLVMTargetMachineRef,
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
/// version and commit), the pipeline and its flags, the record schema the output is written
/// in, and the revision of the lowering and reader that make it. Two emissions that differ in
/// any of these are different work, so the driver keys a recipe on this before any module
/// exists.
pub fn identity(machine: Machine, opt: u8) -> ContentId {
    identity_of(machine, opt, &blaze_code::record::MAGIC, NATIVE_REVISION)
}

/// [`identity`] over the record schema and the implementation revision it is given, so a test
/// can prove both are material to the digest.
pub(super) fn identity_of(machine: Machine, opt: u8, magic: &[u8; 8], revision: &str) -> ContentId {
    let (triple, cpu) = target_of(machine);
    let pipeline = pipeline_of(opt);
    let mut hash = Hasher::new(IDENTITY_DOMAIN);
    hash.part(triple.to_bytes()).part(cpu.to_bytes()).part(&machine.features.to_le_bytes());
    hash.part(LLVM_VERSION.as_bytes()).part(LLVM_COMMIT.as_bytes());
    hash.part(pipeline.to_bytes());
    // The flags the target machine is made with: the code model, the relocation and the
    // optimization level LLVM's own API is given.
    hash.part(c"small".to_bytes()).part(c"pic".to_bytes()).part(c"aggressive".to_bytes());
    // The record's schema: a record is read by the magic it carries, so a change to it is a
    // different output. The lowering and reader's revision (build.rs, over their source
    // files): the same LLVM and pipeline make different bytes when the lowering or the reader
    // is repaired, and a recipe keyed on the old identity must not be reused for them.
    hash.part(magic).part(revision.as_bytes());
    hash.finish()
}

/// The lowering and reader's revision, as build.rs hashes their source files
/// (`BLAZE_EMIT_NATIVE_REVISION`): a repair to any of them changes this, so it changes
/// [`identity`]. `env!`, not `option_env!`: a build that did not run build.rs has no revision
/// and does not compile, rather than minting records under an identity that never changes.
const NATIVE_REVISION: &str = env!("BLAZE_EMIT_NATIVE_REVISION");

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
    /// The IR did not parse; LLVM's message.
    Parse { message: FixedString<256> },
    /// LLVM has no target for the machine's triple; LLVM's message.
    NoTarget { message: FixedString<256> },
    /// An import's IR did not link into the root's module.
    Link,
    /// The pipeline refused the module; LLVM's message.
    Pipeline { message: FixedString<256> },
    /// The code generator refused the module; LLVM's message.
    Codegen { message: FixedString<256> },
    /// The object file could not be read back as a record.
    Read(super::object::ReadError),
    /// A folded group named no body among its members.
    NoBody,
}

impl core::fmt::Display for EmitError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            EmitError::Parse { message } => write!(f, "LLVM did not parse the module's IR: {}", message.as_str()),
            EmitError::NoTarget { message } => write!(f, "LLVM has no target for the machine: {}", message.as_str()),
            EmitError::Link => write!(f, "an import did not link into the module"),
            EmitError::Pipeline { message } => write!(f, "LLVM's pipeline refused the module: {}", message.as_str()),
            EmitError::Codegen { message } => write!(f, "LLVM's code generator refused the module: {}", message.as_str()),
            EmitError::Read(why) => write!(f, "LLVM's object file: {why}"),
            EmitError::NoBody => write!(f, "the folded group names no body among its members"),
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

/// LLVM's message for `error`, which is disposed: read into a `FixedString`, cut at a
/// character's boundary if it does not fit.
fn llvm_message(error: LLVMErrorRef) -> FixedString<256> {
    // SAFETY: `error` is LLVM's. `LLVMGetErrorMessage` consumes it and hands back its text
    // as a C string, which stays valid until `LLVMDisposeErrorMessage` below.
    let text = unsafe { LLVMGetErrorMessage(error) };
    let message = c_message(text);
    // SAFETY: `text` is what `LLVMGetErrorMessage` allocated; disposed once.
    unsafe { LLVMDisposeErrorMessage(text) };
    message
}

/// The message in a `char**` LLVM filled (`LLVMParseIRInContext2`, `LLVMGetTargetFromTriple`),
/// which is disposed: read into a `FixedString`, empty for null.
fn c_dispose(text: *mut core::ffi::c_char) -> FixedString<256> {
    let message = c_message(text);
    if !text.is_null() {
        // SAFETY: `text` is what LLVM allocated for this out-parameter; disposed once.
        unsafe { LLVMDisposeMessage(text) };
    }
    message
}

/// A NUL-terminated C string into a `FixedString`, cut at a character's boundary if it does
/// not fit; an empty string for null.
fn c_message(text: *const core::ffi::c_char) -> FixedString<256> {
    let mut message = FixedString::<256>::new();
    if text.is_null() {
        return message;
    }
    // SAFETY: `text` is a live NUL-terminated string.
    let text = unsafe { CStr::from_ptr(text) }.to_string_lossy();
    let mut end = text.len().min(256);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    message.try_push_str(&text[..end]).ok();
    message
}

impl Session {
    /// Parses `ir`, links each of `imports` into it in order (so LLVM sees the whole closure
    /// as one module and can inline across it), and makes the target machine and passes for
    /// `machine`.
    fn new(ir: &[u8], imports: &[&[u8]], machine: Machine) -> Result<Session, EmitError> {
        // SAFETY: the AArch64 target is registered once per process; LLVM's own guards make
        // a second call a no-op. The AsmPrinter is registered too: `LLVMTargetMachineEmitToMemoryBuffer`
        // writes the object file through it, and a target without one has no object to emit
        // (the pipeline then succeeds and the emission fails).
        unsafe {
            LLVMInitializeAArch64TargetInfo();
            LLVMInitializeAArch64Target();
            LLVMInitializeAArch64TargetMC();
            LLVMInitializeAArch64AsmPrinter();
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
            let message = c_dispose(parse_error);
            // SAFETY: `buffer` and `context` were made above and are used no further.
            unsafe {
                LLVMDisposeMemoryBuffer(buffer);
                LLVMContextDispose(context);
            }
            return Err(EmitError::Parse { message });
        }
        // SAFETY: `buffer` is no longer needed once the module is parsed.
        unsafe { LLVMDisposeMemoryBuffer(buffer) };
        let (triple, cpu) = target_of(machine);
        let mut target = ptr::null_mut();
        let mut target_error = ptr::null_mut();
        // SAFETY: `triple` is NUL-terminated; `target` and `target_error` are out-parameters.
        let found = unsafe { LLVMGetTargetFromTriple(triple.as_ptr().cast(), &mut target, &mut target_error) };
        if found != 0 || target.is_null() {
            let message = c_dispose(target_error);
            // SAFETY: `module` and `context` were made above.
            unsafe {
                LLVMDisposeModule(module);
                LLVMContextDispose(context);
            }
            return Err(EmitError::NoTarget { message });
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
        // The module's own triple and layout, from the target machine. The lowering lane's IR
        // has no module header (it starts at the first `define`), so without this the module
        // keeps an empty triple and the default layout: the pipeline would lay out for no
        // target, and the code generator would be handed a module that is not its target's.
        // SAFETY: `module` is live; `triple` is NUL-terminated; the layout is the machine's
        // own, read into the module and then disposed.
        unsafe {
            LLVMSetTarget(module, triple.as_ptr());
            let layout = LLVMCreateTargetDataLayout(machine_ref);
            let spelled = LLVMCopyStringRepOfTargetData(layout);
            LLVMSetDataLayout(module, spelled);
            LLVMDisposeMessage(spelled);
            LLVMDisposeTargetData(layout);
        }
        for imported in imports {
            // SAFETY: the import's bytes are copied into a buffer disposed after its parse.
            let buffer = unsafe { LLVMCreateMemoryBufferWithMemoryRangeCopy(imported.as_ptr().cast(), imported.len(), c"import".as_ptr()) };
            let mut other = ptr::null_mut();
            let mut link_error = ptr::null_mut();
            // SAFETY: `buffer` and `context` are live; `other` and `link_error` are out-parameters.
            let parsed = unsafe { LLVMParseIRInContext2(context, buffer, &mut other, &mut link_error) };
            if parsed != 0 || other.is_null() {
                let message = c_dispose(link_error);
                // SAFETY: `buffer`, `module`, `machine_ref` and `context` were made above.
                unsafe {
                    LLVMDisposeMemoryBuffer(buffer);
                    LLVMDisposeTargetMachine(machine_ref);
                    LLVMDisposeModule(module);
                    LLVMContextDispose(context);
                }
                return Err(EmitError::Parse { message });
            }
            // SAFETY: `buffer` is no longer needed once `other` is parsed.
            unsafe { LLVMDisposeMemoryBuffer(buffer) };
            // The import's own triple and layout, from the same target machine: the lowering
            // lane's IR has no module header, so without this the linker warns that it is
            // linking two modules of different layouts and the import keeps the default one.
            // SAFETY: `other` is live; `triple` is NUL-terminated; the layout is the machine's.
            unsafe {
                LLVMSetTarget(other, triple.as_ptr());
                let layout = LLVMCreateTargetDataLayout(machine_ref);
                let spelled = LLVMCopyStringRepOfTargetData(layout);
                LLVMSetDataLayout(other, spelled);
                LLVMDisposeMessage(spelled);
                LLVMDisposeTargetData(layout);
            }
            // SAFETY: `other` is consumed by the link whether it succeeds or not.
            let linked = unsafe { LLVMLinkModules2(module, other) };
            if linked != 0 {
                // SAFETY: `module`, `machine_ref` and `context` were made above.
                unsafe {
                    LLVMDisposeTargetMachine(machine_ref);
                    LLVMDisposeModule(module);
                    LLVMContextDispose(context);
                }
                return Err(EmitError::Link);
            }
        }
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
            return Err(EmitError::Pipeline { message: llvm_message(error) });
        }
        let mut out = ptr::null_mut();
        let mut message = ptr::null_mut();
        // SAFETY: `self.machine` and `self.module` are live; `out` and `message` are out-parameters.
        let failed = unsafe { LLVMTargetMachineEmitToMemoryBuffer(self.machine, self.module, LLVMCodeGenFileType::LLVMObjectFile, &mut message, &mut out) };
        if failed != 0 || out.is_null() {
            // `LLVMTargetMachineEmitToMemoryBuffer` reports through a `char**`, not an
            // `LLVMErrorRef`: the string is LLVM's, read then disposed.
            let text = if message.is_null() { FixedString::new() } else { c_message(message) };
            return Err(EmitError::Codegen { message: text });
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
    body.map(|body| FoldOutput { body, thunks }).ok_or(EmitError::NoBody)
}
