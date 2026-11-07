//! Constant evaluation details

use base_db::Crate;
use hir_def::{
    ConstId, EnumVariantId, ExpressionStoreOwnerId, HasModule, StaticId,
    attrs::AttrFlags,
    expr_store::{Body, ExpressionStore, HygieneId, path::Path},
    hir::{Expr, ExprId, Literal},
    resolver::{Resolver, ValueNs},
};
use hir_expand::Lookup;
use rustc_abi::Size;
use rustc_apfloat::Float;
use rustc_ast_ir::Mutability;
use rustc_type_ir::inherent::{Const as _, GenericArgs as _, IntoKind, Ty as _};
use salsa::SalsaValue;

use crate::{
    ParamEnvAndCrate, Span,
    db::{AnonConstId, AnonConstLoc, GeneralConstId, HirDatabase},
    generics::Generics,
    lower::LoweringMode,
    mir::{IsSigned, MirEvalError, MirLowerError, pad16},
    next_solver::{
        Allocation, Const, ConstKind, Consts, DbInterner, DefaultAny, GenericArgs, ParamConst,
        ScalarInt, StoredAllocation, StoredEarlyBinder, StoredGenericArgs, Ty, TyKind,
        UnevaluatedConst, ValTreeKind, default_types,
    },
    traits::StoredParamEnvAndCrate,
};

use super::mir::interpret_mir;

pub mod inputs;

#[derive(Debug, Clone, PartialEq, Eq, SalsaValue)]
pub enum ConstEvalError<'db> {
    MirLowerError(MirLowerError<'db>),
    MirEvalError(MirEvalError<'db>),
}

impl<'db> From<MirLowerError<'db>> for ConstEvalError<'db> {
    fn from(value: MirLowerError<'db>) -> Self {
        match value {
            MirLowerError::ConstEvalError(_, e) => *e,
            _ => ConstEvalError::MirLowerError(value),
        }
    }
}

impl<'db> From<MirEvalError<'db>> for ConstEvalError<'db> {
    fn from(value: MirEvalError<'db>) -> Self {
        ConstEvalError::MirEvalError(value)
    }
}

/// Interns a constant scalar with the given type
fn intern_const_ref<'db>(
    interner: DbInterner<'db>,
    value: &Literal,
    ty: Ty<'db>,
) -> Option<Result<Const<'db>, CreateConstError<'db>>> {
    let Ok(data_layout) = interner.db.target_data_layout(interner.expect_crate()) else {
        return Some(Ok(Const::error(interner)));
    };
    let valtree = match (ty.kind(), value) {
        (TyKind::Uint(uint), Literal::Uint(value, _)) => {
            let size = uint.bit_width().map(Size::from_bits).unwrap_or(data_layout.pointer_size());
            let Some(scalar) = ScalarInt::try_from_uint(*value, size) else {
                return Some(Ok(Const::error(interner)));
            };
            ValTreeKind::Leaf(scalar)
        }
        (TyKind::Uint(uint), Literal::Int(value, _)) => {
            // `Literal::Int` is the default, so we also need to account for the type being uint.
            let size = uint.bit_width().map(Size::from_bits).unwrap_or(data_layout.pointer_size());
            let Some(scalar) = ScalarInt::try_from_uint(*value as u128, size) else {
                return Some(Ok(Const::error(interner)));
            };
            ValTreeKind::Leaf(scalar)
        }
        (TyKind::Int(int), Literal::Int(value, _)) => {
            let size = int.bit_width().map(Size::from_bits).unwrap_or(data_layout.pointer_size());
            let Some(scalar) = ScalarInt::try_from_int(*value, size) else {
                return Some(Ok(Const::error(interner)));
            };
            ValTreeKind::Leaf(scalar)
        }
        (TyKind::Bool, Literal::Bool(value)) => ValTreeKind::Leaf(ScalarInt::from(*value)),
        (TyKind::Char, Literal::Char(value)) => ValTreeKind::Leaf(ScalarInt::from(*value)),
        (TyKind::Float(float), Literal::Float(value, _)) => {
            let size = Size::from_bits(float.bit_width());
            let value = match float {
                rustc_ast_ir::FloatTy::F16 => value.to_f16().to_bits(),
                rustc_ast_ir::FloatTy::F32 => value.to_f32().to_bits(),
                rustc_ast_ir::FloatTy::F64 => value.to_f64().to_bits(),
                rustc_ast_ir::FloatTy::F128 => value.to_f128().to_bits(),
            };
            let scalar = ScalarInt::try_from_uint(value, size).unwrap();
            ValTreeKind::Leaf(scalar)
        }
        (TyKind::Ref(_, inner_ty, _), Literal::String(value))
            if matches!(inner_ty.kind(), TyKind::Str) =>
        {
            let u8_values = &interner.default_types().consts.u8_values;
            ValTreeKind::Branch(Consts::new_from_iter(
                value.as_str().as_bytes().iter().map(|&byte| u8_values[usize::from(byte)]),
            ))
        }
        (_, Literal::ByteString(_) | Literal::CString(_)) => {
            // This literals are complicated to construct and/or are possible to coerce.
            // So we just allocate an anon const for them, they should be rare so it's not a problem.
            return None;
        }
        _ => {
            let actual = literal_ty(
                interner,
                value,
                |types| types.types.i32,
                |types| types.types.u32,
                |types| types.types.f64,
            );
            return Some(Err(CreateConstError::TypeMismatch { actual }));
        }
    };
    Some(Ok(Const::new_valtree(interner, ty, valtree)))
}

