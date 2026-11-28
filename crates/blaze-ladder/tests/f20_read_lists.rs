//! F20: a found code entry is taken only when its read list holds today. Every question the
//! live build asks goes through the one recorder (run/make_one.rs `Asking` over code_reads.rs
//! `Recorded`), the entry stores that list, and the walk asks it again of today's program
//! before taking the entry (run/make_one.rs `verify_instance`), on the real path: the lazy
//! program over NAMES RECORDS (`Maker`, `walk_records`), as tests/d106_records.rs walks it.
//!
//! THE STAND-IN ([`TextIds`]): each fn's and static's BodyCheck key is a digest of its OWN text,
//! every crate fact constant. So an edit of a dependency (a struct's field type, a constant's
//! value, a static's kind, a callee's ABI) moves no key of the fn that reads it: only its read
//! list can see the edit. That is the class each case tests.
//!
//! THE ORACLE: after the edit, the walk over the store kept from before is compared with a walk
//! over an EMPTY store of the edited text: every instance's entry must be under the same
//! content key (equal bytes: equal code, equal read list). A stale entry taken is a key that
//! differs. Each case also says the reader was made again (`stale_reads`, its row made).
//!
//! THE CONTROL (rule 23): the recorder planted to record no layout question
//! (`ReadPlant::SkipsLayouts`): the field-type edit is then missed, the stale entry is found
//! holding, and the oracle catches its key. TWINS: an edit nothing reads remakes nothing.

use std::collections::HashMap;

use blaze_emit::{Isa, Os};
use blaze_id::{CrateItemKey, ItemKey as LibKey};
use blaze_ids::producer::{CodeBuilder, ProducerId, Producers};
use blaze_ids::{ContentId, Hasher};
use blaze_ladder::body_ids::{BodyId, BodyIds, Counts, How, Make, Read, Stepped, WalkObserver};
use blaze_ladder::code_records::reads::{ReadKind, ReadPlant, Verdict};
use blaze_ladder::code_records::{Builder, CodeKey, CodeStore, EntryView, MemoryStore, Settings, TierName};
use blaze_ladder::crates::{Inputs, Toolchain, UserCrate};
use blaze_ladder::front2::keys::checker_of;
use blaze_ladder::front2::source::{Load, LoadedExpansion, Log, NamesSource, Records};
use blaze_ladder::run::OverflowChecks;
use blaze_ladder::run::make_one::{Maker, NamesRecords, walk_records};
use blaze_own::Alloc;
use blaze_scope::resolve::aligned;
use blaze_scope::{items_record, names_record};
use blaze_surface::ItemKind;
use blaze_toolchain::ToolchainId;
use blaze_typeck::ItemKey;

const DIR: &str = "/ladder/";
const ROOT: &str = "/ladder/src/main.rs";
/// The crates that can declare a lang item: the program's own, the only one.
const LANG_CRATES: [usize; 1] = [0];

/// The no_core head of every program (tests/d106_records.rs's: `main` is "C-unwind", so the
/// program needs no `panic_cannot_unwind`).
const HEAD: &str = "#![feature(no_core, lang_items)]\n#![no_core]\n#![no_main]\n\n#[lang = \"sized\"]\ntrait Sized {}\n\n#[lang = \"copy\"]\ntrait Copy {}\n\n";

fn builder() -> Builder {
    Builder {
        isa: Isa { lse: true, os: Os::MacOs },
        tier: TierName::Reference,
        settings: Settings::baseline(OverflowChecks::Off, blaze_build::Panic::Unwind),
        toolchain: ToolchainId([7; 32]),
        producer: producer(),
        edition: edition::PackageEdition::of_manifest("2024").expect("2024 is an edition"),
    }
}

fn producer() -> ProducerId<CodeBuilder> {
    Producers::for_tests(b"f20 read lists").get::<CodeBuilder>()
}

fn alloc() -> Alloc<'static> {
    blaze_own::testing::heap().alloc()
}

fn leak(s: &str) -> &'static str {
    Box::leak(s.to_string().into_boxed_str())
}

/// The records and texts as kept after the solve (tests/d106_records.rs `Kept`).
#[derive(Clone, Default)]
struct Kept {
    names: Vec<&'static [u8]>,
    items: Vec<&'static [u8]>,
    files: Vec<HashMap<String, &'static str>>,
    expansions: Vec<HashMap<LibKey, &'static str>>,
    origins: Vec<HashMap<LibKey, &'static [u8]>>,
}

impl Load<'static> for Kept {
    fn names(&self, krate: usize) -> Option<&'static [u8]> {
        self.names.get(krate).copied()
    }

    fn items(&self, krate: usize) -> Option<&'static [u8]> {
        self.items.get(krate).copied()
    }

    fn file(&self, krate: usize, path: blaze_scope::package_path::PackagePath<'_>) -> Option<&'static str> {
        self.files.get(krate)?.get(path.as_str()).copied()
    }

    fn expansion(&self, krate: usize, call: LibKey) -> Option<LoadedExpansion<'static>> {
        let text = self.expansions.get(krate)?.get(&call).copied()?;
        let origins = self.origins.get(krate).and_then(|by_call| by_call.get(&call)).copied().map(|bytes| {
            if bytes.is_empty() {
                (&[][..], &[][..])
            } else {
                blaze_scope::expansion_origins::split(bytes).unwrap_or_else(|e| panic!("the fixture's origins of {call:?}: {e}"))
            }
        });
        Some(LoadedExpansion { text, origins })
    }
}

