use std::{fs, path::Path};

use blaze_spec::{
    native::{Closure, Root},
    native_tree::{Entry, Kind, Tree},
};
use blaze_wire::{ConfigEnvValue, Digest, InputText};

fn canned(name: &str, root: &str, files: &[(&str, &[u8])]) -> Root {
    Root {
        name: name.to_owned(),
        tree: Tree::from_files(
            root,
            files.iter().map(|(path, bytes)| (Path::new(root).join(path), bytes.to_vec())),
        )
        .unwrap(),
    }
}

fn captured(name: &str, path: &Path) -> Closure {
    Closure { roots: vec![Root { name: name.to_owned(), tree: Tree::capture(path).unwrap() }] }
}

#[test]
fn physical_relocation_preserves_identity_and_moves_authority() {
    let first = Closure {
        roots: vec![canned(
            "sdk",
            "/original/sdk",
            &[("include/a.h", b"header"), ("lib/a.a", b"lib")],
        )],
    };
    let moved = Closure {
        roots: vec![canned(
            "sdk",
            "/moved/sdk",
            &[("include/a.h", b"header"), ("lib/a.a", b"lib")],
        )],
    };
    assert_eq!(first.digest(), moved.digest());
    let observed = first.entry(Path::new("/original/sdk/lib/a.a")).unwrap();
    first.admit(Path::new("/original/sdk/lib/a.a"), &observed).unwrap();
    moved.admit(Path::new("/moved/sdk/lib/a.a"), &observed).unwrap();
    assert!(moved.admit(Path::new("/original/sdk/lib/a.a"), &observed).is_err());
}

#[test]
fn role_and_search_order_are_part_of_the_closure_identity() {
    let first = canned("linker/libraries/0", "/first", &[("libsame.a", b"first")]);
    let second = canned("linker/libraries/1", "/second", &[("libsame.a", b"second")]);
    let original = Closure { roots: vec![first.clone(), second.clone()] };
    let reordered = Closure { roots: vec![second, first] };
    assert_ne!(original.digest(), reordered.digest());
    let mut renamed = original.clone();
    renamed.roots[0].name = "source/0".to_owned();
    assert_ne!(original.digest(), renamed.digest());
}

#[test]
fn complete_membership_proves_absence_without_authorizing_outside_paths() {
    let closure =
        Closure { roots: vec![canned("source", "/source", &[("include/present.h", b"header")])] };
    let absent = Entry { links: Vec::new(), kind: Kind::Missing };
    assert_eq!(closure.entry(Path::new("/source/include/missing.h")), Some(absent.clone()));
    assert_eq!(closure.entry(Path::new("/source/new-directory/missing.h")), Some(absent.clone()));
    closure.admit(Path::new("/source/include/missing.h"), &absent).unwrap();
    for path in [
        "/source/../outside",
        "/source/include/../../outside",
        "/sourced/present.h",
        "/outside/libc.so",
        "/source/include/present.h/child",
    ] {
        assert_eq!(closure.entry(Path::new(path)), None, "unexpected authority for {path}");
        assert!(closure.admit(Path::new(path), &absent).is_err());
    }
    let mut wrong = closure.entry(Path::new("/source/include/present.h")).unwrap();
    wrong.kind = Kind::Missing;
    assert!(closure.admit(Path::new("/source/include/present.h"), &wrong).is_err());
}

#[test]
fn a_bound_missing_root_proves_absence_until_it_appears() {
    let parent = tempfile::tempdir().unwrap();
    let path = parent.path().join("missing");
    let closure = captured("optional", &path);
    let absent = Entry { links: Vec::new(), kind: Kind::Missing };
    assert_eq!(closure.entry(&path), Some(absent.clone()));
    assert_eq!(closure.entry(&path.join("child/file")), Some(absent));
    let encoded = closure.encode_bindings();
    assert_eq!(Closure::from_bindings(&encoded).unwrap().digest(), closure.digest());
    fs::create_dir(&path).unwrap();
    assert!(Closure::from_bindings(&encoded).is_err());
    assert!(closure.validate().is_err());
}