pub(crate) fn literal_ty<'db>(
    interner: DbInterner<'db>,
    value: &Literal,
    default_int: impl FnOnce(&DefaultAny<'db>) -> Ty<'db>,
    default_uint: impl FnOnce(&DefaultAny<'db>) -> Ty<'db>,
    default_float: impl FnOnce(&DefaultAny<'db>) -> Ty<'db>,
) -> Ty<'db> {
    let types = interner.default_types();
    match value {
        Literal::Bool(..) => types.types.bool,
        Literal::String(..) => types.types.static_str_ref,
        Literal::ByteString(bs) => {
            let byte_type = types.types.u8;
            let array_type = Ty::new_array(interner, byte_type, bs.len() as u64);
            Ty::new_ref(interner, types.regions.statik, array_type, Mutability::Not)
        }
        Literal::CString(..) => Ty::new_ref(
            interner,
            types.regions.statik,
            interner.lang_items().CStr.map_or(types.types.error, |strukt| {
                Ty::new_adt(interner, strukt.into(), types.empty.generic_args)
            }),
            Mutability::Not,
        ),
        Literal::Char(..) => types.types.char,
        Literal::Int(_v, ty) => match ty {
            Some(int_ty) => match int_ty {
                hir_def::builtin_type::BuiltinInt::Isize => types.types.isize,
                hir_def::builtin_type::BuiltinInt::I8 => types.types.i8,
                hir_def::builtin_type::BuiltinInt::I16 => types.types.i16,
                hir_def::builtin_type::BuiltinInt::I32 => types.types.i32,
                hir_def::builtin_type::BuiltinInt::I64 => types.types.i64,
                hir_def::builtin_type::BuiltinInt::I128 => types.types.i128,
            },
            None => default_int(types),
        },
        Literal::Uint(_v, ty) => match ty {
            Some(int_ty) => match int_ty {
                hir_def::builtin_type::BuiltinUint::Usize => types.types.usize,
                hir_def::builtin_type::BuiltinUint::U8 => types.types.u8,
                hir_def::builtin_type::BuiltinUint::U16 => types.types.u16,
                hir_def::builtin_type::BuiltinUint::U32 => types.types.u32,
                hir_def::builtin_type::BuiltinUint::U64 => types.types.u64,
                hir_def::builtin_type::BuiltinUint::U128 => types.types.u128,
            },
            None => default_uint(types),
        },
        Literal::Float(_v, ty) => match ty {
            Some(float_ty) => match float_ty {
                hir_def::builtin_type::BuiltinFloat::F16 => types.types.f16,
                hir_def::builtin_type::BuiltinFloat::F32 => types.types.f32,
                hir_def::builtin_type::BuiltinFloat::F64 => types.types.f64,
                hir_def::builtin_type::BuiltinFloat::F128 => types.types.f128,
            },
            None => default_float(types),
        },
    }
}

/// Interns a possibly-unknown target usize
pub fn usize_const<'db>(db: &'db dyn HirDatabase, value: Option<u128>, krate: Crate) -> Const<'db> {
    let interner = DbInterner::new_no_crate(db);
    let value = match value {
        Some(value) => value,
        None => {
            return Const::error(interner);
        }
    };
    let Ok(data_layout) = db.target_data_layout(krate) else {
        return Const::error(interner);
    };
    let usize_ty = interner.default_types().types.usize;
    let Some(scalar) = ScalarInt::try_from_uint(value, data_layout.pointer_size()) else {
        return Const::error(interner);
    };
    Const::new_valtree(interner, usize_ty, ValTreeKind::Leaf(scalar))
}

