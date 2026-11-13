//! The directory the server serves.
//!
//! Every pack is checked before it becomes visible, so a name only ever holds what it says:
//! `packs/<d>.pack` is a sealed pack whose bytes hash to `<d>`.
//!
//! A write goes to `.<name>.<pid>-<nanos>.tmp` in the same directory, is synced, renamed, and
//! the directory synced, as the store writes its own files, so a partially written blob never
//! appears: a reader sees all of it or none.

use std::{
    cmp::Reverse,
    collections::BinaryHeap,
    fs::File,
    io::{BufReader, Read, Seek, SeekFrom, Write},
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

mod publication;

/// One native record's header; ordering starts with its key for the merge frontier.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct NativeEntry {
    key: Digest,
    kind: u8,
    length: u64,
    offset: u64,
}

fn native_refusal(name: &str, why: &str) -> Error {
    Error::new(blaze_wire::ErrorCode::Refused, format!("{name}: {why}"))
}

/// Read just a header. Validate the end before any skip or value comparison, so even
/// u64::MAX lengths cannot overflow or seek past the verified file's boundary.
fn native_entry(
    reader: &mut impl Read,
    name: &str,
    offset: u64,
    size: u64,
) -> Result<Option<NativeEntry>, Error> {
    if offset == size {
        return Ok(None);
    }
    let mut header = [0; 41];
    reader.read_exact(&mut header)
        .map_err(|e| native_refusal(name, &format!("ends inside a native value header: {e}")))?;
    let length = u64::from_le_bytes(header[33..].try_into().expect("eight length bytes"));
    let value_offset = offset
        .checked_add(header.len() as u64)
        .ok_or_else(|| native_refusal(name, "native value offset overflows u64"))?;
    if length > size.saturating_sub(value_offset) || value_offset > size {
        return Err(native_refusal(name, "ends inside a native value"));
    }
    Ok(Some(NativeEntry {
        key: header[..32].try_into().expect("32 key bytes"),
        kind: header[32],
        length,
        offset: value_offset,
    }))
}

fn native_keys_hasher(count: usize) -> Sha256 {
    let mut hash = Sha256::new();
    // The wire v5 digest domain, followed by the same array head and byte strings
    // Pack::of hashes. Streaming avoids allocating a CBOR array proportional to keys.
    hash.update(format!("blaze/blaze/dist-pack-keys/v{}\0", blaze_wire::VERSION)); // alloc: fixed-size wire domain
    let mut head = blaze_wire::Encoder::new();
    head.array(count);
    hash.update(head.into_bytes());
    hash
}