#[test]
fn recapture_rejects_content_deletion_and_new_directory_members() {
    for mutation in ["bytes", "delete", "add-file", "add-directory"] {
        let root = tempfile::tempdir().unwrap();
        let file = root.path().join("native.a");
        fs::write(&file, b"first").unwrap();
        let modified = fs::metadata(&file).unwrap().modified().unwrap();
        let closure = captured("source", root.path());
        let encoded = closure.encode_bindings();
        assert_eq!(Closure::from_bindings(&encoded).unwrap().digest(), closure.digest());
        closure.validate().unwrap();
        match mutation {
            "bytes" => {
                fs::write(&file, b"other").unwrap();
                fs::File::options()
                    .write(true)
                    .open(&file)
                    .unwrap()
                    .set_modified(modified)
                    .unwrap();
            }
            "delete" => fs::remove_file(&file).unwrap(),
            "add-file" => fs::write(root.path().join("new.h"), b"new").unwrap(),
            "add-directory" => fs::create_dir(root.path().join("new-empty-directory")).unwrap(),
            _ => unreachable!(),
        }
        assert!(Closure::from_bindings(&encoded).is_err(), "decoded stale {mutation} binding");
        assert!(closure.validate().is_err(), "validated stale {mutation} binding");
    }
}

#[test]
fn binding_decoder_rejects_unknown_versions_duplicate_roles_and_relative_roots() {
    type Binding = (InputText, ConfigEnvValue, Digest);
    let root = tempfile::tempdir().unwrap();
    let closure = captured("source", root.path());
    let binding: Binding = (
        InputText("source".to_owned()),
        ConfigEnvValue::from_native(root.path().as_os_str()),
        closure.roots[0].tree.digest(),
    );
    assert!(Closure::from_bindings(&blaze_wire::encode(&(2_u32, vec![binding.clone()]))).is_err());
    assert!(
        Closure::from_bindings(&blaze_wire::encode(&(
            1_u32,
            vec![binding.clone(), binding.clone()]
        )))
        .is_err()
    );
    let relative = (
        InputText("source".to_owned()),
        ConfigEnvValue::from_native(Path::new("relative").as_os_str()),
        binding.2,
    );
    assert!(Closure::from_bindings(&blaze_wire::encode(&(1_u32, vec![relative]))).is_err());
    let workspace_relative = (
        InputText("source".to_owned()),
        ConfigEnvValue::WorkspaceRelative(blaze_wire::SourcePath::new("root".to_owned()).unwrap()),
        binding.2,
    );
    assert!(
        Closure::from_bindings(&blaze_wire::encode(&(1_u32, vec![workspace_relative]))).is_err()
    );
    assert!(Closure::from_bindings(b"not CBOR").is_err());
}

#[cfg(target_os = "linux")]
#[test]
fn binding_transport_preserves_raw_native_paths() {
    use std::{ffi::OsString, os::unix::ffi::OsStringExt};

    let parent = tempfile::tempdir().unwrap();
    let root = parent.path().join(OsString::from_vec(b"source-\xff".to_vec()));
    fs::create_dir(&root).unwrap();
    let file = root.join(OsString::from_vec(b"native-\xfe.a".to_vec()));
    fs::write(&file, b"library").unwrap();
    let closure = captured("source", &root);
    let decoded = Closure::from_bindings(&closure.encode_bindings()).unwrap();
    assert_eq!(decoded.roots[0].tree.root, root);
    assert_eq!(decoded.digest(), closure.digest());
    let observed = closure.entry(&file).unwrap();
    decoded.admit(&file, &observed).unwrap();
}

#[cfg(unix)]
#[test]
fn binding_recapture_rejects_symlink_target_changes_with_equal_referent_bytes() {
    use std::os::unix::fs::symlink;

    let parent = tempfile::tempdir().unwrap();
    fs::write(parent.path().join("first"), b"same").unwrap();
    fs::write(parent.path().join("second"), b"same").unwrap();
    let root = parent.path().join("source");
    fs::create_dir(&root).unwrap();
    let alias = root.join("native.a");
    symlink("../first", &alias).unwrap();
    let closure = captured("source", &root);
    let encoded = closure.encode_bindings();
    fs::remove_file(&alias).unwrap();
    symlink("../second", &alias).unwrap();
    assert!(Closure::from_bindings(&encoded).is_err());
    assert!(closure.validate().is_err());
}

