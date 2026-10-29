//! Immutable source inputs. The manifest is deterministic CBOR; file blobs and the
//! manifest are addressed by BLAKE3, independently of filesystem timestamps.
//!
//! A capture retains dirty, ignored and untracked files, empty directories and Unix
//! executable permissions. The blob store, explicitly supplied output directories and Git's
//! administration directory are omitted. A source link retains the files it refers to even
//! inside an output directory. Materialization uses copies, never hard links
//! to mutable requester files or to the blob store.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context as _, bail, ensure};
use blaze_wire::{Digest, SourcePath, Wire};

/// A file, directory or symlink: logical path, kind, executable bits, blob digest.
/// Symlink blobs contain native target bytes, not lossy text.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Entry(SourcePath, u8, u32, Digest);

impl Wire for Entry {
    fn encode(&self, e: &mut blaze_wire::Encoder) {
        e.array(4);
        self.0.encode(e);
        self.1.encode(e);
        self.2.encode(e);
        self.3.encode(e);
    }

    fn decode(r: &mut blaze_wire::Reader<'_>) -> Result<Self, blaze_wire::Error> {
        r.array_start(4)?;
        let entry =
            Self(SourcePath::decode(r)?, u8::decode(r)?, u32::decode(r)?, Digest::decode(r)?);
        r.array_end()?;
        Ok(entry)
    }
}

/// The immutable manifest of a source tree.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Snapshot {
    entries: Vec<Entry>,
}

impl Wire for Snapshot {
    fn encode(&self, e: &mut blaze_wire::Encoder) {
        e.map(2);
        e.uint(0);
        e.uint(1); // snapshot format, independent of the transport codec version
        e.uint(1);
        self.entries.encode(e);
    }

    fn decode(r: &mut blaze_wire::Reader<'_>) -> Result<Self, blaze_wire::Error> {
        r.map_start()?;
        r.field(0)?;
        let at = r.position();
        if u32::decode(r)? != 1 {
            return Err(blaze_wire::Error {
                kind: blaze_wire::ErrorKind::UnexpectedType {
                    expected: "source snapshot version 1",
                },
                at,
            });
        }
        r.field(1)?;
        let entries = Vec::<Entry>::decode(r)?;
        r.map_end()?;
        let snapshot = Self { entries };
        snapshot.check().map_err(|_| blaze_wire::Error {
            kind: blaze_wire::ErrorKind::UnexpectedType { expected: "valid source snapshot" },
            at,
        })?;
        Ok(snapshot)
    }
}

impl Snapshot {
    /// Capture `root`, excluding only `outputs`. Structural inputs read while resolving
    /// are compared against the captured bytes, so a racing manifest cannot certify a
    /// different graph. The caller must supply every read, including repeated reads.
    pub fn capture(
        root: &Path,
        outputs: &[PathBuf],
        structural: &[(PathBuf, Vec<u8>)],
        store: &Path,
    ) -> anyhow::Result<Self> {
        Self::capture_selected(root, &[root.to_owned()], outputs, structural, store)
    }

    /// Capture selected source trees and structural files in their original relative
    /// topology. The common ancestor is a layout root, not a directory to walk.
    pub fn capture_selected(
        root: &Path,
        trees: &[PathBuf],
        outputs: &[PathBuf],
        structural: &[(PathBuf, Vec<u8>)],
        store: &Path,
    ) -> anyhow::Result<Self> {
        let cwd = std::env::current_dir()?;
        let given_root = blaze_manifest::paths::normalize_path(&cwd.join(root));
        let root = given_root.canonicalize()?;
        fs::create_dir_all(store)?;
        let store_root = store.canonicalize()?;
        ensure!(root != store_root, "source root is the snapshot blob store");
        let mut excluded =
            vec![blaze_manifest::paths::normalize_path(&cwd.join(store)), store_root.clone()];
        for path in outputs {
            let path = blaze_manifest::paths::normalize_path(&cwd.join(path));
            if let Ok(canonical) = path.canonicalize() {
                excluded.push(canonical);
            }
            excluded.push(path);
        }
        excluded.sort();
        excluded.dedup();
        let mut capture = Capture {
            root,
            given_root,
            outputs: excluded,
            store,
            store_root,
            entries: BTreeMap::new(),
            expanded: BTreeSet::new(),
            links: BTreeMap::new(),
        };
        for tree in trees {
            let relative = capture.relative(tree)?;
            capture.path(&relative, true)?;
        }
        for (path, bytes) in structural {
            let relative = capture.relative(path)?;
            let resolved = capture.path(&relative, false)?;
            let logical = SourcePath::from_native(&resolved)?;
            let entry = capture.entries.get(&logical);
            ensure!(
                entry.is_some_and(|entry| entry.1 == 0 && entry.3 == hash(bytes)),
                "input {} changed during resolution; retry the capture",
                path.display()
            );
        }
        let snapshot = Self { entries: capture.entries.into_values().collect() };
        let bytes = blaze_wire::encode(&snapshot);
        within_budget(snapshot.entries.len(), bytes.len())?;
        parse(&bytes)?;
        put(store, &bytes)?;
        Ok(snapshot)
    }

