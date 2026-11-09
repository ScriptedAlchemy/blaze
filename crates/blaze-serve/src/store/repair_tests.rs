//! Conditional publication through Store's real authorization and the shared publication
//! core. Sim owns the manifest's durability; native fixtures supply pack verification and
//! reopening. Copying Sim's surviving manifest into the fixture is a simulated mount, not
//! evidence that the host filesystem suffered power loss.

use blaze_core::sim::{Faults, Sim};
use blaze_core::{Completion, Done, Driver, Effect, FileName, Tag, run_until_idle};
use blaze_wire::{DIST_PACK_FORMAT, DIST_PACK_MAGIC, PackRef, pack_list};
use hegel::generators as gs;

use super::publication::{self, Publication, Step};
use super::*;

fn digest(bytes: &[u8]) -> Digest {
    Sha256::digest(bytes).into()
}

fn pack(tag: u8) -> (PackRef, Vec<u8>) {
    let key = [tag; 32];
    let mut bytes = DIST_PACK_MAGIC.to_vec();
    bytes.extend_from_slice(&key);
    bytes.push(0);
    bytes.extend_from_slice(&1u64.to_le_bytes());
    bytes.push(tag);
    let reference = PackRef {
        pack: digest(&bytes),
        size: bytes.len() as u64,
        keys: blaze_wire::digest_of_encoded_name(
            "blaze/dist-pack-keys",
            &blaze_wire::encode(&vec![key]),
        ),
        packed: None,
        format: DIST_PACK_FORMAT,
    };
    (reference, bytes)
}

fn path(pack: &PackRef) -> String {
    format!("packs/{}.pack", hex(&pack.pack))
}
fn name() -> String {
    format!("manifests/{}", hex(&[9; 32]))
}
fn put(store: &Store, pack: &(PackRef, Vec<u8>)) {
    store.put_blob(&path(&pack.0), &pack.1).unwrap();
}
fn current(store: &Store) -> Vec<u8> {
    store.read_small(&name(), 1 << 20).unwrap().unwrap()
}

#[hegel::test(test_cases = 32)]
fn first_publication_cannot_name_missing_or_corrupt_packs(tc: hegel::TestCase) {
    let failure = tc.draw(gs::integers::<u8>().max_value(3));
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let (a, b) = (pack(1), pack(2));
    let mut references = vec![a.0.clone(), b.0.clone()];
    references.sort();
    let list = pack_list::encode(&references);
    if failure != 1 {
        put(&store, &a);
    }
    if failure != 2 {
        put(&store, &b);
    }
    if failure == 3 {
        std::fs::write(store.dir().join(path(&b.0)), b"corrupted pack").unwrap();
    }
    let outcome = store.put_list(&name(), &list);
    if failure == 0 {
        assert!(outcome.unwrap(), "a complete verified first publication must succeed");
        assert_eq!(current(&store), list);
    } else {
        assert!(outcome.is_err(), "missing or corrupt packs must refuse first publication");
        assert!(
            store.read_small(&name(), 1 << 20).unwrap().is_none(),
            "refused first publication must not name an immutable manifest"
        );
    }
}

#[test]
fn malformed_first_publication_does_not_claim_the_key() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    assert!(store.put_list(&name(), b"not a pack list\n").is_err());
    assert!(store.read_small(&name(), 1 << 20).unwrap().is_none());
    assert!(store.put_list(&name(), b"").unwrap(), "an empty canonical list is valid");
}

#[test]
fn only_an_exact_stale_list_can_be_repaired_and_the_replacement_survives_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let (a, b) = (pack(1), pack(2));
    put(&store, &a);
    put(&store, &b);
    let old = pack_list::encode(&[a.0.clone()]);
    let new = pack_list::encode(&[b.0.clone()]);
    store.put_list(&name(), &old).unwrap();
    assert!(!store.repair_list(&name(), &digest(&old), &new).unwrap(), "a valid list is immutable");
    std::fs::remove_file(store.dir().join(path(&a.0))).unwrap();
    assert!(
        !store.repair_list(&name(), &[0; 32], &new).unwrap(),
        "a stale comparison cannot write"
    );
    assert_eq!(current(&store), old);
    assert!(store.repair_list(&name(), &digest(&old), &new).unwrap());
    drop(store);
    let store = Store::open(dir.path()).unwrap();
    assert_eq!(current(&store), new);
    assert!(store.verify_pack(&b.0).unwrap());
    assert!(!store.repair_list(&name(), &digest(&old), &old).unwrap());
}

