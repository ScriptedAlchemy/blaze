//! The exporter's side of the distributed work protocol (docs/distributed.md,
//! section 4): canonical root sets over identical front ends, and a worker that
//! turns a holder's grants into exported batches with [`compile_batch`].
//!
//! No instance descriptor crosses the wire. A root is granted by its index in the
//! canonical root list and a discovered generic by the digest of its `WorkKey`;
//! the request behind either stays in this process's database, retained in the
//! worker's findings until the holder releases the name. The exporter only
//! exports requests whose asking environment is the empty one (a nonempty
//! environment is `Local`), so the key's environment fingerprint is the zero
//! digest: the design's spelling of "the environment is irrelevant here", proved
//! by the exporter's own refusal everywhere else.

use std::collections::{BTreeMap, BTreeSet};

use blaze_dist::Ranges;
use blaze_dist::holder::{Compiled, Plan};
use blaze_wire::{
    Digest, EntryKey, EntryRef, Found, ItemKey, ItemOutcome, ItemResult, PackRef, RootSet, Set,
    Why, WorkKey, digest_of_encoded_name, encode,
};
use sha2::{Digest as _, Sha256};

use super::{ExportContext, LocalRequest, Outcome, Receipt, WorkName, compile_batch};
use crate::store;

/// The wire name of one exportable local request: the digest of its `WorkKey`,
/// the unit it is compiled in (its home crate's content digest), the digest of
/// its stable name, and the zero environment.
pub fn wire_name(work: &WorkName) -> blaze_wire::WorkName {
    blaze_wire::WorkName::of(&WorkKey::Item(ItemKey {
        unit: work.home,
        stable: digest_of_encoded_name("blaze/stable-name", &encode(&work.name)),
        env: [0; 32],
    }))
}

/// The digest of a canonical `(name, tuple)` list, sorted by the pair's bytes:
/// what `RootSet.roots` and `RootSet.index` are digests of here.
fn names_digest(domain: &str, names: &[(blaze_wire::WorkName, Digest)]) -> Digest {
    digest_of_encoded_name(domain, &encode(&names.to_vec()))
}

/// One executor's batch compiler over a loaded front end: its canonical roots,
/// the findings its committed batches discovered, and the receipts it sealed.
pub struct Worker<'a, 'db> {
    context: &'a ExportContext<'a, 'db>,
    store: &'a store::Store,
    roots: RootSet,
    canonical: Vec<(blaze_wire::WorkName, LocalRequest)>,
    findings: BTreeMap<blaze_wire::WorkName, LocalRequest>,
    receipts: Vec<Receipt>,
    upload: Box<dyn FnMut(&str, &[u8]) -> Result<(), String> + 'a>,
}

impl<'a, 'db> Worker<'a, 'db> {
    /// Canonicalize `roots` (the build's root requests in any local order) into
    /// the one sorted list every executor of the execution computes, and with it
    /// the `RootSet` a holder joins with. `spec` is the build specification's
    /// digest and `inputs` the digest of the script and proc-macro outputs the
    /// front end consumed; both are the caller's, from its `BuildSpec`. A root
    /// with no total work name is refused: roots are named entries, and one that
    /// cannot be named alike on every executor cannot be granted by index.
    pub fn new(
        context: &'a ExportContext<'a, 'db>,
        store: &'a store::Store,
        roots: Vec<LocalRequest>,
        spec: Digest,
        inputs: Digest,
        upload: impl FnMut(&str, &[u8]) -> Result<(), String> + 'a,
    ) -> Result<Worker<'a, 'db>, String> {
        let mut named: Vec<(blaze_wire::WorkName, Digest, LocalRequest)> = Vec::new();
        for request in roots {
            let work = context.work_name(&request)?;
            named.push((wire_name(&work), work.tuple, request));
        }
        named.sort_by(|a, b| (a.0, a.1).cmp(&(b.0, b.1)));
        named.dedup_by(|next, kept| next.0 == kept.0 && next.1 == kept.1);
        let pairs: Vec<(blaze_wire::WorkName, Digest)> =
            named.iter().map(|(name, tuple, _)| (*name, *tuple)).collect();
        let roots = RootSet {
            spec,
            roots: names_digest("blaze/root-set", &pairs),
            count: pairs.len() as u64,
            // No pre-index is built yet: granted names resolve through findings
            // alone, and an on-demand function nobody found is answered `Lost`
            // and goes to the lead's tail, never silently dropped.
            index: names_digest("blaze/pre-index", &[]),
            inputs,
        };
        let canonical = named.into_iter().map(|(name, _, request)| (name, request)).collect();
        Ok(Worker {
            context,
            store,
            roots,
            canonical,
            findings: BTreeMap::new(),
            receipts: Vec::new(),
            upload: Box::new(upload),
        })
    }

