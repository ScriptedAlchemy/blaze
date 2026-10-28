//! What ra-jit emits code for. The target is an input of the process, named by its triple
//! (`ra-jit --target <triple>`), and the host when nothing names one. Everything in the
//! backend that differs between platforms asks the [`Target`] for the property it depends
//! on (architecture, object format, calling convention, unwind tables) and never asks what
//! the daemon itself was built for.

use std::sync::OnceLock;

use cranelift_codegen::{
    ir::{Type, types},
    isa::{self, CallConv, OwnedTargetIsa},
    settings::{self, Configurable},
};

static SELECTED: OnceLock<Target> = OnceLock::new();

#[derive(Debug)]
pub struct Target {
    triple: String,
    pub arch: Arch,
    pub object_format: ObjectFormat,
    /// The convention of `extern "C"`, which the Rust conventions are laid over.
    pub call_conv: CallConv,
    pub unwind: UnwindFormat,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Arch {
    X86_64,
    Aarch64,
    /// WebAssembly with 32-bit addresses.
    Wasm32,
}

impl Arch {
    /// How long a function is at least, and the byte one that is shorter is filled up with:
    /// the length of a jump from its entry to anywhere a replacement of it may be. Nothing
    /// when every function is that long as it is.
    pub fn shortest_function(self) -> Option<(usize, u8)> {
        match self {
            // `jmp rel32`, filled up with `int3`.
            Arch::X86_64 => Some((5, 0xcc)),
            // `adrp x16`, `add x16` and `br x16`, as `b` reaches only 128 MB, filled up with
            // `udf #0`.
            Arch::Aarch64 => Some((12, 0)),
            // A function of a module is replaced by its slot in the table, not by a jump.
            Arch::Wasm32 => None,
        }
    }
}

/// Where the variable arguments of a C function are when it is entered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VariableArguments {
    /// In the registers the named parameters left, then on the stack: System V for x86-64
    /// and AArch64.
    Registers,
    /// In the integer registers the named parameters left, which the function writes to the
    /// room the caller keeps below the ones on the stack: Windows x64.
    Homed,
    /// On the stack, eight bytes each, whatever registers are left: Apple's AArch64.
    Stack,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObjectFormat {
    Elf,
    MachO,
    Coff,
    /// Relocatable WebAssembly objects.
    Wasm,
}

impl ObjectFormat {
    /// What a label starts with for the assembler to keep it out of the symbols.
    pub(super) fn local_label(self) -> &'static str {
        match self {
            ObjectFormat::MachO => "L",
            ObjectFormat::Elf | ObjectFormat::Coff | ObjectFormat::Wasm => ".L",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnwindFormat {
    /// DWARF call frame information in `.eh_frame`, landing pads in `.gcc_except_table`.
    EhFrame,
    /// The unwind codes of Windows in `.pdata` and `.xdata`, see `unwind::seh`.
    Seh,
    /// Nothing unwinds: a panic aborts.
    None,
}

/// How wasm code calls a function it names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Calls {
    /// `call`, as rustc's code does.
    Direct,
    /// `call_indirect` through the slot the linker gives the function in the function
    /// table, so that a module loaded later replaces the function by writing its slot.
    Table,
}

impl Target {
    /// Names the target of this process. It is said once, before anything is compiled.
    pub fn select(triple: &str) -> Result<&'static Target, String> {
        let target = Target::parse(triple)?;
        let selected = SELECTED.get_or_init(|| {
            hir_ty::mir::target::select_word(target.word());
            target
        });
        if selected.triple == triple {
            Ok(selected)
        } else {
            Err(format!("the target is {} already, not {triple}", selected.triple))
        }
    }

    /// The target that was named, or the host.
    pub fn selected() -> Result<&'static Target, String> {
        match SELECTED.get() {
            Some(target) => Ok(target),
            None => Target::select(Target::host()),
        }
    }

