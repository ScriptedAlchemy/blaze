//! Durability DSTs, planted-bug controls, and native pack publication integrity tests.
//!
//! Pure `#![no_std]` over `blaze-own`, `blaze-rng`, `blaze-sha256`, and `blaze-wire`.

#[path = "native_pack.rs"]
pub(crate) mod native_pack;

use core::cell::Cell;

use blaze_own::{BTreeMap, Vec, format_in};
use blaze_rng::SplitMix64;
use blaze_wire::{Digest, Limits, PackRef, WireAlloc, pack_list};

use self::native_pack::{Pack, Value};
use super::publication::{
    Completion, Done, Effect, LogicalDir, LogicalFile, Phase, Plant, Publication, RefusalReason,
};
use super::tests::TestDir;
use super::{Store, VerifyPlant};
use crate::blob::{Kind, ObjectName, hex_in};
use crate::error::ErrorCode;

fn make_value(alloc: WireAlloc, key_seed: u8, kind: u8, payload: &[u8]) -> Value {
    let mut bytes = Vec::with_capacity_in(payload.len(), alloc);
    bytes.extend_from_slice(payload);
    Value { key: [key_seed; 32], kind, bytes }
}

fn pack_name(alloc: WireAlloc, digest: &Digest) -> ObjectName {
    let h = hex_in(digest, alloc.clone());
    let s = format_in!(alloc, "packs/{}.pack", h.as_str());
    match ObjectName::parse_kind(s.as_str(), Kind::Pack) {
        Ok(name) => name,
        Err(err) => panic!("valid pack name: {err}"),
    }
}

fn manifest_name(alloc: WireAlloc, seed: u8) -> ObjectName {
    let digest = [seed; 32];
    let h = hex_in(&digest, alloc.clone());
    let s = format_in!(alloc, "manifests/{}", h.as_str());
    match ObjectName::parse_kind(s.as_str(), Kind::Manifest) {
        Ok(name) => name,
        Err(err) => panic!("valid manifest name: {err}"),
    }
}

fn upload_pack(store: &Store, pack: &Pack<WireAlloc>) {
    let name = pack_name(store.allocator().clone(), &pack.reference.pack);
    match store.put_blob(&name, pack.bytes.as_slice()) {
        Ok(_) => {}
        Err(err) => panic!("upload pack failed: {err}"),
    }
}

// ---------------------------------------------------------------------------------------------
// Deterministic Fault-Injecting Filesystem Simulator (SimFs) for Publication DST.
// ---------------------------------------------------------------------------------------------

#[derive(Clone)]
struct SimFile {
    /// Bytes guaranteed durable across power loss.
    durable: Vec<u8, WireAlloc>,
    /// Unsynced bytes appended after the last `Sync`.
    unsynced: Vec<u8, WireAlloc>,
}

impl SimFile {
    fn new_unsynced(bytes: &[u8], alloc: WireAlloc) -> Self {
        let mut unsynced = Vec::with_capacity_in(bytes.len(), alloc.clone());
        unsynced.extend_from_slice(bytes);
        Self { durable: Vec::new_in(alloc), unsynced }
    }

    fn sync(&mut self) {
        self.durable.extend_from_slice(self.unsynced.as_slice());
        self.unsynced.clear();
    }

    fn current_bytes(&self, alloc: WireAlloc) -> Vec<u8, WireAlloc> {
        let mut out = Vec::with_capacity_in(
            self.durable.len().saturating_add(self.unsynced.len()),
            alloc,
        );
        out.extend_from_slice(self.durable.as_slice());
        out.extend_from_slice(self.unsynced.as_slice());
        out
    }

