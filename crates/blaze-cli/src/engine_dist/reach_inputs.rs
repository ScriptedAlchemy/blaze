//! The current inputs to semantic read verification. The checked answer's storage root
//! also encodes its generation and cause history, which belong to the local home.

use core::fmt::Write as _;

use blaze_drive::Name;
use blaze_engine::{CellId, last::LastAnswer};
use blaze_id::CrateItemKey;
use blaze_ids::{ContentId, Hasher};
use blaze_own::{AVec, Alloc};
use blaze_wire::json::hex;
use blaze_wire::{WireAlloc, WireString};

use crate::{engine_check::Standing, run_ids::RunIds, run_names::Program, sweep::{Leaf, LeafKind}};
use super::DistError;

const DOMAIN: &str = "blaze distributed dev verification inputs 2";
const SCOPE_DOMAIN: &str = "blaze distributed dev scope inputs 1";
const EVIDENCE_DOMAIN: &str = "blaze distributed dev verification evidence 1";
const MOST_ROWS: usize = 4096;
const MOST_KEY_BYTES: usize = 256;
const MOST_VALUE_BYTES: usize = 256;
const MOST_SCOPES: usize = 1 << 20;
const MOST_SCOPE_READS: usize = 1 << 20;

pub(crate) fn context(
    standing: &Standing<'_, '_>,
    program: &Program,
    ids: &RunIds<'_>,
    report_path: Option<&str>,
) -> Result<ContentId, DistError> {
    let inputs = Inputs::new(standing, program, ids)?;
    let workspace = |key| inputs.scope(key);
    let context = digest(
        IdentityInputs { producers: standing.producers.digest(), toolchain: &standing.toolchain.0, tools: &standing.tools },
        |hash| {
            program.hash_export_inputs(hash, &workspace);
            inputs.hash_modules(hash);
        },
        |hash| ids.hash_export_inputs(hash, &workspace),
        (0..standing.leaves.len()).map(|index| (standing.leaves.path(index), standing.leaves.row(index))),
    );
    write_evidence(report_path, standing.heap, |alloc| evidence(standing, program, ids, &inputs, context, alloc))?;
    Ok(context)
}

struct ScopeInput<'a> {
    key: CrateItemKey,
    source: ContentId,
    outputs: ContentId,
    reads: AVec<'a, (CellId, ContentId)>,
    digest: ContentId,
}

struct Inputs<'a> {
    scopes: AVec<'a, ScopeInput<'a>>,
    modules: AVec<'a, (Name, ContentId)>,
}

