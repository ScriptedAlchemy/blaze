//! Distributed portable wasm bodies. Only granted templates/thunks produce emission receipts.
//! Wasmtime's native objects stay in the proc-macro runtime's sealed local store.
use core::fmt;
use core::sync::atomic::AtomicBool;
use blaze_dist_local::agent::{self, BackendKind, Emission, Grant, Granted, Unit};
use blaze_dist_local::pack::{Value, ValueRef};
use blaze_own::{AVec, Alloc};
use blaze_proc_macro::host::Preparation;
use blaze_proc_macro::module::ModuleRecord;
use blaze_proc_macro::wasm_fold::{self, Plan, RecordView, Work};
use blaze_wire::{WireAlloc, WireString, WireVec};
use super::{DistConfig, append_report_row};

const KIND_WASM_BODY: u8 = 2;
const IDENTITY: &[u8] = b"blaze portable wasm preparation 1";

pub(crate) struct Prepare {
    config: agent::Config,
    report: Option<WireString>,
    alloc: WireAlloc,
}

impl Prepare {
    pub(crate) fn new(dist: &DistConfig) -> Self {
        Self {
            config: dist.config.clone(),
            report: dist.report.as_ref().map(|path| WireString::from_str_in(path.as_str(), dist.wire_alloc)),
            alloc: dist.wire_alloc,
        }
    }
}

#[derive(Debug)]
enum Error {
    Fold(wasm_fold::Error),
    GrantIndex,
    RecordKind,
}
impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Fold(error) => write!(f, "{error}"),
            Self::GrantIndex => f.write_str("wasm grant names a recipe outside the unit"),
            Self::RecordKind => f.write_str("wasm unit returned a non-wasm body record"),
        }
    }
}
impl core::error::Error for Error {}
impl From<wasm_fold::Error> for Error { fn from(error: wasm_fold::Error) -> Self { Self::Fold(error) } }

struct Executor<'p, 'a> {
    plan: &'p Plan<'a>,
    work: &'p [Work],
    alloc: WireAlloc,
    scratch: Alloc<'a>,
    assembled: Option<AVec<'a, u8>>,
}

impl Granted for Executor<'_, '_> {
    type Error = Error;

    fn compile(&mut self, names: &[usize]) -> Result<Grant, Error> {
        let mut values = WireVec::new_in(self.alloc);
        let mut emissions = WireVec::new_in(self.alloc);
        for &name in names {
            let work = self.work.get(name).ok_or(Error::GrantIndex)?;
            let record = self.plan.emit(work)?;
            values.push(Value { key: record.key, kind: KIND_WASM_BODY, bytes: WireVec::from_slice_in(&record.bytes, self.alloc) });
            emissions.push(Emission { key: record.key, backend: Some(BackendKind::FoldedOutput) });
        }
        Ok(Grant { compiled: u64::try_from(values.len()).map_err(|_| Error::GrantIndex)?, stored: 0, values, emissions })
    }

    fn complete(&mut self, values: &mut dyn Iterator<Item = ValueRef<'_>>, _output: &mut dyn FnMut(Value)) -> Result<(), Error> {
        let mut records = AVec::new_in(self.scratch);
        for value in values {
            if value.kind != KIND_WASM_BODY { return Err(Error::RecordKind); }
            records.push(RecordView { key: value.key, bytes: value.bytes });
        }
        records.sort_unstable_by_key(|record| record.key);
        // A missing/failed holder is never filled by an ungranted tail. The manifest can
        // publish only after every original thunk and shared template validates together.
        self.assembled = Some(self.plan.assemble_records(&records)?);
        Ok(())
    }
}

impl Preparation for Prepare {
    fn identity(&self) -> &[u8] { IDENTITY }

    fn prepare<'a>(&self, module: &ModuleRecord, wasm: &'a [u8], alloc: Alloc<'a>) -> Result<AVec<'a, u8>, std::string::String> {
        let plan = Plan::new(wasm, alloc).map_err(|error| error.to_string())?;
        let work = plan.work().map_err(|error| error.to_string())?;
        if work.is_empty() { return plan.assemble_records(&[]).map_err(|error| error.to_string()); }
        let mut spec = blaze_sha256::Sha256::new();
        spec.update(IDENTITY);
        spec.update(&blaze_sha256::digest(wasm));
        let unit = Unit { spec: spec.finish(), names: WireVec::from_iter_in(work.iter().map(Work::key), self.alloc) };
        let mut executor = Executor { plan: &plan, work: &work, alloc: self.alloc, scratch: alloc, assembled: None };
        let cancelled = AtomicBool::new(false);
        let arrived = agent::run(&self.config, &unit, &mut executor, &cancelled).map_err(|error| error.to_string())?;
        let assembled = match executor.assembled.take() {
            Some(bytes) => bytes,
            None => {
                let mut records = AVec::new_in(alloc);
                for (key, (kind, bytes)) in &arrived.values {
                    if *kind != KIND_WASM_BODY { return Err(Error::RecordKind.to_string()); }
                    records.push(RecordView { key: *key, bytes });
                }
                plan.assemble_records(&records).map_err(|error| error.to_string())?
            }
        };
        let unit_name = blaze_own::format_in!(self.alloc, "wasm:{}", module.name);
        append_report_row(self.report.as_ref().map(|path| path.as_str()), unit_name.as_str(), &unit, &arrived, 0, 0, work.len(), &[], self.alloc);
        Ok(assembled)
    }
}
