#include "abi.h"

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
