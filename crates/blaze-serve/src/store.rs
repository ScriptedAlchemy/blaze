//! On-disk cache store for content-addressed packs (`packs/<sha256>.pack`) and unit pack lists
//! (`manifests/<unit-key>`).
//!
//! Every write goes through a locked, dot-prefixed temporary file, verified against its name and
//! format, and published through the sans-IO [`publication::Publication`] state machine (`fsync`
//! file -> `rename` -> `fsync` directory). Every manifest publication checks that all referenced
//! packs exist, match their committed digest/size/keys, agree on any shared key's value, and (on
//! conditional CAS repair) preserve every key of the superseded manifest.

pub(crate) mod publication;

#[cfg(test)]
mod repair_tests;

use core::alloc::Allocator;
use core::cmp::Ordering as CmpOrdering;
use core::sync::atomic::{AtomicU64, Ordering};
use core::time::Duration;

use blaze_own::{Heap4, Vec, format_in};
use blaze_sha256::Sha256;
use blaze_wire::{
    DIST_PACK_FORMAT, DIST_PACK_MAGIC, Digest, Limits, PackRef, WireAlloc, WireString, is_dist_pack,
    pack_list,
};

use crate::blob::{Kind, ObjectName, hex_bytes, hex_in};
use crate::error::Error;
use crate::sys::{
    Errno, Fd, OwnedFd, PublicationGate, ReadExactError, SysError, SystemClock, canonical_path,
    create_dir_all, current_pid, dup_cloexec, fstat_len, fsync_fd, join_path, list_dir, lstat_info,
    open_create_new, open_dir, open_read, pread_exact, pread_some, rename_path, try_flock_exclusive,
    unlink_path, write_all_fd,
};
use publication::{Completion, Done, Effect, LogicalDir, LogicalFile, Phase, Publication};

/// Buffer size for streaming pack verification (`64 KiB`).
const SCAN_BUF_LEN: usize = 64 * 1024;

/// Buffer size for streaming value comparison across packs (`8 KiB`).
const COMPARE_BUF_LEN: usize = 8 * 1024;

/// Maximum time to wait on the in-process manifest publication gate (`30 s`).
const GATE_DEADLINE_MS: u64 = 30_000;

/// Maximum attempts to create a unique staging file name.
const MAX_TEMP_ATTEMPTS: usize = 16;

/// On-disk store rooted at a directory holding `packs/` and `manifests/`.
pub struct Store {
    root: WireString,
    limits: Limits,
    alloc: WireAlloc,
    clock: SystemClock,
    next_tmp: AtomicU64,
    publication: PublicationGate,
}

/// A newly created, exclusively `flock`-leased temporary staging file.
pub(crate) struct StagedTemp {
    pub(crate) path: WireString,
    pub(crate) fd: OwnedFd,
    pub(crate) dir_path: WireString,
}

/// Planted verification bug for DST oracle testing only.
#[cfg(test)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum VerifyPlant {
    None,
    SkipCrossPackComparison,
    SkipOldPackComparison,
}

impl Store {
    /// Opens or initializes a store at `root` under `limits`, using `alloc` for internal buffers.
    ///
    /// Refuses symlinks at `root`, `root/packs`, and `root/manifests`.
    pub fn open(root: &str, limits: Limits, alloc: WireAlloc) -> Result<Self, Error> {
        ensure_real_directory(root, alloc.clone())?;
        let canonical = canonical_path(root, alloc.clone())
            .map_err(|e| Error::store_io(alloc.clone(), "canonicalize", root, e))?;
        let packs_dir = join_path(canonical.as_str(), Kind::Pack.directory(), alloc.clone());
        let manifests_dir = join_path(canonical.as_str(), Kind::Manifest.directory(), alloc.clone());
        ensure_real_directory(packs_dir.as_str(), alloc.clone())?;
        ensure_real_directory(manifests_dir.as_str(), alloc.clone())?;
        Ok(Self {
            root: canonical,
            limits,
            alloc,
            clock: SystemClock,
            next_tmp: AtomicU64::new(1),
            publication: PublicationGate::new(),
        })
    }

    /// Canonical path of the store root.
    pub fn root(&self) -> &str {
        self.root.as_str()
    }

    /// Active wire and blob limits.
    pub const fn limits(&self) -> &Limits {
        &self.limits
    }

    /// Allocator used by the store for owned buffers and error messages.
    pub fn allocator(&self) -> &WireAlloc {
        &self.alloc
    }

    /// Absolute path on disk for `name`.
    pub(crate) fn object_path(&self, name: &ObjectName) -> WireString {
        let rel = name.as_string_in(self.alloc.clone());
        join_path(self.root.as_str(), rel.as_str(), self.alloc.clone())
    }

    /// Absolute path of the kind's directory (`<root>/packs` or `<root>/manifests`).
    pub(crate) fn kind_dir(&self, kind: Kind) -> WireString {
        join_path(self.root.as_str(), kind.directory(), self.alloc.clone())
    }

    /// Opens an existing object read-only, returning its descriptor and byte length.
    ///
    /// Returns `Ok(None)` if the object does not exist; refuses symlinks or non-regular files.
    pub fn open_blob(&self, name: &ObjectName) -> Result<Option<(OwnedFd, u64)>, Error> {
        let path = self.object_path(name);
        let meta = match lstat_info(path.as_str()) {
            Ok(meta) => meta,
            Err(Errno::NoEnt) => return Ok(None),
            Err(err) => return Err(Error::store_io(self.alloc.clone(), "stat", path.as_str(), err)),
        };
        if meta.is_symlink || !meta.is_file {
            return Err(Error::malformed_owned(format_in!(
                self.alloc.clone(),
                "store object {} is not a regular file",
                path.as_str()
            )));
        }
        let fd = match open_read(path.as_str()) {
            Ok(fd) => fd,
            Err(Errno::NoEnt) => return Ok(None),
            Err(err) => return Err(Error::store_io(self.alloc.clone(), "open", path.as_str(), err)),
        };
        let len = fstat_len(fd.fd())
            .map_err(|e| Error::store_io(self.alloc.clone(), "fstat", path.as_str(), e))?;
        Ok(Some((fd, len)))
    }

