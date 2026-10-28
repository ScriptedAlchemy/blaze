use super::*;

/// A directory of its own for the test `test`, with `files` in it.
struct Package(PathBuf);

impl Package {
    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for Package {
    fn drop(&mut self) {
        _ = std::fs::remove_dir_all(&self.0);
    }
}

fn package(test: &str, files: &[(&str, &str)]) -> Package {
    let dir = std::env::temp_dir()
        .join(format!("blaze-build-script-files-{test}-{}", std::process::id()));
    _ = std::fs::remove_dir_all(&dir);
    for (name, text) in files {
        let path = dir.join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }
    Package(dir)
}

fn listed(root: &Path, include: &[&str], exclude: &[&str]) -> Vec<String> {
    let owned = |it: &[&str]| it.iter().map(|it| (*it).to_owned()).collect::<Vec<_>>();
    let mut files: Vec<String> = list_files(root, &owned(include), &owned(exclude))
        .unwrap()
        .iter()
        .map(|it| it.strip_prefix(root).unwrap().display().to_string())
        .collect();
    files.sort();
    files
}

fn git(dir: &Path, args: &[&str]) -> bool {
    Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@t")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@t")
        .output()
        .is_ok_and(|it| it.status.success())
}

const FILES: &[(&str, &str)] = &[
    ("Cargo.toml", "[package]\n"),
    ("Cargo.lock", ""),
    ("build.rs", ""),
    ("src/main.rs", ""),
    (".hidden", ""),
    ("assets/CACHEDIR.TAG", "Signature: not cargo's\n"),
    ("assets/data.txt", ""),
    ("target/out.txt", ""),
    ("inner/Cargo.toml", "[package]\n"),
    ("inner/src/lib.rs", ""),
    ("sub/target/kept.txt", ""),
];

#[test]
fn a_package_outside_git_is_walked_without_hidden_files_target_or_packages_inside() {
    let dir = package("walk", FILES);
    let root = dir.path();
    if git_repo(root).is_some() {
        return; // the temporary directory is in a repository: not this case
    }
    assert_eq!(
        listed(root, &[], &[]),
        [
            "Cargo.lock",
            "Cargo.toml",
            "assets/CACHEDIR.TAG",
            "assets/data.txt",
            "build.rs",
            "src/main.rs",
            "sub/target/kept.txt"
        ]
    );
}

#[test]
fn exclude_and_include_are_matched_as_gitignore_lines() {
    let dir = package("rules", FILES);
    let root = dir.path();
    if git_repo(root).is_some() {
        return;
    }
    assert_eq!(
        listed(root, &[], &["assets/", "*.rs", "!build.rs"]),
        ["Cargo.lock", "Cargo.toml", "build.rs", "sub/target/kept.txt"]
    );
    // An include lists files, hidden ones too, and the manifest is always among them.
    assert_eq!(
        listed(root, &["assets/**", ".hidden"], &[]),
        [".hidden", "Cargo.lock", "Cargo.toml", "assets/CACHEDIR.TAG", "assets/data.txt"]
    );
    assert_eq!(listed(root, &["/src/*.rs"], &[]), ["Cargo.lock", "Cargo.toml", "src/main.rs"]);
}

#[test]
fn a_package_git_tracks_is_listed_from_git() {
    let dir = package("git", FILES);
    let root = dir.path();
    if !git(root, &["init", "-q"]) {
        return; // no git here
    }
    std::fs::write(root.join(".gitignore"), "ignored.txt\n/target\n").unwrap();
    std::fs::write(root.join("ignored.txt"), "").unwrap();
    std::fs::write(root.join("tracked-ignored.txt"), "").unwrap();
    std::fs::write(root.join("tracked-ignored.txt.keep"), "").unwrap();
    assert!(git(root, &["add", "Cargo.toml", "build.rs", "src/main.rs", ".gitignore"]));
    // Untracked: not listed from git until the manifest is tracked.
    assert!(git(root, &["add", "-f", "target/out.txt"]));
    assert!(git(root, &["commit", "-qm", "x"]));
    let files = listed(root, &[], &[]);
    // Tracked files in `target` are listed, hidden ones too, untracked ones git does not
    // ignore, and no untracked `Cargo.lock`.
    assert_eq!(
        files,
        [
            ".gitignore",
            ".hidden",
            "Cargo.toml",
            "assets/CACHEDIR.TAG",
            "assets/data.txt",
            "build.rs",
            "src/main.rs",
            "sub/target/kept.txt",
            "target/out.txt",
            "tracked-ignored.txt",
            "tracked-ignored.txt.keep"
        ]
    );
}

