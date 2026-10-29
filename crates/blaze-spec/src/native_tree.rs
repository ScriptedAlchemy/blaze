//! Complete native filesystem identities, independent of the directory's location.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, Metadata},
    io::{self, Read},
    path::{Component, Path, PathBuf},
    time::SystemTime,
};

use blaze_wire::Digest;
use sha2::{Digest as _, Sha256};

/// The referent of a native pathname after following its symlinks.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Kind {
    /// The pathname or a symlink referent does not exist.
    Missing,
    /// A directory whose children are separate entries in the tree.
    Directory,
    /// The complete contents and executable permission of a regular file.
    File {
        /// SHA-256 of every byte in the file.
        digest: Digest,
        /// The number of bytes read.
        len: u64,
        /// Whether any Unix executable permission bit is set.
        executable: bool,
    },
    /// A symlink expansion or directory edge returns to an active ancestor.
    Cycle,
}

/// One logical pathname, including the identity of every symlink traversed there.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    /// Symlink targets in traversal order, as returned by `read_link`.
    pub links: Vec<PathBuf>,
    /// The resolved node, with complete file contents represented by their digest.
    pub kind: Kind,
}

/// A complete directory or file closure, including missing paths and symlink referents.
///
/// Entry names are native paths relative to `root`; the root's entry has an empty name.
/// Directory symlinks have their referent's children under the logical symlink name.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Tree {
    /// The absolute location used to capture this tree, excluded from its digest.
    pub root: PathBuf,
    /// All logical nodes in native pathname order.
    pub entries: BTreeMap<PathBuf, Entry>,
    resolved_root: Option<PathBuf>,
    raw_links: BTreeMap<PathBuf, Vec<(PathBuf, PathBuf)>>,
    locations: BTreeMap<PathBuf, (PathBuf, PathBuf)>,
}

impl Tree {
    /// Read the complete closure rooted at `root`, following symlinks and hashing files.
    ///
    /// Files and directory membership are checked again after they are read. Permission,
    /// I/O and unsupported-node errors propagate; only `NotFound` becomes a missing node.
    pub fn capture(root: &Path) -> io::Result<Self> {
        let root =
            if root.is_absolute() { root.to_owned() } else { std::env::current_dir()?.join(root) };
        let first = resolve(&root)?;
        let resolved_root =
            matches!(first.kind, ResolvedKind::Directory(_)).then(|| first.path.clone());
        let mut tree = Self {
            root: root.clone(),
            entries: BTreeMap::new(),
            resolved_root,
            raw_links: BTreeMap::new(),
            locations: BTreeMap::new(),
        };
        tree.walk(&root, Path::new(""), first, &mut BTreeSet::new())?;
        Ok(tree)
    }

    /// Capture multiple roots as one observation, reusing complete captured ancestor trees.
    ///
    /// Broad physical directories are read first. Aliases and contained roots are re-rooted
    /// from those observations, preserving the identity each independent capture would
    /// have on the same filesystem. Re-rooting that changes a directory-cycle boundary
    /// falls back to a separate complete capture. Nothing is cached between calls.
    pub fn capture_many(paths: &[PathBuf]) -> io::Result<Vec<Self>> {
        let requests = paths
            .iter()
            .map(|path| {
                let root = if path.is_absolute() {
                    path.clone()
                } else {
                    std::env::current_dir()?.join(path)
                };
                let resolved = resolve(&root)?;
                Ok((root, resolved))
            })
            .collect::<io::Result<Vec<_>>>()?;
        let mut order: Vec<usize> = (0..requests.len()).collect();
        order.sort_by(|&a, &b| {
            let a = &requests[a].1.path;
            let b = &requests[b].1.path;
            a.components().count().cmp(&b.components().count()).then_with(|| a.cmp(b))
        });
        let mut captured = Vec::<Self>::new();
        let mut result = vec![None; requests.len()];
        for index in order {
            let (root, resolved) = &requests[index];
            if matches!(resolved.kind, ResolvedKind::Missing | ResolvedKind::Cycle) {
                result[index] = Some(Self::capture(root)?);
                continue;
            }
            let mut derived = None;
            for tree in &captured {
                if let Some(tree) = tree.reroot(root, resolved)? {
                    derived = Some(tree);
                    break;
                }
            }
            if derived.is_none() {
                let tree = Self::capture(&resolved.path)?;
                derived = tree.reroot(root, resolved)?;
                captured.push(tree);
            }
            result[index] = Some(match derived {
                Some(tree) => tree,
                None => Self::capture(root)?,
            });
        }
        Ok(result.into_iter().map(Option::unwrap).collect())
    }

