//! Current included inputs across every expression-store owner in a crate.

use base_db::{AbsPathBuf, Crate, CurrentIncludedInput};
use hir_def::{
    AdtId, AssocItemId, DefWithBodyId, ExpressionStoreOwnerId, GenericDefId, ItemContainerId,
    Lookup, ModuleDefId, VariantId,
    expr_store::ExpressionStore,
    item_scope::ItemScope,
    signatures::{FnFlags, FunctionSignature},
};
use rustc_hash::FxHashSet;

use crate::db::HirDatabase;

#[salsa::tracked]
pub fn discover_current_inputs(db: &dyn HirDatabase, krate: Crate) {
    let mut walk = Owners { seen: FxHashSet::default(), pending: Vec::new() };
    for module in crate::Crate::from(krate).modules(db) {
        let id = hir_def::ModuleId::from(module);
        walk.scope(db, id.def_map(db).module(db, id).scope(db));
    }
    while let Some(owner) = walk.pending.pop() {
        let store = ExpressionStore::of(db, owner);
        for (_, map) in store.blocks(db) {
            for (_, module) in map.modules(db) {
                walk.scope(db, module.scope(db));
            }
        }
    }
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

    fn assoc(&mut self, item: AssocItemId) {
        self.signature(item.into());
        match item {
            AssocItemId::FunctionId(id) => self.body(id.into()),
            AssocItemId::ConstId(id) => self.body(id.into()),
            AssocItemId::TypeAliasId(_) => {}
        }
    }

    fn scope(&mut self, db: &dyn HirDatabase, scope: &ItemScope) {
        for def in scope.declarations() {
            match def {
                ModuleDefId::FunctionId(id) => {
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
                        self.assoc(item);
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
                self.assoc(item);
            }
        }
    }
}