#[test]
fn parent_components_require_observed_directories_and_keep_missing_prefixes_missing() {
    let root = tempfile::tempdir().unwrap();
    fs::create_dir(root.path().join("a")).unwrap();
    fs::create_dir(root.path().join("b")).unwrap();
    fs::write(root.path().join("b/library.a"), b"bound library").unwrap();
    let closure = captured("source", root.path());
    let path = root.path().join("a/../b/library.a");
    let actual = Tree::capture(&path).unwrap();
    closure.admit(&path, actual.entry(Path::new("")).unwrap()).unwrap();
    assert_eq!(closure.entry(&path), closure.entry(&root.path().join("b/library.a")));

    let missing = root.path().join("absent/../b/library.a");
    assert_eq!(fs::read(&missing).unwrap_err().kind(), std::io::ErrorKind::NotFound);
    assert_eq!(closure.entry(&missing), Some(Entry { links: Vec::new(), kind: Kind::Missing }));
    assert!(closure.entry(&root.path().join("b/library.a/../library.a")).is_none());
    assert!(closure.entry(&root.path().join("../outside.a")).is_none());

    fs::write(root.path().join("b/library.a"), b"later mutation").unwrap();
    let actual = Tree::capture(&path).unwrap();
    assert!(closure.admit(&path, actual.entry(Path::new("")).unwrap()).is_err());
}

#[cfg(unix)]
#[test]
#[allow(clippy::print_stderr)]
fn an_unreadable_directory_is_an_observation_and_refuses_reads_below_it() {
    use std::os::unix::fs::{PermissionsExt, symlink};

    let work = tempfile::tempdir().unwrap();
    let base = fs::canonicalize(work.path()).unwrap();
    let root = base.join("root");
    let private = base.join("external/private");
    fs::create_dir_all(root.join("include")).unwrap();
    fs::create_dir_all(&private).unwrap();
    fs::write(root.join("include/header.h"), b"header").unwrap();
    fs::write(private.join("secret.h"), b"secret").unwrap();
    // The denied directory is reached through a directory symlink, as Debian's
    // /usr/lib/ssl/private is reached from the captured /usr/lib.
    symlink("../external/private", root.join("keys")).unwrap();

    fs::set_permissions(&private, fs::Permissions::from_mode(0o000)).unwrap();
    let privileged = fs::read_dir(&private).is_ok();
    let first = Tree::capture(&root);
    let second = Tree::capture(&root);
    let single = Tree::capture(&root.join("keys"));
    let batch = Tree::capture_many(&[root.clone(), root.join("keys")]);
    let closure = first.as_ref().ok().map(|tree| Closure {
        roots: vec![Root { name: "linker/libraries/0".to_owned(), tree: tree.clone() }],
    });
    let encoded = closure.as_ref().map(Closure::encode_bindings);
    let decoded = encoded.as_ref().and_then(|bytes| Closure::from_bindings(bytes).ok());
    let validated = closure.as_ref().map(Closure::validate);
    fs::set_permissions(&private, fs::Permissions::from_mode(0o700)).unwrap();
    if privileged {
        eprintln!(
            "skipping: this process may read a 0o000 directory (running as root?), so a denied \
             listing cannot be observed"
        );
        return;
    }

    let tree = first.unwrap();
    let again = second.unwrap();
    assert_eq!(tree.entry(Path::new("keys")).unwrap().kind, Kind::Unreadable);
    assert_eq!(
        tree.entry(Path::new("keys")).unwrap().links,
        vec![std::path::PathBuf::from("../external/private")]
    );
    assert!(
        tree.entry(Path::new("keys/secret.h")).is_none(),
        "nothing below an unreadable directory is bound"
    );
    assert_eq!(tree.digest(), again.digest(), "the observation is stable");
    assert_eq!(tree.entries, again.entries);

    let single = single.unwrap();
    let batch = batch.unwrap();
    assert_eq!(batch[0].digest(), tree.digest());
    assert_eq!(batch[1].digest(), single.digest(), "a re-rooted alias agrees");

    let closure = closure.unwrap();
    assert_eq!(decoded.unwrap().digest(), closure.digest(), "the bound identity survives CBOR");
    validated.unwrap().unwrap();

    // Everything outside the denial is admitted; a toolchain that never reads below it works.
    let header = root.join("include/header.h");
    let observed = closure.entry(&header).unwrap();
    closure.admit(&header, &observed).unwrap();
    assert_eq!(closure.entry(&root.join("keys")).unwrap().kind, Kind::Unreadable);

    // A read below it is an unbound input, refused by name.
    let secret = Entry {
        links: Vec::new(),
        kind: Kind::File { digest: [0; 32], len: 6, executable: false },
    };
    let refused = closure.admit(&root.join("keys/secret.h"), &secret).unwrap_err();
    assert!(refused.to_string().contains("unbound native input"), "{refused}");
    assert!(refused.to_string().contains("keys/secret.h"), "{refused}");

    // The denial is part of the identity: once the members can be read, the binding is stale.
    assert_ne!(Tree::capture(&root).unwrap().digest(), closure.roots[0].tree.digest());
    assert!(Closure::from_bindings(&encoded.unwrap()).is_err());
    assert!(closure.validate().is_err());
}