pub fn allocation_as_usize(ec: Allocation<'_>) -> u128 {
    u128::from_le_bytes(pad16(&ec.memory, IsSigned::No))
}

pub fn try_const_usize<'db>(db: &'db dyn HirDatabase, c: Const<'db>) -> Option<u128> {
    match c.kind() {
        ConstKind::Param(_) => None,
        ConstKind::Infer(_) => None,
        ConstKind::Bound(_, _) => None,
        ConstKind::Placeholder(_) => None,
        ConstKind::Unevaluated(unevaluated_const) => match unevaluated_const.def.0 {
            GeneralConstId::ConstId(id) => {
                let subst = unevaluated_const.args;
                let ec = db.const_eval(id, subst, None).ok()?;
                Some(allocation_as_usize(ec))
            }
            GeneralConstId::StaticId(id) => {
                let ec = db.const_eval_static(id).ok()?;
                Some(allocation_as_usize(ec))
            }
            GeneralConstId::AnonConstId(id) => {
                let subst = unevaluated_const.args;
                let ec = db.anon_const_eval(id, subst, None).ok()?;
                Some(allocation_as_usize(ec))
            }
        },
        ConstKind::Value(val) => {
            if val.ty == default_types().types.usize {
                Some(val.value.inner().to_leaf().to_uint_unchecked())
            } else {
                None
            }
        }
        ConstKind::Error(_) => None,
        ConstKind::Expr(_) => None,
    }
}

pub fn allocation_as_isize(ec: Allocation<'_>) -> i128 {
    i128::from_le_bytes(pad16(&ec.memory, IsSigned::Yes))
}

pub fn try_const_isize<'db>(db: &'db dyn HirDatabase, c: Const<'db>) -> Option<i128> {
    match c.kind() {
        ConstKind::Param(_) => None,
        ConstKind::Infer(_) => None,
        ConstKind::Bound(_, _) => None,
        ConstKind::Placeholder(_) => None,
        ConstKind::Unevaluated(unevaluated_const) => match unevaluated_const.def.0 {
            GeneralConstId::ConstId(id) => {
                let subst = unevaluated_const.args;
                let ec = db.const_eval(id, subst, None).ok()?;
                Some(allocation_as_isize(ec))
            }
            GeneralConstId::StaticId(id) => {
                let ec = db.const_eval_static(id).ok()?;
                Some(allocation_as_isize(ec))
            }
            GeneralConstId::AnonConstId(id) => {
                let subst = unevaluated_const.args;
                let ec = db.anon_const_eval(id, subst, None).ok()?;
                Some(allocation_as_isize(ec))
            }
        },
        ConstKind::Value(val) => {
            if val.ty == default_types().types.isize {
                Some(val.value.inner().to_leaf().to_int_unchecked())
            } else {
                None
            }
        }
        ConstKind::Error(_) => None,
        ConstKind::Expr(_) => None,
    }
}

