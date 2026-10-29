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

#[cfg(unix)]
#[test]
fn selected_tree_retains_transitive_symlink_inputs_without_copying_unrelated_files() {
    use std::os::unix::fs::symlink;
    let fixture = tempfile::tempdir().unwrap();
    let root = fixture.path().canonicalize().unwrap();
    let store = tempfile::tempdir().unwrap();
    let worker = tempfile::tempdir().unwrap();
    fs::create_dir_all(root.join("project/src")).unwrap();
    fs::create_dir_all(root.join("shared")).unwrap();
    fs::create_dir_all(root.join("native/include")).unwrap();
    fs::write(root.join("native/include/header.h"), "original header").unwrap();
    fs::write(root.join("native/unrelated"), "not an input").unwrap();
    symlink("../native/include", root.join("shared/include")).unwrap();
    symlink("../../shared/include", root.join("project/src/include")).unwrap();
    let snapshot =
        Snapshot::capture_selected(&root, &[root.join("project")], &[], &[], store.path()).unwrap();
    fs::write(root.join("native/include/header.h"), "edited header").unwrap();
    let edited =
        Snapshot::capture_selected(&root, &[root.join("project")], &[], &[], store.path()).unwrap();
    assert_ne!(snapshot.digest(), edited.digest());
    let destination = worker.path().join("image");
    Snapshot::materialize(snapshot.digest(), store.path(), &destination).unwrap();
    assert_eq!(
        fs::read(destination.join("project/src/include/header.h")).unwrap(),
        b"original header"
    );
    assert_eq!(
        fs::read_link(destination.join("project/src/include")).unwrap(),
        std::path::Path::new("../../shared/include")
    );
    assert!(destination.join("shared/include").is_symlink());
    assert!(!destination.join("native/unrelated").exists());
}

#[cfg(unix)]
#[test]
fn selected_tree_and_structural_aliases_survive_relocation() {
    use std::os::unix::fs::symlink;
    let fixture = tempfile::tempdir().unwrap();
    let root = fixture.path().canonicalize().unwrap();
    let store = tempfile::tempdir().unwrap();
    let worker = tempfile::tempdir().unwrap();
    fs::create_dir_all(root.join("packages/app/src")).unwrap();
    fs::create_dir_all(root.join("home")).unwrap();
    fs::create_dir_all(root.join("configuration")).unwrap();
    fs::write(root.join("packages/app/Cargo.toml"), "manifest").unwrap();
    fs::write(root.join("packages/app/src/lib.rs"), "source").unwrap();
    fs::write(root.join("configuration/config.toml"), "configuration").unwrap();
    symlink("packages/app", root.join("selected")).unwrap();
    symlink("../configuration", root.join("home/.cargo")).unwrap();
    let snapshot = Snapshot::capture_selected(
        &root,
        &[root.join("selected")],
        &[],
        &[
            (root.join("selected/Cargo.toml"), b"manifest".to_vec()),
            (root.join("home/.cargo/config.toml"), b"configuration".to_vec()),
        ],
        store.path(),
    )
    .unwrap();
    let destination = worker.path().join("image");
    Snapshot::materialize(snapshot.digest(), store.path(), &destination).unwrap();
    assert!(destination.join("selected").is_symlink());
    assert!(destination.join("home/.cargo").is_symlink());
    assert_eq!(fs::read(destination.join("selected/src/lib.rs")).unwrap(), b"source");
    assert_eq!(fs::read(destination.join("home/.cargo/config.toml")).unwrap(), b"configuration");
}

#[cfg(unix)]
#[test]
fn source_links_capture_referenced_output_without_unrelated_build_artifacts() {
    use std::os::unix::fs::symlink;
    let fixture = tempfile::tempdir().unwrap();
    let root = fixture.path().canonicalize().unwrap();
    let store = tempfile::tempdir().unwrap();
    let worker = tempfile::tempdir().unwrap();
    fs::create_dir(root.join("source")).unwrap();
    fs::create_dir(root.join("target")).unwrap();
    fs::write(root.join("target/generated.rs"), "generated input").unwrap();
    fs::write(root.join("target/unrelated"), "build output").unwrap();
    symlink("../target/generated.rs", root.join("source/generated.rs")).unwrap();
    let snapshot = Snapshot::capture(&root, &[root.join("target")], &[], store.path()).unwrap();
    let destination = worker.path().join("image");
    Snapshot::materialize(snapshot.digest(), store.path(), &destination).unwrap();
    assert_eq!(fs::read(destination.join("source/generated.rs")).unwrap(), b"generated input");
    assert!(!destination.join("target/unrelated").exists());
}

