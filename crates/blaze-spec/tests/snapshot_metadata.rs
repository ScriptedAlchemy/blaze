use blaze_spec::snapshot::Snapshot;
use blaze_wire::SourcePath;
use std::fs;

fn path(value: &str) -> SourcePath {
    SourcePath::new(value.to_owned()).unwrap()
}

#[test]
fn embedded_file_is_authenticated_and_cannot_replace_source_content() {
    let source = tempfile::tempdir().unwrap();
    let store = tempfile::tempdir().unwrap();
    let worker = tempfile::tempdir().unwrap();
    fs::write(source.path().join("source"), "unchanged").unwrap();
    let original = Snapshot::capture(source.path(), &[], &[], store.path()).unwrap();
    let metadata = path("control/request.cbor");
    let with_metadata = original.clone().with_file(&metadata, b"request", store.path()).unwrap();
    assert_ne!(original.digest(), with_metadata.digest());
    assert!(Snapshot::read_file(original.digest(), store.path(), &metadata).is_err());
    assert_eq!(
        Snapshot::read_file(with_metadata.digest(), store.path(), &metadata).unwrap(),
        b"request"
    );
    assert!(with_metadata.clone().with_file(&metadata, b"replacement", store.path()).is_err());
    assert!(original.clone().with_file(&path("source"), b"replacement", store.path()).is_err());
    assert!(original.with_file(&path("source/child"), b"replacement", store.path()).is_err());
    let destination = worker.path().join("image");
    Snapshot::materialize(with_metadata.digest(), store.path(), &destination).unwrap();
    assert_eq!(fs::read(destination.join("control/request.cbor")).unwrap(), b"request");
    assert_eq!(fs::read(destination.join("source")).unwrap(), b"unchanged");

    let digest = blake3::hash(b"request");
    fs::write(store.path().join(digest.to_hex().as_str()), b"corrupt").unwrap();
    assert!(Snapshot::read_file(with_metadata.digest(), store.path(), &metadata).is_err());
    let absent = worker.path().join("absent");
    assert!(Snapshot::materialize(with_metadata.digest(), store.path(), &absent).is_err());
    assert!(!absent.exists());
}

#[cfg(unix)]
#[test]
fn control_metadata_cannot_use_a_source_symlink() {
    use std::os::unix::fs::symlink;
    let source = tempfile::tempdir().unwrap();
    let store = tempfile::tempdir().unwrap();
    fs::create_dir(source.path().join("directory")).unwrap();
    fs::write(source.path().join("directory/metadata"), "source").unwrap();
    symlink("directory", source.path().join("alias")).unwrap();
    symlink("directory/metadata", source.path().join("metadata")).unwrap();
    let snapshot = Snapshot::capture(source.path(), &[], &[], store.path()).unwrap();
    assert!(snapshot.clone().with_file(&path("alias/new"), b"control", store.path()).is_err());
    assert!(snapshot.with_file(&path("metadata"), b"control", store.path()).is_err());
    let snapshot = Snapshot::capture(source.path(), &[], &[], store.path()).unwrap();
    assert!(Snapshot::read_file(snapshot.digest(), store.path(), &path("metadata")).is_err());
}

#[cfg(unix)]
#[test]
fn an_authenticated_manifest_cannot_materialize_a_link_outside_its_root() {
    use blaze_wire::{Encoder, Wire};
    let store = tempfile::tempdir().unwrap();
    let worker = tempfile::tempdir().unwrap();
    fs::create_dir(worker.path().join("outside")).unwrap();
    fs::write(worker.path().join("outside/sentinel"), "untouched").unwrap();
    let target = b"../outside";
    let digest = *blake3::hash(target).as_bytes();
    fs::write(store.path().join(blake3::Hash::from(digest).to_hex().as_str()), target).unwrap();
    let mut encoded = Encoder::new();
    encoded.map(2);
    encoded.uint(0);
    encoded.uint(1);
    encoded.uint(1);
    encoded.array(1);
    encoded.array(4);
    path("link").encode(&mut encoded);
    2u8.encode(&mut encoded);
    0u32.encode(&mut encoded);
    digest.encode(&mut encoded);
    let bytes = encoded.into_bytes();
    let root = *blake3::hash(&bytes).as_bytes();
    fs::write(store.path().join(blake3::Hash::from(root).to_hex().as_str()), bytes).unwrap();
    let destination = worker.path().join("image");
    assert!(Snapshot::materialize(root, store.path(), &destination).is_err());
    assert!(!destination.exists());
    assert_eq!(fs::read(worker.path().join("outside/sentinel")).unwrap(), b"untouched");
}
