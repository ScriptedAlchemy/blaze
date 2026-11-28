use super::*;
use crate::engine_seeds::export_heads_tests::{CRATE, Fixture, NAMES, TOOL, fixture, fixture_for};
use crate::store::{CommandLent, CommandStore, Held, StoreKept};
use blaze_cas::plain::Settings;
use blaze_drive::profile::Profile;
use bstd::io::{Io, LentDriver, Limits, SimWorld};
use bstd::mem::TASK_FRAME;
use bstd::test::BUDGET;

const ROOT: &str = "engine-dependency-heads.publication-test";

fn closed() -> CrateAt {
    CrateAt { prefix: CRATE.prefix(), key: CRATE, names_at: Some(NAMES), member: false, closed: true, toolchain: false }
}

fn fresh(fixture: &Fixture) -> FreshHead<'_> {
    FreshHead { cell: fixture.cell, krate: Some(CRATE), bytes: &fixture.frozen.bytes }
}

fn stage<'h>(heads: impl IntoIterator<Item = FreshHead<'h>>, crates: &[CrateAt]) -> Result<Batch<'h, 'static>, Invalid> {
    Batch::stage(StageOpts { heads, crates, names: &|at| at.names_at, tool: TOOL, alloc: blaze_own::testing::heap().alloc() })
}

fn with_store<R>(io: &Io, f: impl for<'m, 'r, 's> FnOnce(&Cx, &'s mut CommandStore<'m, 'r, LentDriver>, &'m CommandLent) -> R) -> R {
    with_store_at(io, crate::store::Facts { boot: Some(1), nonce: 713 }, false, |cx, store, lent, _| f(cx, store, lent))
}

fn with_store_at<R>(io: &Io, facts: crate::store::Facts, durable: bool, f: impl for<'m, 'r, 's> FnOnce(&Cx, &'s mut CommandStore<'m, 'r, LentDriver>, &'m CommandLent, &crate::store::Home) -> R) -> R {
    let cx = io.cx(BUDGET, TASK_FRAME).expect("the publication context");
    let home = crate::store::Home::of(&cx, &io.need_fs().expect("filesystem"), Some("/h")).expect("home");
    let heap = blaze_own::testing::heap();
    let erased = blaze_own::Erased::new(cx.frame());
    let clock = io.need_clock().expect("clock");
    let err = io.need_err().expect("stderr");
    let settings = Settings { durable, ..Settings::default() };
    let lent = home.open_store(&settings, facts, heap.alloc()).expect("store");
    let mut store = CommandStore::new(&cx, &lent, false, 0, None, clock, &err, erased.handle()).expect("command store");
    f(&cx, &mut store, &lent, &home)
}

#[test]
fn every_frozen_cell_and_equal_key_value_are_checked_before_publication() {
    let first = fixture();
    let changed = fixture_for("#![no_core]\npub fn sample() -> u8 { 1 }\n", "sample");
    assert_eq!(first.cell, changed.cell, "a signature edit retains the item identity");
    assert_ne!(first.frozen.bytes, changed.frozen.bytes, "the real freezer records the changed signature");
    assert!(matches!(stage([fresh(&first), fresh(&changed)], &[closed()]), Err(Invalid::Unequal { cell, key })
        if cell == first.cell && key == dependency_key(NAMES, cell, TOOL)));
    for (cell, bytes) in [(first.cell, &b"not a frozen head"[..]), (Want::Head(first.cell.key()), first.frozen.bytes.as_slice())] {
        assert!(matches!(stage([fresh(&first), FreshHead { cell, krate: Some(CRATE), bytes }], &[closed()]), Err(Invalid::Frozen { cell: found, .. }) if found == cell));
    }
    let other_crate = CrateItemKey(ContentId([83; 32]));
    assert!(matches!(stage([FreshHead { cell: first.cell, krate: Some(other_crate), bytes: &first.frozen.bytes }], &[closed()]), Err(Invalid::Origin { cell, krate })
        if cell == first.cell && krate == other_crate));
}