#[cfg(unix)]
#[test]
fn parent_components_follow_symlink_directory_topology() {
    use std::os::unix::fs::symlink;
    let fixture = tempfile::tempdir().unwrap();
    let root = fixture.path().canonicalize().unwrap();
    let store = tempfile::tempdir().unwrap();
    let worker = tempfile::tempdir().unwrap();
    fs::create_dir(root.join("source")).unwrap();
    fs::create_dir_all(root.join("deep/one/two")).unwrap();
    fs::write(root.join("deep/payload"), "resolved parent").unwrap();
    fs::write(root.join("payload"), "root parent").unwrap();
    symlink("../deep/one/two", root.join("source/pivot")).unwrap();
    symlink("pivot/../../payload", root.join("source/data")).unwrap();
    symlink("pivot/../../../payload", root.join("source/root-data")).unwrap();
    let snapshot =
        Snapshot::capture_selected(&root, &[root.join("source")], &[], &[], store.path()).unwrap();
    let destination = worker.path().join("image");
    Snapshot::materialize(snapshot.digest(), store.path(), &destination).unwrap();
    assert_eq!(fs::read(destination.join("source/data")).unwrap(), b"resolved parent");
    assert_eq!(fs::read(destination.join("source/root-data")).unwrap(), b"root parent");
}

#[test]
fn external_structural_reads_and_repeated_versions_must_match_captured_bytes() {
    let fixture = tempfile::tempdir().unwrap();
    let root = fixture.path().canonicalize().unwrap();
    let store = tempfile::tempdir().unwrap();
    fs::create_dir(root.join("project")).unwrap();
    fs::create_dir(root.join("config")).unwrap();
    let config = root.join("config/settings.toml");
    fs::write(&config, "old").unwrap();
    let trees = [root.join("project")];
    let reads = [
        (config.clone(), b"old".to_vec()),
        (config.clone(), b"intermediate".to_vec()),
        (config.clone(), b"old".to_vec()),
    ];
    assert!(Snapshot::capture_selected(&root, &trees, &[], &reads, store.path()).is_err());
    fs::write(&config, "new").unwrap();
    assert!(
        Snapshot::capture_selected(
            &root,
            &trees,
            &[],
            &[(config.clone(), b"old".to_vec())],
            store.path()
        )
        .is_err()
    );
    assert!(
        Snapshot::capture_selected(&root, &trees, &[], &[(config, b"new".to_vec())], store.path())
            .is_ok()
    );
}

#[test]
fn a_blob_store_inside_the_source_is_not_recaptured() {
    let fixture = tempfile::tempdir().unwrap();
    let root = fixture.path().canonicalize().unwrap();
    let store = root.join(".snapshots");
    let worker = tempfile::tempdir().unwrap();
    fs::write(root.join("input"), "source").unwrap();
    let first = Snapshot::capture(&root, &[], &[], &store).unwrap();
    let second = Snapshot::capture(&root, &[], &[], &store).unwrap();
    assert_eq!(first.digest(), second.digest());
    let destination = worker.path().join("image");
    Snapshot::materialize(first.digest(), &store, &destination).unwrap();
    assert!(!destination.join(".snapshots").exists());
    assert_eq!(fs::read(destination.join("input")).unwrap(), b"source");
}

#[cfg(unix)]
#[test]
fn an_ancestor_directory_link_is_finite_but_a_link_to_the_store_is_refused() {
    use std::os::unix::fs::symlink;
    let fixture = tempfile::tempdir().unwrap();
    let root = fixture.path().canonicalize().unwrap();
    let store = root.join(".snapshots");
    let worker = tempfile::tempdir().unwrap();
    fs::create_dir(root.join("source")).unwrap();
    fs::write(root.join("source/input"), "source").unwrap();
    symlink(".", root.join("source/self")).unwrap();
    let snapshot = Snapshot::capture(&root, &[], &[], &store).unwrap();
    let destination = worker.path().join("image");
    Snapshot::materialize(snapshot.digest(), &store, &destination).unwrap();
    assert_eq!(fs::read(destination.join("source/self/input")).unwrap(), b"source");
    symlink("../.snapshots", root.join("source/store")).unwrap();
    assert!(Snapshot::capture(&root, &[], &[], &store).is_err());
}

#[cfg(unix)]
#[test]
fn output_and_store_directory_aliases_are_excluded_before_following_links() {
    use std::os::unix::fs::symlink;
    let fixture = tempfile::tempdir().unwrap();
    let root = fixture.path().canonicalize().unwrap();
    let outputs = tempfile::tempdir().unwrap();
    let store = tempfile::tempdir().unwrap();
    let worker = tempfile::tempdir().unwrap();
    fs::write(root.join("input"), "source").unwrap();
    fs::write(outputs.path().join("artifact"), "build output").unwrap();
    symlink(outputs.path(), root.join("target")).unwrap();
    symlink(store.path(), root.join("store")).unwrap();
    let snapshot =
        Snapshot::capture(&root, &[root.join("target")], &[], &root.join("store")).unwrap();
    let destination = worker.path().join("image");
    Snapshot::materialize(snapshot.digest(), store.path(), &destination).unwrap();
    assert_eq!(fs::read(destination.join("input")).unwrap(), b"source");
    assert!(!destination.join("target").exists());
    assert!(!destination.join("store").exists());
}
