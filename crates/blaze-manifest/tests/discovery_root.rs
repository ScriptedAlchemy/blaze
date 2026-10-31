//! A replay's snapshot root bounds the workspace walk as it bounds configuration discovery:
//! nothing above the directory a worker materialized the snapshot in is read.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use blaze_manifest::metadata::root_manifest;
use blaze_manifest::{Config, Context, Workspace};

const PACKAGE: &str = "[package]\nname='saved'\nversion='0.1.0'\nedition='2021'\n";

/// `<scratch>/<parent>/snapshot`, a single package with no `[workspace]` of its own, under a
/// parent whose `Cargo.toml` is `above` (none when `None`).
fn tree(scratch: &Path, parent: &str, above: Option<&str>) -> PathBuf {
    let snapshot = scratch.join(parent).join("snapshot");
    std::fs::create_dir_all(snapshot.join("src")).unwrap();
    std::fs::write(snapshot.join("Cargo.toml"), PACKAGE).unwrap();
    std::fs::write(snapshot.join("src/main.rs"), "fn main() {}\n").unwrap();
    if let Some(above) = above {
        std::fs::write(scratch.join(parent).join("Cargo.toml"), above).unwrap();
    }
    snapshot.canonicalize().unwrap()
}

/// The workspace root and members a reading of `snapshot` finds, or its error.
fn read(scratch: &Path, snapshot: &Path, bounded: bool) -> Result<(PathBuf, Vec<PathBuf>), String> {
    let home = scratch.join("cargo-home");
    std::fs::create_dir_all(&home).unwrap();
    let env = [
        ("CARGO_HOME".to_owned(), home.display().to_string()),
        // Configuration discovery stops at the scratch directory either way, so the only
        // difference `bounded` makes is the workspace walk's.
        ("__CARGO_TEST_ROOT".to_owned(), scratch.display().to_string()),
    ];
    (|| {
        let mut config = Config::load(snapshot, env, &[])?;
        if bounded {
            config = config.with_discovery_root(snapshot)?;
        }
        let ctx = Context::new(config);
        let manifest = root_manifest(None, &ctx)?;
        let ws = Workspace::new(&manifest, &ctx)?;
        let members = ws.members().map(|(path, _)| path.to_owned()).collect();
        anyhow::Ok((ws.root().to_owned(), members))
    })()
    .map_err(|error| format!("{error:#}"))
}

struct Scratch(PathBuf);

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn a_replay_reads_no_manifest_above_its_snapshot_root() {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let scratch = std::env::temp_dir().join(format!(
        "blaze-discovery-root-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&scratch).unwrap();
    let scratch = Scratch(scratch.canonicalize().unwrap());
    let scratch = scratch.0.as_path();
    let empty = tree(scratch, "empty", None);
    let broken = tree(scratch, "broken", Some("[package\nthis is not a manifest"));
    let claiming = tree(scratch, "claiming", Some("[workspace]\nmembers = ['snapshot']\n"));

    // Unbounded, as for an original request, cargo's walk reads both parents.
    let error = read(scratch, &broken, false).unwrap_err();
    assert!(error.contains("failed searching for potential workspace"), "{error}");
    let (root, _) = read(scratch, &claiming, false).unwrap();
    assert_eq!(root, scratch.join("claiming"));

    // Bounded at the snapshot, each replays exactly as under an empty parent.
    let relative = |snapshot: &Path, (root, members): (PathBuf, Vec<PathBuf>)| {
        let members: Vec<_> =
            members.iter().map(|path| path.strip_prefix(snapshot).unwrap().to_owned()).collect();
        (root.strip_prefix(snapshot).unwrap().to_owned(), members)
    };
    let expected = relative(&empty, read(scratch, &empty, true).unwrap());
    assert_eq!(expected, (PathBuf::new(), vec![PathBuf::from("Cargo.toml")]));
    for snapshot in [&broken, &claiming] {
        assert_eq!(relative(snapshot, read(scratch, snapshot, true).unwrap()), expected);
    }
}
