//! Error text that can be embedded in persistent objects. Database IDs are never names.

use base_db::FileId;
use either::Either;
use hir_def::{
    AdtId, DefWithBodyId, ExpressionStoreOwnerId, GenericDefId, GenericParamId, HasModule,
    ItemContainerId, Lookup, ModuleIdLt, VariantId,
    expr_store::{Body, ExpressionStore, path::Path},
    hir::generics::GenericParams,
    signatures::{
        ConstSignature, EnumSignature, FunctionSignature, StaticSignature, StructSignature,
        TraitSignature, TypeAliasSignature, UnionSignature,
    },
    src::HasSource,
};
use hir_expand::InFile;
use rustc_type_ir::inherent::{BoundExistentialPredicates as _, IntoKind, Safety as _, Term as _};
use syntax::{AstNode, SyntaxNode, SyntaxNodePtr, TextRange};

use super::MirLowerError;
use crate::{
    InferBodyId,
    consteval::ConstEvalError,
    db::HirDatabase,
    mir::{MirEvalError, MirSpan, ProjectionElem, StoredPlace},
    next_solver::{
        AliasTyKind, ConstKind, GenericArg, GenericArgKind, RegionKind, Ty, TyKind, ValTreeKind,
    },
};

struct Diagnostic<'a, 'db> {
    db: &'db dyn HirDatabase,
    span: &'a dyn Fn(FileId, TextRange) -> String,
    ty: &'a dyn Fn(Ty<'db>) -> String,
    arg: &'a dyn Fn(GenericArg<'db>) -> String,
}