    /// The triple of what the daemon itself runs on.
    pub fn host() -> &'static str {
        // The wasm backend writes its own code from Cranelift's IR and only borrows the
        // settings of an ISA Cranelift has, whatever the compiler itself runs on.
        if cfg!(feature = "wasm") {
            return "wasm32-unknown-unknown";
        }
        // FIXME: the triples of the hosts that have no code emitted for them yet are missing,
        // and `parse` turns those down by name.
        match (std::env::consts::ARCH, std::env::consts::OS) {
            ("x86_64", "linux") => "x86_64-unknown-linux-gnu",
            ("aarch64", "linux") => "aarch64-unknown-linux-gnu",
            ("aarch64", "macos") => "aarch64-apple-darwin",
            ("x86_64", "macos") => "x86_64-apple-darwin",
            ("x86_64", "windows") => "x86_64-pc-windows-msvc",
            ("aarch64", "windows") => "aarch64-pc-windows-msvc",
            _ => "unknown",
        }
    }

    fn parse(triple: &str) -> Result<Target, String> {
        let mut parts = triple.split('-');
        let arch = match parts.next() {
            Some("x86_64") => Arch::X86_64,
            Some("aarch64") => Arch::Aarch64,
            Some("wasm32") => Arch::Wasm32,
            _ => return Err(format!("ra-jit emits no code for the architecture of {triple}")),
        };
        let system = parts.find(|part| matches!(*part, "linux" | "darwin" | "windows"));
        // Cranelift's names for the C convention: `SystemV` is the psABI's on x86-64 and
        // AAPCS64 on AArch64, which Apple's differs from in what goes on the stack.
        let (object_format, unwind, call_conv) = match (system, arch) {
            // The calling convention is the one of the ISA whose settings are borrowed.
            (_, Arch::Wasm32) => (ObjectFormat::Wasm, UnwindFormat::None, CallConv::SystemV),
            (Some("linux"), _) => (ObjectFormat::Elf, UnwindFormat::EhFrame, CallConv::SystemV),
            (Some("darwin"), Arch::Aarch64) => {
                (ObjectFormat::MachO, UnwindFormat::EhFrame, CallConv::AppleAarch64)
            }
            (Some("windows"), Arch::X86_64) if triple.ends_with("-msvc") => {
                (ObjectFormat::Coff, UnwindFormat::Seh, CallConv::WindowsFastcall)
            }
            // FIXME: x86-64 Mach-O and AArch64 PE have no splice written for them, and the
            // GNU flavour of Windows unwinds another way than the tables of `unwind::seh`.
            _ => return Err(format!("ra-jit emits no code for the system of {triple}")),
        };
        Ok(Target { triple: triple.to_owned(), arch, object_format, call_conv, unwind })
    }

    /// The target of the process, once it is known that there is one: nothing is compiled
    /// before [`Target::selected`] has said so.
    pub fn current() -> &'static Target {
        Target::selected().unwrap_or_else(|error| panic!("{error}"))
    }

    pub fn is_wasm(&self) -> bool {
        self.arch == Arch::Wasm32
    }

    /// Whether an object is written as a relocatable wasm object, for a linker. A page links
    /// what the compiler emits itself, and reads the object Cranelift's writer made.
    pub fn writes_wasm_objects(&self) -> bool {
        self.is_wasm() && self.calls() == Calls::Direct
    }

    /// How wasm code calls: through the table where functions are replaced in a page.
    pub fn calls(&self) -> Calls {
        if cfg!(feature = "wasm") { Calls::Table } else { Calls::Direct }
    }

    /// The integer an address is.
    pub(super) fn pointer(&self) -> Type {
        match self.arch {
            Arch::X86_64 | Arch::Aarch64 => types::I64,
            Arch::Wasm32 => types::I32,
        }
    }

    /// How many bytes an address takes.
    pub(crate) fn word(&self) -> usize {
        self.pointer().bytes() as usize
    }

    pub fn triple(&self) -> &str {
        &self.triple
    }

    /// Whether a call can name its landing pad and the pad is entered with the exception in a
    /// register, as Cranelift has it for the calling conventions it knows the register of.
    pub fn exception_in_register(&self) -> bool {
        self.call_conv.supports_exceptions()
    }

    /// Where a function that takes `...` finds its variable arguments, for the conventions a
    /// definition of one is compiled for.
    pub fn variable_arguments(&self) -> Option<VariableArguments> {
        match (self.call_conv, self.arch) {
            (CallConv::SystemV, Arch::X86_64 | Arch::Aarch64) => Some(VariableArguments::Registers),
            (CallConv::WindowsFastcall, Arch::X86_64) => Some(VariableArguments::Homed),
            (CallConv::AppleAarch64, Arch::Aarch64) => Some(VariableArguments::Stack),
            _ => None,
        }
    }

    /// Whether code reads the frame pointer, which every function then has to keep.
    fn reads_frame_pointer(&self) -> bool {
        // The pads of a convention Cranelift has no landing pads for take the exception
        // from the frame (see `unwind::seh`), which the frame pointer finds. Variable
        // arguments that are all on the stack are found from it too.
        !self.exception_in_register() || self.variable_arguments() == Some(VariableArguments::Stack)
    }

    /// The processor code is made for: the baseline of the target, as rustc compiles for.
    pub fn cpu(&self) -> &'static str {
        match (self.arch, self.object_format) {
            (Arch::X86_64, _) => "x86-64",
            (Arch::Wasm32, _) => "generic",
            (Arch::Aarch64, ObjectFormat::MachO) => "apple-m1",
            (Arch::Aarch64, _) => "generic",
        }
    }

    /// What LLVM is told of how a frame larger than a page reaches the stack it needs. A PE
    /// image has `__chkstk` for it, which LLVM calls unasked; elsewhere the code touches
    /// each page itself.
    pub fn stack_probe(&self) -> Option<&'static str> {
        (self.object_format != ObjectFormat::Coff).then_some("inline-asm")
    }

    /// The section a static with a name gets to itself, where a symbol does not say how
    /// large it is and the section does. Without one a static is as large as the next thing
    /// in its section is far, which counts what that is aligned by. The linker merges
    /// these into the section before the `$`, and the name is not one the writer of the
    /// object would put anything else in.
    pub fn section_of_static(&self, writable: bool) -> Option<&'static str> {
        match self.object_format {
            ObjectFormat::Coff => Some(if writable { ".data$static" } else { ".rdata$static" }),
            ObjectFormat::Elf | ObjectFormat::MachO | ObjectFormat::Wasm => None,
        }
    }

    /// The code generator for the target. `regalloc` names Cranelift's register allocator.
    /// With `frame_pointers` every function it compiles keeps one, which a function that
    /// asks for its frame pointer needs.
    pub(super) fn isa(
        &self,
        regalloc: &str,
        frame_pointers: bool,
    ) -> Result<OwnedTargetIsa, String> {
        self.isa_for(regalloc, frame_pointers, &Default::default())
    }

    pub(super) fn isa_for(
        &self,
        regalloc: &str,
        frame_pointers: bool,
        options: &hir_ty::mir::codegen_options::TargetMachine,
    ) -> Result<OwnedTargetIsa, String> {
        let mut flags = settings::builder();
        // An image of PE reaches all of itself relative to the instruction, and what it
        // imports through the stubs the linker adds: there is no table of addresses to go
        // through.
        let (pic, tls_model) = match self.object_format {
            ObjectFormat::Elf => ("true", "elf_gd"),
            ObjectFormat::MachO => ("true", "macho"),
            ObjectFormat::Coff => ("false", "coff"),
            ObjectFormat::Wasm => ("true", "elf_gd"),
        };
        flags.set("is_pic", pic).map_err(|e| e.to_string())?;
        flags.set("tls_model", tls_model).map_err(|e| e.to_string())?;
        if self.object_format == ObjectFormat::Coff {
            // Windows commits a stack one page at a time, as the guard page below the last
            // is touched: a frame larger than a page has to touch each on its way down.
            flags.set("enable_probestack", "true").map_err(|e| e.to_string())?;
            flags.set("probestack_strategy", "inline").map_err(|e| e.to_string())?;
        }
        if frame_pointers || self.reads_frame_pointer() {
            // Cranelift gives a function its frame pointer only when all of them keep one.
            flags.set("preserve_frame_pointers", "true").map_err(|e| e.to_string())?;
        }
        let optimization = match options.opt_level.as_deref() {
            Some("s" | "z") => "speed_and_size",
            Some("1" | "2" | "3") => "speed",
            _ => "none",
        };
        flags.set("opt_level", optimization).map_err(|e| e.to_string())?;
        // Cranelift checks the IR of every function it is given unless told not to.
        let verify = if cfg!(debug_assertions) { "true" } else { "false" };
        flags.set("enable_verifier", verify).map_err(|e| e.to_string())?;
        flags.set("enable_llvm_abi_extensions", "true").map_err(|e| e.to_string())?;
        flags.set("regalloc_algorithm", regalloc).map_err(|e| e.to_string())?;
        // Cranelift has no ISA for WebAssembly: its code is written from the IR, and the
        // settings of this one are what the IR is made under.
        let triple = match self.arch {
            Arch::Wasm32 => "x86_64-unknown-linux-gnu",
            Arch::X86_64 | Arch::Aarch64 => &self.triple,
        };
        let mut isa = isa::lookup_by_name(triple).map_err(|e| e.to_string())?;
        if self.arch == Arch::X86_64 {
            // Only a function of `#[target_feature(enable = "cmpxchg16b")]` compares and
            // swaps 128 bits, as `core::arch::x86_64::cmpxchg16b` does.
            isa.enable("has_cmpxchg16b").map_err(|e| e.to_string())?;
        }
        if let Some(cpu) = options.cpu.as_deref() {
            // x86 Cranelift CPU presets are real instruction-set presets. AArch64 has
            // no CPU presets; its CPU tuning remains an LLVM setting, not guessed here.
            if self.arch == Arch::X86_64 && cpu != "generic" && cpu != "x86-64" {
                if let Err(error) = isa.enable(cpu) {
                    eprintln!("codegen: Cranelift has no target-cpu preset for {cpu}: {error}");
                }
            } else if self.arch == Arch::Aarch64 && cpu != "generic" {
                eprintln!("codegen: target-cpu={cpu} tuning is not applied by Cranelift");
            }
        }
        for (feature, enabled) in &options.features {
            let setting = match (self.arch, feature.as_str()) {
                (Arch::Aarch64, "lse") => Some("has_lse"),
                (Arch::Aarch64, "fp16") => Some("has_fp16"),
                (Arch::Aarch64, "dotprod") => Some("has_dotprod"),
                (Arch::Aarch64, "i8mm") => Some("has_i8mm"),
                (Arch::Aarch64, "paca" | "pacg") => Some("has_pauth"),
                (Arch::X86_64, "sse3") => Some("has_sse3"),
                (Arch::X86_64, "ssse3") => Some("has_ssse3"),
                (Arch::X86_64, "sse4.1") => Some("has_sse41"),
                (Arch::X86_64, "sse4.2") => Some("has_sse42"),
                (Arch::X86_64, "avx") => Some("has_avx"),
                (Arch::X86_64, "avx2") => Some("has_avx2"),
                (Arch::X86_64, "fma") => Some("has_fma"),
                (Arch::X86_64, "bmi1") => Some("has_bmi1"),
                (Arch::X86_64, "bmi2") => Some("has_bmi2"),
                (Arch::X86_64, "lzcnt") => Some("has_lzcnt"),
                (Arch::X86_64, "popcnt") => Some("has_popcnt"),
                (Arch::X86_64, "cmpxchg16b") => Some("has_cmpxchg16b"),
                _ => None,
            };
            match setting {
                Some(setting) => isa
                    .set(setting, if *enabled { "true" } else { "false" })
                    .map_err(|error| error.to_string())?,
                None => eprintln!(
                    "codegen: target-feature={}{feature} is not applied by Cranelift",
                    if *enabled { '+' } else { '-' }
                ),
            }
        }
        isa.finish(settings::Flags::new(flags)).map_err(|e| e.to_string())
    }
}