#[cfg(unix)]
fn ancestor_path(aliased: bool) -> (tempfile::TempDir, std::path::PathBuf) {
    use std::os::unix::fs::symlink;
    let work = tempfile::tempdir().unwrap();
    let base = fs::canonicalize(work.path()).unwrap();
    let physical = base.join("physical");
    fs::create_dir(&physical).unwrap();
    let root = if aliased {
        let alias = base.join("ancestor-alias");
        symlink("physical", &alias).unwrap();
        alias
    } else {
        physical
    };
    (work, root)
}

#[test]
#[cfg(unix)]
fn symlink_parent_components_follow_the_referent_without_granting_an_escape() {
    use std::os::unix::fs::symlink;
    for aliased in [false, true] {
        let (_work, base) = ancestor_path(aliased);
        let source = base.join("source");
        let external = base.join("external");
        fs::create_dir_all(source.join("library")).unwrap();
        fs::create_dir_all(external.join("inner")).unwrap();
        fs::create_dir(external.join("library")).unwrap();
        fs::write(source.join("library/choice.a"), b"lexical choice is wrong").unwrap();
        fs::write(external.join("library/choice.a"), b"actual native choice").unwrap();
        symlink(&external.join("inner"), source.join("alias")).unwrap();
        let path = source.join("alias/../library/choice.a");
        assert_eq!(fs::read(&path).unwrap(), b"actual native choice");
        let source_only = captured("source", &source);
        assert!(source_only.entry(&path).is_none(), "the symlink's parent was not captured");
        let mut both = source_only;
        both.roots.push(Root {
            name: "native-dependency".into(),
            tree: Tree::capture(&external).unwrap(),
        });
        let actual = Tree::capture(&path).unwrap();
        both.admit(&path, actual.entry(Path::new("")).unwrap()).unwrap();
        assert_eq!(both.entry(&path), both.entry(&external.join("library/choice.a")));
        assert_ne!(both.entry(&path), both.entry(&source.join("library/choice.a")));
    }
}

#[test]
#[cfg(unix)]
fn observed_root_aliases_and_compiler_style_parent_roots_retain_physical_authority() {
    use std::os::unix::fs::symlink;
    for aliased in [false, true] {
        let (_work, root) = ancestor_path(aliased);
        fs::create_dir(root.join("a")).unwrap();
        fs::create_dir(root.join("b")).unwrap();
        let library = root.join("b/library.a");
        fs::write(&library, b"library").unwrap();
        let compiler_root = root.join("a/../b");
        symlink("b", root.join("alias")).unwrap();
        for bound in [compiler_root, root.join("alias")] {
            let closure = captured("linker/libraries/0", &bound);
            for path in [bound.join("library.a"), library.clone()] {
                let actual = Tree::capture(&path).unwrap();
                closure.admit(&path, actual.entry(Path::new("")).unwrap()).unwrap();
            }
        }
    }
}

#[cfg(unix)]
#[test]
fn nested_sysroot_aliases_retain_canonical_referent_authority() {
    use std::os::unix::fs::symlink;
    let temp = tempfile::tempdir().unwrap();
    let base = fs::canonicalize(temp.path()).unwrap();
    let sysroot = base.join("toolchain");
    let sources = base.join("rust-src");
    fs::create_dir_all(sysroot.join("lib/rustlib/src")).unwrap();
    fs::create_dir_all(sources.join("library/.cargo")).unwrap();
    let config = sources.join("library/.cargo/config.toml");
    fs::write(&config, b"[build]\n").unwrap();
    symlink(&sources, sysroot.join("lib/rustlib/src/rust")).unwrap();
    let closure = captured("toolchain/sysroot", &sysroot);
    let observed = Tree::capture(&config).unwrap();
    closure.admit(&config, observed.entry(Path::new("")).unwrap()).unwrap();
    let absent = Entry { links: Vec::new(), kind: Kind::Missing };
    closure.admit(&sources.join("library/.cargo/absent.toml"), &absent).unwrap();
    assert!(closure.entry(&base.join("uncaptured/file")).is_none());
    assert!(closure.entry(&sources.join("../uncaptured/file")).is_none());
    assert!(closure.entry(&config.join("child")).is_none());
    fs::write(&config, b"different").unwrap();
    let changed = Tree::capture(&config).unwrap();
    assert!(closure.admit(&config, changed.entry(Path::new("")).unwrap()).is_err());
}

