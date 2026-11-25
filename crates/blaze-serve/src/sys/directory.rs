//! Directory enumeration for the cache store and its real-filesystem tests.

use core::alloc::Allocator;
use blaze_own::{String, Vec};
use blaze_sys::{Errno, posix::io::{CWD, OpenFlags, OwnedFd, openat}};
#[cfg(target_os = "linux")]
use blaze_sys::linux::getdents64;

use super::with_cpath;

/// One directory entry's name (excluding `.` and `..`).
pub(crate) struct DirEntry<A: Allocator> {
    pub(crate) name: String<A>,
}

/// Opens a directory descriptor (`O_RDONLY | O_DIRECTORY | O_CLOEXEC`).
pub(crate) fn open_dir(path: &str) -> Result<OwnedFd, Errno> {
    let flags = OpenFlags::READ
        .with(OpenFlags::DIRECTORY)
        .with(OpenFlags::CLOSE_ON_EXEC);
    with_cpath(path, |cpath| openat(CWD, cpath, flags, 0))
}

/// Lists all entry names inside directory `path`, skipping `.` and `..`.
pub(crate) fn list_dir<A: Allocator + Clone>(path: &str, alloc: A) -> Result<Vec<DirEntry<A>, A>, Errno> {
    let dir_fd = open_dir(path)?;
    let mut entries = Vec::new_in(alloc.clone());
    #[cfg(target_os = "linux")]
    {
        let mut buf = [0u8; 8192];
        loop {
            let mut count = 0usize;
            for entry in getdents64(dir_fd.fd(), &mut buf)? {
                count = count.saturating_add(1);
                if entry.name == b"." || entry.name == b".." {
                    continue;
                }
                if let Ok(name_str) = core::str::from_utf8(entry.name) {
                    let mut owned = String::with_capacity_in(name_str.len(), alloc.clone());
                    owned.push_str(name_str);
                    entries.push(DirEntry { name: owned });
                }
            }
            if count == 0 {
                break;
            }
        }
    }
    #[cfg(target_os = "macos")]
    {
        use blaze_sys::posix::proc::{ATTR_CMN_NAME, ATTR_CMN_RETURNED_ATTRS, attrlist, getattrlistbulk};
        let mut list = attrlist {
            bitmapcount: 5,
            reserved: 0,
            commonattr: ATTR_CMN_RETURNED_ATTRS | ATTR_CMN_NAME,
            volattr: 0,
            dirattr: 0,
            fileattr: 0,
            forkattr: 0,
        };
        let mut buf = [0u8; 8192];
        loop {
            // SAFETY: the descriptor is open, `list` is a live attrlist, and `buf` has
            // `buf.len()` writable bytes. The call keeps none of these pointers.
            let count = unsafe {
                getattrlistbulk(
                    dir_fd.fd().raw(), (&raw mut list).cast(), buf.as_mut_ptr().cast(), buf.len(), 0,
                )
            };
            if count < 0 {
                return Err(blaze_sys::posix::signal::errno());
            }
            if count == 0 {
                break;
            }
            let mut pos = 0usize;
            for _ in 0..count {
                let Some(len_bytes) = buf.get(pos..pos.saturating_add(4)).and_then(|s| s.first_chunk::<4>()) else {
                    break;
                };
                let rec_len = usize::try_from(u32::from_ne_bytes(*len_bytes)).unwrap_or(0);
                if rec_len < 32 {
                    break;
                }
                // After length(4) + attribute_set_t(20) = offset 24: attrreference_t { dataoffset: i32, length: u32 }.
                let ref_at = pos.saturating_add(24);
                if let (Some(off_b), Some(nlen_b)) = (
                    buf.get(ref_at..ref_at.saturating_add(4)).and_then(|s| s.first_chunk::<4>()),
                    buf.get(ref_at.saturating_add(4)..ref_at.saturating_add(8)).and_then(|s| s.first_chunk::<4>()),
                ) {
                    let off = i32::from_ne_bytes(*off_b);
                    let nlen = usize::try_from(u32::from_ne_bytes(*nlen_b)).unwrap_or(0);
                    if let Ok(off_u) = usize::try_from(off) {
                        let start = ref_at.saturating_add(off_u);
                        let end = start.saturating_add(nlen);
                        if let Some(raw_name) = buf.get(start..end) {
                            let trimmed = match raw_name.iter().position(|&b| b == 0) {
                                Some(nul) => raw_name.get(..nul).unwrap_or(raw_name),
                                None => raw_name,
                            };
                            if trimmed != b"." && trimmed != b".." {
                                if let Ok(name_str) = core::str::from_utf8(trimmed) {
                                    let mut owned = String::with_capacity_in(name_str.len(), alloc.clone());
                                    owned.push_str(name_str);
                                    entries.push(DirEntry { name: owned });
                                }
                            }
                        }
                    }
                }
                pos = pos.saturating_add(rec_len);
            }
        }
    }
    Ok(entries)
}