    /// The BLAKE3 root named by `SourceRef::Root`.
    pub fn digest(&self) -> Digest {
        hash(&blaze_wire::encode(self))
    }

    /// Add an authenticated regular file without replacing source content or following
    /// source symlinks. This binds replay metadata to the same root as the source inputs.
    pub fn with_file(self, path: &SourcePath, bytes: &[u8], store: &Path) -> anyhow::Result<Self> {
        self.check()?;
        let relative = path.to_native()?;
        ensure!(relative != Path::new("."), "snapshot cannot replace its root");
        let mut entries: BTreeMap<_, _> =
            self.entries.into_iter().map(|entry| (entry.0.clone(), entry)).collect();
        ensure!(
            !entries.contains_key(path),
            "snapshot input {} already exists",
            relative.display()
        );
        for parent in relative.ancestors().skip(1).filter(|path| !path.as_os_str().is_empty()) {
            let logical = SourcePath::from_native(parent)?;
            if let Some(entry) = entries.get(&logical) {
                ensure!(entry.1 == 1, "snapshot parent {} is not a directory", parent.display());
            } else {
                entries.insert(logical.clone(), Entry(logical, 1, 0, [0; 32]));
            }
        }
        entries.insert(path.clone(), Entry(path.clone(), 0, 0, put(store, bytes)?));
        let snapshot = Self { entries: entries.into_values().collect() };
        let bytes = blaze_wire::encode(&snapshot);
        within_budget(snapshot.entries.len(), bytes.len())?;
        parse(&bytes)?;
        put(store, &bytes)?;
        Ok(snapshot)
    }

    /// Place the captured tree `mounted` at the directory `at`, which holds nothing else.
    /// The result addresses its files below `at`, whatever the mounted tree's original
    /// root was, so the snapshot does not name where that root lived. Links of `mounted`
    /// were checked to stay inside it, and still do.
    pub fn with_mount(self, at: &SourcePath, mounted: Snapshot) -> anyhow::Result<Self> {
        self.check()?;
        mounted.check()?;
        let prefix = at.to_native()?;
        ensure!(prefix != Path::new("."), "a mount cannot replace the snapshot root");
        let mut entries: BTreeMap<_, _> =
            self.entries.into_iter().map(|entry| (entry.0.clone(), entry)).collect();
        for parent in prefix.ancestors().filter(|path| !path.as_os_str().is_empty()) {
            let logical = SourcePath::from_native(parent)?;
            if let Some(entry) = entries.get(&logical) {
                ensure!(
                    entry.1 == 1 && parent != prefix,
                    "mount point {} is already in the snapshot",
                    parent.display()
                );
            } else {
                entries.insert(logical.clone(), Entry(logical, 1, 0, [0; 32]));
            }
        }
        for Entry(logical, kind, mode, digest) in mounted.entries {
            let moved = SourcePath::from_native(&prefix.join(logical.to_native()?))?;
            entries.insert(moved.clone(), Entry(moved, kind, mode, digest));
        }
        let snapshot = Self { entries: entries.into_values().collect() };
        snapshot.check()?;
        Ok(snapshot)
    }

