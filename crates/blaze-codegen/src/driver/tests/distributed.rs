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

/// A planted bug in the store a warm grant reads its debug tables from: the grant's own code is
/// untouched, so the oracle of the invariant below sees only what a grant path that mishandled a
/// debug table would let through.
#[derive(Clone, Copy, PartialEq)]
enum Bug {
    /// A store that answers an empty table for the debug table it does not hold, as a grant
    /// that took a miss for a table would.
    AnswersAMissingTable,
    /// A store that hands back the table it holds with a byte added, as a grant that rebuilt or
    /// rewrote a restored table instead of exporting it as it was would.
    ChangesARestoredTable,
}

/// The warm grant's input store, with `bug` planted on the debug table `at` when there is one.
struct Planted<'a> {
    store: &'a Store,
    bug: Option<Bug>,
    at: Key,
    /// The bytes of `at` with one more byte, for [`Bug::ChangesARestoredTable`].
    changed: &'a [u8],
}

impl Lookup for Planted<'_> {
    fn read(&self, key: &Key) -> Result<Option<&[u8]>, String> {
        const EMPTY: &[u8] = &[];
        let held = Lookup::read(self.store, key)?;
        Ok(match (self.bug, held) {
            (Some(Bug::AnswersAMissingTable), None) if *key == self.at => Some(EMPTY),
            (Some(Bug::ChangesARestoredTable), Some(_)) if *key == self.at => Some(self.changed),
            (_, held) => held,
        })
    }
}

/// The oracle of [`warm_dev_and_release_grants_refuse_missing_debug_without_recompiling`]: for a
/// dev and a release build, with the templates in the store and in the memo only, a warm grant
/// whose input lacks one debug table is refused naming exactly that key with nothing recompiled,
/// and restoring only that table makes exactly the original grant transferable.
fn warm_grants_refuse_missing_debug(bug: Option<Bug>) {
    for (level, built) in [(OptLevel::O0, "dev"), (OptLevel::O3, "release")] {
        for memo_only in [false, true] {
            let templates = if memo_only { "memo" } else { "store" };
            let case = format!("{built} grant with {templates} templates");
            let (program, policy, machine, memo) = (
                program(),
                policy(level, Mode::Output),
                Machine { emit_debug: true, ..Machine::default() },
                Memo::new(),
            );
            let build = Build::new(&machine, &program, &policy, &memo);
            let work = build.work_set(&[(1, member())], &[], &[], &Store::default()).unwrap();
            let first = build.compile_granted(&work, &work.names(), &Store::default()).unwrap();
            let debug: Vec<_> = first
                .compiled
                .iter()
                .chain(&first.stored)
                .filter(|put| put.kind == Kind::Debug)
                .map(|put| put.key)
                .collect();
            assert!(!debug.is_empty(), "the fixture must emit actual debug tables");
            let mut complete = Store::default();
            outputs(first, &mut complete);
            let mut input = Store::default();
            for (key, bytes) in &complete.0 {
                if !memo_only || debug.contains(key) {
                    input.0.insert(*key, bytes.clone());
                }
            }
            let missing = debug[0];
            let bytes = input.0.remove(&missing).unwrap();
            let changed = [&bytes[..], &[0xff]].concat();
            let emitted = machine.emitted.load(Ordering::Relaxed);
            let planted = Planted { store: &input, bug, at: missing, changed: &changed };
            let errors = build
                .compile_granted(&work, &work.names(), &planted)
                .expect_err("granted a bundle that lacks a debug table");
            assert!(
                errors.iter().any(|error| {
                    error.what.contains("missing debug table")
                        && error.what.contains(&format!("{missing:?}"))
                }),
                "{case}: the refusal does not name the missing {missing:?}"
            );
            assert_eq!(
                machine.emitted.load(Ordering::Relaxed),
                emitted,
                "{case}: the refusal recompiled"
            );

            // Restoring only the omitted table makes exactly the original grant transferable.
            input.0.insert(missing, bytes);
            let planted = Planted { store: &input, bug, at: missing, changed: &changed };
            let restored = build.compile_granted(&work, &work.names(), &planted).unwrap();
            assert_eq!(restored.stats.compiled, 0, "{case}: the restored grant compiled");
            let mut receiver = Store::default();
            outputs(restored, &mut receiver);
            assert_eq!(
                receiver.0, complete.0,
                "{case}: the restored grant is not the original, byte for byte"
            );
            assert_eq!(
                machine.emitted.load(Ordering::Relaxed),
                emitted,
                "{case}: the restored grant recompiled"
            );
        }
    }
}

/// The invariant: a warm grant exports every debug table its templates name, or refuses
/// the missing key without recompiling. This holds for both store and memo templates.
#[test]
fn warm_dev_and_release_grants_refuse_missing_debug_without_recompiling() {
    warm_grants_refuse_missing_debug(None);
}

/// The control of [`warm_dev_and_release_grants_refuse_missing_debug_without_recompiling`]: its
/// own oracle, over the same fixtures, finds a store that answers an empty table for the debug
/// table it lacks, which a grant that never refused would export as the real one: the grant is
/// granted, and the oracle says so before it looks at anything else.
#[test]
#[should_panic(expected = "granted a bundle that lacks")]
fn the_oracle_catches_a_missing_debug_table_answered_as_present() {
    warm_grants_refuse_missing_debug(Some(Bug::AnswersAMissingTable));
}

/// The control of [`warm_dev_and_release_grants_refuse_missing_debug_without_recompiling`] for
/// its restoration half: the refusal is right and only the restored table differs by a byte, which
/// the oracle finds in the exported bytes and nowhere else.
#[test]
#[should_panic(expected = "not the original, byte for byte")]
fn the_oracle_catches_a_restored_debug_table_that_is_not_the_original() {
    warm_grants_refuse_missing_debug(Some(Bug::ChangesARestoredTable));
}