    fn reroot(&self, root: &Path, resolved: &Resolved) -> io::Result<Option<Self>> {
        let Ok(prefix) = resolved.path.strip_prefix(&self.root) else { return Ok(None) };
        let Some(entry) = self.entries.get(prefix) else { return Ok(None) };
        if !same_kind(&resolved.kind, &entry.kind) {
            return Ok(None);
        }
        let mut tree = Self {
            root: root.to_owned(),
            entries: BTreeMap::new(),
            resolved_root: matches!(resolved.kind, ResolvedKind::Directory(_))
                .then(|| resolved.path.clone()),
            raw_links: BTreeMap::new(),
            locations: BTreeMap::new(),
        };
        for (path, entry) in &self.entries {
            let Ok(relative) = path.strip_prefix(prefix) else { continue };
            if !prefix.as_os_str().is_empty() && entry.kind == Kind::Cycle {
                let cycle = resolve(&resolved.path.join(relative))?;
                if matches!(cycle.kind, ResolvedKind::Directory(_))
                    && !cycle.path.starts_with(&resolved.path)
                {
                    return Ok(None);
                }
            }
            let raw = if relative.as_os_str().is_empty() {
                resolved.links.clone()
            } else {
                let Some(raw) = self.raw_links.get(path) else { return Ok(None) };
                raw.clone()
            };
            let links = tree.visible_links(raw.clone());
            tree.raw_links.insert(relative.to_owned(), raw);
            let location = if relative.as_os_str().is_empty() {
                (root.to_owned(), resolved.path.clone())
            } else {
                let Some(location) = self.locations.get(path) else { return Ok(None) };
                location.clone()
            };
            tree.locations.insert(relative.to_owned(), location);
            tree.entries.insert(relative.to_owned(), Entry { links, kind: entry.kind.clone() });
        }
        Ok(Some(tree))
    }