/// The directory, shared by every connection.
pub struct Store {
    dir: PathBuf,
    /// Serializes publication's comparison, validation and atomic rename. Ordinary lists
    /// are immutable; a conditional repair may replace only an exactly matched stale list.
    publication: Mutex<()>,
    #[cfg(test)]
    publication_failure: Mutex<Option<publication::Step>>,
    #[cfg(test)]
    bypass_comparison: std::sync::atomic::AtomicBool,
    #[cfg(test)]
    bypass_mirrors: std::sync::atomic::AtomicBool,
    #[cfg(test)]
    verification_peak: std::sync::atomic::AtomicUsize,
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
            publication_failure: Mutex::new(None),
            #[cfg(test)]
            bypass_comparison: std::sync::atomic::AtomicBool::new(false),
            #[cfg(test)]
            bypass_mirrors: std::sync::atomic::AtomicBool::new(false),
            #[cfg(test)]
            verification_peak: std::sync::atomic::AtomicUsize::new(0),
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
        self.name_file_whole(&file, from, &to).map_err(|e| failed(&format!("naming {name}"), e))?;
        Ok(!existed)
    }

    /// Writes the list `name`, if nothing is there. `false` is another writer having written
    /// first: a list is written once.
    pub fn put_list(&self, name: &str, bytes: &[u8]) -> Result<bool, Error> {
        self.put_list_with(name, bytes, |object, bytes| self.write_whole(object, bytes))
    }

    // The simulation supplies publication IO, keeping the real first-writer predicate.
    fn put_list_with(
        &self,
        name: &str,
        bytes: &[u8],
        publish: impl FnOnce(&ObjectName, &[u8]) -> Result<(), Error>,
    ) -> Result<bool, Error> {
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
        // A first writer must not publish a missing, corrupt or noncanonical
        // pack list. Keep validation under the same publication lock as the
        // atomic name, so concurrent PUTs cannot change the verified packs.
        let packs = blaze_wire::pack_list::decode(bytes)
            .map_err(|why| Error::malformed(format!("the new {name}: {why}")))?;
        self.verify_manifest_packs(name, &packs, true)?;
        publish(&object, bytes)?;
        Ok(true)
    }

    /// Repair an exactly matched manifest only when at least one of its packs is absent.
    /// Existing corrupt packs and malformed lists are refused. Every replacement pack is
    /// verified before the existing atomic writer is entered, under the publication gate.
    /// False is a missing/different current list or a list whose packs are all still valid.
    pub fn repair_list(&self, name: &str, expected: &Digest, bytes: &[u8]) -> Result<bool, Error> {
        self.repair_list_with(name, expected, bytes, |object, bytes| {
            self.write_whole(object, bytes)
        })
    }

    // Keep authorization and the publication gate shared with the simulation's IO adapter.
    fn repair_list_with(
        &self,
        name: &str,
        expected: &Digest,
        bytes: &[u8],
        publish: impl FnOnce(&ObjectName, &[u8]) -> Result<(), Error>,
    ) -> Result<bool, Error> {
        if bytes.len() > blaze_wire::Limits::DOCUMENTED.request as usize {
            return Err(Error::too_large("the replacement list exceeds the request limit"));
        }
        let object = self.object(name)?;
        if object.is_blob() {
            return Err(Error::malformed(format!("{name} is a blob, not a manifest")));
        }
        let _gate = self.publication.lock().unwrap_or_else(|error| error.into_inner());
        let Some(current) =
            self.read_small(name, u64::from(blaze_wire::Limits::DOCUMENTED.request))?
        else {
            return Ok(false);
        };
        let same = <Digest>::from(Sha256::digest(&current)) == *expected;
        #[cfg(test)]
        let same = same || self.bypass_comparison.load(Ordering::Relaxed);
        if !same {
            return Ok(false);
        }
        let old = blaze_wire::pack_list::decode(&current)
            .map_err(|why| Error::malformed(format!("the current {name}: {why}")))?;
        // Inspect every present old pack, even when another is missing: a
        // conflicting immutable key is not a repairable cache miss.
        let missing = self.verify_manifest_packs(name, &old, false)?;
        if !missing {
            return Ok(false);
        }
        let replacement = blaze_wire::pack_list::decode(bytes)
            .map_err(|why| Error::malformed(format!("the replacement {name}: {why}")))?;
        self.verify_manifest_packs(name, &replacement, true)?;
        publish(&object, bytes)?;
        Ok(true)
    }

    /// Validate each native pack, then merge its ordered records to compare equal keys.
    /// The request limit bounds the frontier to one record per manifest line, independent
    /// of the number of keys in a unit. Only two files and fixed byte buffers are live.
    /// Work is linear in pack bytes plus O(records * log(packs)) frontier operations.
    /// Reopened content names remain stable under the caller's publication gate: every
    /// Store blob replacement takes that gate, and the janitor only removes temporaries.
    fn verify_manifest_packs(
        &self,
        name: &str,
        packs: &[blaze_wire::PackRef],
        require_all: bool,
    ) -> Result<bool, Error> {
        let mut frontier = BinaryHeap::new(); // alloc: one pending record per bounded manifest line
        let mut missing = false;
        for (at, pack) in packs.iter().enumerate() {
            if !self.verify_pack(pack)? {
                if require_all {
                    return Err(Error::new(
                        blaze_wire::ErrorCode::Refused,
                        format!("{name} names absent pack {}", hex(&pack.pack)),
                    ));
                }
                missing = true;
            } else if let Some(entry) = self.native_entry_at(pack, 8)? {
                frontier.push(Reverse((entry, at)));
            }
        }
        #[cfg(test)]
        self.verification_peak.fetch_max(frontier.len(), Ordering::Relaxed);
        let mut previous: Option<(NativeEntry, usize)> = None;
        while let Some(Reverse((entry, at))) = frontier.pop() {
            if let Some((prior, prior_at)) = previous
                && prior.key == entry.key
            {
                self.compare_native_values(&packs[prior_at], prior, &packs[at], entry)?;
            }
            previous = Some((entry, at));
            if let Some(next) = self.native_entry_at(&packs[at], entry.offset + entry.length)? {
                frontier.push(Reverse((next, at)));
            }
        }
        Ok(missing)
    }

    /// True is a whole present native pack; false is absence. CBOR's batch limit
    /// bounds wire arrays, not the opaque records in a native blob (Pack::of has no
    /// such cap). Count ordered records first, then hash their CBOR encoding without
    /// retaining keys or values. The blob byte limit bounds both scans.
    fn verify_pack(&self, pack: &blaze_wire::PackRef) -> Result<bool, Error> {
        let name = format!("packs/{}.pack", hex(&pack.pack));
        let object = self.object(&name)?;
        let Some((mut file, size)) = self.open_blob(&name)? else { return Ok(false) };
        if size != pack.size || size > blaze_wire::Limits::DOCUMENTED.blob {
            return Err(native_refusal(
                &name,
                &format!(
                    "has {size} bytes, its list names {}; the wire blob limit is {}",
                    pack.size,
                    blaze_wire::Limits::DOCUMENTED.blob,
                ),
            ));
        }
        self.verify_reader(&object, &mut file)?;
        file.seek(SeekFrom::Start(8)).map_err(|e| failed(&format!("rewinding {name}"), e))?;
        let mut reader = BufReader::new(file); // alloc: one fixed-capacity native scan buffer
        let mut count = 0;
        let mut previous = None;
        let mut offset = 8;
        while let Some(entry) = native_entry(&mut reader, &name, offset, size)? {
            if previous.is_some_and(|key| key >= entry.key) {
                return Err(native_refusal(&name, "native entries have duplicate or unordered keys"));
            }
            previous = Some(entry.key);
            count += 1;
            offset = entry.offset + entry.length;
            reader.seek_relative(entry.length as i64)
                .map_err(|e| failed(&format!("skipping a value in {name}"), e))?;
        }
        reader.seek(SeekFrom::Start(8)).map_err(|e| failed(&format!("rewinding {name}"), e))?;
        let mut hash = native_keys_hasher(count);
        offset = 8;
        while let Some(entry) = native_entry(&mut reader, &name, offset, size)? {
            // A Digest is a CBOR byte string of 32 bytes, not an array of integers.
            hash.update([0x58, 32]);
            hash.update(entry.key);
            offset = entry.offset + entry.length;
            reader.seek_relative(entry.length as i64)
                .map_err(|e| failed(&format!("skipping a value in {name}"), e))?;
        }
        if Digest::from(hash.finalize()) != pack.keys {
            return Err(native_refusal(
                &name,
                "native keys differ from the immutable list commitment",
            ));
        }
        Ok(true)
    }

    fn native_entry_at(
        &self,
        pack: &blaze_wire::PackRef,
        offset: u64,
    ) -> Result<Option<NativeEntry>, Error> {
        if offset == pack.size {
            return Ok(None);
        }
        let name = format!("packs/{}.pack", hex(&pack.pack));
        let mut file = self.verified_pack_file(&name)?;
        file.seek(SeekFrom::Start(offset))
            .map_err(|e| failed(&format!("locating a record in {name}"), e))?;
        native_entry(&mut file, &name, offset, pack.size)
    }

    fn verified_pack_file(&self, name: &str) -> Result<File, Error> {
        self.open_blob(name)?.map(|(file, _)| file).ok_or_else(|| {
            native_refusal(name, "previously verified pack vanished under the publication gate")
        })
    }

    fn compare_native_values(
        &self,
        prior_pack: &blaze_wire::PackRef,
        prior: NativeEntry,
        pack: &blaze_wire::PackRef,
        entry: NativeEntry,
    ) -> Result<(), Error> {
        #[cfg(test)]
        if self.bypass_mirrors.load(Ordering::Relaxed) {
            return Ok(());
        }
        let name = format!("packs/{}.pack", hex(&pack.pack));
        let conflict = |why| {
            native_refusal(
                &name,
                &format!(
                    "immutable key {} has conflicting {why} across packs {} and {}",
                    hex(&entry.key),
                    hex(&prior_pack.pack),
                    hex(&pack.pack),
                ),
            )
        };
        if prior.kind != entry.kind || prior.length != entry.length {
            return Err(conflict("kinds or lengths"));
        }
        let prior_name = format!("packs/{}.pack", hex(&prior_pack.pack));
        let mut old = self.verified_pack_file(&prior_name)?;
        let mut new = self.verified_pack_file(&name)?;
        old.seek(SeekFrom::Start(prior.offset))
            .map_err(|e| failed(&format!("locating a value in {prior_name}"), e))?;
        new.seek(SeekFrom::Start(entry.offset))
            .map_err(|e| failed(&format!("locating a value in {name}"), e))?;
        let mut old_bytes = [0; 8192];
        let mut new_bytes = [0; 8192];
        let mut remaining = entry.length;
        while remaining > 0 {
            let chunk = remaining.min(old_bytes.len() as u64) as usize;
            old.read_exact(&mut old_bytes[..chunk])
                .map_err(|e| failed(&format!("reading a value in {prior_name}"), e))?;
            new.read_exact(&mut new_bytes[..chunk])
                .map_err(|e| failed(&format!("reading a value in {name}"), e))?;
            if old_bytes[..chunk] != new_bytes[..chunk] {
                return Err(conflict("bytes"));
            }
            remaining -= chunk as u64;
        }
        Ok(())
    }

    /// Removes the temporary files of writes that were stopped, once they are older than the
    /// hour a remote's collection gives an object: a write that is in flight when this runs is
    /// younger than that, and no request is in flight when the store is opened.
    pub(crate) fn sweep_temporaries(&self, older_than: std::time::Duration) -> Result<(), Error> {
        for sub in ["", "packs", "manifests"] {
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
            self.name_file_whole(&file, &partial, &to)
        })();
        if let Err(e) = write {
            _ = std::fs::remove_file(&partial);
            return Err(failed(&format!("writing {}", object.name), e));
        }
        Ok(())
    }

    /// Drive the publication core on the original handle and paths. In particular, do not
    /// reopen the staged file by name or substitute a driver that changes Windows rename
    /// semantics, creates directories, or syncs an ancestor instead of the original parent.
    fn name_file_whole(&self, file: &File, from: &Path, to: &Path) -> std::io::Result<()> {
        use blaze_core::{Completion, Done, Effect};
        use publication::{Publication, Step};

        let mut publication = Publication::new();
        for _ in 0..Publication::MAX_STEPS {
            let Some(effect) = publication.effect() else { break };
            let tag = effect.tag();
            let step = publication.step();
            #[cfg(test)]
            if *self.publication_failure.lock().unwrap() == Some(step) {
                return Err(std::io::Error::other(format!(
                    "{}: injected {step:?} failure",
                    to.display()
                )));
            }
            // The core uses logical file slots so native paths need no UTF-8 conversion.
            // Rename here reports the atomic namespace change; the core separately asks
            // for its durability barrier. Native's general Rename already bundles both.
            let result = match (step, effect) {
                (Step::FileSync, Effect::Sync { .. }) => file.sync_all().map(|()| Done::Synced),
                (Step::Rename, Effect::Rename { .. }) => {
                    std::fs::rename(from, to).map(|()| Done::Renamed)
                }
                (Step::DirectorySync, Effect::Sync { .. }) => to
                    .parent()
                    .ok_or_else(|| std::io::Error::from(std::io::ErrorKind::InvalidInput))
                    .and_then(|parent| File::open(parent)?.sync_all())
                    .map(|()| Done::Synced),
                _ => return Err(std::io::Error::other("invalid cache publication effect")),
            };
            let done = result.map_err(|error| {
                std::io::Error::new(error.kind(), format!("{}: {step:?}: {error}", to.display()))
            })?;
            publication.complete(Completion { tag, done }).map_err(std::io::Error::other)?;
        }
        if publication.acknowledged() {
            Ok(())
        } else {
            Err(std::io::Error::other(format!(
                "{}: cache publication exhausted its effect bound",
                to.display()
            )))
        }
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
    fn sweeping_staging_files_preserves_published_objects() {
        let dir = scratch();
        let store = Store::open(dir.path()).unwrap();
        let name = format!("manifests/{}", hex(&[1; 32]));
        // Legacy persisted names remain janitor fixtures, not valid new PUTs.
        std::fs::write(store.dir().join(&name), b"published\n").unwrap();
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

    /// A native pack of sorted keys and opaque values, as the distributed agent emits.
    fn pack(values: &[(Digest, &[u8])]) -> Vec<u8> {
        let mut out = blaze_wire::DIST_PACK_MAGIC.to_vec();
        for (key, value) in values {
            out.extend_from_slice(key);
            out.push(0);
            out.extend_from_slice(&(value.len() as u64).to_le_bytes());
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
        let bytes = pack(&[([1; 32], b"first")]);
        let digest = digest(&bytes);
        store.put_blob(&format!("packs/{}.pack", hex(&digest)), &bytes).unwrap();
        let first = blaze_wire::pack_list::encode(&[blaze_wire::PackRef {
            pack: digest,
            size: bytes.len() as u64,
            keys: blaze_wire::digest_of_encoded_name(
                "blaze/dist-pack-keys",
                &blaze_wire::encode(&vec![[1; 32]]),
            ),
            packed: None,
            format: blaze_wire::DIST_PACK_FORMAT,
        }]);
        assert!(store.put_list(name, &first).expect("a write"));
        assert!(!store.put_list(name, b"").expect("a second write"));
        assert_eq!(store.read_small(name, 1000).unwrap().unwrap(), first);
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
        std::fs::remove_dir(dir.path().join("manifests")).unwrap();
        std::os::unix::fs::symlink(outside.path(), dir.path().join("manifests")).unwrap();
        assert!(Store::open(dir.path()).is_err());
    }
}

#[cfg(test)]
mod repair_tests;
