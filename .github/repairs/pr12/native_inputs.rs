//! Native outputs of build scripts are inputs to a link, not just to Rust name resolution.
//!
//! Keep the last successfully linked identity, never the last observed one. Generated
//! archives may change without changing a directive, Rust source, size, or timestamp.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::{self, Read},
    path::{Path, PathBuf},
    sync::{LazyLock, Mutex},
};

use anyhow::{Context as _, bail};
use sha2::{Digest as _, Sha256};

use crate::{Args, Program, RootDatabase};

type Digest = [u8; 32];
static LINKED_INPUTS: LazyLock<Mutex<BTreeMap<PathBuf, Digest>>> =
    LazyLock::new(|| Mutex::new(BTreeMap::new()));

#[derive(Default)]
struct Inputs {
    directives: Vec<Vec<u8>>,
    paths: BTreeSet<PathBuf>,
}

/// A link's proposed identity. Dropping it after a failed build publishes nothing.
pub(crate) struct Pending {
    binary: PathBuf,
    inputs: Inputs,
    before: Option<Digest>,
    changed: bool,
}

impl Pending {
    /// An untrackable input prevents reuse, but does not refuse a build the linker accepts.
    pub(crate) fn changed(&self) -> bool {
        self.changed
    }

    pub(crate) fn commit(self) -> anyhow::Result<()> {
        let Some(before) = self.before else { return Ok(()) };
        if self.inputs.digest().ok() != Some(before) {
            bail!("native link inputs changed while the executable was being linked");
        }
        crate::lock(&LINKED_INPUTS).insert(self.binary, before);
        Ok(())
    }
}

fn collect(db: &RootDatabase, args: &Args) -> anyhow::Result<Option<(PathBuf, Inputs)>> {
    if args.exe.is_none() {
        return Ok(None);
    }
    let Some(binary) = args.splice.as_ref().or(args.exe.as_ref()) else { return Ok(None) };
    let mut roots = Vec::new();
    for name in args.crates() {
        roots.extend(crate::crates_named(db, name, args)?);
    }
    let mut out_dirs = BTreeSet::new();
    for &root in &roots {
        let root: ide_db::base_db::Crate = root.into();
        for krate in std::iter::once(root).chain(root.transitive_deps(db)) {
            if krate.data(db).origin.is_lang() {
                continue;
            }
            if let Some(out) = krate.env(db).get("OUT_DIR") {
                out_dirs.insert(PathBuf::from(out));
            }
        }
    }
    let cwd = std::env::current_dir()?;
    let mut inputs = Inputs::default();
    for out in out_dirs {
        let Some(printed) = crate::cargo::script::printed(&out) else { continue };
        inputs.paths.insert(printed.clone());
        let text = match fs::read_to_string(&printed) {
            Ok(text) => text,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error).with_context(|| format!("reading {}", printed.display())),
        };
        let mut native = false;
        for line in text.lines() {
            let Some(line) = line.strip_prefix("cargo:") else { continue };
            let line = line.strip_prefix(':').unwrap_or(line);
            let Some((key, value)) = line.split_once('=') else { continue };
            if !key.starts_with("rustc-link-") && key != "rustc-flags" {
                continue;
            }
            native = true;
            inputs.directives.push(line.as_bytes().to_vec());
            if key == "rustc-link-search" {
                let path = match value.split_once('=') {
                    Some(("native" | "dependency" | "crate" | "framework" | "all", path)) => path,
                    _ => value,
                };
                inputs.paths.insert(cwd.join(path));
            } else if key.starts_with("rustc-link-arg") {
                // Include explicit files and response/linker scripts in addition to OUT_DIR.
                for word in value.trim_start_matches("-Wl,").split(',') {
                    let word = word.split_once('=').map_or(word, |(_, value)| value);
                    let word = word.strip_prefix('@').unwrap_or(word);
                    if !word.is_empty() && !word.starts_with('-') {
                        let path = cwd.join(word);
                        if path.is_file()
                            || path.extension().is_some_and(|ext| {
                                matches!(ext.to_str(), Some("a" | "o" | "so" | "dylib" | "lib" | "obj" | "ld" | "rsp"))
                            })
                        {
                            inputs.paths.insert(path);
                        }
                    }
                }
            } else if key == "rustc-flags" {
                let mut words = value.split_whitespace();
                while let Some(word) = words.next() {
                    if let Some(path) = word.strip_prefix("-L") {
                        let path = if path.is_empty() { words.next().unwrap_or("") } else { path };
                        let path = match path.split_once('=') {
                            Some(("native" | "dependency" | "crate" | "framework" | "all", path)) => path,
                            _ => path,
                        };
                        if !path.is_empty() {
                            inputs.paths.insert(cwd.join(path));
                        }
                    }
                }
            }
        }
        if native {
            inputs.paths.insert(out);
        }
    }
    // Include the effective native link order and flags, not merely a sorted set of packages.
    // Empty object inputs avoid tracking the Rust objects this build itself is about to write.
    let sysroot = crate::SYSROOT.get().and_then(Option::as_deref);
    inputs.directives.push(crate::link::command(db, &roots, &[], sysroot, binary)?.into_bytes());
    // Keep an explicit identity even when the last native script was removed.
    Ok(Some((binary.clone(), inputs)))
}

