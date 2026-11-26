use super::*;
use crate::backend_plan::{Backend, Member};
use crate::code_records::MemoryStore;
use blaze_code::record::CodeRecord;
use blaze_own::testing::heap;

#[derive(Clone, Copy)]
enum Change {
    None,
    Missing,
    Duplicate,
    WrongKind,
    InvalidRecord,
    DifferentBytes,
}

struct Renderer {
    change: Change,
    folds: usize,
}

fn emitted<'a>(output: Output, value: u8, alloc: Alloc<'a>) -> Emitted<'a> {
    let mut code = CodeRecord::new_in(alloc);
    code.code.extend_from_slice(&(0x5280_0000u32 | u32::from(value) << 5).to_le_bytes());
    code.code.extend_from_slice(&0xd65f_03c0u32.to_le_bytes());
    let mut record = Vec::new_in(alloc);
    code.seal().unwrap().encode(&mut record);
    Emitted { output, record }
}

impl Emit for Renderer {
    type Error = &'static str;

    fn standalone<'a>(
        &mut self,
        _: &Function<'_>,
        _: Alloc<'a>,
    ) -> Result<Emitted<'a>, Self::Error> {
        Err("expected fold")
    }
    fn release<'a>(
        &mut self,
        _: &Function<'_>,
        _: &[&Function<'_>],
        _: Alloc<'a>,
    ) -> Result<Emitted<'a>, Self::Error> {
        Err("expected fold")
    }
    fn fold<'a>(
        &mut self,
        group: &Fold<'_>,
        alloc: Alloc<'a>,
    ) -> Result<AVec<'a, Emitted<'a>>, Self::Error> {
        self.folds += 1;
        let body = Output { symbol: Symbol(group.body.0.0), kind: OutputKind::FoldedBody };
        let mut rows = Vec::new_in(alloc);
        rows.push(emitted(body, u8::from(matches!(self.change, Change::DifferentBytes)), alloc));
        for member in &group.members {
            let output = Output { symbol: member.function.symbol, kind: OutputKind::Thunk };
            rows.push(emitted(output, 0, alloc));
        }
        match self.change {
            Change::Missing => {
                rows.pop();
            }
            Change::Duplicate => rows.push(emitted(body, 0, alloc)),
            Change::WrongKind => rows[0].output.kind = OutputKind::ReleaseBlob,
            Change::InvalidRecord => rows[0].record.clear(),
            Change::None | Change::DifferentBytes => {}
        }
        Ok(rows)
    }
}

#[test]
fn partial_fold_needs_every_real_record_and_an_immutable_result() {
    let alloc = heap().alloc();
    let key = |n| CodeKey(ContentId([n; 32]));
    let functions = [1, 2].map(|n| Function {
        symbol: Symbol([n; 32]),
        template: key(n),
        reads: key(0).0,
        references: &[],
        backend: Backend::Standalone,
    });
    let mut members = Vec::new_in(alloc);
    for function in &functions {
        members.push(Member { function, constants: &[], thunk: function.template });
    }
    let body = key(3);
    let task = Task::Fold(Fold { body, parameters: Vec::new_in(alloc), members });
    let mut backend = Renderer { change: Change::None, folds: 0 };
    let mut complete = MemoryStore::new_in(alloc);
    let cold = task.compile(&mut backend, &mut complete, alloc).unwrap();
    assert_eq!(cold.len(), 3);
    assert_eq!(complete.len(), 7, "three recipes and receipts share one machine record");
    for row in &cold {
        assert_eq!(row.source, Source::Emitted);
        let held = read::<()>(&complete, row.output, row.recipe).unwrap().unwrap();
        assert_eq!((held.receipt, held.content), (row.receipt, row.content));
        assert_eq!(held.record.code().len(), 8);
    }
    let warm = task.compile(&mut backend, &mut complete, alloc).unwrap();
    assert!(warm.iter().all(|row| row.source == Source::Store));
    assert_eq!(backend.folds, 1);
    for change in [
        Change::Missing,
        Change::Duplicate,
        Change::WrongKind,
        Change::InvalidRecord,
        Change::DifferentBytes,
        Change::None,
    ] {
        let mut partial = MemoryStore::new_in(alloc);
        let old = cold.iter().find(|row| row.recipe == body).unwrap();
        for key in [old.content, old.receipt, binding(body)] {
            partial.put(CodeClass::Program, key, complete.get(key).unwrap().unwrap()).unwrap();
        }
        backend.change = change;
        let before = backend.folds;
        let result = task.compile(&mut backend, &mut partial, alloc);
        assert_eq!(backend.folds, before + 1);
        match change {
            Change::None => assert_eq!(result.unwrap(), cold),
            _ => {
                assert!(result.is_err());
                assert_eq!(partial.len(), 3);
            }
        }
    }
}
