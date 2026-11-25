//! Immutable grants over a traversed unit. Each task describes the complete input to an
//! emitter, including the ordered LLVM import closure or every thunk of a folded body.
//! Planning needs no object store and makes no assertion that any output has been emitted.

use blaze_code::record::{Space, Symbol, Target};
use blaze_ids::{ContentId, Hasher};
use blaze_int::Int;
use blaze_own::{ABTreeMap, AVec, Alloc, BTreeMap, BTreeSet, Vec};

use crate::code_records::{CodeKey, StoreError};

mod fold;
pub use fold::{Constant, Fold, Mask, Member, Parameter};

const UNIT_DOMAIN: &str = "blaze-ladder backend unit 1";
const RECIPE_DOMAIN: &str = "blaze-ladder backend recipe 1";
const RELEASE_DOMAIN: &str = "blaze-ladder release blob 1";

/// A resolved place in backend IR. A direct call and a slot load can name the same symbol.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub struct Reference {
    pub target: Target,
    pub binding: Binding,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
#[repr(u8)]
pub enum Binding {
    Direct,
    Slot,
}

impl Reference {
    fn kind(self) -> [u8; 2] {
        [self.target.space.byte(), u8::from(self.binding == Binding::Slot)]
    }

    fn hash(self, hash: &mut Hasher) {
        hash.update(&self.kind()).update(&self.target.symbol.0);
    }
}

/// Semantic inputs from the backend preparation that will actually be used for emission.
#[derive(Clone, Copy)]
pub struct Recipe<'a> {
    /// Content of prepared IR including local data, relocations, relative line/debug tables
    /// and ABI. External references in the IR are places in the function's references.
    pub prepared: ContentId,
    /// Backend producer, toolchain, target, ABI options, profile inputs and effective flags.
    pub settings: ContentId,
    /// The code identities of bodies built into the prepared IR, in backend order.
    pub built_in: &'a [CodeKey],
}

#[derive(Clone, Copy)]
pub struct Summary<'a> {
    pub key: ContentId,
    /// The verified canonical encoding used by the LLVM import decider.
    pub bytes: &'a [u8],
}

#[derive(Clone, Copy)]
pub struct Release<'a> {
    pub recipe: Recipe<'a>,
    pub summary: Summary<'a>,
    /// Canonical encoding of the chosen transformations, including specialized arguments,
    /// inferred callee attributes, discarded parameters and the mapping of places into the
    /// merged import table. The emitter must consume these same decisions.
    pub decisions: &'a [u8],
    /// The LLVM decider's full ordered closure, with each imported instance listed once.
    pub imports: &'a [Symbol],
}

#[derive(Clone, Copy)]
pub enum Backend<'a> {
    Standalone,
    Foldable { recipe: Recipe<'a>, mask: Mask<'a> },
    Release(Release<'a>),
}

pub struct Function<'a> {
    pub symbol: Symbol,
    /// Local writer's lookup key. The immutable emitted bytes have a separate content key.
    pub template: CodeKey,
    /// All read-list facts validated for this build. A late read can move the stored content
    /// while the local lookup key stays the same.
    pub reads: ContentId,
    pub references: &'a [Reference],
    pub backend: Backend<'a>,
}

pub struct Static<'a> {
    pub symbol: Symbol,
    pub value: ContentId,
    pub references: &'a [Reference],
}

pub struct Unit<'a> {
    /// Product and request inputs outside the walk, including settings for empty units.
    pub product: ContentId,
    pub functions: &'a [Function<'a>],
    pub statics: &'a [Static<'a>],
    pub roots: &'a [Symbol],
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub struct GrantName(pub ContentId);

/// A planned lookup key and the content identity the writer actually stored under it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Written {
    lookup: CodeKey,
    content: CodeKey,
}

impl Written {
    pub fn lookup(self) -> CodeKey {
        self.lookup
    }

    pub fn content(self) -> CodeKey {
        self.content
    }