    /// Reads a whole object into `alloc` provided its size does not exceed `cap`.
    pub fn read_small<A: Allocator>(
        &self,
        name: &ObjectName,
        cap: u64,
        alloc: A,
    ) -> Result<Option<Vec<u8, A>>, Error> {
        let Some((fd, len)) = self.open_blob(name)? else {
            return Ok(None);
        };
        if len > cap {
            let obj_name = name.as_string_in(self.alloc.clone());
            return Err(Error::too_large_owned(format_in!(
                self.alloc.clone(),
                "{} is {} bytes, over limit {}",
                obj_name.as_str(),
                len,
                cap
            )));
        }
        let initial_cap = usize::try_from(len).unwrap_or(0);
        let mut out = Vec::with_capacity_in(initial_cap, alloc);
        let mut buf = [0u8; 8192];
        let mut offset = 0u64;
        loop {
            let n = pread_some(fd.fd(), &mut buf, offset).map_err(|e| {
                let p = self.object_path(name);
                Error::store_io(self.alloc.clone(), "read", p.as_str(), e)
            })?;
            if n == 0 {
                break;
            }
            let n_u64 = u64::try_from(n).unwrap_or(u64::MAX);
            let next_offset = offset.saturating_add(n_u64);
            if next_offset > cap {
                let obj_name = name.as_string_in(self.alloc.clone());
                return Err(Error::too_large_owned(format_in!(
                    self.alloc.clone(),
                    "{} grew past limit {}",
                    obj_name.as_str(),
                    cap
                )));
            }
            if let Some(chunk) = buf.get(..n) {
                out.extend_from_slice(chunk);
            }
            offset = next_offset;
        }
        Ok(Some(out))
    }

    /// Creates a uniquely named, `flock`-leased temporary file in `name.kind()`'s directory.
    pub(crate) fn stage_temporary(&self, name: &ObjectName) -> Result<StagedTemp, Error> {
        let dir_path = self.kind_dir(name.kind());
        let file_name = name.file_name_in(self.alloc.clone());
        let pid = current_pid();
        let now_ns = self.clock.monotonic_nanos().unwrap_or(0);
        for _ in 0..MAX_TEMP_ATTEMPTS {
            let seq = self.next_tmp.fetch_add(1, Ordering::Relaxed);
            let tmp_name = format_in!(
                self.alloc.clone(),
                ".{}.{}-{}-{}.tmp",
                file_name.as_str(),
                pid,
                now_ns,
                seq
            );
            let path = join_path(dir_path.as_str(), tmp_name.as_str(), self.alloc.clone());
            match open_create_new(path.as_str(), true) {
                Ok(fd) => {
                    match try_flock_exclusive(fd.fd()) {
                        Ok(true) => return Ok(StagedTemp { path, fd, dir_path }),
                        Ok(false) => {
                            let _ = unlink_path(path.as_str());
                            continue;
                        }
                        Err(err) => {
                            let _ = unlink_path(path.as_str());
                            return Err(Error::store_io(
                                self.alloc.clone(),
                                "flock",
                                path.as_str(),
                                err,
                            ));
                        }
                    }
                }
                Err(Errno::Exist) => continue,
                Err(err) => {
                    return Err(Error::store_io(
                        self.alloc.clone(),
                        "create temp",
                        path.as_str(),
                        err,
                    ));
                }
            }
        }
        Err(Error::internal("exhausted unique temporary names"))
    }

    /// Stores a whole pack from memory, checking magic, blob limit and SHA-256 against `name`.
    ///
    /// Returns `Ok(true)` if newly written, `Ok(false)` if already present.
    pub fn put_blob(&self, name: &ObjectName, body: &[u8]) -> Result<bool, Error> {
        if name.kind() != Kind::Pack {
            return Err(Error::malformed("put_blob only writes packs/<sha256>.pack"));
        }
        let len = u64::try_from(body.len()).unwrap_or(u64::MAX);
        if len > self.limits.blob {
            return Err(Error::too_large_owned(format_in!(
                self.alloc.clone(),
                "pack is {} bytes, over limit {}",
                len,
                self.limits.blob
            )));
        }
        if !is_dist_pack(body) {
            return Err(Error::refused("pack is not of the distributed pack format"));
        }
        let got = blaze_sha256::digest(body);
        if &got != name.digest() {
            let want_hex = hex_in(name.digest(), self.alloc.clone());
            let got_hex = hex_in(&got, self.alloc.clone());
            return Err(Error::refused_owned(format_in!(
                self.alloc.clone(),
                "pack digest mismatch: name {}, body {}",
                want_hex.as_str(),
                got_hex.as_str()
            )));
        }
        let dst = self.object_path(name);
        if self.existing_regular_file(dst.as_str())? {
            return Ok(false);
        }
        self.write_whole(name, body)?;
        Ok(true)
    }

    /// Finalizes a streamed pack from a staged temporary file.
    ///
    /// Takes ownership of `temp_fd` and unlinks `temp_path` on any refusal or if the blob already
    /// exists. Returns `Ok(true)` if newly published, `Ok(false)` if already present.
    pub(crate) fn put_blob_from(
        &self,
        name: &ObjectName,
        temp_path: &str,
        temp_fd: OwnedFd,
        dir_path: &str,
        len: u64,
        got_digest: Digest,
        starts_with_magic: bool,
    ) -> Result<bool, Error> {
        if name.kind() != Kind::Pack {
            let _ = unlink_path(temp_path);
            return Err(Error::malformed("put_blob only writes packs/<sha256>.pack"));
        }
        if len > self.limits.blob {
            let _ = unlink_path(temp_path);
            return Err(Error::too_large_owned(format_in!(
                self.alloc.clone(),
                "pack is {} bytes, over limit {}",
                len,
                self.limits.blob
            )));
        }
        if !starts_with_magic {
            let _ = unlink_path(temp_path);
            return Err(Error::refused("pack is not of the distributed pack format"));
        }
        if &got_digest != name.digest() {
            let _ = unlink_path(temp_path);
            let want_hex = hex_in(name.digest(), self.alloc.clone());
            let got_hex = hex_in(&got_digest, self.alloc.clone());
            return Err(Error::refused_owned(format_in!(
                self.alloc.clone(),
                "pack digest mismatch: name {}, body {}",
                want_hex.as_str(),
                got_hex.as_str()
            )));
        }
        let dst = self.object_path(name);
        if self.existing_regular_file(dst.as_str())? {
            let _ = unlink_path(temp_path);
            return Ok(false);
        }
        self.name_file_whole(temp_path, temp_fd, dst.as_str(), dir_path)?;
        Ok(true)
    }

    /// Publishes a manifest under first-writer semantics (no `If-Match`).
    ///
    /// Returns `Ok(true)` if written, `Ok(false)` if a manifest already existed.
    pub fn put_list(&self, name: &ObjectName, body: &[u8]) -> Result<bool, Error> {
        self.repair_list(name, None, body)
    }

