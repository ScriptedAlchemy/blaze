//! Folded groups merged and landed in one daemon's memo across builds: what a build asks the
//! memo for is what the landing it takes holds.

use std::sync::atomic::AtomicUsize;

use super::*;

/// [`Machine`] whose functions of body `0x22` (2 and 3 of [`program`]) have one masked form,
/// a body of 100 instructions calling place 0, which varies between them, so they fold. Its
/// merge makes a body of the group's key alone and a thunk per member calling it, counting
/// each merge.
#[derive(Default)]
struct Merging {
    machine: Machine,
    merged: AtomicUsize,
    debug: bool,
}

fn compiled(key: InstanceKey, code: Vec<u8>, relocs: Vec<crate::Relocation>) -> Emitted {
    let function = CompiledFunction {
        key,
        arch: Arch::Wasm32,
        align: 16,
        code,
        relocs: relocs.into(),
        data: Vec::new(),
        inlined: Vec::new(),
        side: None,
        unwind: None,
        lines: Vec::new(),
        coverage: None,
        profile: None,
    };
    Emitted { function, side: None, extra: Vec::new() }
}

impl Prepare for Merging {
    type Frontend = Program;
    type Prepared = Prepared;
    type Export = Exported;

    fn prepare(
        &self,
        program: &Program,
        unit: &Unit,
        spec: &Spec,
        id: &u32,
        refs: &crate::template::Refs,
    ) -> Result<(Shape, Prepared), CodegenError> {
        self.machine.prepare(program, unit, spec, id, refs)
    }

    fn summarize(
        &self,
        unit: &Unit,
        spec: &Spec,
        prepared: &mut Prepared,
        refs: &crate::template::Refs,
    ) -> Result<(Summary, Exported), CodegenError> {
        self.machine.summarize(unit, spec, prepared, refs)
    }