    fn crash_tear(&self, rng: &mut SplitMix64, alloc: WireAlloc) -> Vec<u8, WireAlloc> {
        let mut out = Vec::with_capacity_in(
            self.durable.len().saturating_add(self.unsynced.len()),
            alloc,
        );
        out.extend_from_slice(self.durable.as_slice());
        if !self.unsynced.is_empty() {
            // Simulate a torn power-loss write: keep a strict prefix (possibly 0 bytes) of unsynced data.
            let keep_u64 = rng.next_u64() % u64::try_from(self.unsynced.len()).unwrap_or(1);
            let keep = usize::try_from(keep_u64).unwrap_or(0);
            if let Some(prefix) = self.unsynced.get(..keep) {
                out.extend_from_slice(prefix);
            }
        }
        out
    }
}

struct PendingRename {
    from: LogicalFile,
    to: LogicalFile,
}

struct SimFs {
    alloc: WireAlloc,
    /// Live files indexed by logical handle.
    live_files: BTreeMap<u64, SimFile, WireAlloc>,
    /// Durable directory entries (name_id -> durable bytes snapshot at last SyncDir).
    durable_target: Option<Vec<u8, WireAlloc>>,
    /// Live target handle currently visible before `SyncDir`.
    live_target: Option<u64>,
    /// Pending rename not yet persisted by `SyncDir`.
    pending_rename: Option<PendingRename>,
    fail_sync: bool,
    fail_rename: bool,
    fail_dir_sync: bool,
}

impl SimFs {
    fn new(alloc: WireAlloc) -> Self {
        Self {
            live_files: BTreeMap::new_in(alloc.clone()),
            durable_target: None,
            live_target: None,
            pending_rename: None,
            fail_sync: false,
            fail_rename: false,
            fail_dir_sync: false,
            alloc,
        }
    }

    fn stage(&mut self, handle: LogicalFile, bytes: &[u8]) {
        self.live_files.insert(handle.0, SimFile::new_unsynced(bytes, self.alloc.clone()));
    }

    fn execute(&mut self, effect: Effect) -> Completion<()> {
        match effect {
            Effect::Sync { tag, file } => {
                if self.fail_sync {
                    return Completion { tag, result: Err(5) };
                }
                if let Some(f) = self.live_files.get_mut(&file.0) {
                    f.sync();
                    Completion { tag, result: Ok(Done::Synced) }
                } else {
                    Completion { tag, result: Err(2) }
                }
            }
            Effect::Rename { tag, from, to, .. } => {
                if self.fail_rename {
                    return Completion { tag, result: Err(5) };
                }
                if !self.live_files.contains_key(&from.0) {
                    return Completion { tag, result: Err(2) };
                }
                self.live_target = Some(from.0);
                self.pending_rename = Some(PendingRename { from, to });
                Completion { tag, result: Ok(Done::Renamed) }
            }
            Effect::SyncDir { tag, .. } => {
                if self.fail_dir_sync {
                    return Completion { tag, result: Err(5) };
                }
                if let Some(ren) = self.pending_rename.take() {
                    if let Some(f) = self.live_files.get(&ren.from.0) {
                        self.durable_target = Some(f.durable.clone());
                    }
                    let _ = ren.to;
                }
                Completion { tag, result: Ok(Done::DirSynced) }
            }
        }
    }

    /// Simulates a sudden power loss and reboot, returning the reopened target file bytes if any.
    fn crash_and_reopen(&mut self, rng: &mut SplitMix64, persist_pending_rename: bool) -> Option<Vec<u8, WireAlloc>> {
        let reopened = if let Some(ren) = self.pending_rename.take() {
            if persist_pending_rename {
                // Rename metadata happened to reach disk before SyncDir; file contents may still tear!
                self.live_files
                    .get(&ren.from.0)
                    .map(|f| f.crash_tear(rng, self.alloc.clone()))
            } else {
                // Rename metadata lost before SyncDir; reverts to last durably synced target.
                self.durable_target.clone()
            }
        } else if let Some(handle) = self.live_target {
            if let Some(f) = self.live_files.get(&handle) {
                if !f.unsynced.is_empty() {
                    Some(f.crash_tear(rng, self.alloc.clone()))
                } else {
                    self.durable_target.clone()
                }
            } else {
                self.durable_target.clone()
            }
        } else {
            self.durable_target.clone()
        };

        // After crash, durable state becomes what reopened.
        self.durable_target = reopened.clone();
        self.live_files.clear();
        self.live_target = None;
        if let Some(ref bytes) = reopened {
            let mut f = SimFile::new_unsynced(bytes.as_slice(), self.alloc.clone());
            f.sync();
            self.live_files.insert(999, f);
            self.live_target = Some(999);
        }
        reopened
    }