#[derive(Debug)]
pub(crate) enum CreateConstError<'db> {
    UsedForbiddenParam,
    ResolveToNonConst,
    NeedsAnonConst,
    DoesNotResolve,
    ConstHasGenerics,
    UnderscoreExpr,
    AnonConstInterningDisabled,
    TypeMismatch {
        #[expect(unused, reason = "will need this for diagnostics")]
        actual: Ty<'db>,
    },
}

pub(crate) fn path_to_const<'a, 'db>(
    db: &'db dyn HirDatabase,
    resolver: &Resolver<'db>,
    generics: &dyn Fn() -> &'a Generics<'db>,
    forbid_params_after: Option<u32>,
    path: &Path,
) -> Result<Const<'db>, CreateConstError<'db>> {
    let interner = DbInterner::new_no_crate(db);
    let resolution = resolver
        .resolve_path_in_value_ns_fully(db, path, HygieneId::ROOT)
        .ok_or(CreateConstError::DoesNotResolve)?;
    let no_generics = |def| crate::generics::generics(db, def).has_no_params();
    let konst = match resolution {
        ValueNs::ConstId(id) if no_generics(id.into()) => GeneralConstId::ConstId(id),
        ValueNs::StaticId(id) => GeneralConstId::StaticId(id),
        ValueNs::ConstId(_) => return Err(CreateConstError::ConstHasGenerics),
        ValueNs::GenericParam(param) => {
            let index = generics().type_or_const_param_idx(param.into());
            if forbid_params_after.is_some_and(|forbid_after| index >= forbid_after) {
                return Err(CreateConstError::UsedForbiddenParam);
            }
            return Ok(Const::new_param(interner, ParamConst { id: param, index }));
        }
        // These are not valid as consts.
        // FIXME: Report an error?
        ValueNs::ImplSelf(_) | ValueNs::LocalBinding(_) => {
            return Err(CreateConstError::ResolveToNonConst);
        }
        // A function or a constructor is a value, but only an anon const can hold it.
        ValueNs::FunctionId(_) | ValueNs::StructId(_) | ValueNs::EnumVariantId(_) => {
            return Err(CreateConstError::NeedsAnonConst);
        }
    };
    let args = GenericArgs::empty();
    Ok(Const::new_unevaluated(interner, UnevaluatedConst { def: konst.into(), args }))
}

