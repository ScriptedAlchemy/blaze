use std::fs;

use blaze_spec::snapshot::Snapshot;

#[test]
fn dirty_untracked_and_non_rust_inputs_are_immutable_and_relocated() {
    let source = tempfile::tempdir().unwrap();
    let store = tempfile::tempdir().unwrap();
    let worker = tempfile::tempdir().unwrap();
    fs::create_dir(source.path().join("src")).unwrap();
    fs::create_dir(source.path().join("empty")).unwrap();
    fs::write(source.path().join("src/main.rs"), "fn main() {}").unwrap();
    fs::write(source.path().join("data"), b"untracked\0bytes").unwrap();
    let first = Snapshot::capture(source.path(), &[], &[], store.path()).unwrap();
    fs::write(source.path().join("src/main.rs"), "fn main() { panic!() }").unwrap();
    let second = Snapshot::capture(source.path(), &[], &[], store.path()).unwrap();
    assert_ne!(first.digest(), second.digest());
    let destination = worker.path().join("elsewhere");
    Snapshot::materialize(first.digest(), store.path(), &destination).unwrap();
    assert_eq!(fs::read(destination.join("src/main.rs")).unwrap(), b"fn main() {}");
    assert_eq!(fs::read(destination.join("data")).unwrap(), b"untracked\0bytes");
    assert!(destination.join("empty").is_dir());
    let recaptured = Snapshot::capture(&destination, &[], &[], store.path()).unwrap();
    assert_eq!(first.digest(), recaptured.digest());
}

#[test]
fn outputs_do_not_change_identity_but_ignored_source_does() {
    let source = tempfile::tempdir().unwrap();
    let store = tempfile::tempdir().unwrap();
    fs::create_dir(source.path().join("target")).unwrap();
    fs::create_dir(source.path().join(".git")).unwrap();
    fs::write(source.path().join(".gitignore"), "secret\n").unwrap();
    fs::write(source.path().join("secret"), "one").unwrap();
    let outputs = [source.path().join("target")];
    let first = Snapshot::capture(source.path(), &outputs, &[], store.path()).unwrap();
    fs::write(source.path().join("target/generated"), "changed").unwrap();
    fs::write(source.path().join(".git/HEAD"), "changed").unwrap();
    assert_eq!(
        first.digest(),
        Snapshot::capture(source.path(), &outputs, &[], store.path()).unwrap().digest()
    );
    fs::write(source.path().join("secret"), "two").unwrap();
    assert_ne!(
        first.digest(),
        Snapshot::capture(source.path(), &outputs, &[], store.path()).unwrap().digest()
    );
}

#[test]
fn racing_structural_input_and_corruption_fail_before_materialization() {
    let source = tempfile::tempdir().unwrap();
    let store = tempfile::tempdir().unwrap();
    let worker = tempfile::tempdir().unwrap();
    let manifest = source.path().join("Cargo.toml");
    fs::write(&manifest, "new").unwrap();
    assert!(
        Snapshot::capture(source.path(), &[], &[(manifest, b"old".to_vec())], store.path())
            .is_err()
    );
    let snapshot = Snapshot::capture(source.path(), &[], &[], store.path()).unwrap();
    let name: String = snapshot.digest().iter().map(|byte| format!("{byte:02x}")).collect();
    fs::write(store.path().join(name), b"damaged").unwrap();
    let destination = worker.path().join("absent");
    assert!(Snapshot::materialize(snapshot.digest(), store.path(), &destination).is_err());
    assert!(!destination.exists());
}

#[cfg(unix)]
#[test]
fn executable_modes_and_relative_symlinks_roundtrip_without_store_aliases() {
    use std::os::unix::fs::{PermissionsExt, symlink};
    let source = tempfile::tempdir().unwrap();
    let store = tempfile::tempdir().unwrap();
    let worker = tempfile::tempdir().unwrap();
    fs::write(source.path().join("script"), "#!/bin/sh\nexit 3\n").unwrap();
    fs::set_permissions(source.path().join("script"), fs::Permissions::from_mode(0o755)).unwrap();
    symlink("script", source.path().join("alias")).unwrap();
    let snapshot = Snapshot::capture(source.path(), &[], &[], store.path()).unwrap();
    let destination = worker.path().join("tree");
    Snapshot::materialize(snapshot.digest(), store.path(), &destination).unwrap();
    assert_eq!(fs::read_link(destination.join("alias")).unwrap(), std::path::Path::new("script"));
    assert_eq!(
        fs::metadata(destination.join("script")).unwrap().permissions().mode() & 0o111,
        0o111
    );
    fs::set_permissions(destination.join("script"), fs::Permissions::from_mode(0o755)).unwrap();
    fs::write(destination.join("script"), "worker mutation").unwrap();
    let other = worker.path().join("other");
    Snapshot::materialize(snapshot.digest(), store.path(), &other).unwrap();
    assert_eq!(fs::read(other.join("script")).unwrap(), b"#!/bin/sh\nexit 3\n");
}

#[test]
fn sparse_external_package_and_config_preserve_topology_without_copying_ancestor() {
    let fixture = tempfile::tempdir().unwrap();
    let root = fixture.path().canonicalize().unwrap();
    let store = tempfile::tempdir().unwrap();
    let worker = tempfile::tempdir().unwrap();
    fs::create_dir_all(root.join("code/workspace/src")).unwrap();
    fs::create_dir_all(root.join("shared/dependency/src")).unwrap();
    fs::create_dir_all(root.join("home/.cargo")).unwrap();
    fs::write(root.join("code/workspace/src/main.rs"), "workspace").unwrap();
    fs::write(root.join("shared/dependency/src/lib.rs"), "external").unwrap();
    let config = root.join("home/.cargo/config.toml");
    fs::write(&config, "[profile.dev]\nopt-level=1\n").unwrap();
    fs::write(root.join("unrelated"), "must not copy me").unwrap();
    let snapshot = Snapshot::capture_selected(
        &root,
        &[root.join("code/workspace"), root.join("shared/dependency")],
        &[],
        &[(config, b"[profile.dev]\nopt-level=1\n".to_vec())],
        store.path(),
    )
    .unwrap();
    let destination = worker.path().join("image");
    Snapshot::materialize(snapshot.digest(), store.path(), &destination).unwrap();
    assert_eq!(fs::read(destination.join("shared/dependency/src/lib.rs")).unwrap(), b"external");
    assert!(destination.join("home/.cargo/config.toml").is_file());
    assert!(!destination.join("unrelated").exists());
}
