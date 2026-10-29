use std::{fs, path::Path};

use blaze_spec::native_tree::{Kind, Tree};

fn write(root: &Path, path: &str, bytes: &[u8]) {
    let path = root.join(path);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, bytes).unwrap();
}

fn digest(root: &Path) -> blaze_wire::Digest {
    Tree::capture(root).unwrap().digest()
}

#[test]
fn sdk_identity_covers_headers_stubs_and_other_libraries() {
    let sdk = tempfile::tempdir().unwrap();
    let files = [
        "SDKSettings.json",
        "usr/lib/libSystem.tbd",
        "usr/include/nested/config.h",
        "usr/lib/libc++.tbd",
        "System/Library/Frameworks/Metal.framework/Modules/module.modulemap",
    ];
    for path in files {
        write(sdk.path(), path, b"original");
    }
    let original = digest(sdk.path());
    for path in files {
        write(sdk.path(), path, b"mutation");
        assert_ne!(original, digest(sdk.path()), "unbound SDK file {path}");
        write(sdk.path(), path, b"original");
        assert_eq!(original, digest(sdk.path()), "timestamps must not affect identity");
    }
}

#[test]
fn directory_membership_and_missing_roots_are_bound() {
    let parent = tempfile::tempdir().unwrap();
    let root = parent.path().join("root");
    let missing = Tree::capture(&root).unwrap();
    assert_eq!(missing.entry(Path::new("")).unwrap().kind, Kind::Missing);
    fs::create_dir(&root).unwrap();
    let empty = digest(&root);
    assert_ne!(missing.digest(), empty);
    fs::create_dir(root.join("empty-child")).unwrap();
    assert_ne!(empty, digest(&root));
    fs::remove_dir(root.join("empty-child")).unwrap();
    assert_eq!(empty, digest(&root));
    write(&root, "new.h", b"new file");
    assert_ne!(empty, digest(&root));
    fs::remove_file(root.join("new.h")).unwrap();
    assert_eq!(empty, digest(&root));
}

#[test]
fn file_bytes_are_bound_even_when_size_and_mtime_are_restored() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("native.a");
    fs::write(&path, b"first").unwrap();
    let modified = fs::metadata(&path).unwrap().modified().unwrap();
    let original = digest(root.path());
    fs::write(&path, b"other").unwrap();
    fs::File::options().write(true).open(&path).unwrap().set_modified(modified).unwrap();
    assert_ne!(original, digest(root.path()));
    let file = Tree::capture(&path).unwrap();
    assert!(matches!(file.entry(Path::new("")).unwrap().kind, Kind::File { len: 5, .. }));
}

#[test]
fn canned_files_derive_the_same_complete_tree_as_regular_files() {
    let root = tempfile::tempdir().unwrap();
    write(root.path(), "include/a.h", b"header");
    write(root.path(), "lib/a.tbd", b"library");
    let canned = Tree::from_files(
        root.path(),
        [
            (root.path().join("include/a.h"), b"header".to_vec()),
            (root.path().join("lib/a.tbd"), b"library".to_vec()),
            (root.path().parent().unwrap().join("unrelated"), b"outside".to_vec()),
        ],
    )
    .unwrap();
    assert_eq!(canned.digest(), digest(root.path()));
    assert_eq!(canned.entries.len(), 5);
    assert!(Tree::from_files(root.path(), [("../outside".into(), vec![])]).is_err());
    assert!(Tree::from_files(root.path(), [("a".into(), vec![]), ("a/b".into(), vec![])]).is_err());
    assert!(canned.entry(Path::new("include/../lib/a.tbd")).is_none());
}

#[cfg(unix)]
mod unix {
    use super::*;
    use std::os::unix::fs::{PermissionsExt, symlink};

    #[test]
    fn relocation_normalizes_internal_absolute_symlinks() {
        let parent = tempfile::tempdir().unwrap();
        let a = parent.path().join("SDK-a");
        let b = parent.path().join("elsewhere/SDK-b");
        for sdk in [&a, &b] {
            write(sdk, "usr/include/config.h", b"identical header");
            write(sdk, "usr/lib/libx.tbd", b"identical library");
            symlink(sdk.join("usr/include"), sdk.join("absolute-headers")).unwrap();
            symlink("usr/lib", sdk.join("relative-libraries")).unwrap();
        }
        assert_eq!(digest(&a), digest(&b));
        write(&b, "usr/include/config.h", b"different header");
        assert_ne!(digest(&a), digest(&b));
    }

    #[test]
    fn ancestor_symlinks_are_locations_not_sdk_contents() {
        let parent = tempfile::tempdir().unwrap();
        let root = parent.path().join("physical");
        write(&root, "SDK/usr/include/config.h", b"header");
        symlink("physical", parent.path().join("alias")).unwrap();
        assert_eq!(digest(&root.join("SDK")), digest(&parent.path().join("alias/SDK")));
    }