/// `forbid_params_after` and `allow_anon_const_using_generic_params` fill a similar role, but are not the same.
///
/// `forbid_params_after` is only respected if an anon const is *not* created, that is, we directly resolve to a const param.
/// `allow_anon_const_using_generic_params` is only respected if an anon const *is* created.
///
/// For example, inside types, you are allowed to refer to const params in bare paths (but might be forbidden from referring
/// to *some*, in param defaults), but not allowed to do so in anon consts (e.g. `{ {}; N }` is forbidden, because it creates
/// an anon const). Inside field defaults, you are allowed to refer to them even inside anon consts.
///
/// There's also `generic_const_exprs`, `min_generic_const_args`, and `generic_const_args`, all of which we don't implement yet.
pub(crate) fn create_anon_const<'a, 'db>(
    interner: DbInterner<'db>,
    owner: ExpressionStoreOwnerId,
    store: &ExpressionStore,
    expr_id: ExprId,
    resolver: &Resolver<'db>,
    expected_ty: Ty<'db>,
    generics: &dyn Fn() -> &'a Generics<'db>,
    create_var: Option<&mut dyn FnMut(Span) -> Const<'db>>,
    lowering_mode: LoweringMode,
    forbid_params_after: Option<u32>,
    allow_anon_const_using_generic_params: bool,
) -> Result<Const<'db>, CreateConstError<'db>> {
    let mut expr = &store[expr_id];
    if let Expr::Block { statements, tail: Some(tail), .. } = expr
        && statements.is_empty()
    {
        // rustc unwraps *one* layer of blocks, so we do too (this impacts whether the const can use generic parameters.
        // Anon consts sometimes cannot while bare paths can). mGCA allows arbitrarily many blocks, but we don't implement
        // it yet.
        expr = &store[*tail];
    }
    match expr {
        Expr::Literal(literal)
            if let Some(literal) = intern_const_ref(interner, literal, expected_ty) =>
        {
            literal
        }
        Expr::Underscore => match create_var {
            Some(create_var) => Ok(create_var(expr_id.into())),
            None => Err(CreateConstError::UnderscoreExpr),
        },
        Expr::Path(path)
            if let konst =
                path_to_const(interner.db, resolver, generics, forbid_params_after, path)
                && !matches!(
                    konst,
                    Err(CreateConstError::DoesNotResolve
                        | CreateConstError::ConstHasGenerics
                        | CreateConstError::NeedsAnonConst)
                ) =>
        {
            konst
        }
        _ => {
            let Some(token) = lowering_mode.allow_tracked_structs() else {
                return Err(CreateConstError::AnonConstInterningDisabled);
            };

            let konst = AnonConstId::new(
                interner.db,
                AnonConstLoc {
                    owner,
                    expr: expr_id,
                    ty: StoredEarlyBinder::bind(expected_ty.store()),
                    allow_using_generic_params: allow_anon_const_using_generic_params,
                },
                token,
            );
            let args = if allow_anon_const_using_generic_params {
                GenericArgs::identity_for_item(interner, owner.generic_def(interner.db).into())
            } else {
                GenericArgs::empty()
            };
            Ok(Const::new_unevaluated(
                interner,
                UnevaluatedConst { def: GeneralConstId::AnonConstId(konst).into(), args },
            ))
        }
    }
}

/// A constant result and the source spellings read by its interpreter, including nested
/// evaluations. Reads participate in memo equality even on error or unchanged bytes.
/// This is not an exhaustive type-analysis trace: constants normalized before evaluation
/// are composed by the identity collector through source owners and dependency hashes.
#[derive(Debug, Clone, PartialEq, Eq, SalsaValue)]
pub struct WithSourceNames<T> {
    pub value: T,
    pub source_names: Vec<base_db::CurrentSourceName>,
}

impl<T> WithSourceNames<T> {
    fn new(value: T) -> Self {
        Self { value, source_names: Vec::new() }
    }

    fn map<U>(self, f: impl FnOnce(T) -> U) -> WithSourceNames<U> {
        WithSourceNames { value: f(self.value), source_names: self.source_names }
    }
}

pub(crate) fn const_eval_discriminant_variant<'db>(
    db: &'db dyn HirDatabase,
    variant_id: EnumVariantId,
) -> Result<i128, ConstEvalError<'db>> {
    const_eval_discriminant_with_source_names(db, variant_id).value
}

#[salsa::tracked(cycle_result = const_eval_discriminant_cycle_result, returns(clone))]
pub fn const_eval_discriminant_with_source_names<'db>(
    db: &'db dyn HirDatabase,
    variant_id: EnumVariantId,
) -> WithSourceNames<Result<i128, ConstEvalError<'db>>> {
    let def = variant_id.into();
    let body = Body::of(db, def);
    let loc = variant_id.lookup(db);
    if matches!(body[body.root_expr()], Expr::Missing) {
        return match loc.index(db).checked_sub(1) {
            Some(previous) => const_eval_discriminant_with_source_names(
                db,
                loc.parent.enum_variants(db).variants[previous].0,
            )
            .map(|value| value.map(|value| value + 1)),
            None => WithSourceNames::new(Ok(0)),
        };
    }
    let repr = AttrFlags::repr(db, loc.parent.into());
    let is_signed = repr.and_then(|repr| repr.int).is_none_or(|int| int.is_signed());
    let body = db.monomorphized_runtime_mir_body(
        def.into(),
        GenericArgs::empty().store(),
        ParamEnvAndCrate {
            param_env: db.trait_environment(def.generic_def(db)),
            krate: def.krate(db),
        }
        .store(),
    );
    evaluate_with_source_names(db, body, None).map(|value| {
        value.map(|value| {
            let value = value.as_ref();
            if is_signed { allocation_as_isize(value) } else { allocation_as_usize(value) as i128 }
        })
    })
}