#[test]
fn absent_or_corrupt_replacements_and_corrupt_old_packs_leave_the_old_list_untouched() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let (a, b, c) = (pack(1), pack(2), pack(3));
    let old = pack_list::encode(&[a.0.clone()]);
    let new = pack_list::encode(&[b.0.clone()]);
    // Seed legacy stale state: new first-writer PUTs must reject absent packs.
    std::fs::write(store.dir().join(name()), &old).unwrap();
    assert!(store.repair_list(&name(), &digest(&old), &new).is_err());
    assert_eq!(current(&store), old);
    put(&store, &b);
    let mut corrupt = b.1.clone();
    *corrupt.last_mut().unwrap() ^= 1;
    std::fs::write(store.dir().join(path(&b.0)), corrupt).unwrap();
    assert!(store.repair_list(&name(), &digest(&old), &new).is_err());
    assert_eq!(current(&store), old);
    put(&store, &a);
    put(&store, &c);
    let mut references = [a.0.clone(), b.0.clone()];
    references.sort();
    let old_two = pack_list::encode(&references);
    std::fs::write(store.dir().join(name()), &old_two).unwrap();
    std::fs::remove_file(store.dir().join(path(&a.0))).unwrap();
    let refused =
        store.repair_list(&name(), &digest(&old_two), &pack_list::encode(&[c.0])).unwrap_err();
    assert!(
        refused.to_string().contains(&path(&b.0)),
        "one absent pack must not hide another corrupt old pack: {refused}"
    );
    assert_eq!(current(&store), old_two);
}

#[test]
fn malformed_old_or_new_lists_are_explicit_refusals() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    // Persisted malformed legacy state is a recovery fixture, not a valid PUT.
    std::fs::write(store.dir().join(name()), b"not a pack list\n").unwrap();
    assert!(store.repair_list(&name(), &digest(b"not a pack list\n"), b"").is_err());
    assert_eq!(current(&store), b"not a pack list\n");
    let old = pack_list::encode(&[pack(1).0]);
    std::fs::write(store.dir().join(name()), &old).unwrap();
    assert!(store.repair_list(&name(), &digest(&old), b"not a pack list\n").is_err());
    assert_eq!(current(&store), old);
}

#[test]
fn two_native_cas_writers_cannot_both_replace_the_same_list() {
    let dir = tempfile::tempdir().unwrap();
    let store = std::sync::Arc::new(Store::open(dir.path()).unwrap());
    let a = pack(1);
    let b = pack(2);
    let c = pack(3);
    put(&store, &b);
    put(&store, &c);
    let old = pack_list::encode(&[a.0]);
    // Seed legacy stale state: new first-writer PUTs must reject absent packs.
    std::fs::write(store.dir().join(name()), &old).unwrap();
    let expected = digest(&old);
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
    let results = std::thread::scope(|scope| {
        let writers: Vec<_> = [b.0, c.0]
            .into_iter()
            .map(|pack| {
                let (store, barrier) = (&store, &barrier);
                scope.spawn(move || {
                    let new = pack_list::encode(&[pack]);
                    barrier.wait();
                    (store.repair_list(&name(), &expected, &new).unwrap(), new)
                })
            })
            .collect();
        writers.into_iter().map(|thread| thread.join().unwrap()).collect::<Vec<_>>()
    });
    assert_eq!(results.iter().filter(|(won, _)| *won).count(), 1);
    assert_eq!(current(&store), results.into_iter().find(|(won, _)| *won).unwrap().1);
}