    /// Publishes or conditionally replaces a manifest (`If-Match` = `expected`).
    ///
    /// - When `expected` is `None`: refuses to overwrite an existing manifest (`Ok(false)`).
    /// - When `expected` is `Some(digest)`: requires the existing manifest's SHA-256 to match
    ///   `digest` (`Ok(false)` on mismatch or absence) and requires every key from the existing
    ///   manifest's packs to remain present with equal kind and bytes in the new packs.
    pub fn repair_list(
        &self,
        name: &ObjectName,
        expected: Option<Digest>,
        body: &[u8],
    ) -> Result<bool, Error> {
        if name.kind() != Kind::Manifest {
            return Err(Error::malformed("manifest route only writes manifests/<sha256>"));
        }
        let body_len = u64::try_from(body.len()).unwrap_or(u64::MAX);
        let req_cap = u64::from(self.limits.request);
        if body_len > req_cap {
            return Err(Error::too_large_owned(format_in!(
                self.alloc.clone(),
                "manifest is {} bytes, over limit {}",
                body_len,
                req_cap
            )));
        }
        let new_packs = pack_list::decode(body, self.alloc.clone()).map_err(|err| {
            Error::malformed_owned(format_in!(
                self.alloc.clone(),
                "malformed manifest pack list: {err}"
            ))
        })?;

        let _guard = self.publication.acquire(self.clock, GATE_DEADLINE_MS).map_err(|err| {
            Error::internal_owned(format_in!(
                self.alloc.clone(),
                "acquiring publication gate: {}",
                SysError(err)
            ))
        })?;

        let existing = self.read_small(name, req_cap, self.alloc.clone())?;
        let old_packs = match (expected, existing) {
            (None, Some(_)) => return Ok(false),
            (None, None) => Vec::new_in(self.alloc.clone()),
            (Some(_), None) => return Ok(false),
            (Some(want), Some(current)) => {
                if blaze_sha256::digest(current.as_slice()) != want {
                    return Ok(false);
                }
                pack_list::decode(current.as_slice(), self.alloc.clone()).map_err(|err| {
                    Error::internal_owned(format_in!(
                        self.alloc.clone(),
                        "existing manifest on disk is malformed: {err}"
                    ))
                })?
            }
        };

        self.verify_manifest_packs(old_packs.as_slice(), new_packs.as_slice())?;
        self.write_whole(name, body)?;
        Ok(true)
    }

    /// Verifies that every pack in `old_packs` and `new_packs` exists, passes integrity checks,
    /// has consistent values for any duplicated keys, and preserves every old key in `new_packs`.
    pub(crate) fn verify_manifest_packs(
        &self,
        old_packs: &[PackRef],
        new_packs: &[PackRef],
    ) -> Result<(), Error> {
        verify_manifest_packs_impl(
            self,
            old_packs,
            new_packs,
            #[cfg(test)]
            VerifyPlant::None,
        )
    }

    /// Test-only entrypoint allowing planted verification bugs for the DST oracle.
    #[cfg(test)]
    pub(crate) fn verify_manifest_packs_planted(
        &self,
        old_packs: &[PackRef],
        new_packs: &[PackRef],
        plant: VerifyPlant,
    ) -> Result<(), Error> {
        verify_manifest_packs_impl(self, old_packs, new_packs, plant)
    }

    /// Deletes stale unleased `.tmp` files older than `older_than` from `packs/` and `manifests/`.
    pub fn sweep_temporaries(&self, older_than: Duration) -> Result<usize, Error> {
        let now = self.clock.realtime().map_err(|e| {
            Error::internal_owned(format_in!(self.alloc.clone(), "read wall clock: {}", SysError(e)))
        })?;
        let now_ns = timespec_signed_nanos(now.sec, now.nsec);
        let cutoff_ns = i128::try_from(older_than.as_nanos()).unwrap_or(i128::MAX);
        let mut removed = 0usize;

        for kind in [Kind::Pack, Kind::Manifest] {
            let dir = self.kind_dir(kind);
            let entries = match list_dir(dir.as_str(), self.alloc.clone()) {
                Ok(entries) => entries,
                Err(Errno::NoEnt) => continue,
                Err(err) => {
                    return Err(Error::store_io(self.alloc.clone(), "list dir", dir.as_str(), err));
                }
            };
            for entry in &entries {
                let name = entry.name.as_str();
                if !name.starts_with('.') || !name.ends_with(".tmp") {
                    continue;
                }
                let path = join_path(dir.as_str(), name, self.alloc.clone());
                let meta = match lstat_info(path.as_str()) {
                    Ok(meta) => meta,
                    Err(Errno::NoEnt) => continue,
                    Err(_) => continue,
                };
                if !meta.is_file || meta.is_symlink {
                    continue;
                }
                let mtime_ns = timespec_signed_nanos(meta.mtime_sec, meta.mtime_nsec);
                let age_ns = now_ns.saturating_sub(mtime_ns);
                if age_ns < cutoff_ns {
                    continue;
                }
                let fd = match open_read(path.as_str()) {
                    Ok(fd) => fd,
                    Err(_) => continue,
                };
                match try_flock_exclusive(fd.fd()) {
                    Ok(true) => {
                        if unlink_path(path.as_str()).is_ok() {
                            removed = removed.saturating_add(1);
                        }
                    }
                    Ok(false) | Err(_) => {}
                }
            }
        }
        Ok(removed)
    }

    fn existing_regular_file(&self, path: &str) -> Result<bool, Error> {
        match lstat_info(path) {
            Ok(meta) => {
                if meta.is_symlink || !meta.is_file {
                    return Err(Error::internal_owned(format_in!(
                        self.alloc.clone(),
                        "store path {path} is not a regular file"
                    )));
                }
                Ok(true)
            }
            Err(Errno::NoEnt) => Ok(false),
            Err(err) => Err(Error::store_io(self.alloc.clone(), "stat", path, err)),
        }
    }

    fn write_whole(&self, name: &ObjectName, body: &[u8]) -> Result<(), Error> {
        let staged = self.stage_temporary(name)?;
        if let Err(err) = write_all_fd(staged.fd.fd(), body) {
            let _ = unlink_path(staged.path.as_str());
            return Err(Error::store_io(self.alloc.clone(), "write", staged.path.as_str(), err));
        }
        let dst = self.object_path(name);
        self.name_file_whole(staged.path.as_str(), staged.fd, dst.as_str(), staged.dir_path.as_str())
    }