#[cfg(unix)]
#[test]
fn physical_referents_traverse_nested_aliases_and_native_parent_rules() {
    use std::os::unix::fs::symlink;
    let temp = tempfile::tempdir().unwrap();
    let base = fs::canonicalize(temp.path()).unwrap();
    let sysroot = base.join("toolchain");
    let sources = base.join("rust-src");
    let external = base.join("external");
    fs::create_dir_all(sysroot.join("lib/src")).unwrap();
    fs::create_dir_all(sources.join("library")).unwrap();
    fs::create_dir(&external).unwrap();
    fs::write(external.join("native.a"), b"native").unwrap();
    symlink(&sources, sysroot.join("lib/src/rust")).unwrap();
    symlink(&external, sources.join("library/nested")).unwrap();
    let closure = captured("sysroot", &sysroot);
    let path = sources.join("library/nested/native.a");
    let observed = Tree::capture(&path).unwrap();
    closure.admit(&path, observed.entry(Path::new("")).unwrap()).unwrap();
    let missing = Entry { links: Vec::new(), kind: Kind::Missing };
    for path in [
        sources.join("library/nested/missing.a"),
        sources.join("library/absent/../nested/native.a"),
    ] {
        assert_eq!(closure.entry(&path), Some(missing.clone()));
    }
    assert!(closure.entry(&sources.join("library/nested/native.a/../missing")).is_none());
    assert!(closure.entry(&sources.join("library/nested/../uncaptured")).is_none());
}

#[cfg(unix)]
#[test]
fn physical_conflicts_are_terminal_through_logical_roots_and_ancestor_aliases() {
    use std::os::unix::fs::symlink;
    let temp = tempfile::tempdir().unwrap();
    let base = fs::canonicalize(temp.path()).unwrap();
    let physical = base.join("physical/source");
    let logical = base.join("logical/source");
    fs::create_dir_all(&physical).unwrap();
    symlink(base.join("physical"), base.join("logical")).unwrap();
    let file = physical.join("file");
    fs::write(&file, b"first").unwrap();
    let first = captured("first", &logical).roots.remove(0);
    fs::write(&file, b"other").unwrap();
    let second = captured("second", &physical).roots.remove(0);
    for roots in [vec![first.clone(), second.clone()], vec![second, first]] {
        let closure = Closure { roots };
        assert!(closure.entry(&file).is_none());
        assert!(closure.entry(&logical.join("file")).is_none());
    }
}

#[test]
fn a_poisoned_ancestor_cannot_be_skipped_for_an_indexed_descendant() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("changing");
    fs::create_dir(&path).unwrap();
    fs::write(path.join("file"), b"first").unwrap();
    let before = captured("before", &path).roots.remove(0);
    fs::remove_dir_all(&path).unwrap();
    fs::write(&path, b"now a file").unwrap();
    let after = captured("after", &path).roots.remove(0);
    for roots in [vec![before.clone(), after.clone()], vec![after, before]] {
        let closure = Closure { roots };
        assert!(closure.entry(&path).is_none());
        assert!(closure.entry(&path.join("file")).is_none());
    }
}

#[cfg(unix)]
#[test]
fn nested_directory_retargeting_with_equal_file_bytes_invalidates_bindings() {
    use std::os::unix::fs::symlink;
    let temp = tempfile::tempdir().unwrap();
    let base = fs::canonicalize(temp.path()).unwrap();
    let root = base.join("sysroot");
    fs::create_dir(&root).unwrap();
    for name in ["first", "second"] {
        fs::create_dir(base.join(name)).unwrap();
        fs::write(base.join(name).join("config.toml"), b"same").unwrap();
    }
    let alias = root.join("sources");
    symlink(base.join("first"), &alias).unwrap();
    let closure = captured("sysroot", &root);
    let bindings = closure.encode_bindings();
    fs::remove_file(&alias).unwrap();
    symlink(base.join("second"), &alias).unwrap();
    assert!(closure.validate().is_err());
    assert!(Closure::from_bindings(&bindings).is_err());
    let changed = base.join("second/config.toml");
    let observed = Tree::capture(&changed).unwrap();
    assert!(closure.admit(&changed, observed.entry(Path::new("")).unwrap()).is_err());
}