fn const_eval_discriminant_cycle_result<'db>(
    _: &'db dyn HirDatabase,
    _: salsa::Id,
    _: EnumVariantId,
) -> WithSourceNames<Result<i128, ConstEvalError<'db>>> {
    WithSourceNames::new(Err(ConstEvalError::MirLowerError(MirLowerError::Loop)))
}

fn evaluate_with_source_names<'db>(
    db: &'db dyn HirDatabase,
    body: Result<&'db crate::mir::MirBody<'db>, MirLowerError<'db>>,
    trait_env: Option<ParamEnvAndCrate<'db>>,
) -> WithSourceNames<Result<StoredAllocation, ConstEvalError<'db>>> {
    let result = body
        .map_err(ConstEvalError::from)
        .and_then(|body| interpret_mir(db, body, false, trait_env).map_err(ConstEvalError::from));
    match result {
        Ok((value, output)) => WithSourceNames {
            value: value.map(|value| value.store()).map_err(ConstEvalError::from),
            source_names: output.source_names,
        },
        Err(error) => WithSourceNames::new(Err(error)),
    }
}

pub(crate) fn const_eval<'db>(
    db: &'db dyn HirDatabase,
    def: ConstId,
    subst: GenericArgs<'db>,
    trait_env: Option<ParamEnvAndCrate<'db>>,
) -> Result<Allocation<'db>, ConstEvalError<'db>> {
    const_eval_with_source_names(db, def, subst, trait_env).value
}

pub fn const_eval_with_source_names<'db>(
    db: &'db dyn HirDatabase,
    def: ConstId,
    subst: GenericArgs<'db>,
    trait_env: Option<ParamEnvAndCrate<'db>>,
) -> WithSourceNames<Result<Allocation<'db>, ConstEvalError<'db>>> {
    const_eval_query(db, def, subst.store(), trait_env.map(|env| env.store()))
        .clone()
        .map(|value| value.map(|value| value.as_ref()))
}

#[salsa::tracked(returns(ref), cycle_result = const_eval_cycle_result)]
fn const_eval_query<'db>(
    db: &'db dyn HirDatabase,
    def: ConstId,
    subst: StoredGenericArgs,
    trait_env: Option<StoredParamEnvAndCrate>,
) -> WithSourceNames<Result<StoredAllocation, ConstEvalError<'db>>> {
    let body = db.monomorphized_runtime_mir_body(
        def.into(),
        subst,
        // Impl selection can depend on crates the constant's own crate does not depend on.
        ParamEnvAndCrate {
            param_env: db.trait_environment(def.into()),
            krate: trait_env.as_ref().map_or_else(|| def.krate(db), |env| env.as_ref(db).krate),
        }
        .store(),
    );
    evaluate_with_source_names(db, body, trait_env.as_ref().map(|env| env.as_ref(db)))
}

fn const_eval_cycle_result<'db>(
    _: &'db dyn HirDatabase,
    _: salsa::Id,
    _: ConstId,
    _: StoredGenericArgs,
    _: Option<StoredParamEnvAndCrate>,
) -> WithSourceNames<Result<StoredAllocation, ConstEvalError<'db>>> {
    WithSourceNames::new(Err(ConstEvalError::MirLowerError(MirLowerError::Loop)))
}