fn made_origins(call: LibKey, origins: &Result<&[u8], blaze_scope::expansion_origins::OriginsError>) -> &'static [u8] {
    use blaze_scope::expansion_origins::OriginsError;
    match origins {
        Ok(bytes) => Box::leak(bytes.to_vec().into_boxed_slice()),
        Err(OriginsError::Rewritten { .. } | OriginsError::Unplaced { .. }) => &[],
        Err(e) => panic!("the origins of expansion {call:x}: {e:?}"),
    }
}

/// One program's names records: the solve's output, the solve itself gone.
struct Fixture {
    kept: Kept,
    keys: &'static [CrateItemKey],
    crate_names: &'static [&'static str],
}

/// `HEAD` + `body` solved once, its two records written from the solve.
fn fixture(body: &str) -> Fixture {
    let a = alloc();
    let files = [(String::from(ROOT), format!("{HEAD}{body}"))];
    let edition = edition::PackageEdition::of_manifest("2024").expect("2024 is an edition");
    let user = UserCrate { name: "ladder", dir: DIR, root: ROOT, files: &files, edition, panic: blaze_build::Panic::Unwind };
    let tc = Toolchain::none(a);
    let inputs = Inputs::new(&user, &tc).unwrap_or_else(|e| panic!("the crates: {e}"));
    let crates = inputs.crates();
    let keys: &'static [CrateItemKey] = Vec::leak(crates.iter().map(|c| c.key).collect());
    let crate_names: &'static [&'static str] = Vec::leak(crates.iter().map(|c| leak(c.name)).collect());
    let names = inputs.resolve(&crates, blaze_own::testing::bump()).unwrap_or_else(|e| panic!("the fixture's one solve: {e}"));
    let mut kept = Kept::default();
    for c in names.crates().iter() {
        kept.names.push(aligned(&names_record::encode(c, a).unwrap_or_else(|e| panic!("the names record of `{}`: {e:?}", c.name)), blaze_own::testing::bump()));
        kept.items.push(aligned(&items_record::encode_with(c, &names, a).unwrap_or_else(|e| panic!("the items record of `{}`: {e:?}", c.name)), blaze_own::testing::bump()));
        kept.files.push(c.files.iter().map(|f| (f.package.as_str().to_string(), leak(f.text))).collect());
        kept.expansions.push(c.expansions.iter().filter_map(|(call, e)| e.made.map(|m| (*call, leak(m.text)))).collect());
        kept.origins.push(c.expansions.iter().filter_map(|(call, e)| e.made.as_ref().map(|m| (*call, made_origins(*call, &m.origins)))).collect());
    }
    Fixture { kept, keys, crate_names }
}

/// A FRESH reader of `f`'s records, as one `blaze run` process makes it.
fn reader(f: &Fixture) -> NamesRecords<'static> {
    let a = alloc();
    let frame: &'static blaze_mem::Frame = Box::leak(Box::new(blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the reader's frame")));
    let log: &'static Log<'static> = Box::leak(Box::new(Log::new(f.crate_names, a)));
    let load: &'static Kept = Box::leak(Box::new(f.kept.clone()));
    let records: &'static Records<'static> = Box::leak(Box::new(Records::new(f.keys, load, log, frame, a)));
    NamesRecords { records, frame, lang_crates: &LANG_CRATES, body_sources: None }
}

fn logged(records: &Records<'_>) -> Vec<String> {
    records.errors().iter().map(|e| e.to_string()).collect()
}

mod member_index_preparation {
    use super::*;
    use blaze_ladder::front2::keys::{MemberKind, member_key_of};
    use blaze_ladder::front2::program::Front2Refusal;
    use blaze_ladder::front2::program::lazy::{LazyProgram, LazyRefusal};
    use blaze_ladder::front2::source::{MemberWant, SourceError};
    use blaze_ladder::run::RunError;
    use blaze_ladder::seam::FrontEnd;
    use core::cell::Cell;

