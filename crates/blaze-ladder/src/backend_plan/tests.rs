use super::*;
use blaze_own::testing::heap;

fn id(n: u8) -> ContentId {
    ContentId([n; 32])
}

fn symbol(n: u8) -> Symbol {
    Symbol([n; 32])
}

fn recipe(n: u8) -> Recipe<'static> {
    Recipe { prepared: id(n), settings: id(1), built_in: &[] }
}

fn function(n: u8, backend: Backend<'_>) -> Function<'_> {
    Function { symbol: symbol(n), template: CodeKey(id(n)), reads: id(0), references: &[], backend }
}

fn unit<'a>(functions: &'a [Function<'a>]) -> Unit<'a> {
    Unit { product: id(0), functions, statics: &[], roots: &[] }
}

fn release<'a>(n: u8, imports: &'a [Symbol]) -> Release<'a> {
    Release {
        recipe: recipe(n),
        summary: Summary { key: id(n), bytes: b"summary" },
        decisions: b"layout",
        imports,
    }
}

#[test]
fn selected_fold_owns_the_body_and_all_thunks() {
    let alloc = heap().alloc();
    let byte = |value| Constant::Int { bits: 8, value };
    let a = [byte(3), byte(3)];
    let b = [byte(5), byte(5)];
    let changed = [byte(6), byte(6)];
    let foldable = |n, constants| Backend::Foldable {
        recipe: recipe(n),
        mask: Mask { shape: id(9), instructions: 40, constants },
    };
    let mut functions = [
        function(2, foldable(2, &b)),
        function(3, Backend::Standalone),
        function(1, foldable(1, &a)),
    ];
    let original = {
        let work = WorkSet::plan(unit(&functions), alloc).unwrap();
        assert_eq!(work.tasks().len(), 2);
        let (name, group) = work
            .tasks()
            .find_map(|(&name, task)| match task {
                Task::Fold(fold) => Some((name, fold)),
                _ => None,
            })
            .unwrap();
        assert_eq!(group.parameters.len(), 2);
        assert_eq!(group.parameters[0].argument, group.parameters[1].argument);
        assert_eq!(group.members.len(), 2);
        assert_eq!(group.members[0].function.symbol, symbol(1));
        assert_eq!(group.members[1].constants, &b[..]);
        assert_ne!(group.members[0].thunk, group.members[1].thunk);
        assert_ne!(group.members[0].thunk, group.body);
        assert_eq!(work.binding(symbol(1)).unwrap().grant, name);
        assert_eq!(work.binding(symbol(2)).unwrap().grant, name);
        assert!(matches!(work.select(&[name], alloc).unwrap()[0], Task::Fold(_)));
        assert!(
            matches!(work.select(&[name, name], alloc), Err(PlanError::RepeatedGrant(key)) if key == name)
        );
        assert!(matches!(
            work.select(&[GrantName(id(255))], alloc),
            Err(PlanError::UnknownGrant(_))
        ));
        (name, group.body, work.digest)
    };
    functions.reverse();
    let reordered = WorkSet::plan(unit(&functions), alloc).unwrap();
    assert_eq!(original.2, reordered.digest);
    assert!(
        matches!(reordered.tasks.get(&original.0), Some(Task::Fold(fold)) if fold.body == original.1)
    );
    drop(reordered);
    functions[2].backend = foldable(2, &changed);
    let edited = WorkSet::plan(unit(&functions), alloc).unwrap();
    assert!(!edited.tasks.contains_key(&original.0));
    assert!(
        edited.tasks().any(|(_, task)| matches!(task, Task::Fold(fold) if fold.body == original.1))
    );
    drop(edited);
    functions[2].backend = foldable(2, &a);
    let identical = WorkSet::plan(unit(&functions), alloc).unwrap();
    assert_eq!(identical.tasks().len(), 3);
    assert!(identical.tasks().all(|(_, task)| matches!(task, Task::Standalone { .. })));
}

