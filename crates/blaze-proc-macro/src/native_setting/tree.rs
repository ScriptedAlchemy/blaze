//! Installed tool inputs are verified by their complete tree, including negative lookup inputs.
use std::{
    ffi::OsStr,
    fs,
    io::{self, Read},
    path::{Path, PathBuf},
};

use blaze_sha256::Sha256;
use blaze_own::{AVec, Alloc, Vec};

const MOST_ENTRIES: usize = 200_000;
const MOST_BYTES: u64 = 16 << 30;
const MOST_DEPTH: usize = 128;

fn invalid(words: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, words)
}

struct Tree<'a> {
    digest: Sha256,
    entries: usize,
    bytes: u64,
    alloc: Alloc<'a>,
}

fn text(digest: &mut Sha256, text: &str) -> io::Result<()> {
    let len = u64::try_from(text.len()).map_err(|_| invalid("native tool path is too long"))?;
    digest.update(&len.to_le_bytes()).update(text.as_bytes());
    Ok(())
}

impl<'a> Tree<'a> {
    fn children(&mut self, stack: &mut AVec<'a, (PathBuf, usize)>, dir: &Path, depth: usize) -> io::Result<()> {
        if depth > MOST_DEPTH {
            return Err(invalid("native tool tree exceeds 128 directories of depth"));
        }
        let first = stack.len();
        for entry in fs::read_dir(dir)? {
            self.entries += 1;
            if self.entries > MOST_ENTRIES {
                return Err(invalid("native tool tree exceeds 200000 entries"));
            }
            stack.push((entry?.path(), depth));
        }
        let children = stack.get_mut(first..).ok_or_else(|| invalid("native tool directory worklist moved while listing it"))?;
        // Reverse the byte order so popping the stack visits each directory in v1 order.
        children.sort_unstable_by(|(left, _), (right, _)| {
            right.file_name().map(OsStr::as_encoded_bytes).cmp(&left.file_name().map(OsStr::as_encoded_bytes))
        });
        Ok(())
    }

    fn walk(&mut self, root: &Path) -> io::Result<()> {
        let mut stack = Vec::new_in(self.alloc);
        self.children(&mut stack, root, 0)?;
        while let Some((path, depth)) = stack.pop() {
            let relative = path.strip_prefix(root).map_err(|_| invalid("native tool path escapes its root"))?;
            let relative = relative.to_str().ok_or_else(|| invalid("native tool tree path is not UTF-8"))?;
            let meta = fs::symlink_metadata(&path)?;
            let kind = meta.file_type();
            let tag = if kind.is_dir() { b'd' } else if kind.is_file() { b'f' } else if kind.is_symlink() { b'l' } else {
                return Err(invalid("native tool tree contains a special file"));
            };
            self.digest.update(&[tag]);
            text(&mut self.digest, relative)?;
            if kind.is_dir() {
                self.children(&mut stack, &path, depth + 1)?;
            } else if kind.is_symlink() {
                let target = fs::read_link(&path)?;
                let target = target.to_str().ok_or_else(|| invalid("native tool link is not UTF-8"))?;
                if !fs::canonicalize(&path)?.starts_with(root) {
                    return Err(invalid("native tool symlink escapes its verified tree"));
                }
                text(&mut self.digest, target)?;
            } else {
                self.bytes = self.bytes.checked_add(meta.len()).ok_or_else(|| invalid("native tool tree size overflow"))?;
                if self.bytes > MOST_BYTES {
                    return Err(invalid("native tool tree exceeds 16 GiB"));
                }
                let mut file = fs::File::open(&path)?;
                let mut digest = Sha256::new();
                let mut left = meta.len();
                let mut buffer = [0u8; 64 << 10];
                let limit = u64::try_from(buffer.len()).map_err(|_| invalid("native tool read buffer is too large"))?;
                while left > 0 {
                    let bytes = left.min(limit);
                    let count = usize::try_from(bytes).map_err(|_| invalid("native tool read block is too large"))?;
                    let block = buffer.get_mut(..count).ok_or_else(|| invalid("native tool read exceeds its buffer"))?;
                    file.read_exact(block)?;
                    digest.update(block);
                    left -= bytes;
                }
                if file.read(&mut [0])? != 0 {
                    return Err(invalid("native tool file changed while being verified"));
                }
                self.digest.update(&meta.len().to_le_bytes()).update(&digest.finish());
            }
        }
        Ok(())
    }
}

/// v1: SHA256(seed, per-directory UTF8 byte-sorted preorder entries). Each entry is its
/// d/f/l byte, u64LE path length, relative UTF8 path; files add u64LE length and SHA256(bytes),
/// links add u64LE target length and UTF8 link target. The root itself is excluded.
pub(super) fn digest(root: &Path, alloc: Alloc<'_>) -> io::Result<[u8; 32]> {
    let root = fs::canonicalize(root)?;
    let mut tree = Tree { digest: Sha256::new(), entries: 0, bytes: 0, alloc };
    tree.digest.update(b"blaze-native-tree-v1\0");
    tree.walk(&root)?;
    Ok(tree.digest.finish())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_input_and_negative_lookup_changes_the_tree_identity() {
        let alloc = blaze_own::testing::heap().alloc();
        let root = blaze_core::temp::TempDir::for_test("blaze-native-tree-");
        fs::create_dir(root.join("include")).expect("include directory");
        let header = root.join("include/a.h");
        fs::write(&header, b"#define VALUE 1\n").expect("header");
        let before = digest(root.path(), alloc).expect("verified tree");
        assert_eq!(before, [134, 21, 158, 12, 123, 72, 6, 120, 74, 106, 40, 222, 228, 178, 250, 181, 64, 206, 31, 108, 178, 220, 61, 198, 196, 55, 120, 92, 79, 100, 75, 25], "installer/runtime v1 encoding vector");
        fs::write(&header, b"#define VALUE 2\n").expect("changed header");
        assert_ne!(before, digest(root.path(), alloc).expect("changed tree"));
        fs::write(&header, b"#define VALUE 1\n").expect("restored header");
        assert_eq!(before, digest(root.path(), alloc).expect("restored tree"));
        fs::write(root.join("include/optional.h"), b"").expect("new optional header");
        assert_ne!(before, digest(root.path(), alloc).expect("negative lookup changed"));
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink("/", root.join("outside")).expect("escaping link");
            assert!(digest(root.path(), alloc).is_err(), "a tree cannot grant bytes outside its identity");
        }
    }
}
