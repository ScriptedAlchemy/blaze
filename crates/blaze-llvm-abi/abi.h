/* The emitter's header surface. LLVM declarations come only from the pinned prefix. */
#ifndef BLAZE_LLVM_ABI_H
#define BLAZE_LLVM_ABI_H

#include <llvm-c/Core.h>
#include <llvm-c/Error.h>
#include <llvm-c/IRReader.h>
#include <llvm-c/Linker.h>
#include <llvm-c/Target.h>
#include <llvm-c/TargetMachine.h>

/* The emitter's native wrappers. build.rs includes this header before their implementations. */
typedef struct LLVMBlazeOpaquePasses *LLVMBlazePassesRef;
#ifdef __cplusplus
extern "C" {
#endif
void LLVMBlazeInitializeAArch64(void);
LLVMBlazePassesRef LLVMBlazeCreatePasses(LLVMTargetMachineRef Machine);
void LLVMBlazeDisposePasses(LLVMBlazePassesRef Passes);
void LLVMBlazeRequireDwarfUnwind(LLVMTargetMachineRef Machine);
LLVMErrorRef LLVMBlazeRunPasses(LLVMBlazePassesRef Passes, LLVMModuleRef M,
                              const char *Pipeline, size_t Length,
                              LLVMBool SLPVectorization);
#ifdef __cplusplus
}
#endif

#endif