    /// Drives the sans-IO [`Publication`] state machine (`Sync` -> `Rename` -> `SyncDir`).
    fn name_file_whole(
        &self,
        temp_path: &str,
        temp_fd: OwnedFd,
        dst_path: &str,
        dir_path: &str,
    ) -> Result<(), Error> {
        let (mut machine, mut next) =
            Publication::start(LogicalDir(1), LogicalFile(1), LogicalFile(2), 1);
        loop {
            let completion = match next {
                Effect::Sync { tag, .. } => match fsync_fd(temp_fd.fd()) {
                    Ok(()) => Completion { tag, result: Ok(Done::<()>::Synced) },
                    Err(err) => {
                        let _ = unlink_path(temp_path);
                        return Err(Error::store_io(self.alloc.clone(), "fsync", temp_path, err));
                    }
                },
                Effect::Rename { tag, .. } => match rename_path(temp_path, dst_path) {
                    Ok(()) => Completion { tag, result: Ok(Done::<()>::Renamed) },
                    Err(err) => {
                        let _ = unlink_path(temp_path);
                        return Err(Error::store_io(self.alloc.clone(), "rename", dst_path, err));
                    }
                },
                Effect::SyncDir { tag, .. } => {
                    let dir_fd = open_dir(dir_path).map_err(|err| {
                        Error::store_io(self.alloc.clone(), "open dir", dir_path, err)
                    })?;
                    match fsync_fd(dir_fd.fd()) {
                        Ok(()) => Completion { tag, result: Ok(Done::<()>::DirSynced) },
                        Err(err) => {
                            return Err(Error::store_io(
                                self.alloc.clone(),
                                "fsync dir",
                                dir_path,
                                err,
                            ));
                        }
                    }
                }
            };
            match machine.complete(completion) {
                Ok(Some(effect)) => next = effect,
                Ok(None) => {
                    if machine.phase() == Phase::Acknowledged {
                        return Ok(());
                    }
                    return Err(Error::internal("publication finished without acknowledgement"));
                }
                Err(_) => return Err(Error::internal("publication refused its own completion")),
            }
        }
    }

    fn open_and_verify_pack(&self, pack_ref: &PackRef) -> Result<OwnedFd, Error> {
        let h = hex_bytes(&pack_ref.pack);
        let h_str = core::str::from_utf8(&h).unwrap_or("");
        let name_str = format_in!(self.alloc.clone(), "packs/{h_str}.pack");
        let obj = ObjectName::parse_kind(name_str.as_str(), Kind::Pack)?;
        let Some((fd, actual_len)) = self.open_blob(&obj)? else {
            return Err(Error::refused_owned(format_in!(
                self.alloc.clone(),
                "manifest names missing pack {}",
                name_str.as_str()
            )));
        };
        verify_pack_fd(fd.fd(), actual_len, pack_ref, name_str.as_str(), self.alloc.clone())?;
        Ok(fd)
    }
}

/// Ensures `path` exists as a real (non-symlink) directory, creating missing directories as needed.
fn ensure_real_directory(path: &str, alloc: WireAlloc) -> Result<(), Error> {
    match lstat_info(path) {
        Ok(meta) => {
            if meta.is_symlink {
                return Err(Error::internal_owned(format_in!(
                    alloc,
                    "store directory {path} must not be a symlink"
                )));
            }
            if !meta.is_dir {
                return Err(Error::internal_owned(format_in!(
                    alloc,
                    "store path {path} is not a directory"
                )));
            }
            Ok(())
        }
        Err(Errno::NoEnt) => {
            create_dir_all(path).map_err(|e| Error::store_io(alloc.clone(), "mkdir", path, e))?;
            let meta = lstat_info(path)
                .map_err(|e| Error::store_io(alloc.clone(), "stat", path, e))?;
            if meta.is_symlink || !meta.is_dir {
                return Err(Error::internal_owned(format_in!(
                    alloc,
                    "store directory {path} must be a real directory"
                )));
            }
            Ok(())
        }
        Err(err) => Err(Error::store_io(alloc, "stat", path, err)),
    }
}

fn timespec_signed_nanos(sec: i64, nsec: i64) -> i128 {
    i128::from(sec).saturating_mul(1_000_000_000).saturating_add(i128::from(nsec))
}

// ---------------------------------------------------------------------------------------------
// Streaming native pack verification & multi-pack key-value merge check.
// ---------------------------------------------------------------------------------------------

/// Header of a single record inside a native pack, along with its byte offset in the pack file.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct RecordMeta {
    pub(crate) key: Digest,
    pub(crate) kind: u8,
    pub(crate) value_offset: u64,
    pub(crate) value_len: u64,
}

/// Buffered forward scanner over a native pack file descriptor using explicit `pread` offsets.
///
/// Never loads values into memory when skipping; advances from its logical file position so
/// skips across buffer boundaries are exact.
pub(crate) struct PackScanner {
    fd: OwnedFd,
    file_len: u64,
    logical_pos: u64,
    buf_start: u64,
    buf_filled: usize,
    buf: Vec<u8, WireAlloc>,
}

impl PackScanner {
    /// Opens a scanner at byte 0 with a `buf_len`-byte read window.
    pub(crate) fn new(fd: OwnedFd, file_len: u64, buf_len: usize, alloc: WireAlloc) -> Self {
        let cap = buf_len.max(64);
        let mut buf = Vec::with_capacity_in(cap, alloc);
        buf.resize(cap, 0);
        Self { fd, file_len, logical_pos: 0, buf_start: 0, buf_filled: 0, buf }
    }

    /// Logical position in the file.
    pub(crate) const fn pos(&self) -> u64 {
        self.logical_pos
    }

    /// Underlying file descriptor.
    pub(crate) fn fd(&self) -> Fd {
        self.fd.fd()
    }

    /// Reads exact `dst.len()` bytes from the current logical position.
    pub(crate) fn read_exact(&mut self, dst: &mut [u8]) -> Result<(), ReadExactError> {
        let mut filled = 0usize;
        while filled < dst.len() {
            let avail = self.buffered_slice();
            if !avail.is_empty() {
                let take = avail.len().min(dst.len().saturating_sub(filled));
                if let (Some(src), Some(out)) =
                    (avail.get(..take), dst.get_mut(filled..filled.saturating_add(take)))
                {
                    out.copy_from_slice(src);
                    let take_u64 = u64::try_from(take).unwrap_or(0);
                    self.logical_pos = self.logical_pos.saturating_add(take_u64);
                    filled = filled.saturating_add(take);
                    continue;
                }
            }
            if !self.refill().map_err(ReadExactError::Os)? {
                return Err(ReadExactError::UnexpectedEof);
            }
        }
        Ok(())
    }

