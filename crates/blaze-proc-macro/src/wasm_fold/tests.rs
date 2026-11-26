use super::{Error, Plan, Record};
use crate::tests::{name, op, section};

fn fixture(lose_constant: bool) -> std::vec::Vec<u8> {
    const COUNT: u8 = 64;
    const I64: u8 = 0x7e;
    const GET: u8 = 0x20;
    const SET: u8 = 0x21;
    const CONST: u8 = 0x42;
    const XOR: u8 = 0x85;
    const MUL: u8 = 0x7e;
    const ROTL: u8 = 0x89;
    let mut module = b"\0asm\x01\0\0\0".to_vec();
    section(1, &[1, 0x60, 1, I64, 1, I64], &mut module);
    let mut functions = [0; COUNT as usize + 1]; // Every function has type 0.
    functions[0] = COUNT;
    section(3, &functions, &mut module);
    let mut exports = vec![COUNT];
    let mut code = vec![COUNT];
    for n in 1..=COUNT {
        name(&format!("member{n}"), &mut exports);
        exports.extend_from_slice(&[0, n - 1]); // An exported function and its index.
        let mut function = vec![1, 1, I64, GET, 0, SET, 1]; // One i64 local, initialized from the argument.
        for _ in 0..16 {
            function.extend_from_slice(&[GET, 1, CONST]);
            op::sleb(if lose_constant { 1 } else { i64::from(n) }, &mut function);
            function.extend_from_slice(&[XOR, CONST, 3, MUL, CONST, 5, ROTL, SET, 1]);
        }
        function.extend_from_slice(&[GET, 1, op::END]);
        op::uleb(function.len() as u64, &mut code);
        code.extend_from_slice(&function);
    }
    section(7, &exports, &mut module);
    section(10, &code, &mut module);
    module
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

#[test]
fn assembly_requires_every_distinct_granted_body_of_one_group() {
    let alloc = blaze_own::testing::heap().alloc();
    let input = fixture(false);
    let plan = Plan::new(&input, alloc).unwrap();
    let work = plan.work().unwrap();
    assert_eq!(work.len(), 65, "one template and 64 distinct thunks");
    assert!(work.windows(2).all(|pair| pair[0].key() < pair[1].key()));
    let mut records = blaze_own::AVec::new_in(alloc);
    // Delivery order cannot change either the group or the final module.
    for task in work.iter().rev() { records.push(plan.emit(task).unwrap()); }
    records.sort_unstable_by_key(|record| record.key);
    let views = blaze_own::AVec::from_iter_in(records.iter().map(Record::view), alloc);
    assert_eq!(plan.assemble_records(&views).unwrap(), plan.assemble().unwrap());
    assert!(matches!(plan.assemble_records(&views[1..]), Err(Error::Record(_))), "an absent holder cannot be replaced by a local tail");
    let mut duplicates = blaze_own::AVec::from_slice_in(&views, alloc);
    duplicates[1] = duplicates[0];
    assert!(matches!(plan.assemble_records(&duplicates), Err(Error::Record(_))));

    let changed = fixture(true);
    let other = Plan::new(&changed, alloc).unwrap();
    assert!(matches!(other.emit(&work[0]), Err(Error::Record(_))), "a recipe cannot be reused against changed constants");
    assert!(matches!(other.assemble_records(&views), Err(Error::Record(_))), "old outputs cannot satisfy the edited module");
}