pub(crate) fn anon_const_eval<'db>(
    db: &'db dyn HirDatabase,
    def: AnonConstId<'db>,
    subst: GenericArgs<'db>,
    trait_env: Option<ParamEnvAndCrate<'db>>,
) -> Result<Allocation<'db>, ConstEvalError<'db>> {
    anon_const_eval_with_source_names(db, def, subst, trait_env).value
}

/// Evaluate only anonymous constants already created by the compiler's typed
/// lowering. Identity discovery must compose these with every source owner's
/// results because layout can evaluate an inner constant before an interpreter.
pub fn owner_anonymous_consts_with_source_names<'db>(
    db: &'db dyn HirDatabase,
    owner: ExpressionStoreOwnerId,
) -> Vec<(ExprId, WithSourceNames<Result<Allocation<'db>, ConstEvalError<'db>>>)> {
    evaluate_owner_anonymous_consts(db, owner, owner_anonymous_const_ids(db, owner))
}

/// The owner's anonymous constants whose [`AnonConstLoc::allow_using_generic_params`] is false.
/// They evaluate with empty arguments, so no instance of the owner can change what they select
/// and their reads belong to the owner now.
pub fn owner_non_generic_anonymous_consts_with_source_names<'db>(
    db: &'db dyn HirDatabase,
    owner: ExpressionStoreOwnerId,
) -> Vec<(ExprId, WithSourceNames<Result<Allocation<'db>, ConstEvalError<'db>>>)> {
    let mut constants = owner_anonymous_const_ids(db, owner);
    constants.retain(|constant| !constant.loc(db).allow_using_generic_params);
    evaluate_owner_anonymous_consts(db, owner, constants)
}

fn evaluate_owner_anonymous_consts<'db>(
    db: &'db dyn HirDatabase,
    owner: ExpressionStoreOwnerId,
    constants: Vec<AnonConstId<'db>>,
) -> Vec<(ExprId, WithSourceNames<Result<Allocation<'db>, ConstEvalError<'db>>>)> {
    let interner = DbInterner::new_no_crate(db);
    constants
        .into_iter()
        .map(|constant| {
            let loc = constant.loc(db);
            let args = if loc.allow_using_generic_params {
                GenericArgs::identity_for_item(interner, owner.generic_def(db).into())
            } else {
                GenericArgs::empty()
            };
            (loc.expr, anon_const_eval_with_source_names(db, constant, args, None))
        })
        .collect()
}

fn owner_anonymous_const_ids<'db>(
    db: &'db dyn HirDatabase,
    owner: ExpressionStoreOwnerId,
) -> Vec<AnonConstId<'db>> {
    let mut constants: Vec<_> = match owner {
        ExpressionStoreOwnerId::Body(body) => {
            crate::InferenceResult::of(db, body).defined_anon_consts().to_vec()
        }
        ExpressionStoreOwnerId::Signature(def) => {
            AnonConstId::all_from_signature(db, def).collect()
        }
        ExpressionStoreOwnerId::VariantFields(variant) => {
            db.field_types_with_diagnostics(variant).defined_anon_consts().to_vec()
        }
    };
    if let ExpressionStoreOwnerId::Body(body @ hir_def::DefWithBodyId::FunctionId(_)) = owner {
        // Inline consts are inferred in place and acquire their typed ID during
        // MIR lowering. Read those actual operands, including deferred closures.
        let mut pending: Vec<_> = db.mir_body(body.into()).into_iter().collect();
        let mut closures = rustc_hash::FxHashSet::default();
        while let Some(body) = pending.pop() {
            let mut operand = |operand: &crate::mir::Operand| {
                if let crate::mir::OperandKind::Constant { konst, .. } = &operand.kind
                    && let ConstKind::Unevaluated(value) = konst.as_ref().kind()
                    && let GeneralConstId::AnonConstId(id) = value.def.0
                {
                    constants.push(id);
                }
            };
            for (_, block) in body.basic_blocks.iter() {
                for statement in &block.statements {
                    statement.kind.for_each_operand(&mut operand);
                }
                if let Some(terminator) = &block.terminator {
                    terminator.kind.for_each_operand(&mut operand);
                }
            }
            for &closure in &body.closures {
                if closures.insert(closure) {
                    pending.extend(db.mir_body_for_closure(closure));
                }
            }
        }
    }
    let mut seen = rustc_hash::FxHashSet::default();
    constants
        .into_iter()
        .filter(|&constant| seen.insert(constant) && constant.loc(db).owner == owner)
        .collect()
}

