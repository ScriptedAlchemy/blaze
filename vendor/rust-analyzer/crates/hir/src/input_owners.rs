//! Current included inputs across every expression-store owner in a crate.

use base_db::{AbsPathBuf, Crate, CurrentIncludedInput};
use hir_def::{
    AdtId, AssocItemId, AttrDefId, DefWithBodyId, ExpressionStoreOwnerId, GenericDefId,
    ItemContainerId, Lookup, ModuleDefId, VariantId,
    attrs::AttrFlags,
    expr_store::ExpressionStore,
    item_scope::ItemScope,
    signatures::{FnFlags, FunctionSignature},
};
use rustc_hash::FxHashSet;

use crate::db::HirDatabase;

#[salsa::tracked]
pub fn discover_current_inputs(db: &dyn HirDatabase, krate: Crate) {
    expression_owners(db, krate);
}

/// Observe filenames consumed by CTFE before assigning persistent code identities.
/// Anonymous and generic constants that cannot be evaluated without an instance
/// retain their exact source context instead of guessing that relocation is safe.
#[salsa::tracked(returns(copy))]
pub fn discover_identity_inputs(db: &dyn HirDatabase, krate: Crate) -> bool {
    use hir_def::{expr_store::Body, hir::Expr};
    use hir_expand::{
        MacroDefKind,
        builtin::{BuiltinFnLikeExpander, EagerExpander},
    };
    use hir_ide::next_solver::GenericArgs;
    let mut unresolved = false;
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
        match owner {
            ExpressionStoreOwnerId::Body(DefWithBodyId::ConstId(id)) => {
                unresolved |= db.const_eval(id, GenericArgs::empty(), None).is_err();
            }
            ExpressionStoreOwnerId::Body(DefWithBodyId::StaticId(id)) => {
                unresolved |= db.const_eval_static(id).is_err();
            }
            ExpressionStoreOwnerId::Body(DefWithBodyId::VariantId(id)) => {
                unresolved |= db.const_eval_discriminant(id).is_err();
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
                hir_ide::mir::location::source_name(db, file);
            }
        }
    }
    unresolved
}

pub(crate) fn expression_owners(db: &dyn HirDatabase, krate: Crate) -> Vec<ExpressionStoreOwnerId> {
    let mut walk = Owners { seen: FxHashSet::default(), pending: Vec::new() };
    for module in crate::Crate::from(krate).modules(db) {
        let id = hir_def::ModuleId::from(module);
        walk.scope(db, id.def_map(db).module(db, id).scope(db));
    }
    let mut owners = Vec::new();
    while let Some(owner) = walk.pending.pop() {
        owners.push(owner);
        let store = ExpressionStore::of(db, owner);
        for (_, map) in store.blocks(db) {
            for (_, module) in map.modules(db) {
                walk.scope(db, module.scope(db));
            }
        }
    }
    owners
}

pub fn current_input_paths(db: &dyn HirDatabase, krate: Crate) -> Vec<AbsPathBuf> {
    let mut paths: Vec<_> = discover_current_inputs::accumulated::<CurrentIncludedInput>(db, krate)
        .into_iter()
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
                Self::codegen_attributes(db, id.into());
                self.body(id.into());
            }
            AssocItemId::ConstId(id) => self.body(id.into()),
            AssocItemId::TypeAliasId(_) => {}
        }
    }

    fn codegen_attributes(db: &dyn HirDatabase, owner: AttrDefId) {
        // These values expand macros lazily outside expression stores. Their
        // environment and included-file reads affect symbols/sections and must be
        // discovered before content identity or missing-input validation, even when
        // no body or emitted symbol has been requested yet. Reuse the compiler's
        // tracked semantic queries rather than inspecting attribute token text.
        AttrFlags::export_name(db, owner);
        AttrFlags::link_name(db, owner);
        AttrFlags::link_section(db, owner);
        AttrFlags::linkage(db, owner);
    }

    fn scope(&mut self, db: &dyn HirDatabase, scope: &ItemScope) {
        for def in scope.declarations() {
            match def {
                ModuleDefId::FunctionId(id) => {
                    Self::codegen_attributes(db, id.into());
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
                    Self::codegen_attributes(db, id.into());
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
