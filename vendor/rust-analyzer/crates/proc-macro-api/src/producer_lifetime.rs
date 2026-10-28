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
const INVALID: &str = ".blaze-producer-invalid";
const PENDING: &str = ".blaze-producer-pending-";
static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Retire under the root lock before recursive removal can destroy the marker. A crash
/// halfway through removal then leaves only a pending name, never a blocked content id.
fn retire(generation: &Path) -> io::Result<PathBuf> {
    let root = generation.parent().ok_or_else(|| io::Error::other("producer without root"))?;
    loop {
        let pending = root.join(format!(
            "{PENDING}remove-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        if pending.try_exists()? {
            continue;
        }
        std::fs::rename(generation, &pending)?;
        return Ok(pending);
    }
}

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
    if !generation.exists() {
        let pending = root.join(format!(
            "{PENDING}{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::create_dir(&pending)?;
        // Only a complete marker and lease are ever published under the content id.
        // A crash leaves a pending directory that collection can reclaim under this lock.
        let published = (|| {
            let mut marker =
                File::options().write(true).create_new(true).open(pending.join(MARKER))?;
            marker.write_all(FORMAT.as_bytes())?;
            marker.sync_all()?;
            File::options().write(true).create_new(true).open(pending.join(LEASE))?;
            std::fs::rename(&pending, generation)
        })();
        if published.is_err() {
            let _ = std::fs::remove_dir_all(&pending);
        }
        published?;
    } else if generation
        .file_name()
        .and_then(|it| it.to_str())
        .is_some_and(|name| name.len() == 64 && name.bytes().all(|byte| byte.is_ascii_hexdigit()))
        && std::fs::read_dir(generation)?.next().is_none()
    {
        // Recover an empty content-id directory left by the older non-atomic lease.
        std::fs::remove_dir(generation)?;
        drop(_root);
        return lease(generation);
    }
    ordinary(&generation.join(MARKER))?;
    ordinary(&generation.join(LEASE))?;
    if std::fs::read_to_string(generation.join(MARKER))? != FORMAT {
        return Err(io::Error::other("unrecognized producer generation"));
    }
    ordinary(&generation.join(INVALID))?;
    if generation.join(INVALID).try_exists()? {
        // Retain OUT_DIR paths of older consumers. Retry is possible when they retire.
        let lock = File::options()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(generation.join(LEASE))?;
        lock.try_lock()
            .map_err(|_| io::Error::other("invalid producer generation still leased"))?;
        std::fs::remove_dir_all(retire(generation)?)?;
        drop(_root);
        return lease(generation);
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

/// A failed build keeps its directory for live OUT_DIR consumers, but it is never reused.
pub fn invalidate(generation: &Path) -> io::Result<()> {
    let _root = locked_root(generation.parent().ok_or_else(|| io::Error::other("no root"))?)?;
    ordinary(&generation.join(INVALID))?;
    File::options().write(true).create(true).truncate(false).open(generation.join(INVALID))?;
    Ok(())
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
        let generation = match entry {
            Ok(entry) => entry.path(),
            Err(error) => {
                eprintln!("macro producers: skipped collection entry: {error}");
                continue;
            }
        };
        let pending = generation
            .file_name()
            .and_then(|it| it.to_str())
            .is_some_and(|name| name.starts_with(PENDING));
        if generation == current
            || ordinary(&generation).is_err()
            || !generation.is_dir()
            || !pending
                && std::fs::read_to_string(generation.join(MARKER)).ok().as_deref() != Some(FORMAT)
        {
            continue;
        }
        let remove = (|| {
            ordinary(&generation.join(MARKER))?;
            ordinary(&generation.join(LEASE))?;
            // A complete old marker without a lease is unleased, not a fatal GC error.
            let lock = File::options()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .open(generation.join(LEASE))?;
            if lock.try_lock().is_err() {
                return Ok(false);
            }
            let pending = if pending { generation.clone() } else { retire(&generation)? };
            std::fs::remove_dir_all(pending)?;
            Ok::<_, io::Error>(true)
        })();
        match remove {
            Ok(true) => removed.push(generation),
            Ok(false) => {}
            Err(error) => eprintln!("macro producers: skipped {}: {error}", generation.display()),
        }
    }
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interrupted_removal_never_blocks_the_content_id() {
        let root = stdx::tempfile::NamedTempDir::new("producer-remove-crash").unwrap();
        let generation = root.path().join("a".repeat(64));
        drop(lease(&generation).unwrap());
        let pending = {
            let _root = locked_root(root.path()).unwrap();
            retire(&generation).unwrap()
        };
        // A SIGKILL during removal can leave payloads after the marker is gone.
        std::fs::remove_file(pending.join(MARKER)).unwrap();
        std::fs::write(pending.join("partially-removed"), b"old output").unwrap();
        let new = lease(&generation).unwrap();
        assert_eq!(std::fs::read_to_string(generation.join(MARKER)).unwrap(), FORMAT);
        assert_eq!(collect(root.path(), &generation).unwrap(), [pending.clone()]);
        assert!(!pending.exists());
        assert!(!generation.join("partially-removed").exists());
        drop(new);
    }

    #[test]
    fn interrupted_claims_and_collection_are_recoverable() {
        let root = stdx::tempfile::NamedTempDir::new("producer-crash").unwrap();
        let bare = root.path().join("a".repeat(64));
        std::fs::create_dir(&bare).unwrap();
        let held = lease(&bare).unwrap();
        assert_eq!(std::fs::read_to_string(bare.join(MARKER)).unwrap(), FORMAT);
        let pending = root.path().join(format!("{PENDING}crashed"));
        std::fs::create_dir(&pending).unwrap();
        let marker_only = root.path().join("marker-only");
        std::fs::create_dir(&marker_only).unwrap();
        std::fs::write(marker_only.join(MARKER), FORMAT).unwrap();
        let malformed = root.path().join("malformed-lease");
        std::fs::create_dir(&malformed).unwrap();
        std::fs::write(malformed.join(MARKER), FORMAT).unwrap();
        std::fs::create_dir(malformed.join(LEASE)).unwrap();
        let healthy = root.path().join("healthy");
        drop(lease(&healthy).unwrap());
        let removed = collect(root.path(), &bare).unwrap();
        assert!(removed.contains(&pending) && removed.contains(&marker_only));
        assert!(removed.contains(&healthy), "one bad entry must not stop collection");
        assert!(collect(root.path(), &bare).unwrap().is_empty());
        invalidate(&bare).unwrap();
        assert!(lease(&bare).is_err(), "live invalid OUT_DIR must not be deleted");
        drop(held);
        let recovered = lease(&bare).unwrap();
        assert!(!bare.join(INVALID).exists());
        drop(recovered);
        assert_eq!(collect(root.path(), &root.path().join("current")).unwrap(), [bare]);
    }

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