pub fn anon_const_eval_with_source_names<'db>(
    db: &'db dyn HirDatabase,
    def: AnonConstId<'db>,
    subst: GenericArgs<'db>,
    trait_env: Option<ParamEnvAndCrate<'db>>,
) -> WithSourceNames<Result<Allocation<'db>, ConstEvalError<'db>>> {
    anon_const_eval_query(db, def, subst.store(), trait_env.map(|env| env.store()))
        .clone()
        .map(|value| value.map(|value| value.as_ref()))
}

#[salsa::tracked(returns(ref), cycle_result = anon_const_eval_cycle_result)]
fn anon_const_eval_query<'db>(
    db: &'db dyn HirDatabase,
    def: AnonConstId<'db>,
    subst: StoredGenericArgs,
    trait_env: Option<StoredParamEnvAndCrate>,
) -> WithSourceNames<Result<StoredAllocation, ConstEvalError<'db>>> {
    let body = db.monomorphized_runtime_mir_body(
        def.into(),
        subst,
        ParamEnvAndCrate {
            param_env: db.trait_environment(def.loc(db).owner.generic_def(db)),
            krate: trait_env.as_ref().map_or_else(|| def.krate(db), |env| env.as_ref(db).krate),
        }
        .store(),
    );
    evaluate_with_source_names(db, body, trait_env.as_ref().map(|env| env.as_ref(db)))
}

fn anon_const_eval_cycle_result<'db>(
    _: &'db dyn HirDatabase,
    _: salsa::Id,
    _: AnonConstId<'db>,
    _: StoredGenericArgs,
    _: Option<StoredParamEnvAndCrate>,
) -> WithSourceNames<Result<StoredAllocation, ConstEvalError<'db>>> {
    WithSourceNames::new(Err(ConstEvalError::MirLowerError(MirLowerError::Loop)))
}

pub(crate) fn const_eval_static<'db>(
    db: &'db dyn HirDatabase,
    def: StaticId,
) -> Result<Allocation<'db>, ConstEvalError<'db>> {
    const_eval_static_with_source_names(db, def).value
}

pub fn const_eval_static_with_source_names<'db>(
    db: &'db dyn HirDatabase,
    def: StaticId,
) -> WithSourceNames<Result<Allocation<'db>, ConstEvalError<'db>>> {
    const_eval_static_query(db, def).clone().map(|value| value.map(|value| value.as_ref()))
}

#[salsa::tracked(returns(ref), cycle_result = const_eval_static_cycle_result)]
fn const_eval_static_query<'db>(
    db: &'db dyn HirDatabase,
    def: StaticId,
) -> WithSourceNames<Result<StoredAllocation, ConstEvalError<'db>>> {
    let body = db.monomorphized_runtime_mir_body(
        def.into(),
        GenericArgs::empty().store(),
        ParamEnvAndCrate { param_env: db.trait_environment(def.into()), krate: def.krate(db) }
            .store(),
    );
    evaluate_with_source_names(db, body, None)
}

fn const_eval_static_cycle_result<'db>(
    _: &'db dyn HirDatabase,
    _: salsa::Id,
    _: StaticId,
) -> WithSourceNames<Result<StoredAllocation, ConstEvalError<'db>>> {
    WithSourceNames::new(Err(ConstEvalError::MirLowerError(MirLowerError::Loop)))
}
