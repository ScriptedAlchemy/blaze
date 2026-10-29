//! Compiler checks which must run even when no generated code reaches an item.

use std::sync::Arc;

use hir_def::{
    DefWithBodyId, ExpressionStoreOwnerId, HasModule as _, Lookup as _,
    expr_store::ExpressionStore,
    hir::Expr,
    signatures::{ConstSignature, FunctionSignature, StaticFlags, StaticSignature},
    src::HasSource as _,
};
use hir_expand::name::AsName as _;
use hir_ide::{InferenceResult, db::signature_anon_consts_and_diagnostics};
use rustc_hash::FxHashSet;
use salsa::SalsaValue;
use syntax::{AstNode as _, ast};

use crate::{
    Crate, HasCrate as _, HasSource as _, HasVisibility as _, Module, ModuleDef, PathResolution,
    Semantics, Type, db::HirDatabase,
};

#[derive(Debug, Clone, Default, PartialEq, Eq, SalsaValue)]
pub struct Acceptance {
    pub errors: Vec<String>,
    pub unsupported: Vec<String>,
}

/// Check active signatures, references and constant initializers, including items
/// emitted by macros and bodies that are never called.
#[salsa::tracked(returns(ref))]
pub fn check(db: &dyn HirDatabase, krate: base_db::Crate) -> Acceptance {
    let sema = Semantics::new_dyn_within(db, Arc::new(FxHashSet::from_iter([krate])));
    let mut check = Check { db, krate: krate.into(), sema, result: Acceptance::default() };
    for module in Crate::from(krate).modules(db) {
        check.imports(module);
    }
    for owner in crate::input_owners::expression_owners(db, krate) {
        check.owner(owner);
    }
    check.result.errors.sort();
    check.result.errors.dedup();
    check.result.unsupported.sort();
    check.result.unsupported.dedup();
    check.result
}

struct Check<'db> {
    db: &'db dyn HirDatabase,
    krate: Crate,
    sema: Semantics<'db, dyn HirDatabase>,
    result: Acceptance,
}

