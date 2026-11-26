use super::*;
use blaze_code::record::CodeRecord;
use blaze_ladder::backend_plan::{
    Backend, Function, Unit as BackendUnit,
    execute::{Output, OutputKind},
};

#[test]
fn accepted_native_grants_require_every_bound_record_from_that_publication() {
    let heap = blaze_own::testing::heap();
    let alloc = heap.alloc();
    let id = ContentId([11; 32]);
    let original = Symbol([12; 32]);
    let functions = [Function {
        symbol: original,
        template: CodeKey(id),
        reads: id,
        references: &[],
        backend: Backend::Standalone,
    }];
    let roots = [original];
    let work = WorkSet::plan(
        BackendUnit { product: id, functions: &functions, statics: &[], roots: &roots },
        alloc,
    )
    .unwrap();
    let names = work.tasks().map(|(&grant, _)| grant).collect_in(alloc);
    let krate = Crate {
        name: "native-fixture",
        product: true,
        unit: Unit { spec: id.0, names: WireVec::new_in(heap.shared()) },
        names,
        side: Vec::new_in(alloc),
        members: Vec::new_in(alloc),
    };
    let output = Output { symbol: original, kind: OutputKind::Standalone };
    let mut record = CodeRecord::new_in(alloc);
    record.code.extend_from_slice(&0xd65f_03c0u32.to_le_bytes());
    let mut raw = Vec::new_in(alloc);
    record.seal().unwrap().encode(&mut raw);
    let content = content_key(&raw);
    let written = execute::receipt(output, CodeKey(id), content);
    let receipt = content_key(&written);
    let mut binding = None;
    execute::read_with::<()>(
        |key| {
            if key == content {
                Ok(Some(&raw))
            } else if key == receipt {
                Ok(Some(&written))
            } else {
                binding = Some(key);
                Ok(Some(&receipt.0.0))
            }
        },
        output,
        CodeKey(id),
    )
    .unwrap()
    .unwrap();
    let binding = binding.unwrap();
    let side = [Side { key: content_key(b"carried vtable"), bytes: b"carried vtable" }];
    let mut received = BTreeMap::new_in(alloc);
    received.insert(content, (super::super::KIND_CODE, raw.as_slice()));
    received.insert(receipt, (super::super::KIND_CODE, &written[..]));
    received.insert(binding, (super::super::KIND_CODE, &receipt.0.0[..]));
    let check = |rows: &blaze_own::ABTreeMap<'_, CodeKey, (u8, &[u8])>, data: &[Side<'_>]| {
        check_publication(
            &krate,
            &work,
            data,
            rows.len(),
            |key| rows.get(&key).copied(),
            alloc,
            heap,
        )
    };
    assert!(check(&received, &[]).is_ok());
    for (wrong_output, wrong_recipe) in [
        (Output { symbol: Symbol([91; 32]), ..output }, CodeKey(id)),
        (Output { kind: OutputKind::Thunk, ..output }, CodeKey(id)),
        (output, CodeKey(ContentId([92; 32]))),
    ] {
        let other = execute::receipt(wrong_output, wrong_recipe, content);
        let other_key = content_key(&other);
        let result = execute::read_with::<()>(|key| Ok(match key {
            k if k == binding => Some(&other_key.0.0),
            k if k == other_key => Some(&other),
            k if k == content => Some(&raw),
            _ => None,
        }), output, CodeKey(id));
        assert!(matches!(result, Err(execute::Error::Receipt { .. })));
    }
    assert!(check(&received, &side).is_err());
    received.insert(side[0].key, (super::super::KIND_CODE, side[0].bytes));
    assert!(check(&received, &side).is_ok());
    assert!(check(&received, &[]).is_err());
    received.remove(&content);
    assert!(check(&received, &side).is_err());
    received.insert(content, (super::super::KIND_CODE, b"another valid pack's value"));
    assert!(check(&received, &side).is_err());
    received.insert(content, (super::super::KIND_POINTER, &raw));
    assert!(check(&received, &side).is_err());
    received.insert(content, (super::super::KIND_CODE, &raw));
    received.remove(&binding);
    assert!(check(&received, &side).is_err());
}