#[test]
fn release_keys_change_with_the_imported_preparation_and_unit_digest_covers_other_inputs() {
    let alloc = heap().alloc();
    let imports = [symbol(2)];
    let mut functions = [
        function(1, Backend::Release(release(1, &imports))),
        function(2, Backend::Release(release(2, &[]))),
    ];
    let result = |functions: &[Function<'_>], roots: &[Symbol]| {
        let work = WorkSet::plan(Unit { roots, ..unit(functions) }, alloc).unwrap();
        let blob = work
            .tasks()
            .find_map(|(_, task)| match task {
                Task::Release { function, imports, blob } if function.symbol == symbol(1) => {
                    assert_eq!(imports.len(), 1);
                    assert_eq!(imports[0].symbol, symbol(2));
                    Some(*blob)
                }
                _ => None,
            })
            .unwrap();
        (blob, work.digest)
    };
    let roots = [symbol(1), symbol(2)];
    let before = result(&functions, &roots);
    functions.swap(0, 1);
    assert_eq!(before, result(&functions, &roots));
    let reversed = result(&functions, &[symbol(2), symbol(1)]);
    assert_eq!(before.0, reversed.0);
    assert_ne!(before.1, reversed.1);
    let Backend::Release(ref mut changed) = functions[0].backend else { unreachable!() };
    changed.summary.bytes = b"verified summary with another call count";
    let summary = result(&functions, &roots);
    assert_eq!(before.0, summary.0);
    assert_ne!(before.1, summary.1);
    let built_in = [CodeKey(id(7))];
    let Backend::Release(ref mut changed) = functions[0].backend else { unreachable!() };
    changed.recipe.built_in = &built_in;
    assert_ne!(summary.0, result(&functions, &roots).0);
}

#[test]
fn masks_resolve_places_and_refuse_an_out_of_range_operand() {
    let alloc = heap().alloc();
    let target = Reference {
        target: Target { space: Space::Func, symbol: symbol(7) },
        binding: Binding::Slot,
    };
    let refs = [target, target];
    let masks = [[Constant::Place(0)], [Constant::Place(1)]];
    let mut functions = masks.each_ref().map(|constants| {
        function(
            1,
            Backend::Foldable {
                recipe: recipe(1),
                mask: Mask { shape: id(5), instructions: 100, constants },
            },
        )
    });
    functions[1].symbol = symbol(2);
    functions[1].template = CodeKey(id(2));
    for function in &mut functions {
        function.references = &refs;
    }
    let work = WorkSet::plan(unit(&functions), alloc).unwrap();
    assert!(work.tasks().all(|(_, task)| matches!(task, Task::Standalone { .. })));
    drop(work);
    functions[1].references = &refs[..1];
    assert!(matches!(WorkSet::plan(unit(&functions), alloc),
        Err(PlanError::InvalidPlace { function, place: 1 }) if function == symbol(2)));
}

#[test]
fn planned_output_uses_the_writers_content_key() {
    use crate::code_records::{CodeClass, CodeStore, MemoryStore, content_key};

    let mut store = MemoryStore::new_in(heap().alloc());
    let lookup = CodeKey(id(3));
    let made = content_key(b"current entry");
    store.put(CodeClass::Program, made, b"current entry").unwrap();
    store.point(CodeClass::Program, lookup, made).unwrap();
    let pointed = store.pointed(lookup).unwrap();
    assert_eq!(Written::check(lookup, pointed, made).unwrap().content(), made);
    assert!(Written::check(lookup, pointed, lookup).is_err());
    assert!(Written::check(lookup, pointed, content_key(b"previous entry")).is_err());
    assert!(Written::check(lookup, None, made).is_err());
}

#[test]
fn split_obeys_the_parameter_cap_and_strict_savings_test() {
    let alloc = heap().alloc();
    let zero = [Constant::Int { bits: 8, value: 0 }; 17];
    let distinct: [_; 17] =
        core::array::from_fn(|at| Constant::Int { bits: 8, value: at.widen_to::<u128>() + 1 });
    for (positions, instructions, expected_groups) in
        [(17, 100, 0), (16, 100, 1), (1, 6, 0), (1, 7, 1)]
    {
        let functions = [(&zero, 1), (&distinct, 2)].map(|(constants, n)| {
            function(
                n,
                Backend::Foldable {
                    recipe: recipe(n),
                    mask: Mask { shape: id(9), instructions, constants: &constants[..positions] },
                },
            )
        });
        let work = WorkSet::plan(unit(&functions), alloc).unwrap();
        assert_eq!(
            work.tasks().filter(|(_, task)| matches!(task, Task::Fold(_))).count(),
            expected_groups
        );
    }
}

#[test]
fn a_release_import_must_name_one_distinct_release_function() {
    let alloc = heap().alloc();
    for imports in [&[symbol(2), symbol(2)][..], &[symbol(1)], &[symbol(3)], &[symbol(4)]] {
        let functions = [
            function(1, Backend::Release(release(1, imports))),
            function(2, Backend::Release(release(2, &[]))),
            function(3, Backend::Standalone),
        ];
        assert!(
            matches!(WorkSet::plan(unit(&functions), alloc), Err(PlanError::InvalidImport { root, .. }) if root == symbol(1))
        );
    }
}