    fn current_target(&self) -> Option<Vec<u8, WireAlloc>> {
        self.live_target
            .and_then(|h| self.live_files.get(&h))
            .map(|f| f.current_bytes(self.alloc.clone()))
            .or_else(|| self.durable_target.clone())
    }
}

/// Runs a publication durability oracle over a planted or clean `Publication` state machine.
///
/// Returns `Ok(())` if every invariant holds across every crash boundary and fault injection;
/// returns `Err(reason)` when an invariant is violated.
fn run_durability_oracle(plant: Plant, alloc: WireAlloc) -> Result<(), &'static str> {
    let mut rng = SplitMix64::new(0x5EED_D00B_CAFE_1234);
    let mut fs = SimFs::new(alloc.clone());

    let mut authorized: Vec<Vec<u8, WireAlloc>, WireAlloc> = Vec::new_in(alloc.clone());
    let mut last_acked: Option<Vec<u8, WireAlloc>> = None;

    for round in 0u64..40 {
        let mut payload = Vec::with_capacity_in(48, alloc.clone());
        let tag = format_in!(alloc.clone(), "manifest-v{round:04}-whole-payload-bytes\n");
        payload.extend_from_slice(tag.as_bytes());
        authorized.push(payload.clone());

        let staging = LogicalFile(100 + round);
        let target = LogicalFile(1);
        fs.stage(staging, payload.as_slice());

        // Inject failures on selected rounds.
        fs.fail_sync = round % 9 == 1;
        fs.fail_rename = round % 9 == 2;
        fs.fail_dir_sync = round % 9 == 3;
        let crash_step = if round % 4 == 0 {
            Some((round / 4) % 4)
        } else {
            None
        };

        let (mut machine, mut next) =
            Publication::start_planted(LogicalDir(1), staging, target, round * 10 + 1, plant);

        let mut step = 0u64;
        let mut crashed = false;
        loop {
            if crash_step == Some(step) {
                let persist_rename = (round & 1) == 1;
                let reopened = fs.crash_and_reopen(&mut rng, persist_rename);
                check_reopened(&reopened, &last_acked, &authorized)?;
                if let Some(ref bytes) = reopened {
                    if Some(bytes) == Some(&payload) {
                        last_acked = reopened;
                    }
                }
                crashed = true;
                break;
            }
            let completion = fs.execute(next);
            match machine.complete(completion) {
                Ok(Some(effect)) => {
                    next = effect;
                    step = step.saturating_add(1);
                }
                Ok(None) => break,
                Err(_) => return Err("publication refused its own completion"),
            }
        }

        if !crashed {
            if machine.phase() == Phase::Acknowledged {
                // Once acknowledged, an immediate power loss MUST reopen this exact payload
                // even if pending directory metadata is lost (persist_pending_rename = false)
                // AND even if rename reached disk with unsynced file data (persist_pending_rename = true).
                for persist_rename in [false, true] {
                    let mut clone_fs = SimFs {
                        alloc: alloc.clone(),
                        live_files: fs.live_files.clone(),
                        durable_target: fs.durable_target.clone(),
                        live_target: fs.live_target,
                        pending_rename: fs
                            .pending_rename
                            .as_ref()
                            .map(|r| PendingRename { from: r.from, to: r.to }),
                        fail_sync: false,
                        fail_rename: false,
                        fail_dir_sync: false,
                    };
                    let reopened = clone_fs.crash_and_reopen(&mut rng, persist_rename);
                    if reopened.as_ref() != Some(&payload) {
                        return Err("acknowledged publication did not survive power loss");
                    }
                }
                last_acked = Some(payload.clone());
                // Commit durable snapshot for next round.
                let live = fs.current_target();
                if live.as_ref() != Some(&payload) {
                    return Err("live target does not match acknowledged payload");
                }
            } else {
                // Failed publication: crash must still reopen an authorized whole list (or None).
                let reopened = fs.crash_and_reopen(&mut rng, (round & 1) == 1);
                check_reopened(&reopened, &last_acked, &authorized)?;
            }
        }
    }
    Ok(())
}