#[test]
fn publication_uses_the_full_origin_and_existing_closed_and_names_rules() {
    let fixture = fixture();
    let mut open = closed();
    open.closed = false;
    // `member == false` proves transitive openness is enough to exclude a dependency.
    let batch = stage([fresh(&fixture)], &[open]).expect("open source head is counted");
    assert_eq!((batch.puts.len(), batch.not_put.member), (0, 1));
    let mut missing_names = closed();
    missing_names.names_at = None;
    let batch = stage([fresh(&fixture)], &[missing_names]).expect("missing names is counted");
    assert_eq!((batch.puts.len(), batch.not_put.no_names), (0, 1));
    let mut other_origin = CRATE;
    *other_origin.0.0.last_mut().expect("a full crate key") ^= 1;
    assert_eq!(other_origin.prefix(), CRATE.prefix());
    let batch = stage([FreshHead { cell: fixture.cell, krate: Some(other_origin), bytes: &fixture.frozen.bytes }], &[closed()]).expect("unlisted full origin is counted");
    assert_eq!((batch.puts.len(), batch.not_put.unlisted), (0, 1), "an equal prefix cannot substitute for the full source crate");
    let batch = stage([FreshHead { cell: fixture.cell, krate: None, bytes: &fixture.frozen.bytes }], &[closed()]).expect("no source crate is counted");
    assert_eq!((batch.puts.len(), batch.not_put.unlisted), (0, 1));
    // The ready request obtains names through its validated callback, not CrateAt.names_at.
    missing_names.toolchain = true;
    let batch = Batch::stage(StageOpts {
        heads: [fresh(&fixture)], crates: &[missing_names], names: &|_| Some(NAMES), tool: TOOL, alloc: blaze_own::testing::heap().alloc(),
    }).expect("the ready request's names are authoritative");
    let mut toolchain = batch.toolchain_keys();
    assert_eq!(toolchain.next(), Some(dependency_key(NAMES, fixture.cell, TOOL)));
    assert_eq!(toolchain.next(), None);
}

#[test]
fn equal_duplicates_publish_once_and_renew_the_existing_union_without_probes() {
    let fixture = fixture();
    let io = Io::simulated(SimWorld::new(713, Limits::USUAL));
    with_store(&io, |cx, store, lent| {
        let initial = Key(ContentId([1; 32]));
        store.put(cx, initial, b"an earlier dependency").expect("initial head");
        store.pin(ROOT, &[initial]).expect("initial root");
        lent.commit().expect("initial commit");
        let prior = RootState::read(store, ROOT).expect("prior root");
        let batch = stage([fresh(&fixture), fresh(&fixture)], &[closed()]).expect("equal duplicate cells");
        let mut ledger = Ledger::new();
        let counts = batch.commit(CommitOpts { cx, store, root: ROOT, prior, added: &[], stage: Stage::Heads, ledger: &mut ledger }).expect("publication");
        assert_eq!((counts.frozen, counts.put, counts.duplicates, counts.listed, counts.retained), (2, 1, 1, 1, 2));
        lent.commit().expect("published commit");
        let key = dependency_key(NAMES, fixture.cell, TOOL);
        let bytes = store.get(cx, key).expect("published lookup").expect("published head").bytes;
        assert_eq!(super::super::dependency_head(bytes, NAMES, fixture.cell).expect("canonical reader"), fixture.frozen.bytes.as_slice());
        let root = store.named(ROOT).expect("retained root").expect("root exists");
        assert!(root.chunks_exact(KEY_BYTES).any(|bytes| bytes == initial.0.0));
        assert!(root.chunks_exact(KEY_BYTES).any(|bytes| bytes == key.0.0));
        let prior = RootState::read(store, ROOT).expect("warm root");
        let probes = lent.put_tally().disk.root_probes;
        let batch = stage(core::iter::empty(), &[closed()]).expect("no new cells");
        let warm = batch.commit(CommitOpts { cx, store, root: ROOT, prior, added: &[], stage: Stage::Heads, ledger: &mut ledger }).expect("warm renewal");
        assert_eq!((warm.put, warm.listed, warm.retained), (0, 2, 2));
        assert_eq!(lent.put_tally().disk.root_probes, probes, "warm publication never probes the root's records");
    });
}

