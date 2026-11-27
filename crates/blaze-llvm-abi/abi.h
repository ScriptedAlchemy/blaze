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
typedef struct LLVMBlazeOpaqueLibcalls *LLVMBlazeLibcallsRef;
#ifdef __cplusplus
extern "C" {
#endif
void LLVMBlazeInitializeAArch64(void);
LLVMBlazeLibcallsRef LLVMBlazeCreateLibcalls(LLVMModuleRef Module);
void LLVMBlazeDisposeLibcalls(LLVMBlazeLibcallsRef Libcalls);
LLVMBool LLVMBlazeIsSynthesizedLibcall(LLVMBlazeLibcallsRef Libcalls,
                                      const char *Name, size_t Length);
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
