//! A warm export must find the bytes emitted for these inputs at this destination.

use blaze_ids::{ContentId, Hasher};
use blaze_sys::posix::io::{CWD, OpenFlags, openat, pread};
use blaze_sys::posix::proc::{AT_SYMLINK_NOFOLLOW, S_IFMT, S_IFREG, stat, stat_at, stat_fd};
use blaze_sys::{Errno, Fd, Path};
use blaze_wire::{Digest, WireAlloc, WireString};
use bstd::int::Int as _;

use super::DistError;

const MAGIC: &[u8; 4] = b"BDF1";
const DOMAIN: &str = "blaze distributed export file 1";
const RECORD_BYTES: usize = 4 + 32 + 8 + 32;

struct OutputFingerprint {
    input: Digest,
    len: u64,
    output: ContentId,
}

impl OutputFingerprint {
    fn encode(&self) -> [u8; RECORD_BYTES] {
        let mut record = [0; RECORD_BYTES];
        record[..4].copy_from_slice(MAGIC);
        record[4..36].copy_from_slice(&self.input);
        record[36..44].copy_from_slice(&self.len.to_le_bytes());
        record[44..].copy_from_slice(&self.output.0);
        record
    }

    fn decode(record: &[u8; RECORD_BYTES]) -> Option<Self> {
        if &record[..4] != MAGIC {
            return None;
        }
        Some(Self {
            input: record[4..36].try_into().ok()?,
            len: u64::from_le_bytes(record[36..44].try_into().ok()?),
            output: ContentId(record[44..].try_into().ok()?),
        })
    }
}

fn sidecar(output: &str, alloc: WireAlloc) -> WireString {
    let mut path = WireString::from_str_in(output, alloc);
    path.push_str(".blaze-dist-fp");
    path
}

/// Publish the input digest with the exact bytes passed to the atomic executable publisher.
/// A concurrent writer may have already replaced the destination when this sidecar is written.
pub(crate) fn write_output_fingerprint<E>(
    path: &str, input: &Digest, bytes: &[u8], alloc: WireAlloc,
    publish: impl FnOnce(&str, &[u8]) -> Result<(), E>,
) -> Result<(), E> {
    let record = OutputFingerprint { input: *input, len: bytes.len().widen_to::<u64>(), output: ContentId::of(DOMAIN, bytes) };
    publish(sidecar(path, alloc).as_str(), &record.encode())
}

fn same_version(a: &stat, b: &stat) -> bool {
    a.st_dev == b.st_dev && a.st_ino == b.st_ino && a.st_size == b.st_size && a.st_mode == b.st_mode
        && (a.st_mtime, a.st_mtime_nsec) == (b.st_mtime, b.st_mtime_nsec)
        && (a.st_ctime, a.st_ctime_nsec) == (b.st_ctime, b.st_ctime_nsec)
}

/// Inspect the open file and confirm the destination still names that version at the end.
/// O_NONBLOCK and O_NOFOLLOW apply even if a FIFO or a symlink replaces the statted file.
fn inspect(path: &str, len: u64, inspect_fd: impl FnOnce(Fd, &stat) -> Result<bool, Errno>) -> Result<bool, Errno> {
    let mut buf = [0; 4096];
    let name = Path::in_buffer(path.as_bytes(), &mut buf)?;
    let snapshot = stat_at(CWD, name, AT_SYMLINK_NOFOLLOW)?;
    if snapshot.st_mode & S_IFMT != S_IFREG || u64::try_from(snapshot.st_size) != Ok(len) {
        return Ok(false);
    }
    let flags = OpenFlags::READ.with(OpenFlags::CLOSE_ON_EXEC).with(OpenFlags::NONBLOCK).with(OpenFlags::NO_FOLLOW);
    let fd = openat(CWD, name, flags, 0)?;
    if !same_version(&snapshot, &stat_fd(fd.fd())?) || !inspect_fd(fd.fd(), &snapshot)? {
        return Ok(false);
    }
    Ok(same_version(&snapshot, &stat_at(CWD, name, AT_SYMLINK_NOFOLLOW)?))
}

fn read_exact(fd: Fd, mut at: u64, mut buf: &mut [u8]) -> Result<bool, Errno> {
    while !buf.is_empty() {
        let read = pread(fd, buf, at)?;
        if read == 0 {
            return Ok(false);
        }
        at += read.widen_to::<u64>();
        buf = buf.get_mut(read..).ok_or(Errno::Io)?;
    }
    Ok(true)
}

fn checked_file(path: &str, len: u64, alloc: WireAlloc, read: impl FnOnce(Fd, &stat) -> Result<bool, Errno>) -> Result<bool, DistError> {
    match inspect(path, len, read) {
        Ok(hit) => Ok(hit),
        Err(Errno::NoEnt | Errno::Loop) => Ok(false),
        Err(errno) => Err(DistError::Fingerprint { path: WireString::from_str_in(path, alloc), errno }),
    }
}

pub(super) fn matches_output_fingerprint(path: &str, input: &Digest, alloc: WireAlloc) -> Result<bool, DistError> {
    let mut record = [0; RECORD_BYTES];
    if !checked_file(sidecar(path, alloc).as_str(), RECORD_BYTES.widen_to::<u64>(), alloc, |fd, _| read_exact(fd, 0, &mut record))? {
        return Ok(false);
    }
    let Some(record) = OutputFingerprint::decode(&record).filter(|record| record.input == *input) else {
        return Ok(false);
    };
    checked_file(path, record.len, alloc, |fd, metadata| {
        if metadata.st_mode & 0o111 != 0o111 {
            return Ok(false);
        }
        let mut hash = Hasher::new(DOMAIN);
        let mut chunk = [0; 16 << 10];
        let mut offset = 0;
        while offset < record.len {
            let take = (record.len - offset).min(chunk.len().widen_to::<u64>());
            let size = take.narrow_or_stop::<usize>(); // At most the length of the buffer.
            let part = chunk.get_mut(..size).ok_or(Errno::Overflow)?;
            if !read_exact(fd, offset, part)? {
                return Ok(false);
            }
            hash.update(part);
            offset += take;
        }
        Ok(hash.finish() == record.output)
    })
}

#[cfg(test)]
mod tests;
