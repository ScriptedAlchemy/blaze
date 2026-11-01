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

/// Freeze the manifest publication under the directory remote's actual CAS lock.
/// A peer publishes after the keeper starts uploading, before its CAS can commit.
/// No sleeps choose the ordering: the uploaded keys object is the barrier.
#[test]
fn publishing_cannot_acknowledge_a_peers_unpulled_pack() {
    use ra_jit_store::Store;
    use std::fs::File;

    for lazy in [false, true] {
        let fixture = Fixture::new(lazy);
        let manifest = fixture.remote_dir.join(Manifest::at("recovery"));
        std::fs::write(&manifest, b"").unwrap();
        fixture.look();
        let observed = fixture.shared.runtime.block_on(fixture.shared.manifest_now());
        assert!(fixture.shared.asked.saw(observed));

        ra_jit_store::compiles_against("keeper-publication-regression");
        let local = Store::owned(&fixture.shared.store).unwrap();
        local.insert(&[41; 32], b"this worker's compiled entry");
        let own_packs = local.seal_checked().unwrap();
        assert_eq!(own_packs.len(), 1);
        let own_bytes = local.pack(&own_packs[0]).unwrap();
        let own_keys = ra_jit_store::pack_keys(&own_packs[0], &own_bytes).unwrap().concat();
        let uploaded_keys = fixture.remote_dir.join(keys_object(&Digest::of(&own_keys)));
        assert!(!uploaded_keys.exists());

        let peer_store = Store::owned(&fixture._work.path().join("peer")).unwrap();
        peer_store.insert(&[42; 32], b"a different worker's compiled entry");
        let peer_packs = peer_store.seal_checked().unwrap();
        let peer_pack = peer_store.pack(&peer_packs[0]).unwrap();
        let peer_digest = Digest::of(&peer_pack);
        let peer_keys = ra_jit_store::pack_keys(&peer_packs[0], &peer_pack).unwrap().concat();
        let peer_keys_digest = Digest::of(&peer_keys);
        tests::put(&fixture.remote_dir, &pack_object(&peer_digest), &peer_pack);
        tests::put(&fixture.remote_dir, &keys_object(&peer_keys_digest), &peer_keys);
        let lock = File::create(manifest.with_extension("lock")).unwrap();
        lock.lock().unwrap();
        fixture.shared.runtime.block_on(async {
            let peer = async {
                let uploaded = tokio::time::timeout(Duration::from_secs(10), async {
                    while !uploaded_keys.exists() {
                        tokio::task::yield_now().await;
                    }
                })
                .await;
                if uploaded.is_ok() {
                    let listed = Listed {
                        size: peer_pack.len() as u64,
                        keys: peer_keys_digest,
                        format: STORE_FORMAT,
                        packed: None,
                    };
                    std::fs::write(&manifest, Manifest([(peer_digest, listed)].into()).lines())
                        .unwrap();
                }
                // Always release the lock before asserting, including on a timeout.
                lock.unlock().unwrap();
                uploaded.expect("the publisher never uploaded its keys");
            };
            futures_util::future::join(fixture.shared.send(), peer).await;
        });
        let published = fixture.shared.runtime.block_on(fixture.shared.manifest_now());
        assert!(observed != published);
        assert!(fixture.shared.asked.saw(observed), "a write is not an installed lookup");
        assert!(!fixture.shared.asked.saw(published));
        fixture.look();
        assert!(fixture.shared.asked.saw(published));
        if lazy {
            let lookup = fixture.shared.asked.lazy.read().unwrap();
            assert!(
                lookup.as_ref().unwrap().packs.iter().any(|(digest, _, _)| *digest == peer_digest)
            );
        } else {
            assert_eq!(
                std::fs::read(fixture.shared.store.join(pack_file(&peer_digest))).unwrap(),
                peer_pack,
            );
        }
    }
}
