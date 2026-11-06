//! Current included inputs across every expression-store owner in a crate.

use base_db::{AbsPathBuf, Crate, InputReadSet};
use hir_def::{
    AdtId, AssocItemId, AttrDefId, DefWithBodyId, ExpressionStoreOwnerId, GenericDefId,
    ItemContainerId, Lookup, ModuleDefId, VariantId,
    attrs::codegen_attribute_macro_calls,
    expr_store::{ExpressionStore, StoreVisitor},
    item_scope::ItemScope,
    signatures::{ConstFlags, ConstSignature, FnFlags, FunctionSignature},
};
use rustc_hash::{FxHashMap, FxHashSet};

use crate::db::HirDatabase;

#[salsa::tracked(returns(ref))]
pub fn discover_current_inputs(db: &dyn HirDatabase, krate: Crate) -> InputReadSet {
    let walk = owners(db, krate);
    let mut seen = FxHashSet::default();
    let mut reads = InputReadSet::default();
    for call in walk.calls.into_iter().filter(|call| seen.insert(*call)) {
        reads.extend(hir_expand::builtin::macro_input_reads(db, call));
    }
    reads
}

/// Observe filenames consumed by CTFE before assigning persistent code identities.
/// Anonymous and generic constants that cannot be evaluated without an instance
/// retain their exact source context instead of guessing that relocation is safe.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct IdentityInputs {
    pub unresolved: bool,
    pub source_names: Vec<base_db::CurrentSourceName>,
}

#[salsa::tracked(returns(ref))]
pub fn discover_identity_inputs(db: &dyn HirDatabase, krate: Crate) -> IdentityInputs {
    use hir_def::{expr_store::Body, hir::Expr};
    use hir_ide::next_solver::GenericArgs;
    let mut unresolved = false;
    let mut source_names = Vec::new();
    let mut proof = ContextFreeInputs::new(db);
    for owner in expression_owners(db, krate) {
        if unresolved {
            break;
        }
        let store = ExpressionStore::of(db, owner);
        let mut context_free = |expr| proof.expression(owner, expr).is_some();
        // Nested type constants are deliberately absent from inference roots
        // while a body is lowered. Visit their actual HIR references as well.
        let mut anonymous = AnonymousConsts::default();
        for (ty, _) in store.types.iter() {
            store.visit_type_ref_children(ty, &mut anonymous);
        }
        for (expr, _) in store.exprs() {
            store.visit_expr_children(expr, &mut anonymous);
        }
        for (pat, _) in store.pats() {
            store.visit_pat_children(pat, &mut anonymous);
        }
        if !matches!(owner, ExpressionStoreOwnerId::Body(_)) {
            anonymous.0.extend(store.expr_roots());
        }
        anonymous.0.sort_unstable();
        anonymous.0.dedup();
        let covered_inline = match owner {
            ExpressionStoreOwnerId::Body(body) if !matches!(body, DefWithBodyId::FunctionId(_)) => {
                directly_evaluated_inline_consts(store, Body::of(db, body).root_expr())
            }
            _ => FxHashSet::default(),
        };
        let unknown: Vec<_> = anonymous
            .0
            .into_iter()
            .filter(|expr| !covered_inline.contains(expr))
            .filter(|&expr| !context_free(expr))
            .collect();
        if !unknown.is_empty() {
            let observed = hir_ide::consteval::owner_anonymous_consts_with_source_names(db, owner);
            for (_, result) in &observed {
                source_names.extend_from_slice(&result.source_names);
            }
            if unknown.into_iter().any(|expr| {
                !observed.iter().any(|(root, result)| {
                    let same = *root == expr
                        || matches!(store[*root], Expr::Const(inner) if inner == expr);
                    same && result.value.is_ok()
                })
            }) {
                unresolved = true;
                break;
            }
        }
        match owner {
            ExpressionStoreOwnerId::Body(DefWithBodyId::ConstId(id)) => {
                let observed = hir_ide::consteval::const_eval_with_source_names(
                    db,
                    id,
                    GenericArgs::empty(),
                    None,
                );
                source_names.extend(observed.source_names);
                unresolved |=
                    observed.value.is_err() && !context_free(Body::of(db, id.into()).root_expr());
            }
            ExpressionStoreOwnerId::Body(DefWithBodyId::StaticId(id)) => {
                let observed = hir_ide::consteval::const_eval_static_with_source_names(db, id);
                source_names.extend(observed.source_names);
                unresolved |=
                    observed.value.is_err() && !context_free(Body::of(db, id.into()).root_expr());
            }
            ExpressionStoreOwnerId::Body(DefWithBodyId::VariantId(id)) => {
                let observed =
                    hir_ide::consteval::const_eval_discriminant_with_source_names(db, id);
                source_names.extend(observed.source_names);
                unresolved |=
                    observed.value.is_err() && !context_free(Body::of(db, id.into()).root_expr());
            }
            _ => {}
        }
    }
    if unresolved {
        // A later instance can evaluate an anonymous/generic constant and call a
        // dependency's const fn. Without that instance, retain every reachable
        // source spelling rather than only the caller's filename.
        let mut seen = FxHashSet::default();
        for dependency in krate.transitive_deps(db).into_iter().chain([krate]) {
            discover_current_inputs(db, dependency);
            let root = db.file_source_root(dependency.data(db).root_file_id).source_root_id(db);
            let root = db.source_root(root).source_root(db);
            for file in root.iter().filter(|file| seen.insert(*file)) {
                source_names.push(base_db::CurrentSourceName {
                    file,
                    name: hir_ide::mir::location::source_name(db, file),
                });
            }
        }
    }
    IdentityInputs { unresolved, source_names }
}