impl<'db> Check<'db> {
    fn owner(&mut self, owner: ExpressionStoreOwnerId) {
        let db = self.db;
        if let ExpressionStoreOwnerId::Body(def) = owner {
            let has_body = match def {
                DefWithBodyId::FunctionId(id) => FunctionSignature::of(db, id).has_body(),
                DefWithBodyId::ConstId(id) => ConstSignature::of(db, id).has_body(),
                DefWithBodyId::StaticId(id) => {
                    StaticSignature::of(db, id).flags.contains(StaticFlags::HAS_BODY)
                }
                DefWithBodyId::VariantId(id) => crate::EnumVariant::from(id)
                    .source(db)
                    .and_then(|source| source.value.const_arg())
                    .and_then(|arg| arg.expr())
                    .is_some(),
            };
            if !has_body {
                return;
            }
        }
        let module = Module::from(owner.generic_def(db).module(db));
        let (store, sources) = ExpressionStore::with_source_map(db, owner);
        for (_, map) in store.blocks(db) {
            for (id, _) in map.modules(db) {
                self.imports(Module::from(id));
            }
        }
        for (id, _) in store.types.iter() {
            let Ok(source) = sources.type_syntax(id) else { continue };
            let node = self.sema.to_node(source);
            if let Some(ty) = self.sema.resolve_type(&node) {
                if ty.contains_unknown() {
                    self.result.errors.push(format!("unresolved type `{node}`"));
                }
                self.ty(&ty);
            } else {
                self.result.unsupported.push(format!("cannot validate type `{node}`"));
            }
            for path in node.syntax().descendants().filter_map(ast::Path::cast) {
                if let Some(def) = self.resolve_reference(&path) {
                    self.reference(def, module, source.file_id.is_macro());
                }
            }
        }
        let constant = match owner {
            ExpressionStoreOwnerId::Body(
                DefWithBodyId::ConstId(_)
                | DefWithBodyId::StaticId(_)
                | DefWithBodyId::VariantId(_),
            ) => true,
            ExpressionStoreOwnerId::Body(DefWithBodyId::FunctionId(id)) => {
                FunctionSignature::of(db, id).is_const()
            }
            _ => false,
        };
        let mut const_exprs = FxHashSet::default();
        if constant && let ExpressionStoreOwnerId::Body(def) = owner {
            const_children(
                store,
                hir_def::expr_store::Body::of(db, def).root_expr(),
                &mut const_exprs,
            );
        }
        if let ExpressionStoreOwnerId::Signature(def) = owner {
            for (constants, _) in signature_anon_consts_and_diagnostics(db, def) {
                for &id in constants {
                    const_children(store, id.loc(db).expr, &mut const_exprs);
                }
            }
        }
        for (_, expr) in store.exprs() {
            if let Expr::Const(inner) = expr {
                const_children(store, *inner, &mut const_exprs);
            }
        }
        for (id, _) in store.exprs() {
            let Ok(source) = sources.expr_syntax(id) else { continue };
            let node = self.sema.to_node(source);
            for path in node.syntax().children().filter_map(ast::Path::cast) {
                for part in path.syntax().descendants().filter_map(ast::Path::cast) {
                    if let Some(def) = self.resolve_reference(&part) {
                        self.reference(def, module, source.file_id.is_macro());
                    }
                }
            }
            if const_exprs.contains(&id)
                && let Some(cast) = ast::CastExpr::cast(node.syntax().clone())
                && let Some(expr) = cast.expr()
                && let (Some(from), Some(to)) = (
                    self.sema.type_of_expr(&expr),
                    self.sema.type_of_expr(&ast::Expr::CastExpr(cast.clone())),
                )
                && (from.original.is_raw_ptr() || from.original.is_fn())
                && to.original.is_int_or_uint()
            {
                self.result
                    .errors
                    .push("pointers cannot be cast to integers during const evaluation".into());
            }
            if let Some(call) = ast::MethodCallExpr::cast(node.syntax().clone()) {
                if let Some(function) = self.sema.resolve_method_call(&call) {
                    self.reference(function.into(), module, source.file_id.is_macro());
                    if const_exprs.contains(&id) && !function.is_const(db) {
                        self.result
                            .errors
                            .push(format!("non-const method in constant context: `{call}`"));
                    }
                }
            }
            if const_exprs.contains(&id)
                && let Some(call) = ast::CallExpr::cast(node.syntax().clone())
                && let Some(callee) = call.expr()
            {
                match ast::PathExpr::cast(callee.syntax().clone())
                    .and_then(|expr| expr.path())
                    .and_then(|path| self.sema.resolve_path(&path))
                {
                    Some(PathResolution::Def(ModuleDef::Function(function)))
                        if !function.is_const(db) =>
                    {
                        self.result
                            .errors
                            .push(format!("non-const function in constant context: `{call}`"));
                    }
                    Some(PathResolution::Def(
                        ModuleDef::Adt(_) | ModuleDef::EnumVariant(_) | ModuleDef::Function(_),
                    )) => {}
                    _ => self
                        .result
                        .unsupported
                        .push(format!("cannot validate constant call `{call}`")),
                }
            }
        }
        match owner {
            ExpressionStoreOwnerId::Body(def) => {
                let inferred = InferenceResult::of(db, def);
                if inferred.has_type_mismatches() || inferred.is_erroneous() {
                    self.result.errors.push("type inference failed".into());
                }
                for (_, ty) in inferred.expression_types() {
                    self.ty(&Type::new_body(db, owner, ty));
                }
                let borrowed = hir_ty_borrowck::source_borrowck(db, def);
                self.result.errors.extend(
                    borrowed
                        .diagnostics
                        .iter()
                        .filter(|diag| diag.code.starts_with('E'))
                        .map(|diag| diag.message.clone()),
                );
                if let Some(reason) = &borrowed.skipped {
                    self.result.unsupported.push(format!("borrow checking: {reason}"));
                }
                match def {
                    DefWithBodyId::ConstId(id) => {
                        if let Err(error) = crate::Const::from(id).eval(db) {
                            self.constant_error(error);
                        }
                    }
                    DefWithBodyId::StaticId(id) => {
                        if let Err(error) = crate::Static::from(id).eval(db) {
                            self.constant_error(error);
                        }
                    }
                    _ => {}
                }
            }
            ExpressionStoreOwnerId::Signature(def) => {
                for (constants, _) in signature_anon_consts_and_diagnostics(db, def) {
                    for &id in constants {
                        if let Err(error) = (crate::AnonConst { id }).eval(db) {
                            self.constant_error(error);
                        }
                    }
                }
            }
            ExpressionStoreOwnerId::VariantFields(_) => {}
        }
    }

