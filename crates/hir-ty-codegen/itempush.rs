//! What each root and instance of a base refers to, by item, and the names its code is written
//! with: an edit that changes items asks again about those that refer to one that changed, or
//! call something by a name bound to another thing now, rather than about all of each crate
//! that can see the edited files (#344).

use blaze_items::{Index, ItemId, Pushed, Refs};
use hir_def::{DefWithBodyId, FunctionId, ModuleId};
use hir_ty::db::HirDatabase;
use intern::Symbol;
use rustc_hash::{FxHashMap, FxHashSet};

use std::sync::OnceLock;

use super::{
    Instance, SymbolResolver,
    home::mentioned_bodies,
    patch::{BaseInstance, FxIndexMap, probe},
};

#[derive(Clone)]
pub(super) struct ItemPush {
    pub(super) index: Index,
    /// By the index of each in the base; `Err` says why one's code can't be told.
    roots: Vec<Told>,
    instances: Vec<Told>,
}

type Told = Result<Used, &'static str>;

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
    /// Made on the threads `symbols` has, each taking a part of the items, the roots and the
    /// instances in turn.
    pub(super) fn new(
        db: &dyn HirDatabase,
        symbols: &dyn SymbolResolver,
        compiled: &[(FunctionId, String)],
        instances: &FxIndexMap<Instance, BaseInstance>,
    ) -> ItemPush {
        const PART: usize = 64;
        let start = std::time::Instant::now();
        let crates: Vec<base_db::Crate> = base_db::all_crates(db)
            .iter()
            .copied()
            .filter(|it| it.data(db).origin.is_local())
            .collect();
        let items = blaze_items::declared_by(db, &crates);
        let declared = start.elapsed();
        let instances: Vec<&Instance> = instances.keys().collect();
        let parts = |n: usize| n.div_ceil(PART);
        let (of_items, of_roots) = (parts(items.len()), parts(compiled.len()));
        fn lock<T>(n: usize) -> Vec<OnceLock<T>> {
            (0..n).map(|_| OnceLock::new()).collect()
        }
        let entries: Vec<OnceLock<blaze_items::Entries>> = lock(of_items);
        let roots: Vec<OnceLock<Vec<Told>>> = lock(of_roots);
        let of_instances: Vec<OnceLock<Vec<Told>>> = lock(parts(instances.len()));
        let part = |all: usize, at: usize| at * PART..((at + 1) * PART).min(all);
        let work = |db: &dyn HirDatabase, at: usize| {
            if let Some(at) = at.checked_sub(of_items + of_roots) {
                of_instances[at].get_or_init(|| {
                    let refs = Refs::new(db);
                    let mut bodies: FxHashMap<DefWithBodyId, Told> = FxHashMap::default();
                    let mut of_body = |body: DefWithBodyId| -> Told {
                        bodies.entry(body).or_insert_with(|| used(db, &refs, body)).clone()
                    };
                    part(instances.len(), at)
                        .map(|i| used_by_instance(db, &refs, instances[i], &mut of_body))
                        .collect::<Vec<_>>()
                });
            } else if let Some(at) = at.checked_sub(of_items) {
                roots[at].get_or_init(|| {
                    let refs = Refs::new(db);
                    part(compiled.len(), at)
                        .map(|i| used(db, &refs, compiled[i].0.into()))
                        .collect::<Vec<_>>()
                });
            } else {
                entries[at].get_or_init(|| blaze_items::entries(db, &items[part(items.len(), at)]));
            }
        };
        let count = of_items + of_roots + of_instances.len();
        symbols.in_parallel(count, &|db, _, at| work(db, at));
        (0..count).for_each(|at| work(db, at));
        let made = start.elapsed();
        let index =
            Index::of_entries(db, &crates, entries.into_iter().filter_map(OnceLock::into_inner));
        let flat = |parts: Vec<OnceLock<Vec<Told>>>| {
            parts.into_iter().filter_map(OnceLock::into_inner).flatten().collect::<Vec<_>>()
        };
        let (roots, instances) = (flat(roots), flat(of_instances));
        if std::env::var_os("RA_JIT_STATS").is_some() {
            let mut why: FxHashMap<&str, usize> = FxHashMap::default();
            for it in roots.iter().chain(&instances) {
                if let Err(it) = it {
                    *why.entry(it).or_default() += 1;
                }
            }
            let mut why: Vec<_> = why.into_iter().collect();
            why.sort_by(|a, b| b.1.cmp(&a.1));
            let unknown = |it: &[Told]| it.iter().filter(|it| it.is_err()).count();
            eprintln!(
                "items-made: declared {} us, written {} us, indexed {} us; {} of {} roots and {} of {} instances unknown: {:?}",
                declared.as_micros(),
                (made - declared).as_micros(),
                (start.elapsed() - made).as_micros(),
                unknown(&roots),
                roots.len(),
                unknown(&instances),
                instances.len(),
                &why[..why.len().min(6)]
            );
        }
        ItemPush { index, roots, instances }
    }

    /// The roots and instances that refer to what the edit changed, or call something by a
    /// name bound to something else now, and those that can't be told.
    pub(super) fn asked(&self, pushed: &Pushed) -> Asked {
        let ask = |used: &Told| used.as_ref().ok().is_none_or(|it| it.touched(pushed));
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
    of_body: &mut dyn FnMut(DefWithBodyId) -> Told,
) -> Told {
    let mut items = probe(|| refs.of_args(instance.args.as_ref())).ok_or(PANICKED)??;
    let (mut names, mut modules) = (Vec::new(), Vec::new());
    let bodies = probe(|| mentioned_bodies(db, instance)).ok_or(PANICKED)?;
    for body in bodies.ok_or("an instance whose bodies can't be told")? {
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
    Ok(Used { items: items.into(), names: names.into(), modules: modules.into() })
}

const PANICKED: &str = "a query that panicked";

fn used(db: &dyn HirDatabase, refs: &Refs<'_>, body: DefWithBodyId) -> Told {
    let id = match body {
        DefWithBodyId::FunctionId(it) => ItemId::Function(it),
        DefWithBodyId::ConstId(it) => ItemId::Const(it),
        DefWithBodyId::StaticId(it) => ItemId::Static(it),
        DefWithBodyId::VariantId(_) => return Err("a variant's discriminant"),
    };
    let mut items = probe(|| refs.of_code(id, Some(body))).ok_or(PANICKED)??;
    items.push(id);
    let names = probe(|| blaze_items::code_names(db, body)).ok_or(PANICKED)?;
    let modules = probe(|| blaze_items::modules_of(db, id)).ok_or(PANICKED)?;
    Ok(Used { items: items.into(), names: names.into(), modules: modules.into() })
}
