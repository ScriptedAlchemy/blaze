//! Crate units over the product's actual reached native backend tasks. A group is assigned
//! once, to its first member's crate. Each member's crate still keys on the complete group.

use super::{
    DistConfig, DistError, DistStore, append_report_row, matches_output_fingerprint, own_str,
    unit_digest_of,
};
use crate::code_store::HomeCodeStore;
use crate::engine_seeds::CrateAt;
use blaze_code::record::Symbol;
use blaze_core::Driver;
use blaze_dist_local::agent::{self, BackendKind, Emission, Grant, Granted, Unit};
use blaze_dist_local::pack::{Value, ValueRef};
use blaze_ids::ContentId;
use blaze_ladder::backend_plan::{
    GrantName, Task, WorkSet,
    execute::{self, Emit, OutputKind},
};
use blaze_ladder::code_records::{Builder, CodeClass, CodeKey, CodeStore, StoreError, content_key};
use blaze_ladder::run::make_one::{
    NamesRecords,
    source_unit::{Reached, Row},
};
use blaze_own::{AVec, Alloc, BTreeMap, BTreeSet, CollectIn, Heap, String, Vec};
use blaze_wire::{Digest, WireAlloc, WireVec};
use core::{
    fmt::{self, Write as _},
    sync::atomic::AtomicBool,
};

pub(crate) mod artifact;
#[cfg(test)]
mod tests;

struct Side<'a> {
    key: CodeKey,
    bytes: &'a [u8],
}

struct Crate<'a> {
    name: &'a str,
    product: bool,
    unit: Unit,
    names: AVec<'a, GrantName>,
    side: AVec<'a, Side<'a>>,
    /// Framed pairs of source symbol and grant or static-entry identity.
    members: AVec<'a, (Symbol, CodeKey)>,
}

fn refused(heap: &'static Heap, why: fmt::Arguments<'_>) -> DistError {
    let mut shown = String::new_in(heap.alloc());
    let _ = shown.write_fmt(why);
    DistError::Compile { shown }
}

pub(crate) struct Request<'a, 'n> {
    pub(crate) names: NamesRecords<'n>,
    pub(crate) closure: &'a [CrateAt],
    pub(crate) builder: &'a Builder,
    pub(crate) product: Digest,
    pub(crate) native_image: ContentId,
    pub(crate) compiler: ContentId,
    pub(crate) checked: ContentId,
    pub(crate) program: &'a str,
    pub(crate) destination: &'a str,
    pub(crate) heap: &'static Heap,
}

