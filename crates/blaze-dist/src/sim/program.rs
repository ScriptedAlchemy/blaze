//! A program for simulated executors to compile: roots, generic instances and on-demand
//! functions, the calls between them (cycles included), requests with no stable name, and the
//! ways one executor's front end can differ from the lead's.
//!
//! Compiling an item is a pure function of the item, so every executor that compiles one gets
//! the same [`ItemResult`], and the checker can hold an accepted result to what the lead's front
//! end says it must be.

use std::collections::{BTreeMap, BTreeSet};

use blaze_wire::{
    Digest, EntryKey, EntryRef, Found, ItemKey, ItemOutcome, ItemResult, RootSet, Set, Why,
    WorkKey, WorkName,
};

use super::Rng;

/// What compiling one item gives.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Item {
    /// Its name.
    pub name: WorkName,
    /// The tuple of its full local request.
    pub tuple: Digest,
    /// Its outcome.
    pub outcome: ItemOutcome,
    /// What its code names: each callee's name and the tuple this call site holds for it.
    pub calls: Vec<Found>,
    /// Requests of its code with no stable name: ordinal and reason.
    pub unnamed: Vec<(u32, String)>,
}

impl Item {
    /// The item's result, as an executor reports it. A front end with impure inputs compiles
    /// other code: its entries differ by `salt`.
    pub fn result(&self, salt: u64) -> ItemResult {
        let outcome = match &self.outcome {
            ItemOutcome::Exported(entry) if salt != 0 => {
                let mut bytes = entry.entry.0;
                for (b, s) in bytes.iter_mut().zip(salt.to_le_bytes()) {
                    *b ^= s;
                }
                ItemOutcome::Exported(EntryRef { entry: EntryKey(bytes) })
            }
            outcome => outcome.clone(),
        };
        ItemResult {
            name: self.name,
            outcome,
            found: Set::new(self.calls.clone()),
            unnamed: Set::new(self.unnamed.clone()),
        }
    }
}

/// How one helper's front end differs from the lead's.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Divergence {
    /// Two roots with one `WorkKey` and different tuples, held swapped.
    SwappedTie,
    /// One root is another function.
    OtherRoot,
    /// The same roots, from an impure macro or build script: the inputs digest and the code
    /// differ.
    Impure,
}

/// What one executor's front end computed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FrontEnd {
    /// Its sorted roots.
    pub roots: Vec<Item>,
    /// Its root set.
    pub rootset: RootSet,
    /// Its code's difference: 0 for the lead's code.
    pub salt: u64,
}

/// The program.
#[derive(Clone, Debug)]
pub struct Program {
    /// The lead's front end: the truth an accepted result is held to.
    pub lead: FrontEnd,
    /// Generic instances and on-demand functions, by name.
    pub instances: BTreeMap<WorkName, Item>,
    /// The names every executor's pre-index resolves: the on-demand ones.
    pub index: BTreeSet<WorkName>,
    /// The names the lead's daemon rejects.
    pub rejected: BTreeSet<WorkName>,
    spec: Digest,
    index_digest: Digest,
    inputs: Digest,
    tie: Option<usize>,
}

fn name(rng: &mut Rng, units: &[Digest]) -> (WorkName, WorkKey) {
    let key = WorkKey::Item(ItemKey {
        unit: units[rng.below(units.len() as u64) as usize],
        stable: rng.digest(),
        env: if rng.chance(700_000) { [0; 32] } else { rng.digest() },
    });
    (WorkName::of(&key), key)
}

fn outcome(rng: &mut Rng, name: &WorkName, tuple: &Digest) -> ItemOutcome {
    let mut entry = [0u8; 32];
    for (i, b) in entry.iter_mut().enumerate() {
        *b = name.0[i] ^ tuple[(i + 7) % 32];
    }
    match rng.below(100) {
        0..=74 => ItemOutcome::Exported(EntryRef { entry: EntryKey(entry) }),
        75..=82 => ItemOutcome::FromStore(EntryRef { entry: EntryKey(entry) }),
        83..=89 => ItemOutcome::Failed(Why { why: "a CodegenError".to_owned() }),
        _ => ItemOutcome::Local(Why { why: "not exportable".to_owned() }),
    }
}