    /// Construct the complete filesystem of a canned probe from its regular files.
    ///
    /// Absolute paths outside `root` are ignored; relative paths are rooted there.
    /// Parent directories are inferred. No matching files means the root is missing.
    /// This constructor performs no filesystem reads and gives files no executable bits.
    pub fn from_files(
        root: impl Into<PathBuf>,
        files: impl IntoIterator<Item = (PathBuf, Vec<u8>)>,
    ) -> io::Result<Self> {
        let root = root.into();
        if !root.is_absolute() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "native tree root must be absolute",
            ));
        }
        let root = lexical(&root);
        let mut entries = BTreeMap::new();
        for (path, bytes) in files {
            let relative = if path.is_absolute() {
                match path.strip_prefix(&root) {
                    Ok(path) => path.to_owned(),
                    Err(_) => continue,
                }
            } else {
                path
            };
            if relative
                .components()
                .any(|part| !matches!(part, Component::Normal(_) | Component::CurDir))
            {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "native tree entry is not relative",
                ));
            }
            let relative = lexical(&relative);
            let file = Entry {
                links: Vec::new(),
                kind: Kind::File {
                    digest: Sha256::digest(&bytes).into(),
                    len: bytes.len() as u64,
                    executable: false,
                },
            };
            if let Some(previous) = entries.insert(relative.clone(), file.clone())
                && previous != file
            {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "conflicting native tree entries",
                ));
            }
            let mut parent = relative.parent();
            while let Some(path) = parent {
                let directory = Entry { links: Vec::new(), kind: Kind::Directory };
                if let Some(previous) = entries.get(path)
                    && previous != &directory
                {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "native file is also a directory",
                    ));
                }
                entries.insert(path.to_owned(), directory);
                parent = path.parent();
            }
        }
        if entries.is_empty() {
            entries.insert(PathBuf::new(), Entry { links: Vec::new(), kind: Kind::Missing });
        }
        let locations = entries
            .keys()
            .map(|relative| {
                let path = root.join(relative);
                (relative.clone(), (path.clone(), path))
            })
            .collect();
        Ok(Self { root, entries, resolved_root: None, raw_links: BTreeMap::new(), locations })
    }

    /// Hash the complete logical tree, without reading the filesystem.
    ///
    /// Absolute symlink targets inside the logical or resolved root are relative to that
    /// root in the identity. The same SDK moved to another directory therefore hashes alike.
    pub fn digest(&self) -> Digest {
        let mut hash = Sha256::new();
        frame(&mut hash, b"blaze.native-tree.v1");
        hash.update((self.entries.len() as u64).to_le_bytes());
        for (path, entry) in &self.entries {
            frame(&mut hash, path.as_os_str().as_encoded_bytes());
            hash.update((entry.links.len() as u64).to_le_bytes());
            for target in &entry.links {
                if target.is_absolute() {
                    let relative = target.strip_prefix(&self.root).ok().or_else(|| {
                        self.resolved_root.as_ref().and_then(|root| target.strip_prefix(root).ok())
                    });
                    match relative {
                        Some(relative) => {
                            frame(&mut hash, b"root");
                            frame(&mut hash, relative.as_os_str().as_encoded_bytes());
                        }
                        None => {
                            frame(&mut hash, b"absolute");
                            frame(&mut hash, target.as_os_str().as_encoded_bytes());
                        }
                    }
                } else {
                    frame(&mut hash, b"relative");
                    frame(&mut hash, target.as_os_str().as_encoded_bytes());
                }
            }
            match &entry.kind {
                Kind::Missing => frame(&mut hash, b"missing"),
                Kind::Directory => frame(&mut hash, b"directory"),
                Kind::Cycle => frame(&mut hash, b"cycle"),
                Kind::File { digest, len, executable } => {
                    frame(&mut hash, b"file");
                    hash.update(len.to_le_bytes());
                    hash.update([u8::from(*executable)]);
                    hash.update(digest);
                }
            }
        }
        hash.finalize().into()
    }

    /// Find the captured entry of an absolute or root-relative native pathname.
    pub fn entry(&self, path: &Path) -> Option<&Entry> {
        let relative = if path.is_absolute() { path.strip_prefix(&self.root).ok()? } else { path };
        if relative.components().any(|part| matches!(part, Component::ParentDir)) {
            return None;
        }
        self.entries.get(&lexical(relative))
    }

    pub(crate) fn physical_root(&self) -> Option<&Path> {
        let entry = self.entries.get(Path::new(""))?;
        if !matches!(entry.kind, Kind::Directory | Kind::File { .. }) {
            return None;
        }
        self.locations.get(Path::new("")).map(|(_, resolved)| resolved.as_path())
    }

    pub(crate) fn observations(&self) -> impl Iterator<Item = (&Path, &Path, &Entry)> {
        self.locations.iter().filter_map(|(logical, (requested, resolved))| {
            Some((requested.as_path(), resolved.as_path(), self.entries.get(logical)?))
        })
    }

    fn visible_links(&self, links: Vec<(PathBuf, PathBuf)>) -> Vec<PathBuf> {
        links
            .into_iter()
            .filter_map(|(at, target)| {
                let ancestor = (at != self.root && self.root.starts_with(&at))
                    || self
                        .resolved_root
                        .as_ref()
                        .is_some_and(|root| at != *root && root.starts_with(&at));
                (!ancestor).then_some(target)
            })
            .collect()
    }

    fn walk(
        &mut self,
        requested: &Path,
        logical: &Path,
        resolved: Resolved,
        active: &mut BTreeSet<PathBuf>,
    ) -> io::Result<()> {
        let Resolved { path, links, kind } = resolved;
        self.locations.insert(logical.to_owned(), (requested.to_owned(), path.clone()));
        self.raw_links.insert(logical.to_owned(), links.clone());
        let links = self.visible_links(links);
        let stamp = match &kind {
            ResolvedKind::File(metadata) | ResolvedKind::Directory(metadata) => {
                Some(Stamp::of(metadata)?)
            }
            _ => None,
        };
        let kind = match kind {
            ResolvedKind::Missing => Kind::Missing,
            ResolvedKind::Cycle => Kind::Cycle,
            ResolvedKind::File(metadata) => {
                let before = Stamp::of(&metadata)?;
                let mut file = fs::File::open(&path)?;
                if Stamp::of(&file.metadata()?)? != before {
                    return Err(changed(&path));
                }
                let mut hash = Sha256::new();
                let mut len = 0;
                let mut buffer = [0; 64 * 1024];
                loop {
                    let count = file.read(&mut buffer)?;
                    if count == 0 {
                        break;
                    }
                    len += count as u64;
                    hash.update(&buffer[..count]);
                }
                if Stamp::of(&file.metadata()?)? != before
                    || Stamp::of(&fs::metadata(&path)?)? != before
                    || len != metadata.len()
                {
                    return Err(changed(&path));
                }
                Kind::File {
                    digest: hash.finalize().into(),
                    len,
                    executable: executable(&metadata),
                }
            }
            ResolvedKind::Directory(metadata) => {
                if !active.insert(path.clone()) {
                    self.entries.insert(logical.to_owned(), Entry { links, kind: Kind::Cycle });
                    return Ok(());
                }
                let before = Stamp::of(&metadata)?;
                let children = names(&path)?;
                self.entries.insert(
                    logical.to_owned(),
                    Entry { links: links.clone(), kind: Kind::Directory },
                );
                for name in &children {
                    let requested = path.join(name);
                    self.walk(&requested, &logical.join(name), resolve(&requested)?, active)?;
                }
                if names(&path)? != children || Stamp::of(&fs::metadata(&path)?)? != before {
                    return Err(changed(&path));
                }
                active.remove(&path);
                Kind::Directory
            }
        };
        let after = resolve(requested)?;
        let after_stamp = match &after.kind {
            ResolvedKind::File(metadata) | ResolvedKind::Directory(metadata) => {
                Some(Stamp::of(metadata)?)
            }
            _ => None,
        };
        if after.path != path
            || self.visible_links(after.links) != links
            || !same_kind(&after.kind, &kind)
            || after_stamp != stamp
        {
            return Err(changed(requested));
        }
        self.entries.insert(logical.to_owned(), Entry { links, kind });
        Ok(())
    }
}