    /// Feeds `len` bytes starting at `self.logical_pos` into `hasher` and advances `logical_pos`.
    pub(crate) fn hash_bytes(&mut self, mut len: u64, hasher: &mut Sha256) -> Result<(), ReadExactError> {
        while len > 0 {
            let avail = self.buffered_slice();
            if !avail.is_empty() {
                let take_u64 = len.min(u64::try_from(avail.len()).unwrap_or(u64::MAX));
                let take = usize::try_from(take_u64).unwrap_or(avail.len());
                if let Some(chunk) = avail.get(..take) {
                    hasher.update(chunk);
                }
                self.logical_pos = self.logical_pos.saturating_add(take_u64);
                len = len.saturating_sub(take_u64);
                continue;
            }
            if !self.refill().map_err(ReadExactError::Os)? {
                return Err(ReadExactError::UnexpectedEof);
            }
        }
        Ok(())
    }

    /// Advances the logical position by `len` bytes without reading them from disk.
    pub(crate) fn skip(&mut self, len: u64) -> Result<(), ReadExactError> {
        let Some(next) = self.logical_pos.checked_add(len) else {
            return Err(ReadExactError::UnexpectedEof);
        };
        if next > self.file_len {
            return Err(ReadExactError::UnexpectedEof);
        }
        self.logical_pos = next;
        Ok(())
    }

    /// Reads the next native record header (`key[32] || kind[1] || len[8]`) and skips its payload,
    /// returning `Ok(None)` cleanly at exact EOF.
    pub(crate) fn next_record(&mut self) -> Result<Option<RecordMeta>, ReadExactError> {
        if self.logical_pos >= self.file_len {
            return Ok(None);
        }
        let mut header = [0u8; 41];
        self.read_exact(&mut header)?;
        let Some((key, rest)) = header.split_first_chunk::<32>() else {
            return Err(ReadExactError::UnexpectedEof);
        };
        let Some((&kind, len_bytes)) = rest.split_first() else {
            return Err(ReadExactError::UnexpectedEof);
        };
        let Some(len_arr) = len_bytes.first_chunk::<8>() else {
            return Err(ReadExactError::UnexpectedEof);
        };
        let value_len = u64::from_le_bytes(*len_arr);
        let value_offset = self.logical_pos;
        self.skip(value_len)?;
        Ok(Some(RecordMeta { key: *key, kind, value_offset, value_len }))
    }

    fn buffered_slice(&self) -> &[u8] {
        let buf_end = self
            .buf_start
            .saturating_add(u64::try_from(self.buf_filled).unwrap_or(0));
        if self.logical_pos < self.buf_start || self.logical_pos >= buf_end {
            return &[];
        }
        let rel = usize::try_from(self.logical_pos.saturating_sub(self.buf_start)).unwrap_or(0);
        self.buf.get(rel..self.buf_filled).unwrap_or(&[])
    }

    fn refill(&mut self) -> Result<bool, Errno> {
        if self.logical_pos >= self.file_len {
            self.buf_start = self.logical_pos;
            self.buf_filled = 0;
            return Ok(false);
        }
        let n = pread_some(self.fd.fd(), self.buf.as_mut_slice(), self.logical_pos)?;
        self.buf_start = self.logical_pos;
        self.buf_filled = n;
        Ok(n > 0)
    }
}

/// Encodes the CBOR definite-length array header for `count` items into `out`, returning the slice.
pub(crate) fn encode_cbor_array_header(count: u64, out: &mut [u8; 9]) -> &[u8] {
    encode_cbor_uint_header(4 << 5, count, out)
}

/// Encodes the CBOR definite-length byte-string header for `len` bytes into `out`.
pub(crate) fn encode_cbor_bytes_header(len: u64, out: &mut [u8; 9]) -> &[u8] {
    encode_cbor_uint_header(2 << 5, len, out)
}

fn encode_cbor_uint_header(major: u8, n: u64, out: &mut [u8; 9]) -> &[u8] {
    if n < 24 {
        let low = u8::try_from(n).unwrap_or(0);
        out[0] = major | low;
        out.get(..1).unwrap_or(&[])
    } else if let Ok(v) = u8::try_from(n) {
        out[0] = major | 24;
        out[1] = v;
        out.get(..2).unwrap_or(&[])
    } else if let Ok(v) = u16::try_from(n) {
        out[0] = major | 25;
        let b = v.to_be_bytes();
        out[1] = b[0];
        out[2] = b[1];
        out.get(..3).unwrap_or(&[])
    } else if let Ok(v) = u32::try_from(n) {
        out[0] = major | 26;
        let b = v.to_be_bytes();
        out[1] = b[0];
        out[2] = b[1];
        out[3] = b[2];
        out[4] = b[3];
        out.get(..5).unwrap_or(&[])
    } else {
        out[0] = major | 27;
        let b = n.to_be_bytes();
        out[1] = b[0];
        out[2] = b[1];
        out[3] = b[2];
        out[4] = b[3];
        out[5] = b[4];
        out[6] = b[5];
        out[7] = b[6];
        out[8] = b[7];
        out.as_slice()
    }
}