    fn emit(
        &self,
        unit: &Unit,
        spec: &Spec,
        prepared: Prepared,
        refs: &crate::template::Refs,
        imports: Option<(&Imports, &[Import<'_, Exported>])>,
    ) -> Result<Emitted, CodegenError> {
        self.machine.emit(unit, spec, prepared, refs, imports)
    }

    fn masked(&self, _: &Unit, prepared: &Prepared, _: &crate::template::Refs) -> Option<Masked> {
        matches!(prepared.body, 0x22 | 0x33).then(|| Masked {
            shape: vec![prepared.body],
            constants: vec![fold::Constant::Place(0)],
            size: 100,
        })
    }

    fn merge(
        &self,
        group: &Group,
        members: Vec<(&Unit, &Spec, Prepared, &crate::template::Refs)>,
    ) -> Result<fold::Merged, CodegenError> {
        self.merged.fetch_add(1, Ordering::Relaxed);
        let key = group.key();
        let with_debug = |mut emitted: Emitted, byte: u8| {
            if self.debug {
                let bytes = vec![byte; 8];
                emitted.function.side = Some(ContentId::of("test folded debug 1", &bytes));
                emitted.side = Some(bytes);
            }
            emitted
        };
        let body = with_debug(compiled(key, vec![0xf0], Vec::new()), 0xf0);
        let thunks = (members.iter())
            .map(|(unit, ..)| {
                let call = crate::Relocation {
                    at: 1,
                    kind: RelocKind::PcRel32,
                    target: Target::Direct(key),
                    addend: -4,
                };
                with_debug(compiled(unit.instance.key(), vec![0xe9, 0, 0, 0, 0], vec![call]), 0xe9)
            })
            .collect();
        Ok(fold::Merged { body, thunks })
    }

    fn compile_static(
        &self,
        program: &Program,
        spec: &Spec,
        id: &u32,
        refs: &crate::template::Refs,
    ) -> Result<Static, CodegenError> {
        self.machine.compile_static(program, spec, id, refs)
    }
}

/// C1 of the fold-key review: a member's thunk is kept under what its code is kept under,
/// which an edit of a body it builds in moves while its instance key, and so the group's key
/// when it is not the first member, stay. Two builds of one memo, as a daemon's: the second,
/// one member building its callee in, takes no landing of the first that lacks the thunk it
/// asks for, but merges again. Each of 2 and 3 is edited in turn, so whichever is not first
/// is.
#[test]
fn a_landed_group_holds_every_thunk_it_is_asked_for() {
    let policy = folding_policy();
    for edited in [2, 3] {
        let mut program = program();
        let (machine, memo) = (Merging::default(), Memo::new());
        let mut store = Store::default();
        let first =
            Build::new(&machine, &program, &policy, &memo).run(&[(1, member())], &store).unwrap();
        assert_eq!(first.stats.folded, 2, "2 and 3 fold");
        assert_eq!(machine.merged.load(Ordering::Relaxed), 1);
        assert_eq!(first.stats.compiled_keys.len(), first.stats.compiled);
        assert_eq!(
            first.stats.compiled,
            machine.machine.emitted.load(Ordering::Relaxed) + 1 + first.stats.folded,
            "the merge emits one body and one thunk per folded member"
        );
        let body = first.functions.iter().find(|function| function.code == [0xf0]).unwrap();
        let body_key = keys::folded(&body.key);
        assert_eq!(first.stats.compiled_keys.iter().filter(|key| **key == body_key).count(), 1);
        let mut errors = Vec::new();
        let (nodes, ..) = Build::new(&machine, &program, &policy, &memo).walk(
            &[(1, member())],
            &[],
            &[],
            None,
            &mut errors,
        );
        assert!(errors.is_empty());
        // Built.functions is after dedupe: identical thunks may share code, but each
        // requested member must have emitted and stored its own template before dedupe.
        for id in [2, 3] {
            let (member, node) = nodes.iter().find(|(_, node)| node.unit.item == item(id)).unwrap();
            let kept = first.deduped.merged.get(member).unwrap_or(member);
            assert!(first.functions.iter().any(|function| {
                function.key == *kept && function.code.first() == Some(&0xe9)
            }));
            let key = keys::thunk(&body.key, &node.stored());
            assert_eq!(
                first.stats.compiled_keys.iter().filter(|reported| **reported == key).count(),
                1
            );
        }
        let mut reported = first.stats.compiled_keys.clone();
        reported.sort_unstable();
        let mut code_keys: Vec<_> =
            first.puts.iter().filter(|put| put.kind == Kind::Code).map(|put| put.key).collect();
        code_keys.sort_unstable();
        assert_eq!(
            reported, code_keys,
            "the body, thunks and ordinary emits use actual store keys"
        );
        let from_memo =
            Build::new(&machine, &program, &policy, &memo).run(&[(1, member())], &store).unwrap();
        assert_eq!(from_memo.stats.compiled, 0);
        assert!(from_memo.stats.compiled_keys.is_empty(), "a landed fold emits nothing");
        assert_eq!(machine.merged.load(Ordering::Relaxed), 1);
        store.keep(&first.puts);
        let warm = Build::new(&machine, &program, &policy, &Memo::new())
            .run(&[(1, member())], &store)
            .unwrap();
        assert_eq!(warm.stats.compiled, 0);
        assert!(warm.stats.compiled_keys.is_empty(), "stored folds emit nothing");
        assert_eq!(machine.merged.load(Ordering::Relaxed), 1);

        program.functions.get_mut(&edited).unwrap().built_in = vec![0];
        let again = Build::new(&machine, &program, &policy, &memo).run(&[(1, member())], &store);
        let again = again.unwrap_or_else(|errors| panic!("f{edited} edited: {errors:?}"));
        assert_eq!(again.stats.folded, 2, "f{edited} edited: 2 and 3 fold");
        assert_eq!(
            machine.merged.load(Ordering::Relaxed),
            2,
            "f{edited} edited: its thunk is merged again"
        );
    }
}

#[test]
fn folded_grants_export_whole_groups_and_debug_tables_to_an_empty_tail() {
    let mut program = program();
    program.functions.get_mut(&1).unwrap().calls.extend([6, 7]);
    program.function(6, 0x33, &[4]);
    program.function(7, 0x33, &[5]);
    let policy = folding_policy();
    let (a, b) = (
        Merging { debug: true, ..Merging::default() },
        Merging { debug: true, ..Merging::default() },
    );
    let (ma, mb, empty) = (Memo::new(), Memo::new(), Store::default());
    let ba = Build::new(&a, &program, &policy, &ma);
    let bb = Build::new(&b, &program, &policy, &mb);
    let wa = ba.work_set(&[(1, member())], &[], &[], &empty).unwrap();
    let wb = bb.work_set(&[(1, member())], &[], &[], &empty).unwrap();
    assert_eq!(wa.digest(), wb.digest());
    assert_eq!(wa.names(), wb.names());
    assert_eq!(wa.groups.len(), 2);
    assert_eq!(a.merged.load(Ordering::Relaxed), 0, "planning never merges");
    let mut arrived = Store::default();
    let mut emitted = Vec::new();
    for (key, task) in wa.tasks.iter() {
        let second = matches!(*task, WorkTask::Fold(1));
        let (worker, work) = if second { (&bb, &wb) } else { (&ba, &wa) };
        let values = worker.compile_granted(work, &[*key], &empty).unwrap();
        if let WorkTask::Fold(at) = *task {
            let group = &wa.groups[at];
            assert_eq!(values.stats.compiled_keys.len(), 1 + group.members.len());
            assert!(values.compiled.iter().any(|p| p.kind == Kind::Debug));
        }
        emitted.extend(super::distributed::outputs(values, &mut arrived));
    }
    assert_eq!(a.merged.load(Ordering::Relaxed), 1);
    assert_eq!(b.merged.load(Ordering::Relaxed), 1);
    let mut unique = emitted.clone();
    unique.sort_unstable();
    unique.dedup();
    assert_eq!(unique.len(), emitted.len(), "no duplicate body, thunk or standalone template");
    assert_eq!(wa.folded_keys().len(), 6);
    assert!(wa.folded_keys().iter().all(|key| unique.contains(key)));
    let (tail, memo) = (Merging { debug: true, ..Merging::default() }, Memo::new());
    let restored =
        Build::new(&tail, &program, &policy, &memo).run(&[(1, member())], &arrived).unwrap();
    assert_eq!(restored.stats.compiled, 0);
    assert!(restored.stats.compiled_keys.is_empty());
    assert_eq!(tail.merged.load(Ordering::Relaxed), 0);
    let (fresh, memo) = (Merging { debug: true, ..Merging::default() }, Memo::new());
    let fresh = Build::new(&fresh, &program, &policy, &memo).run(&[(1, member())], &empty).unwrap();
    assert_eq!(restored.functions, fresh.functions);
    assert_eq!(restored.digests, fresh.digests);
    let work = ba.work_set(&[(1, member())], &[], &[], &arrived).unwrap();
    assert_eq!(work.digest(), wa.digest());
    let warm = ba.compile_granted(&work, &work.names(), &arrived).unwrap();
    assert_eq!(warm.stats.compiled, 0);
    let mut exported = Store::default();
    super::distributed::outputs(warm, &mut exported);
    assert_eq!(exported.0, arrived.0, "warm groups re-export body, all thunks and debug bytes");

    // A missing debug table is refused; a holder cannot publish an incomplete bundle.
    let side = fresh.functions.iter().find_map(|f| f.side).unwrap();
    arrived.0.remove(&keys::side(&side));
    assert!(ba.compile_granted(&work, &work.names(), &arrived).is_err());
}

#[test]
fn fold_task_identity_follows_members_settings_and_resolved_targets() {
    let (mut program, mut policy) = (program(), folding_policy());
    let plan = |p: &Program, policy: &Policy| {
        let (machine, memo) = (Merging::default(), Memo::new());
        Build::new(&machine, p, policy, &memo)
            .work_set(&[(1, member())], &[], &[], &Store::default())
            .unwrap()
    };
    let first = plan(&program, &policy);
    let name = group_flight(&first.groups[0]);
    assert!(first.names().contains(&name), "one grant contains all folded members");
    assert_eq!(first.digest(), plan(&program, &policy).digest());
    program.function(99, 0x99, &[]);
    assert_eq!(
        first.digest(),
        plan(&program, &policy).digest(),
        "unreachable member is not an input"
    );
    program.functions.get_mut(&3).unwrap().outside = "other_memcpy";
    let changed = plan(&program, &policy);
    assert_ne!(first.digest(), changed.digest());
    assert_eq!(
        name,
        group_flight(&changed.groups[0]),
        "relative templates can bind another external target; the unit identity still changes"
    );
    program.functions.get_mut(&3).unwrap().outside = "memcpy";
    program.functions.get_mut(&3).unwrap().body = 0x34;
    let changed = plan(&program, &policy);
    assert!(changed.groups.is_empty(), "one member no longer has the masked form");
    assert_ne!(first.digest(), changed.digest());
    assert!(!changed.names().contains(&name));
    assert_eq!(
        changed.names().len(),
        changed.functions(),
        "ungrouped fold candidates are grantable"
    );
    program.functions.get_mut(&3).unwrap().body = 0x22;
    assert_eq!(
        first.digest(),
        plan(&program, &policy).digest(),
        "restoring a member restores reuse"
    );
    policy.profile.debuginfo = DebugInfo::None;
    let changed = plan(&program, &policy);
    assert_ne!(name, group_flight(&changed.groups[0]));
    assert_ne!(first.digest(), changed.digest());
}