    const MEMBERS: &str = "
struct Selected;
struct Unrelated;
macro_rules! selected_member { () => { pub fn selected() {} }; }
macro_rules! selected_impl { ($t:ident) => { impl $t { selected_member!(); } }; }
selected_impl!(Selected);
macro_rules! unrelated_member { () => { pub fn unrelated() {} }; }
impl Unrelated { unrelated_member!(); }
#[unsafe(no_mangle)]
pub extern \"C-unwind\" fn main() -> i32 { 0 }
";

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    struct Metadata<'n> {
        owner: LibKey,
        key: LibKey,
        kind: ItemKind,
        name: Option<&'n str>,
        written_by: Option<LibKey>,
    }

    fn metadata<'n>(records: &'n Records<'n>, want: MemberWant) -> blaze_own::AVec<'n, Metadata<'n>> {
        let mut containers = blaze_own::Vec::new_in(alloc());
        assert!(records.each_item(0, &mut |key, _| {
            if matches!(records.kind_of_item(key), Some(ItemKind::Trait | ItemKind::Impl)) {
                containers.push(key);
            }
        }));
        let mut rows = blaze_own::Vec::new_in(alloc());
        for owner in containers {
            records.each_member_matching(owner, &mut |_, _| want, &mut |m| {
                rows.push(Metadata { owner, key: m.key, kind: m.kind, name: m.name, written_by: m.written_by });
            });
        }
        rows.sort_unstable_by_key(|row| (row.owner, row.key));
        rows
    }

    fn targets(f: &Fixture) -> [Metadata<'static>; 2] {
        let rows = metadata(reader(f).records, MemberWant::NameOnly);
        ["selected", "unrelated"].map(|name| {
            let mut found = rows.iter().filter(|row| row.kind == ItemKind::Function && row.name == Some(name));
            let row = *found.next().expect("the named member is in the fixture");
            assert!(found.next().is_none(), "one fixture member has this name");
            row
        })
    }

    #[derive(Clone, Copy, Debug)]
    enum Plant {
        None,
        Missing(LibKey),
        Changed(LibKey),
    }

    struct MemberLoad<'a> {
        kept: &'a Kept,
        calls: [LibKey; 2],
        reads: [Cell<usize>; 2],
        plant: Plant,
    }

    impl<'a> MemberLoad<'a> {
        fn new(kept: &'a Kept, calls: [LibKey; 2], plant: Plant) -> Self {
            Self { kept, calls, reads: core::array::from_fn(|_| Cell::new(0)), plant }
        }

        fn counts(&self) -> [usize; 2] {
            self.reads.each_ref().map(Cell::get)
        }
    }

    impl<'n> Load<'n> for MemberLoad<'_> {
        fn names(&self, krate: usize) -> Option<&'n [u8]> { self.kept.names(krate) }
        fn items(&self, krate: usize) -> Option<&'n [u8]> { self.kept.items(krate) }
        fn file(&self, krate: usize, path: blaze_scope::package_path::PackagePath<'_>) -> Option<&'n str> { self.kept.file(krate, path) }
        fn expansion(&self, krate: usize, call: LibKey) -> Option<LoadedExpansion<'n>> {
            for (watched, count) in self.calls.iter().zip(&self.reads) {
                if *watched == call {
                    count.set(count.get() + 1);
                }
            }
            if matches!(self.plant, Plant::Missing(key) if key == call) {
                return None;
            }
            let mut read = self.kept.expansion(krate, call)?;
            if matches!(self.plant, Plant::Changed(key) if key == call) {
                read.text = "pub fn changed_expansion() {}";
            }
            Some(read)
        }
    }

    /// NameOnly keeps the member index's inputs; the real selected head opens its own text.
    /// WithText restores the unrelated read while yielding the same metadata.
    #[test]
    fn member_metadata_keeps_index_inputs_without_opening_member_texts() {
        let f = fixture(MEMBERS);
        let selected = targets(&f);
        let calls = selected.map(|row| row.written_by.expect("each member was written by its own macro call"));
        assert_ne!(calls[0], calls[1], "distinct expansion records make text reads observable");
        let expected = metadata(reader(&f).records, MemberWant::WithText);
        for want in [MemberWant::NameOnly, MemberWant::WithText] {
            let load = MemberLoad::new(&f.kept, calls, Plant::None);
            let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the reader's frame");
            // These owners and every retained cache use the same frame: no skipped heap
            // destructor when the arena releases Log/Records together (as macro_written_names).
            let memory = blaze_own::Erased::new(&frame);
            let records_alloc = memory.handle();
            let log = frame.alloc(Log::new(f.crate_names, records_alloc));
            let records = &*frame.alloc(Records::new(f.keys, &load, log, &frame, records_alloc));
            let names = NamesRecords { records, frame: &frame, lang_crates: &LANG_CRATES, body_sources: None };
            let before = names.records.opened();
            assert_eq!(metadata(names.records, want), expected, "the same kind/name/container and expansion provenance");
            let opened = names.records.opened();
            let reads = usize::from(want == MemberWant::WithText);
            assert_eq!(load.counts(), [reads, reads]);
            if want == MemberWant::NameOnly {
                assert_eq!(opened.items.texts, before.items.texts);
                assert_eq!(opened.items.text_bytes, before.items.text_bytes);
            } else {
                assert!(opened.items.texts >= before.items.texts + 2);
                assert!(opened.items.text_bytes > before.items.text_bytes);
            }
            assert!(names.records.errors().is_empty());

            let key = member_key_of(checker_of(selected[0].owner), MemberKind::Fn, "selected");
            let mut program = LazyProgram::from_records(names.records, names.frame, names.lang_crates, None, alloc()).expect("the lazy program");
            assert_eq!(load.counts(), [reads, reads], "constructing the program does not open either member's expansion");
            let row = program.head_row(key).expect("the selected head loads").expect("the selected function has a row");
            assert_eq!(program.function(row).key, key);
            let head_reads = [1, reads];
            assert_eq!(load.counts(), head_reads, "the requested head opens only its own member expansion");
            let text = program.function(row).text;
            let marks = blaze_ladder::front2::item_site::head_marks(names.records, selected[0].owner, "selected", ItemKind::Function, text)
                .expect("the selected member's marks").expect("the fixture's nested member expansion has an origin column");
            let saved = f.kept.expansion(0, calls[0]).expect("the selected expansion is retained");
            assert_eq!(marks.text, saved.text);
            assert!(blaze_scope::expansion_origins::offset_in(marks.text, text).is_some());
            assert!(matches!(blaze_ladder::front2::item_site::member_marks(names.records, selected[0].owner, None, text),
                Err(blaze_ladder::front2::item_site::MarksLost::NotInText { .. })), "the container's expansion is not the selected member's origin");
            assert_eq!(program.head_row(key).expect("the retained head"), Some(row));
            assert_eq!(load.counts(), head_reads, "the retained head adds no source reads");
            assert!(names.records.errors().is_empty());
        }
    }

    /// Metadata is no proof of text: an old record must refuse a missing or changed selected
    /// expansion. Conversely, NameOnly can list an unrelated member without opening its text;
    /// restoring WithText exposes the planted missing source through its normal typed error.
    #[test]
    fn member_metadata_does_not_prove_selected_or_unrelated_text() {
        let f = fixture(MEMBERS);
        let selected = targets(&f);
        let calls = selected.map(|row| row.written_by.expect("a member macro call"));
        let expected = metadata(reader(&f).records, MemberWant::NameOnly);
        for (plant, want) in [Plant::Missing(calls[0]), Plant::Changed(calls[0]), Plant::Missing(calls[1])]
            .into_iter().flat_map(|plant| [MemberWant::NameOnly, MemberWant::WithText].map(|want| (plant, want)))
        {
            let load = MemberLoad::new(&f.kept, calls, plant);
            let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the reader's frame");
            let memory = blaze_own::Erased::new(&frame);
            let records_alloc = memory.handle();
            let log = frame.alloc(Log::new(f.crate_names, records_alloc));
            let records = &*frame.alloc(Records::new(f.keys, &load, log, &frame, records_alloc));
            let names = NamesRecords { records, frame: &frame, lang_crates: &LANG_CRATES, body_sources: None };
            let rows = metadata(names.records, want);
            if want == MemberWant::NameOnly {
                assert_eq!(rows, expected);
                assert_eq!(load.counts(), [0, 0]);
                assert!(names.records.errors().is_empty(), "metadata asks no expansion text");
            } else {
                assert_ne!(rows, expected, "WithText cannot yield the member whose expansion was refused");
                assert!(!names.records.errors().is_empty());
            }
            let affected = match plant { Plant::Missing(call) | Plant::Changed(call) => call, Plant::None => unreachable!() };
            let key = member_key_of(checker_of(selected[0].owner), MemberKind::Fn, "selected");
            if affected == calls[0] {
                let mut program = LazyProgram::from_records(names.records, names.frame, names.lang_crates, None, alloc()).expect("the lazy program");
                assert!(program.head_row(key).is_err(), "the actual selected head must refuse {plant:?} after {want:?}");
            } else if want == MemberWant::NameOnly {
                let mut program = LazyProgram::from_records(names.records, names.frame, names.lang_crates, None, alloc()).expect("the lazy program");
                let row = program.head_row(key).expect("the unrelated missing text does not refuse the selected head").expect("the selected function has a row");
                assert_eq!(program.function(row).key, key);
                assert_eq!(load.counts(), [1, 0]);
                assert!(names.records.errors().is_empty(), "the selected head does not ask the unrelated expansion");
                let _ = metadata(names.records, MemberWant::WithText);
                assert!(load.counts()[1] > 0, "restoring the unused read must expose its missing source");
            }
            assert!(names.records.errors().iter().any(|error| match (plant, error) {
                (Plant::Missing(want), SourceError::Items { error: items_record::ItemsRecordError::Gone { call: Some(got), .. }, .. }) => want == *got,
                (Plant::Changed(want), SourceError::Items { error: items_record::ItemsRecordError::Stale { call: Some(got), .. }, .. }) => want == *got,
                _ => false,
            }), "the source error must name the planted expansion, {plant:?}");
            let b = builder();
            let mut maker = Maker::new(names, Some(key), &b, alloc());
            assert!(matches!(maker.verify(key, &[], &[]),
                Err(RunError::Front2(Front2Refusal::Lazy(LazyRefusal::RecordsVoid { tabled, .. }))) if tabled == key),
                "the source log must void verification before entry decoding");
        }
    }
}

