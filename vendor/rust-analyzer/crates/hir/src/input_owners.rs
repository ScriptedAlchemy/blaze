//! Current included inputs across every expression-store owner in a crate.

use base_db::{AbsPathBuf, Crate, InputReadSet};
use hir_def::{
    AdtId, AssocItemId, AttrDefId, DefWithBodyId, ExpressionStoreOwnerId, GenericDefId,
    ItemContainerId, Lookup, ModuleDefId, VariantId,
    attrs::codegen_attribute_macro_calls,
    expr_store::{ExpressionStore, StoreVisitor},
    item_scope::ItemScope,
    signatures::{FnFlags, FunctionSignature},
};
use rustc_hash::FxHashSet;

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
    use hir_expand::{
        MacroDefKind,
        builtin::{BuiltinFnLikeExpander, EagerExpander},
    };
    use hir_ide::next_solver::GenericArgs;
    let mut unresolved = false;
    let mut source_names = Vec::new();
    for owner in expression_owners(db, krate) {
        if unresolved {
            break;
        }
        let (store, source_map) = ExpressionStore::with_source_map(db, owner);
        let context_free_literal = |expr| {
            if !matches!(&store[expr], Expr::Literal(_)) {
                return false;
            }
            let Ok(source) = source_map.expr_syntax(expr) else { return false };
            let Some(call) = source.file_id.macro_file() else { return true };
            matches!(
                call.loc(db).def.kind,
                MacroDefKind::BuiltIn(
                    _,
                    BuiltinFnLikeExpander::Line | BuiltinFnLikeExpander::Column
                ) | MacroDefKind::BuiltInEager(
                    _,
                    EagerExpander::Env
                        | EagerExpander::OptionEnv
                        | EagerExpander::Include
                        | EagerExpander::IncludeBytes
                        | EagerExpander::IncludeStr
                )
            )
        };
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
        if anonymous.0.into_iter().any(|expr| !context_free_literal(expr)) {
            unresolved = true;
            break;
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
                unresolved |= observed.value.is_err();
            }
            ExpressionStoreOwnerId::Body(DefWithBodyId::StaticId(id)) => {
                let observed = hir_ide::consteval::const_eval_static_with_source_names(db, id);
                source_names.extend(observed.source_names);
                unresolved |= observed.value.is_err();
            }
            ExpressionStoreOwnerId::Body(DefWithBodyId::VariantId(id)) => {
                let observed =
                    hir_ide::consteval::const_eval_discriminant_with_source_names(db, id);
                source_names.extend(observed.source_names);
                unresolved |= observed.value.is_err();
            }
            ExpressionStoreOwnerId::Body(body) => {
                let root = Body::of(db, body).root_expr();
                unresolved |=
                    store.expr_roots().any(|expr| expr != root && !context_free_literal(expr))
                        || store.exprs().any(|(_, expr)| {
                            matches!(
                                expr,
                                Expr::Const(inner) if !context_free_literal(*inner)
                            )
                        });
            }
            _ => unresolved |= store.expr_roots().any(|expr| !context_free_literal(expr)),
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
            AssocItemId::ConstId(id) => self.body(id.into()),
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