fn crates<'s, 'n: 's, M>(
    request: &Request<'_, 'n>,
    reached: &'s Reached<'_, M>,
    work: &WorkSet<'_>,
    alloc: Alloc<'s>,
    wire: WireAlloc,
) -> Result<AVec<'s, Crate<'s>>, DistError> {
    let mut slots = BTreeMap::new_in(alloc);
    let mut crates = Vec::new_in(alloc);
    // Records are in dependency order from the root down. Run the depending unit last.
    for idx in (0..request.closure.len()).rev() {
        let Some(rec) = request.names.records.items_record(idx) else {
            if idx == 0 {
                return Err(refused(
                    request.heap,
                    format_args!("missing product crate record for {}", request.program),
                ));
            }
            continue;
        };
        let key = rec.key();
        let name = rec
            .name()
            .map_err(|why| refused(request.heap, format_args!("source crate {key:?}: {why}")))?;
        let slot = crates.len();
        if slots.insert(key.prefix(), slot).is_some() {
            return Err(refused(
                request.heap,
                format_args!("repeated source crate prefix for {name}"),
            ));
        }
        crates.push(Crate {
            name,
            product: idx == 0,
            unit: Unit { spec: key.0.0, names: WireVec::new_in(wire) },
            names: Vec::new_in(alloc),
            side: Vec::new_in(alloc),
            members: Vec::new_in(alloc),
        });
    }
    let mut owners = BTreeMap::new_in(alloc);
    for (at, row) in reached.rows.iter().enumerate() {
        let item = reached.walk.tabled(at);
        let symbol = reached.walk.symbol(at);
        let slot = *slots.get(&crate::run_ids::prefix_of(item)).ok_or_else(|| {
            refused(request.heap, format_args!("no crate owns source item {item:?} ({symbol})"))
        })?;
        let unit = crates
            .get_mut(slot)
            .ok_or_else(|| refused(request.heap, format_args!("crate slot {slot} for {symbol}")))?;
        let key = match row {
            Row::Function(body) => {
                let binding = work.binding(symbol).ok_or_else(|| {
                    refused(request.heap, format_args!("no backend task for {symbol}"))
                })?;
                for table in &body.vtables {
                    unit.side.push(Side { key: content_key(&table.bytes), bytes: &table.bytes });
                }
                CodeKey(binding.grant.0)
            }
            Row::Datum(bytes) => {
                let key = content_key(bytes);
                unit.side.push(Side { key, bytes });
                key
            }
        };
        if owners.insert(symbol, slot).is_some() {
            return Err(refused(request.heap, format_args!("two source rows own symbol {symbol}")));
        }
        unit.members.push((symbol, key));
    }
    for (&grant, task) in work.tasks() {
        let function = match task {
            Task::Standalone { function } | Task::Release { function, .. } => *function,
            Task::Fold(group) => {
                group
                    .members
                    .first()
                    .ok_or_else(|| {
                        refused(request.heap, format_args!("no member of folded grant {grant:?}"))
                    })?
                    .function
            }
        };
        let slot = owners
            .get(&function.symbol)
            .ok_or_else(|| refused(request.heap, format_args!("no unit owns grant {grant:?}")))?;
        let krate = crates.get_mut(*slot).ok_or_else(|| {
            refused(request.heap, format_args!("crate slot {slot} for grant {grant:?}"))
        })?;
        krate.names.push(grant);
        krate.unit.names.push(grant.0.0);
    }
    // A proc-macro-only crate belongs to its real Wasm compilation. Only reached native
    // crates and the final placement own a native unit.
    crates.retain(|krate| krate.product || !krate.members.is_empty());
    let mut previous = Vec::new_in(alloc);
    for krate in &mut crates {
        krate.members.sort_unstable();
        krate.side.sort_unstable_by_key(|side| side.key);
        for [left, right] in krate.side.array_windows::<2>() {
            if left.key == right.key && left.bytes != right.bytes {
                return Err(DistError::Store(StoreError::TwoValues { key: left.key }));
            }
        }
        krate.side.dedup_by(|left, right| left.key == right.key);
        let mut inputs = Vec::new_in(alloc);
        inputs.push(krate.unit.spec);
        inputs.push(request.compiler.0);
        for (symbol, key) in &krate.members {
            inputs.push(symbol.0);
            inputs.push(key.0.0);
        }
        if krate.product {
            inputs.push(request.product);
            inputs.extend_from_slice(&previous);
        }
        krate.unit.spec =
            unit_digest_of(krate.name, request.builder, true, &krate.unit.names, &inputs, alloc);
        previous.push(krate.unit.spec);
    }
    Ok(crates)
}

struct Holder<'s, 'p, 'h, 'm, D: Driver, B> {
    krate: &'s Crate<'s>,
    work: &'s WorkSet<'p>,
    backend: &'s mut B,
    store: &'s mut DistStore<'h, 'm, D>,
    alloc: Alloc<'s>,
    wire: WireAlloc,
    heap: &'static Heap,
}