/// Inline blocks in a constant body are evaluated by that body's ordinary CTFE
/// query. A closure's body is deferred, so its roots need independent coverage.
fn directly_evaluated_inline_consts(
    store: &ExpressionStore,
    root: hir_def::hir::ExprId,
) -> FxHashSet<hir_def::hir::ExprId> {
    use hir_def::hir::Expr;
    let mut roots = FxHashSet::default();
    let mut seen = FxHashSet::default();
    let mut pending = vec![root];
    while let Some(expr) = pending.pop() {
        if !seen.insert(expr) || matches!(store[expr], Expr::Closure { .. }) {
            continue;
        }
        if let Expr::Const(inner) = store[expr] {
            roots.insert(inner);
        }
        store.walk_child_exprs(expr, |child| pending.push(child));
    }
    roots
}

/// A proof about source-name dependencies, not an evaluation of a constant's value.
/// Operators are accepted only over primitive scalar leaves; user constructors and
/// unresolved calls stay contextual because their code may observe a filename.
#[derive(Clone, Copy)]
enum ContextFreeValue {
    Scalar,
    Aggregate,
}

struct ContextFreeInputs<'db> {
    db: &'db dyn HirDatabase,
    active: FxHashSet<(ExpressionStoreOwnerId, hir_def::hir::ExprId)>,
    known: FxHashMap<(ExpressionStoreOwnerId, hir_def::hir::ExprId), Option<ContextFreeValue>>,
}

impl<'db> ContextFreeInputs<'db> {
    fn new(db: &'db dyn HirDatabase) -> Self {
        Self { db, active: FxHashSet::default(), known: FxHashMap::default() }
    }

    fn expression(
        &mut self,
        owner: ExpressionStoreOwnerId,
        expr: hir_def::hir::ExprId,
    ) -> Option<ContextFreeValue> {
        if let Some(result) = self.known.get(&(owner, expr)) {
            return *result;
        }
        if !self.active.insert((owner, expr)) {
            return None;
        }
        let result = self.expression_inner(owner, expr);
        self.active.remove(&(owner, expr));
        self.known.insert((owner, expr), result);
        result
    }