const READS: [Read; 1] = [Read { fact: [3; 32], digest: [4; 32] }];

/// The command's stand-in (the module doc): each free fn's and static's key from its own text.
struct TextIds {
    rows: Vec<(ItemKey, ContentId)>,
    entry: ItemKey,
}

impl BodyIds for TextIds {
    fn body_check(&self, item: ItemKey) -> Option<BodyId<'_>> {
        let n = self.rows.binary_search_by_key(&item, |r| r.0).ok()?;
        Some(BodyId { key: self.rows[n].1, reads: &READS })
    }

    fn today(&self, fact: &[u8; 32]) -> Option<[u8; 32]> {
        (*fact == READS[0].fact).then_some(READS[0].digest)
    }

    fn crate_fact(&self, _item: ItemKey) -> Option<Read> {
        Some(Read { fact: [8; 32], digest: [9; 32] })
    }
}

fn ids_of(f: &Fixture) -> TextIds {
    let records = reader(f).records;
    let mut libs: Vec<LibKey> = Vec::new();
    assert!(records.each_item(0, &mut |key, _| libs.push(key)), "the program's items record lists its items");
    let mut rows = Vec::new();
    let mut entry = None;
    for &lib in libs.iter() {
        let kind = records.kind_of_item(lib);
        if !matches!(kind, Some(ItemKind::Function | ItemKind::Static)) || records.member_owner(lib).is_some() {
            continue;
        }
        let text = records.text_of(lib).unwrap_or_else(|| panic!("item {lib:x} has a text: {:?}", logged(records)));
        rows.push((checker_of(lib), Hasher::new("f20 test body check").part(text.text.as_bytes()).finish()));
        if kind == Some(ItemKind::Function) && records.name_of(lib) == Some("main") {
            entry = Some(checker_of(lib));
        }
    }
    assert!(logged(records).is_empty(), "the stand-in's reads: {:?}", logged(records));
    rows.sort_by_key(|r| r.0);
    TextIds { rows, entry: entry.expect("the program has a `main`") }
}

