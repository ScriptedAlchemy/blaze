//! Existing frozen heads reach the real lazy program and Maker. Removing the provider's
//! bytes restores source freezes; a refused record never becomes a source lowering or a
//! stale code-entry verdict. The producer here is the actual records-backed SeededHeads.

use super::{Fixture, Kept, LANG_CRATES, alloc, builder, fixture, fn_named, producer, walk};
use blaze_drive::key::Key;
use blaze_id::ItemKey as LibKey;
use blaze_ids::{ContentId, Hasher};
use blaze_ladder::body_ids::{BodyId, BodyIds, Make, Read};
use blaze_ladder::code_records::reads::{self, ReadKind, ReadPlant};
use blaze_ladder::code_records::{CodeKey, CodeStore, EntryView, MemoryStore};
use blaze_ladder::front2::Front2;
use blaze_ladder::front2::heads::Want;
use blaze_ladder::front2::keys::{MemberKind, checker_of, member_key_of};
use blaze_ladder::front2::program::Front2Refusal;
use blaze_ladder::front2::program::lazy::{HeadCounts, LazyProgram, LazyRefusal, RecordsOpts};
use blaze_ladder::front2::receiver::Pointers;
use blaze_ladder::front2::source::{Load, LoadedExpansion, Log, MemberWant, NamesSource, Records, SourceError};
use blaze_ladder::front2::tables::head::{FreezeError, FrozenError, HeadSource, Known, KnownError, KnownHeads, KnownSource, NotTheirs, SeededHeads, SeededHeadsOpts, StoreConsts};
use blaze_ladder::run::RunError;
use blaze_ladder::run::make_one::{Maker, MakerOpts, NamesRecords, RecordsProgram, SecondCheck, walk_records};
use blaze_ladder::run::special::MakerStart;
use blaze_ladder::seam::FrontEnd;
use blaze_lower::scoped::Prepares;
use blaze_own::{AVec, Alloc};
use blaze_surface::ItemKind;
use blaze_typeck::{ItemKey, PNode, PTy, PTys, PortableTys, UintTy};
use core::cell::Cell;

/// Shortens the fixture's retained static bytes without leaking a new reader or its caches.
struct BorrowedLoad<'f>(&'f Kept);

impl<'n> Load<'n> for BorrowedLoad<'_> {
    fn names(&self, krate: usize) -> Option<&'n [u8]> {
        self.0.names(krate)
    }

    fn items(&self, krate: usize) -> Option<&'n [u8]> {
        self.0.items(krate)
    }

    fn file(&self, krate: usize, path: blaze_scope::package_path::PackagePath<'_>) -> Option<&'n str> {
        self.0.file(krate, path)
    }

    fn expansion(&self, krate: usize, call: LibKey) -> Option<LoadedExpansion<'n>> {
        self.0.expansion(krate, call)
    }
}

fn records<'n>(f: &Fixture, load: &'n dyn Load<'n>, frame: &'n blaze_mem::Frame, memory: Alloc<'n>) -> NamesRecords<'n> {
    let log = frame.alloc(Log::new(f.crate_names, memory));
    let records = &*frame.alloc(Records::new(f.keys, load, log, frame, memory));
    NamesRecords { records, frame, lang_crates: &LANG_CRATES, body_sources: None }
}

fn named<'a>(records: &'a Records<'a>, name: &str) -> LibKey {
    let mut found = None;
    assert!(records.each_item(0, &mut |key, _| {
        if records.name_of(key) == Some(name) && records.member_owner(key).is_none() {
            assert!(found.replace(key).is_none(), "one top-level item named {name}");
        }
    }));
    found.unwrap_or_else(|| panic!("the fixture has no item named {name}"))
}

struct Saved {
    requested: AVec<'static, Want>,
    heads: AVec<'static, (Want, AVec<'static, u8>)>,
}

/// Keeps real frozen forms after the producing source and every scratch store have dropped.
fn produce(f: &Fixture, requested: &[(&str, fn(LibKey) -> Want)]) -> Saved {
    let load = BorrowedLoad(&f.kept);
    let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("producer frame");
    let memory = blaze_own::Erased::new(&frame);
    let names = records(f, &load, &frame, memory.handle());
    let mut wants = blaze_own::Vec::new_in(alloc());
    for &(name, want) in requested {
        wants.push(want(named(names.records, name)));
    }
    freeze(names, &wants)
}