/// Streaming two-pass verification of a native pack file descriptor against `expected`.
///
/// Pass 1 verifies size, magic header, strictly ascending keys, record boundaries, exact EOF,
/// and whole-pack SHA-256 while counting keys. Pass 2 computes the streaming CBOR key-array
/// digest (`blaze/dist-pack-keys`) with O(1) memory and checks it against `expected.keys`.
pub(crate) fn verify_pack_fd(
    fd: Fd,
    actual_len: u64,
    expected: &PackRef,
    label: &str,
    alloc: WireAlloc,
) -> Result<(), Error> {
    if expected.format != DIST_PACK_FORMAT {
        return Err(Error::refused_owned(format_in!(
            alloc,
            "pack {label} has unsupported format {}",
            expected.format
        )));
    }
    if expected.size > Limits::DOCUMENTED.blob {
        return Err(Error::refused_owned(format_in!(
            alloc,
            "pack {label} exceeds documented blob limit"
        )));
    }
    if actual_len != expected.size {
        return Err(Error::refused_owned(format_in!(
            alloc,
            "pack {label} is {} bytes on disk, committed as {}",
            actual_len,
            expected.size
        )));
    }
    let dup1 = dup_cloexec(fd)
        .map_err(|e| Error::store_io(alloc.clone(), "dup", label, e))?;
    let mut scan = PackScanner::new(dup1, actual_len, SCAN_BUF_LEN, alloc.clone());
    let mut pack_hasher = Sha256::new();

    let mut magic = [0u8; 8];
    scan.read_exact(&mut magic).map_err(|_| {
        Error::refused_owned(format_in!(alloc.clone(), "pack {label} is shorter than magic header"))
    })?;
    if magic != DIST_PACK_MAGIC {
        return Err(Error::refused_owned(format_in!(
            alloc,
            "pack {label} has invalid DIST_PACK_MAGIC"
        )));
    }
    pack_hasher.update(&magic);

    let mut prev_key: Option<Digest> = None;
    let mut key_count = 0u64;
    while scan.pos() < actual_len {
        let mut header = [0u8; 41];
        scan.read_exact(&mut header).map_err(|_| {
            Error::refused_owned(format_in!(
                alloc.clone(),
                "pack {label} truncated in record header"
            ))
        })?;
        pack_hasher.update(&header);
        let Some((key, rest)) = header.split_first_chunk::<32>() else {
            return Err(Error::refused("truncated record key"));
        };
        let Some((_, len_bytes)) = rest.split_first() else {
            return Err(Error::refused("truncated record kind"));
        };
        let Some(len_arr) = len_bytes.first_chunk::<8>() else {
            return Err(Error::refused("truncated record length"));
        };
        let value_len = u64::from_le_bytes(*len_arr);
        if let Some(prev) = prev_key {
            if *key <= prev {
                return Err(Error::refused_owned(format_in!(
                    alloc,
                    "pack {label} keys are not strictly sorted"
                )));
            }
        }
        prev_key = Some(*key);
        key_count = key_count.saturating_add(1);
        scan.hash_bytes(value_len, &mut pack_hasher).map_err(|_| {
            Error::refused_owned(format_in!(
                alloc.clone(),
                "pack {label} truncated in record value"
            ))
        })?;
    }

    let got_pack = pack_hasher.finish();
    if got_pack != expected.pack {
        return Err(Error::refused_owned(format_in!(
            alloc,
            "pack {label} digest mismatch"
        )));
    }

    // Pass 2: stream keys into domain-separated CBOR array digest without buffering all keys.
    let dup2 = dup_cloexec(fd)
        .map_err(|e| Error::store_io(alloc.clone(), "dup", label, e))?;
    let mut key_scan = PackScanner::new(dup2, actual_len, SCAN_BUF_LEN, alloc.clone());
    key_scan.skip(u64::try_from(DIST_PACK_MAGIC.len()).unwrap_or(8)).map_err(|_| {
        Error::refused_owned(format_in!(alloc.clone(), "pack {label} magic skip failed"))
    })?;

    let mut keys_hasher = Sha256::new();
    keys_hasher.update(b"blaze/blaze/dist-pack-keys/v5\0");
    let mut hdr_buf = [0u8; 9];
    keys_hasher.update(encode_cbor_array_header(key_count, &mut hdr_buf));
    let mut bytes_hdr_buf = [0u8; 9];
    let key_bstr_hdr = encode_cbor_bytes_header(32, &mut bytes_hdr_buf);

    let mut seen = 0u64;
    while let Some(rec) = key_scan.next_record().map_err(|_| {
        Error::refused_owned(format_in!(alloc.clone(), "pack {label} key pass failed"))
    })? {
        keys_hasher.update(key_bstr_hdr);
        keys_hasher.update(&rec.key);
        seen = seen.saturating_add(1);
    }
    if seen != key_count || keys_hasher.finish() != expected.keys {
        return Err(Error::refused_owned(format_in!(
            alloc,
            "pack {label} keys digest does not match commitment"
        )));
    }
    Ok(())
}

/// Which side of a manifest publication a pack cursor belongs to.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
enum Side {
    Old,
    New,
}

struct OpenPack {
    side: Side,
    scanner: PackScanner,
}

#[derive(Clone, Copy, Eq, PartialEq)]
struct FrontierEntry {
    key: Digest,
    side: Side,
    pack_idx: usize,
    record: RecordMeta,
}

impl Ord for FrontierEntry {
    fn cmp(&self, other: &Self) -> CmpOrdering {
        self.key
            .cmp(&other.key)
            .then_with(|| self.side.cmp(&other.side))
            .then_with(|| self.pack_idx.cmp(&other.pack_idx))
    }
}

impl PartialOrd for FrontierEntry {
    fn partial_cmp(&self, other: &Self) -> Option<CmpOrdering> {
        Some(self.cmp(other))
    }
}