/// The checker key of the free fn named `name`.
fn fn_named(f: &Fixture, name: &str) -> ItemKey {
    let records = reader(f).records;
    let mut found = None;
    assert!(
        records.each_item(0, &mut |key, _| if found.is_none() && records.kind_of_item(key) == Some(ItemKind::Function) && records.name_of(key) == Some(name) {
            found = Some(checker_of(key));
        }),
        "the program's items record lists its items"
    );
    found.unwrap_or_else(|| panic!("no fn named `{name}`: {:?}", logged(records)))
}

/// One walked row: its fn (or static), its instance name, its content key, whether found.
struct Row {
    tabled: ItemKey,
    name: Vec<u8>,
    key: Option<CodeKey>,
    found: bool,
}

struct Walked {
    counts: Counts,
    rows: Vec<Row>,
}

impl Walked {
    /// The one row of fn `tabled` (each case's reader has one instance).
    fn of(&self, tabled: ItemKey) -> &Row {
        let mut of = self.rows.iter().filter(|r| r.tabled == tabled);
        let row = of.next().unwrap_or_else(|| panic!("no row of {:#x}", tabled.0));
        assert!(of.next().is_none(), "two rows of {:#x}", tabled.0);
        row
    }
}

/// One walk of `f` from `main` over `store`, a fresh reader and a fresh maker (one process), its
/// recorder planted with `plant`.
fn walk(f: &Fixture, store: &mut MemoryStore<'static>, plant: ReadPlant) -> Walked {
    let ids = ids_of(f);
    let names = reader(f);
    let b = builder();
    let mut maker = Maker::new(names, Some(ids.entry), &b, alloc());
    maker.plant_reads(plant);
    let (walk, counts) = walk_records(ids.entry, &ids, store, &b, &producer(), &mut maker, alloc()).unwrap_or_else(|e| panic!("the record walk: {e}; the records filed {:?}", logged(names.records)));
    assert!(logged(names.records).is_empty(), "the records filed {:?}", logged(names.records));
    let rows = (0..walk.len())
        .map(|row| Row { tabled: walk.tabled(row), name: walk.name(row).to_vec(), key: walk.key(row), found: matches!(walk.how(row), How::Done(Stepped::Found(_))) })
        .collect();
    Walked { counts, rows }
}

fn shared_constants(shared: usize, other: usize) -> Fixture {
    let body = blaze_own::format_in!(alloc(),
        "const SHARED: usize = {shared};\n\nconst OTHER: usize = {other};\n\n\
         fn known() -> usize {{\n    SHARED\n}}\n\n\
         fn mixed() -> usize {{\n    SHARED + OTHER\n}}\n\n\
         #[unsafe(no_mangle)]\npub extern \"C-unwind\" fn main() -> i32 {{\n    (known() + mixed()) as i32\n}}\n"
    );
    fixture(body.as_str())
}

fn verify_row(maker: &mut Maker<'static>, row: &Row, store: &MemoryStore<'static>) -> Verdict {
    let Some(key) = row.key else { panic!("the fixture's row must have a code key") };
    let bytes = match store.get(key) {
        Ok(Some(bytes)) => bytes,
        other => panic!("the fixture's code record: {other:?}"),
    };
    match maker.verify(row.tabled, &row.name, bytes) {
        Ok(Some(verdict)) => verdict,
        other => panic!("the fixture's verification: {other:?}"),
    }
}

/// Removing the earlier proof with a fresh maker prepares both constants. Keeping it prepares
/// only the other constant, while the real verifier still accepts the same complete read list.
#[test]
fn partially_known_read_lists_prepare_only_unanswered_rows() {
    let f = shared_constants(2, 3);
    let ids = ids_of(&f);
    let mut store = MemoryStore::new_in(alloc());
    let made = walk(&f, &mut store, ReadPlant::None);
    let known = made.of(fn_named(&f, "known"));
    let mixed = made.of(fn_named(&f, "mixed"));
    let b = builder();
    let mut maker = Maker::new(reader(&f), Some(ids.entry), &b, alloc());
    assert_eq!(verify_row(&mut maker, known, &store), Verdict::Holds);
    let before = maker.read_preparation();
    assert_eq!(verify_row(&mut maker, mixed, &store), Verdict::Holds);
    let after = maker.read_preparation();
    let prepared = after.prepared - before.prepared;
    let reused = after.reused - before.reused;
    assert!(prepared > 0, "the second entry must contain an unanswered preparation row");
    assert!(reused > 0, "the second entry must reuse a proved preparation row");

    let mut fresh = Maker::new(reader(&f), Some(ids.entry), &b, alloc());
    assert_eq!(verify_row(&mut fresh, mixed, &store), Verdict::Holds);
    let control = fresh.read_preparation();
    assert_eq!(control.reused, 0, "a fresh maker has no earlier proof to reuse");
    assert_eq!(control.prepared, prepared + reused, "removing the proof must restore its preparation work");
    println!("partial read preparation: retained maker prepared {prepared}, reused {reused}; fresh maker {control:?}");
}

