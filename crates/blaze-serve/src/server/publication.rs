//! RAII staging guard for streamed pack uploads.
//!
//! Holds the exclusively `flock`-leased temporary file until [`Temporary::publish`] commits it
//! through [`Store::put_blob_from`]. Dropping an uncommitted [`Temporary`] closes its descriptor
//! and unlinks its path so aborted, timed-out, or oversized uploads never leak stray blobs.

use blaze_own::format_in;
use blaze_sha256::Sha256;
use blaze_wire::{DIST_PACK_MAGIC, WireAlloc, WireString, is_dist_pack};

use crate::blob::ObjectName;
use crate::error::Error;
use crate::store::Store;
use crate::sys::{OwnedFd, unlink_path, write_all_fd};

/// An active staged temporary file for a streamed pack upload.
pub(crate) struct Temporary {
    path: WireString,
    dir_path: WireString,
    fd: Option<OwnedFd>,
    committed: bool,
    written: u64,
    hasher: Sha256,
    prefix: [u8; 8],
    prefix_len: usize,
    alloc: WireAlloc,
}

impl Temporary {
    /// Creates a new locked temporary file in `store` for `name`.
    pub(crate) fn open(store: &Store, name: &ObjectName) -> Result<Self, Error> {
        let staged = store.stage_temporary(name)?;
        Ok(Self {
            path: staged.path,
            dir_path: staged.dir_path,
            fd: Some(staged.fd),
            committed: false,
            written: 0,
            hasher: Sha256::new(),
            prefix: [0u8; 8],
            prefix_len: 0,
            alloc: store.allocator().clone(),
        })
    }

    /// Appends `chunk` to the staged file, enforcing `limit`, checking the leading magic prefix,
    /// and updating the streaming SHA-256 digest.
    pub(crate) fn write_chunk(&mut self, chunk: &[u8], limit: u64) -> Result<(), Error> {
        if chunk.is_empty() {
            return Ok(());
        }
        let chunk_len = u64::try_from(chunk.len()).unwrap_or(u64::MAX);
        let next_total = self.written.saturating_add(chunk_len);
        if next_total > limit {
            return Err(Error::too_large_owned(format_in!(
                self.alloc.clone(),
                "pack upload exceeds {limit}-byte limit"
            )));
        }
        if self.prefix_len < DIST_PACK_MAGIC.len() {
            let need = DIST_PACK_MAGIC.len().saturating_sub(self.prefix_len);
            let take = need.min(chunk.len());
            if let (Some(dst), Some(src)) = (
                self.prefix.get_mut(self.prefix_len..self.prefix_len.saturating_add(take)),
                chunk.get(..take),
            ) {
                dst.copy_from_slice(src);
                self.prefix_len = self.prefix_len.saturating_add(take);
            }
            if self.prefix_len == DIST_PACK_MAGIC.len() && !is_dist_pack(&self.prefix) {
                return Err(Error::refused("pack is not of the distributed pack format"));
            }
        }
        let Some(ref fd) = self.fd else {
            return Err(Error::internal("staged temporary descriptor already closed"));
        };
        write_all_fd(fd.fd(), chunk).map_err(|e| {
            Error::store_io(self.alloc.clone(), "write temp", self.path.as_str(), e)
        })?;
        self.hasher.update(chunk);
        self.written = next_total;
        Ok(())
    }

    /// Commits the staged temporary into `store` under `name`.
    pub(crate) fn publish(mut self, store: &Store, name: &ObjectName) -> Result<bool, Error> {
        self.committed = true;
        let Some(fd) = self.fd.take() else {
            return Err(Error::internal("staged temporary descriptor missing at publish"));
        };
        let starts_with_magic =
            self.prefix_len == DIST_PACK_MAGIC.len() && is_dist_pack(&self.prefix);
        let digest = self.hasher.finish();
        store.put_blob_from(
            name,
            self.path.as_str(),
            fd,
            self.dir_path.as_str(),
            self.written,
            digest,
            starts_with_magic,
        )
    }
}

impl Drop for Temporary {
    fn drop(&mut self) {
        if !self.committed {
            let _ = self.fd.take();
            let _ = unlink_path(self.path.as_str());
        }
    }
}