impl Program {
    /// A program drawn from `rng`. With `conflicts`, some call sites hold another tuple for
    /// their callee than the others do: an identity the work key does not tell apart.
    pub fn generate(rng: &mut Rng, conflicts: bool) -> Program {
        let units = [rng.digest(), rng.digest()];
        let n_instances = rng.range(0, 48) as usize;
        let mut instances: Vec<Item> = (0..n_instances)
            .map(|_| {
                let (name, _) = name(rng, &units);
                let tuple = rng.digest();
                Item {
                    name,
                    tuple,
                    outcome: outcome(rng, &name, &tuple),
                    calls: Vec::new(),
                    unnamed: Vec::new(),
                }
            })
            .collect();
        let n_roots = rng.range(1, 40) as usize;
        let mut roots: Vec<Item> = (0..n_roots)
            .map(|_| {
                let (name, _) = name(rng, &units);
                let tuple = rng.digest();
                Item {
                    name,
                    tuple,
                    outcome: outcome(rng, &name, &tuple),
                    calls: Vec::new(),
                    unnamed: Vec::new(),
                }
            })
            .collect();
        // A tied pair: one work key, two requests the key does not tell apart, distinct roots.
        if rng.chance(400_000) {
            let twin = roots[0].name;
            let tuple = rng.digest();
            roots.push(Item {
                name: twin,
                tuple,
                outcome: outcome(rng, &twin, &tuple),
                calls: Vec::new(),
                unnamed: Vec::new(),
            });
        }
        let true_tuples: Vec<(WorkName, Digest)> =
            instances.iter().map(|i| (i.name, i.tuple)).collect();
        let wire = |rng: &mut Rng, item: &mut Item, most: u64| {
            if matches!(item.outcome, ItemOutcome::FromStore(_) | ItemOutcome::Failed(_)) {
                // Neither compiled nor followed: it names nothing.
                return;
            }
            if !true_tuples.is_empty() {
                for _ in 0..rng.range(0, most) {
                    let (callee, tuple) = true_tuples[rng.below(true_tuples.len() as u64) as usize];
                    let tuple = if conflicts && rng.chance(40_000) { rng.digest() } else { tuple };
                    item.calls.push(Found { name: callee, tuple });
                }
            }
            if rng.chance(80_000) {
                for ordinal in 0..rng.range(1, 2) as u32 {
                    item.unnamed.push((ordinal, "a const argument with no stable name".to_owned()));
                }
            }
        };
        for item in &mut roots {
            wire(rng, item, 3);
        }
        for item in &mut instances {
            wire(rng, item, 2);
        }
        roots.sort_by(|a, b| (a.name, a.tuple).cmp(&(b.name, b.tuple)));
        let tie = roots.windows(2).position(|w| w[0].name == w[1].name);
        let index: BTreeSet<WorkName> =
            instances.iter().filter(|_| rng.chance(300_000)).map(|i| i.name).collect();
        let rejected: BTreeSet<WorkName> =
            instances.iter().filter(|_| rng.chance(40_000)).map(|i| i.name).collect();
        let spec = rng.digest();
        let index_digest = blaze_wire::digest_of_encoded_name(
            "PreIndex",
            &blaze_wire::encode(&Set::new(index.iter().copied().collect())),
        );
        let inputs = rng.digest();
        let lead =
            FrontEnd { rootset: rootset(spec, &roots, index_digest, inputs), roots, salt: 0 };
        Program {
            lead,
            instances: instances.into_iter().map(|i| (i.name, i)).collect(),
            index,
            rejected,
            spec,
            index_digest,
            inputs,
            tie,
        }
    }

    /// A helper's front end that differs from the lead's as `divergence` says, when the
    /// program has what it needs (a tie to swap).
    pub fn diverge(&self, rng: &mut Rng, divergence: Divergence) -> FrontEnd {
        let mut roots = self.lead.roots.clone();
        let mut inputs = self.inputs;
        let mut salt = 0;
        match (divergence, self.tie) {
            (Divergence::SwappedTie, Some(at)) => roots.swap(at, at + 1),
            (Divergence::SwappedTie, None) | (Divergence::OtherRoot, _) => {
                let at = rng.below(roots.len() as u64) as usize;
                roots[at].name = WorkName(rng.digest());
                roots[at].outcome = ItemOutcome::Local(Why { why: "another function".to_owned() });
                roots.sort_by(|a, b| (a.name, a.tuple).cmp(&(b.name, b.tuple)));
            }
            (Divergence::Impure, _) => {
                inputs = rng.digest();
                salt = rng.next_u64() | 1;
            }
        }
        FrontEnd { rootset: rootset(self.spec, &roots, self.index_digest, inputs), roots, salt }
    }
}

/// The root set of a sorted root list: the digest of its `(WorkName, tuple)` pairs.
fn rootset(spec: Digest, roots: &[Item], index: Digest, inputs: Digest) -> RootSet {
    let pairs: Vec<(WorkName, Digest)> = roots.iter().map(|r| (r.name, r.tuple)).collect();
    RootSet {
        spec,
        roots: blaze_wire::digest_of_encoded_name("RootList", &blaze_wire::encode(&pairs)),
        count: roots.len() as u64,
        index,
        inputs,
    }
}