#[test]
fn failed_native_publication_barriers_never_acknowledge_a_repair() {
    for step in [Step::FileSync, Step::Rename, Step::DirectorySync] {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let (a, b) = (pack(1), pack(2));
        put(&store, &b);
        let old = pack_list::encode(&[a.0]);
        let new = pack_list::encode(&[b.0]);
        // Seed legacy stale state: new first-writer PUTs must reject absent packs.
    std::fs::write(store.dir().join(name()), &old).unwrap();
        *store.publication_failure.lock().unwrap() = Some(step);
        let error = store.repair_list(&name(), &digest(&old), &new).unwrap_err();
        assert!(error.message.contains(&format!("{step:?}")), "{error}");
        drop(store);
        let store = Store::open(dir.path()).unwrap();
        // A directory-sync error is an uncertain commit, never an acknowledged one.
        assert_eq!(current(&store), if step == Step::DirectorySync { new } else { old });
        assert_eq!(std::fs::read_dir(dir.path().join("manifests")).unwrap().count(), 1);
    }
}

#[derive(Clone, Copy)]
enum Plant {
    None,
    CasBypass,
    Publication(publication::Plant),
}

fn slot(name: &str) -> FileName {
    FileName::new(name).unwrap()
}

fn finish(sim: &mut Sim, effect: Effect) -> Completion {
    // alloc: the simulation drives one effect at a time, with Sim's bounded virtual wait.
    let mut effects = vec![effect];
    let mut result = None;
    run_until_idle(sim, &mut effects, |completion, _| result = Some(completion)).unwrap();
    result.expect("one submitted effect completed")
}

#[derive(Default)]
struct Attempt {
    ack: bool,
    renamed: bool,
    directory_synced: bool,
    failed: Option<Step>,
}

/// The only simulated writer: stages unsynced bytes, then runs the production core's
/// effects without inventing their order. Faults and power loss belong to this IO adapter.
fn publish(sim: &mut Sim, bytes: &[u8], plan: u8, plant: Plant) -> Attempt {
    let staged = slot("objects/.staged");
    let fault = plan % 4;
    // 0: return normally; 1: append in flight; 2: staged; 3: file synced;
    // 4: renamed; 5: directory synced, response not yet delivered;
    // 6/7/8: file sync / rename / directory sync submitted but not completed.
    let crash_at = (plan / 4) % 9;
    let mut attempt = Attempt::default();
    sim.delete(&staged);
    let append = Effect::Append {
        file: staged.clone(),
        bytes: bytes.to_vec(), // alloc: staged test record
        tag: Tag(10),
    };
    if crash_at == 1 {
        sim.submit(&mut vec![append]); // alloc: one pending effect, lost at power failure
        return attempt;
    }
    assert_eq!(finish(sim, append).done, Done::Appended);
    if crash_at == 2 {
        return attempt;
    }
    let mut publication = match plant {
        Plant::Publication(plant) => Publication::planted(plant),
        _ => Publication::new(),
    };
    for _ in 0..Publication::MAX_STEPS {
        let Some(effect) = publication.effect() else { break };
        let step = publication.step();
        let fail = matches!(
            (fault, step),
            (1, Step::FileSync) | (2, Step::Rename) | (3, Step::DirectorySync)
        );
        if fail {
            match &effect {
                Effect::Sync { file, .. } => sim.fail_syncs(file.clone()),
                // Sim has no targeted rename-error switch. Removing its source makes
                // the actual Rename effect fail before it can change the published name.
                Effect::Rename { from, .. } => sim.delete(from),
                _ => unreachable!(),
            }
        }
        if matches!(
            (crash_at, step),
            (6, Step::FileSync) | (7, Step::Rename) | (8, Step::DirectorySync)
        ) {
            sim.submit(&mut vec![effect]); // alloc: an in-flight effect lost at power failure
            return attempt;
        }
        let completion = finish(sim, effect);
        let succeeded = !matches!(&completion.done, Done::Failed(_));
        if step == Step::Rename && succeeded {
            attempt.renamed = true;
        }
        if step == Step::DirectorySync && succeeded {
            attempt.directory_synced = true;
        }
        if publication.complete(completion).is_err() {
            attempt.failed = Some(step);
            return attempt;
        }
        if matches!(
            (crash_at, step),
            (3, Step::FileSync) | (4, Step::Rename) | (5, Step::DirectorySync)
        ) {
            return attempt;
        }
    }
    attempt.ack = publication.acknowledged();
    attempt
}