fn freeze(names: NamesRecords<'_>, requested: &[Want]) -> Saved {
    let front = Front2::from_records(names.records, names.frame, names.lang_crates).expect("producer front");
    let prepared = Prepares::new_in(names.frame.alloc(blaze_own::Erased::new(names.frame)).bump());
    let mut evaluator = StoreConsts { names: names.records, index: &front.index, prepared };
    let mut source = SeededHeads::new(SeededHeadsOpts {
        names: names.records, sized: front.index.lang("sized"), pointers: Pointers::find(&front.index, front.resolver),
        frame: names.frame, scratch: alloc(), alloc: alloc(), consts: Some(&mut evaluator), known: None, owners: None,
    });
    let mut saved = Saved { requested: blaze_own::Vec::new_in(alloc()), heads: blaze_own::Vec::new_in(alloc()) };
    for &want in requested {
        source.frozen(want).unwrap_or_else(|error| panic!("producing {want:?}: {error}"));
        saved.requested.push(want);
    }
    for (want, bytes) in source.frozen_from(0) {
        let mut owned = blaze_own::Vec::with_capacity_in(bytes.len(), alloc());
        owned.extend_from_slice(bytes);
        saved.heads.push((want, owned));
    }
    assert!(source.attempts > 0, "the producer must actually freeze the records");
    assert!(names.records.errors().is_empty());
    saved
}

#[derive(Clone, Copy, Debug)]
enum Supply {
    Present,
    Absent,
    Without(Want),
    Refused { want: Want, why: NotTheirs },
    Unreadable(Want),
    Invalid(Want),
    WrongCell { want: Want, other: Want },
    Malformed(Want),
    RefuseLocals,
}

struct Provider<'s> {
    saved: &'s Saved,
    supply: Supply,
    calls: Cell<u32>,
    hits: Cell<u32>,
    locals: Cell<u32>,
}

impl<'s> Provider<'s> {
    fn new(saved: &'s Saved, supply: Supply) -> Self {
        Self { saved, supply, calls: Cell::new(0), hits: Cell::new(0), locals: Cell::new(0) }
    }

    fn bytes(&self, want: Want) -> Option<&[u8]> {
        self.saved.heads.iter().find(|(cell, _)| *cell == want).map(|(_, bytes)| &bytes[..])
    }
}

impl KnownHeads for Provider<'_> {
    fn known(&self, cell: Want) -> Known<'_> {
        self.calls.set(self.calls.get() + 1);
        if cell.key().is_inner() {
            self.locals.set(self.locals.get() + 1);
            if matches!(self.supply, Supply::RefuseLocals) {
                return Known::Refused(NotTheirs::NoCell { said: "body-local heads belong to their body" });
            }
        }
        match self.supply {
            Supply::Absent => return Known::Freeze,
            Supply::Without(want) if cell == want => return Known::Freeze,
            Supply::Refused { want, why } if cell == want => return Known::Refused(why),
            Supply::Unreadable(want) if cell == want => return Known::Invalid(KnownError::Unreadable { key: Key(ContentId([41; 32])) }),
            Supply::Invalid(want) if cell == want => return Known::Invalid(KnownError::Invalid { key: Key(ContentId([42; 32])), why: "the stored head's source identity differs" }),
            Supply::WrongCell { want, other } if cell == want => return Known::Bytes(self.bytes(other).expect("the other actual frozen cell")),
            Supply::Malformed(want) if cell == want => return Known::Bytes(b"malformed BZFZ"),
            _ => {}
        }
        match self.bytes(cell) {
            Some(bytes) => {
                self.hits.set(self.hits.get() + 1);
                Known::Bytes(bytes)
            }
            None => Known::Freeze,
        }
    }
}

fn program<'n>(names: NamesRecords<'n>, provider: &'n dyn KnownHeads) -> RecordsProgram<'n> {
    LazyProgram::from_records(RecordsOpts {
        records: names.records, frame: names.frame, lang_crates: names.lang_crates, entry: None,
        known_heads: Some(KnownSource { heads: provider, errors: blaze_own::testing::heap().shared() }), alloc: alloc(),
    }).expect("the consuming lazy program")
}