/// A shared question with a different stored answer and an unknown question whose dependency
/// changed both leave the entry stale. Neither case may seed the still-unproved other answer.
#[test]
fn partially_known_read_lists_still_reject_changed_answers() {
    let before = shared_constants(2, 3);
    let mut old_store = MemoryStore::new_in(alloc());
    let old = walk(&before, &mut old_store, ReadPlant::None);
    let old_mixed = old.of(fn_named(&before, "mixed"));

    for (shared, other) in [(5, 3), (2, 7)] {
        let after = shared_constants(shared, other);
        let ids = ids_of(&after);
        let mut today_store = MemoryStore::new_in(alloc());
        let today = walk(&after, &mut today_store, ReadPlant::None);
        let known = today.of(fn_named(&after, "known"));
        let mixed = today.of(fn_named(&after, "mixed"));
        let b = builder();
        let mut maker = Maker::new(reader(&after), Some(ids.entry), &b, alloc());
        assert_eq!(verify_row(&mut maker, known, &today_store), Verdict::Holds);
        assert!(matches!(verify_row(&mut maker, old_mixed, &old_store), Verdict::Stale { kind: ReadKind::Const, .. }), "an edited constant must invalidate the old entry");
        let rejected = maker.read_preparation();
        assert_eq!(verify_row(&mut maker, mixed, &today_store), Verdict::Holds);
        assert!(maker.read_preparation().prepared > rejected.prepared, "the refused entry must not prove its unknown constant for the next entry");
        println!("partial read invalidation: SHARED={shared}, OTHER={other}; stale entry refused, today's entry held");
    }
}

/// Observation sees the real verification stages and preserves both acceptance and rejection,
/// including the fact that a rejected read list must not seed later preparation proofs.
#[test]
fn observed_verification_preserves_read_answers_and_preparation() {
    struct Seen<'a>(&'a core::cell::Cell<usize>);
    impl<'a> WalkObserver for Seen<'a> {
        type Span = Seen<'a>;

        fn span(&self, _name: &'static str) -> Self::Span {
            self.0.set(self.0.get() + 1);
            Seen(self.0)
        }
    }

    let before = shared_constants(2, 3);
    let mut old_store = MemoryStore::new_in(alloc());
    let old = walk(&before, &mut old_store, ReadPlant::None);
    let old_mixed = old.of(fn_named(&before, "mixed"));
    let spans = core::cell::Cell::new(0);
    let observer = Seen(&spans);
    for (shared, other) in [(2, 3), (5, 3), (2, 7)] {
        let after = shared_constants(shared, other);
        let ids = ids_of(&after);
        let mut today_store = MemoryStore::new_in(alloc());
        let today = walk(&after, &mut today_store, ReadPlant::None);
        let known = today.of(fn_named(&after, "known"));
        let mixed = today.of(fn_named(&after, "mixed"));
        let b = builder();
        let mut plain = Maker::new(reader(&after), Some(ids.entry), &b, alloc());
        let mut observed = Maker::new(reader(&after), Some(ids.entry), &b, alloc());
        for (row, store) in [(known, &today_store), (old_mixed, &old_store), (mixed, &today_store)] {
            let key = row.key.expect("the fixture's code key");
            let bytes = store.get(key).expect("the fixture's store").expect("the fixture's entry");
            let expected = verify_row(&mut plain, row, store);
            let actual = observed.verify_observed(key, key, row.tabled, &row.name, bytes, &observer)
                .expect("the observed verifier").expect("the observed verdict");
            assert_eq!(actual, expected, "SHARED={shared}, OTHER={other}");
            assert_eq!(observed.read_preparation(), plain.read_preparation());
            assert_eq!(observed.verified_from_answers(), plain.verified_from_answers());
        }
    }
    assert!(spans.get() > 0, "the equality control must exercise an observing strategy");
}

/// The rows of `fresh` whose entry `kept` holds under another key: stale code served.
fn stale_served(kept: &Walked, fresh: &Walked) -> Vec<String> {
    assert_eq!(kept.rows.len(), fresh.rows.len(), "the two walks reached the same instances");
    let mut stale = Vec::new();
    for r in fresh.rows.iter() {
        let k = kept.rows.iter().find(|k| k.tabled == r.tabled && k.name == r.name).unwrap_or_else(|| panic!("the kept walk reached no row of {:#x}", r.tabled.0));
        if k.key != r.key {
            stale.push(format!("{:#x} ({})", r.tabled.0, if k.found { "found" } else { "made" }));
        }
    }
    stale
}

/// What one edit did: the walk after it over the kept store, and over an empty one.
struct Edited {
    kept: Walked,
    fresh: Walked,
    before: Fixture,
    after: Fixture,
}

/// Walk `before` into a store, then `after` over that store (and over an empty one), each
/// recorder planted with `plant`; the fresh walk is never planted.
fn edit(before: &str, after: &str, plant: ReadPlant) -> Edited {
    assert_ne!(before, after, "the edit applies");
    let before = fixture(before);
    let after = fixture(after);
    let mut store = MemoryStore::new_in(alloc());
    walk(&before, &mut store, plant);
    let kept = walk(&after, &mut store, plant);
    let fresh = walk(&after, &mut MemoryStore::new_in(alloc()), ReadPlant::None);
    Edited { kept, fresh, before, after }
}

