/* The emitter's header surface. LLVM declarations come only from the pinned prefix. */
#include <llvm-c/Core.h>
#include <llvm-c/Error.h>
#include <llvm-c/IRReader.h>
#include <llvm-c/Linker.h>
#include <llvm-c/Target.h>
#include <llvm-c/TargetMachine.h>

/* The entry points owned in vendor/llvm-sys/wrappers/passes.cpp.
 * build.rs includes this header before that implementation, checking the declarations. */
typedef struct LLVMBlazeOpaquePasses *LLVMBlazePassesRef;
#ifdef __cplusplus
extern "C" {
#endif
LLVMBlazePassesRef LLVMBlazeCreatePasses(LLVMTargetMachineRef Machine);
void LLVMBlazeDisposePasses(LLVMBlazePassesRef Passes);
void LLVMBlazeRequireDwarfUnwind(LLVMTargetMachineRef Machine);
LLVMErrorRef LLVMBlazeRunPasses(LLVMBlazePassesRef Passes, LLVMModuleRef M,
                              const char *Pipeline, size_t Length,
                              LLVMBool SLPVectorization);
#ifdef __cplusplus
}
#endif
