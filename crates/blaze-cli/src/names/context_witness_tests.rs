//! A retained names scope and a fresh solve can carry different synthetic body expansions
//! after a literal edit that leaves name resolution's input unchanged. The distributed
//! context uses that input witness while retaining exact outputs of opaque proc macros.
//! These fixtures run the resolver and the driver's real scope-record writer.

use core::num::NonZeroU32;
use core::sync::atomic::{AtomicUsize, Ordering};
use core::time::Duration;
use std::string::String;
use std::sync::Mutex;
use std::vec::Vec;

use blaze_drive::{Name, Seen};
use blaze_engine::CellId;
use blaze_expand::{HostAsk, HostError, HostWhy, MacroDefinition, ProcCall, ProcKind, Produced, ReadAnswer, ReadKind, Reads, TokenOrigin, Transparency, lex};
use blaze_id::{CrateItemKey, ItemKey};
use blaze_ids::ContentId;
use blaze_ids::producer::{Expander, ProducerId, Producers};
use blaze_own::{AVec, Alloc, Bump, Erased};
use blaze_scope::crate_scope::{CrateScope, Kept, Ready, Solved, resolve_crate, resolve_kept};
use blaze_scope::graph::Sysroot;
use blaze_scope::items_record::ItemsRecord;
use blaze_scope::prepares::Prepares;
use blaze_scope::resolve::{CrateInput, NamesNote, SourceTexts, aligned};
use blaze_surface::{Edition, ItemKind};
use blaze_unit_graph::{Implicit, Prelude, StdIs};
use bstd::cx::Cx;
use bstd::io::{Io, Limits, SimWorld};
use bstd::mem::{Frame, TASK_FRAME, Tag};
use bstd::test::BUDGET;

use super::metered::{CallScratch, Metered};
use super::{ScopeRecord, scope_record};

const ROOT: &str = "/fixture/lib.rs";
const DEP_ROOT: &str = "/dependency/lib.rs";
const OWNER: CrateItemKey = CrateItemKey(ContentId([71; 32]));
const DEPENDENCY: CrateItemKey = CrateItemKey(ContentId([72; 32]));
const RUNNER: CrateItemKey = CrateItemKey(ContentId([73; 32]));
const RUST: Implicit = Implicit { std: StdIs::Rust, prelude: Prelude::Rust(edition::Edition::Edition2021), proc_macro: false, entry: false };

// The pinned format! shape: its expansion writes a block containing a dependency's
// expression macro. The block has a names scope even though it declares no item.
const FORMAT: &str = "#![no_core]
#[macro_export]
macro_rules! format { ($($a:tt)*) => {{ let res = $crate::__export::inner!($($a)*); res }}; }
#[macro_export]
macro_rules! __inner { ($($a:tt)*) => { 0 }; }
pub mod __export { pub use crate::__inner as inner; }
";