#[cfg(unix)]
fn supports_non_utf8_paths(root: &Path) -> bool {
    use std::os::unix::ffi::OsStrExt as _;

    let file = root.join(std::ffi::OsStr::from_bytes(b"probe-\xff"));
    match std::fs::write(&file, "") {
        Ok(()) => {
            std::fs::remove_file(file).unwrap();
            true
        }
        Err(error) => {
            // APFS can reject ill-formed names. This is a filesystem capability probe,
            // not a platform-wide exclusion: Unix byte-path decoding is tested below,
            // and the complete tracked/untracked/directory fixtures still run on Linux.
            #[cfg(target_vendor = "apple")]
            if error.raw_os_error() == Some(libc::EILSEQ) {
                eprintln!(
                    "filesystem rejects non-UTF-8 names (EILSEQ); byte decoding is tested separately"
                );
                return false;
            }
            panic!("non-UTF-8 path capability probe failed: {error}");
        }
    }
}

#[cfg(unix)]
#[test]
fn git_path_decodes_non_utf8_bytes_without_a_filesystem() {
    use std::os::unix::ffi::OsStrExt as _;

    let bytes = b"checkout-\xff/tracked-\xfe";
    assert_eq!(git_path(bytes).unwrap().as_os_str().as_bytes(), bytes);
}

#[cfg(unix)]
#[test]
fn git_listed_paths_do_not_need_to_be_utf8() {
    use std::os::unix::ffi::OsStrExt as _;

    let dir = package("git-bytes", &[("Cargo.toml", "[package]\n")]);
    let root = dir.path();
    if !supports_non_utf8_paths(root) {
        return;
    }
    assert!(git(root, &["init", "-q"]));
    let tracked = root.join(std::ffi::OsStr::from_bytes(b"tracked-\xff"));
    std::fs::write(&tracked, "old").unwrap();
    assert!(git(root, &["add", "."]));
    let untracked = root.join(std::ffi::OsStr::from_bytes(b"untracked-\xfe"));
    std::fs::write(&untracked, "new").unwrap();
    let files = list_files(root, &[], &[]).unwrap();
    assert!(files.contains(&tracked), "a tracked script input must be fingerprinted");
    assert!(files.contains(&untracked), "an untracked script input must be fingerprinted");
}

#[cfg(unix)]
#[test]
fn a_non_utf8_checkout_is_still_listed_from_git() {
    use std::os::unix::ffi::OsStrExt as _;

    let dir = package("git-root-bytes", &[("placeholder", "")]);
    if !supports_non_utf8_paths(dir.path()) {
        return;
    }
    let root = dir.path().join(std::ffi::OsStr::from_bytes(b"checkout-\xff"));
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("Cargo.toml"), "[package]\n").unwrap();
    assert!(git(&root, &["init", "-q"]));
    assert!(git(&root, &["add", "Cargo.toml"]));
    std::fs::write(root.join(".hidden-input"), "").unwrap();
    assert_eq!(listed(&root, &[], &[]), [".hidden-input", "Cargo.toml"]);
}

#[test]
fn the_newest_file_is_the_first_of_the_latest_time() {
    let dir = package("newest", &[("a", ""), ("b", "")]);
    let (a, b) = (dir.path().join("a"), dir.path().join("b"));
    let at = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000);
    for file in [&a, &b] {
        std::fs::File::options().write(true).open(file).unwrap().set_modified(at).unwrap();
    }
    assert_eq!(last_modified(&[a.clone(), b.clone()]), (at, a.clone()));
    assert_eq!(last_modified(&[dir.path().join("gone")]), (SystemTime::UNIX_EPOCH, PathBuf::new()));
}

#[test]
fn globs_match_as_gitignore_does() {
    assert!(glob("*.rs", "main.rs"));
    assert!(!glob("*.rs", "main.rsx"));
    assert!(glob("ma?n.[a-r]s", "main.rs"));
    assert!(!glob("[!m]ain.rs", "main.rs"));
    assert!(segments(&["**".into(), "b".into()], &["a".into(), "x".into(), "b".into()]));
    let mut rules = Rules::default();
    rules.add("build/");
    rules.add("*.log");
    rules.add("!keep.log");
    assert!(rules.matched_path_or_any_parents(Path::new("build/x.rs"), false));
    assert!(!rules.matched_path_or_any_parents(Path::new("build"), false));
    assert!(rules.matched_path_or_any_parents(Path::new("a/b.log"), false));
    assert!(!rules.matched_path_or_any_parents(Path::new("a/keep.log"), false));
}