fn check_reopened(
    reopened: &Option<Vec<u8, WireAlloc>>,
    last_acked: &Option<Vec<u8, WireAlloc>>,
    authorized: &Vec<Vec<u8, WireAlloc>, WireAlloc>,
) -> Result<(), &'static str> {
    match reopened {
        None => {
            if last_acked.is_some() {
                return Err("previously acknowledged list disappeared after crash");
            }
            Ok(())
        }
        Some(bytes) => {
            if !authorized.iter().any(|ok| ok.as_slice() == bytes.as_slice()) {
                return Err("reopened list is torn or unauthorized");
            }
            Ok(())
        }
    }
}

/// Owned mapping guard tracking live mapping count for refusal-release tests.
struct OwnedMapping<'a> {
    live: &'a Cell<usize>,
}

impl<'a> OwnedMapping<'a> {
    fn new(live: &'a Cell<usize>) -> Self {
        live.set(live.get().saturating_add(1));
        Self { live }
    }
}

impl Drop for OwnedMapping<'_> {
    fn drop(&mut self) {
        self.live.set(self.live.get().saturating_sub(1));
    }
}

fn check_refused_mapping_release(retain_bug: bool) -> Result<(), &'static str> {
    let live = Cell::new(0usize);
    let mut retained: Option<OwnedMapping<'_>> = None;

    let (mut machine, first) =
        Publication::start(LogicalDir(1), LogicalFile(10), LogicalFile(20), 7);
    let _ = first;

    // 1) Wrong tag with an owned MappedWrite must be refused AND release its mapping.
    let wrong_tag = Completion {
        tag: super::publication::Tag(999),
        result: Ok(Done::MappedWrite(OwnedMapping::new(&live))),
    };
    let refused = match machine.complete(wrong_tag) {
        Err(refused) => refused,
        Ok(_) => return Err("wrong tag completion was accepted"),
    };
    assert!(matches!(refused.reason, RefusalReason::UnexpectedTag { .. }));
    if retain_bug {
        retained = refused.mapping;
    } else {
        drop(refused);
    }
    if live.get() != 0 {
        drop(retained);
        return Err("refused completion retained its owned mapping");
    }

    // 2) Right tag but wrong completion kind (MappedWrite instead of Synced) must also release.
    let wrong_kind = Completion {
        tag: super::publication::Tag(7),
        result: Ok(Done::MappedWrite(OwnedMapping::new(&live))),
    };
    let refused = match machine.complete(wrong_kind) {
        Err(refused) => refused,
        Ok(_) => return Err("wrong kind completion was accepted"),
    };
    assert_eq!(refused.reason, RefusalReason::UnexpectedKind);
    if retain_bug {
        retained = refused.mapping;
    } else {
        drop(refused);
    }
    if live.get() != 0 {
        drop(retained);
        return Err("wrong-kind refused completion retained its owned mapping");
    }

    // 3) Advance to completion, then send a late completion with NoEffectInFlight.
    let next1 = match machine.complete(Completion::<OwnedMapping<'_>> {
        tag: super::publication::Tag(7),
        result: Ok(Done::Synced),
    }) {
        Ok(Some(e)) => e,
        _ => return Err("expected Rename effect"),
    };
    let tag2 = match next1 {
        Effect::Rename { tag, .. } => tag,
        _ => return Err("expected Rename"),
    };
    let next2 = match machine.complete(Completion::<OwnedMapping<'_>> {
        tag: tag2,
        result: Ok(Done::Renamed),
    }) {
        Ok(Some(e)) => e,
        _ => return Err("expected SyncDir effect"),
    };
    let tag3 = match next2 {
        Effect::SyncDir { tag, .. } => tag,
        _ => return Err("expected SyncDir"),
    };
    assert!(matches!(
        machine.complete(Completion::<OwnedMapping<'_>> { tag: tag3, result: Ok(Done::DirSynced) }),
        Ok(None)
    ));
    assert_eq!(machine.phase(), Phase::Acknowledged);
    assert!(machine.in_flight().is_none());

    let late = Completion {
        tag: tag3,
        result: Ok(Done::MappedWrite(OwnedMapping::new(&live))),
    };
    let refused = match machine.complete(late) {
        Err(refused) => refused,
        Ok(_) => return Err("late completion was accepted"),
    };
    assert_eq!(refused.reason, RefusalReason::NoEffectInFlight);
    drop(refused);
    if live.get() != 0 {
        return Err("late refused completion retained its owned mapping");
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Required Distributed Control Tests (exact names in .github/distributed-controls.txt).
// ---------------------------------------------------------------------------------------------

#[test]
fn first_publication_cannot_name_missing_or_corrupt_packs() {
    let alloc = blaze_own::testing::heap().shared();
    let dir = TestDir::new("missing-corrupt", alloc.clone());
    let Ok(store) = Store::open(dir.path(), Limits::DOCUMENTED, alloc.clone()) else {
        panic!("open store");
    };

    let mut vals = Vec::new_in(alloc.clone());
    vals.push(make_value(alloc.clone(), 1, 1, b"alpha"));
    let Ok(pack) = Pack::of(vals, alloc.clone()) else {
        panic!("build pack");
    };
    let manifest = manifest_name(alloc.clone(), 0x11);

    // 1) Missing pack must be refused.
    let list_bytes = pack_list::encode(core::slice::from_ref(&pack.reference), alloc.clone());
    let missing_err = store.put_list(&manifest, list_bytes.as_slice()).err();
    assert!(missing_err.is_some());
    if let Some(err) = missing_err {
        assert_eq!(err.code, ErrorCode::Refused);
    }

    // 2) Upload valid pack, then reference it with a wrong keys digest -> refused.
    upload_pack(&store, &pack);
    let mut bad_keys_ref = pack.reference.clone();
    bad_keys_ref.keys = [0xEE; 32];
    let bad_list = pack_list::encode(core::slice::from_ref(&bad_keys_ref), alloc.clone());
    let keys_err = store.put_list(&manifest, bad_list.as_slice()).err();
    assert!(keys_err.is_some());
    if let Some(err) = keys_err {
        assert_eq!(err.code, ErrorCode::Refused);
    }

    // 3) Wrong committed size -> refused.
    let mut bad_size_ref = pack.reference.clone();
    bad_size_ref.size = bad_size_ref.size.saturating_add(1);
    let bad_size_list = pack_list::encode(core::slice::from_ref(&bad_size_ref), alloc.clone());
    let size_err = store.put_list(&manifest, bad_size_list.as_slice()).err();
    assert!(size_err.is_some());
    if let Some(err) = size_err {
        assert_eq!(err.code, ErrorCode::Refused);
    }

    // 4) Valid pack reference succeeds.
    assert_eq!(store.put_list(&manifest, list_bytes.as_slice()).ok(), Some(true));
}

fn check_cross_pack_equality(plant: VerifyPlant) -> Result<(), &'static str> {
    let alloc = blaze_own::testing::heap().shared();
    let dir = TestDir::new("cross-pack", alloc.clone());
    let Ok(store) = Store::open(dir.path(), Limits::DOCUMENTED, alloc.clone()) else {
        return Err("open store failed");
    };

    let mut v_a = Vec::new_in(alloc.clone());
    v_a.push(make_value(alloc.clone(), 1, 1, b"shared-value"));
    v_a.push(make_value(alloc.clone(), 2, 1, b"only-a"));
    let Ok(pack_a) = Pack::of(v_a, alloc.clone()) else {
        return Err("pack_a failed");
    };

    let mut v_b_same = Vec::new_in(alloc.clone());
    v_b_same.push(make_value(alloc.clone(), 1, 1, b"shared-value"));
    v_b_same.push(make_value(alloc.clone(), 3, 1, b"only-b"));
    let Ok(pack_b_same) = Pack::of(v_b_same, alloc.clone()) else {
        return Err("pack_b_same failed");
    };

    let mut v_b_diff = Vec::new_in(alloc.clone());
    v_b_diff.push(make_value(alloc.clone(), 1, 1, b"DIFFERENT-value"));
    v_b_diff.push(make_value(alloc.clone(), 3, 1, b"only-b"));
    let Ok(pack_b_diff) = Pack::of(v_b_diff, alloc.clone()) else {
        return Err("pack_b_diff failed");
    };

    upload_pack(&store, &pack_a);
    upload_pack(&store, &pack_b_same);
    upload_pack(&store, &pack_b_diff);

    // Equal mirrored value across two packs is allowed.
    let ok_refs = [pack_a.reference.clone(), pack_b_same.reference.clone()];
    if store.verify_manifest_packs_planted(&[], &ok_refs, plant).is_err() {
        return Err("equal mirrored values across packs were unexpectedly refused");
    }

    // Conflicting mirrored value across two packs MUST be refused.
    let conflict_refs = [pack_a.reference.clone(), pack_b_diff.reference.clone()];
    if store.verify_manifest_packs_planted(&[], &conflict_refs, plant).is_ok() {
        return Err("conflicting mirrored values across packs were accepted");
    }
    Ok(())
}

/// The invariant: first-writer manifest publication requires equal values for keys
/// mirrored across distinct packs, refusing any manifest whose packs disagree on a key.
#[test]
fn first_writer_requires_equal_values_for_keys_mirrored_across_packs() {
    assert_eq!(check_cross_pack_equality(VerifyPlant::None), Ok(()));
}

#[test]
fn the_oracle_catches_removing_cross_pack_comparison() {
    assert!(
        check_cross_pack_equality(VerifyPlant::SkipCrossPackComparison).is_err(),
        "oracle must catch removing cross-pack value comparison"
    );
}

fn check_conditional_cas_comparison(plant: VerifyPlant) -> Result<(), &'static str> {
    let alloc = blaze_own::testing::heap().shared();
    let dir = TestDir::new("cas-compare", alloc.clone());
    let Ok(store) = Store::open(dir.path(), Limits::DOCUMENTED, alloc.clone()) else {
        return Err("open store failed");
    };

    let mut old_vals = Vec::new_in(alloc.clone());
    old_vals.push(make_value(alloc.clone(), 1, 1, b"original-1"));
    old_vals.push(make_value(alloc.clone(), 2, 2, b"original-2"));
    let Ok(old_pack) = Pack::of(old_vals, alloc.clone()) else {
        return Err("old_pack failed");
    };

    let mut bad_new_vals = Vec::new_in(alloc.clone());
    bad_new_vals.push(make_value(alloc.clone(), 1, 1, b"original-1"));
    bad_new_vals.push(make_value(alloc.clone(), 2, 2, b"mutated!!2"));
    let Ok(bad_new_pack) = Pack::of(bad_new_vals, alloc.clone()) else {
        return Err("bad_new_pack failed");
    };

    upload_pack(&store, &old_pack);
    upload_pack(&store, &bad_new_pack);

    let old_refs = [old_pack.reference.clone()];
    let bad_refs = [bad_new_pack.reference.clone()];
    if store.verify_manifest_packs_planted(&old_refs, &bad_refs, plant).is_ok() {
        return Err("mutated value during CAS repair was accepted");
    }
    Ok(())
}