    fn ty(&mut self, ty: &Type<'db>) {
        ty.walk(self.db, |ty| {
            if let Some(trait_) = ty.as_dyn_trait()
                && trait_.dyn_compatibility(self.db).is_some()
            {
                self.result.errors.push("trait is not dyn compatible".into());
            }
        });
    }

    fn imports(&mut self, module: Module) {
        let id = hir_def::ModuleId::from(module);
        let map = id.def_map(self.db);
        let scope = map.module(self.db, id).scope(self.db);
        for import in scope.use_decls() {
            let source = import.lookup(self.db).source(self.db);
            self.sema.parse_or_expand(source.file_id);
            for path in source.value.syntax().descendants().filter_map(ast::Path::cast) {
                if let Some(def) = self.resolve_reference(&path) {
                    self.reference(def, module, source.file_id.is_macro());
                }
            }
        }
    }

    fn resolve_reference(&self, path: &ast::Path) -> Option<ModuleDef> {
        if let Some(PathResolution::Def(def)) = self.sema.resolve_path(path) {
            return Some(def);
        }
        // Normal resolution hides inaccessible candidates. Retain the item for
        // the visibility diagnostic rather than accepting an unresolved body.
        let ModuleDef::Module(module) = self.resolve_reference(&path.qualifier()?)? else {
            return None;
        };
        let name = path.segment()?.name_ref()?.as_name();
        let id = hir_def::ModuleId::from(module);
        let map = id.def_map(self.db);
        let found = map.module(self.db, id).scope(self.db).get(&name);
        found.take_values().or_else(|| found.take_types()).map(ModuleDef::from)
    }

    fn reference(&mut self, def: ModuleDef, from: Module, expanded: bool) {
        if !def.is_visible_from(self.db, from) {
            let message = format!("private item: {:?}", def.name(self.db));
            if expanded {
                self.result.unsupported.push(format!("macro privacy context: {message}"));
            } else {
                self.result.errors.push(message);
            }
        }
        if def.krate(self.db) != self.krate
            && let Some(attrs) = def.attrs(self.db)
            && attrs.is_unstable()
            && attrs
                .unstable_feature(self.db)
                .is_none_or(|feature| !self.krate.is_unstable_feature_enabled(self.db, &feature))
        {
            let message = format!("use of unstable library item: {:?}", def.name(self.db));
            if expanded {
                self.result.unsupported.push(format!("macro stability context: {message}"));
            } else {
                self.result.errors.push(message);
            }
        }
    }

    fn constant_error(&mut self, error: hir_ide::consteval::ConstEvalError<'db>) {
        if invalid_constant(&error) {
            self.result.errors.push(format!("constant evaluation failed: {error:?}"));
        } else {
            self.result.unsupported.push(format!("constant evaluation: {error:?}"));
        }
    }
}

fn invalid_constant(error: &hir_ide::consteval::ConstEvalError<'_>) -> bool {
    match error {
        hir_ide::consteval::ConstEvalError::MirEvalError(error) => invalid_evaluation(error),
        _ => false,
    }
}

fn invalid_evaluation(error: &hir_ide::mir::MirEvalError<'_>) -> bool {
    use hir_ide::mir::MirEvalError;
    match error {
        MirEvalError::Panic(_)
        | MirEvalError::UndefinedBehavior(_)
        | MirEvalError::TypeIsUnsized(..) => true,
        MirEvalError::InFunction(error, _) => invalid_evaluation(error),
        MirEvalError::ConstEvalError(_, error) => invalid_constant(error),
        _ => false,
    }
}

fn const_children(
    store: &ExpressionStore,
    expr: hir_def::hir::ExprId,
    found: &mut FxHashSet<hir_def::hir::ExprId>,
) {
    if !found.insert(expr) || matches!(store[expr], Expr::Closure { .. }) {
        return;
    }
    store.walk_child_exprs(expr, |child| const_children(store, child, found));
}
