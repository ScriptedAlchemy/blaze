//! Installed tool inputs are verified by their complete tree, including negative lookup inputs.
use std::{fs, io::{self, Read}, path::Path};

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

fn text(digest: &mut Sha256, text: &str) {
    digest.update(&(text.len() as u64).to_le_bytes()).update(text.as_bytes());
}

impl Tree<'_> {
    fn walk(&mut self, root: &Path, dir: &Path, depth: usize) -> io::Result<()> {
        if depth > MOST_DEPTH {
            return Err(invalid("native tool tree exceeds 128 directories of depth"));
        }
        let mut entries: AVec<'_, (std::string::String, std::path::PathBuf)> = Vec::new_in(self.alloc);
        for entry in fs::read_dir(dir)? {
            self.entries += 1;
            if self.entries > MOST_ENTRIES {
                return Err(invalid("native tool tree exceeds 200000 entries"));
            }
            let entry = entry?;
            let name = entry.file_name().into_string().map_err(|_| invalid("native tool tree path is not UTF-8"))?;
            entries.push((name, entry.path()));
        }
        entries.sort_unstable_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));
        for (_, path) in entries {
            let relative = path.strip_prefix(root).map_err(|_| invalid("native tool path escapes its root"))?;
            let relative = relative.to_str().ok_or_else(|| invalid("native tool tree path is not UTF-8"))?;
            let meta = fs::symlink_metadata(&path)?;
            let kind = meta.file_type();
            let tag = if kind.is_dir() { b'd' } else if kind.is_file() { b'f' } else if kind.is_symlink() { b'l' } else {
                return Err(invalid("native tool tree contains a special file"));
            };
            self.digest.update(&[tag]);
            text(&mut self.digest, relative);
            if kind.is_dir() {
                self.walk(root, &path, depth + 1)?;
            } else if kind.is_symlink() {
                let target = fs::read_link(&path)?;
                let target = target.to_str().ok_or_else(|| invalid("native tool link is not UTF-8"))?;
                if !fs::canonicalize(&path)?.starts_with(root) {
                    return Err(invalid("native tool symlink escapes its verified tree"));
                }
                text(&mut self.digest, target);
            } else {
                self.bytes = self.bytes.checked_add(meta.len()).ok_or_else(|| invalid("native tool tree size overflow"))?;
                if self.bytes > MOST_BYTES {
                    return Err(invalid("native tool tree exceeds 16 GiB"));
                }
                let mut file = fs::File::open(&path)?;
                let mut digest = Sha256::new();
                let mut left = meta.len();
                let mut buffer = [0u8; 64 << 10];
                while left > 0 {
                    let count = usize::try_from(left.min(buffer.len() as u64)).expect("buffer length");
                    file.read_exact(&mut buffer[..count])?;
                    digest.update(&buffer[..count]);
                    left -= count as u64;
                }
                if file.read(&mut buffer[..1])? != 0 {
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
    tree.walk(&root, &root, 0)?;
    Ok(tree.digest.finish())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_input_and_negative_lookup_changes_the_tree_identity() {
        let root = std::env::temp_dir().join(format!("blaze-native-tree-{}", std::process::id()));
        fs::create_dir(&root).expect("fresh test tree");
        fs::create_dir(root.join("include")).expect("include directory");
        let header = root.join("include/a.h");
        fs::write(&header, b"#define VALUE 1\n").expect("header");
        let before = digest(&root, Alloc::global()).expect("verified tree");
        assert_eq!(before, [134, 21, 158, 12, 123, 72, 6, 120, 74, 106, 40, 222, 228, 178, 250, 181, 64, 206, 31, 108, 178, 220, 61, 198, 196, 55, 120, 92, 79, 100, 75, 25], "installer/runtime v1 encoding vector");
        fs::write(&header, b"#define VALUE 2\n").expect("changed header");
        assert_ne!(before, digest(&root, Alloc::global()).expect("changed tree"));
        fs::write(&header, b"#define VALUE 1\n").expect("restored header");
        assert_eq!(before, digest(&root, Alloc::global()).expect("restored tree"));
        fs::write(root.join("include/optional.h"), b"").expect("new optional header");
        assert_ne!(before, digest(&root, Alloc::global()).expect("negative lookup changed"));
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink("/", root.join("outside")).expect("escaping link");
            assert!(digest(&root, Alloc::global()).is_err(), "a tree cannot grant bytes outside its identity");
        }
        fs::remove_dir_all(root).expect("remove test tree");
    }
}
