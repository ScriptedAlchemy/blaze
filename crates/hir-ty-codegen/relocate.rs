//! Where a path is, written so that another machine reads it as its own.
//!
//! A panic location, `file!()` and debug info name the files of the crates a program is built
//! from as rustc spells them: a crate of the workspace by its place in it, any other by its path on
//! this machine. What a store keeps of such a file has to be the same on every machine, and read as
//! that machine's own. [`super::CARGO_HOME`] stands in the store for the directory cargo fetched
//! into. [`CHECKOUT`] stands for the checkout, the workspace's root: a file below it is written
//! below it, and a file of a crate that is beside it (a path dependency in the same repository,
//! `path = "../shared"`) is written below it with the `..` that reaches it, so that two machines
//! that have the repository at different places have each other's entries.
//!
//! A path that is neither is written as it is, which no other machine has. Two directories have to
//! be shared by the checkout and the path, [`SHARED`], for the path to be beside it: a path under
//! `/opt` and a checkout under `/home` are not in one repository, and `../../../opt/x` would be
//! another place on a machine whose checkout is deeper.

use std::path::{Component, Path, PathBuf};

/// What stands for the checkout's directory in what is stored.
pub const CHECKOUT: &str = "/checkout";

/// How many named directories a path and the checkout share, at least, for it to be beside it.
pub const SHARED: usize = 2;

fn named(path: &Path) -> usize {
    path.components().filter(|it| matches!(it, Component::Normal(_))).count()
}

/// `path` as the store names it, when it is below the checkout `root` or beside it: `/checkout`,
/// then the `..` that reach it from the root and what is below the directory they reach.
///
/// `None` for a path that is relative (a file of the workspace is, and is the same on every
/// machine) and one that shares fewer than [`SHARED`] directories with the root.
pub fn stored_as(path: &str, root: &str) -> Option<String> {
    let text = path;
    let (path, root) = (Path::new(path), Path::new(root));
    if !path.is_absolute() || !root.is_absolute() {
        return None;
    }
    let common = path.ancestors().find(|it| root.starts_with(it))?;
    if named(common) < SHARED {
        return None;
    }
    let up = named(root.strip_prefix(common).ok()?);
    // Only the common directory and generated leading traversal are structural.
    // The tail can be concat!(file!(), "/../tag") text, not a filesystem path.
    let common_text = common.to_str()?;
    let down = text.strip_prefix(common_text)?.strip_prefix('/')?;
    let mut stored = String::from(CHECKOUT);
    for _ in 0..up {
        stored.push_str("/..");
    }
    stored.push('/');
    stored.push_str(down);
    Some(stored)
}

/// What [`stored_as`] named as `root` has it: the checkout's directory, up as many as the `..`
/// say and down as the rest of the path goes. `None` for a path the store does not name so, and
/// one that goes up from the root past its top.
pub fn read_as(stored: &str, root: &str) -> Option<String> {
    let mut rest = stored.strip_prefix(CHECKOUT)?.strip_prefix('/')?;
    let mut path = PathBuf::from(root);
    // Interpret only the leading ".." emitted by stored_as. Everything after the
    // first ordinary segment is exact program text, including slash/dot suffixes.
    while let Some(tail) = rest.strip_prefix("../") {
        if !path.pop() {
            return None;
        }
        rest = tail;
    }
    Some(format!("{}/{rest}", path.to_str()?.trim_end_matches('/')))
}

/// Whether `path` is already named as the store names a place of the checkout, and so would be
/// read back as one: it can't be kept by that name, as a path of cargo's directory can't.
pub fn names_checkout(path: &str) -> bool {
    path.strip_prefix(CHECKOUT).is_some_and(|it| it.starts_with('/'))
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn a_path_below_the_checkout_is_written_below_it() {
        let stored = stored_as("/home/a/repo/ws/vendor/dep/src/lib.rs", "/home/a/repo/ws");
        assert_eq!(stored.as_deref(), Some("/checkout/vendor/dep/src/lib.rs"));
        assert_eq!(
            read_as("/checkout/vendor/dep/src/lib.rs", "/Users/b/work/src/ws").as_deref(),
            Some("/Users/b/work/src/ws/vendor/dep/src/lib.rs"),
        );
    }

    #[test]
    fn a_path_beside_the_checkout_is_written_with_the_dots_that_reach_it() {
        let stored = stored_as("/home/a/repo/shared/src/lib.rs", "/home/a/repo/ws");
        assert_eq!(stored.as_deref(), Some("/checkout/../shared/src/lib.rs"));
        assert_eq!(
            read_as("/checkout/../shared/src/lib.rs", "/private/tmp/m/b/x/repo/ws").as_deref(),
            Some("/private/tmp/m/b/x/repo/shared/src/lib.rs"),
        );
        // Two directories up, from a workspace in a crates directory.
        let stored = stored_as("/r/p/libs/x/lib.rs", "/r/p/crates/ws");
        assert_eq!(stored.as_deref(), Some("/checkout/../../libs/x/lib.rs"));
    }

    #[test]
    fn what_each_machine_names_is_what_the_other_reads() {
        let (a, b) = ("/home/a/repo/ws", "/Users/b/some/deeper/place/repo/ws");
        for file in
            ["shared/src/lib.rs", "ws/vendor/dep/lib.rs", "ws/src/main.rs", "other/dir/x.rs"]
        {
            let on_a = stored_as(&format!("/home/a/repo/{file}"), a);
            let on_b = stored_as(&format!("/Users/b/some/deeper/place/repo/{file}"), b);
            assert_eq!(on_a, on_b, "{file}");
            assert_eq!(
                read_as(on_a.as_deref().unwrap(), b).as_deref(),
                Some(&*format!("/Users/b/some/deeper/place/repo/{file}")),
                "{file}",
            );
        }
    }

    #[test]
    fn a_path_of_another_part_of_the_disk_is_not_beside_the_checkout() {
        assert_eq!(stored_as("/opt/shared/src/lib.rs", "/home/a/repo/ws"), None);
        // One directory in common is not a repository: `/home` is everyone's.
        assert_eq!(stored_as("/home/c/other/lib.rs", "/home/a/repo/ws"), None);
        // Relative: a file of the workspace, which every machine spells alike.
        assert_eq!(stored_as("inner/src/lib.rs", "/home/a/repo/ws"), None);
    }

    #[test]
    fn a_stored_path_that_goes_up_past_the_top_is_not_read() {
        assert_eq!(read_as("/checkout/../../../../x", "/a/b"), None);
        assert_eq!(read_as("/checkoutx/y", "/a/b"), None);
        assert_eq!(read_as("/cargo/registry/src/x", "/a/b"), None);
    }

    #[test]
    fn a_path_named_as_a_place_of_the_checkout_is_told() {
        assert!(names_checkout("/checkout/src/lib.rs"));
        assert!(!names_checkout("/checkout"));
        assert!(!names_checkout("/checkouts/x"));
        assert!(!names_checkout("src/lib.rs"));
    }

    #[test]
    fn literal_suffixes_are_not_filesystem_traversal() {
        for tail in ["shared/src/lib.rs/../tag", "shared/src/lib.rs//./tag///"] {
            let text = format!("/home/a/repo/{tail}");
            let stored = stored_as(&text, "/home/a/repo/ws").unwrap();
            assert_eq!(stored, format!("/checkout/../{tail}"));
            assert_eq!(
                read_as(&stored, "/Users/b/repo/ws").unwrap(),
                format!("/Users/b/repo/{tail}")
            );
            assert_eq!(read_as(&stored, "/home/a/repo/ws").unwrap(), text);
        }
    }
}
