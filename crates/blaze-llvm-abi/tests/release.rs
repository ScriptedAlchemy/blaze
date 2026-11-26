//! Exercise the pinned parser, custom optimization pipeline and target machine through
//! the owned declarations. Both supported object formats are produced by the real LLVM.
use blaze_llvm_abi::*;
use std::{ffi::CStr, ptr, slice};

#[test]
fn release_pipeline_emits_both_arm64_object_formats() {
    // SAFETY: one test initializes LLVM's global target registry before creating machines.
    // Every input C string is static; all returned LLVM objects stay live until disposed
    // below, with passes destroyed before their target machine and context.
    unsafe {
        LLVMInitializeAArch64TargetInfo();
        LLVMInitializeAArch64Target();
        LLVMInitializeAArch64TargetMC();
        LLVMInitializeAArch64AsmPrinter();
        for (triple, magic) in [
            (c"aarch64-apple-darwin", &[0xcf, 0xfa, 0xed, 0xfe]),
            (c"aarch64-unknown-linux-gnu", b"\x7fELF"),
        ] {
            let context = LLVMContextCreate();
            let ir = c"define i32 @answer(i32 %x) { %sum = add i32 %x, 42 ret i32 %sum }";
            let buffer = LLVMCreateMemoryBufferWithMemoryRangeCopy(
                ir.as_ptr(),
                ir.to_bytes().len(),
                c"owned-abi-test".as_ptr(),
            );
            let mut module = ptr::null_mut();
            let mut message = ptr::null_mut();
            assert_eq!(LLVMParseIRInContext2(context, buffer, &mut module, &mut message), 0);
            LLVMDisposeMemoryBuffer(buffer);
            let mut target = ptr::null_mut();
            assert_eq!(LLVMGetTargetFromTriple(triple.as_ptr(), &mut target, &mut message), 0);
            let machine = LLVMCreateTargetMachine(
                target,
                triple.as_ptr(),
                c"generic".as_ptr(),
                c"".as_ptr(),
                LLVMCodeGenOptLevel::LLVMCodeGenLevelAggressive,
                LLVMRelocMode::LLVMRelocPIC,
                LLVMCodeModel::LLVMCodeModelSmall,
            );
            assert!(!machine.is_null());
            LLVMSetTarget(module, triple.as_ptr());
            let data = LLVMCreateTargetDataLayout(machine);
            let layout = LLVMCopyStringRepOfTargetData(data);
            LLVMSetDataLayout(module, layout);
            LLVMDisposeMessage(layout);
            LLVMDisposeTargetData(data);
            let passes = LLVMBlazeCreatePasses(machine);
            let pipeline = c"default<O2>";
            let error =
                LLVMBlazeRunPasses(passes, module, pipeline.as_ptr(), pipeline.to_bytes().len(), 1);
            if !error.is_null() {
                let text = LLVMGetErrorMessage(error);
                let why = CStr::from_ptr(text).to_string_lossy().into_owned();
                LLVMDisposeErrorMessage(text);
                panic!("release pipeline: {why}");
            }
            let mut object = ptr::null_mut();
            assert_eq!(
                LLVMTargetMachineEmitToMemoryBuffer(
                    machine,
                    module,
                    LLVMCodeGenFileType::LLVMObjectFile,
                    &mut message,
                    &mut object
                ),
                0
            );
            let bytes = slice::from_raw_parts(
                LLVMGetBufferStart(object).cast::<u8>(),
                LLVMGetBufferSize(object),
            );
            assert!(
                bytes.starts_with(magic),
                "{}: expected native object magic",
                triple.to_string_lossy()
            );
            assert!(bytes.windows(6).any(|name| name == b"answer"));
            println!("{}: {} bytes from pinned LLVM O2", triple.to_string_lossy(), bytes.len());
            LLVMDisposeMemoryBuffer(object);
            LLVMBlazeDisposePasses(passes);
            LLVMDisposeModule(module);
            LLVMDisposeTargetMachine(machine);
            LLVMContextDispose(context);
        }
    }
}