const BEFORE: &str = "#![no_core]
use std::format;
pub fn value() { format!(\"before\"); }
";
const AFTER: &str = "#![no_core]
use std::format;
pub fn value() { format!(\"after_\"); }
";
const DECLARATION_EDIT: &str = "#![no_core]
use std::format;
pub fn value() { format!(\"after_\"); }
fn private_added() {}
";

const MACROS: &str = "#![no_core]
#[proc_macro]
pub fn make(input: TokenStream) -> TokenStream { input }
";
const CALLER: &str = "#![no_core]
macros::make!();
";
const GENERATED: &str = "pub const C: u8 = 1; pub static S: u8 = 2; pub fn signature() -> u8 { 0 }";

#[derive(Clone, Copy)]
struct ProcAnswer {
    text: &'static str,
    origins: Transparency,
    runner: CrateItemKey,
    caller_data: Option<ContentId>,
}

struct Fixture {
    source: &'static str,
    answer: Option<ProcAnswer>,
}

impl<'a> SourceTexts<'a> for Fixture {
    fn read(&self, krate: usize, path: &str) -> Option<&'a str> {
        match (krate, path) {
            (0, ROOT) => Some(self.source),
            (1, DEP_ROOT) => Some(if self.answer.is_some() { MACROS } else { FORMAT }),
            _ => None,
        }
    }

    fn proc_macro_key(&self, krate: usize) -> Option<CrateItemKey> {
        self.answer.filter(|_| krate == 1).map(|answer| answer.runner)
    }

    fn run_proc_macro<'b>(&self, krate: usize, def: &MacroDefinition<'_>, call: ProcCall<'_>, alloc: Alloc<'b>) -> Result<Produced<'b>, HostError> {
        let refused = HostError { ask: HostAsk::ProcMacro, why: HostWhy::Refused };
        let Some(answer) = self.answer else { return Err(refused) };
        if krate != 0 || call.kind != ProcKind::FnLike || def.proc_name() != Some("make") {
            return Err(refused);
        }
        let tokens = lex(answer.text, Edition::E2021, alloc).map_err(|_| refused)?;
        let mut origins = AVec::new_in(alloc);
        origins.extend(tokens.tokens().iter().map(|_| TokenOrigin::Made(answer.origins)));
        let mut reads = Reads::new_in(alloc);
        if let Some(digest) = answer.caller_data {
            reads.push(ReadKind::ProcMacro, "file:caller-input.txt", ReadAnswer::Digest(digest)).map_err(|_| refused)?;
        }
        Ok(Produced { tokens, origins, reads, diagnostics: AVec::new_in(alloc), text: AVec::new_in(alloc) })
    }
}

struct Snapshot {
    surface: ContentId,
    exports: Vec<u8>,
    inner: Vec<u8>,
    record: ScopeRecord,
    blocks: usize,
    opaque: Vec<(ItemKey, ContentId)>,
    declarations: Vec<(String, ItemKind)>,
}

fn snapshot(fixture: Fixture) -> Snapshot {
    let io = Io::simulated(SimWorld::new(71, Limits::USUAL));
    let cx = io.cx(BUDGET, TASK_FRAME).expect("the scope-record test context");
    let home = Frame::new(Tag::Nameres).expect("the resolver frame");
    let erased = Erased::new(&home);
    let bump = erased.bump();
    let metered = Metered::new(
        &home,
        Frame::new(Tag::Expand).expect("the expansion scratch"),
        Frame::new(Tag::Nameres).expect("the solver scratch"),
        CallScratch::Reset,
    );
    let proc_macro = fixture.answer.is_some();
    let dep_name = if proc_macro { "macros" } else { "std" };
    let dependency = CrateInput {
        name: dep_name, key: DEPENDENCY, edition: Edition::E2021,
        implicit: Implicit { proc_macro, ..RUST }, root: DEP_ROOT, shown_from: "/",
        features: &[], deps: &[], cfg: &[], script: None,
    };
    let prepares = Prepares::new_in(bump);
    let dep = resolve_crate(&dependency, &[], Sysroot::default(), &fixture, 1, &metered, prepares, bump)
        .unwrap_or_else(|error| panic!("the fixture dependency resolves: {error:?}"));
    let deps = [aligned(&dep.record, bump)];
    let called = [(dep_name, 0)];
    let owner = CrateInput {
        name: "fixture", key: OWNER, edition: Edition::E2021, implicit: RUST,
        root: ROOT, shown_from: "/", features: &[], deps: &called, cfg: &[], script: None,
    };
    let done = resolve_crate(&owner, &deps, Sysroot::default(), &fixture, 0, &metered, prepares, bump)
        .unwrap_or_else(|error| panic!("the fixture resolves: {error:?}"));
    capture(&fixture, &done, &cx, &metered, bump)
}

fn capture<'a>(fixture: &Fixture, done: &CrateScope<'a>, cx: &Cx, metered: &Metered<'_>, bump: Bump<'a>) -> Snapshot {
    assert!(!done.names.notes.iter().any(|note| matches!(note, NamesNote::Expand { .. })), "the fixture's calls expand: {:?}", done.names.notes);
    let surface = blaze_file_items::outline(fixture.source, Edition::E2021, bump.alloc()).surface();
    let blocks = done.names.made.values().filter(|module| module.info.name.is_some_and(|name| name.starts_with("__blaze_block_"))).count();
    let opaque = done.names.opaque_outputs.iter().map(|row| (row.call, row.digest)).collect();
    let record = scope_record(
        cx, NonZeroU32::MIN, done, "fixture", true, true,
        &blaze_scope::waits_on::WaitsOn::of(done.names, &[], bump.alloc()),
        metered, blaze_own::testing::heap().shared(),
    ).unwrap_or_else(|error| panic!("the driver writes the fixture scope: {error}"));
    let parts = super::sections::sections(&record.payload).expect("the written scope sections");
    assert_eq!(parts.context_outputs().expect("the persisted opaque-output witness"), record.context_outputs);
    assert_eq!(
        super::sections::cell_of(&record.payload, crate::engine_tasks::CONTEXT_OUTPUTS).expect("the full-width witness cell"),
        Seen::Is(record.context_outputs),
        "a restored scope serves the exact witness that the resolver wrote",
    );
    let items = ItemsRecord::open(aligned(parts.items, bump), bump.alloc()).expect("the written item table");
    let declarations = (0..items.items()).filter_map(|index| {
        let row = items.identity_at(index).expect("an item identity").expect("an existing item");
        row.name.map(|name| (String::from(name), row.kind))
    }).collect();
    Snapshot { surface, exports: Vec::from(&done.record[..]), inner: Vec::from(&done.inner[..]), record, blocks, opaque, declarations }
}

fn witness(snapshot: &Snapshot, outputs: ContentId) -> ContentId {
    let file = Name::file(ROOT.as_bytes());
    let reads = [(CellId { of: file, cell: crate::engine_tasks::SURFACE }, snapshot.surface)];
    let source = ContentId::of(super::SOURCE_DOMAIN, &OWNER.0.0);
    crate::engine_dist::reach_inputs::scope_digest(source, outputs, &reads)
}

#[test]
fn a_retained_format_block_and_a_fresh_scope_share_the_context_witness() {
    let retained = snapshot(Fixture { source: BEFORE, answer: None });
    let fresh = snapshot(Fixture { source: AFTER, answer: None });
    assert_eq!(retained.surface, fresh.surface, "a literal edit does not wake the names task");
    assert_eq!(retained.exports, fresh.exports, "the exported names are unchanged");
    assert_eq!(retained.inner, fresh.inner, "the private and synthetic names are unchanged");
    assert!(retained.blocks > 0, "the real resolver made a synthetic expansion block");
    assert_eq!(retained.blocks, fresh.blocks);
    assert!(retained.opaque.is_empty() && fresh.opaque.is_empty(), "a declarative expression macro is reconstructed from its inputs");
    assert_ne!(retained.record.expansions_out, fresh.record.expansions_out, "retained and freshly rendered expression bodies differ");
    assert_eq!(witness(&retained, retained.record.context_outputs), witness(&fresh, fresh.record.context_outputs), "the current Surface transcript permits the retained scope and the fresh solve");
}

#[test]
fn the_context_oracle_catches_retained_synthetic_expansion_history() {
    let retained = snapshot(Fixture { source: BEFORE, answer: None });
    let fresh = snapshot(Fixture { source: AFTER, answer: None });
    // Plant the old key's history-sensitive constituent into the same production hasher.
    // Removing that cause is what makes the preceding retained/fresh equality hold.
    assert_ne!(witness(&retained, retained.record.expansions_out), witness(&fresh, fresh.record.expansions_out));
    let changed = snapshot(Fixture { source: DECLARATION_EDIT, answer: None });
    assert_ne!(fresh.surface, changed.surface, "a private declaration is a names input");
    assert_ne!(fresh.inner, changed.inner, "the private name is present in the resolved output");
    assert_ne!(witness(&fresh, fresh.record.context_outputs), witness(&changed, changed.record.context_outputs), "normalization must retain a real names edit");
}

#[test]
fn opaque_macro_values_signatures_origins_modules_and_reads_remain_in_the_context() {
    let base = ProcAnswer { text: GENERATED, origins: Transparency::Transparent, runner: RUNNER, caller_data: Some(ContentId([74; 32])) };
    let before = snapshot(Fixture { source: CALLER, answer: Some(base) });
    assert_eq!(before.opaque.len(), 1, "the actual proc call contributes one opaque output");
    for (name, kind) in [("C", ItemKind::Const), ("S", ItemKind::Static), ("signature", ItemKind::Function)] {
        assert!(before.declarations.iter().any(|(found, actual)| found == name && *actual == kind), "the proc output declares {name} as {kind:?}");
    }
    for (cause, answer) in [
        ("const value", ProcAnswer { text: "pub const C: u8 = 3; pub static S: u8 = 2; pub fn signature() -> u8 { 0 }", ..base }),
        ("static value", ProcAnswer { text: "pub const C: u8 = 1; pub static S: u8 = 4; pub fn signature() -> u8 { 0 }", ..base }),
        ("signature", ProcAnswer { text: "pub const C: u8 = 1; pub static S: u8 = 2; pub fn signature() -> u16 { 0 }", ..base }),
        ("origins", ProcAnswer { origins: Transparency::SemiOpaque, ..base }),
        ("module identity", ProcAnswer { runner: CrateItemKey(ContentId([76; 32])), ..base }),
        ("caller data read", ProcAnswer { caller_data: Some(ContentId([75; 32])), ..base }),
    ] {
        let after = snapshot(Fixture { source: CALLER, answer: Some(answer) });
        assert_eq!(before.surface, after.surface, "{cause}: the written source is fixed");
        assert_ne!(before.opaque, after.opaque, "{cause}: the real proc expansion transcript changes");
        assert_ne!(before.record.context_outputs, after.record.context_outputs, "{cause}: the driver persists the opaque result");
        assert_ne!(witness(&before, before.record.context_outputs), witness(&after, after.record.context_outputs), "{cause}: the distributed scope context changes");
        assert_eq!(witness(&before, ContentId::default()), witness(&after, ContentId::default()), "{cause}: omitting opaque outputs is the planted unsound control");
    }
}

struct CountedFixture {
    fixture: Fixture,
    runs: AtomicUsize,
}

impl<'a> SourceTexts<'a> for CountedFixture {
    fn read(&self, krate: usize, path: &str) -> Option<&'a str> {
        self.fixture.read(krate, path)
    }

    fn proc_macro_key(&self, krate: usize) -> Option<CrateItemKey> {
        self.fixture.proc_macro_key(krate)
    }

    fn run_proc_macro<'b>(&self, krate: usize, def: &MacroDefinition<'_>, call: ProcCall<'_>, alloc: Alloc<'b>) -> Result<Produced<'b>, HostError> {
        self.runs.fetch_add(1, Ordering::Relaxed);
        self.fixture.run_proc_macro(krate, def, call, alloc)
    }
}

