//! A completed export's cold heads survive the producing command. Each consumer opens a
//! new store snapshot and a new records-backed Maker, with an empty code store. Omitting
//! only publication makes the next command freeze again, while emitted bytes stay equal.

use core::cell::Cell;

use blaze_emit::{Isa, Os};
use blaze_engine::Stage;
use blaze_id::{CrateItemKey, ItemKey as LibKey};
use blaze_ids::producer::{CodeBuilder, ProducerId, Producers};
use blaze_ids::{ContentId, Hasher};
use blaze_ladder::body_ids::{BodyId, BodyIds, Read};
use blaze_ladder::code_records::{Builder, CodeKey, CodeStore, MemoryStore, Settings, TierName};
use blaze_ladder::front2::heads::Want;
use blaze_ladder::front2::keys::checker_of;
use blaze_ladder::front2::program::Front2Refusal;
use blaze_ladder::front2::program::lazy::{HeadCounts, LazyRefusal};
use blaze_ladder::front2::source::{Load, LoadedExpansion, Log, NamesSource, Records, SourceError, Which};
use blaze_ladder::front2::tables::head::{FreezeError, Known, KnownHeads, KnownSource, NotTheirs};
use blaze_ladder::run::make_one::{Maker, MakerOpts, NamesRecords, SecondCheck, walk_records};
use blaze_ladder::run::special::MakerStart;
use blaze_ladder::run::{OverflowChecks, Panic, RunError};
use blaze_own::{AVec, Alloc, Bump, Erased};
use blaze_scope::graph::Sysroot;
use blaze_scope::package_path::PackagePath;
use blaze_scope::resolve::{CrateInput, SourceTexts, aligned, resolve_crates};
use blaze_surface::Edition;
use blaze_typeck::ItemKey;
use blaze_unit_graph::{Implicit, Prelude, StdIs};
use bstd::io::{Io, Limits, SimWorld};
use bstd::mem::{Frame, TASK_FRAME, Tag};
use bstd::test::BUDGET;

use super::publish::{Batch, CommitOpts, FreshHead, Published, RootState, StageOpts};
use super::{CrateAt, DependencyFinder, Key, Tool, dependency_key};
use crate::engine_store::Ledger;
use crate::store::{CommandStore, Facts, Home, Store as _};

const CRATE: CrateItemKey = CrateItemKey(ContentId([84; 32]));
const CRATES: [CrateItemKey; 1] = [CRATE];
const TOOL: Tool = Tool(ContentId([85; 32]));
const ROOT: &str = "engine-dependency-heads.export-publish-test";
const LANG_CRATES: [usize; 1] = [0];
const READS: [Read; 1] = [Read { fact: [86; 32], digest: [87; 32] }];
const TEXT: &str = "#![feature(no_core, lang_items)]\n#![no_core]\n#![no_main]\n\
#[lang = \"sized\"] trait Sized {}\n#[lang = \"copy\"] trait Copy {}\n\
#[inline(never)]\nfn value() -> i32 { 7 }\n#[no_mangle]\npub extern \"C-unwind\" fn main() -> i32 { value() }\n";

struct Files;

impl<'a> SourceTexts<'a> for Files {
    fn read(&self, krate: usize, path: &str) -> Option<&'a str> {
        (krate == 0 && path == "/dep/lib.rs").then_some(TEXT)
    }
}

struct OneCrate<'a> {
    names: Option<&'a [u8]>,
    items: &'a [u8],
}

impl<'a> Load<'a> for OneCrate<'a> {
    fn names(&self, krate: usize) -> Option<&'a [u8]> {
        if krate == 0 { self.names } else { None }
    }

    fn items(&self, krate: usize) -> Option<&'a [u8]> {
        (krate == 0).then_some(self.items)
    }

    fn file(&self, krate: usize, path: PackagePath<'_>) -> Option<&'a str> {
        (krate == 0 && path.as_str() == "lib.rs").then_some(TEXT)
    }

    fn expansion(&self, _krate: usize, _call: LibKey) -> Option<LoadedExpansion<'a>> {
        None
    }
}