fn scenario(ops: &[u16], seed: u64, plant: Plant) -> Result<(), String> {
    let dir = tempfile::tempdir().unwrap();
    let mut store = Store::open(dir.path()).unwrap();
    let packs = [pack(1), pack(2), pack(3)];
    let mut disk: [Option<Vec<u8>>; 3] = std::array::from_fn(|i| Some(packs[i].1.clone()));
    for pack in &packs {
        put(&store, pack)
    }
    let mut model = pack_list::encode(&[packs[0].0.clone()]);
    store.put_list(&name(), &model).unwrap();
    let mut sim = Sim::new(seed, Faults::default());
    let published = slot("objects/published");
    sim.change(&published, &model);
    let mut expected = [digest(&model); 2];
    for raw in ops {
        let op = (raw & 255) as u8 % 8;
        let pack_at = usize::from(((raw & 255) / 8) % 3);
        match op {
            0 | 1 => expected[usize::from(op)] = digest(&model),
            2 | 3 => {
                let actor = usize::from(op - 2);
                let replacement = pack_list::encode(&[packs[pack_at].0.clone()]);
                // Independent authority oracle, using byte snapshots, not Store's predicate.
                let old = pack_list::decode(&model).unwrap();
                let mut missing = false;
                let mut invalid_old = false;
                for reference in old {
                    let i = packs.iter().position(|pack| pack.0.pack == reference.pack).unwrap();
                    match &disk[i] {
                        None => missing = true,
                        Some(bytes)
                            if digest(bytes) != reference.pack
                                || bytes.len() as u64 != reference.size =>
                        {
                            invalid_old = true
                        }
                        Some(_) => {}
                    }
                }
                let valid_new =
                    disk[pack_at].as_ref().is_some_and(|bytes| *bytes == packs[pack_at].1);
                let comparison = expected[actor] == digest(&model);
                let authorized = comparison && !invalid_old && missing && valid_new;
                let validation_error = comparison && (invalid_old || (missing && !valid_new));
                let plan = (raw >> 8) as u8;
                let mut attempted = None;
                store.bypass_comparison.store(matches!(plant, Plant::CasBypass), Ordering::Relaxed);
                let actual =
                    store.repair_list_with(&name(), &expected[actor], &replacement, |_, bytes| {
                        let attempt = publish(&mut sim, bytes, plan, plant);
                        let ack = attempt.ack;
                        attempted = Some(attempt);
                        if ack {
                            Ok(())
                        } else {
                            Err(Error::internal("simulated IO failure or power loss"))
                        }
                    });
                if attempted.is_some() != authorized {
                    return Err(format!(
                        "publication authority violated at operation {raw}, seed {seed}"
                    ));
                }
                if let Some(attempt) = attempted {
                    if attempt.ack && !attempt.directory_synced {
                        return Err("repair acknowledged before the directory durability boundary"
                            .to_owned()); // alloc: oracle failure
                    }
                    let before_crash = sim.contents(&published);
                    if matches!(attempt.failed, Some(Step::FileSync | Step::Rename))
                        && before_crash != model
                    {
                        return Err("failed file sync or rename replaced the old record".to_owned()); // alloc: oracle failure
                    }
                    // Sim retains a completed rename's namespace but still tears unsynced
                    // Append data. Also explore a kernel losing the rename before directory
                    // sync. This is a crash model extension, never another publication path.
                    sim.crash();
                    if plan >= 36 && attempt.renamed && !attempt.directory_synced {
                        sim.change(&published, &model);
                    }
                    let surviving = sim.contents(&published);
                    if surviving != model && surviving != replacement {
                        return Err(format!("torn reopened record at operation {raw}, seed {seed}"));
                    }
                    if attempt.ack && surviving != replacement {
                        return Err("acknowledged repair lost on power failure".to_owned()); // alloc: oracle failure
                    }
                    match actual {
                        Ok(true) if attempt.ack => {}
                        Err(_) if !attempt.ack => {}
                        _ => {
                            return Err(
                                "repair response disagrees with its publication outcome".to_owned()
                            );
                        } // alloc: oracle failure
                    }
                    model = surviving;
                    // Mount the surviving simulated manifest for the real reopen/reader.
                    std::fs::write(store.dir().join(name()), &model).unwrap();
                } else {
                    match actual {
                        Ok(false) if !validation_error => {}
                        Err(_) if validation_error => {}
                        _ => {
                            return Err(format!(
                                "repair refusal violated its oracle at operation {raw}"
                            ));
                        }
                    }
                }
                store = Store::open(dir.path()).unwrap();
            }
            4 => {
                let path = store.dir().join(path(&packs[pack_at].0));
                match std::fs::remove_file(path) {
                    Ok(()) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => return Err(error.to_string()),
                }
                disk[pack_at] = None;
            }
            5 => {
                if let Some(bytes) = &mut disk[pack_at] {
                    *bytes.last_mut().unwrap() ^= 1;
                    std::fs::write(store.dir().join(path(&packs[pack_at].0)), bytes).unwrap();
                }
            }
            6 => {
                put(&store, &packs[pack_at]);
                disk[pack_at] = Some(packs[pack_at].1.clone());
            }
            7 => store = Store::open(dir.path()).unwrap(),
            _ => unreachable!(),
        }
        if current(&store) != model {
            return Err(format!(
                "reopened publication changed without authority at operation {raw}"
            ));
        }
    }
    Ok(())
}