impl<'a> Inputs<'a> {
    fn new(standing: &Standing<'_, '_>, program: &Program, ids: &RunIds<'_>) -> Result<Inputs<'a>, DistError> {
        let alloc: Alloc<'a> = standing.heap.alloc();
        let mut keys = AVec::new_in(alloc);
        ids.each_workspace_scope(|key| keys.push(key));
        keys.sort_unstable();
        keys.dedup();
        let refuse = |name: Name, what: &str| DistError::Compile {
            shown: blaze_own::format_in!(standing.heap.alloc(), "cannot witness distributed verification inputs for {name}: {what}"),
        };
        if keys.len() > MOST_SCOPES {
            return Err(refuse(Name::unit_graph(), "more than the bounded workspace scope count"));
        }
        let rows = standing.rows();
        let mut scopes = AVec::with_capacity_in(keys.len(), alloc);
        for key in keys {
            let name = blaze_drive::key::crate_scope_name(&key);
            let row = rows.row_facts(name).map_err(|_| refuse(name, "the stored scope row was refused"))?
                .ok_or_else(|| refuse(name, "the checked scope has no row"))?;
            if row.failed {
                return Err(refuse(name, "the checked scope failed"));
            }
            let output_cell = CellId { of: name, cell: crate::engine_tasks::CONTEXT_OUTPUTS };
            let mut defines = rows.defines(name).ok_or_else(|| refuse(name, "the scope has no defined cells"))?;
            if !defines.any(|(id, _)| id == output_cell) {
                return Err(refuse(name, "the scope lacks its context output witness; rebuild it with the current compiler"));
            }
            let outputs = rows.cell(&output_cell);
            let observed = rows.observations(name).map_err(|_| refuse(name, "the stored scope reads were refused"))?
                .ok_or_else(|| refuse(name, "the checked scope has no read transcript"))?;
            if observed.len() > MOST_SCOPE_READS {
                return Err(refuse(name, "more than the bounded scope read count"));
            }
            let mut reads = AVec::with_capacity_in(observed.len(), alloc);
            reads.extend(observed);
            reads.sort_unstable();
            let digest = scope_digest(row.source, outputs, &reads);
            scopes.push(ScopeInput { key, source: row.source, outputs, reads, digest });
        }
        let mut names = AVec::new_in(alloc);
        program.each_macro_input(|name| names.push(name));
        names.sort_unstable();
        names.dedup();
        if names.len() > MOST_SCOPES {
            return Err(refuse(Name::unit_graph(), "more than the bounded proc-module count"));
        }
        let mut modules = AVec::with_capacity_in(names.len(), alloc);
        for name in names {
            let row = rows.row_facts(name).map_err(|_| refuse(name, "the proc-module row was refused"))?
                .ok_or_else(|| refuse(name, "the checked proc module has no row"))?;
            if row.failed {
                return Err(refuse(name, "the checked proc module failed"));
            }
            modules.push((name, row.out));
        }
        if rows.refusal().is_some() {
            return Err(refuse(Name::unit_graph(), "the stored answer refused a witness read"));
        }
        Ok(Inputs { scopes, modules })
    }

    fn scope(&self, key: CrateItemKey) -> Option<ContentId> {
        self.scopes.binary_search_by_key(&key, |scope| scope.key).ok()
            .and_then(|at| self.scopes.get(at)).map(|scope| scope.digest)
    }

    fn hash_modules(&self, hash: &mut Hasher) {
        hash.update(&u64::try_from(self.modules.len()).unwrap_or(u64::MAX).to_le_bytes());
        for (name, out) in &self.modules {
            hash.id(&name.subject).id(out);
        }
    }
}

/// A scope is valid by its semantic source and the current values of its read
/// transcript. `reads` is sorted by (cell, value). The opaque-output witness is
/// mandatory: proc outputs and macro-valued link attributes are not source alone.
/// Neither the local record key, generation nor derived whole-scope output enters.
pub(crate) fn scope_digest(source: ContentId, outputs: ContentId, reads: &[(CellId, ContentId)]) -> ContentId {
    let mut hash = Hasher::new(SCOPE_DOMAIN);
    hash.id(&source).id(&outputs).update(&u64::try_from(reads.len()).unwrap_or(u64::MAX).to_le_bytes());
    for (id, saw) in reads {
        hash.part(id.of.kind.name().as_bytes()).id(&id.of.subject).update(&id.cell.0).id(saw);
    }
    hash.finish()
}

struct IdentityInputs<'a> {
    producers: ContentId,
    toolchain: &'a [u8; 32],
    tools: &'a [blaze_drive::Tool],
}

fn digest<'a>(
    inputs: IdentityInputs<'_>,
    program: impl FnOnce(&mut Hasher),
    ids: impl FnOnce(&mut Hasher),
    leaves: impl Iterator<Item = (&'a str, Leaf)>,
) -> ContentId {
    let mut hash = Hasher::new(DOMAIN);
    hash.id(&inputs.producers).update(inputs.toolchain);
    for tool in inputs.tools {
        hash.id(&tool.0);
    }
    program(&mut hash);
    ids(&mut hash);
    // Leaves are validated on open in (kind, logical path) order. The sweep used
    // stat stamps to establish these contents locally; only their values travel.
    for (path, leaf) in leaves {
        leaf.hash_state(path, &mut hash);
    }
    hash.finish()
}

fn write_evidence(
    report_path: Option<&str>,
    heap: &'static blaze_own::Heap,
    render: impl FnOnce(WireAlloc) -> WireString,
) -> Result<(), DistError> {
    if let Some(path) = report_path {
        let alloc = heap.shared();
        let body = render(alloc);
        let mut file = WireString::from_str_in(path, alloc);
        file.push_str(".context");
        blaze_dist_local::sys::write_file(file.as_str(), body.as_bytes()).map_err(|error| DistError::Compile {
            shown: blaze_own::format_in!(heap.alloc(), "cannot write distributed verification inputs at {}: {error}", file.as_str()),
        })?;
    }
    Ok(())
}