    #[test]
    fn symlink_targets_and_external_referent_bytes_are_bound() {
        let parent = tempfile::tempdir().unwrap();
        let sdk = parent.path().join("SDK");
        fs::create_dir(&sdk).unwrap();
        fs::write(parent.path().join("a"), b"same").unwrap();
        fs::write(parent.path().join("b"), b"same").unwrap();
        let link = sdk.join("library");
        symlink("../a", &link).unwrap();
        let first = digest(&sdk);
        fs::remove_file(&link).unwrap();
        symlink("../b", &link).unwrap();
        let second = digest(&sdk);
        assert_ne!(first, second, "the referent's bytes alone do not bind a symlink");
        fs::write(parent.path().join("b"), b"edit").unwrap();
        assert_ne!(second, digest(&sdk));
        fs::remove_file(&link).unwrap();
        symlink("../absent", &link).unwrap();
        let absent = Tree::capture(&sdk).unwrap();
        assert_eq!(absent.entry(Path::new("library")).unwrap().kind, Kind::Missing);
        fs::write(parent.path().join("absent"), b"created").unwrap();
        assert_ne!(absent.digest(), digest(&sdk));
    }

    #[test]
    fn directory_symlink_referents_include_all_their_files() {
        let parent = tempfile::tempdir().unwrap();
        let root = parent.path().join("SDK");
        let external = parent.path().join("external");
        fs::create_dir(&root).unwrap();
        write(&external, "nested/header.h", b"external header");
        symlink(&external, root.join("headers")).unwrap();
        let before = Tree::capture(&root).unwrap();
        assert!(matches!(
            before.entry(Path::new("headers/nested/header.h")).unwrap().kind,
            Kind::File { .. }
        ));
        write(&external, "nested/header.h", b"different header");
        assert_ne!(before.digest(), digest(&root));
    }

    #[test]
    fn symlink_and_directory_cycles_are_finite_bound_nodes() {
        let root = tempfile::tempdir().unwrap();
        symlink("self", root.path().join("self")).unwrap();
        symlink(".", root.path().join("parent")).unwrap();
        let tree = Tree::capture(root.path()).unwrap();
        assert_eq!(tree.entry(Path::new("self")).unwrap().kind, Kind::Cycle);
        assert_eq!(tree.entry(Path::new("parent")).unwrap().kind, Kind::Cycle);
        assert_eq!(tree.entries.len(), 3);
        let cycle = Tree::capture(&root.path().join("self")).unwrap();
        assert_eq!(cycle.entry(Path::new("")).unwrap().kind, Kind::Cycle);
    }

    #[test]
    fn excessive_symlink_chains_return_an_error_before_unbounded_recursion() {
        let root = tempfile::tempdir().unwrap();
        fs::write(root.path().join("end"), b"referent").unwrap();
        for at in 0..128 {
            let target = if at == 127 { "end".to_owned() } else { format!("link-{}", at + 1) };
            symlink(target, root.path().join(format!("link-{at}"))).unwrap();
        }
        let error = Tree::capture(&root.path().join("link-0")).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    }

    #[test]
    fn executable_permissions_change_identity() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("header.h");
        fs::write(&path, b"header").unwrap();
        let before = digest(root.path());
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        assert_ne!(before, digest(root.path()));
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(before, digest(root.path()));
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn raw_file_names_change_identity() {
        use std::{ffi::OsString, os::unix::ffi::OsStringExt};
        let root = tempfile::tempdir().unwrap();
        let name = OsString::from_vec(b"header-\xff.h".to_vec());
        let path = root.path().join(&name);
        fs::write(&path, b"raw name").unwrap();
        let before = Tree::capture(root.path()).unwrap();
        assert!(before.entry(Path::new(&name)).is_some());
        fs::rename(&path, root.path().join(OsString::from_vec(b"header-\xfe.h".to_vec()))).unwrap();
        assert_ne!(before.digest(), digest(root.path()));
    }

    #[test]
    fn unreadable_files_and_directories_are_errors_not_missing_entries() {
        let root = tempfile::tempdir().unwrap();
        let file = root.path().join("denied");
        fs::write(&file, b"must be read").unwrap();
        fs::set_permissions(&file, fs::Permissions::from_mode(0o000)).unwrap();
        let privileged = fs::File::open(&file).is_ok();
        let result = Tree::capture(root.path());
        fs::set_permissions(&file, fs::Permissions::from_mode(0o600)).unwrap();
        if !privileged {
            assert_eq!(result.unwrap_err().kind(), std::io::ErrorKind::PermissionDenied);
        }
        let directory = root.path().join("directory");
        fs::create_dir(&directory).unwrap();
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o000)).unwrap();
        let privileged = fs::read_dir(&directory).is_ok();
        let result = Tree::capture(root.path());
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o700)).unwrap();
        if !privileged {
            assert_eq!(result.unwrap_err().kind(), std::io::ErrorKind::PermissionDenied);
        }
    }

    #[test]
    fn parent_components_follow_native_symlink_semantics() {
        let root = tempfile::tempdir().unwrap();
        write(root.path(), "first/deeper/header.h", b"first");
        write(root.path(), "second/header.h", b"second");
        symlink("first/deeper", root.path().join("alias")).unwrap();
        let native = Tree::capture(&root.path().join("alias/..")).unwrap();
        assert_eq!(native.digest(), digest(&root.path().join("first")));
        let file = root.path().join("file");
        fs::write(&file, b"not a directory").unwrap();
        assert!(Tree::capture(&file.join("..")).is_err());
    }
}

