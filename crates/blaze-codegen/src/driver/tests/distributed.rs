//! Canonical release/fold grants restored into an independent, empty compiler store.
use super::*;

pub(super) fn outputs(values: GrantedValues, store: &mut Store) -> Vec<Key> {
    assert_eq!(values.stats.compiled_keys.len(), values.stats.compiled);
    for put in values.compiled.iter().chain(&values.stored) {
        if put.kind == Kind::Code {
            let template = blob::decode(&put.bytes).unwrap();
            if let Some(side) = template.side {
                let key = keys::side(&side);
                assert!(
                    values.compiled.iter().chain(&values.stored).any(|p| p.key == key),
                    "a grant exports every debug table its templates name"
                );
            }
        }
    }
    store.keep(&values.compiled);
    store.keep(&values.stored);
    values.stats.compiled_keys
}

#[test]
fn release_grants_keep_whole_unit_imports_and_the_tail_only_binds() {
    let (program, policy) = (program(), policy(OptLevel::O3, Mode::Output));
    let (a, b, ma, mb) = (Machine::default(), Machine::default(), Memo::new(), Memo::new());
    let empty = Store::default();
    let ba = Build::new(&a, &program, &policy, &ma);
    let bb = Build::new(&b, &program, &policy, &mb);
    let wa = ba.work_set(&[(1, member())], &[], &[], &empty).unwrap();
    let wb = bb.work_set(&[(1, member())], &[], &[], &empty).unwrap();
    assert_eq!(wa.digest(), wb.digest());
    assert_eq!(wa.names(), wb.names());
    assert_eq!(a.emitted.load(Ordering::Relaxed), 0, "planning emits nothing");
    assert!(
        wa.release.iter().any(|t| !t.imports.imported.is_empty()),
        "no per-function no-import shortcut"
    );
    assert_eq!(wa.names().len(), 5, "every release function is grantable");
    let names = wa.names();
    let mut arrived = Store::default();
    let mut emitted = Vec::new();
    for (worker, work, granted) in [(&ba, &wa, &names[..2]), (&bb, &wb, &names[2..])] {
        let values = worker.compile_granted(work, granted, &empty).unwrap();
        assert!(values.stats.compiled > 0);
        emitted.extend(outputs(values, &mut arrived));
    }
    let mut unique = emitted.clone();
    unique.sort_unstable();
    unique.dedup();
    assert_eq!(unique.len(), emitted.len(), "no template emitted by both holders");
    assert_eq!(
        wa.release_keys(),
        unique,
        "release evidence names exactly the canonical emitted store keys"
    );
    let (tail, memo) = (Machine::default(), Memo::new());
    let restored = build(&program, &tail, &policy, &memo, &arrived).unwrap();
    assert_eq!(restored.stats.compiled, 0);
    assert_eq!(tail.emitted.load(Ordering::Relaxed), 0);
    assert_eq!(tail.summarized.load(Ordering::Relaxed), 0, "summaries traveled with the grants");
    let fresh = build(&program, &Machine::default(), &policy, &Memo::new(), &empty).unwrap();
    assert!(fresh.stats.imported > 0);
    assert_eq!(restored.functions, fresh.functions);
    assert_eq!(restored.digests, fresh.digests);

    let (warm, memo) = (Machine::default(), Memo::new());
    let build = Build::new(&warm, &program, &policy, &memo);
    let work = build.work_set(&[(1, member())], &[], &[], &arrived).unwrap();
    assert_eq!(work.digest(), wa.digest(), "cold, memo and stored summary plans agree");
    let values = build.compile_granted(&work, &work.names(), &arrived).unwrap();
    assert!(values.stats.compiled_keys.is_empty());
    let mut exported = Store::default();
    outputs(values, &mut exported);
    assert_eq!(exported.0, arrived.0, "warm holders export the same complete immutable values");
}

#[test]
fn release_work_identity_and_keys_follow_imports_settings_and_resolved_refs() {
    let (mut program, mut policy) = (program(), policy(OptLevel::O3, Mode::Output));
    let plan = |p: &Program, policy: &Policy| {
        let (machine, memo) = (Machine::default(), Memo::new());
        Build::new(&machine, p, policy, &memo)
            .work_set(&[(1, member())], &[], &[], &Store::default())
            .unwrap()
    };
    let original = plan(&program, &policy);
    let importer = original.release.iter().find(|t| !t.imports.imported.is_empty()).unwrap();
    let importer_item = original.nodes[&importer.name].unit.item;
    let import_item = original.nodes[&importer.imports.imported[0]].unit.item;
    let imported = (1..=5).find(|id| item(*id) == import_item).unwrap();
    let before = program.functions[&imported].body;
    program.functions.get_mut(&imported).unwrap().body ^= 1;
    let changed = plan(&program, &policy);
    let next =
        changed.release.iter().find(|t| changed.nodes[&t.name].unit.item == importer_item).unwrap();
    assert_ne!(importer.blob_key, next.blob_key, "imported code invalidates its importer");
    assert_ne!(original.digest(), changed.digest());
    program.functions.get_mut(&imported).unwrap().body = before;
    assert_eq!(
        original.digest(),
        plan(&program, &policy).digest(),
        "reverting restores positive reuse"
    );
    program.function(99, 0x99, &[]);
    assert_eq!(
        original.digest(),
        plan(&program, &policy).digest(),
        "unreachable edits do not invalidate work"
    );
    program.functions.get_mut(&2).unwrap().outside = "other_memcpy";
    assert_ne!(
        original.digest(),
        plan(&program, &policy).digest(),
        "resolved external target is an input"
    );
    program.functions.get_mut(&2).unwrap().outside = "memcpy";
    policy.toolchain.cpu.push_str("-other");
    let changed = plan(&program, &policy);
    assert_ne!(original.digest(), changed.digest());
    assert!(
        original.names().iter().all(|name| !changed.names().contains(name)),
        "settings invalidate release blob tasks"
    );
}

