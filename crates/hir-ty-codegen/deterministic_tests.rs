use super::*;
use std::{
    collections::VecDeque,
    sync::{Condvar, Mutex},
};
use test_fixture::WithFixture;

fn fixture(test: impl FnOnce(Vec<FunctionId>, StoredParamEnvAndCrate, Vec<Instance>)) {
    let (db, _) = ide_db::RootDatabase::with_single_file("pub fn first() {}\npub fn second() {}\n");
    hir::attach_db(&db, || {
        let krate = base_db::all_crates(&db)[0];
        let mut functions: Vec<_> = hir_def::nameres::crate_def_map(&db, krate)
            .modules(&db)
            .flat_map(|(_, module)| module.scope(&db).declarations())
            .filter_map(|item| match item {
                hir_def::ModuleDefId::FunctionId(id) => Some(id),
                _ => None,
            })
            .collect();
        functions.sort_by_key(|id| FunctionSignature::of(&db, *id).name.as_str().to_owned());
        let instances = functions.iter().map(|id| root_instance(&db, *id)).collect();
        let env =
            ParamEnvAndCrate { param_env: hir_ty::next_solver::ParamEnv::empty(), krate }.store();
        test(functions, env, instances);
    });
}

fn ahead() -> Ahead {
    Ahead {
        roots: VecDeque::new(),
        rooted: FxHashSet::default(),
        rooted_done: FxHashSet::default(),
        queue: Vec::new(),
        seen: FxHashSet::default(),
        done: FxHashSet::default(),
        fresh: Vec::new(),
        working: 1,
        stopped: false,
        open: true,
        alive: 1,
    }
}

fn scheduled(held: Option<usize>, ordered: bool) -> (Vec<FuncId>, Vec<usize>) {
    let (mut emitted, mut waves) = (Vec::new(), Vec::new());
    fixture(|_, env, instances| {
        let rows: Vec<_> = instances
            .iter()
            .enumerate()
            .map(|(id, instance)| (instance.clone(), env.clone(), FuncId::from_u32(id as u32)))
            .collect();
        let held = held.map(|at| (rows[at].0.clone(), env.clone()));
        let mut state = ahead();
        for (instance, env, _) in &rows {
            let key = (instance.clone(), env.clone());
            if held.as_ref() != Some(&key) {
                state.seen.insert(key.clone());
                state.done.insert(key.clone());
                state.fresh.push(key);
            }
        }
        let state = Mutex::new(state);
        let idle = Condvar::new();
        let governor = budget::Governor::new(None, 2);
        let mut ready = Ready {
            ahead: &state,
            idle: &idle,
            governor: &governor,
            waiting: FxHashMap::default(),
            ordered: ordered.then(VecDeque::new),
        };
        std::thread::scope(|scope| {
            let worker = held.map(|held| {
                scope.spawn(|| {
                    let mut state = state.lock().unwrap();
                    while !state.seen.contains(&held) {
                        state = idle.wait(state).unwrap();
                    }
                    state.queue.retain(|it| it != &held);
                    state.done.insert(held.clone());
                    state.fresh.push(held);
                    state.working = 0;
                    idle.notify_all();
                })
            });
            let first = ready.take(&mut rows.clone());
            waves.push(first.len());
            emitted.extend(first.into_iter().map(|(_, _, id)| id));
            if let Some(worker) = worker {
                worker.join().unwrap();
            }
            let rest = ready.take(&mut Vec::new());
            if !rest.is_empty() {
                waves.push(rest.len());
            }
            emitted.extend(rest.into_iter().map(|(_, _, id)| id));
        });
    });
    (emitted, waves)
}

fn emitted(reverse: bool, ordered: bool) -> Vec<FuncId> {
    scheduled(reverse.then_some(0), ordered).0
}

fn object(order: &[FuncId]) -> Vec<u8> {
    use cranelift_codegen::ir::InstBuilder;
    let builder =
        ObjectBuilder::new(target().unwrap().clone(), "ordered", default_libcall_names()).unwrap();
    let mut module = ObjectModule::new(builder);
    for id in order {
        let mut ctx = module.make_context();
        ctx.func.signature.returns.push(AbiParam::new(types::I64));
        let declaration = module
            .declare_function(
                &format!("fixture_{}", id.as_u32()),
                Linkage::Export,
                &ctx.func.signature,
            )
            .unwrap();
        let mut functions = FunctionBuilderContext::new();
        let mut function = FunctionBuilder::new(&mut ctx.func, &mut functions);
        let block = function.create_block();
        function.switch_to_block(block);
        let value = function.ins().iconst(types::I64, 11 + i64::from(id.as_u32()));
        function.ins().return_(&[value]);
        function.seal_all_blocks();
        function.finalize(module.isa().frontend_config());
        module.define_function(declaration, &mut ctx).unwrap();
    }
    module.finish().emit().unwrap()
}