fn same_kind(actual: &ResolvedKind, expected: &Kind) -> bool {
    matches!(
        (actual, expected),
        (ResolvedKind::Missing, Kind::Missing)
            | (ResolvedKind::Cycle, Kind::Cycle)
            | (ResolvedKind::Directory(_), Kind::Directory)
            | (ResolvedKind::File(_), Kind::File { .. })
    )
}

fn names(path: &Path) -> io::Result<Vec<std::ffi::OsString>> {
    let mut names = fs::read_dir(path)
        .map_err(|error| {
            io::Error::new(error.kind(), format!("reading {}: {error}", path.display()))
        })?
        .map(|entry| entry.map(|entry| entry.file_name()))
        .collect::<io::Result<Vec<_>>>()?;
    names.sort();
    Ok(names)
}

struct Resolved {
    path: PathBuf,
    links: Vec<(PathBuf, PathBuf)>,
    kind: ResolvedKind,
}

enum ResolvedKind {
    Missing,
    Directory(Metadata),
    File(Metadata),
    Cycle,
}

fn resolve(path: &Path) -> io::Result<Resolved> {
    let mut links = Vec::new();
    let (resolved, kind) = resolve_path(path, &mut BTreeSet::new(), &mut links)?;
    if let ResolvedKind::File(metadata) | ResolvedKind::Directory(metadata) = &kind
        && Stamp::of(metadata)? != Stamp::of(&fs::metadata(path)?)?
    {
        return Err(changed(path));
    }
    Ok(Resolved { path: resolved, links, kind })
}

