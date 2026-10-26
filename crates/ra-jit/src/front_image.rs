//! The front-end image (`docs/research/front-end-image.md`; #274 for the dependency layer, unit
//! D3): the frozen parts of the dependency crates (`frozen.rs`) in one file that a worker maps
//! read-only instead of reading each part out of the store into its own heap. Off unless
//! `RA_JIT_FRONT_IMAGE` names a file. A prototype: what it holds is the parts' bytes, which are
//! still decoded into the heap when name resolution asks for them, so what it takes out of the
//! heap is the encoded bytes and not the decoded `DefMap`.
//!
//! The file is
//!
//! * a header: magic, version, the number of parts, the build name of the ra-jit that wrote it,
//!   the root, and where the index, the payloads and the end are;
//! * the index, one entry per part in the order of its key: key, part, offset, length and the
//!   SHA-256 of its bytes;
//! * the payloads, each part's bytes as the store has them, 64-byte aligned, from a page
//!   boundary on.
//!
//! The root is SHA-256 over the build name, the number of parts and the index, so checking it
//! reads the header and the index, a few tens of KB, and no payload. A part is checked against
//! its own SHA-256 when served; `RA_JIT_FRONT_IMAGE_VERIFY=1` checks the whole file. The file
//! is written to a name of its own and renamed, so a process that maps the old one keeps it.
//! A part the image lacks is read from the store as before; the image is written when the file
//! is absent or is not accepted, and never extended.

use std::fs::File;
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use hir_def::nameres::frozen::Bytes;
use memmap2::Mmap;
use rustc_hash::FxHashMap;
use sha2::{Digest, Sha256};

const MAGIC: [u8; 8] = *b"RAJITFEI";
const VERSION: u32 = 1;
/// The largest page of a machine this runs on (Apple silicon's), a multiple of Linux's.
const PAGE: usize = 16 * 1024;
const ALIGN: usize = 64;
const KEY: usize = 32;
const NAME: usize = 16;
/// Magic 0..8, version 8..12, parts 12..16, build name 16..32, root 32..64, index at 64..72,
/// payloads at 72..80, end 80..88.
const HEADER: usize = 88;
/// Key 0..32, part 32..40, offset 40..48, length 48..56, SHA-256 56..88.
const ENTRY: usize = 88;

/// The file `RA_JIT_FRONT_IMAGE` names, when it names one.
pub(crate) fn path() -> Option<PathBuf> {
    match std::env::var_os("RA_JIT_FRONT_IMAGE") {
        Some(it) if !it.is_empty() && it != "0" && it != "off" => Some(PathBuf::from(it)),
        _ => None,
    }
}

/// Whether to say what the image and the frozen parts hold: `RA_JIT_FRONT_STATS` set.
pub(crate) fn stats() -> bool {
    std::env::var_os("RA_JIT_FRONT_STATS").is_some_and(|it| !it.is_empty() && it != "0")
}

#[derive(Clone, Copy)]
struct Located {
    at: usize,
    len: usize,
    sha: [u8; 32],
}

/// A mapped image: the parts it holds by key and part.
pub(crate) struct Image {
    map: Arc<Mmap>,
    parts: FxHashMap<([u8; KEY], u8), Located>,
    path: PathBuf,
}

fn le32(bytes: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(bytes.get(at..at + 4)?.try_into().ok()?))
}

fn le64(bytes: &[u8], at: usize) -> Option<u64> {
    Some(u64::from_le_bytes(bytes.get(at..at + 8)?.try_into().ok()?))
}

fn root_of(name: &[u8], count: usize, index: &[u8]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(name);
    hasher.update((count as u64).to_le_bytes());
    hasher.update(index);
    hasher.finalize().into()
}

/// Maps the image `RA_JIT_FRONT_IMAGE` names, when the file is there and is one this build
/// accepts. Says why when it is there and is not.
pub(crate) fn open() -> Option<Image> {
    let path = path()?;
    let file = File::open(&path).ok()?;
    match Image::read(&path, &file) {
        Ok(it) => Some(it),
        Err(why) => {
            eprintln!("front-end image: {} not used: {why}", path.display());
            None
        }
    }
}