/// Whole scopes always resolve. Only the expansion sets cross between the two solves.
struct ExpansionSets<'a> {
    prior: &'a [Option<&'a [u8]>; 2],
    made: Mutex<[Vec<u8>; 2]>,
}

impl Kept for ExpansionSets<'_> {
    fn find(&self, _krate: usize) -> Option<(&[u8], &[u8], &[u8])> {
        None
    }

    fn made(&self, _krate: usize, _record: &[u8], _inner: &[u8], _read: &[u8]) {}

    fn keeps_expansions(&self) -> Option<ProducerId<Expander>> {
        Some(Producers::for_tests(b"context witness").get::<Expander>())
    }

    fn expansions(&self, krate: usize) -> Option<&[u8]> {
        self.prior.get(krate).copied().flatten()
    }

    fn expansions_made(&self, krate: usize, bytes: &[u8]) {
        let mut made = self.made.lock().expect("the fixture's expansion sets");
        let slot = made.get_mut(krate).expect("one of the fixture's two crates");
        *slot = Vec::from(bytes);
    }
}

fn kept_snapshot(answer: ProcAnswer, prior: &[Option<&[u8]>; 2]) -> (Snapshot, [Vec<u8>; 2], usize) {
    let io = Io::simulated(SimWorld::new(71, Limits::USUAL));
    let cx = io.cx(BUDGET, TASK_FRAME).expect("the kept scope-record test context");
    let home = Frame::new(Tag::Nameres).expect("the kept resolver frame");
    let erased = Erased::new(&home);
    let bump = erased.bump();
    let metered = Metered::new(
        &home,
        Frame::new(Tag::Expand).expect("the kept expansion scratch"),
        Frame::new(Tag::Nameres).expect("the kept solver scratch"),
        CallScratch::Reset,
    );
    let crates = [
        CrateInput {
            name: "fixture", key: OWNER, edition: Edition::E2021, implicit: RUST,
            root: ROOT, shown_from: "/", features: &[], deps: &[("macros", 1)], cfg: &[], script: None,
        },
        CrateInput {
            name: "macros", key: DEPENDENCY, edition: Edition::E2021,
            implicit: Implicit { proc_macro: true, ..RUST }, root: DEP_ROOT, shown_from: "/",
            features: &[], deps: &[], cfg: &[], script: None,
        },
    ];
    let sources = CountedFixture { fixture: Fixture { source: CALLER, answer: Some(answer) }, runs: AtomicUsize::new(0) };
    let sets = ExpansionSets { prior, made: Mutex::new([Vec::new(), Vec::new()]) };
    let (solved, _) = resolve_kept(&crates, Sysroot::default(), &sources, &metered, &Ready::serial(Duration::from_secs(30)), &sets, bump)
        .unwrap_or_else(|error| panic!("the fixture resolves through its kept expansion sets: {error:?}"));
    let Some(Solved::Solved(done)) = solved.first() else {
        panic!("the fixture's whole scope must resolve in both passes");
    };
    let snapshot = capture(&sources.fixture, done, &cx, &metered, bump);
    (snapshot, sets.made.into_inner().expect("the completed fixture expansion sets"), sources.runs.load(Ordering::Relaxed))
}