fn resolve_path(
    path: &Path,
    active: &mut BTreeSet<PathBuf>,
    links: &mut Vec<(PathBuf, PathBuf)>,
) -> io::Result<(PathBuf, ResolvedKind)> {
    let mut current = PathBuf::new();
    let mut metadata = None;
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => {
                current.push(prefix.as_os_str());
                metadata = None;
            }
            Component::RootDir => {
                current.push(component.as_os_str());
                metadata = None;
            }
            Component::CurDir => {}
            Component::ParentDir => {
                if metadata.as_ref().is_some_and(|metadata: &Metadata| !metadata.is_dir()) {
                    return Err(io::Error::new(
                        io::ErrorKind::NotADirectory,
                        "native path traverses a file",
                    ));
                }
                current.pop();
                metadata = None;
            }
            Component::Normal(name) => {
                current.push(name);
                let found = match fs::symlink_metadata(&current) {
                    Ok(metadata) => metadata,
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {
                        return Ok((current, ResolvedKind::Missing));
                    }
                    Err(error) => return Err(error),
                };
                if found.file_type().is_symlink() {
                    if active.len() >= 64 {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "too many native symlink expansions",
                        ));
                    }
                    if !active.insert(current.clone()) {
                        return Ok((current, ResolvedKind::Cycle));
                    }
                    let target = fs::read_link(&current)?;
                    if Stamp::of(&fs::symlink_metadata(&current)?)? != Stamp::of(&found)? {
                        return Err(changed(&current));
                    }
                    links.push((current.clone(), target.clone()));
                    let destination = current.parent().unwrap_or(Path::new("/")).join(target);
                    let result = resolve_path(&destination, active, links)?;
                    active.remove(&current);
                    current = result.0;
                    match result.1 {
                        ResolvedKind::Missing | ResolvedKind::Cycle => {
                            return Ok((current, result.1));
                        }
                        ResolvedKind::Directory(value) | ResolvedKind::File(value) => {
                            metadata = Some(value)
                        }
                    }
                } else {
                    metadata = Some(found);
                }
            }
        }
    }
    let metadata = match metadata {
        Some(metadata) => metadata,
        None => fs::symlink_metadata(&current)?,
    };
    let kind = if metadata.is_dir() {
        ResolvedKind::Directory(metadata)
    } else if metadata.is_file() {
        ResolvedKind::File(metadata)
    } else {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            format!("unsupported native node {}", current.display()),
        ));
    };
    Ok((current, kind))
}

#[derive(PartialEq, Eq)]
struct Stamp {
    len: u64,
    modified: SystemTime,
    readonly: bool,
    #[cfg(unix)]
    unix: (u64, u64, i64, i64, u32),
}

impl Stamp {
    fn of(metadata: &Metadata) -> io::Result<Self> {
        #[cfg(unix)]
        use std::os::unix::fs::MetadataExt;
        Ok(Self {
            len: metadata.len(),
            modified: metadata.modified()?,
            readonly: metadata.permissions().readonly(),
            #[cfg(unix)]
            unix: (
                metadata.dev(),
                metadata.ino(),
                metadata.ctime(),
                metadata.ctime_nsec(),
                metadata.mode(),
            ),
        })
    }
}

fn executable(metadata: &Metadata) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        let _ = metadata;
        false
    }
}

fn changed(path: &Path) -> io::Error {
    io::Error::other(format!("native input changed while reading {}", path.display()))
}

fn frame(hash: &mut Sha256, value: &[u8]) {
    hash.update((value.len() as u64).to_le_bytes());
    hash.update(value);
}

fn lexical(path: &Path) -> PathBuf {
    let mut result = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                result.pop();
            }
            _ => result.push(component.as_os_str()),
        }
    }
    result
}
