//! Conditional publication on the real store and atomic writer, with Hegel-drawn actor
//! schedules, pack loss/corruption, and crashes on both sides of the native naming boundary.

use super::*;
use blaze_wire::{DIST_PACK_FORMAT, DIST_PACK_MAGIC, PackRef, pack_list};
use hegel::generators as gs;
use std::panic::{AssertUnwindSafe, catch_unwind};

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
    store.put_list(&name(), &old).unwrap();
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
    let old_two = pack_list::encode(&[a.0.clone(), b.0.clone()]);
    std::fs::write(store.dir().join(name()), &old_two).unwrap();
    std::fs::remove_file(store.dir().join(path(&a.0))).unwrap();
    assert!(
        store.repair_list(&name(), &digest(&old_two), &pack_list::encode(&[c.0])).is_err(),
        "one absent pack must not hide another corrupt old pack"
    );
    assert_eq!(current(&store), old_two);
}

#[test]
fn malformed_old_or_new_lists_are_explicit_refusals() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    store.put_list(&name(), b"not a pack list\n").unwrap();
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
    store.put_list(&name(), &old).unwrap();
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

#[derive(Clone, Copy)]
enum Plant {
    None,
    CasBypass,
}

fn scenario(ops: &[u8], plant: Plant) -> Result<(), String> {
    let dir = tempfile::tempdir().unwrap();
    let mut store = Store::open(dir.path()).unwrap();
    let packs = [pack(1), pack(2), pack(3)];
    let mut disk: [Option<Vec<u8>>; 3] = std::array::from_fn(|i| Some(packs[i].1.clone()));
    for pack in &packs {
        put(&store, pack)
    }
    let mut model = pack_list::encode(&[packs[0].0.clone()]);
    store.put_list(&name(), &model).unwrap();
    let mut expected = [digest(&model); 2];
    for raw in ops {
        let op = raw % 8;
        let pack_at = usize::from((raw / 8) % 3);
        match op {
            0 | 1 => expected[usize::from(op)] = digest(&model),
            2 | 3 => {
                let actor = usize::from(op - 2);
                let replacement = pack_list::encode(&[packs[pack_at].0.clone()]);
                // The oracle uses its own byte snapshot, not Store's eligibility predicate.
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
                let should_write = comparison && !invalid_old && missing && valid_new;
                let should_error = comparison && (invalid_old || (missing && !valid_new));
                let crash = (raw / 24) % 3;
                let reached = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
                if should_write && crash == 1 {
                    let reached = reached.clone();
                    *store.before_name.lock().unwrap() = Some(Box::new(move || {
                        reached.store(true, Ordering::Relaxed);
                        panic!("simulated loss before naming");
                    }));
                }
                if should_write && crash == 2 {
                    let reached = reached.clone();
                    *store.after_name.lock().unwrap() = Some(Box::new(move || {
                        reached.store(true, Ordering::Relaxed);
                        panic!("simulated loss after naming");
                    }));
                }
                store.bypass_comparison.store(matches!(plant, Plant::CasBypass), Ordering::Relaxed);
                let actual = catch_unwind(AssertUnwindSafe(|| {
                    store.repair_list(&name(), &expected[actor], &replacement)
                }));
                if should_write && crash != 0 {
                    if actual.is_ok() || !reached.load(Ordering::Relaxed) {
                        return Err("the native crash point was not reached".to_owned());
                    }
                    if crash == 2 {
                        model = replacement
                    }
                } else {
                    match actual {
                        Ok(Ok(wrote)) if wrote == should_write && !should_error => {
                            if wrote {
                                model = replacement
                            }
                        }
                        Ok(Err(_)) if should_error => {}
                        _ => {
                            return Err(format!(
                                "conditional repair violated its oracle at operation {raw}"
                            ));
                        }
                    }
                }
                // Reopen after every repair/crash: the visible file is old or completely new.
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
                "reopened publication was torn or changed without authority at operation {raw}"
            ));
        }
    }
    Ok(())
}

#[hegel::test(test_cases = 32)]
fn conditional_publication_preserves_comparison_and_whole_file_integrity(tc: hegel::TestCase) {
    let ops = tc.draw(gs::vecs(gs::integers::<u8>()).min_size(1).max_size(24));
    scenario(&ops, Plant::None).unwrap();
}

#[test]
fn the_oracle_catches_the_comparison_bypass_plant() {
    // Both actors capture A. Actor 1 installs B, B's pack is lost, then delayed actor 0
    // tries C under A's old comparison. B is stale, but it is not the record actor 0 owns.
    assert!(scenario(&[4, 11, 12, 18], Plant::CasBypass).is_err());
}

#[test]
fn a_crash_on_either_side_of_naming_reopens_one_whole_authorized_list() {
    // Lose A, repair to B; run the real writer before-name and after-name crash points.
    scenario(&[4, 34], Plant::None).unwrap();
    scenario(&[4, 58], Plant::None).unwrap();
}