    /// Join the writer's returned content key to the pointer in its outgoing publication.
    pub fn check(
        lookup: CodeKey,
        pointed: Option<CodeKey>,
        emitted: CodeKey,
    ) -> Result<Self, StoreError> {
        if pointed != Some(emitted) {
            return Err(StoreError::Failed {
                key: lookup,
                what: "a planned code output must point to the content the emitter wrote",
            });
        }
        Ok(Self { lookup, content: emitted })
    }
}

impl GrantName {
    pub fn standalone(lookup: CodeKey) -> Self {
        Self(lookup.0)
    }
}

pub enum Task<'a> {
    Standalone { function: &'a Function<'a> },
    Fold(Fold<'a>),
    Release { function: &'a Function<'a>, imports: AVec<'a, &'a Function<'a>>, blob: CodeKey },
}

pub struct WorkSet<'a> {
    pub digest: ContentId,
    tasks: ABTreeMap<'a, GrantName, Task<'a>>,
    bindings: ABTreeMap<'a, Symbol, BindingPlan<'a>>,
}

/// Every original instance keeps its own resolved references when immutable templates share a grant.
pub struct BindingPlan<'a> {
    pub function: &'a Function<'a>,
    pub grant: GrantName,
}

#[derive(Debug, Eq, PartialEq)]
pub enum PlanError {
    RepeatedFunction(Symbol),
    RepeatedStatic(Symbol),
    MissingRoot(Symbol),
    SummaryConflict(ContentId),
    InvalidImport { root: Symbol, import: Symbol },
    InvalidPlace { function: Symbol, place: u32 },
    GrantCollision(GrantName),
    UnknownGrant(GrantName),
    RepeatedGrant(GrantName),
}

fn count(hash: &mut Hasher, len: usize) {
    hash.update(&len.widen_to::<u64>().to_le_bytes());
}

fn references(hash: &mut Hasher, refs: &[Reference]) {
    count(hash, refs.len());
    for place in refs {
        place.hash(hash);
    }
}

fn recipe_key(function: &Function<'_>, recipe: Recipe<'_>) -> CodeKey {
    let mut hash = Hasher::new(RECIPE_DOMAIN);
    hash.id(&function.template.0).id(&function.reads).id(&recipe.prepared).id(&recipe.settings);
    count(&mut hash, recipe.built_in.len());
    for key in recipe.built_in {
        hash.id(&key.0);
    }
    references(&mut hash, function.references);
    // A self-reference is emitted differently from a reference to an outside symbol.
    for reference in function.references {
        hash.update(&[u8::from(
            reference.target.space == Space::Func && reference.target.symbol == function.symbol,
        )]);
    }
    CodeKey(hash.finish())
}