    /// Read a regular file only after authenticating its manifest and content. A symlink
    /// cannot stand in for control metadata even when it resolves to a captured file.
    pub fn read_file(root: Digest, store: &Path, path: &SourcePath) -> anyhow::Result<Vec<u8>> {
        let snapshot = parse(&get(store, &root)?)?;
        let entry = snapshot
            .entries
            .iter()
            .find(|entry| &entry.0 == path)
            .context("snapshot file is absent")?;
        ensure!(entry.1 == 0, "snapshot input is not a regular file");
        get(store, &entry.3)
    }

    /// Load and authenticate a manifest and all its blob references before writing any
    /// source files. The destination must not already exist.
    pub fn materialize(root: Digest, store: &Path, destination: &Path) -> anyhow::Result<()> {
        let bytes = get(store, &root)?;
        let snapshot = parse(&bytes)?;
        for Entry(_, kind, _, digest) in &snapshot.entries {
            if *kind != 1 {
                // Authenticate the whole closure before creating the destination, without
                // retaining all source contents in memory at once.
                get(store, digest)?;
            }
        }
        fs::create_dir(destination)?;
        let result = (|| {
            for Entry(logical, kind, mode, digest) in &snapshot.entries {
                let relative = logical.to_native()?;
                let path = destination.join(&relative);
                match kind {
                    1 => fs::create_dir(&path)?,
                    0 => {
                        fs::write(&path, get(store, digest)?)?;
                        #[cfg(unix)]
                        {
                            use std::os::unix::fs::PermissionsExt;
                            fs::set_permissions(&path, fs::Permissions::from_mode(0o444 | mode))?;
                        }
                    }
                    2 => {
                        #[cfg(unix)]
                        {
                            use std::os::unix::{ffi::OsStringExt, fs::symlink};
                            let target =
                                PathBuf::from(std::ffi::OsString::from_vec(get(store, digest)?));
                            ensure!(!target.is_absolute(), "absolute symlink in snapshot");
                            symlink(target, &path)?;
                        }
                        #[cfg(not(unix))]
                        bail!("native symlink materialization is not implemented on this host");
                    }
                    _ => unreachable!("parse validates kinds"),
                }
            }
            let destination = destination.canonicalize()?;
            for Entry(logical, kind, _, _) in &snapshot.entries {
                if *kind == 2 {
                    let path = destination.join(logical.to_native()?);
                    let resolved = path.canonicalize()?;
                    ensure!(
                        resolved.starts_with(&destination),
                        "snapshot symlink resolves outside the captured inputs"
                    );
                }
            }
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_dir_all(destination);
        }
        result
    }

    fn check(&self) -> anyhow::Result<()> {
        let mut previous = None;
        let mut directories = std::collections::BTreeSet::new();
        for Entry(logical, kind, mode, digest) in &self.entries {
            ensure!(previous.is_none_or(|prev| prev < logical), "snapshot paths are not ordered");
            previous = Some(logical);
            let path = logical.to_native()?;
            ensure!(path != Path::new("."), "snapshot cannot replace its root");
            ensure!(*kind <= 2 && mode & !0o111 == 0, "invalid snapshot entry");
            let parent = path.parent().unwrap_or(Path::new(""));
            ensure!(
                parent.as_os_str().is_empty() || directories.contains(parent),
                "missing snapshot directory"
            );
            if *kind == 1 {
                ensure!(*mode == 0 && *digest == [0; 32], "invalid directory entry");
                directories.insert(path);
            }
        }
        Ok(())
    }
}

/// What a manifest may hold. It is a blob of the store listing a tree, not a wire message: the
/// 16 MiB message budget and the 65,536 items of a batch would refuse a tree of 70,000 files,
/// which a workspace with a vendored or ignored directory is.
const MANIFEST: blaze_wire::Limits = blaze_wire::Limits {
    request: 256 << 20,
    batch_items: 4 << 20,
    ..blaze_wire::Limits::DOCUMENTED
};

/// Whether a manifest of `entries` entries in `bytes` bytes is within [`MANIFEST`], as a named
/// refusal rather than a wire-level oversize item.
fn within_budget(entries: usize, bytes: usize) -> anyhow::Result<()> {
    ensure!(
        entries as u64 <= u64::from(MANIFEST.batch_items)
            && bytes as u64 <= u64::from(MANIFEST.request),
        "the source snapshot lists {entries} files, directories and links in {bytes} bytes; a \
         manifest holds at most {} entries in {} bytes: capture a smaller tree (a vendored or \
         generated directory does not belong in the workspace)",
        MANIFEST.batch_items,
        MANIFEST.request,
    );
    Ok(())
}

fn parse(bytes: &[u8]) -> anyhow::Result<Snapshot> {
    within_budget(0, bytes.len())?;
    Ok(blaze_wire::decode(bytes, &MANIFEST)?)
}

struct Capture<'a> {
    root: PathBuf,
    given_root: PathBuf,
    outputs: Vec<PathBuf>,
    store: &'a Path,
    store_root: PathBuf,
    entries: BTreeMap<SourcePath, Entry>,
    expanded: BTreeSet<PathBuf>,
    links: BTreeMap<PathBuf, PathBuf>,
}