#[test]
fn the_oracle_catches_the_comparison_bypass_plant() {
    assert_eq!(check_conditional_cas_comparison(VerifyPlant::None), Ok(()));
    assert!(
        check_conditional_cas_comparison(VerifyPlant::SkipOldPackComparison).is_err(),
        "oracle must catch bypassing old-vs-new pack value comparison"
    );
}

#[test]
fn conditional_publication_preserves_comparison_and_whole_file_integrity() {
    let alloc = blaze_own::testing::heap().shared();
    let dir = TestDir::new("conditional-pub", alloc.clone());
    let Ok(store) = Store::open(dir.path(), Limits::DOCUMENTED, alloc.clone()) else {
        panic!("open store");
    };

    let mut v1 = Vec::new_in(alloc.clone());
    v1.push(make_value(alloc.clone(), 1, 1, b"key-one"));
    v1.push(make_value(alloc.clone(), 2, 1, b"key-two"));
    let Ok(p1) = Pack::of(v1, alloc.clone()) else {
        panic!("p1");
    };

    let mut v2 = Vec::new_in(alloc.clone());
    v2.push(make_value(alloc.clone(), 1, 1, b"key-one"));
    v2.push(make_value(alloc.clone(), 2, 1, b"key-two"));
    v2.push(make_value(alloc.clone(), 3, 2, b"key-three"));
    let Ok(p2) = Pack::of(v2, alloc.clone()) else {
        panic!("p2");
    };

    let mut v_drop = Vec::new_in(alloc.clone());
    v_drop.push(make_value(alloc.clone(), 1, 1, b"key-one"));
    let Ok(p_drop) = Pack::of(v_drop, alloc.clone()) else {
        panic!("p_drop");
    };

    upload_pack(&store, &p1);
    upload_pack(&store, &p2);
    upload_pack(&store, &p_drop);

    let manifest = manifest_name(alloc.clone(), 0x42);
    let list1 = pack_list::encode(core::slice::from_ref(&p1.reference), alloc.clone());
    let list2 = pack_list::encode(core::slice::from_ref(&p2.reference), alloc.clone());
    let list_drop = pack_list::encode(core::slice::from_ref(&p_drop.reference), alloc.clone());

    assert_eq!(store.put_list(&manifest, list1.as_slice()).ok(), Some(true));
    // Second unconditional put must not overwrite.
    assert_eq!(store.put_list(&manifest, list2.as_slice()).ok(), Some(false));

    // Wrong If-Match digest returns Ok(false) without modifying manifest.
    let wrong_etag = [0xAA; 32];
    assert_eq!(store.repair_list(&manifest, Some(wrong_etag), list2.as_slice()).ok(), Some(false));

    let etag1 = blaze_sha256::digest(list1.as_slice());
    // Dropping a key during CAS repair must be refused.
    let drop_err = store.repair_list(&manifest, Some(etag1), list_drop.as_slice()).err();
    assert!(drop_err.is_some());
    if let Some(err) = drop_err {
        assert_eq!(err.code, ErrorCode::Refused);
    }

    // Superset repair with matching If-Match succeeds.
    assert_eq!(store.repair_list(&manifest, Some(etag1), list2.as_slice()).ok(), Some(true));
    let Ok(Some(reopened)) =
        store.read_small(&manifest, u64::from(Limits::DOCUMENTED.request), alloc.clone())
    else {
        panic!("read repaired manifest");
    };
    assert_eq!(reopened.as_slice(), list2.as_slice());
}