    fn expression_inner(
        &mut self,
        owner: ExpressionStoreOwnerId,
        expr: hir_def::hir::ExprId,
    ) -> Option<ContextFreeValue> {
        use ContextFreeValue::{Aggregate, Scalar};
        use hir_def::{
            builtin_type::BuiltinType,
            expr_store::{Body, scope::ExprScopes},
            hir::{Array, Expr, Literal, Statement},
            resolver::{TypeNs, ValueNs, resolver_for_scope},
            type_ref::TypeRef,
        };
        use hir_expand::{
            MacroDefKind,
            builtin::{BuiltinFnLikeExpander, EagerExpander},
        };
        let db = self.db;
        let (store, source_map) = ExpressionStore::with_source_map(db, owner);
        let resolve = || resolver_for_scope(db, owner, ExprScopes::of(db, owner).scope_for(expr));
        let scalar = |value| matches!(value, Some(Scalar));
        match &store[expr] {
            Expr::Literal(value) => {
                let source = source_map.expr_syntax(expr).ok()?;
                if let Some(call) = source.file_id.macro_file() {
                    if !matches!(
                        call.loc(db).def.kind,
                        MacroDefKind::Declarative(..)
                            | MacroDefKind::BuiltIn(
                                _,
                                BuiltinFnLikeExpander::Line | BuiltinFnLikeExpander::Column
                            )
                            | MacroDefKind::BuiltInEager(
                                _,
                                EagerExpander::Env
                                    | EagerExpander::OptionEnv
                                    | EagerExpander::Include
                                    | EagerExpander::IncludeBytes
                                    | EagerExpander::IncludeStr
                            )
                    ) {
                        return None;
                    }
                }
                Some(match value {
                    Literal::String(_) | Literal::ByteString(_) | Literal::CString(_) => Aggregate,
                    _ => Scalar,
                })
            }
            Expr::Path(path) => {
                let proved = match resolve().resolve_path_in_value_ns_fully(
                    db,
                    path,
                    store.expr_path_hygiene(expr),
                ) {
                    Some(ValueNs::ConstId(id)) => {
                        let owner = ExpressionStoreOwnerId::Body(id.into());
                        self.expression(owner, Body::of(db, id.into()).root_expr())
                    }
                    Some(ValueNs::GenericParam(id)) if db.const_param_ty(id).is_scalar() => {
                        Some(Scalar)
                    }
                    _ => None,
                };
                proved.or_else(|| {
                    let ExpressionStoreOwnerId::Body(body) = owner else { return None };
                    let inferred = hir_ide::InferenceResult::of(db, body);
                    let (hir_ide::method_resolution::CandidateId::ConstId(id), _) =
                        inferred.assoc_resolutions_for_expr(expr)?
                    else {
                        return None;
                    };
                    // Like a const parameter, a required associated value is
                    // supplied by the instance. Its implementation owns any
                    // filename reads; declarations still own their type roots.
                    let ty = inferred.expr_ty(expr);
                    (matches!(id.lookup(db).container, ItemContainerId::TraitId(_))
                        && !ConstSignature::of(db, id).flags.contains(ConstFlags::HAS_BODY)
                        && !inferred.expr_has_type_mismatch(expr)
                        && !ty.is_infer()
                        && (ty.is_numeric() || ty.is_bool() || ty.is_char()))
                    .then_some(Scalar)
                })
            }
            Expr::BinaryOp { lhs, rhs, .. } => (scalar(self.expression(owner, *lhs))
                && scalar(self.expression(owner, *rhs)))
            .then_some(Scalar),
            Expr::UnaryOp {
                expr: inner,
                op: syntax::ast::UnaryOp::Not | syntax::ast::UnaryOp::Neg,
            } => scalar(self.expression(owner, *inner)).then_some(Scalar),
            Expr::Cast { expr: inner, type_ref } => {
                if !scalar(self.expression(owner, *inner)) {
                    return None;
                }
                let TypeRef::Path(path) = &store[*type_ref] else { return None };
                match resolve().resolve_path_in_type_ns(db, path)? {
                    (
                        TypeNs::BuiltinType(
                            BuiltinType::Bool
                            | BuiltinType::Char
                            | BuiltinType::Int(_)
                            | BuiltinType::Uint(_)
                            | BuiltinType::Float(_),
                        ),
                        None,
                        _,
                    ) => Some(Scalar),
                    _ => None,
                }
            }
            Expr::Const(inner) => self.expression(owner, *inner),
            Expr::Tuple { exprs } => exprs
                .iter()
                .all(|&expr| self.expression(owner, expr).is_some())
                .then_some(Aggregate),
            Expr::Array(Array::ElementList { elements }) => elements
                .iter()
                .all(|&expr| self.expression(owner, expr).is_some())
                .then_some(Aggregate),
            Expr::Array(Array::Repeat { initializer, repeat }) => {
                (self.expression(owner, *initializer).is_some()
                    && scalar(self.expression(owner, *repeat)))
                .then_some(Aggregate)
            }
            Expr::Block { statements, tail, .. }
                if statements.iter().all(|stmt| matches!(stmt, Statement::Item(_))) =>
            {
                tail.map_or(Some(Aggregate), |tail| self.expression(owner, tail))
            }
            _ => None,
        }
    }
}

#[derive(Default)]
struct AnonymousConsts(Vec<hir_def::hir::ExprId>);

impl StoreVisitor for AnonymousConsts {
    fn on_anon_const_expr(&mut self, expr: hir_def::hir::ExprId) {
        self.0.push(expr);
    }
}

pub(crate) fn expression_owners(db: &dyn HirDatabase, krate: Crate) -> Vec<ExpressionStoreOwnerId> {
    owners(db, krate).complete
}

