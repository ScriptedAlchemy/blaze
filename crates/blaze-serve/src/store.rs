//! The directory the server serves: today's remote layout, with an existence set in memory.
//!
//! Every blob is checked before it becomes visible, so a name only ever holds what it says:
//!
//! - `packs/<d>.pack` is a sealed pack of this store format whose bytes hash to `<d>`, which
//!   is what the store itself checks a pack against.
//! - `packs/<d>.pack.zst` is zstd of a sealed pack of this format, and the pack's bytes hash
//!   to `<d>`: the name is the pack's, as `docs/cache.md` writes it, so the check unpacks.
//! - `keys/<d>` and `units/<d>` are bytes that hash to `<d>`.
//!
//! A write goes to `.<name>.<pid>-<nanos>.tmp` in the same directory, is synced, renamed, and
//! the directory synced, as the store writes its own files ([`ra_jit_store`]'s
//! `name_file_whole`), so a partially written blob never appears under its name and a reader
//! sees all of it or none.
//!
//! The existence set is what `missing` is answered from: the digests of the blobs the
//! directory holds, read once at startup and kept as writes and removals come. It is a hint,
//! not the truth a fetch goes by: a `GET` reads the file, and one that is gone is 404.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs::File,
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::{
        Mutex, RwLock,
        atomic::{AtomicU64, Ordering},
    },
    time::{SystemTime, UNIX_EPOCH},
};

use blaze_wire::Digest;
use sha2::{Digest as _, Sha256};

use crate::blob::{Kind, ObjectName, hex};
use crate::error::{Error, failed};

/// What a write of a list expects to find under the name.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Expect<'a> {
    /// Nothing is said about what is there: the write lands.
    Any,
    /// Nothing may be there: a `PUT` of a name that did not exist.
    Absent,
    /// What is there must be what was read, by the hash of its bytes.
    Read(&'a str),
}

/// How the bytes of a blob were checked against its name.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verification {
    /// The bytes hash to the name's digest.
    Hashed,
    /// The bytes are zstd of a pack whose bytes hash to the name's digest.
    Unpacked,
}

/// An object of the directory as a listing has it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Listed {
    /// The name, as today's layout writes it.
    pub name: String,
    /// How many bytes are under it.
    pub len: u64,
    /// When it was written, by this machine's clock.
    pub written: SystemTime,
}

/// The directory, shared by every connection.
pub struct Store {
    dir: PathBuf,
    /// The digests of the blobs the directory holds, and the names they are held under. A
    /// digest can be under two names: `packs/<d>.pack` and `packs/<d>.pack.zst` are both
    /// named `<d>`, and a pack that was sent both ways is both.
    held: RwLock<BTreeMap<Digest, BTreeSet<String>>>,
    /// Publication and deletion update disk and the existence set in the same order.
    /// Conditional list writes hold this across their compare and rename.
    publication: Mutex<()>,
    #[cfg(test)]
    before_name: Mutex<Option<Box<dyn Fn() + Send>>>,
    #[cfg(test)]
    before_stat: Mutex<Option<Box<dyn Fn(&Path) + Send>>>,
}

impl Store {
    /// The directory `dir`, with the existence set read from what is in it. A directory that
    /// is not there is made, so that the server can be pointed at a new one.
    pub fn open(dir: &Path) -> Result<Store, Error> {
        for sub in ["packs", "keys", "units", "manifests", "deps"] {
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
            held: RwLock::new(BTreeMap::new()),
            publication: Mutex::new(()),
            #[cfg(test)]
            before_name: Mutex::new(None),
            #[cfg(test)]
            before_stat: Mutex::new(None),
        };
        store.sweep_temporaries(std::time::Duration::from_secs(3600))?;
        store.rescan()?;
        Ok(store)
    }