fn array_len(types: PTys<'_>, ty: PTy) -> Option<u64> {
    let PNode::Array(element, len) = types.node(ty).expect("a head's type") else { return None };
    assert_eq!(types.node(element).expect("array element"), PNode::Uint(UintTy::U8));
    Some(types.value_u64(len).expect("a concrete array length"))
}

#[derive(Debug, PartialEq, Eq)]
struct Facts {
    abi: Option<blaze_build::Abi>,
    nounwind: Option<bool>,
    caller: Option<bool>,
    attrs: blaze_lir::func::FnAttrs,
    inputs: usize,
    input_array: Option<u64>,
    output_array: Option<u64>,
}

fn facts(program: &RecordsProgram<'_>, item: ItemKey) -> Facts {
    let row = program.fn_index(item).expect("a made row");
    let sig = program.sig(row);
    Facts {
        abi: program.fn_abi(item), nounwind: program.nounwind(item), caller: program.track_caller(item), attrs: program.fn_attrs(item),
        inputs: sig.inputs.len(), input_array: sig.inputs.first().and_then(|&ty| array_len(sig.types, ty)), output_array: array_len(sig.types, sig.output),
    }
}

const HEADS: &str = "
const LENGTH: usize = 3;
type Bytes = [u8; LENGTH];
#[cold]
#[inline(never)]
pub extern \"C\" fn selected(x: Bytes) -> Bytes { x }
#[track_caller]
pub fn tracked(x: u8) -> u8 { x }
pub fn other() -> u16 { 9 }
#[unsafe(no_mangle)]
pub extern \"C-unwind\" fn main() -> i32 { 0 }
";

#[test]
fn existing_heads_preserve_abi_attributes_and_constant_shapes_without_freezing() {
    let f = fixture(HEADS);
    let saved = produce(&f, &[("selected", Want::Fn), ("tracked", Want::Fn)]);
    let keys = [checker_of(saved.requested[0].key()), checker_of(saved.requested[1].key())];
    let mut expected = None;
    for supply in [Supply::Present, Supply::Absent] {
        let provider = Provider::new(&saved, supply);
        let load = BorrowedLoad(&f.kept);
        let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("consumer frame");
        let memory = blaze_own::Erased::new(&frame);
        let names = records(&f, &load, &frame, memory.handle());
        let mut program = program(names, &provider);
        assert_eq!(program.head_counts(), HeadCounts::default());
        for key in keys {
            assert!(program.head_row(key).expect("the actual head request").is_some());
        }
        let got = keys.map(|key| facts(&program, key));
        assert_eq!((got[0].input_array, got[0].output_array), (Some(3), Some(3)));
        assert_eq!(got[0].abi, Some(blaze_build::Abi::C));
        assert_eq!(got[0].nounwind, Some(true));
        assert_ne!(got[0].attrs.0 & blaze_lir::func::FnAttrs::COLD, 0);
        assert_eq!(got[0].attrs.inline(), blaze_lir::func::Inline::Never);
        assert_eq!(got[1].caller, Some(true));
        assert_eq!(got[1].abi, Some(blaze_build::Abi::Rust));
        let counts = program.head_counts();
        if matches!(supply, Supply::Present) {
            assert!(counts.taken >= 2);
            assert_eq!((counts.attempts, counts.lowered, counts.eval_lowered), (0, 0, 0));
            expected = Some(got);
        } else {
            assert_eq!(Some(got), expected, "removing known bytes preserves every asked head fact");
            assert_eq!(counts.taken, 0);
            assert!(counts.attempts > 0 && counts.lowered > 0, "removing the record restores source freezing: {counts:?}");
        }
        let calls = provider.calls.get();
        for key in keys {
            assert!(program.head_row(key).expect("the retained row").is_some());
            program.check_known_heads(key).expect("the source remains valid");
        }
        assert_eq!(program.head_counts(), counts);
        assert_eq!(provider.calls.get(), calls, "a retained row never asks the provider again");
        assert!(names.records.errors().is_empty());
    }
}