impl Capture<'_> {
    fn relative(&self, path: &Path) -> anyhow::Result<PathBuf> {
        let path = blaze_manifest::paths::normalize_path(&std::env::current_dir()?.join(path));
        path.strip_prefix(&self.given_root)
            .or_else(|_| path.strip_prefix(&self.root))
            .map(Path::to_owned)
            .with_context(|| format!("source input {} is outside the snapshot", path.display()))
    }

    fn insert(
        &mut self,
        relative: &Path,
        kind: u8,
        mode: u32,
        digest: Digest,
    ) -> anyhow::Result<()> {
        let logical = SourcePath::from_native(relative)?;
        let entry = Entry(logical.clone(), kind, mode, digest);
        if let Some(previous) = self.entries.get(&logical) {
            ensure!(
                previous == &entry,
                "source input {} changed during capture",
                relative.display()
            );
        } else {
            self.entries.insert(logical, entry);
        }
        Ok(())
    }

    /// Retain links in the selected path itself and in its parents. Following only the
    /// canonical pathname would lose aliases that a manifest or source file still names.
    /// Every link includes its target closure, even when it is outside the selected trees.
    fn path(&mut self, relative: &Path, expand: bool) -> anyhow::Result<PathBuf> {
        ensure!(!relative.is_absolute(), "source path escapes snapshot");
        let components: Vec<_> = relative
            .components()
            .filter(|component| *component != std::path::Component::CurDir)
            .collect();
        let mut at = PathBuf::new();
        for (index, component) in components.iter().enumerate() {
            if *component == std::path::Component::ParentDir {
                ensure!(at.pop(), "source path escapes snapshot");
                continue;
            }
            at.push(component.as_os_str());
            let path = self.root.join(&at);
            ensure!(
                !path.starts_with(&self.store_root),
                "source input refers to its snapshot blob store"
            );
            let metadata = fs::symlink_metadata(&path)?;
            if metadata.is_symlink() {
                let native = fs::read_link(&path)?;
                ensure!(
                    !native.is_absolute(),
                    "absolute symlink {} requires relocation",
                    path.display()
                );
                let target = at.parent().unwrap_or(Path::new("")).join(&native);
                let resolved = path.canonicalize()?;
                let resolved = resolved
                    .strip_prefix(&self.root)
                    .with_context(|| {
                        format!("symlink {} requires a wider source root", path.display())
                    })?
                    .to_owned();
                #[cfg(unix)]
                let bytes = {
                    use std::os::unix::ffi::OsStrExt;
                    native.as_os_str().as_bytes().to_vec()
                };
                #[cfg(not(unix))]
                let bytes =
                    native.to_str().context("unrepresentable symlink target")?.as_bytes().to_vec();
                self.insert(&at, 2, 0, put(self.store, &bytes)?)?;
                if let Some(previous) = self.links.get(&at) {
                    ensure!(
                        previous == &resolved,
                        "symlink {} changed during capture",
                        path.display()
                    );
                } else {
                    self.links.insert(at.clone(), resolved.clone());
                    // A source link makes its target an input even when the caller also
                    // names that location as build output. Other output files stay omitted.
                    let captured = self.path(&target, true)?;
                    ensure!(
                        captured == resolved,
                        "symlink {} changed during capture",
                        path.display()
                    );
                }
                at = resolved;
            } else if metadata.is_dir() {
                self.insert(&at, 1, 0, [0; 32])?;
            } else if metadata.is_file() {
                ensure!(
                    index + 1 == components.len(),
                    "source file {} is not a directory",
                    path.display()
                );
                #[cfg(unix)]
                let mode = {
                    use std::os::unix::fs::PermissionsExt;
                    metadata.permissions().mode() & 0o111
                };
                #[cfg(not(unix))]
                let mode = 0;
                self.insert(&at, 0, mode, put(self.store, &fs::read(&path)?)?)?;
            } else {
                bail!("{} is not a source file, directory or symlink", path.display());
            }
        }
        if expand && self.root.join(&at).is_dir() && self.expanded.insert(at.clone()) {
            for entry in fs::read_dir(self.root.join(&at))? {
                let entry = entry?;
                let path = entry.path();
                if entry.file_name() == ".git"
                    || path.starts_with(&self.store_root)
                    || self.outputs.iter().any(|out| path == *out)
                {
                    continue;
                }
                self.path(path.strip_prefix(&self.root)?, true)?;
            }
        }
        Ok(at)
    }
}