fn owners(db: &dyn HirDatabase, krate: Crate) -> Owners {
    let mut walk = Owners {
        seen: FxHashSet::default(),
        pending: Vec::new(),
        complete: Vec::new(),
        calls: Vec::new(),
    };
    for module in crate::Crate::from(krate).modules(db) {
        let id = hir_def::ModuleId::from(module);
        walk.scope(db, id.def_map(db).module(db, id).scope(db));
    }
    while let Some(owner) = walk.pending.pop() {
        walk.complete.push(owner);
        let (store, source_map) = ExpressionStore::with_source_map(db, owner);
        walk.calls.extend(source_map.macro_calls().map(|(_, call)| call));
        for (_, map) in store.blocks(db) {
            for (_, module) in map.modules(db) {
                walk.scope(db, module.scope(db));
            }
        }
    }
    walk
}

pub fn current_input_paths(db: &dyn HirDatabase, krate: Crate) -> Vec<AbsPathBuf> {
    let mut paths: Vec<_> = discover_current_inputs(db, krate)
        .included
        .iter()
        .filter(|input| input.krate == krate)
        .map(|input| input.path.clone())
        .collect();
    paths.sort();
    paths.dedup();
    paths
}

struct Owners {
    seen: FxHashSet<ExpressionStoreOwnerId>,
    pending: Vec<ExpressionStoreOwnerId>,
    complete: Vec<ExpressionStoreOwnerId>,
    calls: Vec<hir_expand::MacroCallId>,
}

impl Owners {
    fn owner(&mut self, owner: ExpressionStoreOwnerId) {
        if self.seen.insert(owner) {
            self.pending.push(owner);
        }
    }

    fn signature(&mut self, owner: GenericDefId) {
        self.owner(ExpressionStoreOwnerId::Signature(owner));
    }

    fn body(&mut self, owner: DefWithBodyId) {
        self.owner(ExpressionStoreOwnerId::Body(owner));
    }

    fn assoc(&mut self, db: &dyn HirDatabase, item: AssocItemId) {
        self.signature(item.into());
        match item {
            AssocItemId::FunctionId(id) => {
                self.codegen_attributes(db, id.into());
                self.body(id.into());
            }
            AssocItemId::ConstId(id) => {
                // Required trait constants have a signature but no expression
                // to evaluate. Defaults and implementations own their bodies.
                if !matches!(id.lookup(db).container, ItemContainerId::TraitId(_))
                    || ConstSignature::of(db, id).flags.contains(ConstFlags::HAS_BODY)
                {
                    self.body(id.into());
                }
            }
            AssocItemId::TypeAliasId(_) => {}
        }
    }

    fn codegen_attributes(&mut self, db: &dyn HirDatabase, owner: AttrDefId) {
        self.calls.extend(codegen_attribute_macro_calls(db, owner));
    }

    fn scope(&mut self, db: &dyn HirDatabase, scope: &ItemScope) {
        self.calls.extend(scope.all_macro_calls());
        for def in scope.declarations() {
            match def {
                ModuleDefId::FunctionId(id) => {
                    self.codegen_attributes(db, id.into());
                    self.signature(id.into());
                    let sig = FunctionSignature::of(db, id);
                    if !matches!(id.lookup(db).container, ItemContainerId::ExternBlockId(_))
                        && !(sig.flags.contains(FnFlags::RUSTC_INTRINSIC) && !sig.has_body())
                    {
                        self.body(id.into());
                    }
                }
                ModuleDefId::ConstId(id) => {
                    self.signature(id.into());
                    self.body(id.into());
                }
                ModuleDefId::StaticId(id) => {
                    self.codegen_attributes(db, id.into());
                    self.signature(id.into());
                    self.body(id.into());
                }
                ModuleDefId::AdtId(id) => {
                    self.signature(id.into());
                    match id {
                        AdtId::EnumId(id) => {
                            for &(variant, _) in id.enum_variants(db).variants.values() {
                                self.body(variant.into());
                                self.owner(ExpressionStoreOwnerId::VariantFields(variant.into()));
                            }
                        }
                        other => self.owner(ExpressionStoreOwnerId::VariantFields(
                            VariantId::from_non_enum(other).unwrap(),
                        )),
                    }
                }
                ModuleDefId::TraitId(id) => {
                    self.signature(id.into());
                    for &(_, item) in id.trait_items(db).items.iter() {
                        self.assoc(db, item);
                    }
                }
                ModuleDefId::TypeAliasId(id) => self.signature(id.into()),
                _ => {}
            }
        }
        for id in scope.unnamed_consts() {
            self.body(id.into());
        }
        for id in scope.impls() {
            self.signature(id.into());
            for &(_, item) in id.impl_items(db).items.iter() {
                self.assoc(db, item);
            }
        }
    }
}
