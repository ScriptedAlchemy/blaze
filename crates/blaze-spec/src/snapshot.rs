//! Immutable source inputs. The manifest is deterministic CBOR; file blobs and the
//! manifest are addressed by BLAKE3, independently of filesystem timestamps.
//!
//! A capture retains dirty, ignored and untracked files, empty directories and Unix
//! executable permissions. Only explicitly supplied output directories and Git's
//! administration directory are omitted. Materialization uses copies, never hard links
//! to mutable requester files or to the blob store.

use std::{
    collections::BTreeMap,
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
        let root = root.canonicalize()?;
        let outputs: Vec<_> = outputs
            .iter()
            .map(|path| {
                let path = blaze_manifest::paths::normalize_path(path);
                path.canonicalize().unwrap_or(path)
            })
            .collect();
        let mut entries = BTreeMap::new();
        for tree in trees {
            let tree = tree.canonicalize()?;
            let relative = tree.strip_prefix(&root)?;
            directories(relative, &mut entries)?;
            walk(&root, relative, &outputs, store, &mut entries)?;
        }
        for (path, bytes) in structural {
            let path = path.canonicalize()?;
            let relative = path.strip_prefix(&root)?;
            if let Some(parent) = relative.parent() {
                directories(parent, &mut entries)?;
            }
            let logical = SourcePath::from_native(relative)?;
            if !entries.contains_key(&logical) {
                #[cfg(unix)]
                let mode = {
                    use std::os::unix::fs::PermissionsExt;
                    fs::metadata(&path)?.permissions().mode() & 0o111
                };
                #[cfg(not(unix))]
                let mode = 0;
                entries.insert(logical.clone(), Entry(logical, 0, mode, put(store, bytes)?));
            }
        }
        let snapshot = Self { entries: entries.into_values().collect() };
        for (path, bytes) in structural {
            let path = blaze_manifest::paths::normalize_path(path);
            let path = path.canonicalize().unwrap_or(path);
            let relative = path.strip_prefix(&root).with_context(|| {
                format!("structural input {} is outside the snapshot", path.display())
            })?;
            let logical = SourcePath::from_native(relative)?;
            let entry = snapshot.entries.iter().find(|entry| entry.0 == logical);
            ensure!(
                entry.is_some_and(|entry| entry.1 == 0 && entry.3 == hash(bytes)),
                "input {} changed during resolution; retry the capture",
                path.display()
            );
        }
        let bytes = blaze_wire::encode(&snapshot);
        parse(&bytes)?;
        put(store, &bytes)?;
        Ok(snapshot)
    }

    /// The BLAKE3 root named by `SourceRef::Root`.
    pub fn digest(&self) -> Digest {
        hash(&blaze_wire::encode(self))
    }

    /// Load and authenticate a manifest and all its blob references before writing any
    /// source files. The destination must not already exist.
    pub fn materialize(root: Digest, store: &Path, destination: &Path) -> anyhow::Result<()> {
        let bytes = get(store, &root)?;
        let snapshot = parse(&bytes)?;
        let mut contents = Vec::with_capacity(snapshot.entries.len());
        for Entry(_, kind, _, digest) in &snapshot.entries {
            contents.push(if *kind == 1 { Vec::new() } else { get(store, digest)? });
        }
        fs::create_dir(destination)?;
        let result = (|| {
            for (Entry(logical, kind, mode, _), bytes) in snapshot.entries.iter().zip(&contents) {
                let relative = logical.to_native()?;
                let path = destination.join(&relative);
                match kind {
                    1 => fs::create_dir(&path)?,
                    0 => {
                        fs::write(&path, bytes)?;
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
                            let target = PathBuf::from(std::ffi::OsString::from_vec(bytes.clone()));
                            ensure!(!target.is_absolute(), "absolute symlink in snapshot");
                            let joined = blaze_manifest::paths::normalize_path(
                                &relative.parent().unwrap_or(Path::new("")).join(&target),
                            );
                            ensure!(!joined.starts_with(".."), "symlink escapes snapshot");
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

fn directories(path: &Path, entries: &mut BTreeMap<SourcePath, Entry>) -> anyhow::Result<()> {
    for path in
        path.ancestors().filter(|path| !path.as_os_str().is_empty() && *path != Path::new("."))
    {
        let logical = SourcePath::from_native(path)?;
        entries.entry(logical.clone()).or_insert(Entry(logical, 1, 0, [0; 32]));
    }
    Ok(())
}

fn parse(bytes: &[u8]) -> anyhow::Result<Snapshot> {
    Ok(blaze_wire::decode(bytes, &blaze_wire::Limits::default())?)
}

fn walk(
    root: &Path,
    relative: &Path,
    outputs: &[PathBuf],
    store: &Path,
    entries: &mut BTreeMap<SourcePath, Entry>,
) -> anyhow::Result<()> {
    for entry in fs::read_dir(root.join(relative))? {
        let entry = entry?;
        let path = entry.path();
        if entry.file_name() == ".git" || outputs.iter().any(|out| path.starts_with(out)) {
            continue;
        }
        let relative = blaze_manifest::paths::normalize_path(path.strip_prefix(root)?);
        let logical = SourcePath::from_native(&relative)?;
        let metadata = fs::symlink_metadata(&path)?;
        let (kind, mode, digest) = if metadata.is_dir() {
            (1, 0, [0; 32])
        } else if metadata.is_file() {
            #[cfg(unix)]
            let mode = {
                use std::os::unix::fs::PermissionsExt;
                metadata.permissions().mode() & 0o111
            };
            #[cfg(not(unix))]
            let mode = 0;
            let bytes = fs::read(&path)?;
            (0, mode, put(store, &bytes)?)
        } else if metadata.is_symlink() {
            let target = fs::read_link(&path)?;
            let resolved = path.canonicalize()?;
            ensure!(
                resolved.starts_with(root),
                "symlink {} requires a wider source root",
                path.display()
            );
            ensure!(
                !target.is_absolute(),
                "absolute symlink {} requires relocation",
                path.display()
            );
            #[cfg(unix)]
            let bytes = {
                use std::os::unix::ffi::OsStrExt;
                target.as_os_str().as_bytes().to_vec()
            };
            #[cfg(not(unix))]
            let bytes =
                target.to_str().context("unrepresentable symlink target")?.as_bytes().to_vec();
            (2, 0, put(store, &bytes)?)
        } else {
            bail!("{} is not a source file, directory or symlink", path.display());
        };
        entries.insert(logical.clone(), Entry(logical, kind, mode, digest));
        if kind == 1 {
            walk(root, &relative, outputs, store, entries)?;
        }
    }
    Ok(())
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