#[cfg(unix)]
#[test]
fn logical_aliases_cannot_jump_over_rejected_physical_ancestors() {
    use std::os::unix::fs::symlink;
    for case in ["conflicting", "file", "redirected"] {
        let temp = tempfile::tempdir().unwrap();
        let base = fs::canonicalize(temp.path()).unwrap();
        let parent = base.join("physical");
        fs::create_dir(&parent).unwrap();
        let file = parent.join("file");
        fs::write(&file, b"same").unwrap();
        let alias = base.join("logical-file");
        symlink(&file, &alias).unwrap();
        let mut roots = captured("logical", &alias).roots;
        if case == "conflicting" {
            roots.extend(captured("before", &parent).roots);
        }
        fs::remove_dir_all(&parent).unwrap();
        if case == "redirected" {
            let other = base.join("other");
            fs::create_dir(&other).unwrap();
            fs::write(other.join("file"), b"same").unwrap();
            symlink(&other, &parent).unwrap();
        } else {
            fs::write(&parent, b"now a file").unwrap();
        }
        roots.extend(captured("after", &parent).roots);
        let closure = Closure { roots };
        assert!(
            closure.entry(&alias).is_none(),
            "{case}: retained entry beyond rejected physical ancestor"
        );
    }
}

#[test]
fn an_admission_scope_checks_a_sequence_against_captured_values() {
    let root = tempfile::tempdir().unwrap();
    let tree = Tree::from_files(
        root.path(),
        (0..512).map(|i| (format!("entry-{i}").into(), b"abc".to_vec())),
    )
    .unwrap();
    let closure = Closure { roots: vec![Root { name: "source".into(), tree }] };
    let admission = closure.admission();
    let expected = Entry {
        links: Vec::new(),
        kind: Kind::File {
            digest: [
                0xba, 0x78, 0x16, 0xbf, 0x8f, 0x01, 0xcf, 0xea, 0x41, 0x41, 0x40, 0xde, 0x5d, 0xae,
                0x22, 0x23, 0xb0, 0x03, 0x61, 0xa3, 0x96, 0x17, 0x7a, 0x9c, 0xb4, 0x10, 0xff, 0x61,
                0xf2, 0x00, 0x15, 0xad,
            ],
            len: 3,
            executable: false,
        },
    };
    for i in 0..512 {
        let path = root.path().join(format!("entry-{i}"));
        assert_eq!(admission.entry(&path), Some(expected.clone()));
        admission.admit(&path, &expected).unwrap();
        assert!(admission.admit(&path, &Entry { links: vec![], kind: Kind::Missing }).is_err());
    }
    assert_eq!(
        admission.entry(&root.path().join("absent")),
        Some(Entry { links: vec![], kind: Kind::Missing })
    );
    assert!(admission.entry(&root.path().join("../outside")).is_none());
}

#[test]
fn a_new_admission_scope_observes_replaced_and_relocated_roots() {
    let work = tempfile::tempdir().unwrap();
    let first = work.path().join("first");
    let moved = work.path().join("moved");
    fs::create_dir(&first).unwrap();
    fs::write(first.join("file"), b"old").unwrap();
    let mut closure = captured("source", &first);
    let admission = closure.admission();
    let old = admission.entry(&first.join("file")).unwrap();
    fs::write(first.join("file"), b"new").unwrap();
    let new = Tree::capture(&first.join("file")).unwrap();
    assert!(admission.admit(&first.join("file"), new.entry(Path::new("")).unwrap()).is_err());
    assert_eq!(admission.entry(&first.join("file")), Some(old));
    drop(admission);
    closure.roots[0].tree = Tree::capture(&first).unwrap();
    let digest = closure.digest();
    let admission = closure.admission();
    let expected = admission.entry(&first.join("file")).unwrap();
    admission.admit(&first.join("file"), new.entry(Path::new("")).unwrap()).unwrap();
    drop(admission);
    fs::rename(&first, &moved).unwrap();
    closure.roots[0].tree = Tree::capture(&moved).unwrap();
    assert_eq!(closure.digest(), digest);
    let admission = closure.admission();
    assert_eq!(admission.entry(&moved.join("file")), Some(expected));
    assert!(admission.entry(&first.join("file")).is_none());
}