#[test]
fn a_cold_function_freeze_reuses_its_known_alias_dependency() {
    let f = fixture(HEADS);
    let saved = produce(&f, &[("selected", Want::Fn), ("Bytes", Want::Head)]);
    let own = saved.requested[0];
    let alias = saved.requested[1];
    assert_ne!(own, alias);
    let key = checker_of(own.key());
    let mut reused = None;
    for supply in [Supply::Without(own), Supply::Absent] {
        let provider = Provider::new(&saved, supply);
        let load = BorrowedLoad(&f.kept);
        let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("consumer frame");
        let memory = blaze_own::Erased::new(&frame);
        let names = records(&f, &load, &frame, memory.handle());
        let mut program = program(names, &provider);
        assert!(program.head_row(key).expect("the cold caller's head").is_some());
        let counts = program.head_counts();
        assert!(counts.attempts > 0, "the parent itself has no saved record");
        let got = facts(&program, key);
        assert_eq!((got.input_array, got.output_array), (Some(3), Some(3)));
        if matches!(supply, Supply::Without(_)) {
            assert!(counts.taken > 0 && provider.hits.get() > 0, "a dependency is actually taken during the cold freeze");
            reused = Some((got, counts));
        } else {
            let (expected, before) = reused.take().expect("the known-dependency case");
            assert_eq!(got, expected);
            assert_eq!(counts.taken, 0);
            assert!(counts.lowered > before.lowered, "removing only available records restores their lowerings: {before:?} -> {counts:?}");
        }
        assert!(names.records.errors().is_empty());
    }
}

fn is_known_failure(error: &RunError) -> bool {
    matches!(error, RunError::Front2(Front2Refusal::Lazy(LazyRefusal::KnownHead { .. })))
}

fn assert_cause(error: &RunError, supply: Supply, want: Want) {
    let RunError::Front2(Front2Refusal::Lazy(LazyRefusal::KnownHead { failure, .. })) = error else {
        panic!("{supply:?}: the typed known-head failure was lost: {error}")
    };
    assert_eq!(failure.want, want);
    let matches = match (supply, failure.error()) {
        (Supply::Refused { why, .. }, FreezeError::NotTheirs { want: cell, why: actual }) => *cell == want && *actual == why,
        (Supply::Unreadable(_), FreezeError::KnownProvider { want: cell, error: KnownError::Unreadable { key } }) => *cell == want && *key == Key(ContentId([41; 32])),
        (Supply::Invalid(_), FreezeError::KnownProvider { want: cell, error: KnownError::Invalid { key, why } }) => *cell == want && *key == Key(ContentId([42; 32])) && *why == "the stored head's source identity differs",
        (Supply::WrongCell { other, .. }, FreezeError::Read { want: cell, error: FrozenError::WrongCell { found } }) => *cell == want && *found == other,
        (Supply::Malformed(_), FreezeError::Read { want: cell, .. }) => *cell == want,
        _ => false,
    };
    assert!(matches, "{supply:?}: a different typed cause was retained: {failure:?}");
}

#[test]
fn invalid_known_heads_are_terminal_and_never_lowered_instead() {
    let f = fixture(HEADS);
    let saved = produce(&f, &[("selected", Want::Fn), ("other", Want::Fn)]);
    let want = saved.requested[0];
    let other = saved.requested[1];
    let key = checker_of(want.key());
    for supply in [
        Supply::Refused { want, why: NotTheirs::NoCell { said: "the selected head has no current cell" } },
        Supply::Refused { want, why: NotTheirs::OtherUnits { said: "the selected head belongs to another unit" } },
        Supply::Refused { want, why: NotTheirs::Failed { said: "the selected head's producer refused it" } },
        Supply::Unreadable(want), Supply::Invalid(want), Supply::WrongCell { want, other }, Supply::Malformed(want),
    ] {
        let provider = Provider::new(&saved, supply);
        let load = BorrowedLoad(&f.kept);
        let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("consumer frame");
        let memory = blaze_own::Erased::new(&frame);
        let names = records(&f, &load, &frame, memory.handle());
        let mut program = program(names, &provider);
        let refused = program.head_row(key).expect_err("the known record must refuse");
        assert_cause(&refused, supply, want);
        assert!(matches!(program.check_known_heads(key), Err(LazyRefusal::KnownHead { .. })));
        let counts = program.head_counts();
        assert_eq!((counts.attempts, counts.lowered, counts.eval_lowered), (0, 0, 0), "{supply:?}");
        assert!(program.fn_index(key).is_none());
        let calls = provider.calls.get();
        let again = program.head_row(key).expect_err("the failure remains terminal");
        assert_cause(&again, supply, want);
        assert_eq!(program.head_counts(), counts);
        assert_eq!(provider.calls.get(), calls, "a known failure is retained, not retried");
        assert!(names.records.errors().is_empty(), "the typed failure must not depend on a names-log error");
    }
}