    /// The root set this worker's holder joins with.
    pub fn root_set(&self) -> RootSet {
        self.roots.clone()
    }

    /// The names resolvable with no finding: none yet (see `new`).
    pub fn pre_index(&self) -> BTreeSet<blaze_wire::WorkName> {
        BTreeSet::new()
    }

    /// The sealed receipts of every batch compiled so far, for the lead to merge
    /// and import with [`super::import_batch`] after [`merged_receipt`].
    pub fn receipts(&self) -> &[Receipt] {
        &self.receipts
    }

    /// Drop retained findings the coordinator no longer counts on.
    pub fn release(&mut self, names: &[blaze_wire::WorkName]) {
        for name in names {
            self.findings.remove(name);
        }
    }

    /// Compile one grant whole: its roots by canonical index, then its names
    /// through the findings they were granted to. Nothing is returned
    /// unprocessed; a name whose finding is gone is answered `Lost`, never
    /// left out. Each request is its own `compile_batch` call so its
    /// discoveries are attributed to it alone (the exporter's flat
    /// `discovered` list names no parent); the seal per call is the durable
    /// finish the protocol asks for before a result is posted.
    pub fn compile(&mut self, plan: &Plan) -> Result<Compiled, String> {
        let ranges =
            Ranges::from_wire(&plan.grant.roots).ok_or("granted root ranges are not canonical")?;
        let mut packs: Vec<PackRef> = Vec::new();
        let mut root_items = Vec::new();
        for index in ranges.indexes() {
            let (name, request) = self
                .canonical
                .get(index as usize)
                .ok_or_else(|| format!("granted root {index} is outside the root set"))?;
            let (name, request) = (*name, request.clone());
            root_items.push(self.compile_one(name, &request, &mut packs)?);
        }
        let mut name_items = Vec::new();
        for name in &plan.compile {
            match self.findings.get(name).cloned() {
                Some(request) => name_items.push(self.compile_one(*name, &request, &mut packs)?),
                None => name_items.push(ItemResult {
                    name: *name,
                    outcome: ItemOutcome::Lost(Why {
                        why: "lost-finding: the retained request is gone".to_owned(),
                    }),
                    found: Set::new(Vec::new()),
                    unnamed: Set::new(Vec::new()),
                }),
            }
        }
        Ok(Compiled {
            generation: plan.grant.lease.generation,
            roots: root_items,
            names: name_items,
            packs,
            returned_roots: Ranges::new(),
            returned_names: Vec::new(),
        })
    }