    /// The directory it serves.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub(crate) fn checked_path(&self, name: &str) -> Result<PathBuf, Error> {
        Ok(self.object(name)?.path(&self.dir))
    }

    /// Reads the existence set again from the directory. A name that is not of the layout is
    /// left alone: it is not the server's.
    pub fn rescan(&self) -> Result<(), Error> {
        let _gate = self.publication.lock().unwrap_or_else(|e| e.into_inner());
        let mut held: BTreeMap<Digest, BTreeSet<String>> = BTreeMap::new();
        for sub in ["packs", "keys", "units"] {
            let at = self.dir.join(sub);
            let entries = match std::fs::read_dir(&at) {
                Ok(entries) => entries,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => return Err(failed(&format!("listing {}", at.display()), e)),
            };
            for entry in entries {
                let entry = entry.map_err(|e| failed(&format!("listing {}", at.display()), e))?;
                if !entry.file_type()?.is_file() {
                    continue;
                }
                let Some(name) = entry.file_name().to_str().map(str::to_owned) else { continue };
                let Some(object) = ObjectName::parse(&format!("{sub}/{name}")) else { continue };
                let Some(digest) = object.digest else { continue };
                held.entry(digest).or_default().insert(object.name);
            }
        }
        *self.held.write().unwrap_or_else(|e| e.into_inner()) = held;
        Ok(())
    }

    /// The digests of `asked` that the directory does not hold, in the order they were asked
    /// and without repeats. This is what `missing` answers, from memory.
    pub fn missing(&self, asked: &[Digest]) -> Vec<Digest> {
        let held = self.held.read().unwrap_or_else(|e| e.into_inner());
        let mut missing = Vec::new();
        let mut seen = BTreeSet::new();
        for digest in asked {
            if !held.contains_key(digest) && seen.insert(*digest) {
                missing.push(*digest);
            }
        }
        missing
    }

    /// How many blobs the existence set holds.
    pub fn held(&self) -> usize {
        self.held.read().unwrap_or_else(|e| e.into_inner()).len()
    }

    /// The raw object named by a bare digest, in keys, units or uncompressed packs.
    /// Compressed packs need the named route until the encoding-header protocol exists.
    pub fn name_of_digest(&self, digest: &Digest) -> Option<String> {
        let held = self.held.read().unwrap_or_else(|e| e.into_inner());
        let names = held.get(digest)?;
        let order = |name: &String| {
            let kind = ObjectName::parse(name).map(|it| it.kind);
            match kind {
                Some(Kind::Keys) => 0,
                Some(Kind::Unit) => 1,
                Some(Kind::Pack) => 2,
                _ => 3,
            }
        };
        names
            .iter()
            .filter(|name| ObjectName::parse(name).is_some_and(|it| it.kind != Kind::Packed))
            .min_by_key(|it| order(it))
            .cloned()
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

    /// The object under `name` as a listing has it, or `None` when nothing is there.
    pub fn found(&self, name: &str) -> Result<Option<Listed>, Error> {
        let object = self.object(name)?;
        let at = object.path(&self.dir);
        match std::fs::metadata(&at) {
            Ok(found) => Ok(Some(Listed {
                name: name.to_owned(),
                len: found.len(),
                written: found.modified().unwrap_or(UNIX_EPOCH),
            })),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(failed(&format!("looking for {name}"), e)),
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

    /// Writes a blob `name` with `bytes`, after checking that they are what the name says.
    ///
    /// A repeated PUT refreshes the write time, as remote collection expects of a reupload.
    /// The answer says how the bytes were checked and whether the name was new.
    pub fn put_blob(&self, name: &str, bytes: &[u8]) -> Result<(Verification, bool), Error> {
        let object = self.object(name)?;
        if !object.is_blob() {
            return Err(Error::malformed(format!("{name} is a list, not a blob")));
        }
        let digest =
            object.digest.ok_or_else(|| Error::malformed(format!("{name} names no digest")))?;
        let verified = self.verify(&object, bytes)?;
        let _gate = self.publication.lock().unwrap_or_else(|e| e.into_inner());
        let existed = self.found(name)?.is_some();
        self.write_whole(&object, bytes)?;
        self.remember(&object, digest);
        Ok((verified, !existed))
    }

    /// Writes a blob from a temporary file the caller filled, which is renamed into place:
    /// the way a blob too large to hold is written. The bytes are read back and checked
    /// first, and nothing is named when the check fails.
    pub fn put_blob_from(&self, name: &str, from: &Path) -> Result<(Verification, bool), Error> {
        let object = self.object(name)?;
        if !object.is_blob() {
            return Err(Error::malformed(format!("{name} is a list, not a blob")));
        }
        let digest =
            object.digest.ok_or_else(|| Error::malformed(format!("{name} names no digest")))?;
        let verified = self.verify_file(&object, from)?;
        let _gate = self.publication.lock().unwrap_or_else(|e| e.into_inner());
        let existed = self.found(name)?.is_some();
        let to = object.path(&self.dir);
        let file = File::options()
            .write(true)
            .open(from)
            .map_err(|e| failed(&format!("writing {name}"), e))?;
        ra_jit_store::name_file_whole(&file, from, &to)
            .map_err(|e| failed(&format!("naming {name}"), e))?;
        self.remember(&object, digest);
        Ok((verified, !existed))
    }

    /// Writes the list `name`, if what is there is what `expect` says it is. `false` is another
    /// writer having written first, or a name that had to be empty and was not.
    pub fn put_list(&self, name: &str, bytes: &[u8], expect: Expect<'_>) -> Result<bool, Error> {
        if bytes.len() > blaze_wire::Limits::DOCUMENTED.request as usize {
            return Err(Error::too_large("the list exceeds the advertised request limit"));
        }
        let object = self.object(name)?;
        if object.is_blob() {
            return Err(Error::malformed(format!("{name} is a blob, not a list")));
        }
        let at = object.path(&self.dir);
        // The check and the write are one step: the gate is held across both.
        let _gate = self.publication.lock().unwrap_or_else(|e| e.into_inner());
        let now = if expect == Expect::Any {
            None
        } else {
            match File::open(&at) {
                Ok(mut file) => {
                    let mut hash = Sha256::new();
                    std::io::copy(&mut file, &mut Unhashed(&mut hash))?;
                    Some(hex(&hash.finalize().into()))
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
                Err(e) => return Err(failed(&format!("reading {name}"), e)),
            }
        };
        let holds = match expect {
            Expect::Any => true,
            Expect::Absent => now.is_none(),
            Expect::Read(tag) => now.as_deref() == Some(tag),
        };
        if !holds {
            return Ok(false);
        }
        self.write_whole(&object, bytes)?;
        Ok(true)
    }

    /// Removes the object under `name`, and says whether it was there. A blob that is still
    /// there after a removal that raced with a write stays in the existence set.
    pub fn delete(&self, name: &str) -> Result<bool, Error> {
        let object = self.object(name)?;
        let _gate = self.publication.lock().unwrap_or_else(|e| e.into_inner());
        let at = object.path(&self.dir);
        let removed = match std::fs::remove_file(&at) {
            Ok(()) => true,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
            Err(e) => return Err(failed(&format!("removing {name}"), e)),
        };
        if removed {
            File::open(at.parent().expect("an object has a directory"))?.sync_all()?;
        }
        if let Some(digest) = object.digest {
            let mut held = self.held.write().unwrap_or_else(|e| e.into_inner());
            let empty = match held.get_mut(&digest) {
                Some(names) => {
                    names.remove(&object.name);
                    names.is_empty()
                }
                None => false,
            };
            if empty {
                held.remove(&digest);
            }
        }
        Ok(removed)
    }

    /// The objects whose names start with `prefix`, which ends a directory.
    pub fn list(&self, prefix: &str) -> Result<Vec<Listed>, Error> {
        if prefix.is_empty() {
            let mut all = Vec::new();
            for sub in ["packs/", "keys/", "units/", "manifests/", "deps/"] {
                all.extend(self.list(sub)?);
            }
            all.sort_by(|a, b| a.name.cmp(&b.name));
            return Ok(all);
        }
        let dir = prefix.split_once('/').map_or(prefix, |(dir, _)| dir);
        if dir.is_empty() {
            return Err(Error::malformed("the prefix cannot be an absolute path"));
        }
        if !matches!(dir, "" | "packs" | "keys" | "units" | "manifests" | "deps") {
            return Err(Error::malformed("the prefix is not a cache directory"));
        }
        let at = if dir.is_empty() { self.dir.clone() } else { self.dir.join(dir) };
        if std::fs::symlink_metadata(&at)?.file_type().is_symlink() {
            return Err(Error::malformed("a cache directory cannot be a symlink"));
        }
        let entries = match std::fs::read_dir(&at) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(failed(&format!("listing {prefix}"), e)),
        };
        let mut objects = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|e| failed(&format!("listing {prefix}"), e))?;
            let Some(name) = entry.file_name().to_str().map(|it| match dir {
                "" => it.to_owned(),
                dir => format!("{dir}/{it}"),
            }) else {
                continue;
            };
            if !name.starts_with(prefix) || ObjectName::parse(&name).is_none() {
                continue;
            }
            #[cfg(test)]
            if let Some(pause) = self.before_stat.lock().unwrap().as_ref() {
                pause(&entry.path());
            }
            let found = match std::fs::symlink_metadata(entry.path()) {
                Ok(found) => found,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => return Err(failed(&format!("listing {prefix}"), e)),
            };
            if !found.is_file() {
                continue;
            }
            objects.push(Listed {
                name,
                len: found.len(),
                written: found.modified().unwrap_or(UNIX_EPOCH),
            });
        }
        objects.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(objects)
    }

    /// Removes the temporary files of writes that were stopped, once they are older than the
    /// hour a remote's collection gives an object: a write that is in flight when this runs is
    /// younger than that, and no request is in flight when the store is opened.
    pub(crate) fn sweep_temporaries(&self, older_than: std::time::Duration) -> Result<(), Error> {
        for sub in ["", "packs", "keys", "units", "manifests", "deps"] {
            let at = if sub.is_empty() { self.dir.clone() } else { self.dir.join(sub) };
            let Ok(entries) = std::fs::read_dir(&at) else { continue };
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
                        Err(e) => return Err(e.into()),
                    };
                    match file.try_lock() {
                        Ok(()) => {
                            _ = std::fs::remove_file(entry.path());
                        }
                        Err(std::fs::TryLockError::WouldBlock) => {}
                        Err(std::fs::TryLockError::Error(e)) => return Err(e.into()),
                    }
                }
            }
        }
        Ok(())
    }

    /// Checks that `bytes` are what `object`'s name says, and says how.
    pub fn verify(&self, object: &ObjectName, bytes: &[u8]) -> Result<Verification, Error> {
        self.verify_reader(object, std::io::Cursor::new(bytes))
    }

    fn verify_file(&self, object: &ObjectName, from: &Path) -> Result<Verification, Error> {
        let file = File::open(from)
            .map_err(|e| failed(&format!("reading what was sent for {}", object.name), e))?;
        self.verify_reader(object, file)
    }

    fn verify_reader(&self, object: &ObjectName, input: impl Read) -> Result<Verification, Error> {
        let digest = object.digest.ok_or_else(|| Error::malformed("a list is not a blob"))?;
        let mut reader: Box<dyn Read + '_> = if object.kind == Kind::Packed {
            Box::new(zstd::Decoder::new(input).map_err(|e| {
                Error::new(
                    blaze_wire::ErrorCode::Refused,
                    format!("{} is not zstd: {e}", object.name),
                )
            })?)
        } else {
            Box::new(input)
        };
        let mut hash = Sha256::new();
        if matches!(object.kind, Kind::Pack | Kind::Packed) {
            let mut start = [0; 8];
            reader.read_exact(&mut start).map_err(|e| {
                Error::new(
                    blaze_wire::ErrorCode::Refused,
                    format!("{} has no pack header: {e}", object.name),
                )
            })?;
            if ra_jit_store::pack_format(&start) != Some(ra_jit_store::FORMAT) {
                return Err(Error::new(
                    blaze_wire::ErrorCode::Refused,
                    format!("{} is not a pack of format {}", object.name, ra_jit_store::FORMAT),
                ));
            }
            hash.update(start);
        }
        std::io::copy(&mut reader, &mut Unhashed(&mut hash)).map_err(|e| {
            Error::new(
                blaze_wire::ErrorCode::Refused,
                format!("{} does not read: {e}", object.name),
            )
        })?;
        let got: Digest = hash.finalize().into();
        if got != digest {
            return Err(wrong(&object.name, &hex(&got), &hex(&digest)));
        }
        Ok(if object.kind == Kind::Packed { Verification::Unpacked } else { Verification::Hashed })
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
            ra_jit_store::name_file_whole(&file, &partial, &to)
        })();
        if let Err(e) = write {
            _ = std::fs::remove_file(&partial);
            return Err(failed(&format!("writing {}", object.name), e));
        }
        Ok(())
    }

    fn remember(&self, object: &ObjectName, digest: Digest) {
        let mut held = self.held.write().unwrap_or_else(|e| e.into_inner());
        held.entry(digest).or_default().insert(object.name.clone());
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

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch() -> tempfile::TempDir {
        tempfile::tempdir().expect("a temporary directory")
    }

    #[test]
    fn listing_skips_a_published_object_deleted_after_enumeration() {
        let dir = scratch();
        let store = std::sync::Arc::new(Store::open(dir.path()).unwrap());
        let name = format!("units/{}", hex(&Sha256::digest(b"bytes").into()));
        store.put_blob(&name, b"bytes").unwrap();
        let (ready, arrived) = std::sync::mpsc::channel();
        let (resume, resumed) = std::sync::mpsc::channel();
        *store.before_stat.lock().unwrap() = Some(Box::new(move |_| {
            ready.send(()).unwrap();
            resumed.recv_timeout(std::time::Duration::from_secs(5)).unwrap();
        }));
        let listing = {
            let store = store.clone();
            std::thread::spawn(move || store.list("units/"))
        };
        arrived.recv_timeout(std::time::Duration::from_secs(5)).unwrap();
        assert!(store.delete(&name).unwrap());
        resume.send(()).unwrap();
        assert!(listing.join().unwrap().unwrap().is_empty());
    }

    #[test]
    fn listing_does_not_hide_a_non_not_found_filesystem_error() {
        let dir = scratch();
        let store = Store::open(dir.path()).unwrap();
        let name = format!("units/{}", hex(&Sha256::digest(b"bytes").into()));
        store.put_blob(&name, b"bytes").unwrap();
        *store.before_stat.lock().unwrap() = Some(Box::new(move |path| {
            std::fs::remove_file(path).unwrap();
            std::fs::remove_dir(path.parent().unwrap()).unwrap();
            std::fs::write(path.parent().unwrap(), b"not a directory").unwrap();
        }));
        let error = store.list("units/").unwrap_err();
        assert_eq!(error.code, blaze_wire::ErrorCode::Internal);
        assert!(error.message.contains("listing units/"));
    }

    #[test]
    fn listing_during_publication_excludes_current_and_legacy_list_temporaries() {
        for prefix in ["manifests", "deps"] {
            let dir = scratch();
            let store = std::sync::Arc::new(Store::open(dir.path()).unwrap());
            let name = format!("{prefix}/default");
            store.put_list(&name, b"old\n", Expect::Any).unwrap();
            let legacy = dir.path().join(format!("{prefix}/default.123-456.tmp"));
            std::fs::write(&legacy, b"cut").unwrap();
            let (ready, arrived) = std::sync::mpsc::channel();
            let (resume, resumed) = std::sync::mpsc::channel();
            *store.before_name.lock().unwrap() = Some(Box::new(move || {
                ready.send(()).unwrap();
                resumed.recv_timeout(std::time::Duration::from_secs(5)).unwrap();
            }));
            let writer = {
                let store = store.clone();
                let name = name.clone();
                std::thread::spawn(move || store.put_list(&name, b"new\n", Expect::Any).unwrap())
            };
            arrived.recv_timeout(std::time::Duration::from_secs(5)).unwrap();
            let listed = store.list(&format!("{prefix}/")).unwrap();
            assert_eq!(listed.iter().map(|it| &it.name).collect::<Vec<_>>(), vec![&name]);
            assert_eq!(store.read_small(&name, 100).unwrap().unwrap(), b"old\n");
            // Model GC's list-then-read ordering across the publisher's rename.
            resume.send(()).unwrap();
            writer.join().unwrap();
            assert_eq!(store.read_small(&listed[0].name, 100).unwrap().unwrap(), b"new\n");
            assert!(store.open_blob(&format!("{prefix}/default.123-456.tmp")).is_err());
        }
    }

    /// A sealed pack of `values`, laid out as `harness/packs.py` lays one out.
    fn pack(values: &[(Digest, &[u8])]) -> Vec<u8> {
        let mut out = b"rajitpk\x03".to_vec();
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
        assert_eq!(store.put_blob(&name, &bytes).expect("a write").0, Verification::Hashed);
        assert!(store.found(&name).expect("a lookup").is_some());

        let other = pack(&[([2; 32], b"another value")]);
        let refused = store.put_blob(&format!("packs/{}.pack", hex(&digest(&bytes))), &other);
        assert!(refused.is_err());
        assert_eq!(
            store
                .found(&format!("packs/{}.pack", hex(&digest(&bytes))))
                .expect("a lookup")
                .map(|it| it.len),
            Some(bytes.len() as u64)
        );
        // A name the layout does not have is not a name.
        assert!(store.put_blob("packs/../x.pack", &bytes).is_err());
    }

    #[test]
    fn a_pack_sent_compressed_is_unpacked_and_checked() {
        let dir = scratch();
        let store = Store::open(dir.path()).expect("a store");
        let bytes = pack(&[([3; 32], b"a value")]);
        let compressed = zstd::bulk::compress(&bytes, 3).expect("zstd");
        let name = format!("packs/{}.pack.zst", hex(&digest(&bytes)));
        assert_eq!(store.put_blob(&name, &compressed).expect("a write").0, Verification::Unpacked);
        // Compressed bytes of another pack under this name are refused.
        let other = zstd::bulk::compress(&pack(&[([4; 32], b"other")]), 3).expect("zstd");
        assert!(store.put_blob(&name, &other).is_err());
        assert_eq!(store.read_small(&name, 1 << 20).expect("a read"), Some(compressed));
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
    fn missing_is_answered_from_the_existence_set() {
        let dir = scratch();
        let store = Store::open(dir.path()).expect("a store");
        let bytes = pack(&[([7; 32], b"a value")]);
        let held: Digest = digest(&bytes);
        store.put_blob(&format!("packs/{}.pack", hex(&held)), &bytes).expect("a write");
        let absent = digest(b"nothing");
        assert!(store.missing(&[]).is_empty());
        assert!(store.missing(&[held]).is_empty());
        assert_eq!(store.missing(&[absent]), vec![absent]);
        let many: Vec<Digest> = (0..4096u32).map(|at| digest(&at.to_le_bytes())).collect();
        assert_eq!(store.missing(&many).len(), 4096);
        assert_eq!(store.missing(&many[..1]), vec![many[0]]);
        // A blob that is removed is missing again, and one that is there is not.
        store.delete(&format!("packs/{}.pack", hex(&held))).expect("a delete");
        assert_eq!(store.missing(&[held]), vec![held]);
    }

    #[test]
    fn a_list_is_replaced_only_by_a_writer_that_read_what_was_there() {
        let dir = scratch();
        let store = Store::open(dir.path()).expect("a store");
        let name = "manifests/default";
        assert!(store.put_list(name, b"first\n", Expect::Absent).expect("a write"));
        assert!(!store.put_list(name, b"second\n", Expect::Absent).expect("a write"));
        let read = store.read_small(name, 1 << 20).expect("a read").expect("a list");
        let tag = hex(&digest(&read));
        assert!(store.put_list(name, b"first\nsecond\n", Expect::Read(&tag)).expect("a write"));
        assert!(!store.put_list(name, b"third\n", Expect::Read(&tag)).expect("a write"));
        assert!(store.put_list(name, b"whatever\n", Expect::Any).expect("a write"));
        assert_eq!(store.read_small(name, 1 << 20).expect("a read"), Some(b"whatever\n".to_vec()));
    }

    #[test]
    fn compressed_wrong_format_is_refused_and_file_verification_streams() {
        let dir = scratch();
        let store = Store::open(dir.path()).unwrap();
        let bytes = b"not a pack";
        let compressed = zstd::bulk::compress(bytes, 3).unwrap();
        let name = format!("packs/{}.pack.zst", hex(&digest(bytes)));
        assert!(store.put_blob(&name, &compressed).is_err());
        let bytes = vec![5; 2 << 20];
        let name = format!("units/{}", hex(&digest(&bytes)));
        let from = dir.path().join("input.tmp");
        std::fs::write(&from, &bytes).unwrap();
        store.put_blob_from(&name, &from).unwrap();
        assert!(!from.exists());
        assert_eq!(store.found(&name).unwrap().unwrap().len, bytes.len() as u64);
        assert!(store.read_small(&name, 10).is_err());
    }

    #[test]
    fn deletion_and_publication_leave_the_existence_set_equal_to_disk() {
        let dir = scratch();
        let store = std::sync::Arc::new(Store::open(dir.path()).unwrap());
        let bytes = b"one object";
        let d = digest(bytes);
        let name = format!("units/{}", hex(&d));
        let write = {
            let store = store.clone();
            let name = name.clone();
            std::thread::spawn(move || {
                for _ in 0..100 {
                    store.put_blob(&name, bytes).unwrap();
                }
            })
        };
        for _ in 0..100 {
            store.delete(&name).unwrap();
        }
        write.join().unwrap();
        assert_eq!(store.missing(&[d]).is_empty(), store.found(&name).unwrap().is_some());
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_are_not_cache_objects() {
        let dir = scratch();
        let store = Store::open(dir.path()).unwrap();
        let outside = scratch();
        let name = format!("units/{}", hex(&digest(b"outside")));
        std::fs::write(outside.path().join("secret"), b"outside").unwrap();
        std::os::unix::fs::symlink(outside.path().join("secret"), dir.path().join(&name)).unwrap();
        assert!(store.open_blob(&name).is_err());
        assert!(store.put_blob(&name, b"outside").is_err());
        std::fs::remove_dir(dir.path().join("keys")).unwrap();
        std::os::unix::fs::symlink(outside.path(), dir.path().join("keys")).unwrap();
        assert!(Store::open(dir.path()).is_err());
    }
}