#[test]
fn native_partitions_preserve_values_through_publication_faults_and_reopen() {
    let alloc = blaze_own::testing::heap().shared();
    let dir = TestDir::new("partitions-reopen", alloc.clone());
    let Ok(store) = Store::open(dir.path(), Limits::DOCUMENTED, alloc.clone()) else {
        panic!("open store");
    };

    let mut all_values = Vec::new_in(alloc.clone());
    for i in 1u8..=8 {
        let mut payload = Vec::new_in(alloc.clone());
        for j in 0u8..32 {
            payload.push(i ^ j);
        }
        all_values.push(Value { key: [i; 32], kind: i % 4, bytes: payload });
    }

    let Ok(packs) = Pack::batches_with_limits(all_values.clone(), 3, usize::MAX, alloc.clone()) else {
        panic!("partition packs");
    };
    assert_eq!(packs.len(), 3);

    let mut refs: Vec<PackRef, WireAlloc> = Vec::new_in(alloc.clone());
    for p in &packs {
        refs.push(p.reference.clone());
    }
    refs.sort_unstable_by(|a, b| a.pack.cmp(&b.pack));
    let manifest = manifest_name(alloc.clone(), 0x77);
    let list_bytes = pack_list::encode(refs.as_slice(), alloc.clone());

    // Publishing before all partition packs are uploaded fails cleanly.
    upload_pack(&store, &packs[0]);
    assert!(store.put_list(&manifest, list_bytes.as_slice()).is_err());

    // Upload remaining partitions and publish.
    for p in packs.iter().skip(1) {
        upload_pack(&store, p);
    }
    assert_eq!(store.put_list(&manifest, list_bytes.as_slice()).ok(), Some(true));

    // Reopen a fresh Store instance from the same directory and verify every value round-trips.
    let Ok(reopened_store) = Store::open(dir.path(), Limits::DOCUMENTED, alloc.clone()) else {
        panic!("reopen store");
    };
    let Ok(saved_list) = reopened_store.read_small(
        &manifest,
        u64::from(Limits::DOCUMENTED.request),
        alloc.clone(),
    ) else {
        panic!("read manifest after reopen");
    };
    let Some(saved_list) = saved_list else {
        panic!("manifest missing after reopen");
    };
    let Ok(decoded_refs) = pack_list::decode(saved_list.as_slice(), alloc.clone()) else {
        panic!("decode reopened list");
    };
    let mut recovered = Vec::new_in(alloc.clone());
    for pref in &decoded_refs {
        let pname = pack_name(alloc.clone(), &pref.pack);
        let Ok(Some(pbytes)) =
            reopened_store.read_small(&pname, Limits::DOCUMENTED.blob, alloc.clone())
        else {
            panic!("read pack after reopen");
        };
        let Ok(views) = native_pack::read(pbytes.as_slice(), pref, alloc.clone()) else {
            panic!("verify pack after reopen");
        };
        for view in views {
            let mut copy = Vec::with_capacity_in(view.bytes.len(), alloc.clone());
            copy.extend_from_slice(view.bytes);
            recovered.push(Value { key: view.key, kind: view.kind, bytes: copy });
        }
    }
    recovered.sort_unstable_by(|a, b| a.key.cmp(&b.key));
    assert_eq!(recovered, all_values);
}

#[test]
fn failed_syncs_rename_and_every_crash_boundary_reopen_authorized_whole_lists() {
    let alloc = blaze_own::testing::heap().shared();
    assert_eq!(run_durability_oracle(Plant::None, alloc), Ok(()));
}

#[test]
fn the_oracle_catches_missing_file_sync_with_torn_appends() {
    let alloc = blaze_own::testing::heap().shared();
    assert!(
        run_durability_oracle(Plant::NoFileSync, alloc).is_err(),
        "oracle must catch torn unsynced bytes when FileSync is omitted"
    );
}

#[test]
fn the_oracle_catches_acknowledgement_before_directory_sync() {
    let alloc = blaze_own::testing::heap().shared();
    assert!(
        run_durability_oracle(Plant::EarlyAck, alloc).is_err(),
        "oracle must catch acknowledging before directory sync"
    );
}

#[test]
fn refused_publication_completions_release_owned_mappings() {
    assert_eq!(check_refused_mapping_release(false), Ok(()));
}

#[test]
fn the_oracle_catches_a_retained_refused_mapping() {
    assert!(
        check_refused_mapping_release(true).is_err(),
        "oracle must catch a refused completion retaining its owned mapping"
    );
}
