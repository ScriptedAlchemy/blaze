//! The export provider reads the same closed dependency record as checking. These controls
//! use a real resolver and frozen head; the injected store changes only availability or the
//! record returned under its key, so a bad record can never be confused with a source miss.

use core::sync::atomic::{AtomicUsize, Ordering};

use blaze_id::{CrateItemKey, ItemKey};
use blaze_ladder::front2::receiver::Pointers;
use blaze_ladder::front2::source::{Load, LoadedExpansion, Log, NamesSource, Records};
use blaze_ladder::front2::tables::head::{Frozen, freeze};
use blaze_own::{AVec, Erased, Vec};
use blaze_scope::graph::Sysroot;
use blaze_scope::package_path::PackagePath;
use blaze_scope::resolve::{CrateInput, SourceTexts, aligned, resolve_crates};
use blaze_surface::Edition;
use blaze_unit_graph::{Implicit, Prelude, StdIs};
use bstd::mem::{Frame, Tag};

use super::{ContentId, CrateAt, DependencyFinder, HeadRecords, Key, Known, KnownError, KnownHeads, Tool, Unreadable, Want, dependency_key};

const CRATE: CrateItemKey = CrateItemKey(ContentId([49; 32]));
const NAMES: Key = Key(ContentId([50; 32]));
const TOOL: Tool = Tool(ContentId([51; 32]));
const TEXT: &str = "#![no_core]\npub fn sample() {}\n";

struct Files;

impl<'a> SourceTexts<'a> for Files {
    fn read(&self, krate: usize, path: &str) -> Option<&'a str> {
        (krate == 0 && path == "/dep/lib.rs").then_some(TEXT)
    }
}

struct OneCrate<'a> {
    names: &'a [u8],
    items: &'a [u8],
}

impl<'a> Load<'a> for OneCrate<'a> {
    fn names(&self, krate: usize) -> Option<&'a [u8]> {
        (krate == 0).then_some(self.names)
    }

    fn items(&self, krate: usize) -> Option<&'a [u8]> {
        (krate == 0).then_some(self.items)
    }

    fn file(&self, krate: usize, path: PackagePath<'_>) -> Option<&'a str> {
        (krate == 0 && path.as_str() == "lib.rs").then_some(TEXT)
    }

    fn expansion(&self, _krate: usize, _call: ItemKey) -> Option<LoadedExpansion<'a>> {
        None
    }
}

struct Fixture {
    cell: Want,
    frozen: Frozen<'static>,
}

fn fixture() -> Fixture {
    let g = blaze_own::testing::heap().alloc();
    let frame = Frame::new(Tag::Infer).expect("the head fixture frame");
    let erased = Erased::new(&frame);
    let bump = erased.bump();
    let inputs = [CrateInput {
        name: "dep", key: CRATE, edition: Edition::E2021,
        implicit: Implicit { std: StdIs::Rust, prelude: Prelude::Rust(edition::Edition::Edition2021), proc_macro: false, entry: false },
        root: "/dep/lib.rs", shown_from: "/dep", features: &[], deps: &[], cfg: &[], script: None,
    }];
    let solved = resolve_crates(&inputs, Sysroot::default(), &Files, &blaze_scope::task::Serial, bump)
        .unwrap_or_else(|error| panic!("the head fixture resolves: {error}"));
    let [krate] = solved.crates() else { panic!("the fixture has one crate") };
    let names = blaze_scope::names_record::encode(krate, g).expect("the names record");
    let items = blaze_scope::items_record::encode(krate, g).expect("the items record");
    let load = OneCrate { names: aligned(&names, bump), items: aligned(&items, bump) };
    let keys = [CRATE];
    // Records borrows itself through its NamesSource implementation. Keep the reader and
    // its buffers in the explicit frame so that borrow is independent of stack drop order.
    let log = frame.alloc(Log::new(&["dep"], erased.handle()));
    let records = &*frame.alloc(Records::new(&keys, &load, log, &frame, erased.handle()));
    let mut function = None;
    records.each_item(0, &mut |key, _| {
        if records.name_of(key) == Some("sample") {
            function = Some(key);
        }
    });
    let cell = Want::Fn(function.expect("the sample function"));
    let frozen = freeze(records, None, Pointers::NOT_LOOKED_UP, cell, &frame, g, g)
        .unwrap_or_else(|error| panic!("the fixture head freezes: {error}"));
    Fixture { cell, frozen }
}

struct Probe<'a> {
    key: Key,
    answer: Result<Option<&'a [u8]>, Unreadable>,
    reads: AtomicUsize,
}

impl HeadRecords for Probe<'_> {
    fn record(&self, key: Key) -> Result<Option<&[u8]>, Unreadable> {
        self.reads.fetch_add(1, Ordering::Relaxed);
        if key == self.key { self.answer } else { Ok(None) }
    }
}