#[test]
fn a_kept_proc_expansion_preserves_the_full_scope_witness() {
    // No outside read: the expansion library can ask every input again and reuse it.
    // The separate caller-data control above covers opaque reads that it refuses to keep.
    let answer = ProcAnswer { text: GENERATED, origins: Transparency::SemiOpaque, runner: RUNNER, caller_data: None };
    let (fresh, sets, fresh_runs) = kept_snapshot(answer, &[None, None]);
    assert_eq!(fresh_runs, 1, "the fresh solve executes the proc macro");
    assert_eq!(fresh.opaque.len(), 1);
    assert!(sets.iter().all(|set| !set.is_empty()), "both crates persist their expansion sets");
    let kept = Frame::new(Tag::Expand).expect("the persisted expansion sets' frame");
    let erased = Erased::new(&kept);
    let bump = erased.bump();
    let prior = [Some(aligned(&sets[0], bump)), Some(aligned(&sets[1], bump))];
    let (restored, _, restored_runs) = kept_snapshot(answer, &prior);
    assert_eq!(restored_runs, 0, "the second solve must take the kept expansion path");
    assert_eq!(fresh.opaque, restored.opaque, "the kept path captures the same complete proc result");
    assert_eq!(fresh.record.payload, restored.record.payload, "fresh and restored scopes write equal bytes");
    assert_eq!(witness(&fresh, fresh.record.context_outputs), witness(&restored, restored.record.context_outputs));

    let (changed, _, changed_runs) = kept_snapshot(ProcAnswer { runner: CrateItemKey(ContentId([77; 32])), ..answer }, &prior);
    assert_eq!(changed_runs, 1, "a new proc module cannot take the old module's expansion");
    assert_ne!(fresh.opaque, changed.opaque);
    assert_ne!(witness(&fresh, fresh.record.context_outputs), witness(&changed, changed.record.context_outputs));
}