#[test]
fn invalid_and_repeated_grants_fail_before_emission() {
    let (program, policy, machine, memo, store) = (
        program(),
        policy(OptLevel::O3, Mode::Output),
        Machine::default(),
        Memo::new(),
        Store::default(),
    );
    let build = Build::new(&machine, &program, &policy, &memo);
    let work = build.work_set(&[(1, member())], &[], &[], &store).unwrap();
    let first = work.names()[0];
    let errors = build.compile_granted(&work, &[first, first], &store).unwrap_err();
    assert!(errors.iter().any(|e| e.what.contains("duplicate task")));
    let errors =
        build.compile_granted(&work, &[InstanceKey(ContentId([0xff; 32]))], &store).unwrap_err();
    assert!(errors.iter().any(|e| e.what.contains("canonical work plan")));
    assert_eq!(machine.emitted.load(Ordering::Relaxed), 0);
}

#[test]
fn granted_release_rejects_a_corrupt_summary_during_planning() {
    let (program, policy, machine, memo) =
        (program(), policy(OptLevel::O3, Mode::Output), Machine::default(), Memo::new());
    let mut store = Store::default();
    let build = Build::new(&machine, &program, &policy, &memo);
    let work = build.work_set(&[(1, member())], &[], &[], &store).unwrap();
    store.0.insert(work.summaries[0].key, vec![0xff]);
    assert!(build.work_set(&[(1, member())], &[], &[], &store).is_err());
    assert_eq!(machine.emitted.load(Ordering::Relaxed), 0);
}

#[test]
fn changed_summary_import_choice_moves_the_plan_even_with_equal_walk_keys() {
    let (program, policy, machine, memo, empty) = (
        program(),
        policy(OptLevel::O3, Mode::Output),
        Machine::default(),
        Memo::new(),
        Store::default(),
    );
    let build = Build::new(&machine, &program, &policy, &memo);
    let original = build.work_set(&[(1, member())], &[], &[], &empty).unwrap();
    let importer = original.release.iter().find(|task| !task.imports.imported.is_empty()).unwrap();
    let imported = importer.imports.imported[0];
    let mut changed = Store::default();
    changed.keep(&original.summaries);
    let summary = original
        .summaries
        .iter()
        .find(|put| Summary::decode(&put.bytes).unwrap().key == imported)
        .unwrap();
    let mut replacement = Summary::decode(&summary.bytes).unwrap();
    replacement.hint = Hint::Never;
    changed.0.insert(summary.key, replacement.encode());
    let plan = build.work_set(&[(1, member())], &[], &[], &changed).unwrap();
    assert!(original.nodes.keys().eq(plan.nodes.keys()), "the front-end walk is unchanged");
    assert_ne!(original.digest(), plan.digest(), "summary decisions are identity inputs");
    let next = plan.release.iter().find(|task| task.name == importer.name).unwrap();
    assert!(!next.imports.imported.contains(&imported));
    assert_ne!(importer.blob_key, next.blob_key);
}

#[test]
fn successive_dev_grants_reexport_debug_of_a_shared_memo_shape() {
    let (program, policy, machine, memo) = (
        program(),
        policy(OptLevel::O0, Mode::Output),
        Machine { emit_debug: true, ..Machine::default() },
        Memo::new(),
    );
    let mut holder_values = Store::default();
    let build = Build::new(&machine, &program, &policy, &memo);
    let work = build.work_set(&[(1, member())], &[], &[], &holder_values).unwrap();
    let mut reused = 0;
    let mut receiver = Store::default();
    for name in work.names() {
        let values = build.compile_granted(&work, &[name], &holder_values).unwrap();
        reused += values.stats.memo;
        // The caller retains prior grant outputs in its existing store overlay.
        holder_values.keep(&values.compiled);
        holder_values.keep(&values.stored);
        outputs(values, &mut receiver);
    }
    assert!(reused > 0, "distinct templates of one shape reuse the memo across grants");
    let (machine, memo) = (Machine::default(), Memo::new());
    let restored =
        Build::new(&machine, &program, &policy, &memo).run(&[(1, member())], &receiver).unwrap();
    assert_eq!(restored.stats.compiled, 0);
    assert!(restored.functions.iter().all(|function| function.side.is_some()));
}