#[hegel::test(test_cases = 32)]
fn conditional_publication_preserves_comparison_and_whole_file_integrity(tc: hegel::TestCase) {
    let seed = tc.draw(gs::integers::<u64>());
    let ops = tc.draw(gs::vecs(gs::integers::<u16>()).min_size(1).max_size(24));
    scenario(&ops, seed, Plant::None).unwrap();
}

#[test]
fn the_oracle_catches_the_comparison_bypass_plant() {
    // Both actors capture A. Actor 1 installs B, B's pack is lost, then delayed actor 0
    // tries C under A's old comparison. B is stale but actor 0 does not own that comparison.
    scenario(&[4, 11, 12, 18], 0, Plant::None).unwrap();
    assert!(
        scenario(&[4, 11, 12, 18], 0, Plant::CasBypass)
            .is_err_and(|why| why.contains("publication authority"))
    );
}

#[test]
fn a_lost_repair_response_preserves_the_reopened_cas_winner_and_comparison() {
    // Actor 1 repairs A to B. Lose B's pack and let delayed actor 0 try C under A's
    // comparison: it may win only if the simulated power loss really restored A.
    for plan in [0, 3, 12, 16, 20, 52] {
        scenario(&[4, (plan << 8) | 11, 12, 18], 0, Plant::None).unwrap();
    }
}

#[test]
fn failed_syncs_rename_and_every_crash_boundary_reopen_authorized_whole_lists() {
    for seed in 0..4 {
        for fault in 0..4 {
            for cut in 0..9 {
                for lose_rename in [0, 36] {
                    let plan = fault + 4 * cut + lose_rename;
                    scenario(&[4, (plan << 8) | 10], seed, Plant::None).unwrap();
                }
            }
        }
    }
}

#[test]
fn the_oracle_catches_missing_file_sync_with_torn_appends() {
    // Keep the completed rename (Sim's default), crash before the directory sync.
    // The missing file sync must be exposed by Sim's torn data, not a phase assertion.
    let found = (0..64).any(|seed| {
        scenario(&[4, (16 << 8) | 10], seed, Plant::Publication(publication::Plant::NoFileSync))
            .is_err_and(|why| why.contains("torn reopened record"))
    });
    assert!(found, "the unsynced-data plant escaped the crash oracle");
}

#[test]
fn the_oracle_catches_acknowledgement_before_directory_sync() {
    // A directory sync would fail. Skipping it and returning success must be caught even
    // though Sim retains the whole new record after the completed rename.
    let result = scenario(&[4, (3 << 8) | 10], 0, Plant::Publication(publication::Plant::EarlyAck));
    assert!(result.is_err_and(|why| why.contains("directory durability boundary")));
}