#[test]
fn a_cold_parent_keeps_its_known_dependencys_exact_failure() {
    let f = fixture(HEADS);
    let mut saved = produce(&f, &[("selected", Want::Fn), ("Bytes", Want::Head)]);
    let own = saved.requested[0];
    let alias = saved.requested[1];
    // The parent must enter the real source freezer before it meets the bad known child.
    saved.heads.retain(|(cell, _)| *cell != own);
    let key = checker_of(own.key());
    for supply in [
        Supply::Refused { want: alias, why: NotTheirs::Failed { said: "the alias's producer refused it" } },
        Supply::Unreadable(alias), Supply::Invalid(alias), Supply::Malformed(alias),
    ] {
        let provider = Provider::new(&saved, supply);
        let load = BorrowedLoad(&f.kept);
        let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("cold parent frame");
        let memory = blaze_own::Erased::new(&frame);
        let names = records(&f, &load, &frame, memory.handle());
        let mut program = program(names, &provider);
        let refused = program.head_row(key).expect_err("a failed known dependency prevents the cold parent's head");
        assert_cause(&refused, supply, alias);
        let counts = program.head_counts();
        assert!(counts.attempts > 0, "the parent's source freeze must have reached the dependency");
        assert!(program.fn_index(key).is_none(), "the failed parent has no successful row");
        let calls = provider.calls.get();
        let again = program.head_row(key).expect_err("the dependency's failure remains terminal");
        assert_cause(&again, supply, alias);
        assert_eq!(program.head_counts(), counts, "the retained Again chain never starts another source freeze");
        assert_eq!(provider.calls.get(), calls);
        assert!(names.records.errors().is_empty());
    }
}

const CALLER: &str = "
extern \"C-unwind\" fn callee(x: i32) -> i32 { x + 1 }
#[unsafe(no_mangle)]
pub extern \"C-unwind\" fn main() -> i32 { callee(4) }
";

#[test]
fn a_known_callee_failure_cannot_become_a_stale_code_entry() {
    let f = fixture(CALLER);
    let mut store = MemoryStore::new_in(alloc());
    let walked = walk(&f, &mut store, ReadPlant::None);
    let caller = walked.of(fn_named(&f, "main"));
    let entry = store.get(caller.key.expect("caller code key")).expect("caller record readable").expect("caller record retained");
    let saved = produce(&f, &[("main", Want::Fn), ("callee", Want::Fn)]);
    let callee = saved.requested[1];
    let mut ground = PortableTys::new_in(alloc());
    let asked = EntryView::read(entry).expect("the real caller entry").reads().any(|row| {
        let (question, _) = row.expect("a read row");
        let (kind, _, item, _) = reads::question(question, &mut ground, alloc()).expect("the recorded question");
        kind == ReadKind::Abi && item == checker_of(callee.key())
    });
    assert!(asked, "the failing callee must be reached through preparation of the stored caller's ABI question");
    for supply in [Supply::Refused { want: callee, why: NotTheirs::NoCell { said: "the callee has no current cell" } }, Supply::Unreadable(callee), Supply::Malformed(callee)] {
        let provider = Provider::new(&saved, supply);
        let load = BorrowedLoad(&f.kept);
        let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("verification frame");
        let memory = blaze_own::Erased::new(&frame);
        let names = records(&f, &load, &frame, memory.handle());
        let b = builder();
        let mut maker: Maker<'_> = Maker::of_start(MakerOpts {
            names, entry: Some(caller.tabled), start: MakerStart::ByRule, typing: SecondCheck,
            builder: &b, known_heads: Some(KnownSource { heads: &provider, errors: blaze_own::testing::heap().shared() }), alloc: alloc(),
        });
        let refused = maker.verify(caller.tabled, &caller.name, entry).expect_err("provider failure is neither Holds nor Stale");
        assert_cause(&refused, supply, callee);
        let counts = maker.head_counts();
        assert!(counts.taken > 0, "the caller's valid frozen head must be imported first");
        assert_eq!((counts.attempts, counts.lowered, counts.eval_lowered), (0, 0, 0));
        let calls = provider.calls.get();
        assert!(is_known_failure(&maker.verify(caller.tabled, &caller.name, entry).expect_err("the failure is retained")));
        assert_eq!(maker.head_counts(), counts);
        assert_eq!(provider.calls.get(), calls);
        assert!(names.records.errors().is_empty());
    }
}