/// A receipt is requested only through the existing distributed report option.
/// Each section has a complete digest;
/// row detail has explicit count and byte bounds, including for large workspaces.
fn evidence(standing: &Standing<'_, '_>, program: &Program, ids: &RunIds<'_>, inputs: &Inputs<'_>, context: ContentId, alloc: WireAlloc) -> WireString {
    let mut report = Report::new(context, alloc);
    let workspace = |key| inputs.scope(key);
    report.section("producers", |hash| { hash.id(&standing.producers.digest()); });
    report.section("toolchain", |hash| { hash.update(&standing.toolchain.0); });
    report.section("tools", |hash| {
        for tool in &standing.tools {
            hash.id(&tool.0);
        }
    });
    report.section("program", |hash| program.hash_export_inputs(hash, &workspace));
    report.section("run_ids", |hash| ids.hash_export_inputs(hash, &workspace));
    report.section("macro_modules", |hash| inputs.hash_modules(hash));
    report.section("leaves", |hash| {
        for index in 0..standing.leaves.len() {
            standing.leaves.row(index).hash_state(standing.leaves.path(index), hash);
        }
    });
    report.begin_rows();
    report.row("producers", &[], &standing.producers.digest().0);
    report.row("toolchain", &[], &standing.toolchain.0);
    for (index, tool) in standing.tools.iter().enumerate() {
        report.row("tool", &u64::try_from(index).unwrap_or(u64::MAX).to_le_bytes(), &tool.0.0);
    }
    // Keep source leaves ahead of potentially large body tables, so a detail cap
    // cannot hide a changed home listing or build-script observation behind them.
    for index in 0..standing.leaves.len() {
        let leaf = standing.leaves.row(index);
        let (flags, id) = leaf.state();
        let mut value = [0u8; 33];
        value[0] = flags;
        value[1..].copy_from_slice(&id.0);
        let kind = match leaf.kind {
            LeafKind::Slot => "leaf.slot",
            LeafKind::Manifest => "leaf.manifest",
            LeafKind::Listing => "leaf.listing",
            LeafKind::HomeListing => "leaf.home_listing",
            LeafKind::MacroFiles => "leaf.macro_files",
            LeafKind::MacroFile => "leaf.macro_file",
            LeafKind::MacroFolder => "leaf.macro_folder",
            LeafKind::ScriptFile => "leaf.script_file",
            LeafKind::ScriptListing => "leaf.script_listing",
            LeafKind::ScriptStat => "leaf.script_stat",
            LeafKind::ScriptGit => "leaf.script_git",
        };
        report.row(kind, standing.leaves.path(index).as_bytes(), &value);
    }
    for scope in &inputs.scopes {
        report.row("scope.source", &scope.key.0.0, &scope.source.0);
        report.row("scope.context_outputs", &scope.key.0.0, &scope.outputs.0);
        report.row("scope.inputs", &scope.key.0.0, &scope.digest.0);
        for (id, saw) in &scope.reads {
            let mut key = blaze_wire::WireVec::new_in(alloc);
            key.extend_from_slice(&scope.key.0.0);
            key.extend_from_slice(id.of.kind.name().as_bytes());
            key.push(0);
            key.extend_from_slice(&id.of.subject.0);
            key.extend_from_slice(&id.cell.0);
            report.row("scope.read", &key, &saw.0);
        }
    }
    for (name, out) in &inputs.modules {
        report.row("macro_module", &name.subject.0, &out.0);
    }
    program.each_export_input(&workspace, |kind, key, value| report.row(kind, key, value));
    ids.each_export_input(&workspace, |kind, key, value| report.row(kind, key, value));
    report.finish()
}

struct Report {
    body: WireString,
    sections: usize,
    rows: usize,
}

impl Report {
    fn new(context: ContentId, alloc: WireAlloc) -> Self {
        let mut body = WireString::new_in(alloc);
        let _ = write!(body, "{{\"schema\":1,\"context\":\"{}\",\"sections\":{{", hex(&context.0));
        Self { body, sections: 0, rows: 0 }
    }

    fn section(&mut self, name: &'static str, inputs: impl FnOnce(&mut Hasher)) {
        let mut hash = Hasher::new(EVIDENCE_DOMAIN);
        inputs(&mut hash);
        if self.sections != 0 {
            self.body.push(',');
        }
        self.sections += 1;
        let _ = write!(self.body, "\"{name}\":\"{}\"", hex(&hash.finish().0));
    }

    fn begin_rows(&mut self) {
        self.body.push_str("},\"rows\":[");
    }