fn hash(bytes: &[u8]) -> Digest {
    *blake3::hash(bytes).as_bytes()
}

fn name(digest: &Digest) -> String {
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn get(store: &Path, digest: &Digest) -> anyhow::Result<Vec<u8>> {
    let bytes = fs::read(store.join(name(digest)))?;
    ensure!(hash(&bytes) == *digest, "snapshot blob {} is corrupt", name(digest));
    Ok(bytes)
}

fn put(store: &Path, bytes: &[u8]) -> anyhow::Result<Digest> {
    use std::io::Write as _;
    fs::create_dir_all(store)?;
    let digest = hash(bytes);
    let path = store.join(name(&digest));
    if path.exists() {
        get(store, &digest)?;
        return Ok(digest);
    }
    let mut temporary = tempfile::NamedTempFile::new_in(store)?;
    temporary.write_all(bytes)?;
    temporary.as_file().sync_all()?;
    match temporary.persist_noclobber(&path) {
        Ok(_) => {}
        Err(error) if error.error.kind() == std::io::ErrorKind::AlreadyExists => {
            get(store, &digest)?;
        }
        Err(error) => return Err(error.into()),
    }
    Ok(digest)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tree(files: usize) -> Snapshot {
        let directory = SourcePath::new("d".to_owned()).unwrap();
        let mut entries = vec![Entry(directory, 1, 0, [0; 32])];
        entries.extend((0..files).map(|at| {
            Entry(SourcePath::new(format!("d/f{at:08}")).unwrap(), 0, 0, [at as u8; 32])
        }));
        Snapshot { entries }
    }

    #[test]
    fn a_manifest_of_a_large_tree_is_read_under_its_own_budget() {
        // More files than a wire batch holds, and more bytes than a wire message.
        let snapshot = tree(400_000);
        let bytes = blaze_wire::encode(&snapshot);
        assert!(bytes.len() > 16 << 20 && snapshot.entries.len() > 65_536);
        assert_eq!(parse(&bytes).unwrap(), snapshot);
    }

    #[test]
    fn a_manifest_over_its_budget_is_refused_by_name() {
        let message = within_budget(5 << 20, 1 << 20).unwrap_err().to_string();
        assert!(message.contains("source snapshot lists 5242880 files"), "{message}");
        assert!(within_budget(1, 300 << 20).is_err());
        within_budget(4 << 20, 256 << 20).unwrap();
        let message = parse(&vec![0; 300 << 20]).unwrap_err().to_string();
        assert!(message.contains("source snapshot lists"), "{message}");
    }
}