fn same_batch_and_single(paths: &[std::path::PathBuf]) {
    let batch = Tree::capture_many(paths).unwrap();
    assert_eq!(batch.len(), paths.len());
    for (path, batch) in paths.iter().zip(batch) {
        let single = Tree::capture(path).unwrap();
        assert_eq!(batch.root, single.root, "root spelling changed for {}", path.display());
        assert_eq!(batch.entries, single.entries, "different closure for {}", path.display());
        assert_eq!(batch.digest(), single.digest(), "different digest for {}", path.display());
    }
}

#[test]
fn batch_matches_single_for_overlapping_files_directories_and_absence() {
    let root = tempfile::tempdir().unwrap();
    write(root.path(), "lib/nested/native.a", b"archive");
    write(root.path(), "include/native.h", b"header");
    same_batch_and_single(&[
        root.path().join("lib/nested"),
        root.path().join("lib/nested/native.a"),
        root.path().join("missing/child"),
        root.path().to_owned(),
        root.path().join("lib"),
        root.path().to_owned(),
    ]);
}

#[test]
fn batch_observes_content_and_membership_changes_between_calls() {
    let root = tempfile::tempdir().unwrap();
    write(root.path(), "lib/a", b"first");
    let paths = [root.path().to_owned(), root.path().join("lib")];
    let first = Tree::capture_many(&paths).unwrap();
    let stamp = fs::metadata(root.path().join("lib/a")).unwrap().modified().unwrap();
    write(root.path(), "lib/a", b"other");
    fs::File::options()
        .write(true)
        .open(root.path().join("lib/a"))
        .unwrap()
        .set_modified(stamp)
        .unwrap();
    let second = Tree::capture_many(&paths).unwrap();
    assert!(first.iter().zip(&second).all(|(a, b)| a.digest() != b.digest()));
    write(root.path(), "lib/new", b"new");
    let third = Tree::capture_many(&paths).unwrap();
    assert!(second.iter().zip(&third).all(|(a, b)| a.digest() != b.digest()));
    same_batch_and_single(&paths);
}

#[cfg(unix)]
#[test]
fn batch_preserves_aliases_external_referents_and_parent_components() {
    use std::os::unix::fs::symlink;
    let parent = tempfile::tempdir().unwrap();
    let root = parent.path().join("physical");
    write(&root, "SDK/include/native.h", b"header");
    write(parent.path(), "external/other.h", b"outside");
    symlink("physical", parent.path().join("alias")).unwrap();
    symlink("include/native.h", root.join("SDK/relative-file")).unwrap();
    symlink(parent.path().join("alias/SDK/include/native.h"), root.join("SDK/absolute-file"))
        .unwrap();
    symlink(parent.path().join("external"), root.join("SDK/external")).unwrap();
    symlink("absent", root.join("SDK/broken")).unwrap();
    let paths = vec![
        root.join("SDK/include"),
        parent.path().join("alias/SDK"),
        root.join("SDK/relative-file"),
        root.join("SDK/absolute-file"),
        root.join("SDK/external"),
        root.join("SDK/broken"),
        root.join("SDK/include/.."),
        root.clone(),
        root.join("SDK"),
    ];
    // APFS rejects invalid UTF-8 names; all symlink and parent-path cases still run there.
    #[cfg(target_os = "linux")]
    let paths = {
        use std::{ffi::OsString, os::unix::ffi::OsStringExt};
        let raw = root.join("SDK/include").join(OsString::from_vec(b"raw-\xff".to_vec()));
        fs::write(&raw, b"raw header").unwrap();
        let mut paths = paths;
        paths.push(raw);
        paths
    };
    same_batch_and_single(&paths);
}

#[cfg(unix)]
#[test]
fn batch_recaptures_subtrees_when_rerooting_changes_cycle_boundaries() {
    use std::os::unix::fs::symlink;
    let root = tempfile::tempdir().unwrap();
    write(root.path(), "a/header.h", b"a");
    write(root.path(), "b/header.h", b"b");
    symlink("..", root.path().join("a/up")).unwrap();
    symlink("self", root.path().join("b/self")).unwrap();
    same_batch_and_single(&[root.path().join("a"), root.path().to_owned(), root.path().join("b")]);
}