fn verify_manifest_packs_impl(
    store: &Store,
    old_packs: &[PackRef],
    new_packs: &[PackRef],
    #[cfg(test)] plant: VerifyPlant,
) -> Result<(), Error> {
    let total = old_packs.len().saturating_add(new_packs.len());
    let mut open_packs = Vec::with_capacity_in(total, store.alloc.clone());
    let mut frontier = Heap4::with_capacity_in(total, store.alloc.clone());

    for pack_ref in old_packs {
        let fd = store.open_and_verify_pack(pack_ref)?;
        let mut scanner = PackScanner::new(fd, pack_ref.size, SCAN_BUF_LEN, store.alloc.clone());
        scanner
            .skip(u64::try_from(DIST_PACK_MAGIC.len()).unwrap_or(8))
            .map_err(|_| Error::refused("old pack header skip failed"))?;
        let idx = open_packs.len();
        if let Some(record) = scanner
            .next_record()
            .map_err(|_| Error::refused("old pack record scan failed"))?
        {
            frontier.push(FrontierEntry { key: record.key, side: Side::Old, pack_idx: idx, record });
        }
        open_packs.push(OpenPack { side: Side::Old, scanner });
    }

    for pack_ref in new_packs {
        let fd = store.open_and_verify_pack(pack_ref)?;
        let mut scanner = PackScanner::new(fd, pack_ref.size, SCAN_BUF_LEN, store.alloc.clone());
        scanner
            .skip(u64::try_from(DIST_PACK_MAGIC.len()).unwrap_or(8))
            .map_err(|_| Error::refused("new pack header skip failed"))?;
        let idx = open_packs.len();
        if let Some(record) = scanner
            .next_record()
            .map_err(|_| Error::refused("new pack record scan failed"))?
        {
            frontier.push(FrontierEntry { key: record.key, side: Side::New, pack_idx: idx, record });
        }
        open_packs.push(OpenPack { side: Side::New, scanner });
    }

    let mut group = Vec::new_in(store.alloc.clone());
    while let Some(first) = frontier.pop() {
        let current_key = first.key;
        group.clear();
        group.push(first);
        while let Some(peek) = frontier.peek() {
            if peek.key != current_key {
                break;
            }
            if let Some(item) = frontier.pop() {
                group.push(item);
            }
        }

        let mut saw_old = false;
        let mut saw_new = false;
        let mut canonical_entry: Option<FrontierEntry> = None;

        for &entry in group.as_slice() {
            match entry.side {
                Side::Old => saw_old = true,
                Side::New => saw_new = true,
            }
            if let Some(canon) = canonical_entry {
                #[cfg(test)]
                let skip = (plant == VerifyPlant::SkipCrossPackComparison
                    && canon.side == Side::New
                    && entry.side == Side::New)
                    || (plant == VerifyPlant::SkipOldPackComparison
                        && canon.side == Side::Old
                        && entry.side == Side::New);
                #[cfg(not(test))]
                let skip = false;

                if !skip {
                    if canon.record.kind != entry.record.kind
                        || canon.record.value_len != entry.record.value_len
                    {
                        let key_hex = hex_in(&current_key, store.alloc.clone());
                        return Err(Error::refused_owned(format_in!(
                            store.alloc.clone(),
                            "conflicting value metadata for key {}",
                            key_hex.as_str()
                        )));
                    }
                    let (Some(pack_a), Some(pack_b)) =
                        (open_packs.get(canon.pack_idx), open_packs.get(entry.pack_idx))
                    else {
                        return Err(Error::internal("pack cursor index out of range"));
                    };
                    let equal = compare_native_values(
                        pack_a.scanner.fd(),
                        canon.record.value_offset,
                        pack_b.scanner.fd(),
                        entry.record.value_offset,
                        canon.record.value_len,
                        store.alloc.clone(),
                    )?;
                    if !equal {
                        let key_hex = hex_in(&current_key, store.alloc.clone());
                        return Err(Error::refused_owned(format_in!(
                            store.alloc.clone(),
                            "conflicting value bytes for key {}",
                            key_hex.as_str()
                        )));
                    }
                }
            } else {
                canonical_entry = Some(entry);
            }
        }

        if saw_old && !saw_new {
            let key_hex = hex_in(&current_key, store.alloc.clone());
            return Err(Error::refused_owned(format_in!(
                store.alloc.clone(),
                "replacement manifest dropped key {}",
                key_hex.as_str()
            )));
        }

        // Advance every pack that contributed to `current_key`.
        for &entry in group.as_slice() {
            if let Some(pack) = open_packs.get_mut(entry.pack_idx) {
                if let Some(next_rec) = pack
                    .scanner
                    .next_record()
                    .map_err(|_| Error::refused("pack advance failed"))?
                {
                    frontier.push(FrontierEntry {
                        key: next_rec.key,
                        side: pack.side,
                        pack_idx: entry.pack_idx,
                        record: next_rec,
                    });
                }
            }
        }
    }
    Ok(())
}