impl Image {
    fn read(path: &Path, file: &File) -> Result<Image, String> {
        let name = ra_jit_store::build_name().ok_or("this build has no name yet")?;
        // Safety: the file is replaced by renaming another over it and never written in place.
        let map = unsafe { Mmap::map(file) }.map_err(|it| format!("cannot be mapped: {it}"))?;
        let bytes: &[u8] = &map;
        if bytes.len() < HEADER || bytes[..8] != MAGIC {
            return Err("not an image".to_owned());
        }
        let version = le32(bytes, 8).ok_or("no version")?;
        if version != VERSION {
            return Err(format!("version {version}, this reads {VERSION}"));
        }
        if bytes[16..16 + NAME] != *name.as_bytes() {
            return Err("written by another build of ra-jit".to_owned());
        }
        let count = le32(bytes, 12).ok_or("no count")? as usize;
        let (index_at, payload_at, end) = (
            le64(bytes, 64).ok_or("no index")? as usize,
            le64(bytes, 72).ok_or("no payloads")? as usize,
            le64(bytes, 80).ok_or("no end")? as usize,
        );
        if end != bytes.len() {
            return Err(format!("{} bytes where its header says {end}", bytes.len()));
        }
        let index_end = count
            .checked_mul(ENTRY)
            .and_then(|it| it.checked_add(index_at))
            .filter(|&it| index_at >= HEADER && it <= payload_at && payload_at <= end)
            .ok_or("an index outside the file")?;
        let index = &bytes[index_at..index_end];
        if root_of(name.as_bytes(), count, index) != bytes[32..64] {
            return Err("its root is not its index's".to_owned());
        }
        let mut parts = FxHashMap::default();
        parts.reserve(count);
        for entry in index.chunks_exact(ENTRY) {
            let key: [u8; KEY] = entry[..KEY].try_into().map_err(|_| "a short key")?;
            let part =
                u8::try_from(le64(entry, 32).ok_or("no part")?).map_err(|_| "a part > 255")?;
            let at = le64(entry, 40).ok_or("no offset")? as usize;
            let len = le64(entry, 48).ok_or("no length")? as usize;
            let sha: [u8; 32] = entry[56..88].try_into().map_err(|_| "a short hash")?;
            if at < payload_at || at.checked_add(len).is_none_or(|it| it > end) {
                return Err("a part outside the file".to_owned());
            }
            if parts.insert((key, part), Located { at, len, sha }).is_some() {
                return Err("a duplicate part".to_owned());
            }
        }
        let image = Image { map: Arc::new(map), parts, path: path.to_owned() };
        if std::env::var_os("RA_JIT_FRONT_IMAGE_VERIFY").is_some_and(|it| it == "1") {
            image.verify()?;
        }
        Ok(image)
    }

    /// Reads every part and checks it against its hash in the index.
    fn verify(&self) -> Result<(), String> {
        for (&(_, part), at) in &self.parts {
            let sha: [u8; 32] = Sha256::digest(&self.map[at.at..at.at + at.len]).into();
            if sha != at.sha {
                return Err(format!("a part {part} that is not what the index says"));
            }
        }
        Ok(())
    }