fn side(store: &mut dyn CodeStore, data: &[Side<'_>]) -> Result<(), StoreError> {
    for Side { key, bytes } in data {
        store.put(CodeClass::Program, *key, bytes)?;
    }
    Ok(())
}

/// A publication is self-contained. Bytes left by an earlier local execution cannot
/// satisfy a missing member or data record in a committed pack.
fn check_publication<'v>(
    krate: &Crate<'_>,
    work: &WorkSet<'_>,
    data: &[Side<'_>],
    value_count: usize,
    lookup: impl Fn(CodeKey) -> Option<(u8, &'v [u8])>,
    alloc: Alloc<'_>,
    heap: &'static Heap,
) -> Result<(), DistError> {
    let mut seen = BTreeSet::new_in(alloc);
    let mut get = |key: CodeKey| {
        let Some((kind, bytes)) = lookup(key) else { return Ok(None) };
        if kind != super::KIND_CODE {
            return Err(StoreError::Failed {
                key,
                what: "native grants publish immutable code values",
            });
        }
        seen.insert(key);
        Ok(Some(bytes))
    };
    for row in data {
        if get(row.key).map_err(DistError::Store)? != Some(row.bytes) {
            return Err(refused(
                heap,
                format_args!(
                    "committed native unit {} omitted or changed data {}",
                    krate.name, row.key
                ),
            ));
        }
    }
    let tasks = work.select(&krate.names, alloc).map_err(|why| {
        refused(heap, format_args!("checking committed native unit {}: {why}", krate.name))
    })?;
    for task in tasks {
        let mut outputs = Vec::new_in(alloc);
        task.outputs(|output, recipe| outputs.push((output, recipe)));
        for (output, recipe) in outputs {
            execute::read_with::<core::convert::Infallible>(&mut get, output, recipe)
                .map_err(|why| {
                    refused(heap, format_args!("committed native output {}: {why}", output.symbol))
                })?
                .ok_or_else(|| {
                    refused(
                        heap,
                        format_args!(
                            "committed native unit {} omitted {} ({recipe})",
                            krate.name, output.symbol
                        ),
                    )
                })?;
        }
    }
    if seen.len() != value_count {
        return Err(refused(
            heap,
            format_args!(
                "committed native unit {} contains records outside its grants",
                krate.name
            ),
        ));
    }
    Ok(())
}

impl<D: Driver, B: Emit> Granted for Holder<'_, '_, '_, '_, D, B>
where
    B::Error: fmt::Display,
{
    type Error = DistError;

    fn compile(&mut self, indexes: &[usize]) -> Result<Grant, DistError> {
        let mut names = Vec::with_capacity_in(indexes.len(), self.alloc);
        for &index in indexes {
            let grant = self.krate.names.get(index).ok_or_else(|| {
                refused(self.heap, format_args!("grant index {index} outside {}", self.krate.name))
            })?;
            names.push(*grant);
        }
        let tasks = self.work.select(&names, self.alloc).map_err(|why| {
            refused(self.heap, format_args!("native lease for {}: {why}", self.krate.name))
        })?;
        self.store.clear_tracking();
        let mut emissions = WireVec::new_in(self.wire);
        let mut stored = 0;
        let mut compiled = 0;
        for task in tasks {
            let rows = task.compile(self.backend, self.store, self.alloc).map_err(|why| {
                refused(self.heap, format_args!("native emission for {}: {why}", self.krate.name))
            })?;
            for row in rows {
                if row.source == execute::Source::Store {
                    stored += 1;
                    continue;
                }
                let backend = match row.output.kind {
                    OutputKind::Standalone => None,
                    OutputKind::ReleaseBlob => Some(BackendKind::ReleaseBlob),
                    OutputKind::FoldedBody | OutputKind::Thunk => Some(BackendKind::FoldedOutput),
                };
                compiled += 1;
                emissions.push(Emission { key: row.content.0.0, backend });
            }
        }
        let values = self.store.drain_tracked_values().map_err(DistError::Store)?;
        Ok(Grant { values, emissions, compiled, stored })
    }

    fn complete(
        &mut self,
        values: &mut dyn Iterator<Item = ValueRef<'_>>,
        output: &mut dyn FnMut(Value),
    ) -> Result<(), DistError> {
        let mut grants = BTreeMap::new_in(self.alloc);
        for value in values {
            let key = CodeKey(ContentId(value.key));
            let raw = (value.kind, value.bytes);
            if grants.insert(key, raw).is_some_and(|previous| previous != raw) {
                return Err(DistError::Store(StoreError::TwoValues { key }));
            }
        }
        check_publication(
            self.krate,
            self.work,
            &[],
            grants.len(),
            |key| grants.get(&key).copied(),
            self.alloc,
            self.heap,
        )?;
        // The coordinator retains all grant packs. Only the source's static/TLS entries
        // and carried vtables remain to complete the unit.
        for row in &self.krate.side {
            output(Value {
                key: row.key.0.0,
                kind: super::KIND_CODE,
                bytes: super::copy_wire_bytes(row.bytes, self.wire),
            });
        }
        Ok(())
    }
}

pub(crate) enum ResultImage<'a> {
    UpToDate,
    Ready { image: artifact::Artifact<'a>, compiled: usize, cached: usize },
}

pub(crate) fn run<'a, D: Driver, B: Emit, M>(
    request: &Request<'_, '_>,
    reached: &Reached<'_, M>,
    work: &WorkSet<'_>,
    backend: &mut B,
    codes: &mut HomeCodeStore<'_, D>,
    dist: Option<&DistConfig>,
    alloc: Alloc<'a>,
) -> Result<(ResultImage<'a>, Option<Digest>), DistError>
where
    B::Error: fmt::Display,
{
    let wire = request.heap.shared();
    let plans = crates(request, reached, work, alloc, wire)?;
    let mut compiled = 0;
    let mut cached = 0;
    let mut fingerprint = None;
    if let Some(dist) = dist {
        let units = plans.iter().map(|plan| plan.unit.spec).collect_in(alloc);
        let fp = unit_digest_of(
            request.program,
            request.builder,
            true,
            &units,
            &[request.checked.0, request.product, request.native_image.0, work.digest.0],
            alloc,
        );
        if matches_output_fingerprint(request.destination, &fp, wire)? {
            return Ok((ResultImage::UpToDate, Some(fp)));
        }
        fingerprint = Some(fp);
        let cancelled = AtomicBool::new(false);
        let mut store = DistStore::new(codes, wire);
        for krate in &plans {
            let mut holder =
                Holder { krate, work, backend, store: &mut store, alloc, wire, heap: request.heap };
            let arrived = match agent::run(&dist.config, &krate.unit, &mut holder, &cancelled) {
                Ok(arrived) => arrived,
                Err(err) => match agent::decide(&dist.config.local, err, wire) {
                    Ok(()) => {
                        fingerprint = None;
                        break;
                    }
                    Err(fatal) => {
                        let mut shown = String::new_in(request.heap.alloc());
                        let _ = write!(shown, "{fatal}");
                        return Err(DistError::UnitFailed {
                            unit: own_str(krate.name, request.heap.alloc()),
                            shown,
                        });
                    }
                },
            };
            check_publication(
                krate,
                work,
                &krate.side,
                arrived.values.len(),
                |key| arrived.values.get(&key.0.0).map(|(kind, bytes)| (*kind, bytes.as_slice())),
                alloc,
                request.heap,
            )?;
            store.merge_arrived(&arrived)?;
            compiled += usize::try_from(arrived.compiled_here).map_err(|_| {
                refused(request.heap, format_args!("compiled count for {}", krate.name))
            })?;
            cached += usize::try_from(arrived.stored_here).map_err(|_| {
                refused(request.heap, format_args!("stored count for {}", krate.name))
            })?;
            append_report_row(
                dist.report.as_ref().map(|path| path.as_str()),
                krate.name,
                &krate.unit,
                &arrived,
                0,
                0,
                krate.members.len(),
                &[],
                wire,
            );
        }
    }
    if fingerprint.is_none() {
        for (_, task) in work.tasks() {
            let rows = task.compile(backend, codes, alloc).map_err(|why| {
                refused(request.heap, format_args!("native source emission: {why}"))
            })?;
            for row in rows {
                match row.source {
                    execute::Source::Emitted => compiled += 1,
                    execute::Source::Store => cached += 1,
                }
            }
        }
        for krate in &plans {
            side(codes, &krate.side).map_err(DistError::Store)?;
        }
    }
    let image = artifact::Artifact::read(reached, work, codes, alloc, request.heap)?;
    Ok((ResultImage::Ready { image, compiled, cached }, fingerprint))
}