fn closed(names_at: Option<Key>) -> CrateAt {
    CrateAt { prefix: CRATE.prefix(), key: CRATE, names_at, member: false, closed: true, toolchain: false }
}

fn provider<'a>(records: Probe<'a>, krate: CrateAt, tool: Tool) -> DependencyFinder<Probe<'a>> {
    let mut crates = Vec::new_in(blaze_own::testing::heap().shared());
    crates.push(krate);
    DependencyFinder::new(records, crates, tool)
}

fn record(names_at: Key, bytes: &[u8]) -> AVec<'static, u8> {
    let mut out = AVec::new_in(blaze_own::testing::heap().alloc());
    out.extend_from_slice(&names_at.0.0);
    out.extend_from_slice(bytes);
    out
}

#[test]
fn export_reuses_the_checked_head_in_place_only_under_its_names_and_tool() {
    let Fixture { cell, frozen } = fixture();
    let bytes = record(NAMES, &frozen.bytes);
    let key = dependency_key(NAMES, cell, TOOL);
    for _ in 0..2 {
        let provider = provider(Probe { key, answer: Ok(Some(&bytes)), reads: AtomicUsize::new(0) }, closed(Some(NAMES)), TOOL);
        let Known::Bytes(found) = provider.known(cell) else { panic!("the checked head is present") };
        assert_eq!(found, frozen.bytes.as_slice());
        assert_eq!(found.as_ptr(), bytes.get(super::KEY_BYTES..).expect("the prefix was written").as_ptr(), "the head is borrowed in place");
        assert_eq!(provider.records.reads.load(Ordering::Relaxed), 1);
    }
    for (names, tool) in [(Key(ContentId([52; 32])), TOOL), (NAMES, Tool(ContentId([53; 32])))] {
        let provider = provider(Probe { key, answer: Ok(Some(&bytes)), reads: AtomicUsize::new(0) }, closed(Some(names)), tool);
        assert!(matches!(provider.known(cell), Known::Freeze), "a changed names RECORD key or Signature tool selects another record");
        assert_eq!(provider.records.reads.load(Ordering::Relaxed), 1);
    }
}

#[test]
fn export_open_unknown_and_missing_heads_keep_the_existing_source_path() {
    let Fixture { cell, .. } = fixture();
    let key = dependency_key(NAMES, cell, TOOL);
    let mut open = closed(Some(NAMES));
    open.closed = false;
    for (krate, want) in [
        (open, cell),
        (closed(None), cell),
        (closed(Some(NAMES)), Want::Fn(ItemKey(ContentId([99; 32])))),
    ] {
        let provider = provider(Probe { key, answer: Err(Unreadable), reads: AtomicUsize::new(0) }, krate, TOOL);
        assert!(matches!(provider.known(want), Known::Freeze));
        assert_eq!(provider.records.reads.load(Ordering::Relaxed), 0, "an ineligible crate never asks the snapshot");
    }
    let missing = provider(Probe { key, answer: Ok(None), reads: AtomicUsize::new(0) }, closed(Some(NAMES)), TOOL);
    assert!(matches!(missing.known(cell), Known::Freeze));
    assert_eq!(missing.records.reads.load(Ordering::Relaxed), 1);
}

#[test]
fn export_unreadable_and_mismatched_present_heads_are_typed_refusals() {
    let Fixture { cell, frozen } = fixture();
    let key = dependency_key(NAMES, cell, TOOL);
    let unreadable = provider(Probe { key, answer: Err(Unreadable), reads: AtomicUsize::new(0) }, closed(Some(NAMES)), TOOL);
    assert!(matches!(unreadable.known(cell), Known::Invalid(KnownError::Unreadable { key: got }) if got == key));

    let wrong_names = record(Key(ContentId([54; 32])), &frozen.bytes);
    let malformed = record(NAMES, b"not a frozen head");
    let other_cell = Want::Head(cell.key());
    let correct = record(NAMES, &frozen.bytes);
    for (want, bytes, reason) in [
        (cell, wrong_names.as_slice(), "names key"),
        (cell, &b"short"[..], "names key"),
        (cell, malformed.as_slice(), "does not read as a frozen head"),
        (other_cell, correct.as_slice(), "another cell"),
    ] {
        let key = dependency_key(NAMES, want, TOOL);
        let provider = provider(Probe { key, answer: Ok(Some(bytes)), reads: AtomicUsize::new(0) }, closed(Some(NAMES)), TOOL);
        match provider.known(want) {
            Known::Invalid(KnownError::Invalid { key: got, why }) => {
                assert_eq!(got, key);
                assert!(why.contains(reason), "{why}");
            }
            other => panic!("a bad present record must refuse, got {other:?}"),
        }
    }
}