    fn row(&mut self, kind: &'static str, key: &[u8], value: &[u8]) {
        self.rows += 1;
        if self.rows > MOST_ROWS {
            return;
        }
        if self.rows != 1 {
            self.body.push(',');
        }
        let key_part = key.first_chunk::<MOST_KEY_BYTES>().map_or(key, |prefix| prefix.as_slice());
        let value_part = value.first_chunk::<MOST_VALUE_BYTES>().map_or(value, |prefix| prefix.as_slice());
        let mut hash = Hasher::new(EVIDENCE_DOMAIN);
        hash.part(key).part(value);
        // Hex keeps paths and arbitrary input bytes unambiguous without JSON
        // escaping or retaining source text. Lengths name any clipped prefix.
        let _ = write!(self.body, concat!(
            "{{\"kind\":\"{}\",\"key_hex\":\"{}\",\"value_hex\":\"{}\",",
            "\"key_bytes\":{},\"value_bytes\":{},\"digest\":\"{}\"}}"
        ), kind, hex(key_part), hex(value_part), key.len(), value.len(), hex(&hash.finish().0));
    }

    fn finish(mut self) -> WireString {
        let _ = write!(self.body, concat!(
            "],\"rows_total\":{},\"rows_written\":{},\"rows_limit\":{},",
            "\"key_bytes_limit\":{},\"value_bytes_limit\":{}}}\n"
        ), self.rows, self.rows.min(MOST_ROWS), MOST_ROWS, MOST_KEY_BYTES, MOST_VALUE_BYTES);
        self.body
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sweep::{Leaf, LeafKind, Stamp};

    #[test]
    fn scope_witness_keeps_the_source_and_every_observed_semantic_input() {
        let source = ContentId([21; 32]);
        let outputs = ContentId([22; 32]);
        let mut reads = [
            (CellId { of: Name::new(blaze_drive::Kind::CrateScope, ContentId([23; 32])), cell: crate::engine_tasks::EXPORTS }, ContentId([24; 32])),
            (CellId { of: Name::new(blaze_drive::Kind::Script, ContentId([25; 32])), cell: crate::engine_tasks::WHOLE }, ContentId([26; 32])),
            (CellId { of: Name::new(blaze_drive::Kind::Given, ContentId([27; 32])), cell: crate::engine_tasks::WHOLE }, ContentId([28; 32])),
        ];
        reads.sort_unstable();
        let before = scope_digest(source, outputs, &reads);
        assert_ne!(before, scope_digest(ContentId([29; 32]), outputs, &reads), "the task's semantic source remains an input");
        assert_ne!(before, scope_digest(source, ContentId([30; 32]), &reads), "opaque results and macro-valued link attributes remain inputs");
        for (changed, _) in reads {
            let after = reads.map(|(id, saw)| (id, if id == changed { ContentId([31; 32]) } else { saw }));
            assert_ne!(before, scope_digest(source, outputs, &after), "each dependency export, script output and observed environment value remains an input");
        }
        let other_subject = reads.map(|(mut id, saw)| {
            id.of.subject = ContentId([32; 32]);
            (id, saw)
        });
        assert_ne!(before, scope_digest(source, outputs, &other_subject), "equal values from other tasks are different observations");
        let other_cell = reads.map(|(mut id, saw)| {
            id.cell = crate::engine_tasks::DATA;
            (id, saw)
        });
        assert_ne!(before, scope_digest(source, outputs, &other_cell), "the observed cell's identity is part of the transcript");
    }

    #[test]
    fn checked_proc_module_bytes_remain_inputs_beside_the_scope_witness() {
        let alloc = blaze_own::testing::bump().alloc();
        let mut inputs = Inputs { scopes: AVec::new_in(alloc), modules: AVec::new_in(alloc) };
        let digest = |inputs: &Inputs<'_>| {
            let mut hash = Hasher::new(DOMAIN);
            inputs.hash_modules(&mut hash);
            hash.finish()
        };
        let empty = digest(&inputs);
        let module = Name::new(blaze_drive::Kind::MacroModule, ContentId([33; 32]));
        inputs.modules.push((module, ContentId([34; 32])));
        let before = digest(&inputs);
        assert_ne!(before, empty, "a checked proc module cannot disappear from the context");
        inputs.modules.clear();
        inputs.modules.push((module, ContentId([35; 32])));
        assert_ne!(before, digest(&inputs), "different compiled wasm changes the context even with the same scope output");
    }

