//! The current inputs to semantic read verification. The checked answer's storage root
//! also encodes its generation and cause history, which belong to the local home.

use core::fmt::Write as _;

use blaze_ids::{ContentId, Hasher};
use blaze_wire::json::hex;
use blaze_wire::{WireAlloc, WireString};

use crate::{engine_check::Standing, run_ids::RunIds, run_names::Program, sweep::{Leaf, LeafKind}};
use super::DistError;

const DOMAIN: &str = "blaze distributed dev verification inputs 1";
const EVIDENCE_DOMAIN: &str = "blaze distributed dev verification evidence 1";
const MOST_ROWS: usize = 4096;
const MOST_KEY_BYTES: usize = 256;
const MOST_VALUE_BYTES: usize = 256;

pub(crate) fn context(
    standing: &Standing<'_, '_>,
    program: &Program,
    ids: &RunIds<'_>,
    report_path: Option<&str>,
) -> Result<ContentId, DistError> {
    let context = digest(
        IdentityInputs { producers: standing.producers.digest(), toolchain: &standing.toolchain.0, tools: &standing.tools },
        |hash| program.hash_export_inputs(hash),
        |hash| ids.hash_export_inputs(hash),
        (0..standing.leaves.len()).map(|index| (standing.leaves.path(index), standing.leaves.row(index))),
    );
    write_evidence(report_path, standing.heap, |alloc| evidence(standing, program, ids, context, alloc))?;
    Ok(context)
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
/// The key's byte stream above is unchanged. Each section has a complete digest;
/// row detail has explicit count and byte bounds, including for large workspaces.
fn evidence(standing: &Standing<'_, '_>, program: &Program, ids: &RunIds<'_>, context: ContentId, alloc: WireAlloc) -> WireString {
    let mut report = Report::new(context, alloc);
    report.section("producers", |hash| { hash.id(&standing.producers.digest()); });
    report.section("toolchain", |hash| { hash.update(&standing.toolchain.0); });
    report.section("tools", |hash| {
        for tool in &standing.tools {
            hash.id(&tool.0);
        }
    });
    report.section("program", |hash| program.hash_export_inputs(hash));
    report.section("run_ids", |hash| ids.hash_export_inputs(hash));
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
    program.each_export_input(|kind, key, value| report.row(kind, key, value));
    ids.each_export_input(|kind, key, value| report.row(kind, key, value));
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