#[cfg(test)]
mod machine_tests {
    use super::*;

    #[test]
    fn lse_flags_change_the_actual_cranelift_target_without_mutating_the_baseline() {
        let target = Target::parse("aarch64-unknown-linux-gnu").unwrap();
        let enabled = hir_ty::mir::codegen_options::TargetMachine::from_flags(&[
            "-Ctarget-feature=+lse".to_owned(),
        ]);
        let disabled = hir_ty::mir::codegen_options::TargetMachine::from_flags(&[
            "-Ctarget-feature=-lse".to_owned(),
        ]);
        let yes = target.isa_for("backtracking", false, &enabled).unwrap();
        let no = target.isa_for("backtracking", false, &disabled).unwrap();
        let setting = |isa: &OwnedTargetIsa| {
            isa.isa_flags().into_iter().find(|flag| flag.name == "has_lse").unwrap().as_bool()
        };
        assert_eq!(setting(&yes), Some(true));
        assert_eq!(setting(&no), Some(false));
        assert_eq!(setting(&target.isa("backtracking", false).unwrap()), Some(false));
    }

    #[test]
    fn a_named_cpu_preset_configures_the_x86_instruction_selector() {
        let target = Target::parse("x86_64-unknown-linux-gnu").unwrap();
        let options = hir_ty::mir::codegen_options::TargetMachine::from_flags(&[
            "-Ctarget-cpu=haswell".to_owned(),
        ]);
        let isa = target.isa_for("backtracking", false, &options).unwrap();
        let avx2 = isa.isa_flags().into_iter().find(|flag| flag.name == "has_avx2").unwrap();
        assert_eq!(avx2.as_bool(), Some(true));
        let baseline = target.isa("backtracking", false).unwrap();
        let avx2 = baseline.isa_flags().into_iter().find(|flag| flag.name == "has_avx2").unwrap();
        assert_eq!(avx2.as_bool(), Some(false));
    }

