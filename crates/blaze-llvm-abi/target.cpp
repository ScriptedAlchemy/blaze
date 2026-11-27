#include "abi.h"
#include <llvm/ADT/StringSet.h>
#include <llvm/Analysis/TargetLibraryInfo.h>
#include <llvm/IR/InlineAsm.h>
#include <llvm/IR/InstIterator.h>
#include <llvm/IR/Instructions.h>
#include <llvm/IR/Module.h>

extern "C" void LLVMBlazeInitializeAArch64(void) {
  // LLVM's TargetRegistry requires registration to finish before any concurrent
  // lookup. The C++ local static publishes the complete registry to every caller.
  static const bool Registered = [] {
    LLVMInitializeAArch64TargetInfo();
    LLVMInitializeAArch64Target();
    LLVMInitializeAArch64TargetMC();
    LLVMInitializeAArch64AsmPrinter();
    LLVMInitializeAArch64AsmParser();
    return true;
  }();
  (void)Registered;
}

static bool isRegisterOnlyAssembly(const llvm::CallBase &Call,
                                   const llvm::InlineAsm &Asm) {
  const auto *Type = Asm.getFunctionType();
  if (!llvm::isa<llvm::CallInst>(Call) || !Call.arg_empty() ||
      Type->getNumParams() != 0 || Type->isVarArg() ||
      Call.getCallingConv() != llvm::CallingConv::C ||
      !Asm.hasSideEffects() || Asm.isAlignStack() || Asm.canThrow() ||
      Asm.getDialect() != llvm::InlineAsm::AD_ATT)
    return false;

  // The indexed TLS read only prints its allocated output register, and exact
  // yield has no operands. Neither template can name an external symbol.
  return (Asm.getAsmString() == "mrs $0, tpidrro_el0" &&
          Asm.getConstraintString() == "=r" &&
          Type->getReturnType()->isIntegerTy(64)) ||
         (Asm.getAsmString() == "yield" && Asm.getConstraintString().empty() &&
          Type->getReturnType()->isVoidTy());
}

static bool hasOpaqueAssembly(const llvm::Module &Module) {
  if (!Module.getModuleInlineAsm().empty())
    return true;
  for (const llvm::Function &Function : Module) {
    for (const llvm::Instruction &Instruction : llvm::instructions(Function)) {
      const auto *Call = llvm::dyn_cast<llvm::CallBase>(&Instruction);
      if (!Call)
        continue;
      const auto *Asm = llvm::dyn_cast<llvm::InlineAsm>(Call->getCalledOperand());
      if (Asm && !isRegisterOnlyAssembly(*Call, *Asm))
        return true;
    }
  }
  return false;
}

struct LLVMBlazeOpaqueLibcalls {
  const llvm::Module &Module;
  llvm::TargetLibraryInfoImpl Library;
  llvm::TargetLibraryInfo Info;
  llvm::StringSet<> Inputs;
  const bool OpaqueAssembly;

  explicit LLVMBlazeOpaqueLibcalls(const llvm::Module &M)
      : Module(M), Library(M.getTargetTriple()), Info(Library),
        OpaqueAssembly(hasOpaqueAssembly(M)) {
    const bool MachO = M.getTargetTriple().isOSBinFormatMachO();
    for (const llvm::GlobalValue &Value : M.global_values()) {
      llvm::StringRef Name = Value.getName();
      if (Name.consume_front("\1") && MachO)
        Name.consume_front("_");
      if (!Name.empty())
        Inputs.insert(Name);
    }
  }

  bool synthesized(llvm::StringRef Name) const {
    // Assembly can name a target without an LLVM global, so an absent input
    // name proves compiler origin only when the input assembly is understood.
    if (OpaqueAssembly || Inputs.contains(Name))
      return false;
    llvm::LibFunc Kind;
    if (!Info.getLibFunc(Name, Kind) || Info.getName(Kind) != Name)
      return false;

    // Only these runtime routines have the required ABI and system-library
    // binding on our native exporters. TLI availability alone does not prove
    // that a libm or compiler-runtime name is safe to bind here.
    switch (Kind) {
    case llvm::LibFunc_bcmp:
    case llvm::LibFunc_memcmp:
    case llvm::LibFunc_memcpy:
    case llvm::LibFunc_memmove:
    case llvm::LibFunc_memset:
      break;
    default:
      return false;
    }
    // IR passes add library declarations, while codegen can emit a call
    // directly (for example when it lowers llvm.memcpy).
    const llvm::GlobalValue *Value = Module.getNamedValue(Name);
    if (!Value)
      return true;
    const auto *Function = llvm::dyn_cast<llvm::Function>(Value);
    return Function && Function->isDeclaration() &&
           Function->getCallingConv() == llvm::CallingConv::C &&
           Info.isValidProtoForLibFunc(*Function->getFunctionType(), Kind, Module);
  }
};

extern "C" LLVMBlazeLibcallsRef LLVMBlazeCreateLibcalls(LLVMModuleRef Module) {
  return new LLVMBlazeOpaqueLibcalls(*llvm::unwrap(Module));
}

extern "C" void LLVMBlazeDisposeLibcalls(LLVMBlazeLibcallsRef Libcalls) {
  delete Libcalls;
}

extern "C" LLVMBool LLVMBlazeIsSynthesizedLibcall(LLVMBlazeLibcallsRef Libcalls,
                                                const char *Name, size_t Length) {
  return Libcalls->synthesized(llvm::StringRef(Name, Length));
}
