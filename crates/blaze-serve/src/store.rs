//! The directory the server serves.
//!
//! Every pack is checked before it becomes visible, so a name only ever holds what it says:
//! `packs/<d>.pack` is a sealed pack whose bytes hash to `<d>`.
//!
//! A write goes to `.<name>.<pid>-<nanos>.tmp` in the same directory, is synced, renamed, and
//! the directory synced, as the store writes its own files, so a partially written blob never
//! appears: a reader sees all of it or none.

use std::{
    fs::File,
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::{
        Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::{SystemTime, UNIX_EPOCH},
};

use blaze_wire::Digest;
use sha2::{Digest as _, Sha256};

use crate::blob::{ObjectName, hex};
use crate::error::{Error, failed};

/// The directory, shared by every connection.
pub struct Store {
    dir: PathBuf,
    /// A list's write holds this across its look and its rename: a list is written once.
    publication: Mutex<()>,
    #[cfg(test)]
    before_name: Mutex<Option<Box<dyn Fn() + Send>>>,
}

impl Store {
    /// The directory `dir`. A directory that is not there is made, so that the server can be
    /// pointed at a new one.
    pub fn open(dir: &Path) -> Result<Store, Error> {
        for sub in ["packs", "manifests"] {
            let at = dir.join(sub);
            std::fs::create_dir_all(&at)
                .map_err(|e| failed(&format!("making {}", at.display()), e))?;
            if std::fs::symlink_metadata(&at)?.file_type().is_symlink() {
                return Err(Error::malformed(format!(
                    "{} is a symlink, not a cache directory",
                    at.display()
                )));
            }
        }
        let store = Store {
            dir: dir.canonicalize()?,
            publication: Mutex::new(()),
            #[cfg(test)]
            before_name: Mutex::new(None),
        };
        store.sweep_temporaries(std::time::Duration::from_secs(3600))?;
        Ok(store)
    }

    /// The directory it serves.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub(crate) fn checked_path(&self, name: &str) -> Result<PathBuf, Error> {
        Ok(self.object(name)?.path(&self.dir))
    }

    /// The object under `name`, or `None` when nothing is there.
    pub fn open_blob(&self, name: &str) -> Result<Option<(File, u64)>, Error> {
        let object = self.object(name)?;
        match File::open(object.path(&self.dir)) {
            Ok(file) => {
                let len = file.metadata().map_err(|e| failed(&format!("reading {name}"), e))?.len();
                Ok(Some((file, len)))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(failed(&format!("opening {name}"), e)),
        }
    }

    /// The bytes of a small object, when it is at most `most`.
    pub fn read_small(&self, name: &str, most: u64) -> Result<Option<Vec<u8>>, Error> {
        let Some((file, _)) = self.open_blob(name)? else { return Ok(None) };
        let mut bytes = Vec::new();
        file.take(most.saturating_add(1)).read_to_end(&mut bytes)?;
        if bytes.len() as u64 > most {
            return Err(Error::too_large(format!(
                "`{name}` is longer than the {most} bytes that are taken of it: {}",
                bytes.len()
            )));
        }
        Ok(Some(bytes))
    }

    /// Writes a blob `name` with `bytes`, after checking that they are what the name says, and
    /// says whether the name was new.
    pub fn put_blob(&self, name: &str, bytes: &[u8]) -> Result<bool, Error> {
        let object = self.object(name)?;
        if !object.is_blob() {
            return Err(Error::malformed(format!("{name} is a list, not a blob")));
        }
        self.verify(&object, bytes)?;
        let _gate = self.publication.lock().unwrap_or_else(|e| e.into_inner());
        let existed = object.path(&self.dir).exists();
        self.write_whole(&object, bytes)?;
        Ok(!existed)
    }

    /// Writes a blob from a temporary file the caller filled, which is renamed into place:
    /// the way a blob too large to hold is written. The bytes are read back and checked
    /// first, and nothing is named when the check fails.
    pub fn put_blob_from(&self, name: &str, from: &Path) -> Result<bool, Error> {
        let object = self.object(name)?;
        if !object.is_blob() {
            return Err(Error::malformed(format!("{name} is a list, not a blob")));
        }
        self.verify_file(&object, from)?;
        let _gate = self.publication.lock().unwrap_or_else(|e| e.into_inner());
        let to = object.path(&self.dir);
        let existed = to.exists();
        let file = File::options()
            .write(true)
            .open(from)
            .map_err(|e| failed(&format!("writing {name}"), e))?;
        name_file_whole(&file, from, &to).map_err(|e| failed(&format!("naming {name}"), e))?;
        Ok(!existed)
    }

    /// Writes the list `name`, if nothing is there. `false` is another writer having written
    /// first: a list is written once.
    pub fn put_list(&self, name: &str, bytes: &[u8]) -> Result<bool, Error> {
        if bytes.len() > blaze_wire::Limits::DOCUMENTED.request as usize {
            return Err(Error::too_large("the list exceeds the request limit"));
        }
        let object = self.object(name)?;
        if object.is_blob() {
            return Err(Error::malformed(format!("{name} is a blob, not a list")));
        }
        // The look and the write are one step: the gate is held across both.
        let _gate = self.publication.lock().unwrap_or_else(|e| e.into_inner());
        if object.path(&self.dir).exists() {
            return Ok(false);
        }
        self.write_whole(&object, bytes)?;
        Ok(true)
    }

    /// Removes the temporary files of writes that were stopped, once they are older than the
    /// hour a remote's collection gives an object: a write that is in flight when this runs is
    /// younger than that, and no request is in flight when the store is opened.
    pub(crate) fn sweep_temporaries(&self, older_than: std::time::Duration) -> Result<(), Error> {
        for sub in ["", "packs", "keys", "units", "manifests", "deps"] {
            let at = if sub.is_empty() { self.dir.clone() } else { self.dir.join(sub) };
            let entries = match std::fs::read_dir(&at) {
                Ok(entries) => entries,
                Err(error) => {
                    eprintln!("blaze-serve: janitor cannot list {}: {error}", at.display());
                    continue;
                }
            };
            for entry in entries.flatten() {
                let Some(name) = entry.file_name().to_str().map(str::to_owned) else { continue };
                let temporary = name.strip_suffix(".tmp").is_some_and(|name| {
                    name.rsplit_once('.').is_some_and(|(_, stamp)| {
                        let parts: Vec<_> = stamp.split('-').collect();
                        matches!(parts.len(), 2 | 3)
                            && parts.iter().all(|it| {
                                !it.is_empty() && it.bytes().all(|byte| byte.is_ascii_digit())
                            })
                    })
                });
                if !temporary {
                    continue;
                }
                // Project names may themselves end in a numeric .tmp suffix. Published
                // objects are never staging files, even when the suffix looks like one.
                // Current dot-prefixed and excluded legacy staging names still qualify.
                if !sub.is_empty() && ObjectName::parse(&format!("{sub}/{name}")).is_some() {
                    continue;
                }
                if !entry.file_type().is_ok_and(|kind| kind.is_file()) {
                    continue;
                }
                let old = entry
                    .metadata()
                    .ok()
                    .and_then(|it| it.modified().ok())
                    .and_then(|it| it.elapsed().ok())
                    .is_some_and(|it| it > older_than);
                if old {
                    let file = match File::options().read(true).write(true).open(entry.path()) {
                        Ok(file) => file,
                        Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                        Err(error) => {
                            eprintln!(
                                "blaze-serve: janitor cannot open {}: {error}; left untouched",
                                entry.path().display()
                            );
                            continue;
                        }
                    };
                    match file.try_lock() {
                        Ok(()) => {
                            if let Err(error) = std::fs::remove_file(entry.path())
                                && error.kind() != std::io::ErrorKind::NotFound
                            {
                                eprintln!(
                                    "blaze-serve: janitor cannot remove {}: {error}",
                                    entry.path().display()
                                );
                            }
                        }
                        Err(std::fs::TryLockError::WouldBlock) => {}
                        Err(std::fs::TryLockError::Error(error)) => {
                            eprintln!(
                                "blaze-serve: janitor cannot lock {}: {error}; left untouched",
                                entry.path().display()
                            );
                        }
                    }
                }
            }
        }
        Ok(())
    }

    /// Checks that `bytes` are the pack `object`'s name says.
    pub fn verify(&self, object: &ObjectName, bytes: &[u8]) -> Result<(), Error> {
        self.verify_reader(object, std::io::Cursor::new(bytes))
    }

    fn verify_file(&self, object: &ObjectName, from: &Path) -> Result<(), Error> {
        let file = File::open(from)
            .map_err(|e| failed(&format!("reading what was sent for {}", object.name), e))?;
        self.verify_reader(object, file)
    }

    fn verify_reader(&self, object: &ObjectName, mut reader: impl Read) -> Result<(), Error> {
        let mut hash = Sha256::new();
        let mut start = [0; 8];
        reader.read_exact(&mut start).map_err(|e| {
            Error::new(
                blaze_wire::ErrorCode::Refused,
                format!("{} has no pack header: {e}", object.name),
            )
        })?;
        if !blaze_wire::is_dist_pack(&start) {
            return Err(Error::new(
                blaze_wire::ErrorCode::Refused,
                format!("{} is not a pack of format {}", object.name, blaze_wire::DIST_PACK_FORMAT),
            ));
        }
        hash.update(start);
        std::io::copy(&mut reader, &mut Unhashed(&mut hash)).map_err(|e| {
            Error::new(
                blaze_wire::ErrorCode::Refused,
                format!("{} does not read: {e}", object.name),
            )
        })?;
        let got: Digest = hash.finalize().into();
        if got != object.digest {
            return Err(wrong(&object.name, &hex(&got), &hex(&object.digest)));
        }
        Ok(())
    }

    /// Writes `bytes` under the object's name, whole or not at all.
    fn write_whole(&self, object: &ObjectName, bytes: &[u8]) -> Result<(), Error> {
        let to = object.path(&self.dir);
        let partial = temporary(&to);
        let write = (|| -> std::io::Result<()> {
            let mut file = File::options().write(true).create_new(true).open(&partial)?;
            file.try_lock().map_err(std::io::Error::from)?;
            file.write_all(bytes)?;
            #[cfg(test)]
            if let Some(pause) = self.before_name.lock().unwrap().as_ref() {
                pause();
            }
            name_file_whole(&file, &partial, &to)
        })();
        if let Err(e) = write {
            _ = std::fs::remove_file(&partial);
            return Err(failed(&format!("writing {}", object.name), e));
        }
        Ok(())
    }

    fn object(&self, name: &str) -> Result<ObjectName, Error> {
        let object = ObjectName::parse(name)
            .ok_or_else(|| Error::not_found(format!("`{name}` is not an object of this cache")))?;
        let at = object.path(&self.dir);
        for path in [at.parent().expect("an object has a directory"), at.as_path()] {
            match std::fs::symlink_metadata(path) {
                Ok(meta) if meta.file_type().is_symlink() => {
                    return Err(Error::malformed("a cache object cannot be a symlink"));
                }
                Ok(_) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e.into()),
            }
        }
        Ok(object)
    }
}