pub(crate) fn stale(db: &RootDatabase, programs: &[Program]) -> anyhow::Result<bool> {
    for program in programs {
        if let Some((binary, inputs)) = collect(db, &program.args)? {
            let now = inputs.digest().ok();
            if now.is_none() || crate::lock(&LINKED_INPUTS).get(&binary).copied() != now {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

pub(crate) fn invalidate(db: &RootDatabase, programs: &mut [Program]) -> anyhow::Result<()> {
    for program in programs {
        let Some((binary, inputs)) = collect(db, &program.args)? else { continue };
        let now = inputs.digest().ok();
        if now.is_some() && crate::lock(&LINKED_INPUTS).get(&binary).copied() == now {
            continue;
        }
        // The old identity must not become reusable after a failed A -> B -> A relink.
        crate::lock(&LINKED_INPUTS).remove(&binary);
        forget_binary(&binary);
        program.known.forget_base();
        program.whole = None;
        program.moved = true;
        program.unbuilt.stale = true;
        program.unbuilt.unlinked = true;
        program.args.link_cmd = None;
    }
    Ok(())
}

/// Prevent a native change from being installed as a Rust-only splice.
pub(crate) fn forget_binary(binary: &Path) {
    crate::splicer::claim(binary, true);
    crate::splicer::forget(binary);
    crate::lock(&crate::LINKED).remove(binary);
}

pub(crate) fn begin(db: &RootDatabase, args: &Args) -> anyhow::Result<Option<Pending>> {
    Ok(collect(db, args)?.map(|(binary, inputs)| pending(binary, inputs)))
}

fn pending(binary: PathBuf, inputs: Inputs) -> Pending {
    let before = inputs.digest().ok();
    // A failed attempt cannot leave an old identity available for A -> B -> A reuse.
    let previous = crate::lock(&LINKED_INPUTS).remove(&binary);
    let changed = before.is_none() || previous != before;
    Pending { binary, inputs, before, changed }
}

impl Inputs {
    fn digest(&self) -> io::Result<Digest> {
        let mut hash = Sha256::new();
        frame(&mut hash, b"blaze/native-link-inputs/v1");
        frame(&mut hash, &(self.directives.len() as u64).to_le_bytes());
        for line in &self.directives {
            frame(&mut hash, line);
        }
        frame(&mut hash, &(self.paths.len() as u64).to_le_bytes());
        for path in &self.paths {
            frame(&mut hash, path.as_os_str().as_encoded_bytes());
            hash_path(path, &mut hash, &mut BTreeSet::new())?;
        }
        Ok(hash.finalize().into())
    }
}

fn frame(hash: &mut Sha256, bytes: &[u8]) {
    hash.update((bytes.len() as u64).to_le_bytes());
    hash.update(bytes);
}

fn hash_path(path: &Path, hash: &mut Sha256, active: &mut BTreeSet<PathBuf>) -> io::Result<()> {
    let meta = match fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            frame(hash, b"missing");
            return Ok(());
        }
        Err(error) => return Err(error),
    };
    if meta.file_type().is_symlink() {
        frame(hash, b"symlink");
        let target = fs::read_link(path)?;
        frame(hash, target.as_os_str().as_encoded_bytes());
        // canonicalize diagnoses cycles; dangling links retain both their target and absence.
        match path.canonicalize() {
            Ok(target) => return hash_path(&target, hash, active),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                frame(hash, b"missing-target");
                return Ok(());
            }
            Err(error) => return Err(error),
        }
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        frame(hash, &(meta.permissions().mode() & 0o7777).to_le_bytes());
    }
    if meta.is_file() {
        frame(hash, b"file");
        let mut file_hash = Sha256::new();
        let mut file = fs::File::open(path)?;
        let mut buffer = [0; 64 * 1024];
        loop {
            let read = file.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            file_hash.update(&buffer[..read]);
        }
        frame(hash, &file_hash.finalize());
        return Ok(());
    }
    if !meta.is_dir() {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "native input is not a regular file or directory"));
    }
    frame(hash, b"directory");
    let real = path.canonicalize()?;
    if !active.insert(real.clone()) {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "native input directory cycle"));
    }
    let mut entries = fs::read_dir(path)?.collect::<io::Result<Vec<_>>>()?;
    entries.sort_by_key(|entry| entry.file_name());
    frame(hash, &(entries.len() as u64).to_le_bytes());
    for entry in entries {
        frame(hash, entry.file_name().as_encoded_bytes());
        hash_path(&entry.path(), hash, active)?;
    }
    active.remove(&real);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inputs(path: &Path) -> Inputs {
        Inputs { directives: vec![b"rustc-link-lib=static=generated".to_vec()], paths: BTreeSet::from([path.to_owned()]) }
    }

    #[test]
    fn same_size_and_timestamp_different_archive_bytes_are_not_reused() {
        let dir = tempfile::tempdir().unwrap();
        let archive = dir.path().join("libgenerated.a");
        fs::write(&archive, b"first").unwrap();
        let stamp = fs::metadata(&archive).unwrap().modified().unwrap();
        let before = inputs(dir.path()).digest().unwrap();
        fs::write(&archive, b"other").unwrap();
        fs::File::options().write(true).open(&archive).unwrap().set_times(fs::FileTimes::new().set_modified(stamp)).unwrap();
        assert_ne!(inputs(dir.path()).digest().unwrap(), before);
        fs::write(&archive, b"first").unwrap();
        assert_eq!(inputs(dir.path()).digest().unwrap(), before);
    }

    #[test]
    fn deletion_recreation_and_directive_only_changes_invalidate() {
        let dir = tempfile::tempdir().unwrap();
        let archive = dir.path().join("native.a");
        let absent = inputs(&archive).digest().unwrap();
        fs::write(&archive, b"native").unwrap();
        let present = inputs(&archive).digest().unwrap();
        assert_ne!(absent, present);
        fs::remove_file(&archive).unwrap();
        assert_eq!(inputs(&archive).digest().unwrap(), absent);
        fs::write(&archive, b"native").unwrap();
        assert_eq!(inputs(&archive).digest().unwrap(), present);
        let mut changed = inputs(&archive);
        changed.directives.push(b"rustc-link-arg=-Wl,new-option".to_vec());
        assert_ne!(changed.digest().unwrap(), present);
    }

    #[test]
    fn failed_or_changed_links_do_not_commit_an_identity() {
        let dir = tempfile::tempdir().unwrap();
        let archive = dir.path().join("native.a");
        let binary = dir.path().join("program");
        fs::write(&archive, b"A").unwrap();
        let a = inputs(&archive).digest().unwrap();
        crate::lock(&LINKED_INPUTS).insert(binary.clone(), a);
        let failed = pending(binary.clone(), inputs(&archive));
        assert!(!failed.changed());
        drop(failed);
        assert!(!crate::lock(&LINKED_INPUTS).contains_key(&binary));
        let changed = pending(binary.clone(), inputs(&archive));
        assert!(changed.changed());
        fs::write(&archive, b"B").unwrap();
        assert!(changed.commit().is_err());
        assert!(!crate::lock(&LINKED_INPUTS).contains_key(&binary));
        fs::write(&archive, b"A").unwrap();
        let restored = pending(binary.clone(), inputs(&archive));
        assert!(restored.changed());
        assert!(!crate::lock(&LINKED_INPUTS).contains_key(&binary));
        restored.commit().unwrap();
        assert_eq!(crate::lock(&LINKED_INPUTS).remove(&binary), Some(a));
    }

    #[cfg(unix)]
    #[test]
    fn symlink_targets_and_cycles_are_not_silently_ignored() {
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a"), b"same").unwrap();
        fs::write(dir.path().join("b"), b"same").unwrap();
        let link = dir.path().join("link");
        symlink("a", &link).unwrap();
        let before = inputs(&link).digest().unwrap();
        fs::remove_file(&link).unwrap();
        symlink("b", &link).unwrap();
        assert_ne!(inputs(&link).digest().unwrap(), before);
        fs::remove_file(&link).unwrap();
        symlink(".", &link).unwrap();
        assert!(inputs(dir.path()).digest().is_err());
    }
}