#[test]
fn an_existing_unequal_later_record_refuses_the_batch_before_its_first_put() {
    let first = fixture();
    let other = fixture_for("#![no_core]\npub fn other() {}\n", "other");
    let first_key = dependency_key(NAMES, first.cell, TOOL);
    let other_key = dependency_key(NAMES, other.cell, TOOL);
    let (earlier, earlier_key, later, later_key) = if first_key < other_key { (&first, first_key, &other, other_key) } else { (&other, other_key, &first, first_key) };
    let io = Io::simulated(SimWorld::new(714, Limits::USUAL));
    with_store(&io, |cx, store, lent| {
        store.put(cx, later_key, b"a conflicting immutable value").expect("plant the conflicting later key");
        lent.commit().expect("conflict commit");
        let prior = RootState::read(store, ROOT).expect("initially no root");
        let batch = stage([fresh(earlier), fresh(later)], &[closed()]).expect("valid cells");
        let mut ledger = Ledger::new();
        let result = batch.commit(CommitOpts { cx, store, root: ROOT, prior, added: &[], stage: Stage::Heads, ledger: &mut ledger });
        assert!(matches!(result, Err(CliError::HeadPublication(Invalid::Existing { cell, key })) if cell == later.cell && key == later_key));
        assert!(store.get(cx, earlier_key).expect("earlier lookup").is_none(), "the earlier sorted cell was never put");
        assert!(store.named(ROOT).expect("no successful root").is_none());
    });
}

#[derive(Clone, Copy)]
enum Fault {
    None,
    MalformedRoot,
    MissingRoot,
    Pin,
}

/// The actual simulated CAS receives all non-planted operations. Only the named root seam
/// is changed, so a refusal cannot be explained by skipped puts or an alternate writer.
struct RootFault<'s, 'm> {
    store: &'s mut dyn Store<'m>,
    fault: Fault,
    puts: u32,
    single_keeps: u32,
    batch_keeps: u32,
}

impl<'m> Store<'m> for RootFault<'_, 'm> {
    fn named(&self, name: &str) -> Result<Option<&'m [u8]>, CliError> {
        match self.fault {
            Fault::MalformedRoot => Ok(Some(&[0; 33])),
            Fault::MissingRoot => Ok(None),
            Fault::None | Fault::Pin => self.store.named(name),
        }
    }
    fn pin(&mut self, name: &str, keys: &[Key]) -> Result<(), CliError> {
        match self.fault {
            Fault::Pin => Err(CliError::Text { what: "planted dependency root write failure" }),
            _ => self.store.pin(name, keys),
        }
    }
    fn renew_one(&mut self, name: &str) -> Result<bool, CliError> {
        match self.fault {
            Fault::MissingRoot => Ok(false),
            _ => self.store.renew_one(name),
        }
    }
    fn get(&mut self, _cx: &Cx, _key: Key) -> Result<Option<Held<'m>>, CliError> {
        Err(CliError::Text { what: "publication must never ask a possibly fetching record lookup" })
    }
    fn get_local(&mut self, key: Key) -> Result<Option<Held<'m>>, CliError> { self.store.get_local(key) }
    fn put(&mut self, cx: &Cx, key: Key, bytes: &[u8]) -> Result<(), CliError> { self.put_parts(cx, key, &[bytes]) }
    fn put_parts(&mut self, cx: &Cx, key: Key, parts: &[&[u8]]) -> Result<(), CliError> {
        self.puts += 1;
        self.store.put_parts(cx, key, parts)
    }
    fn put_record(&mut self, cx: &Cx, key: Key, tag: blaze_keyed::kind::Tag, owner: &[&[u8]], bytes: &[u8]) -> Result<blaze_keyed::store::Put, CliError> { self.store.put_record(cx, key, tag, owner, bytes) }
    fn index_beside(&mut self, cx: &Cx, owner: Key, keys: &[Key]) -> Result<(), CliError> { self.store.index_beside(cx, owner, keys) }
    fn map_all(&mut self, cx: &Cx) -> Result<(), CliError> { self.store.map_all(cx) }
    fn after_answer(&mut self, cx: &Cx, profile: &mut Profile) -> Result<(), CliError> { self.store.after_answer(cx, profile) }
    fn public_crates(&mut self, subjects: &[ContentId]) { self.store.public_crates(subjects); }
    fn toolchain_crates(&mut self, subjects: &[ContentId]) { self.store.toolchain_crates(subjects); }
    fn keep_with_toolchain(&mut self, key: Key) -> Result<(), CliError> {
        self.single_keeps += 1;
        self.store.keep_with_toolchain(key)
    }
    fn keep_with_toolchain_batch(&mut self, keys: &[Key]) -> Result<(), CliError> {
        self.batch_keeps += 1;
        self.store.keep_with_toolchain_batch(keys)
    }
    fn kept(&self) -> StoreKept { self.store.kept() }
}