    /// Part `part` of the crate whose part key is `key`: a range of the mapping.
    pub(crate) fn get(&self, part: u8, key: &[u8; KEY]) -> Option<Bytes> {
        let at = self.parts.get(&(*key, part))?;
        // The store checks its bytes too. Do not turn a damaged optional image into a
        // different program: refuse the part and let the caller read it from the store.
        let sha: [u8; 32] = Sha256::digest(&self.map[at.at..at.at + at.len]).into();
        if sha != at.sha {
            eprintln!("front-end image: damaged part {part}, reading the store instead");
            return None;
        }
        Bytes::slice_of(self.map.clone(), at.at..at.at + at.len)
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    /// The size of the file.
    pub(crate) fn size(&self) -> usize {
        self.map.len()
    }

    /// How many parts it holds, and their bytes.
    pub(crate) fn totals(&self) -> (usize, u64) {
        (self.parts.len(), self.parts.values().map(|it| it.len as u64).sum())
    }

    /// The pages of the file the kernel has in memory, and all of them (`mincore`): the pages
    /// of a file mapped read-only are clean, so the kernel can drop them under pressure and
    /// read them again.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    pub(crate) fn in_memory(&self) -> Option<(usize, usize)> {
        let page = usize::try_from(unsafe { libc::sysconf(libc::_SC_PAGESIZE) }).ok()?;
        let pages = self.map.len().div_ceil(page);
        let mut present = vec![0u8; pages];
        // Safety: the mapping is `len` bytes from its start and `present` has a byte per page.
        let ok = unsafe {
            libc::mincore(
                self.map.as_ptr().cast::<libc::c_void>().cast_mut(),
                self.map.len(),
                present.as_mut_ptr().cast(),
            )
        };
        (ok == 0).then(|| (present.iter().filter(|it| **it & 1 != 0).count(), pages))
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    pub(crate) fn in_memory(&self) -> Option<(usize, usize)> {
        None
    }
}

/// Says every `every` how much of the image the kernel keeps and what the process holds, in
/// the worker's log: `RA_JIT_FRONT_IMAGE_WATCH` (milliseconds), for watching the image under
/// pressure from another process.
pub(crate) fn watch(image: &Arc<Image>) {
    let Some(every) = std::env::var("RA_JIT_FRONT_IMAGE_WATCH").ok().and_then(|it| it.parse().ok())
    else {
        return;
    };
    if every == 0 {
        return;
    }
    let image = Arc::downgrade(image);
    std::thread::spawn(move || {
        let started = std::time::Instant::now();
        loop {
            std::thread::sleep(Duration::from_millis(every));
            let Some(image) = image.upgrade() else { break };
            let (present, pages) = image.in_memory().unwrap_or((0, 0));
            let held = crate::memory::resident().map(|it| it.to_string()).unwrap_or_default();
            eprintln!(
                "front-end image watch: {:.1}s {present} of {pages} pages in memory, process holds {held} MB",
                started.elapsed().as_secs_f64()
            );
        }
    });
}

fn round_up(n: usize, to: usize) -> usize {
    n.div_ceil(to) * to
}

/// Writes `parts` (part, key, bytes) to `path` and returns the size of the file.
pub(crate) fn write(path: &Path, mut parts: Vec<(u8, [u8; KEY], Vec<u8>)>) -> io::Result<usize> {
    let name =
        ra_jit_store::build_name().ok_or_else(|| io::Error::other("this build has no name yet"))?;
    if name.len() != NAME {
        return Err(io::Error::other("this build's name is not 16 digits"));
    }
    parts.sort_unstable_by(|a, b| (a.1, a.0).cmp(&(b.1, b.0)));
    for adjacent in parts.windows(2) {
        if adjacent[0].0 == adjacent[1].0
            && adjacent[0].1 == adjacent[1].1
            && adjacent[0].2 != adjacent[1].2
        {
            return Err(io::Error::other("different bytes for the same part key"));
        }
    }
    parts.dedup_by(|a, b| a.0 == b.0 && a.1 == b.1);
    let count = parts.len();
    let index_at = PAGE;
    let payload_at = index_at + round_up(count * ENTRY, PAGE);
    let mut index = Vec::with_capacity(count * ENTRY);
    let mut at = payload_at;
    for (part, key, bytes) in &parts {
        index.extend_from_slice(key);
        index.extend_from_slice(&u64::from(*part).to_le_bytes());
        index.extend_from_slice(&(at as u64).to_le_bytes());
        index.extend_from_slice(&(bytes.len() as u64).to_le_bytes());
        index.extend_from_slice(&Sha256::digest(bytes));
        at = round_up(at + bytes.len(), ALIGN);
    }
    let end = at;
    let mut header = Vec::with_capacity(HEADER);
    header.extend_from_slice(&MAGIC);
    header.extend_from_slice(&VERSION.to_le_bytes());
    header.extend_from_slice(&(count as u32).to_le_bytes());
    header.extend_from_slice(name.as_bytes());
    header.extend_from_slice(&root_of(name.as_bytes(), count, &index));
    for it in [index_at, payload_at, end] {
        header.extend_from_slice(&(it as u64).to_le_bytes());
    }
    debug_assert_eq!(header.len(), HEADER);

    let temporary = path.with_extension(format!("{}.tmp", std::process::id()));
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut out = io::BufWriter::with_capacity(1 << 20, File::create(&temporary)?);
    let zeros = [0u8; PAGE];
    out.write_all(&header)?;
    out.write_all(&zeros[..index_at - HEADER])?;
    out.write_all(&index)?;
    out.write_all(&zeros[..payload_at - index_at - index.len()])?;
    let mut at = payload_at;
    for (_, _, bytes) in &parts {
        out.write_all(bytes)?;
        let padded = round_up(at + bytes.len(), ALIGN);
        out.write_all(&zeros[..padded - at - bytes.len()])?;
        at = padded;
    }
    out.flush()?;
    drop(out);
    std::fs::rename(&temporary, path)?;
    Ok(end)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parts() -> Vec<(u8, [u8; KEY], Vec<u8>)> {
        vec![
            (0, [1; KEY], b"a def map".to_vec()),
            (1, [1; KEY], vec![7; 100_000]),
            (0, [2; KEY], b"another crate's".to_vec()),
        ]
    }

    fn written(dir: &Path) -> PathBuf {
        ra_jit_store::compiles_against("the front-end image's tests");
        let path = dir.join("front.image");
        write(&path, parts()).unwrap();
        path
    }

    fn read(path: &Path) -> Result<Image, String> {
        Image::read(path, &File::open(path).unwrap())
    }

    #[test]
    fn a_written_image_is_mapped_and_each_part_read_back() {
        let dir = tempfile::tempdir().unwrap();
        let path = written(dir.path());
        let image = read(&path).unwrap();
        assert_eq!(image.size() as u64, std::fs::metadata(&path).unwrap().len());
        assert_eq!(image.totals(), (3, 9 + 100_000 + 15));
        for (part, key, bytes) in parts() {
            assert_eq!(&*image.get(part, &key).unwrap(), &bytes[..]);
        }
        assert!(image.get(2, &[1; KEY]).is_none());
        assert!(image.get(0, &[3; KEY]).is_none());
        // Every part starts aligned, and the payloads start on a page.
        assert!(image.parts.values().all(|it| it.at % ALIGN == 0 && it.at >= 2 * PAGE));
        image.verify().unwrap();
    }

    #[test]
    fn the_same_parts_make_the_same_file() {
        let dir = tempfile::tempdir().unwrap();
        let first = std::fs::read(written(dir.path())).unwrap();
        let mut backwards = parts();
        backwards.reverse();
        write(&dir.path().join("again.image"), backwards).unwrap();
        assert_eq!(first, std::fs::read(dir.path().join("again.image")).unwrap());
    }

    #[test]
    fn a_slice_keeps_the_old_mapping_alive_after_replacement() {
        let dir = tempfile::tempdir().unwrap();
        let path = written(dir.path());
        let image = read(&path).unwrap();
        let bytes = image.get(0, &[1; KEY]).unwrap();
        drop(image);
        write(&path, vec![(0, [1; KEY], b"replacement".to_vec())]).unwrap();
        assert_eq!(&*bytes, b"a def map");
        assert_eq!(&*read(&path).unwrap().get(0, &[1; KEY]).unwrap(), b"replacement");
    }

    #[test]
    fn duplicate_keys_are_deduplicated_or_refused() {
        let dir = tempfile::tempdir().unwrap();
        ra_jit_store::compiles_against("the front-end image's tests");
        let path = dir.path().join("front.image");
        let mut duplicate = parts();
        duplicate.push(duplicate[0].clone());
        write(&path, duplicate.clone()).unwrap();
        assert_eq!(read(&path).unwrap().totals().0, 3);
        duplicate.last_mut().unwrap().2.push(0);
        assert!(write(&path, duplicate).is_err());
        assert_eq!(read(&path).unwrap().totals().0, 3);
    }

    #[test]
    fn an_image_that_was_changed_is_refused_or_fails_its_check() {
        let dir = tempfile::tempdir().unwrap();
        let path = written(dir.path());
        let good = std::fs::read(&path).unwrap();
        let refuse = |bytes: Vec<u8>| {
            std::fs::write(&path, bytes).unwrap();
            read(&path).err()
        };
        let at = |from: usize, with: u8| {
            let mut bytes = good.clone();
            bytes[from] ^= with;
            bytes
        };
        assert!(
            refuse(good[..good.len() - 1].to_vec()).is_some_and(|it| it.contains("bytes where"))
        );
        assert!(refuse(at(0, 1)).is_some_and(|it| it.contains("not an image")));
        assert!(refuse(at(8, 1)).is_some_and(|it| it.contains("version")));
        assert!(refuse(at(16, 1)).is_some_and(|it| it.contains("another build")));
        // A byte of the index: the root is not its index's.
        assert!(refuse(at(PAGE + 3, 1)).is_some_and(|it| it.contains("root")));
        // A byte of a part is not in the root, which holds the hash of the part; only a
        // check of the parts finds it.
        let image = {
            std::fs::write(&path, at(2 * PAGE + 1, 1)).unwrap();
            read(&path).unwrap()
        };
        assert!(image.verify().is_err());
        assert!(image.get(0, &[1; KEY]).is_none());
        assert!(image.get(1, &[1; KEY]).is_some());
        drop(image);
        std::fs::write(&path, &good).unwrap();
        assert!(read(&path).is_ok());
    }

    #[test]
    fn malformed_ranges_and_duplicate_index_entries_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = written(dir.path());
        let good = std::fs::read(&path).unwrap();
        let name = ra_jit_store::build_name().unwrap();
        for (field, value) in [(40, u64::MAX), (48, u64::MAX)] {
            let mut bytes = good.clone();
            bytes[PAGE + field..PAGE + field + 8].copy_from_slice(&value.to_le_bytes());
            let root = root_of(name.as_bytes(), 3, &bytes[PAGE..PAGE + 3 * ENTRY]);
            bytes[32..64].copy_from_slice(&root);
            std::fs::write(&path, &bytes).unwrap();
            assert!(read(&path).err().unwrap().contains("outside"));
        }
        let mut bytes = good.clone();
        bytes.copy_within(PAGE..PAGE + ENTRY, PAGE + ENTRY);
        let root = root_of(name.as_bytes(), 3, &bytes[PAGE..PAGE + 3 * ENTRY]);
        bytes[32..64].copy_from_slice(&root);
        std::fs::write(&path, &bytes).unwrap();
        assert!(read(&path).err().unwrap().contains("duplicate"));
        for len in [0, 7, HEADER - 1, HEADER] {
            std::fs::write(&path, &good[..len]).unwrap();
            assert!(read(&path).is_err());
        }
    }

    /// The mapping is of a file opened for reading: it cannot be made writable, so a write
    /// through it is a fault and not a change of the file other workers map.
    #[cfg(unix)]
    #[test]
    fn the_mapping_cannot_be_written_through() {
        let dir = tempfile::tempdir().unwrap();
        let image = read(&written(dir.path())).unwrap();
        let (start, len) = (image.map.as_ptr() as *mut libc::c_void, image.map.len());
        // Safety: asks the kernel to change the protection of this mapping, and expects it to
        // refuse.
        let refused = unsafe { libc::mprotect(start, len, libc::PROT_READ | libc::PROT_WRITE) };
        assert_eq!(refused, -1);
        assert_eq!(std::io::Error::last_os_error().raw_os_error(), Some(libc::EACCES));
    }
}