struct Fixture {
    names: AVec<'static, u8>,
    items: AVec<'static, u8>,
    names_at: Key,
}

fn fixture() -> Fixture {
    let g = blaze_own::testing::heap().alloc();
    let frame = Frame::new(Tag::Infer).expect("fixture frame");
    let memory = Erased::new(&frame);
    let inputs = [CrateInput {
        name: "dep", key: CRATE, edition: Edition::E2021,
        implicit: Implicit { std: StdIs::Rust, prelude: Prelude::Rust(edition::Edition::Edition2021), proc_macro: false, entry: false },
        root: "/dep/lib.rs", shown_from: "/dep", features: &[], deps: &[], cfg: &[], script: None,
    }];
    let solved = resolve_crates(&inputs, Sysroot::default(), &Files, &blaze_scope::task::Serial, memory.bump())
        .unwrap_or_else(|error| panic!("publication fixture resolves: {error}"));
    let [krate] = solved.crates() else { panic!("one fixture crate") };
    let names = blaze_scope::names_record::encode(krate, g).expect("names record");
    let items = blaze_scope::items_record::encode(krate, g).expect("items record");
    let names_at = Key(ContentId::of("export publication fixture names", &names));
    Fixture { names, items, names_at }
}

fn records<'n>(f: &Fixture, frame: &'n Frame, memory: Alloc<'n>, bump: Bump<'n>) -> NamesRecords<'n> {
    let load = &*frame.alloc(OneCrate { names: Some(aligned(&f.names, bump)), items: aligned(&f.items, bump) });
    // Both the self-borrowing reader and every buffer it retains belong to this frame.
    let log = frame.alloc(Log::new(&["dep"], memory));
    let records = &*frame.alloc(Records::new(&CRATES, load, log, frame, memory));
    NamesRecords { records, frame, lang_crates: &LANG_CRATES, body_sources: None }
}

fn named<'n>(records: &'n Records<'n>, name: &str) -> LibKey {
    let mut found = None;
    assert!(records.each_item(0, &mut |key, _| {
        if records.name_of(key) == Some(name) {
            assert!(found.replace(key).is_none(), "one item named {name}");
        }
    }));
    found.unwrap_or_else(|| panic!("fixture item {name}"))
}

fn closed(names_at: Key) -> CrateAt {
    CrateAt { prefix: CRATE.prefix(), key: CRATE, names_at: Some(names_at), member: false, closed: true, toolchain: false }
}

fn producer() -> ProducerId<CodeBuilder> {
    Producers::for_tests(b"export publication fixture").get::<CodeBuilder>()
}

fn builder() -> Builder {
    Builder {
        isa: Isa { os: Os::MacOs, lse: true }, tier: TierName::Reference,
        settings: Settings::baseline(OverflowChecks::Off, Panic::Unwind),
        toolchain: blaze_toolchain::PINNED, producer: producer(),
        edition: edition::PackageEdition::of_manifest("2021").expect("2021 edition"),
    }
}

/// Fixed source in both commands; only head publication changes. The real Maker still
/// checks and emits every body, and a fresh MemoryStore prevents code reuse from hiding it.
struct FixedBodies;

impl BodyIds for FixedBodies {
    fn body_check(&self, item: ItemKey) -> Option<BodyId<'_>> {
        Some(BodyId { key: Hasher::new("export publication fixed body").part(&item.0.to_le_bytes()[..]).finish(), reads: &READS })
    }

    fn today(&self, fact: &[u8; 32]) -> Option<[u8; 32]> {
        (*fact == READS[0].fact).then_some(READS[0].digest)
    }

    fn crate_fact(&self, _item: ItemKey) -> Option<Read> {
        Some(Read { fact: [88; 32], digest: [89; 32] })
    }
}