#[test]
fn root_preflight_and_write_failures_cannot_report_success_or_silently_replace_a_root() {
    let fixture = fixture();
    for fault in [Fault::MalformedRoot, Fault::MissingRoot, Fault::Pin] {
        let io = Io::simulated(SimWorld::new(715, Limits::USUAL));
        with_store(&io, |cx, store, lent| {
            let old = Key(ContentId([2; 32]));
            store.put(cx, old, b"an existing retained head").expect("existing head");
            store.pin(ROOT, &[old]).expect("existing root");
            lent.commit().expect("existing commit");
            let prior = RootState::read(store, ROOT).expect("valid initial root");
            let batch = stage([fresh(&fixture)], &[closed()]).expect("valid new head");
            let mut ledger = Ledger::new();
            let mut planted = RootFault { store, fault, puts: 0, single_keeps: 0, batch_keeps: 0 };
            let result = batch.commit(CommitOpts { cx, store: &mut planted, root: ROOT, prior, added: &[], stage: Stage::Heads, ledger: &mut ledger });
            match fault {
                Fault::None => panic!("the fault history always plants a root failure"),
                Fault::MalformedRoot => {
                    assert!(matches!(result, Err(CliError::HeadPublication(Invalid::RootBytes { bytes: 33 }))));
                    assert_eq!(planted.puts, 0, "bad root bytes stop before the first put");
                }
                Fault::MissingRoot => {
                    assert!(matches!(result, Err(CliError::HeadPublication(Invalid::RootExpired))));
                    assert_eq!(planted.puts, 0, "a missing expected root is not replaced by this batch");
                    let warm = stage(core::iter::empty(), &[closed()]).expect("empty warm batch");
                    assert!(matches!(warm.commit(CommitOpts { cx, store: &mut planted, root: ROOT, prior, added: &[], stage: Stage::Heads, ledger: &mut ledger }), Err(CliError::HeadPublication(Invalid::RootExpired))));
                    assert_eq!(planted.puts, 0, "failed O(1) renewal does not rebuild or probe the root");
                }
                Fault::Pin => {
                    assert!(matches!(result, Err(CliError::Text { what: "planted dependency root write failure" })));
                    assert_eq!(planted.puts, 1, "a later I/O failure is fatal, not a claim that immutable puts were rolled back");
                }
            }
            let root = planted.store.named(ROOT).expect("the actual root").expect("old root retained");
            assert_eq!(root, old.0.0.as_slice(), "no failed branch replaces the old root");
        });
    }
}

#[test]
fn toolchain_heads_join_the_existing_queue_once_and_keep_the_same_canonical_records() {
    let first = fixture();
    let second = fixture_for("#![no_core]\npub fn other() {}\n", "other");
    let first_key = dependency_key(NAMES, first.cell, TOOL);
    let second_key = dependency_key(NAMES, second.cell, TOOL);
    let mut krate = closed();
    krate.toolchain = true;
    let io = Io::simulated(SimWorld::new(716, Limits::USUAL));
    with_store(&io, |cx, store, lent| {
        // A prior user of the single-record seam may already have queued one of the keys.
        store.keep_with_toolchain(first_key).expect("an already queued toolchain head");
        let prior = RootState::read(store, ROOT).expect("the initial root");
        let batch = stage([fresh(&first), fresh(&second), fresh(&first)], &[krate]).expect("the completed toolchain heads");
        let mut ledger = Ledger::new();
        let mut counted = RootFault { store, fault: Fault::None, puts: 0, single_keeps: 0, batch_keeps: 0 };
        let counts = batch.commit(CommitOpts { cx, store: &mut counted, root: ROOT, prior, added: &[], stage: Stage::Heads, ledger: &mut ledger }).expect("one batch publication");
        assert_eq!((counts.put, counts.duplicates, counted.single_keeps, counted.batch_keeps), (2, 1, 0, 1));
        drop(counted);
        store.keep_toolchain(blaze_toolchain::PINNED, &[]).expect("the existing answer root write");
        lent.commit().expect("the retained heads commit");
        let name = crate::store::roots::RootName::toolchain(blaze_toolchain::PINNED);
        let indexes = store.named(name.as_str()).expect("the toolchain root").expect("the root was written");
        let mut kept = blaze_own::Vec::new_in(blaze_own::testing::heap().alloc());
        for index in indexes.as_chunks::<KEY_BYTES>().0 {
            let rows = lent.get(&blaze_cas::Key(ContentId(*index))).expect("the toolchain index").expect("its records are present");
            kept.extend(rows.bytes.as_chunks::<KEY_BYTES>().0.iter().map(|key| Key(ContentId(*key))));
        }
        kept.sort_unstable();
        let mut expected = [first_key, second_key];
        expected.sort_unstable();
        assert_eq!(kept.as_slice(), expected.as_slice(), "existing and batched queue admission retain each canonical record once");
        for one in [&first, &second] {
            let key = dependency_key(NAMES, one.cell, TOOL);
            let held = store.get_local(key).expect("the retained local record").expect("the head exists");
            assert_eq!(super::super::dependency_head(held.bytes, NAMES, one.cell).expect("canonical record"), one.frozen.bytes.as_slice());
        }
    });
}