/// gcc names a search directory below its own with `..` (on Debian
/// `/usr/lib/gcc/aarch64-linux-gnu/12/../../../../aarch64-linux-gnu/lib/aarch64-linux-gnu/12`,
/// which need not exist), and binds its own directory too. A path below the `..`-spelled root is
/// that root's, though the shorter bound directory is a prefix of it as written: a file there is
/// admitted, and a name missing from it or from a missing root is proven absent. A `..` that
/// leaves the root is still refused.
#[test]
fn a_root_spelled_with_parent_components_answers_below_a_shorter_bound_prefix() {
    let work = tempfile::tempdir().unwrap();
    let base = fs::canonicalize(work.path()).unwrap();
    let gcc = base.join("usr/lib/gcc/12");
    let lib = base.join("usr/lib/triple");
    fs::create_dir_all(&gcc).unwrap();
    fs::create_dir_all(&lib).unwrap();
    fs::write(gcc.join("libgcc.a"), b"gcc").unwrap();
    let shared = lib.join("libgcc_s.so.1");
    fs::write(&shared, b"gcc_s").unwrap();
    fs::write(base.join("usr/lib/outside.a"), b"outside").unwrap();
    let spelled = gcc.join("../../triple");
    let absent = gcc.join("../../../absent/triple");
    let closure = Closure {
        roots: [&gcc, &spelled, &absent]
            .iter()
            .enumerate()
            .map(|(at, root)| Root {
                name: format!("linker/libraries/{at}"),
                tree: Tree::capture(root).unwrap(),
            })
            .collect(),
    };
    let admission = closure.admission();
    let below = spelled.join("libgcc_s.so.1");
    let actual = Tree::capture(&below).unwrap();
    admission.admit(&below, actual.entry(Path::new("")).unwrap()).unwrap();
    assert_eq!(admission.entry(&below), admission.entry(&shared));
    let missing = Some(Entry { links: Vec::new(), kind: Kind::Missing });
    assert_eq!(admission.entry(&spelled.join("libnone.so")), missing);
    assert_eq!(admission.entry(&absent.join("libgcc_s.so.1")), missing);

    let escape = spelled.join("../outside.a");
    assert_eq!(fs::read(&escape).unwrap(), b"outside");
    assert!(admission.entry(&escape).is_none());
    let actual = Tree::capture(&escape).unwrap();
    assert!(admission.admit(&escape, actual.entry(Path::new("")).unwrap()).is_err());
}

#[test]
fn parent_spelled_roots_allow_parent_traversal_inside_observed_directories() {
    let work = tempfile::tempdir().unwrap();
    let base = fs::canonicalize(work.path()).unwrap();
    let prefix = base.join("a");
    let library = base.join("b");
    fs::create_dir_all(&prefix).unwrap();
    fs::create_dir_all(library.join("sub")).unwrap();
    fs::write(library.join("libchoice.a"), b"chosen archive").unwrap();
    fs::write(base.join("outside.a"), b"not captured").unwrap();
    let spelled = prefix.join("../b");
    let closure = captured("linker/libraries/0", &spelled);
    let admission = closure.admission();
    let path = spelled.join("sub/../libchoice.a");
    assert_eq!(fs::read(&path).unwrap(), b"chosen archive");
    let actual = Tree::capture(&path).unwrap();
    admission.admit(&path, actual.entry(Path::new("")).unwrap()).unwrap();
    assert_eq!(admission.entry(&path), admission.entry(&library.join("libchoice.a")));
    assert!(admission.entry(&spelled.join("libchoice.a/../libchoice.a")).is_none());
    assert_eq!(
        admission.entry(&spelled.join("absent/../libchoice.a")),
        Some(Entry { links: vec![], kind: Kind::Missing })
    );
    let escape = spelled.join("sub/../../outside.a");
    assert_eq!(fs::read(&escape).unwrap(), b"not captured");
    assert!(admission.entry(&escape).is_none());
    drop(admission);
    let mut closure = closure;
    closure.roots.push(Root { name: "source".into(), tree: Tree::capture(&base).unwrap() });
    let actual = Tree::capture(&escape).unwrap();
    closure.admission().admit(&escape, actual.entry(Path::new("")).unwrap()).unwrap();
}