#[derive(Debug, PartialEq, Eq)]
struct Emitted {
    item: ItemKey,
    key: CodeKey,
    bytes: AVec<'static, u8>,
}

#[derive(Clone, Copy)]
enum Publication {
    Keep,
    Omit,
}

struct Outcome {
    code: AVec<'static, Emitted>,
    heads: HeadCounts,
    frozen_keys: AVec<'static, Key>,
    rooted_before: AVec<'static, Key>,
    published: Option<Published>,
}

fn command(f: &Fixture, io: &Io, publication: Publication) -> Outcome {
    let heap = blaze_own::testing::heap();
    let g = heap.alloc();
    let cx = io.cx(BUDGET, TASK_FRAME).expect("command context");
    let frame = cx.frame();
    let memory = Erased::new(frame);
    let home = Home::of(&cx, &io.need_fs().expect("filesystem"), Some("/h")).expect("home");
    let clock = io.need_clock().expect("clock");
    let err = io.need_err().expect("stderr");
    let lent = home.open_store(&blaze_cas::plain::Settings::default(), Facts { boot: Some(1), nonce: 91 }, g).expect("open store");
    let mut store = CommandStore::new(&cx, &lent, false, 0, None, clock, &err, memory.handle()).expect("command store");
    let prior = RootState::read(&store, ROOT).expect("valid prior root");
    let mut rooted_before = blaze_own::Vec::new_in(g);
    if let Some(bytes) = store.named(ROOT).expect("read prior root") {
        for chunk in bytes.chunks_exact(32) {
            let mut key = [0u8; 32];
            key.copy_from_slice(chunk);
            rooted_before.push(Key(ContentId(key)));
        }
    }
    let snapshot = store.snapshot().expect("store snapshot").expect("CommandStore lends a snapshot");
    let mut crates = blaze_own::Vec::new_in(heap.shared());
    crates.push(closed(f.names_at));
    let provider = DependencyFinder::new(&*snapshot, crates, TOOL);
    let names = records(f, frame, memory.handle(), memory.bump());
    let main = named(names.records, "main");
    let value = named(names.records, "value");
    let entry = checker_of(main);
    let b = builder();
    let mut maker: Maker<'_> = Maker::of_start(MakerOpts {
        names, entry: Some(entry), start: MakerStart::ByRule, typing: SecondCheck,
        builder: &b, known_heads: Some(KnownSource { heads: &provider, errors: heap.shared() }), alloc: g,
    });
    let mut code_store = MemoryStore::new_in(g);
    assert!(code_store.is_empty());
    let (walk, counts) = walk_records(entry, &FixedBodies, &mut code_store, &b, &producer(), &mut maker, g)
        .expect("the real two-function export walk");
    assert_eq!(counts.found, 0, "each command must emit, not reuse code entries");
    assert!(counts.made >= 2, "both main and its called function must be emitted");
    let mut code = blaze_own::Vec::new_in(g);
    for row in 0..walk.len() {
        let key = walk.key(row).expect("every fixed-source instance is keyed");
        let bytes = code_store.get(key).expect("read emitted content").expect("emitted entry exists");
        let mut owned = blaze_own::Vec::with_capacity_in(bytes.len(), g);
        owned.extend_from_slice(bytes);
        code.push(Emitted { item: walk.tabled(row), key, bytes: owned });
    }
    code.sort_unstable_by_key(|row| row.item);
    assert!(code.iter().any(|row| row.item == checker_of(value)), "the called function is reached");
    assert!(names.records.errors().is_empty());
    let heads = maker.head_counts();
    let completed = maker.finish_heads().expect("a valid completed export lends its source freezes");
    let mut frozen_keys = blaze_own::Vec::new_in(g);
    for row in completed.heads() {
        assert_eq!(row.krate, Some(CRATE), "the original source supplies the full closed crate identity");
        assert!(!row.cell.key().is_inner());
        frozen_keys.push(dependency_key(f.names_at, row.cell, TOOL));
    }
    frozen_keys.sort_unstable();
    if heads.taken == 0 {
        for cell in [Want::Fn(main), Want::Fn(value)] {
            assert!(frozen_keys.contains(&dependency_key(f.names_at, cell, TOOL)), "a cold completion includes each emitted function's real head");
        }
    }
    let published = match publication {
        Publication::Omit => None,
        Publication::Keep => {
            let batch = Batch::stage(StageOpts {
                heads: completed.heads().map(|row| FreshHead { cell: row.cell, krate: row.krate, bytes: row.bytes }),
                crates: provider.crates(), names: &|krate| krate.names_at, tool: provider.tool(), alloc: g,
            }).expect("stage completed eligible heads");
            let mut ledger = Ledger::new();
            Some(batch.commit(CommitOpts { cx: &cx, store: &mut store, root: ROOT, prior, added: &[], stage: Stage::Bodies, ledger: &mut ledger })
                .expect("publish and retain the completed heads"))
        }
    };
    lent.commit().expect("commit the command before closing its home");
    // No reader, provider, snapshot, Maker, freezer, code store, or open CAS escapes.
    Outcome { code, heads, frozen_keys, rooted_before, published }
}

