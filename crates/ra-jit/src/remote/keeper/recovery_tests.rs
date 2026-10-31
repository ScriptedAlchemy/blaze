//! Directory-remote failures must not become permanent negative cache entries.
use super::*;
use crate::remote::{
    Listed, Manifest, Remote, STORE_FORMAT, keys_object, pack_file, pack_object, tests,
};
use std::path::PathBuf;

struct Fixture {
    _work: tempfile::TempDir,
    remote_dir: PathBuf,
    shared: Shared,
    pack: Vec<u8>,
    digest: Digest,
    keys: Digest,
}

impl Fixture {
    fn new(lazy: bool) -> Self {
        let work = tempfile::tempdir().unwrap();
        let remote_dir = work.path().join("remote");
        let store = work.path().join("local");
        let runtime = Arc::new(crate::remote::runtime().unwrap());
        let remote = Remote::in_dir(&remote_dir, &store, tests::taking_unsigned(), 1 << 20);
        // A valid empty sealed store pack: magic followed by zero block/entry counts.
        let mut pack = b"rajitpk".to_vec();
        pack.push(STORE_FORMAT as u8);
        pack.extend_from_slice(&[0; 8]);
        let digest = Digest::of(&pack);
        let keys = Digest::of(&[]);
        let listed = Listed { size: pack.len() as u64, keys, format: STORE_FORMAT, packed: None };
        tests::put(
            &remote_dir,
            &Manifest::at("recovery"),
            Manifest([(digest, listed)].into()).lines().as_bytes(),
        );
        let shared = Shared {
            store,
            remote: Arc::new(remote),
            project: "recovery".into(),
            runtime,
            lazy,
            push_every: None,
            asked: Arc::default(),
        };
        Self { _work: work, remote_dir, shared, pack, digest, keys }
    }

    fn restore_pack(&self) {
        tests::put(&self.remote_dir, &pack_object(&self.digest), &self.pack);
    }

    fn restore_keys(&self) {
        tests::put(&self.remote_dir, &keys_object(&self.keys), &[]);
    }

    fn local_pack(&self) -> PathBuf {
        self.shared.store.join(pack_file(&self.digest))
    }

    fn look(&self) {
        self.shared.runtime.block_on(self.shared.look());
    }
}

#[test]
fn unchanged_manifest_is_retried_after_an_eager_pull_fails() {
    let fixture = Fixture::new(false);
    fixture.look();
    assert!(!fixture.local_pack().exists());
    fixture.restore_pack();
    fixture.look();
    assert_eq!(std::fs::read(fixture.local_pack()).unwrap(), fixture.pack);
    // A completed pull does not keep fetching the same manifest's pack.
    let operations = fixture.shared.remote.traffic.operations.load(Ordering::Relaxed);
    fixture.look();
    assert_eq!(fixture.shared.remote.traffic.operations.load(Ordering::Relaxed), operations + 1);
}

#[test]
fn failed_initial_pull_does_not_suppress_the_keepers_next_look() {
    let fixture = Fixture::new(false);
    let observed = fixture.shared.runtime.block_on(fixture.shared.manifest_now());
    let failed = fixture.shared.runtime.block_on(fixture.shared.pull_and_remember(observed));
    assert!(failed.is_err());
    assert!(!fixture.shared.asked.saw(observed));
    fixture.restore_pack();
    fixture.look();
    assert_eq!(std::fs::read(fixture.local_pack()).unwrap(), fixture.pack);
}

#[test]
fn unavailable_lazy_index_is_not_a_successful_partial_refresh() {
    let fixture = Fixture::new(true);
    fixture.look();
    assert!(fixture.shared.asked.lazy.read().unwrap().is_none());
    fixture.restore_keys();
    fixture.look();
    let lookup = fixture.shared.asked.lazy.read().unwrap();
    assert_eq!(lookup.as_ref().unwrap().packs.len(), 1);
}

#[test]
fn a_failed_lazy_refresh_preserves_the_previous_lookup() {
    let fixture = Fixture::new(true);
    fixture.restore_keys();
    fixture.look();
    let before = fixture.shared.asked.lazy.read().unwrap().as_ref().unwrap().clone();
    // The manifest stays identical while the remote object is temporarily unavailable.
    std::fs::remove_file(fixture.remote_dir.join(keys_object(&fixture.keys))).unwrap();
    let observed = fixture.shared.runtime.block_on(fixture.shared.manifest_now());
    let failed = fixture.shared.runtime.block_on(fixture.shared.pull_and_remember(observed));
    assert!(failed.is_err());
    let after = fixture.shared.asked.lazy.read().unwrap().as_ref().unwrap().clone();
    assert!(Arc::ptr_eq(&before, &after));
    fixture.restore_keys();
    fixture.look();
    assert!(fixture.shared.asked.saw(observed));
}