/// A writer that hashes what it is given and keeps none of it.
struct Unhashed<'a>(&'a mut Sha256);

impl Write for Unhashed<'_> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.update(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn wrong(name: &str, got: &str, want: &str) -> Error {
    Error::new(
        blaze_wire::ErrorCode::Refused,
        format!("{name} is named by {want}, and the bytes sent hash to {got}"),
    )
}

/// A dot-prefixed temporary name, excluded from every kind of object in the layout.
pub(crate) fn temporary(to: &Path) -> PathBuf {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |it| it.as_nanos());
    let sequence = NEXT.fetch_add(1, Ordering::Relaxed);
    let name = to.file_name().map(|it| it.to_string_lossy().into_owned()).unwrap_or_default();
    to.with_file_name(format!(".{name}.{}-{nanos}-{sequence}.tmp", std::process::id()))
}

/// A file, written and closed by its caller, under its final name: synced, renamed, and the
/// directory synced, so a crash leaves all of it under `to` or none of it.
fn name_file_whole(file: &File, from: &Path, to: &Path) -> std::io::Result<()> {
    file.sync_all()?;
    std::fs::rename(from, to)?;
    let parent = to.parent().ok_or(std::io::ErrorKind::InvalidInput)?;
    File::open(parent)?.sync_all()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch() -> tempfile::TempDir {
        tempfile::tempdir().expect("a temporary directory")
    }

    #[test]
    fn sweeping_staging_files_preserves_published_objects() {
        let dir = scratch();
        let store = Store::open(dir.path()).unwrap();
        let name = format!("manifests/{}", hex(&[1; 32]));
        assert!(store.put_list(&name, b"published\n").unwrap());
        let published = store.dir().join(&name);
        let stale = temporary(&published);
        std::fs::write(&stale, b"abandoned\n").unwrap();
        for path in [&published, &stale] {
            File::options()
                .write(true)
                .open(path)
                .unwrap()
                .set_times(std::fs::FileTimes::new().set_modified(UNIX_EPOCH))
                .unwrap();
        }
        store.sweep_temporaries(std::time::Duration::from_secs(3600)).unwrap();
        assert_eq!(store.read_small(&name, 100).unwrap(), Some(b"published\n".to_vec()));
        assert!(!stale.exists(), "abandoned hidden staging file was not swept");
    }

    #[cfg(unix)]
    #[test]
    fn an_unreadable_stale_file_is_left_untouched_without_failing_the_sweep() {
        use std::os::unix::fs::PermissionsExt;
        let dir = scratch();
        let store = Store::open(dir.path()).unwrap();
        let path = store.dir().join("packs/.foreign.123-456-1.tmp");
        std::fs::write(&path, b"foreign leftover").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000)).unwrap();
        let unreadable = File::open(&path).is_err();
        let result = store.sweep_temporaries(std::time::Duration::ZERO);
        if unreadable {
            assert!(path.exists());
        }
        if path.exists() {
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        assert!(result.is_ok());
    }

    /// A sealed pack of `values`, laid out as `harness/packs.py` lays one out.
    fn pack(values: &[(Digest, &[u8])]) -> Vec<u8> {
        let mut out = blaze_wire::DIST_PACK_MAGIC.to_vec();
        out.extend_from_slice(&(values.len() as u32).to_le_bytes());
        out.extend_from_slice(&(values.len() as u32).to_le_bytes());
        for (_, value) in values {
            out.extend_from_slice(&(value.len() as u32).to_le_bytes());
            out.extend_from_slice(&(value.len() as u32).to_le_bytes());
            out.extend_from_slice(&0u32.to_le_bytes());
        }
        for (block, (key, value)) in values.iter().enumerate() {
            out.extend_from_slice(key);
            out.extend_from_slice(&(block as u32).to_le_bytes());
            out.extend_from_slice(&0u32.to_le_bytes());
            out.extend_from_slice(&(value.len() as u32).to_le_bytes());
            let mut crc = crc32fast::Hasher::new();
            crc.update(key);
            crc.update(&(value.len() as u32).to_le_bytes());
            crc.update(value);
            out.extend_from_slice(&crc.finalize().to_le_bytes());
        }
        for (_, value) in values {
            out.extend_from_slice(value);
        }
        out
    }

    fn digest(bytes: &[u8]) -> Digest {
        Sha256::digest(bytes).into()
    }

    #[test]
    fn a_pack_is_checked_against_its_name_and_a_wrong_one_is_not_named() {
        let dir = scratch();
        let store = Store::open(dir.path()).expect("a store");
        let bytes = pack(&[([1; 32], b"a value")]);
        let name = format!("packs/{}.pack", hex(&digest(&bytes)));
        assert!(store.put_blob(&name, &bytes).expect("a write"));
        assert!(store.open_blob(&name).expect("a lookup").is_some());

        let other = pack(&[([2; 32], b"another value")]);
        let refused = store.put_blob(&format!("packs/{}.pack", hex(&digest(&bytes))), &other);
        assert!(refused.is_err());
        assert_eq!(
            store
                .open_blob(&format!("packs/{}.pack", hex(&digest(&bytes))))
                .expect("a lookup")
                .map(|(_, len)| len),
            Some(bytes.len() as u64)
        );
        // A name the layout does not have is not a name.
        assert!(store.put_blob("packs/../x.pack", &bytes).is_err());
    }

    #[test]
    fn a_concurrent_write_of_one_blob_leaves_one_whole_blob() {
        let dir = scratch();
        let store = std::sync::Arc::new(Store::open(dir.path()).expect("a store"));
        let bytes = pack(&[([5; 32], b"a value that is not small"), ([6; 32], &[9; 4096])]);
        let name = format!("packs/{}.pack", hex(&digest(&bytes)));
        let writers: Vec<_> = (0..8)
            .map(|_| {
                let (store, name, bytes) = (store.clone(), name.clone(), bytes.clone());
                std::thread::spawn(move || store.put_blob(&name, &bytes))
            })
            .collect();
        for writer in writers {
            writer.join().expect("a writer").expect("a write");
        }
        assert_eq!(store.read_small(&name, 1 << 20).expect("a read"), Some(bytes));
        // One file under the name, and no temporary file of any of the writers.
        let left: Vec<String> = std::fs::read_dir(dir.path().join("packs"))
            .expect("the packs directory")
            .filter_map(|it| it.ok()?.file_name().into_string().ok())
            .collect();
        assert_eq!(left.len(), 1, "{left:?}");
    }

    #[test]
    fn a_list_is_written_once() {
        let dir = scratch();
        let store = Store::open(dir.path()).expect("a store");
        let name = &format!("manifests/{}", hex(&[1; 32]));
        assert!(store.put_list(name, b"first\n").expect("a write"));
        assert!(!store.put_list(name, b"second\n").expect("a write"));
        assert_eq!(store.read_small(name, 100).unwrap().unwrap(), b"first\n");
    }

    #[test]
    fn a_file_is_checked_as_it_streams_and_a_wrong_one_is_not_named() {
        let dir = scratch();
        let store = Store::open(dir.path()).expect("a store");
        let bytes = pack(&[([3; 32], b"a value")]);
        let right = dir.path().join("right");
        std::fs::write(&right, &bytes).unwrap();
        let name = format!("packs/{}.pack", hex(&digest(&bytes)));
        assert!(store.put_blob_from(&name, &right).unwrap());
        let wrong = dir.path().join("wrong");
        std::fs::write(&wrong, &bytes).unwrap();
        let other = format!("packs/{}.pack", hex(&[9; 32]));
        assert!(store.put_blob_from(&other, &wrong).is_err());
        assert!(store.open_blob(&other).unwrap().is_none());
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_are_not_cache_objects() {
        let dir = scratch();
        let store = Store::open(dir.path()).unwrap();
        let outside = scratch();
        let name = format!("packs/{}.pack", hex(&digest(b"outside")));
        std::fs::write(outside.path().join("secret"), b"outside").unwrap();
        std::os::unix::fs::symlink(outside.path().join("secret"), dir.path().join(&name)).unwrap();
        assert!(store.open_blob(&name).is_err());
        assert!(store.put_blob(&name, b"outside").is_err());
        std::fs::remove_dir(dir.path().join("keys")).unwrap();
        std::os::unix::fs::symlink(outside.path(), dir.path().join("keys")).unwrap();
        assert!(Store::open(dir.path()).is_err());
    }
}
