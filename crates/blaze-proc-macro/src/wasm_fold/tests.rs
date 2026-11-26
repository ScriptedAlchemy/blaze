use super::{Error, Plan};
use wasm_encoder::{CodeSection, ExportKind, ExportSection, Function, FunctionSection, Instruction, Module, TypeSection, ValType};

fn fixture(lose_constant: bool) -> std::vec::Vec<u8> {
    let mut module = Module::new();
    let mut types = TypeSection::new();
    types.ty().function([ValType::I64], [ValType::I64]);
    let mut functions = FunctionSection::new();
    let mut exports = ExportSection::new();
    let mut code = CodeSection::new();
    for n in 1..=64 {
        functions.function(0);
        exports.export(&format!("member{n}"), ExportKind::Func, n - 1);
        let mut function = Function::new([(1, ValType::I64)]);
        function.instruction(&Instruction::LocalGet(0));
        function.instruction(&Instruction::LocalSet(1));
        for _ in 0..16 {
            function.instruction(&Instruction::LocalGet(1));
            function.instruction(&Instruction::I64Const(if lose_constant { 1 } else { i64::from(n) }));
            function.instruction(&Instruction::I64Xor);
            function.instruction(&Instruction::I64Const(3));
            function.instruction(&Instruction::I64Mul);
            function.instruction(&Instruction::I64Const(5));
            function.instruction(&Instruction::I64Rotl);
            function.instruction(&Instruction::LocalSet(1));
        }
        function.instruction(&Instruction::LocalGet(1));
        function.instruction(&Instruction::End);
        code.function(&function);
    }
    module.section(&types).section(&functions).section(&exports).section(&code);
    module.finish()
}

fn oracle(wasm: &[u8]) {
    let engine = wasmtime::Engine::default();
    let module = wasmtime::Module::new(&engine, wasm).expect("validated module compiles");
    let mut store = wasmtime::Store::new(&engine, ());
    let instance = wasmtime::Instance::new(&mut store, &module, &[]).expect("instantiate");
    for n in 1..=64u64 {
        let function = instance.get_typed_func::<u64, u64>(&mut store, &format!("member{n}")).expect("original export");
        for input in [0, 11, u64::MAX, 0xdeadbeef01234567] {
            let mut expected = input;
            for _ in 0..16 { expected = (expected ^ n).wrapping_mul(3).rotate_left(5); }
            assert_eq!(function.call(&mut store, input).expect("member executes"), expected, "member result {n}");
        }
    }
}

#[test]
fn folded_members_preserve_constants_and_shift_locals() {
    let alloc = blaze_own::testing::heap().alloc();
    let input = fixture(false);
    let plan = Plan::new(&input, alloc).expect("plan");
    assert_eq!(plan.groups().len(), 1);
    assert_eq!(plan.groups()[0].members().count(), 64);
    assert_eq!(plan.groups()[0].parameters(), 16);
    assert!(plan.ineligible().is_empty());
    let folded = plan.assemble().expect("assembled output validates");
    assert!(folded.len() < input.len());
    assert_eq!(folded, plan.assemble().expect("deterministic output"));
    oracle(&input);
    oracle(&folded);
}

#[test]
#[should_panic(expected = "member result")]
fn the_execution_oracle_rejects_losing_member_constants() {
    let alloc = blaze_own::testing::heap().alloc();
    let input = fixture(true);
    let plan = Plan::new(&input, alloc).expect("plan");
    oracle(&plan.assemble().expect("the planted output is still valid wasm"));
}

#[test]
fn unknown_custom_metadata_is_refused_and_input_bytes_are_validated() {
    let alloc = blaze_own::testing::heap().alloc();
    let input = fixture(false);
    let mut with_offsets = input.clone();
    // A custom section named "x" could contain code offsets. Never silently retain it.
    with_offsets.extend_from_slice(&[0, 2, 1, b'x']);
    assert!(matches!(Plan::new(&with_offsets, alloc), Err(Error::Metadata)));
    assert!(matches!(Plan::new(&input[..input.len() - 1], alloc), Err(Error::Invalid(_))));
}