#[test]
fn completed_export_heads_survive_reopen_and_omitting_publication_freezes_again() {
    let f = fixture();
    for publication in [Publication::Omit, Publication::Keep] {
        let io = Io::simulated(SimWorld::new(91, Limits::USUAL));
        let first = command(&f, &io, publication);
        assert!(first.rooted_before.is_empty());
        assert_eq!(first.heads.taken, 0);
        assert!(first.heads.attempts > 0 && first.heads.lowered > 0);
        assert!(!first.frozen_keys.is_empty());
        let second = command(&f, &io, Publication::Omit);
        assert_eq!(second.code, first.code, "reopened saved heads and fresh source produce identical content keys and entry bytes");
        match publication {
            Publication::Omit => {
                assert!(first.published.is_none());
                assert!(second.rooted_before.is_empty());
                assert_eq!(second.heads, first.heads, "without publication the fresh process must freeze again");
                assert_eq!(second.frozen_keys, first.frozen_keys);
            }
            Publication::Keep => {
                let published = first.published.expect("publication ran");
                let frozen = u32::try_from(first.frozen_keys.len()).expect("small fixture count");
                assert_eq!((published.frozen, published.put, published.retained), (frozen, frozen, frozen));
                assert_eq!((published.duplicates, published.listed, published.unlisted), (0, 0, 0));
                assert_eq!((published.not_put.local, published.not_put.member, published.not_put.unlisted, published.not_put.no_names), (0, 0, 0, 0));
                assert_eq!(second.rooted_before, first.frozen_keys, "the reopened canonical root retains every real frozen record");
                assert!(second.heads.taken > 0, "the new Maker actually imports saved head cells");
                assert_eq!((second.heads.attempts, second.heads.lowered, second.heads.eval_lowered), (0, 0, 0), "all required heads survive without source freezes");
                assert!(second.frozen_keys.is_empty(), "imported heads are not handed back as fresh publication work");
            }
        }
    }
}

struct RefuseValue {
    cell: Cell<Option<Want>>,
    why: NotTheirs,
}

impl KnownHeads for RefuseValue {
    fn known(&self, cell: Want) -> Known<'_> {
        if Some(cell) == self.cell.get() { Known::Refused(self.why) } else { Known::Freeze }
    }
}

