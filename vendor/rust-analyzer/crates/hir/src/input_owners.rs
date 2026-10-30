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