/// This fixed-source equality walk also reaches locals filed during lowering, which the
/// persisted top-level TextIds cannot list. As in d106_records/local_statics, its command
/// stand-in keys every reached item deterministically; the real Maker still lowers/checks it.
struct EveryItem;

impl BodyIds for EveryItem {
    fn body_check(&self, item: ItemKey) -> Option<BodyId<'_>> {
        Some(BodyId { key: Hasher::new("f20 known heads every item").part(&item.0.to_le_bytes()[..]).finish(), reads: &super::READS })
    }

    fn today(&self, fact: &[u8; 32]) -> Option<[u8; 32]> {
        (*fact == super::READS[0].fact).then_some(super::READS[0].digest)
    }

    fn crate_fact(&self, _item: ItemKey) -> Option<Read> {
        Some(Read { fact: [8; 32], digest: [9; 32] })
    }
}

#[test]
fn known_heads_preserve_body_local_lowering_and_emitted_entries() {
    let f = fixture("const N: i32 = 3;\nfn value() -> i32 { fn local() -> i32 { 7 } local() + N }\n#[unsafe(no_mangle)]\npub extern \"C-unwind\" fn main() -> i32 { value() }\n");
    let saved = produce(&f, &[("main", Want::Fn), ("value", Want::Fn)]);
    let entry = fn_named(&f, "main");
    let ids = EveryItem;
    let mut baseline = None;
    for supply in [Supply::Absent, Supply::RefuseLocals] {
        let provider = Provider::new(&saved, supply);
        let load = BorrowedLoad(&f.kept);
        let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("walk frame");
        let memory = blaze_own::Erased::new(&frame);
        let names = records(&f, &load, &frame, memory.handle());
        let b = builder();
        let mut maker: Maker<'_> = Maker::of_start(MakerOpts {
            names, entry: Some(entry), start: MakerStart::ByRule, typing: SecondCheck,
            builder: &b, known_heads: Some(KnownSource { heads: &provider, errors: blaze_own::testing::heap().shared() }), alloc: alloc(),
        });
        let mut store = MemoryStore::new_in(alloc());
        let (walk, _) = walk_records(entry, &ids, &mut store, &b, &producer(), &mut maker, alloc()).expect("the walk containing a body-local function and a constant");
        let mut rows: AVec<'_, (ItemKey, Option<CodeKey>)> = blaze_own::Vec::new_in(alloc());
        for row in 0..walk.len() {
            rows.push((walk.tabled(row), walk.key(row)));
        }
        rows.sort_unstable_by_key(|row| row.0);
        assert!(rows.iter().any(|row| blaze_ladder::front2::program::lazy::typed::is_local(row.0)), "the real local function must be reached");
        assert_eq!(provider.locals.get(), 0, "body-local heads must never be claimed by the record provider");
        if matches!(supply, Supply::Absent) {
            baseline = Some(rows);
        } else {
            assert_eq!(Some(rows), baseline, "all keyed entries and the local reachability agree with fresh lowering");
            assert!(maker.head_counts().taken > 0 && provider.hits.get() > 0);
        }
        assert!(names.records.errors().is_empty());
    }
}

#[derive(Clone, Copy, Debug)]
enum CurrentText {
    Present,
    Missing,
    Changed,
}

struct MemberLoad<'f> {
    kept: &'f Kept,
    selected: LibKey,
    text: CurrentText,
    asked: Cell<u32>,
}

impl<'n> Load<'n> for MemberLoad<'_> {
    fn names(&self, krate: usize) -> Option<&'n [u8]> {
        self.kept.names(krate)
    }

    fn items(&self, krate: usize) -> Option<&'n [u8]> {
        self.kept.items(krate)
    }

    fn file(&self, krate: usize, path: blaze_scope::package_path::PackagePath<'_>) -> Option<&'n str> {
        self.kept.file(krate, path)
    }

    fn expansion(&self, krate: usize, call: LibKey) -> Option<LoadedExpansion<'n>> {
        if call == self.selected {
            self.asked.set(self.asked.get() + 1);
            if matches!(self.text, CurrentText::Missing) {
                return None;
            }
        }
        let mut loaded = self.kept.expansion(krate, call)?;
        if call == self.selected && matches!(self.text, CurrentText::Changed) {
            loaded.text = "pub fn replaced() {}";
        }
        Some(loaded)
    }
}