macro_rules! diagnostic_api {
    ($error:ident, $render:ident) => {
        impl<'db> $error<'db> {
            /// Stable text for persisted diagnostics. Type analysis that failed is not rerun.
            pub fn diagnostic(&self, db: &'db dyn HirDatabase) -> String {
                self.format_diagnostic(
                    db,
                    |_, range| format!("{}..{}", u32::from(range.start()), u32::from(range.end())),
                    |ty| stable_type(db, ty),
                    |arg| stable_arg(db, arg),
                )
            }

            /// The shared renderer, with the IDE's source locations and type display.
            pub fn format_diagnostic(
                &self,
                db: &'db dyn HirDatabase,
                span: impl Fn(FileId, TextRange) -> String,
                ty: impl Fn(Ty<'db>) -> String,
                arg: impl Fn(GenericArg<'db>) -> String,
            ) -> String {
                Diagnostic { db, span: &span, ty: &ty, arg: &arg }.$render(self)
            }
        }
    };
}

diagnostic_api!(MirLowerError, lower);
diagnostic_api!(MirEvalError, eval);
diagnostic_api!(ConstEvalError, constant);

fn module_name<'db>(db: &'db dyn HirDatabase, mut module: ModuleIdLt<'db>) -> String {
    let mut names = Vec::new();
    loop {
        if let Some(name) = module.name(db) {
            names.push(name.as_str().to_owned());
        }
        match module.containing_module(db) {
            Some(parent) => module = parent,
            None => break,
        }
    }
    names.push(
        module
            .krate(db)
            .extra_data(db)
            .display_name
            .as_ref()
            .map_or_else(|| "<unnamed crate>".into(), |name| name.to_string()),
    );
    names.reverse();
    names.join("::")
}

fn definition(db: &dyn HirDatabase, def: GenericDefId) -> String {
    let source: InFile<SyntaxNode>;
    let name;
    macro_rules! named {
        ($id:expr, $signature:ident) => {{
            let id = $id;
            source = id.lookup(db).source(db).map(|node| node.syntax().clone());
            name = $signature::of(db, id).name.as_str().to_owned();
        }};
    }
    match def {
        GenericDefId::FunctionId(id) => {
            named!(id, FunctionSignature);
            let container = match id.lookup(db).container {
                ItemContainerId::ImplId(id) => Some(definition(db, id.into())),
                ItemContainerId::TraitId(id) => Some(definition(db, id.into())),
                _ => None,
            };
            let range = source.original_file_range_rooted(db).range;
            return format!(
                "{}::{name}@{}..{}",
                container.unwrap_or_else(|| module_name(db, def.module(db))),
                u32::from(range.start()),
                u32::from(range.end())
            );
        }
        GenericDefId::ConstId(id) => {
            source = id.lookup(db).source(db).map(|node| node.syntax().clone());
            name = ConstSignature::of(db, id)
                .name
                .as_ref()
                .map_or("_", |name| name.as_str())
                .to_owned();
        }
        GenericDefId::StaticId(id) => named!(id, StaticSignature),
        GenericDefId::TraitId(id) => named!(id, TraitSignature),
        GenericDefId::TypeAliasId(id) => named!(id, TypeAliasSignature),
        GenericDefId::AdtId(AdtId::StructId(id)) => named!(id, StructSignature),
        GenericDefId::AdtId(AdtId::EnumId(id)) => named!(id, EnumSignature),
        GenericDefId::AdtId(AdtId::UnionId(id)) => named!(id, UnionSignature),
        GenericDefId::ImplId(id) => {
            let node = id.lookup(db).source(db);
            name =
                node.value.syntax().text().to_string().split('{').next().unwrap().trim().to_owned();
            source = node.map(|node| node.syntax().clone());
        }
    }
    let range = source.original_file_range_rooted(db).range;
    format!(
        "{}::{name}@{}..{}",
        module_name(db, def.module(db)),
        u32::from(range.start()),
        u32::from(range.end())
    )
}

fn owner_name(db: &dyn HirDatabase, owner: ExpressionStoreOwnerId) -> String {
    let name = definition(db, owner.generic_def(db));
    match owner {
        ExpressionStoreOwnerId::Body(DefWithBodyId::VariantId(id)) => {
            format!(
                "{name}::{}",
                id.lookup(db).parent.enum_variants(db).variant_name_by_id(id).unwrap().as_str()
            )
        }
        _ => name,
    }
}

fn body_name(db: &dyn HirDatabase, body: InferBodyId<'_>) -> String {
    match body {
        InferBodyId::DefWithBodyId(id) => owner_name(db, id.into()),
        InferBodyId::AnonConstId(id) => {
            let loc = id.loc(db);
            format!(
                "{}::const {{ {} }}",
                owner_name(db, loc.owner),
                expression(db, loc.owner, loc.expr).0
            )
        }
    }
}

fn expression(
    db: &dyn HirDatabase,
    owner: ExpressionStoreOwnerId,
    expr: hir_def::hir::ExprId,
) -> (String, Option<(FileId, TextRange)>) {
    let map = ExpressionStore::with_source_map(db, owner).1;
    match map.expr_syntax(expr) {
        Ok(source) => {
            let source = source.map(|ptr| {
                let ptr: SyntaxNodePtr = ptr.into();
                ptr
            });
            let node = source.value.to_node(&source.file_id.parse_or_expand(db));
            let range = InFile::new(source.file_id, node.clone()).original_file_range_rooted(db);
            (node.text().to_string(), Some((range.file_id.file_id(db), range.range)))
        }
        Err(_) => ("<synthetic expression>".into(), None),
    }
}

fn span_location(
    db: &dyn HirDatabase,
    body: InferBodyId<'_>,
    span: MirSpan,
) -> Option<(FileId, TextRange)> {
    let map = ExpressionStore::with_source_map(db, body.expression_store_owner(db)).1;
    let self_param = match body {
        InferBodyId::DefWithBodyId(id) => Body::with_source_map(db, id).1.self_param_syntax(),
        InferBodyId::AnonConstId(_) => None,
    };
    let source: InFile<SyntaxNodePtr> = match span {
        MirSpan::ExprId(expr) | MirSpan::ExprEnd(expr) => {
            map.expr_syntax(expr).ok()?.map(Into::into)
        }
        MirSpan::PatId(pat) => map.pat_syntax(pat).ok()?.map(|ptr| ptr.syntax_node_ptr()),
        MirSpan::BindingId(binding) => map
            .patterns_for_binding(binding)
            .iter()
            .find_map(|pat| map.pat_syntax(*pat).ok())?
            .map(|ptr| ptr.syntax_node_ptr()),
        MirSpan::SelfParam => self_param?.map(|ptr| ptr.syntax_node_ptr()),
        MirSpan::Unknown => return None,
    };
    let node = source.value.to_node(&source.file_id.parse_or_expand(db));
    let range = InFile::new(source.file_id, node).original_file_range_rooted(db);
    Some((range.file_id.file_id(db), range.range))
}

fn param_name(db: &dyn HirDatabase, id: GenericParamId) -> String {
    match id {
        GenericParamId::TypeParamId(id) => GenericParams::of(db, id.parent())[id.local_id()]
            .name()
            .map_or("Self", |name| name.as_str())
            .into(),
        GenericParamId::ConstParamId(id) => GenericParams::of(db, id.parent())[id.local_id()]
            .name()
            .map_or("_", |name| name.as_str())
            .into(),
        GenericParamId::LifetimeParamId(id) => {
            GenericParams::of(db, id.parent)[id.local_id].name.as_str().into()
        }
    }
}

fn stable_arg(db: &dyn HirDatabase, arg: GenericArg<'_>) -> String {
    match arg.kind() {
        GenericArgKind::Type(ty) => stable_type(db, ty),
        GenericArgKind::Lifetime(region) => match region.kind() {
            RegionKind::ReStatic => "'static".into(),
            RegionKind::ReEarlyParam(param) => param_name(db, param.id.into()),
            _ => "'_".into(),
        },
        GenericArgKind::Const(konst) => match konst.kind() {
            ConstKind::Value(value) => match value.value.kind() {
                ValTreeKind::Leaf(value) => {
                    format!("0x{:x}:{}", value.to_bits(value.size()), value.size().bytes())
                }
                ValTreeKind::Branch(values) => format!(
                    "[{}]",
                    values
                        .iter()
                        .map(|value| stable_arg(db, value.into()))
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            },
            ConstKind::Param(param) => param_name(db, param.id.into()),
            ConstKind::Unevaluated(value) => match value.def.0 {
                crate::db::GeneralConstId::ConstId(id) => definition(db, id.into()),
                crate::db::GeneralConstId::StaticId(id) => definition(db, id.into()),
                crate::db::GeneralConstId::AnonConstId(id) => body_name(db, id.into()),
            },
            ConstKind::Infer(_) => "<inferred const>".into(),
            ConstKind::Bound(..) => "<bound const>".into(),
            ConstKind::Placeholder(_) => "<placeholder const>".into(),
            ConstKind::Expr(_) => "<const expression>".into(),
            ConstKind::Error(_) => "<error const>".into(),
        },
    }
}

pub(crate) fn stable_type(db: &dyn HirDatabase, ty: Ty<'_>) -> String {
    let args = |args: crate::next_solver::GenericArgs<'_>| {
        args.iter().map(|arg| stable_arg(db, arg)).collect::<Vec<_>>().join(", ")
    };
    match ty.kind() {
        TyKind::Bool => "bool".into(),
        TyKind::Char => "char".into(),
        TyKind::Str => "str".into(),
        TyKind::Never => "!".into(),
        TyKind::Int(ty) => ty.name_str().into(),
        TyKind::Uint(ty) => ty.name_str().into(),
        TyKind::Float(ty) => ty.name_str().into(),
        TyKind::Param(param) => param_name(db, param.id.into()),
        TyKind::Ref(_, to, mutability) => format!(
            "&{}{}",
            if mutability == rustc_ast_ir::Mutability::Mut { "mut " } else { "" },
            stable_type(db, to)
        ),
        TyKind::RawPtr(to, mutability) => format!(
            "*{} {}",
            if mutability == rustc_ast_ir::Mutability::Mut { "mut" } else { "const" },
            stable_type(db, to)
        ),
        TyKind::Slice(to) => format!("[{}]", stable_type(db, to)),
        TyKind::Array(to, len) => {
            format!("[{}; {}]", stable_type(db, to), stable_arg(db, len.into()))
        }
        TyKind::Tuple(tys) => {
            format!("({})", tys.iter().map(|ty| stable_type(db, ty)).collect::<Vec<_>>().join(", "))
        }
        TyKind::Adt(adt, subst) => {
            format!("{}<{}>", definition(db, adt.def_id().into()), args(subst))
        }
        TyKind::FnDef(def, subst) => format!(
            "{}<{}>",
            match def.0 {
                hir_def::CallableDefId::FunctionId(id) => definition(db, id.into()),
                hir_def::CallableDefId::StructId(id) => definition(db, id.into()),
                hir_def::CallableDefId::EnumVariantId(id) =>
                    owner_name(db, DefWithBodyId::VariantId(id).into()),
            },
            args(subst)
        ),
        TyKind::Closure(id, subst) => {
            format!("{}::closure<{}>", body_name(db, id.0.loc(db).owner), args(subst))
        }
        TyKind::Coroutine(id, subst) => {
            format!("{}::coroutine<{}>", body_name(db, id.0.loc(db).owner), args(subst))
        }
        TyKind::CoroutineClosure(id, subst) => {
            format!("{}::coroutine closure<{}>", body_name(db, id.0.loc(db).owner), args(subst))
        }
        TyKind::Alias(alias) => {
            let name = match alias.kind {
                AliasTyKind::Projection { def_id } => definition(db, def_id.0.into()),
                AliasTyKind::Inherent { def_id } => definition(db, def_id.0.into()),
                AliasTyKind::Free { def_id } => definition(db, def_id.0.into()),
                AliasTyKind::Opaque { def_id } => match def_id.0.loc(db) {
                    crate::ImplTraitId::ReturnTypeImplTrait(id, _) => {
                        format!("impl Trait of {}", definition(db, id.into()))
                    }
                    crate::ImplTraitId::TypeAliasImplTrait(id, _) => {
                        format!("impl Trait of {}", definition(db, id.into()))
                    }
                },
            };
            format!("{name}<{}>", args(alias.args))
        }
        TyKind::Dynamic(bounds, _) => {
            let mut parts = Vec::new();
            if let Some(principal) = bounds.principal() {
                let principal = principal.skip_binder();
                parts.push(format!(
                    "{}<{}>",
                    definition(db, principal.def_id.0.into()),
                    args(principal.args)
                ));
            }
            parts.extend(bounds.auto_traits().into_iter().map(|id| definition(db, id.0.into())));
            for projection in bounds.projection_bounds() {
                let projection = projection.skip_binder();
                parts.push(format!(
                    "{} = {}",
                    definition(
                        db,
                        match projection.def_id.0 {
                            crate::next_solver::TermId::TypeAliasId(id) => id.into(),
                            crate::next_solver::TermId::ConstId(id) => id.into(),
                        }
                    ),
                    projection
                        .term
                        .as_type()
                        .map_or_else(|| "<const>".into(), |ty| stable_type(db, ty))
                ));
            }
            parts.sort();
            format!("dyn {}", parts.join(" + "))
        }
        TyKind::FnPtr(signature, header) => {
            let signature = signature.skip_binder();
            format!(
                "{}extern {} fn({}) -> {}",
                if header.safety().is_safe() { "" } else { "unsafe " },
                header.abi().as_str(),
                signature
                    .inputs()
                    .iter()
                    .map(|ty| stable_type(db, *ty))
                    .collect::<Vec<_>>()
                    .join(", "),
                stable_type(db, signature.output())
            )
        }
        TyKind::Infer(_) => "<inferred type>".into(),
        TyKind::Bound(..) => "<bound type>".into(),
        TyKind::Placeholder(_) => "<placeholder type>".into(),
        TyKind::Error(_) => "<error type>".into(),
        TyKind::Foreign(id) => definition(db, id.0.into()),
        TyKind::UnsafeBinder(_) => "<unsafe binder>".into(),
        TyKind::CoroutineWitness(..) => "<coroutine witness>".into(),
        TyKind::Pat(ty, _) => format!("{} is <pattern>", stable_type(db, ty)),
    }
}

impl Diagnostic<'_, '_> {
    fn constant(&self, error: &ConstEvalError<'_>) -> String {
        match error {
            ConstEvalError::MirLowerError(error) => self.lower(error),
            ConstEvalError::MirEvalError(error) => self.eval(error),
        }
    }

    fn lower(&self, error: &MirLowerError<'_>) -> String {
        match error {
            MirLowerError::ConstEvalError(name, error) => {
                format!("In evaluating constant {name}\n{}", self.constant(error))
            }
            MirLowerError::LayoutError(error) => format!("LayoutError({error})"),
            MirLowerError::TraitFunctionDefinition(id, name) => format!(
                "TraitFunctionDefinition({}::{})",
                definition(self.db, (*id).into()),
                name.as_str()
            ),
            MirLowerError::UnresolvedName { owner, path } => {
                let path = match path {
                    Path::LangItem(_, segment) => format!(
                        "<lang item>{}",
                        segment
                            .as_ref()
                            .map_or_else(String::new, |name| format!("::{}", name.as_str()))
                    ),
                    _ => path.mod_path().unwrap().display_verbatim(self.db).to_string(),
                };
                format!("UnresolvedName({}: {path})", owner_name(self.db, *owner))
            }
            MirLowerError::UnresolvedMethod(name) => format!("UnresolvedMethod({name})"),
            MirLowerError::UnsizedTemporary(ty) => {
                format!("UnsizedTemporary({})", (self.ty)(ty.as_ref()))
            }
            MirLowerError::MissingFunctionDefinition(body, expr) => {
                let (expr, range) =
                    expression(self.db, body.expression_store_owner(self.db), *expr);
                let location = range.map_or_else(String::new, |(file, range)| {
                    format!(" at {}", (self.span)(file, range))
                });
                format!(
                    "Missing function definition in {}{location}: {expr}",
                    body_name(self.db, *body)
                )
            }
            MirLowerError::TypeError(reason) => format!("TypeError({reason})"),
            MirLowerError::NotSupported(reason) => format!("NotSupported({reason})"),
            MirLowerError::ImplementationError(reason) => format!("ImplementationError({reason})"),
            MirLowerError::UnresolvedUpvar(place) => {
                format!("UnresolvedUpvar({})", self.place(place))
            }
            MirLowerError::GenericArgNotProvided(id, args) => format!(
                "Generic arg not provided for {}\nProvided args: [{}]",
                format!(
                    "{}::{}",
                    definition(
                        self.db,
                        match *id {
                            GenericParamId::TypeParamId(id) => id.parent(),
                            GenericParamId::ConstParamId(id) => id.parent(),
                            GenericParamId::LifetimeParamId(id) => id.parent,
                        }
                    ),
                    param_name(self.db, *id)
                ),
                args.as_ref().iter().map(|arg| (self.arg)(arg)).collect::<Vec<_>>().join(", ")
            ),
            MirLowerError::IncompleteExpr => "IncompleteExpr".into(),
            MirLowerError::IncompletePattern => "IncompletePattern".into(),
            MirLowerError::RecordLiteralWithoutPath => "RecordLiteralWithoutPath".into(),
            MirLowerError::UnresolvedField => "UnresolvedField".into(),
            MirLowerError::HasErrors => "Type inference result contains errors".into(),
            MirLowerError::ContinueWithoutLoop => "ContinueWithoutLoop".into(),
            MirLowerError::BreakWithoutLoop => "BreakWithoutLoop".into(),
            MirLowerError::Loop => "Loop".into(),
            MirLowerError::LangItemNotFound => "LangItemNotFound".into(),
            MirLowerError::MutatingRvalue => "MutatingRvalue".into(),
            MirLowerError::UnresolvedLabel => "UnresolvedLabel".into(),
            MirLowerError::InaccessibleLocal => "InaccessibleLocal".into(),
        }
    }

    fn place(&self, place: &StoredPlace) -> String {
        let mut text = format!("_{}", u32::from(place.local.into_raw()));
        for projection in place.projection.as_ref().as_slice() {
            match projection {
                ProjectionElem::Downcast(id) => {
                    let name = match id {
                        VariantId::EnumVariantId(id) => {
                            owner_name(self.db, DefWithBodyId::VariantId(*id).into())
                        }
                        VariantId::StructId(id) => definition(self.db, (*id).into()),
                        VariantId::UnionId(id) => definition(self.db, (*id).into()),
                    };
                    text.push_str(&format!(" as {name}"));
                }
                ProjectionElem::Deref => text = format!("(*{text})"),
                ProjectionElem::Field(field) => text.push_str(&format!(".{}", field.0)),
                ProjectionElem::Index(local) => {
                    text.push_str(&format!("[_{}]", u32::from(local.into_raw())))
                }
                ProjectionElem::ConstantIndex { offset, min_length, from_end } => {
                    text.push_str(&format!("[{offset}; min={min_length}, from_end={from_end}]"))
                }
                ProjectionElem::Subslice { from, to } => text.push_str(&format!("[{from}..{to}]")),
            }
        }
        text
    }

    fn eval(&self, error: &MirEvalError<'_>) -> String {
        let reason = match error {
            MirEvalError::ConstEvalError(name, error) => {
                format!("In evaluating constant {name}\n{}", self.constant(error))
            }
            MirEvalError::LayoutError(error, ty) => {
                format!("Layout for type `{}` is not available due {error}", (self.ty)(ty.as_ref()))
            }
            MirEvalError::MirLowerError(function, error) => format!(
                "MIR lowering for function `{}` failed due:\n{}",
                definition(self.db, (*function).into()),
                self.lower(error)
            ),
            MirEvalError::MirLowerErrorForClosure(closure, error) => {
                let loc = closure.loc(self.db);
                format!(
                    "MIR lowering for closure in {} failed due:\n{}",
                    body_name(self.db, loc.owner),
                    self.lower(error)
                )
            }
            MirEvalError::TypeIsUnsized(ty, reason) => {
                format!("{} is unsized. {reason} should be sized.", (self.ty)(ty.as_ref()))
            }
            MirEvalError::CoerceUnsizedError(ty) => {
                format!("CoerceUnsizedError({})", (self.ty)(ty.as_ref()))
            }
            MirEvalError::InFunction(error, stack) => {
                let mut text = String::new();
                for (function, span, body) in stack.iter().take(30).rev() {
                    let name = match function {
                        Either::Left(id) => definition(self.db, (*id).into()),
                        Either::Right(id) => body_name(self.db, id.loc(self.db).owner),
                    };
                    text.push_str(&format!("In function {name}\n"));
                    if let Some((file, range)) = span_location(self.db, *body, *span) {
                        text.push_str(&(self.span)(file, range));
                        text.push('\n');
                    }
                }
                text.push_str(&self.eval(error));
                text
            }
            MirEvalError::TargetDataLayoutNotAvailable(error) => {
                format!("TargetDataLayoutNotAvailable({error})")
            }
            MirEvalError::UndefinedBehavior(reason) => format!("UndefinedBehavior({reason})"),
            MirEvalError::Panic(reason) => format!("Panic with message:\n{reason:?}"),
            MirEvalError::NotSupported(reason) => format!("NotSupported({reason})"),
            MirEvalError::InternalError(reason) => format!("InternalError({reason})"),
            MirEvalError::InvalidConst => "InvalidConst".into(),
            MirEvalError::ExecutionLimitExceeded => "execution limit exceeded".into(),
            MirEvalError::StackOverflow => "stack overflow".into(),
            MirEvalError::InvalidVTableId(id) => format!("InvalidVTableId({id})"),
        };
        format!("Mir eval error:\n{reason}")
    }
}