#[test]
fn the_root_shape_bound_is_checked_before_allocating_its_union() {
    assert!(matches!(RootState::of(Some(&[0; 31])), Err(Invalid::RootBytes { bytes: 31 })));
    let mut bytes = blaze_own::Vec::new_in(blaze_own::testing::heap().alloc());
    bytes.resize((MOST_CELLS + 1) * KEY_BYTES, 0);
    assert!(matches!(RootState::of(Some(&bytes)), Err(Invalid::PastCap { what: "the stored dependency root", cap: MOST_CELLS })));
}

fn publish_one<'m>(cx: &Cx, store: &mut dyn Store<'m>, fixture: &Fixture) -> Result<Published, CliError> {
    let prior = RootState::read(store, ROOT)?;
    let batch = stage([fresh(fixture)], &[closed()]).map_err(CliError::HeadPublication)?;
    batch.commit(CommitOpts { cx, store, root: ROOT, prior, added: &[], stage: Stage::Heads, ledger: &mut Ledger::new() })
}

/// An acknowledged durable publication must survive a real simulated power loss. A normal
/// incremental commit is deliberately not treated as power-loss acknowledgement: after a
/// cut it may be absent, but any root that remains must name exact, complete head records.
fn publication_history(tape: &blaze_sim::Tape, omit_publication: bool) -> Result<(), blaze_sim::Broke> {
    use blaze_sim::Broke;

    let first = fixture();
    let second = fixture_for("#![no_core]\npub fn other() {}\n", "other");
    let seed = tape.next_u64("publication filesystem seed");
    let phase = tape.int_in(0..=3u8, "stop before staging, after staging, after publication, after commit");
    let fail_syncs = tape.coin("fail the publication directory's syncs");
    let durable = tape.coin("explicit durable commit or ordinary incremental commit");
    let io = Io::simulated(SimWorld::new(seed, Limits::USUAL));
    let facts = |boot, nonce| crate::store::Facts { boot: Some(boot), nonce };

    // The base is protected by the existing caller-selected durable store mode, not by an
    // assumption that the ordinary incremental commit synchronizes every sector.
    with_store_at(&io, facts(1, 714), true, |cx, store, lent, _| {
        publish_one(cx, store, &first).expect("the base publication");
        lent.commit().expect("the base's actual durable commit");
    });
    tape.reached("acknowledged durable base");

    let mut acknowledged = false;
    with_store_at(&io, facts(1, 715), durable, |cx, store, lent, home| {
        if phase == 0 {
            tape.reached("power loss before staging");
            return;
        }
        let prior = RootState::read(store, ROOT).expect("the base root before interruption");
        let batch = stage([fresh(&second)], &[closed()]).expect("the second valid source freeze");
        tape.reached("complete batch staged");
        if phase == 1 {
            return;
        }
        if fail_syncs {
            // The existing simulator fails real Sync/Barrier/SyncDir effects under this home.
            let name = home.dir().name().name().expect("the home has a name").clone();
            io.with_sim(|sim| sim.fail_syncs(name)).expect("the simulated sync fault is installed");
            tape.reached("real sync fault installed");
        }
        let published = if omit_publication {
            // Plant only the missing publication half; the caller still commits its answer.
            Ok(())
        } else {
            batch.commit(CommitOpts { cx, store, root: ROOT, prior, added: &[], stage: Stage::Heads, ledger: &mut Ledger::new() }).map(|_| ())
        };
        if published.is_err() {
            assert!(fail_syncs, "only the drawn IO fault may refuse this valid batch");
            tape.reached("publication stopped by a real IO fault");
            return;
        }
        tape.reached("publication accepted before the commit");
        if phase == 3 {
            let committed = lent.commit();
            if committed.is_ok() {
                acknowledged = durable;
                if durable {
                    tape.reached("durable publication acknowledged");
                } else {
                    tape.reached("incremental commit accepted without power-loss acknowledgement");
                }
            } else {
                assert!(fail_syncs, "only the drawn sync fault may stop this commit");
                tape.reached("commit stopped by a real sync fault");
            }
            if fail_syncs && durable && !omit_publication {
                assert!(committed.is_err(), "the drawn fault must reach the commit's real sync or barrier");
            }
        }
    });
    assert!(io.crash(), "this case loses the simulated machine's unsynced sectors");
    tape.reached("power loss and reopen");

    let key_of = |one: &Fixture| dependency_key(NAMES, one.cell, TOOL);
    let inspect = |cx: &Cx, store: &mut dyn Store<'_>, must_have_base_root: bool, must_have_second: bool| -> Result<(), Broke> {
        let root = store.named(ROOT).expect("read the restored dependency root");
        // D200 drops a whole newer root if an unsynced member was lost; it does not restore
        // an older root of the same name. Only an acknowledged durable union must remain.
        if must_have_second && root.is_none() {
            return Err(Broke::Oracle("acknowledged publication lost a head".into()));
        }
        if must_have_base_root && root.is_none() {
            return Err(Broke::Oracle("the unchanged durable base root was lost".into()));
        }
        for one in [&first, &second] {
            let key = key_of(one);
            let rooted = root.is_some_and(|bytes| bytes.chunks_exact(KEY_BYTES).any(|bytes| bytes == key.0.0));
            let required = (root.is_some() && one.cell == first.cell) || must_have_second;
            if required && !rooted {
                return Err(Broke::Oracle("acknowledged publication lost a head".into()));
            }
            if let Some(held) = store.get(cx, key).expect("read after the power loss") {
                if super::super::dependency_head(held.bytes, NAMES, one.cell).ok() != Some(one.frozen.bytes.as_slice()) {
                    return Err(Broke::Oracle("restored head bytes differ from the real frozen source".into()));
                }
            } else if rooted {
                return Err(Broke::Oracle("a restored dependency root names an absent head".into()));
            } else if one.cell == first.cell {
                return Err(Broke::Oracle("the earlier durable base head's bytes were lost".into()));
            }
        }
        Ok(())
    };
    with_store_at(&io, facts(2, 716), true, |cx, store, lent, _| {
        inspect(cx, store, phase <= 1, acknowledged)?;
        if !omit_publication {
            let prior = RootState::read(store, ROOT).expect("the restored root's shape");
            let batch = stage([fresh(&first), fresh(&second)], &[closed()]).expect("the next complete export's real source heads");
            batch.commit(CommitOpts { cx, store, root: ROOT, prior, added: &[], stage: Stage::Heads, ledger: &mut Ledger::new() })
                .expect("the next command completes the publication");
        }
        lent.commit().expect("the completing command's durable commit");
        inspect(cx, store, true, true)
    })?;
    tape.reached("the next command converges to the complete root");
    assert!(io.crash());
    with_store_at(&io, facts(3, 717), true, |cx, store, _, _| inspect(cx, store, true, true))?;
    tape.reached("the repaired publication survives another power loss");
    Ok(())
}

#[test]
fn published_heads_are_whole_after_drawn_stops_sync_failures_and_power_loss() {
    blaze_sim::check(blaze_sim::Config::cases(64).must_reach(&[
        "commit stopped by a real sync fault",
        "durable publication acknowledged",
        "incremental commit accepted without power-loss acknowledgement",
        "the repaired publication survives another power loss",
    ]), |tape| publication_history(tape, false));
}

#[test]
fn the_crash_oracle_detects_omitted_head_publication() {
    blaze_sim::control(blaze_sim::Config::cases(32), false, true, "acknowledged publication lost a head", publication_history);
}
