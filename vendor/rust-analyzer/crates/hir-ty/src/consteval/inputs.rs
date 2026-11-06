//! Concrete observations for explicit constant operands of one MIR body.
//! Callers must compose type/normalization and reached-body obligations before
//! replacing any conservative source-owner guard with these observations.

use base_db::CurrentSourceName;
use rustc_hash::FxHashSet;
use rustc_type_ir::inherent::{Const as _, IntoKind};
use salsa::SalsaValue;

use crate::{
    InferBodyId,
    db::{GeneralConstId, HirDatabase, InternedClosureId, InternedCoroutineId},
    method_resolution::lookup_impl_const,
    mir::{MirLowerError, Operand, OperandKind},
    next_solver::{
        ConstKind, DbInterner, GenericArgs, StoredGenericArgs, TypingMode, UnevaluatedConst,
        infer::{DbInternerInferExt, InferCtxt},
    },
    traits::StoredParamEnvAndCrate,
};

use super::{ConstEvalError, WithSourceNames};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, SalsaValue)]
pub enum BodyKind<'db> {
    Body(InferBodyId<'db>),
    Closure(InternedClosureId<'db>),
    Coroutine(InternedCoroutineId<'db>),
    CoroutineByMove(InternedCoroutineId<'db>),
}

#[salsa::interned]
pub struct BodyInput<'db> {
    #[returns(copy)]
    pub kind: BodyKind<'db>,
    #[returns(ref)]
    pub arguments: StoredGenericArgs,
    #[returns(ref)]
    pub environment: StoredParamEnvAndCrate,
}

#[derive(Debug, Clone, PartialEq, Eq, SalsaValue)]
pub struct ConstantSelection<'db> {
    pub definition: GeneralConstId<'db>,
    pub arguments: StoredGenericArgs,
}

/// Equality includes the current readset even if all evaluated bytes stayed equal.
#[derive(Debug, Clone, PartialEq, Eq, SalsaValue)]
pub struct PreparedConstants<'db> {
    pub result: Result<(), ConstEvalError<'db>>,
    pub selections: Vec<ConstantSelection<'db>>,
    pub source_names: Vec<CurrentSourceName>,
}

#[salsa::tracked(returns(ref), cycle_result = preparation_cycle)]
pub fn prepare_constants<'db>(
    db: &'db dyn HirDatabase,
    input: BodyInput<'db>,
) -> PreparedConstants<'db> {
    let args = input.arguments(db).clone();
    let environment = input.environment(db).clone();
    let body = match input.kind(db) {
        BodyKind::Body(owner) => {
            db.monomorphized_runtime_mir_body(owner, args, environment.clone())
        }
        BodyKind::Closure(owner) => {
            db.monomorphized_runtime_mir_body_for_closure(owner, args, environment.clone())
        }
        BodyKind::Coroutine(owner) => {
            db.monomorphized_runtime_mir_body_for_coroutine(owner, args, environment.clone())
        }
        BodyKind::CoroutineByMove(owner) => db
            .monomorphized_runtime_mir_body_for_coroutine_by_move(owner, args, environment.clone()),
    };
    let mut prepared =
        PreparedConstants { result: Ok(()), selections: Vec::new(), source_names: Vec::new() };
    let body = match body {
        Ok(body) => body,
        Err(error) => {
            prepared.result = Err(error.into());
            return prepared;
        }
    };
    let mut constants = Vec::new();
    let mut collect = |operand: &Operand| {
        if let OperandKind::Constant { konst, .. } = &operand.kind
            && let ConstKind::Unevaluated(value) = konst.as_ref().kind()
        {
            constants.push(value);
        }
    };
    for (_, block) in body.basic_blocks.iter() {
        for statement in &block.statements {
            statement.kind.for_each_operand(&mut collect);
        }
        if let Some(terminator) = &block.terminator {
            terminator.kind.for_each_operand(&mut collect);
        }
    }
    let mut seen = FxHashSet::default();
    for constant in constants {
        let selection = select_constant(db, constant, &environment);
        if !seen.insert((selection.definition, selection.arguments.clone())) {
            continue;
        }
        let observation = evaluate_selection(db, &selection, &environment);
        prepared.selections.push(selection);
        prepared.source_names.extend(observation.source_names);
        if let Err(error) = observation.value {
            prepared.result = Err(error);
            break;
        }
    }
    prepared.source_names.sort_by(|a, b| (a.file.index(), &a.name).cmp(&(b.file.index(), &b.name)));
    prepared.source_names.dedup();
    prepared
}

pub fn select_constant<'db>(
    db: &'db dyn HirDatabase,
    constant: UnevaluatedConst<'db>,
    environment: &StoredParamEnvAndCrate,
) -> ConstantSelection<'db> {
    let env = environment.as_ref(db);
    let interner = DbInterner::new_with(db, env.krate);
    let infcx = interner.infer_ctxt().build(TypingMode::PostAnalysis);
    select_constant_in_context(&infcx, constant, environment)
}

pub fn select_constant_in_context<'db>(
    infcx: &InferCtxt<'db>,
    constant: UnevaluatedConst<'db>,
    environment: &StoredParamEnvAndCrate,
) -> ConstantSelection<'db> {
    let env = environment.as_ref(infcx.interner.db);
    let (definition, arguments) = match constant.def.0 {
        GeneralConstId::ConstId(id) => {
            let (id, arguments) = lookup_impl_const(infcx, env.param_env, id, constant.args);
            (id.into(), arguments.store())
        }
        id => (id, constant.args.store()),
    };
    ConstantSelection { definition, arguments }
}

pub fn evaluate_selection<'db>(
    db: &'db dyn HirDatabase,
    selection: &ConstantSelection<'db>,
    environment: &StoredParamEnvAndCrate,
) -> WithSourceNames<Result<crate::next_solver::Allocation<'db>, ConstEvalError<'db>>> {
    let args: GenericArgs<'db> = selection.arguments.as_ref();
    let env = Some(environment.as_ref(db));
    match selection.definition {
        GeneralConstId::ConstId(id) => super::const_eval_with_source_names(db, id, args, env),
        GeneralConstId::StaticId(id) => super::const_eval_static_with_source_names(db, id),
        GeneralConstId::AnonConstId(id) => {
            super::anon_const_eval_with_source_names(db, id, args, env)
        }
    }
}

fn preparation_cycle<'db>(
    _: &'db dyn HirDatabase,
    _: salsa::Id,
    _: BodyInput<'db>,
) -> PreparedConstants<'db> {
    PreparedConstants {
        result: Err(MirLowerError::Loop.into()),
        selections: Vec::new(),
        source_names: Vec::new(),
    }
}