impl<'a> WorkSet<'a> {
    pub fn plan(unit: Unit<'a>, alloc: Alloc<'a>) -> Result<Self, PlanError> {
        let mut functions = BTreeMap::new_in(alloc);
        let mut summaries = BTreeMap::new_in(alloc);
        let mut releases = BTreeMap::new_in(alloc);
        for function in unit.functions {
            if functions.insert(function.symbol, function).is_some() {
                return Err(PlanError::RepeatedFunction(function.symbol));
            }
            if let Backend::Release(release) = function.backend {
                let summary = release.summary;
                if summaries
                    .insert(summary.key, summary.bytes)
                    .is_some_and(|old| old != summary.bytes)
                {
                    return Err(PlanError::SummaryConflict(summary.key));
                }
                releases.insert(
                    function.symbol,
                    (function, release, recipe_key(function, release.recipe)),
                );
            }
        }

        let mut hash = Hasher::new(UNIT_DOMAIN);
        hash.id(&unit.product);
        count(&mut hash, functions.len());
        for (&symbol, function) in &functions {
            hash.update(&symbol.0).id(&function.template.0).id(&function.reads);
            references(&mut hash, function.references);
            if let Backend::Release(release) = function.backend {
                hash.update(&[1]).id(&release.summary.key);
            } else {
                hash.update(&[0]);
            }
        }
        let mut statics = BTreeMap::new_in(alloc);
        for datum in unit.statics {
            if statics.insert(datum.symbol, datum).is_some() {
                return Err(PlanError::RepeatedStatic(datum.symbol));
            }
        }
        count(&mut hash, statics.len());
        for (&symbol, datum) in &statics {
            hash.update(&symbol.0).id(&datum.value);
            references(&mut hash, datum.references);
        }
        count(&mut hash, unit.roots.len());
        for root in unit.roots {
            if !functions.contains_key(root) && !statics.contains_key(root) {
                return Err(PlanError::MissingRoot(*root));
            }
            hash.update(&root.0);
        }
        count(&mut hash, summaries.len());
        for (key, bytes) in &summaries {
            hash.id(key).part(bytes);
        }

        let mut tasks = BTreeMap::new_in(alloc);
        let mut assigned = BTreeMap::new_in(alloc);
        for group in fold::groups(&functions, alloc)? {
            let name = group.name();
            for member in &group.members {
                assigned.insert(member.function.symbol, BindingPlan { function: member.function, grant: name });
            }
            if tasks.insert(name, Task::Fold(group)).is_some() {
                return Err(PlanError::GrantCollision(name));
            }
        }
        for (&symbol, &function) in &functions {
            if assigned.contains_key(&symbol) {
                continue;
            }
            if let Some(&(_, release, key)) = releases.get(&symbol) {
                let mut blob = Hasher::new(RELEASE_DOMAIN);
                blob.id(&key.0).part(release.decisions);
                count(&mut blob, release.imports.len());
                let mut seen = BTreeSet::new_in(alloc);
                let mut imports = Vec::with_capacity_in(release.imports.len(), alloc);
                for &import in release.imports {
                    let &(imported, _, exported) = releases
                        .get(&import)
                        .filter(|_| import != symbol && seen.insert(import))
                        .ok_or(PlanError::InvalidImport { root: symbol, import })?;
                    blob.update(&import.0).id(&exported.0);
                    imports.push(imported);
                }
                let blob = CodeKey(blob.finish());
                let name = GrantName(blob.0);
                assigned.insert(symbol, BindingPlan { function, grant: name });
                match tasks.get(&name) {
                    Some(Task::Release { .. }) => continue,
                    Some(_) => return Err(PlanError::GrantCollision(name)),
                    None => tasks.insert(name, Task::Release { function, imports, blob }),
                };
            } else {
                let name = GrantName::standalone(function.template);
                assigned.insert(symbol, BindingPlan { function, grant: name });
                match tasks.get(&name) {
                    Some(Task::Standalone { function: prior }) if prior.reads == function.reads => {
                        continue;
                    }
                    Some(_) => return Err(PlanError::GrantCollision(name)),
                    None => tasks.insert(name, Task::Standalone { function }),
                };
            }
        }
        for (symbol, binding) in &assigned {
            hash.update(&symbol.0).id(&binding.grant.0);
        }
        count(&mut hash, tasks.len());
        for (name, task) in &tasks {
            let kind = match task {
                Task::Standalone { .. } => 0,
                Task::Fold(_) => 1,
                Task::Release { .. } => 2,
            };
            hash.update(&[kind]).id(&name.0);
        }
        Ok(Self { digest: hash.finish(), tasks, bindings: assigned })
    }

    pub fn tasks(&self) -> impl ExactSizeIterator<Item = (&GrantName, &Task<'a>)> {
        self.tasks.iter()
    }

    pub fn binding(&self, symbol: Symbol) -> Option<&BindingPlan<'a>> {
        self.bindings.get(&symbol)
    }

    /// Resolve an entire lease before handing the first task to an emitter.
    pub fn select<'s>(
        &'s self,
        names: &[GrantName],
        alloc: Alloc<'s>,
    ) -> Result<AVec<'s, &'s Task<'a>>, PlanError> {
        let mut selected = Vec::with_capacity_in(names.len(), alloc);
        let mut seen = BTreeSet::new_in(alloc);
        for name in names {
            if !seen.insert(*name) {
                return Err(PlanError::RepeatedGrant(*name));
            }
            selected.push(self.tasks.get(name).ok_or(PlanError::UnknownGrant(*name))?);
        }
        Ok(selected)
    }
}

#[cfg(test)]
mod tests;