#[test]
fn failed_export_cannot_finish_even_its_earlier_successful_source_heads() {
    let f = fixture();
    let why = NotTheirs::NoCell { said: "this callee's authoritative head is refused" };
    let provider = RefuseValue { cell: Cell::new(None), why };
    let frame = Frame::new(Tag::Infer).expect("failed export frame");
    let memory = Erased::new(&frame);
    let names = records(&f, &frame, memory.handle(), memory.bump());
    let entry = checker_of(named(names.records, "main"));
    let cell = Want::Fn(named(names.records, "value"));
    provider.cell.set(Some(cell));
    let heap = blaze_own::testing::heap();
    let b = builder();
    let mut maker: Maker<'_> = Maker::of_start(MakerOpts {
        names, entry: Some(entry), start: MakerStart::ByRule, typing: SecondCheck,
        builder: &b, known_heads: Some(KnownSource { heads: &provider, errors: heap.shared() }), alloc: heap.alloc(),
    });
    let mut code = MemoryStore::new_in(heap.alloc());
    assert!(walk_records(entry, &FixedBodies, &mut code, &b, &producer(), &mut maker, heap.alloc()).is_err());
    assert!(maker.head_counts().attempts > 0 && maker.head_counts().lowered > 0, "a valid earlier head was lowered before the callee refusal");
    assert!(names.records.errors().is_empty(), "the cause is the known provider, not current source");
    let error = match maker.finish_heads() {
        Ok(_) => panic!("a failed session must not expose publishable source heads"),
        Err(error) => error,
    };
    let RunError::Front2(Front2Refusal::Lazy(LazyRefusal::KnownHead { failure, .. })) = error else {
        panic!("completion must preserve the typed known-head failure: {error}")
    };
    assert_eq!(failure.want, cell);
    assert!(matches!(failure.error(), FreezeError::NotTheirs { want, why: actual } if *want == cell && *actual == why));
}

#[test]
fn empty_library_completion_checks_the_actual_source_log_without_an_item() {
    let f = fixture();
    for present in [true, false] {
        let frame = Frame::new(Tag::Infer).expect("empty export frame");
        let memory = Erased::new(&frame);
        let bump = memory.bump();
        let load = &*frame.alloc(OneCrate {
            names: present.then(|| aligned(&f.names, bump)),
            items: aligned(&f.items, bump),
        });
        let log = frame.alloc(Log::new(&["dep"], memory.handle()));
        let records = &*frame.alloc(Records::new(&CRATES, load, log, &frame, memory.handle()));
        let names = NamesRecords { records, frame: &frame, lang_crates: &LANG_CRATES, body_sources: None };
        let b = builder();
        let maker: Maker<'_> = Maker::of_start(MakerOpts {
            names, entry: None, start: MakerStart::ByRule, typing: SecondCheck,
            builder: &b, known_heads: None, alloc: blaze_own::testing::heap().alloc(),
        });
        // A library export can ask a names question without ever asking this Maker for a
        // callable. Omitting its record must invalidate completion even with no item to name.
        let attrs = records.root_attrs(0);
        assert!(maker.program().is_none());
        assert_eq!(maker.checks().constructed, 0);
        assert_eq!(maker.head_counts(), HeadCounts::default());
        if present {
            assert!(attrs.expect("the current names record answers root attributes").no_core);
            assert!(records.errors().is_empty());
            assert_eq!(maker.finish_heads().expect("a valid untouched Maker completes").heads().count(), 0);
        } else {
            assert!(attrs.is_none());
            {
                let errors = records.errors();
                assert_eq!(errors.len(), 1);
                assert!(matches!(errors.first(), Some(SourceError::NoRecord { krate: 0, name: "dep", which: Which::Names })));
            }
            let error = match maker.finish_heads() {
                Ok(_) => panic!("an empty Maker must retain the source refusal"),
                Err(error) => error,
            };
            let RunError::Front2(Front2Refusal::Lazy(LazyRefusal::RecordsVoid { tabled, first, kept, dropped })) = error else {
                panic!("empty completion must preserve the actual source-log failure: {error}")
            };
            assert_eq!(tabled, None, "no callable was requested, so no item identity may be invented");
            assert_eq!(first.as_ref(), "no names record for crate `dep` (crate 0)");
            assert_eq!((kept, dropped), (1, 0));
        }
    }
}
