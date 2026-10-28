//! Owned producer directories. Builds and loaded libraries hold shared leases.
//! Collection removes only marked generations with no live lease.

use std::{
    fs::File,
    io,
    io::Write,
    path::{Path, PathBuf},
};

const MARKER: &str = ".blaze-producer-generation";
const LEASE: &str = ".blaze-producer-lease";
const FORMAT: &str = "blaze producer generation 1\n";

fn ordinary(path: &Path) -> io::Result<()> {
    // Check the owned path and its parent, not platform aliases such as /var -> /private/var.
    for ancestor in path.ancestors().take(2) {
        if std::fs::symlink_metadata(ancestor)
            .is_ok_and(|metadata| metadata.file_type().is_symlink())
        {
            return Err(io::Error::other(format!("symlink producer path: {}", ancestor.display())));
        }
    }
    Ok(())
}

fn locked_root(root: &Path) -> io::Result<File> {
    ordinary(root)?;
    std::fs::create_dir_all(root)?;
    let path = root.join(".blaze-producer-collection");
    ordinary(&path)?;
    let lock = File::options().read(true).write(true).create(true).truncate(false).open(path)?;
    lock.lock()?;
    Ok(lock)
}

/// Claim a newly created directory, or lease one already carrying our format.
/// The root lock serializes claim/lease with collection; it is not a build lock.
pub fn lease(generation: &Path) -> io::Result<File> {
    let root = generation.parent().ok_or_else(|| io::Error::other("producer without root"))?;
    let _root = locked_root(root)?;
    ordinary(generation)?;
    let created = match std::fs::create_dir(generation) {
        Ok(()) => true,
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => false,
        Err(error) => return Err(error),
    };
    ordinary(&generation.join(MARKER))?;
    ordinary(&generation.join(LEASE))?;
    if created {
        let mut marker =
            File::options().write(true).create_new(true).open(generation.join(MARKER))?;
        marker.write_all(FORMAT.as_bytes())?;
    } else if std::fs::read_to_string(generation.join(MARKER))? != FORMAT {
        return Err(io::Error::other("unrecognized producer generation"));
    }
    let lock = File::options()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(generation.join(LEASE))?;
    lock.lock_shared()?;
    Ok(lock)
}

pub(crate) fn lease_for_library(library: &Path) -> io::Result<Option<File>> {
    for generation in library.ancestors() {
        if generation.join(MARKER).is_file() {
            return lease(generation).map(Some);
        }
    }
    Ok(None)
}

pub fn collect(root: &Path, current: &Path) -> io::Result<Vec<PathBuf>> {
    ordinary(root)?;
    let mut removed = Vec::new();
    if !root.exists() {
        return Ok(removed);
    }
    let _root = locked_root(root)?;
    for entry in std::fs::read_dir(root)? {
        let generation = entry?.path();
        if generation == current
            || ordinary(&generation).is_err()
            || !generation.is_dir()
            || std::fs::read_to_string(generation.join(MARKER)).ok().as_deref() != Some(FORMAT)
        {
            continue;
        }
        ordinary(&generation.join(MARKER))?;
        ordinary(&generation.join(LEASE))?;
        let lock = File::options().read(true).write(true).open(generation.join(LEASE))?;
        if lock.try_lock().is_err() {
            continue;
        }
        std::fs::remove_dir_all(&generation)?;
        removed.push(generation);
    }
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collector_keeps_live_generations_and_foreign_directories() {
        let root = stdx::tempfile::NamedTempDir::new("producer-lifetime").unwrap();
        let current = root.path().join("current");
        let old = root.path().join("old");
        let foreign = root.path().join("foreign");
        let _current = lease(&current).unwrap();
        let old_lease = lease(&old).unwrap();
        std::fs::create_dir(&foreign).unwrap();
        assert!(collect(root.path(), &current).unwrap().is_empty());
        assert!(old.exists());
        drop(old_lease);
        assert_eq!(collect(root.path(), &current).unwrap(), [old.clone()]);
        assert!(!old.exists());
        assert!(current.exists() && foreign.exists());
        assert!(lease(&foreign).is_err(), "never claim somebody else's existing directory");
    }

    #[cfg(unix)]
    #[test]
    fn library_consumer_lease_keeps_its_generation_and_symlinks_are_not_collected() {
        use std::os::unix::fs::symlink;
        let root = stdx::tempfile::NamedTempDir::new("producer-library-lease").unwrap();
        let old = root.path().join("old");
        let build = lease(&old).unwrap();
        std::fs::create_dir(old.join("deps")).unwrap();
        let library = old.join("deps/library");
        std::fs::write(&library, b"bytes").unwrap();
        let consumer = lease_for_library(&library).unwrap().unwrap();
        drop(build);
        assert!(collect(root.path(), &root.path().join("current")).unwrap().is_empty());
        drop(consumer);
        assert_eq!(collect(root.path(), &root.path().join("current")).unwrap(), [old]);
        let foreign = stdx::tempfile::NamedTempDir::new("producer-foreign").unwrap();
        symlink(foreign.path(), root.path().join("link")).unwrap();
        assert!(lease(&root.path().join("link")).is_err());
        assert!(collect(root.path(), &root.path().join("current")).unwrap().is_empty());
        assert!(foreign.path().exists());
    }
}
