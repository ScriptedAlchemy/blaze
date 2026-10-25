//! What each root and instance of a base refers to, by item, and the names its code is written
//! with: an edit that changes items asks again about those that refer to one that changed, or
//! call something by a name bound to another thing now, rather than about all of each crate
//! that can see the edited files (#344).

use blaze_items::{Index, ItemId, Pushed, Refs};
use hir_def::{DefWithBodyId, FunctionId, ModuleId};
use hir_ty::db::HirDatabase;
use intern::Symbol;
use rustc_hash::{FxHashMap, FxHashSet};

use super::{
    Instance,
    home::mentioned_bodies,
    patch::{BaseInstance, FxIndexMap, probe},
};

#[derive(Clone)]
pub(super) struct ItemPush {
    pub(super) index: Index,
    /// By the index of each in the base; `None` for one whose code can't be told.
    roots: Vec<Option<Used>>,
    instances: Vec<Option<Used>>,
}

#[derive(Clone)]
struct Used {
    items: Box<[ItemId]>,
    /// In the order of their text.
    names: Box<[Symbol]>,
    /// The modules the code is written in, and those they are in.
    modules: Box<[ModuleId]>,
}

impl Used {
    fn touched(&self, pushed: &Pushed) -> bool {
        self.items.iter().any(|it| pushed.changed.contains(it))
            || pushed.rebinds(&self.modules, &self.names)
    }
}

/// What [`ItemPush::asked`] found to ask about, by the index of each in the base.
#[derive(Clone)]
pub(super) struct Asked {
    pub(super) roots: Vec<u32>,
    pub(super) instances: Vec<u32>,
}

impl ItemPush {
    pub(super) fn new(
        db: &dyn HirDatabase,
        compiled: &[(FunctionId, String)],
        instances: &FxIndexMap<Instance, BaseInstance>,
    ) -> ItemPush {
        let crates: Vec<base_db::Crate> = base_db::all_crates(db)
            .iter()
            .copied()
            .filter(|it| it.data(db).origin.is_local())
            .collect();
        let index = Index::new(db, &crates);
        let refs = Refs::new(db);
        let mut bodies: FxHashMap<DefWithBodyId, Option<Used>> = FxHashMap::default();
        let mut of_body = |body: DefWithBodyId| -> Option<Used> {
            bodies.entry(body).or_insert_with(|| used(db, &refs, body)).clone()
        };
        let roots = compiled.iter().map(|&(root, _)| of_body(root.into())).collect();
        let instances = instances
            .keys()
            .map(|instance| used_by_instance(db, &refs, instance, &mut of_body))
            .collect();
        ItemPush { index, roots, instances }
    }

    /// The roots and instances that refer to what the edit changed, or call something by a
    /// name bound to something else now, and those that can't be told.
    pub(super) fn asked(&self, pushed: &Pushed) -> Asked {
        let ask = |used: &Option<Used>| used.as_ref().is_none_or(|it| it.touched(pushed));
        Asked {
            roots: (0..self.roots.len() as u32).filter(|&i| ask(&self.roots[i as usize])).collect(),
            instances: (0..self.instances.len() as u32)
                .filter(|&i| ask(&self.instances[i as usize]))
                .collect(),
        }
    }

    /// Notes what the roots and instances asked about refer to now, of the revision after the
    /// edit: what called a thing by a name bound to another calls that one now.
    pub(super) fn settle(
        &mut self,
        db: &dyn HirDatabase,
        asked: &Asked,
        compiled: &[(FunctionId, String)],
        instances: &FxIndexMap<Instance, BaseInstance>,
    ) {
        let refs = Refs::new(db);
        let mut of_body = |body: DefWithBodyId| used(db, &refs, body);
        for &i in &asked.roots {
            self.roots[i as usize] = of_body(compiled[i as usize].0.into());
        }
        for &i in &asked.instances {
            let Some((instance, _)) = instances.get_index(i as usize) else { continue };
            self.instances[i as usize] = used_by_instance(db, &refs, instance, &mut of_body);
        }
    }
}

/// What an instance refers to: its arguments, and the bodies it has the code of or mentions a
/// type of.
fn used_by_instance(
    db: &dyn HirDatabase,
    refs: &Refs<'_>,
    instance: &Instance,
    of_body: &mut dyn FnMut(DefWithBodyId) -> Option<Used>,
) -> Option<Used> {
    let mut items = probe(|| refs.of_args(instance.args.as_ref())).flatten()?;
    let (mut names, mut modules) = (Vec::new(), Vec::new());
    for body in probe(|| mentioned_bodies(db, instance)).flatten()? {
        let of = of_body(body)?;
        items.extend(of.items);
        names.extend(of.names);
        modules.extend(of.modules);
    }
    let mut unique = FxHashSet::default();
    items.retain(|it| unique.insert(*it));
    let mut unique = FxHashSet::default();
    modules.retain(|it| unique.insert(*it));
    names.sort_unstable_by(|a, b| a.as_str().cmp(b.as_str()));
    names.dedup();
    Some(Used { items: items.into(), names: names.into(), modules: modules.into() })
}

fn used(db: &dyn HirDatabase, refs: &Refs<'_>, body: DefWithBodyId) -> Option<Used> {
    let id = match body {
        DefWithBodyId::FunctionId(it) => ItemId::Function(it),
        DefWithBodyId::ConstId(it) => ItemId::Const(it),
        DefWithBodyId::StaticId(it) => ItemId::Static(it),
        DefWithBodyId::VariantId(_) => return None,
    };
    let mut items = probe(|| refs.of_code(id, Some(body))).flatten()?;
    items.push(id);
    let names = probe(|| blaze_items::code_names(db, body))?;
    let modules = probe(|| blaze_items::modules_of(db, id))?;
    Some(Used { items: items.into(), names: names.into(), modules: modules.into() })
}