/// Edit ONLY a dependency of fn `reader`: its row made again over the kept store, its read list
/// the cause (`stale_reads`), every entry the fresh walk's. Returns the line it prints.
fn dependency_edit(what: &str, before: &str, after: &str, reader_fn: &str) -> String {
    let e = edit(before, after, ReadPlant::None);
    assert!(e.before.keys.len() == e.after.keys.len());
    let reader_key = fn_named(&e.after, reader_fn);
    let row = e.kept.of(reader_key);
    let stale = stale_served(&e.kept, &e.fresh);
    let line = format!("{what}: {}; `{reader_fn}` {}; stale served {stale:?}", e.kept.counts, if row.found { "FOUND" } else { "made again" });
    println!("{line}");
    assert!(!row.found, "{what}: `{reader_fn}`'s text did not move and only its read list sees the edit: it must be made again ({line})");
    assert!(e.kept.counts.stale_reads >= 1, "{what}: the reader was made again for its read list ({line})");
    assert!(stale.is_empty(), "{what}: an entry served is not the one today's text makes ({line})");
    line
}

/// A struct's field type (`u8` to `u64`): `get`'s `p.b` moves; only its `Adt` row sees it.
#[test]
fn a_dependency_field_type_remakes_its_reader() {
    let tail = "fn get(p: &P) -> i32 {\n    p.b\n}\n\nfn mk() -> P {\n    P { a: 1, b: 7 }\n}\n\n#[unsafe(no_mangle)]\npub extern \"C-unwind\" fn main() -> i32 {\n    let p = mk();\n    get(&p)\n}\n";
    dependency_edit("field type", &format!("struct P {{\n    a: u8,\n    b: i32,\n}}\n\n{tail}"), &format!("struct P {{\n    a: u64,\n    b: i32,\n}}\n\n{tail}"), "get");
}

/// A constant's value (`2` to `5`): `get`'s text reads `N`'s type only; the value is its
/// `Const` row.
#[test]
fn a_dependency_const_value_remakes_its_reader() {
    let tail = "fn get() -> usize {\n    N\n}\n\n#[unsafe(no_mangle)]\npub extern \"C-unwind\" fn main() -> i32 {\n    get() as i32\n}\n";
    dependency_edit("const", &format!("const N: usize = 2;\n\n{tail}"), &format!("const N: usize = 5;\n\n{tail}"), "get");
}

/// A static's kind (`static` to `static mut`): `get`'s read of it is its `Static` row.
#[test]
fn a_dependency_static_kind_remakes_its_reader() {
    let tail = "fn get() -> i32 {\n    unsafe { S }\n}\n\n#[unsafe(no_mangle)]\npub extern \"C-unwind\" fn main() -> i32 {\n    get()\n}\n";
    dependency_edit("static kind", &format!("static S: i32 = 3;\n\n{tail}"), &format!("static mut S: i32 = 3;\n\n{tail}"), "get");
}

/// A callee's signature: its ABI (`extern "C-unwind"` to Rust's), which `f`'s call reads as
/// the callee's `Abi` row; `f`'s text names `g` only.
#[test]
fn a_callee_abi_remakes_its_caller() {
    let tail = "fn f() -> i32 {\n    g(2)\n}\n\n#[unsafe(no_mangle)]\npub extern \"C-unwind\" fn main() -> i32 {\n    f()\n}\n";
    dependency_edit("callee abi", &format!("extern \"C-unwind\" fn g(x: i32) -> i32 {{\n    x + 1\n}}\n\n{tail}"), &format!("fn g(x: i32) -> i32 {{\n    x + 1\n}}\n\n{tail}"), "f");
}

/// A callee's by-value parameter growing past what registers pass (8 bytes to 32): the call in
/// `f` passes it otherwise; `f`'s and `g`'s texts are unchanged (`mk`'s moves).
#[test]
fn a_by_value_parameter_growing_remakes_the_caller() {
    let tail = "fn g(q: Q) -> i64 {\n    q.a\n}\n\nfn f() -> i64 {\n    g(mk())\n}\n\n#[unsafe(no_mangle)]\npub extern \"C-unwind\" fn main() -> i32 {\n    f() as i32\n}\n";
    dependency_edit(
        "by-value parameter",
        &format!("struct Q {{\n    a: i64,\n}}\n\nfn mk() -> Q {{\n    Q {{ a: 4 }}\n}}\n\n{tail}"),
        &format!("struct Q {{\n    a: i64,\n    b: i64,\n    c: i64,\n    d: i64,\n}}\n\nfn mk() -> Q {{\n    Q {{ a: 4, b: 0, c: 0, d: 0 }}\n}}\n\n{tail}"),
        "f",
    );
}

/// TWINS: a struct no reached code reads gaining a wider field, and a fn nothing calls added:
/// nothing is made, every entry found and verified.
#[test]
fn an_unrelated_edit_remakes_nothing() {
    let tail = "struct P {\n    a: u8,\n    b: i32,\n}\n\nfn get(p: &P) -> i32 {\n    p.b\n}\n\n#[unsafe(no_mangle)]\npub extern \"C-unwind\" fn main() -> i32 {\n    let p = P { a: 1, b: 7 };\n    get(&p)\n}\n";
    let e = edit(&format!("struct R {{\n    a: u8,\n}}\n\n{tail}"), &format!("struct R {{\n    a: u64,\n}}\n\nfn unused() -> i32 {{\n    0\n}}\n\n{tail}"), ReadPlant::None);
    let stale = stale_served(&e.kept, &e.fresh);
    let c = e.kept.counts;
    let line = format!("unrelated: {c}; stale served {stale:?}");
    println!("{line}");
    assert_eq!((c.made, c.stale_reads, c.found), (0, 0, c.reached), "an edit nothing reached reads remade something ({line})");
    assert!(c.verified >= 1, "the found entries' read lists were asked ({line})");
    assert!(stale.is_empty(), "{line}");
}