#[test]
fn cached_object_bytes_ignore_instance_completion_order() {
    let forward = emitted(false, true);
    let reverse = emitted(true, true);
    assert!(object(&forward) == object(&reverse), "completion timing changed the object");
    assert_eq!(reverse, [FuncId::from_u32(0), FuncId::from_u32(1)]);
}

#[test]
fn live_emission_can_use_the_first_completed_instance() {
    assert_eq!(emitted(true, false), [FuncId::from_u32(1), FuncId::from_u32(0)]);
}

#[test]
fn closed_root_frontier_keeps_discovery_order() {
    fixture(|functions, _, _| {
        let mut state = ahead();
        state.rooted.extend(functions.iter().copied());
        state.rooted_done.insert(functions[1]);
        state.alive = 0;
        state.working = 0;
        let state = Mutex::new(state);
        let idle = Condvar::new();
        let governor = budget::Governor::new(None, 2);
        let ready = Ready {
            ahead: &state,
            idle: &idle,
            governor: &governor,
            waiting: FxHashMap::default(),
            ordered: Some(VecDeque::new()),
        };
        assert_eq!(ready.ready_root(&functions.into()), 0);
    });
}

#[test]
fn cached_wave_boundaries_ignore_later_instance_completion() {
    let (ids, waves) = scheduled(Some(1), true);
    assert_eq!(ids, [FuncId::from_u32(0), FuncId::from_u32(1)]);
    assert_eq!(waves, [2], "completion timing split a deterministic emission wave");
}

#[test]
fn cached_wave_keeps_duplicate_instance_request_rows() {
    fixture(|_, env, instances| {
        let state = Mutex::new(ahead());
        {
            let mut state = state.lock().unwrap();
            state.alive = 0;
            state.working = 0;
        }
        let idle = Condvar::new();
        let governor = budget::Governor::new(None, 2);
        let mut ready = Ready {
            ahead: &state,
            idle: &idle,
            governor: &governor,
            waiting: FxHashMap::default(),
            ordered: Some(VecDeque::new()),
        };
        let mut pending = [0, 1, 0]
            .into_iter()
            .enumerate()
            .map(|(id, instance)| {
                (instances[instance].clone(), env.clone(), FuncId::from_u32(id as u32))
            })
            .collect();
        let emitted: Vec<_> =
            ready.take(&mut pending).into_iter().map(|(_, _, id)| id.as_u32()).collect();
        assert_eq!(emitted, [0, 1, 2]);
        assert!(pending.is_empty());
    });
}

#[test]
fn cached_wave_shedding_leaves_pending_work_unadmitted() {
    fixture(|_, env, instances| {
        let state = Mutex::new(ahead());
        let idle = Condvar::new();
        let governor = budget::Governor::new(None, 2);
        governor.stop();
        let mut ready = Ready {
            ahead: &state,
            idle: &idle,
            governor: &governor,
            waiting: FxHashMap::default(),
            ordered: Some(VecDeque::new()),
        };
        let mut pending = vec![(instances[0].clone(), env, FuncId::from_u32(0))];
        assert!(ready.take(&mut pending).is_empty());
        assert_eq!(pending.len(), 1);
        assert!(state.lock().unwrap().seen.is_empty());
    });
}

#[test]
fn cached_root_stop_does_not_admit_another_definition() {
    fixture(|functions, _, _| {
        let mut state = ahead();
        state.rooted.insert(functions[0]);
        state.stopped = true;
        let state = Mutex::new(state);
        let idle = Condvar::new();
        let governor = budget::Governor::new(None, 2);
        governor.stop();
        let ready = Ready {
            ahead: &state,
            idle: &idle,
            governor: &governor,
            waiting: FxHashMap::default(),
            ordered: Some(VecDeque::new()),
        };
        assert!(!ready.root_ready(functions[0]));
    });
}