    #[test]
    fn a_cpu_preset_changes_emitted_popcount_instructions() {
        use cranelift_codegen::ir::{AbiParam, Function, InstBuilder, UserFuncName};
        use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext};

        let target = Target::parse("x86_64-unknown-linux-gnu").unwrap();
        let options = hir_ty::mir::codegen_options::TargetMachine::from_flags(&[
            "-Ctarget-cpu=haswell".to_owned(),
        ]);
        let configured = target.isa_for("backtracking", false, &options).unwrap();
        let baseline = target.isa("backtracking", false).unwrap();
        let emit = |isa: &OwnedTargetIsa| {
            let mut signature = cranelift_codegen::ir::Signature::new(isa.default_call_conv());
            signature.params.push(AbiParam::new(types::I64));
            signature.returns.push(AbiParam::new(types::I64));
            let mut function = Function::with_name_signature(UserFuncName::default(), signature);
            let mut context = FunctionBuilderContext::new();
            let mut builder = FunctionBuilder::new(&mut function, &mut context);
            let block = builder.create_block();
            builder.append_block_params_for_function_params(block);
            builder.switch_to_block(block);
            builder.seal_block(block);
            let input = builder.block_params(block)[0];
            let count = builder.ins().popcnt(input);
            builder.ins().return_(&[count]);
            builder.finalize(isa.frontend_config());
            let mut context = cranelift_codegen::Context::for_function(function);
            context.compile(&**isa, &mut Default::default()).unwrap();
            context.compiled_code().unwrap().code_buffer().to_vec()
        };
        let cpu = emit(&configured);
        let generic = emit(&baseline);
        let popcnt = |bytes: &[u8]| bytes.windows(2).any(|bytes| bytes == b"\x0f\xb8");
        assert!(popcnt(&cpu), "Haswell must select POPCNT, not only record its name");
        assert!(!popcnt(&generic), "the generic target must use its software lowering");
        assert_ne!(cpu, generic);
    }

    #[test]
    fn opt_level_selects_real_backend_passes() {
        let target = Target::parse("aarch64-unknown-linux-gnu").unwrap();
        let options =
            hir_ty::mir::codegen_options::TargetMachine::from_flags(&["-Copt-level=3".to_owned()]);
        let optimized = target.isa_for("backtracking", false, &options).unwrap();
        let baseline = target.isa("backtracking", false).unwrap();
        assert_ne!(optimized.flags().opt_level(), baseline.flags().opt_level());
    }
}