    #[test]
    fn leaf_identity_survives_a_different_checkout_but_tracks_its_source() {
        let digest = |path, leaf: &Leaf| {
            let mut hash = Hasher::new(DOMAIN);
            leaf.hash_state(path, &mut hash);
            hash.finish()
        };
        let mut leaf = Leaf { kind: LeafKind::Slot, path: 0, flags: 0, stamp: None, id: ContentId([1; 32]) };
        let before = digest("src/lib.rs", &leaf);
        leaf.path = 42;
        leaf.stamp = Some(Stamp { len: 1, mtime_ns: 2, ctime_ns: 3, ino: 4, dev: 5 });
        assert_eq!(digest("src/lib.rs", &leaf), before);
        assert_ne!(digest("other.rs", &leaf), before);
        leaf.id = ContentId([2; 32]);
        assert_ne!(digest("src/lib.rs", &leaf), before);
    }

    #[test]
    fn verification_receipts_preserve_the_original_context_byte_stream() {
        let producers = ContentId([1; 32]);
        let toolchain = [2; 32];
        let tools = [blaze_drive::Tool(ContentId([3; 32])), blaze_drive::Tool(ContentId([4; 32]))];
        let program = [5; 80];
        let crates = [6; 112];
        let bodies = [7; 192];
        let leaf = Leaf { kind: LeafKind::Slot, path: 0, flags: 0, stamp: None, id: ContentId([8; 32]) };
        let mut original = Hasher::new(DOMAIN);
        original.id(&producers).update(&toolchain);
        for tool in &tools {
            original.id(&tool.0);
        }
        original.update(&program).part(&crates).part(&bodies);
        leaf.hash_state("src/main.rs", &mut original);
        let context = digest(
            IdentityInputs { producers, toolchain: &toolchain, tools: &tools },
            |hash| { hash.update(&program); },
            |hash| { hash.part(&crates).part(&bodies); },
            core::iter::once(("src/main.rs", leaf)),
        );
        assert_eq!(context, original.finish());
        let alloc = blaze_own::testing::heap().shared();
        let mut report = Report::new(context, alloc);
        report.section("program", |hash| { hash.update(&program); });
        report.begin_rows();
        report.row("program.graph", &[], &program);
        let receipt = report.finish();
        let expected = blaze_own::format_in!(alloc, "\"context\":\"{}\"", hex(&original.finish().0));
        assert!(receipt.as_str().contains(expected.as_str()));
    }

    #[test]
    fn verification_evidence_is_opt_in_and_reports_write_errors() {
        let heap = blaze_own::testing::heap();
        write_evidence(None, heap, |_| panic!("no receipt was requested")).unwrap();
        let error = write_evidence(Some("invalid\0report"), heap, |alloc| WireString::from_str_in("{}", alloc)).unwrap_err();
        assert!(matches!(error, DistError::Compile { shown } if shown.as_str().contains("invalid\0report.context")));
    }

    #[test]
    fn verification_evidence_is_bounded_and_retains_clipped_input_digests() {
        let alloc = blaze_own::testing::heap().shared();
        let context = ContentId([7; 32]);
        let key = [b'"'; MOST_KEY_BYTES + 1];
        let mut value = [3; MOST_VALUE_BYTES + 1];
        let render = |bytes: &[u8]| {
            let mut report = Report::new(context, alloc);
            report.section("all", |hash| { hash.part(&key).part(bytes); });
            report.begin_rows();
            for _ in 0..MOST_ROWS + 1 {
                report.row("leaf", &key, bytes);
            }
            report.finish()
        };
        let first = render(&value);
        assert_eq!(first, render(&value));
        assert!(first.as_str().contains("\"rows_total\":4097,\"rows_written\":4096,\"rows_limit\":4096"));
        assert_eq!(first.as_str().matches("\"kind\":\"leaf\"").count(), MOST_ROWS);
        assert!(first.as_str().contains("\"key_bytes\":257,\"value_bytes\":257"));
        assert!(first.len() < 6 * 1024 * 1024);
        assert!(first.as_str().starts_with("{\"schema\":1,\"context\":\"0707070707070707070707070707070707070707070707070707070707070707\""));
        value[MOST_VALUE_BYTES] ^= 1;
        assert_ne!(first, render(&value), "a clipped byte still changes the complete evidence digest");
    }
}