/// THE CONTROL: the field-type edit with the recorder planted to record no layout question:
/// `get`'s entry is found holding and its key is not the fresh walk's: stale code, caught.
#[test]
fn the_planted_recorder_serves_stale_code_and_the_oracle_catches_it() {
    let tail = "fn get(p: &P) -> i32 {\n    p.b\n}\n\nfn mk() -> P {\n    P { a: 1, b: 7 }\n}\n\n#[unsafe(no_mangle)]\npub extern \"C-unwind\" fn main() -> i32 {\n    let p = mk();\n    get(&p)\n}\n";
    let e = edit(&format!("struct P {{\n    a: u8,\n    b: i32,\n}}\n\n{tail}"), &format!("struct P {{\n    a: u64,\n    b: i32,\n}}\n\n{tail}"), ReadPlant::SkipsLayouts);
    let get = fn_named(&e.after, "get");
    let stale = stale_served(&e.kept, &e.fresh);
    let line = format!("planted (SkipsLayouts): {}; `get` {}; stale served {stale:?}", e.kept.counts, if e.kept.of(get).found { "found holding" } else { "made" });
    println!("{line}");
    assert!(e.kept.of(get).found, "the plant changed nothing: `get` was made again ({line})");
    assert!(stale.iter().any(|s| s.starts_with(&format!("{:#x} ", get.0))), "the oracle missed the stale `get` ({line})");
}

/// The read list holds the answers of the build that SUCCEEDED only: `pick::<E>` over a
/// `#[repr(u8)]` enum, made again (its text edited) in a process where `main`, which names `E`,
/// was found and not checked, so `E`'s head may be unmade at the first build's ask (the lazy
/// front then answers `repr` by default; the build is made again once the head is). Its entry
/// holds ONE `Repr` row, the fresh walk's (the right answer), and the next walk finds and
/// verifies every entry.
#[test]
fn a_rebuilt_make_keeps_one_repr_row_the_right_one() {
    let tail = "#[repr(u8)]\nenum E {\n    A = 3,\n    B = 9,\n}\n\nfn tag(e: &E) -> u8 {\n    match e {\n        E::A => 1,\n        E::B => 2,\n    }\n}\n\n#[unsafe(no_mangle)]\npub extern \"C-unwind\" fn main() -> i32 {\n    let e = pick(false, E::A, E::B);\n    tag(&e) as i32\n}\n";
    let before = format!("fn pick<T>(c: bool, a: T, b: T) -> T {{\n    if c {{ a }} else {{ b }}\n}}\n\n{tail}");
    let after = format!("fn pick<T>(c: bool, a: T, b: T) -> T {{\n    match c {{\n        true => a,\n        false => b,\n    }}\n}}\n\n{tail}");
    let before = fixture(&before);
    let after = fixture(&after);
    let mut store = MemoryStore::new_in(alloc());
    walk(&before, &mut store, ReadPlant::None);
    let kept = walk(&after, &mut store, ReadPlant::None);
    let mut fresh_store = MemoryStore::new_in(alloc());
    let fresh = walk(&after, &mut fresh_store, ReadPlant::None);
    let pick = fn_named(&after, "pick");
    assert!(!kept.of(pick).found, "`pick`'s text moved: its instance is made again");
    let repr_rows = |store: &MemoryStore<'static>, row: &Row| -> Vec<(Vec<u8>, ContentId)> {
        let key = row.key.unwrap_or_else(|| panic!("`pick::<E>` has a key"));
        let bytes = store.get(key).unwrap_or_else(|e| panic!("the store: {e:?}")).unwrap_or_else(|| panic!("`pick::<E>`'s entry is in the store"));
        let view = EntryView::read(bytes).unwrap_or_else(|e| panic!("`pick::<E>`'s entry: {e:?}"));
        view.reads()
            .map(|r| r.unwrap_or_else(|e| panic!("a read-list row: {e:?}")))
            .filter(|(q, _)| q.first().copied().and_then(ReadKind::of) == Some(ReadKind::Repr))
            .map(|(q, d)| (q.to_vec(), d))
            .collect()
    };
    let made = repr_rows(&store, kept.of(pick));
    let right = repr_rows(&fresh_store, fresh.of(pick));
    let again = walk(&after, &mut store, ReadPlant::None);
    let line = format!("repr: {} Repr row(s) in `pick::<E>`'s entry, the fresh walk's: {}; next walk {}", made.len(), made == right, again.counts);
    println!("{line}");
    assert_eq!(made.len(), 1, "one `repr` question, asked once ({line})");
    assert_eq!(made, right, "the row is the answer today's program gives once `E`'s head is made ({line})");
    assert_eq!((again.counts.made, again.counts.found, again.counts.stale_reads), (0, again.counts.reached, 0), "the next walk finds and verifies every entry ({line})");
}