/// Compares `len` bytes from `(fd_a, off_a)` and `(fd_b, off_b)` in bounded `8 KiB` chunks.
fn compare_native_values(
    fd_a: Fd,
    mut off_a: u64,
    fd_b: Fd,
    mut off_b: u64,
    mut remaining: u64,
    alloc: WireAlloc,
) -> Result<bool, Error> {
    let mut buf_a = [0u8; COMPARE_BUF_LEN];
    let mut buf_b = [0u8; COMPARE_BUF_LEN];
    while remaining > 0 {
        let step_u64 = remaining.min(u64::try_from(COMPARE_BUF_LEN).unwrap_or(8192));
        let step = usize::try_from(step_u64).unwrap_or(COMPARE_BUF_LEN);
        let (Some(slice_a), Some(slice_b)) = (buf_a.get_mut(..step), buf_b.get_mut(..step)) else {
            return Err(Error::internal("compare buffer slice failed"));
        };
        pread_exact(fd_a, slice_a, off_a).map_err(|_| {
            Error::refused_owned(format_in!(alloc.clone(), "failed reading value for comparison"))
        })?;
        pread_exact(fd_b, slice_b, off_b).map_err(|_| {
            Error::refused_owned(format_in!(alloc.clone(), "failed reading value for comparison"))
        })?;
        if slice_a != slice_b {
            return Ok(false);
        }
        off_a = off_a.saturating_add(step_u64);
        off_b = off_b.saturating_add(step_u64);
        remaining = remaining.saturating_sub(step_u64);
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use core::alloc::{AllocError, Allocator, Layout};
    use core::cell::Cell;
    use core::ptr::NonNull;

    use blaze_own::{String, Vec, format_in};
    use blaze_wire::{DIST_PACK_MAGIC, Limits, WireAlloc, digest_of_encoded_name};

    use super::*;
    use crate::blob::{Kind, ObjectName, hex_in};
    use crate::error::ErrorCode;
    use crate::sys::{
        create_dir_all, current_pid, join_path, list_dir, lstat_info, open_create_new, open_read,
        remove_empty_dir, unlink_path, write_all_fd,
    };

    /// Test-only RAII temp directory rooted under `/tmp` with recursive cleanup on drop.
    pub(crate) struct TestDir {
        path: WireString,
        alloc: WireAlloc,
    }

    impl TestDir {
        pub(crate) fn new(tag: &str, alloc: WireAlloc) -> Self {
            let pid = current_pid();
            let stamp = SystemClock.monotonic_nanos().unwrap_or(0);
            let path = format_in!(alloc.clone(), "/tmp/blaze-serve-{tag}-{pid}-{stamp}");
            assert!(create_dir_all(path.as_str()).is_ok(), "create temp dir {}", path.as_str());
            Self { path, alloc }
        }

        pub(crate) fn path(&self) -> &str {
            self.path.as_str()
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            remove_tree_iterative(self.path.as_str(), self.alloc.clone());
        }
    }

    pub(crate) fn remove_tree_iterative(root: &str, alloc: WireAlloc) {
        let mut stack = Vec::new_in(alloc.clone());
        let mut root_s = String::with_capacity_in(root.len(), alloc.clone());
        root_s.push_str(root);
        stack.push((root_s, false));
        while let Some((dir, ready)) = stack.pop() {
            if ready {
                let _ = remove_empty_dir(dir.as_str());
                continue;
            }
            let Ok(entries) = list_dir(dir.as_str(), alloc.clone()) else {
                let _ = unlink_path(dir.as_str());
                continue;
            };
            stack.push((dir.clone(), true));
            for entry in entries {
                let child = join_path(dir.as_str(), entry.name.as_str(), alloc.clone());
                let is_real_dir = lstat_info(child.as_str())
                    .map(|m| m.is_dir && !m.is_symlink)
                    .unwrap_or(false);
                if is_real_dir {
                    stack.push((child, false));
                } else {
                    let _ = unlink_path(child.as_str());
                }
            }
        }
    }

    #[derive(Default)]
    struct Counts {
        allocations: Cell<usize>,
        deallocations: Cell<usize>,
    }

    #[derive(Clone, Copy)]
    struct Counted<'a> {
        counts: &'a Counts,
        inner: WireAlloc,
    }

    unsafe impl Allocator for Counted<'_> {
        fn allocate(&self, layout: Layout) -> Result<NonNull<[u8]>, AllocError> {
            let ptr = self.inner.allocate(layout)?;
            self.counts.allocations.set(self.counts.allocations.get().saturating_add(1));
            Ok(ptr)
        }

        unsafe fn deallocate(&self, ptr: NonNull<u8>, layout: Layout) {
            self.counts.deallocations.set(self.counts.deallocations.get().saturating_add(1));
            // SAFETY: delegates to the same `inner` allocator.
            unsafe { self.inner.deallocate(ptr, layout) };
        }
    }

    #[test]
    fn bounded_object_reads_use_the_callers_allocator_and_preserve_size_refusals() {
        let alloc = blaze_own::testing::heap().shared();
        let dir = TestDir::new("bounded-read", alloc.clone());
        let Ok(store) = Store::open(dir.path(), Limits::DOCUMENTED, alloc.clone()) else {
            panic!("open store failed");
        };

        let mut pack_bytes = Vec::new_in(alloc.clone());
        pack_bytes.extend_from_slice(&DIST_PACK_MAGIC);
        pack_bytes.extend_from_slice(b"payload-for-bounded-read");
        let digest = blaze_sha256::digest(pack_bytes.as_slice());
        let name_str = format_in!(alloc.clone(), "packs/{}.pack", hex_in(&digest, alloc.clone()).as_str());
        let Ok(name) = ObjectName::parse_kind(name_str.as_str(), Kind::Pack) else {
            panic!("parse name failed");
        };
        assert_eq!(store.put_blob(&name, pack_bytes.as_slice()).ok(), Some(true));

        let counts = Counts::default();
        let counted = Counted { counts: &counts, inner: alloc.clone() };
        let Ok(Some(got)) = store.read_small(&name, u64::try_from(pack_bytes.len()).unwrap_or(0), counted) else {
            panic!("read_small failed");
        };
        assert!(core::ptr::eq(got.allocator().counts, &counts));
        assert_eq!(got.as_slice(), pack_bytes.as_slice());
        drop(got);
        assert_eq!(counts.allocations.get(), counts.deallocations.get());

        // Cap below file size must refuse with TooLarge.
        let err = store
            .read_small(&name, u64::try_from(pack_bytes.len().saturating_sub(1)).unwrap_or(0), counted)
            .err();
        assert!(err.is_some());
        if let Some(err) = err {
            assert_eq!(err.code, ErrorCode::TooLarge);
        }
    }

    #[test]
    fn native_scan_crosses_buffer_edges_and_skips_from_the_logical_position() {
        let alloc = blaze_own::testing::heap().shared();
        let dir = TestDir::new("scan-edges", alloc.clone());
        let file_path = join_path(dir.path(), "edge.pack", alloc.clone());

        let mut raw = Vec::new_in(alloc.clone());
        raw.extend_from_slice(&DIST_PACK_MAGIC);
        for i in 0u8..6 {
            let key = [i.saturating_add(1); 32];
            let payload_len = usize::from(i).saturating_mul(29).saturating_add(17);
            raw.extend_from_slice(&key);
            raw.push(i);
            let len_u64 = u64::try_from(payload_len).unwrap_or(0);
            raw.extend_from_slice(&len_u64.to_le_bytes());
            for j in 0..payload_len {
                let b = u8::try_from(j % 251).unwrap_or(0) ^ i;
                raw.push(b);
            }
        }
        let Ok(fd) = open_create_new(file_path.as_str(), true) else {
            panic!("create edge pack");
        };
        assert!(write_all_fd(fd.fd(), raw.as_slice()).is_ok());
        drop(fd);

        // Use the minimum buffer window (64 bytes) so every record crosses buffer boundaries.
        let Ok(rfd) = open_read(file_path.as_str()) else {
            panic!("open edge pack");
        };
        let file_len = u64::try_from(raw.len()).unwrap_or(0);
        let mut scan = PackScanner::new(rfd, file_len, 64, alloc.clone());
        let mut magic = [0u8; 8];
        assert!(scan.read_exact(&mut magic).is_ok());
        assert_eq!(magic, DIST_PACK_MAGIC);

        let mut seen = 0u8;
        while let Ok(Some(rec)) = scan.next_record() {
            assert_eq!(rec.key, [seen.saturating_add(1); 32]);
            assert_eq!(rec.kind, seen);
            let expected_len = u64::from(seen).saturating_mul(29).saturating_add(17);
            assert_eq!(rec.value_len, expected_len);
            seen = seen.saturating_add(1);
        }
        assert_eq!(seen, 6);
        assert_eq!(scan.pos(), file_len);
    }

    #[test]
    fn native_key_digest_headers_match_the_wire_at_every_length_boundary() {
        let alloc = blaze_own::testing::heap().shared();
        for count in [0usize, 1, 23, 24, 25, 255, 256, 257] {
            let mut keys = Vec::with_capacity_in(count, alloc.clone());
            for i in 0..count {
                let mut k = [0u8; 32];
                let idx = u64::try_from(i).unwrap_or(0).to_be_bytes();
                if let Some(tail) = k.get_mut(24..32) {
                    tail.copy_from_slice(&idx);
                }
                keys.push(k);
            }
            let wire_encoded = blaze_wire::encode(&keys, alloc.clone());
            let expected = digest_of_encoded_name("blaze/dist-pack-keys", wire_encoded.as_slice());

            let mut hasher = Sha256::new();
            hasher.update(b"blaze/blaze/dist-pack-keys/v5\0");
            let mut arr_hdr = [0u8; 9];
            hasher.update(encode_cbor_array_header(u64::try_from(count).unwrap_or(0), &mut arr_hdr));
            let mut b_hdr = [0u8; 9];
            let key_hdr = encode_cbor_bytes_header(32, &mut b_hdr);
            for k in &keys {
                hasher.update(key_hdr);
                hasher.update(k);
            }
            assert_eq!(hasher.finish(), expected, "key digest mismatch at length {count}");
        }
    }
}