    fn compile_one(
        &mut self,
        name: blaze_wire::WorkName,
        request: &LocalRequest,
        packs: &mut Vec<PackRef>,
    ) -> Result<ItemResult, String> {
        let batch =
            compile_batch(self.context, self.context.digest(), &[request.clone()], self.store)?;
        let mut found = Vec::new();
        let mut unnamed: Vec<(u32, String)> = Vec::new();
        for discovered in batch.discovered {
            match discovered.work {
                Ok(work) => {
                    let wire = wire_name(&work);
                    self.findings.entry(wire).or_insert(discovered.request);
                    found.push(Found { name: wire, tuple: work.tuple });
                }
                Err(why) => unnamed.push((unnamed.len() as u32, why)),
            }
        }
        for pack in &batch.receipt.packs {
            let bytes = self.store.pack(pack).map_err(|e| e.to_string())?;
            (self.upload)(pack, &bytes)?;
            packs.push(pack_ref(pack, &bytes)?);
        }
        self.receipts.push(batch.receipt);
        let outcome = match batch.outcomes.into_iter().next() {
            Some(Outcome::Exported(exported)) => {
                ItemOutcome::Exported(EntryRef { entry: EntryKey(exported.key) })
            }
            Some(Outcome::Local { why, .. }) => ItemOutcome::Local(Why { why }),
            Some(Outcome::Failed { why, .. }) => ItemOutcome::Failed(Why { why }),
            None => return Err("a one-request batch made no outcome".to_owned()),
        };
        Ok(ItemResult { name, outcome, found: Set::new(found), unnamed: Set::new(unnamed) })
    }
}

/// The worker is a holder driver's batch compiler: the existing exporter,
/// driven by the holder protocol, with no second pipeline. The trait is the
/// holder's own, so this crate needs no carrier dependency (the local
/// carrier is Unix-only; the compiler builds everywhere ra-jit does).
impl blaze_dist::holder::BatchCompiler for Worker<'_, '_> {
    fn compile(&mut self, plan: &Plan) -> Result<Compiled, String> {
        Worker::compile(self, plan)
    }

    fn release(&mut self, names: &[blaze_wire::WorkName]) {
        Worker::release(self, names);
    }
}

/// The pack's reference on the wire: its content digest (its file name's hash),
/// its size, the digest of its disk keys in pack order, and its format.
fn pack_ref(name: &str, bytes: &[u8]) -> Result<PackRef, String> {
    let keys = store::pack_keys(name, bytes).map_err(str::to_owned)?;
    let mut hasher = Sha256::new();
    for key in &keys {
        hasher.update(key);
    }
    Ok(PackRef {
        pack: Sha256::digest(bytes).into(),
        size: bytes.len() as u64,
        keys: hasher.finalize().into(),
        packed: None,
        format: store::pack_format(bytes).ok_or("sealed pack has no format")?,
    })
}

/// One receipt of many batches of one build: the same context and producer,
/// entries deduplicated by key (two digests for one key are a determinism bug,
/// refused), packs unioned. The lead imports the merged receipt once, so its
/// closure check runs over the whole accepted set at the generation boundary.
pub fn merged_receipt(receipts: &[Receipt]) -> Result<Receipt, String> {
    let first = receipts.first().ok_or("no batch receipts to merge")?;
    let mut merged = Receipt {
        context: first.context,
        producer: first.producer,
        entries: Vec::new(),
        packs: Vec::new(),
    };
    let mut by_key: BTreeMap<super::Digest256, super::Digest256> = BTreeMap::new();
    // Check the first receipt too: collecting it into a map would silently
    // overwrite conflicting duplicates before later receipts are inspected.
    for receipt in receipts {
        if receipt.context != merged.context || receipt.producer != merged.producer {
            return Err("batch receipts of different contexts or producers".to_owned());
        }
        for entry in &receipt.entries {
            match by_key.get(&entry.key) {
                Some(previous) if previous != &entry.entry_digest => {
                    return Err(format!(
                        "entry `{}` was exported with two different digests",
                        entry.work.name
                    ));
                }
                Some(_) => {}
                None => {
                    by_key.insert(entry.key, entry.entry_digest);
                    merged.entries.push(entry.clone());
                }
            }
        }
        for pack in &receipt.packs {
            if !merged.packs.contains(pack) {
                merged.packs.push(pack.clone());
            }
        }
    }
    Ok(merged)
}
