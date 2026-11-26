#include "abi.h"
#include <llvm/ADT/StringSet.h>
#include <llvm/Analysis/TargetLibraryInfo.h>
#include <llvm/IR/Module.h>
#include <optional>

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

struct LLVMBlazeOpaqueLibcalls {
  const llvm::Module &Module;
  llvm::TargetLibraryInfoImpl Library;
  llvm::TargetLibraryInfo Info;
  llvm::StringSet<> Inputs;

  explicit LLVMBlazeOpaqueLibcalls(const llvm::Module &M)
      : Module(M), Library(M.getTargetTriple()), Info(Library) {
    const bool MachO = M.getTargetTriple().isOSBinFormatMachO();
    for (const llvm::GlobalValue &Value : M.global_values()) {
      llvm::StringRef Name = Value.getName();
      if (Name.consume_front("\1") && MachO)
        Name.consume_front("_");
      if (!Name.empty())
        Inputs.insert(Name);
    }
  }

  std::optional<llvm::LibFunc> available(llvm::StringRef Name) const {
    if (Name.empty())
      return std::nullopt;
    llvm::LibFunc Kind;
    if (Info.getLibFunc(Name, Kind) && Info.getName(Kind) == Name)
      return Kind;
    // getLibFunc looks up the canonical spelling. The target may emit a
    // different spelling, such as __exp10 on Darwin.
    for (unsigned I = 0; I < llvm::NumLibFuncs; ++I) {
      Kind = static_cast<llvm::LibFunc>(I);
      if (Info.getName(Kind) == Name)
        return Kind;
    }
    return std::nullopt;
  }

  bool synthesized(llvm::StringRef Name) const {
    if (Inputs.contains(Name))
      return false;
    const auto Kind = available(Name);
    if (!Kind)
      return false;
    // IR passes add library declarations, while codegen can emit a call
    // directly (for example when it lowers llvm.memcpy).
    const llvm::GlobalValue *Value = Module.getNamedValue(Name);
    if (!Value)
      return true;
    const auto *Function = llvm::dyn_cast<llvm::Function>(Value);
    return Function && Function->isDeclaration() &&
           Function->getCallingConv() == llvm::CallingConv::C &&
           Info.isValidProtoForLibFunc(*Function->getFunctionType(), *Kind, Module);
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