#[test]
fn a_known_macro_member_still_requires_current_text_and_nested_origins() {
    let f = fixture("struct S;\nmacro_rules! inner { () => { pub fn selected() {} }; }\nmacro_rules! outer { () => { impl S { inner!(); } }; }\nouter!();\n#[unsafe(no_mangle)]\npub extern \"C-unwind\" fn main() -> i32 { 0 }\n");
    let (saved, owner, key, call) = {
        let load = BorrowedLoad(&f.kept);
        let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("macro producer frame");
        let memory = blaze_own::Erased::new(&frame);
        let names = records(&f, &load, &frame, memory.handle());
        let mut selected = None;
        assert!(names.records.each_item(0, &mut |owner, _| {
            if names.records.kind_of_item(owner) == Some(ItemKind::Impl) {
                names.records.each_member_matching(owner, &mut |_, _| MemberWant::NameOnly, &mut |member| {
                    if member.kind == ItemKind::Function && member.name == Some("selected") {
                        assert!(selected.replace((owner, member.written_by.expect("the member's inner expansion"))).is_none());
                    }
                });
            }
        }));
        let (owner, call) = selected.expect("the nested macro member");
        let key = member_key_of(checker_of(owner), MemberKind::Fn, "selected");
        (freeze(names, &[Want::Impl(owner), Want::MemberFn(owner, key)]), owner, key, call)
    };
    for text in [CurrentText::Present, CurrentText::Missing, CurrentText::Changed] {
        let provider = Provider::new(&saved, Supply::Present);
        let load = MemberLoad { kept: &f.kept, selected: call, text, asked: Cell::new(0) };
        let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("macro consumer frame");
        let memory = blaze_own::Erased::new(&frame);
        let names = records(&f, &load, &frame, memory.handle());
        let mut program = program(names, &provider);
        let result = program.head_row(key);
        assert!(load.asked.get() > 0, "known bytes must still ask for the selected member's current text");
        if matches!(text, CurrentText::Present) {
            let row = result.expect("the known member's current head").expect("the member row");
            assert!(program.head_counts().taken >= 2, "both parent and member records must be taken");
            assert_eq!((program.head_counts().attempts, program.head_counts().lowered), (0, 0));
            let body = program.function(row).text;
            let marks = blaze_ladder::front2::item_site::head_marks(names.records, owner, "selected", ItemKind::Function, body)
                .expect("the known member's current marks").expect("the inner expansion has origins");
            let expansion = f.kept.expansion(0, call).expect("the original inner expansion");
            assert_eq!(marks.text, expansion.text);
            assert!(blaze_scope::expansion_origins::offset_in(marks.text, body).is_some());
            assert!(matches!(blaze_ladder::front2::item_site::member_marks(names.records, owner, None, body),
                Err(blaze_ladder::front2::item_site::MarksLost::NotInText { .. })));
            assert!(names.records.errors().is_empty());
        } else {
            assert!(result.is_err(), "the actual head cannot use {text:?} expansion text");
            assert!(names.records.errors().iter().any(|error| match (text, error) {
                (CurrentText::Missing, SourceError::Items { error: blaze_scope::items_record::ItemsRecordError::Gone { call: Some(actual), .. }, .. }) => *actual == call,
                (CurrentText::Changed, SourceError::Items { error: blaze_scope::items_record::ItemsRecordError::Stale { call: Some(actual), .. }, .. }) => *actual == call,
                _ => false,
            }));
            let b = builder();
            let mut maker: Maker<'_> = Maker::of_start(MakerOpts {
                names, entry: Some(key), start: MakerStart::ByRule, typing: SecondCheck,
                builder: &b, known_heads: Some(KnownSource { heads: &provider, errors: blaze_own::testing::heap().shared() }), alloc: alloc(),
            });
            assert!(matches!(maker.verify(key, &[], &[]),
                Err(RunError::Front2(Front2Refusal::Lazy(LazyRefusal::RecordsVoid { tabled, .. }))) if tabled == key),
                "the current-source refusal retains RecordsVoid precedence before entry decoding");
        }
    }
}
