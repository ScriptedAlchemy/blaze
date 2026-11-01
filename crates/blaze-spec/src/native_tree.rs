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
    /// A directory that exists but whose members the capturing user may not list.
    ///
    /// Its name, its type and every symlink reaching it are bound like any other node. Its
    /// members are unknown: nothing below it is a missing candidate and nothing below it is
    /// admitted. Only a denied listing is recorded here; every other read error propagates.
    Unreadable,
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
    ancestor_links: BTreeMap<PathBuf, PathBuf>,
    raw_links: BTreeMap<PathBuf, Vec<Link>>,
    locations: BTreeMap<PathBuf, (PathBuf, PathBuf)>,
}

impl Tree {
    /// Read the complete closure rooted at `root`, following symlinks and hashing files.
    ///
    /// Files and directory membership are checked again after they are read. A directory
    /// whose listing is denied becomes an [`Kind::Unreadable`] observation, because it is part
    /// of the observed platform even though its members cannot be. Other permission, I/O and
    /// unsupported-node errors propagate; only `NotFound` becomes a missing node.
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
            ancestor_links: first.ancestors.clone(),
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
            ancestor_links: resolved.ancestors.clone(),
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
            let links = Self::visible_links(raw.clone());
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
        Ok(Self {
            root,
            entries,
            resolved_root: None,
            ancestor_links: BTreeMap::new(),
            raw_links: BTreeMap::new(),
            locations,
        })
    }

    /// Hash the complete logical tree, without reading the filesystem.
    ///
    /// Absolute symlink targets inside the logical or resolved root are relative to that
    /// root in the identity. The same SDK moved to another directory therefore hashes alike.
    pub fn digest(&self) -> Digest {
        let mut hash = Sha256::new();
        // An unreadable observation is a new, disjoint kind tag: every tree capturable before
        // this kind existed hashes exactly as it did, so the version and saved identities
        // stay valid.
        frame(&mut hash, b"blaze.native-tree.v1");
        hash.update((self.entries.len() as u64).to_le_bytes());
        for (path, entry) in &self.entries {
            frame(&mut hash, path.as_os_str().as_encoded_bytes());
            hash.update((entry.links.len() as u64).to_le_bytes());
            for target in &entry.links {
                self.hash_link(&mut hash, target);
            }
            match &entry.kind {
                Kind::Missing => frame(&mut hash, b"missing"),
                Kind::Directory => frame(&mut hash, b"directory"),
                Kind::Cycle => frame(&mut hash, b"cycle"),
                Kind::Unreadable => frame(&mut hash, b"unreadable"),
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

    fn hash_link(&self, hash: &mut Sha256, target: &Path) {
        if target.is_absolute() {
            let relative = target.strip_prefix(&self.root).ok().or_else(|| {
                self.resolved_root.as_ref().and_then(|root| target.strip_prefix(root).ok())
            });
            match relative {
                Some(relative) => {
                    frame(hash, b"root");
                    frame(hash, relative.as_os_str().as_encoded_bytes());
                }
                None => {
                    frame(hash, b"absolute");
                    frame(hash, target.as_os_str().as_encoded_bytes());
                }
            }
        } else {
            frame(hash, b"relative");
            frame(hash, target.as_os_str().as_encoded_bytes());
        }
    }

    pub(crate) fn summary(&self) -> Summary {
        let mut membership = component(b"membership");
        let mut content = component(b"content");
        let mut mode = component(b"mode");
        let mut links = component(b"links");
        let mut counts = [0_u64; 6];
        for (path, entry) in &self.entries {
            let path = path.as_os_str().as_encoded_bytes();
            frame(&mut membership, path);
            let kind = match &entry.kind {
                Kind::Missing => {
                    counts[0] += 1;
                    b"missing".as_slice()
                }
                Kind::Directory => {
                    counts[1] += 1;
                    b"directory".as_slice()
                }
                Kind::File { digest, len, executable } => {
                    counts[2] += 1;
                    frame(&mut content, path);
                    content.update(len.to_le_bytes());
                    content.update(digest);
                    frame(&mut mode, path);
                    mode.update([u8::from(*executable)]);
                    b"file".as_slice()
                }
                Kind::Cycle => {
                    counts[3] += 1;
                    b"cycle".as_slice()
                }
                Kind::Unreadable => {
                    counts[4] += 1;
                    b"unreadable".as_slice()
                }
            };
            frame(&mut membership, kind);
            if !entry.links.is_empty() {
                counts[5] += entry.links.len() as u64;
                frame(&mut links, path);
                links.update((entry.links.len() as u64).to_le_bytes());
                for target in &entry.links {
                    self.hash_link(&mut links, target);
                }
            }
        }
        Summary {
            digest: self.digest(),
            counts,
            membership: membership.finalize().into(),
            content: content.finalize().into(),
            mode: mode.finalize().into(),
            links: links.finalize().into(),
        }
    }

    /// Find the captured entry of an absolute or root-relative native pathname.
    pub fn entry(&self, path: &Path) -> Option<&Entry> {
        let relative = if path.is_absolute() { path.strip_prefix(&self.root).ok()? } else { path };
        if relative.components().any(|part| matches!(part, Component::ParentDir)) {
            return None;
        }
        self.entries.get(&lexical(relative))
    }

    pub(crate) fn observations(&self) -> impl Iterator<Item = (&Path, &Path, &Entry)> {
        self.locations.iter().filter_map(|(logical, (requested, resolved))| {
            Some((requested.as_path(), resolved.as_path(), self.entries.get(logical)?))
        })
    }

    pub(crate) fn ancestor_aliases(&self) -> impl Iterator<Item = (&Path, &Path)> {
        self.ancestor_links.iter().map(|(alias, resolved)| (alias.as_path(), resolved.as_path()))
    }

    fn visible_links(links: Vec<Link>) -> Vec<PathBuf> {
        links.into_iter().filter_map(|link| (!link.ancestor).then_some(link.target)).collect()
    }

    fn walk(
        &mut self,
        requested: &Path,
        logical: &Path,
        resolved: Resolved,
        active: &mut BTreeSet<PathBuf>,
    ) -> io::Result<()> {
        let Resolved { path, links, kind, .. } = resolved;
        self.locations.insert(logical.to_owned(), (requested.to_owned(), path.clone()));
        self.raw_links.insert(logical.to_owned(), links.clone());
        let links = Self::visible_links(links);
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
                let (digest, len) = read_file(&path, &metadata, &mut || {})?;
                Kind::File { digest, len, executable: executable(&metadata) }
            }
            ResolvedKind::Directory(_) => {
                if !active.insert(path.clone()) {
                    self.entries.insert(logical.to_owned(), Entry { links, kind: Kind::Cycle });
                    return Ok(());
                }
                let children = match names(&path) {
                    Ok(children) => Some(children),
                    Err(error) if error.kind() == io::ErrorKind::PermissionDenied => None,
                    Err(error) => return Err(error),
                };
                match children {
                    Some(children) => {
                        self.entries.insert(
                            logical.to_owned(),
                            Entry { links: links.clone(), kind: Kind::Directory },
                        );
                        for name in &children {
                            let requested = path.join(name);
                            self.walk(
                                &requested,
                                &logical.join(name),
                                resolve(&requested)?,
                                active,
                            )?;
                        }
                        // Membership is what a directory binds: an entry added, removed or
                        // renamed while its children were read. Its stamp alone is not
                        // (a child relinked in place, as Nix's store optimiser does, changes
                        // the directory's ctime); each child's bytes were checked as read.
                        if names(&path)? != children {
                            return Err(changed(&path));
                        }
                        active.remove(&path);
                        Kind::Directory
                    }
                    // The directory is part of the platform: bind that it exists, is a
                    // directory and could not be listed, never an empty directory. No walk
                    // below it can claim its members are missing.
                    None => {
                        active.remove(&path);
                        Kind::Unreadable
                    }
                }
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
            || Self::visible_links(after.links) != links
            || !same_kind(&after.kind, &kind)
            || after_stamp != stamp
        {
            return Err(changed(requested));
        }
        self.entries.insert(logical.to_owned(), Entry { links, kind });
        Ok(())
    }
}

pub(crate) struct Summary {
    pub(crate) digest: Digest,
    pub(crate) counts: [u64; 6],
    pub(crate) membership: Digest,
    pub(crate) content: Digest,
    pub(crate) mode: Digest,
    pub(crate) links: Digest,
}

fn component(domain: &[u8]) -> Sha256 {
    let mut hash = Sha256::new();
    frame(&mut hash, b"blaze/native-tree-observation/v1");
    frame(&mut hash, domain);
    hash
}

fn same_kind(actual: &ResolvedKind, expected: &Kind) -> bool {
    matches!(
        (actual, expected),
        (ResolvedKind::Missing, Kind::Missing)
            | (ResolvedKind::Cycle, Kind::Cycle)
            | (ResolvedKind::Directory(_), Kind::Directory)
            | (ResolvedKind::Directory(_), Kind::Unreadable)
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

/// Whether a traversal precedes the captured node belongs to the occurrence, not the
/// symlink's pathname: a file's target can traverse an ancestor alias again (Debian's
/// libm.so points through /lib, which is itself a symlink).
#[derive(Clone, Debug, PartialEq, Eq)]
struct Link {
    target: PathBuf,
    ancestor: bool,
}

struct Resolved {
    path: PathBuf,
    links: Vec<Link>,
    ancestors: BTreeMap<PathBuf, PathBuf>,
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
    let mut ancestors = BTreeMap::new();
    let (resolved, kind) =
        resolve_path(path, &mut BTreeSet::new(), &mut links, &mut ancestors, None)?;
    // What the path reaches must still be of the kind found: a file's bytes and a
    // directory's members are checked as they are read, so a stamp alone is not compared
    // (a file relinked in place by Nix's store optimiser keeps its bytes and changes its
    // inode and ctime).
    if let ResolvedKind::File(_) | ResolvedKind::Directory(_) = &kind {
        let now = fs::metadata(path)?;
        let same = match &kind {
            ResolvedKind::File(_) => now.is_file(),
            _ => now.is_dir(),
        };
        if !same {
            return Err(changed(path));
        }
    }
    Ok(Resolved { path: resolved, links, ancestors, kind })
}

fn resolve_path(
    path: &Path,
    active: &mut BTreeSet<PathBuf>,
    links: &mut Vec<Link>,
    ancestors: &mut BTreeMap<PathBuf, PathBuf>,
    ancestor: Option<bool>,
) -> io::Result<(PathBuf, ResolvedKind)> {
    let mut current = PathBuf::new();
    let mut metadata = None;
    let mut components = path.components().peekable();
    while let Some(component) = components.next() {
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
                    // The link's target is what it binds; its stamp is not (relinking it in
                    // place leaves the same target).
                    if fs::read_link(&current)? != target {
                        return Err(changed(&current));
                    }
                    // Prefix expansions precede the captured root. The final root's
                    // own symlink chain remains part of its identity.
                    let is_ancestor = ancestor.unwrap_or_else(|| components.peek().is_some());
                    links.push(Link { target: target.clone(), ancestor: is_ancestor });
                    let destination = current.parent().unwrap_or(Path::new("/")).join(target);
                    let result =
                        resolve_path(&destination, active, links, ancestors, Some(is_ancestor))?;
                    if is_ancestor {
                        ancestors.insert(current.clone(), result.0.clone());
                    }
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

/// The digest and length of the file at `path`, read whole. A file whose stamp moves while it
/// is read is read again, and is the input it was when its bytes are the same both times: Nix's
/// store optimiser relinks a file in place, which changes its inode and ctime and not a byte,
/// and a build elsewhere may touch it. Bytes that differ between two reads are a changed input.
/// `between` runs after the first read, for a test to move the file.
fn read_file(
    path: &Path,
    metadata: &Metadata,
    between: &mut dyn FnMut(),
) -> io::Result<(Digest, u64)> {
    let mut before = Stamp::of(metadata)?;
    let mut first: Option<(Digest, u64)> = None;
    for _ in 0..3 {
        let mut file = fs::File::open(path)?;
        let opened = Stamp::of(&file.metadata()?)?;
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
        between();
        let read: (Digest, u64) = (hash.finalize().into(), len);
        match first {
            Some(it) if it != read => return Err(changed(path)),
            _ => {}
        }
        let after = Stamp::of(&file.metadata()?)?;
        let now = fs::metadata(path)?;
        if opened == before && after == before && Stamp::of(&now)? == before {
            return Ok(read);
        }
        // Moved while read: the next read has to find the same bytes.
        first = Some(read);
        before = Stamp::of(&now)?;
    }
    Err(changed(path))
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

#[cfg(all(test, unix))]
mod digest_compatibility_tests {
    use super::*;

    #[test]
    fn native_tree_digest_literal_vectors() {
        let root = Path::new("/golden-root");
        let missing = Tree::from_files(root, std::iter::empty()).unwrap();
        assert_eq!(
            missing.digest(),
            [
                0xd0, 0x7b, 0xc4, 0xea, 0x26, 0x45, 0xf2, 0x49, 0xaa, 0x9a, 0xac, 0x87, 0x03, 0x7b,
                0x5e, 0x26, 0x40, 0x10, 0xb1, 0x41, 0xbc, 0x8b, 0x73, 0x97, 0x8e, 0x8f, 0x89, 0x86,
                0xba, 0x1a, 0xce, 0x3e,
            ]
        );

        let mut tree =
            Tree::from_files(root, [("include/a.h".into(), b"header".to_vec())]).unwrap();
        assert_eq!(
            tree.digest(),
            [
                0x55, 0xc1, 0x33, 0x8e, 0x17, 0x65, 0x05, 0xff, 0x86, 0x27, 0x18, 0xdf, 0x2c, 0xc6,
                0x2d, 0x88, 0x89, 0x83, 0x76, 0x34, 0x08, 0xff, 0xe5, 0xe7, 0x10, 0xc2, 0x0b, 0x31,
                0x2a, 0x94, 0x96, 0x9b,
            ]
        );
        let entry = tree.entries.get_mut(Path::new("include/a.h")).unwrap();
        entry.links =
            vec![root.join("include/a.h"), PathBuf::from("/outside/x"), PathBuf::from("../other")];
        if let Kind::File { executable, .. } = &mut entry.kind {
            *executable = true;
        } else {
            panic!("fixture is not a file");
        }
        assert_eq!(
            tree.digest(),
            [
                0x3b, 0x02, 0xd5, 0x28, 0xfc, 0xd5, 0xe2, 0xef, 0xfd, 0xb6, 0xca, 0x92, 0x9d, 0x27,
                0x0d, 0xe4, 0xf5, 0x23, 0x70, 0xca, 0x92, 0x0d, 0x67, 0x45, 0xb8, 0x93, 0x09, 0xdd,
                0x44, 0xbe, 0x8c, 0x35,
            ]
        );
    }
}

#[cfg(test)]
mod read_tests {
    use super::*;

    fn written(dir: &Path, bytes: &[u8]) -> (PathBuf, Metadata) {
        let path = dir.join("input");
        fs::write(&path, bytes).unwrap();
        let metadata = fs::metadata(&path).unwrap();
        (path, metadata)
    }

    #[test]
    fn a_file_moved_while_read_is_the_input_when_its_bytes_are_the_same() {
        let dir = tempfile::tempdir().unwrap();
        let (path, metadata) = written(dir.path(), b"the same bytes");
        let mut moved = 0;
        let later = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_000);
        let (digest, len) = read_file(&path, &metadata, &mut || {
            // Relinked in place, as Nix's store optimiser does: a new stamp, the same bytes.
            if moved == 0 {
                let again = dir.path().join("again");
                fs::copy(&path, &again).unwrap();
                fs::rename(&again, &path).unwrap();
                fs::File::options().write(true).open(&path).unwrap().set_modified(later).unwrap();
            }
            moved += 1;
        })
        .unwrap();
        assert_eq!(moved, 2, "read again after the move, then at rest");
        assert_eq!(len, 14);
        let expected: Digest = Sha256::digest(b"the same bytes").into();
        assert_eq!(digest, expected);
    }

    #[test]
    fn a_file_whose_bytes_change_while_read_is_a_changed_input() {
        let dir = tempfile::tempdir().unwrap();
        let (path, metadata) = written(dir.path(), b"first bytes");
        let mut moved = 0;
        let error = read_file(&path, &metadata, &mut || {
            if moved == 0 {
                fs::write(&path, b"other bytes").unwrap();
            }
            moved += 1;
        })
        .unwrap_err();
        assert!(error.to_string().contains("changed while reading"), "{error}");
    }

    #[test]
    fn a_file_that_never_rests_is_a_changed_input() {
        let dir = tempfile::tempdir().unwrap();
        let (path, metadata) = written(dir.path(), b"restless");
        let mut tick = 0_u64;
        let error = read_file(&path, &metadata, &mut || {
            tick += 1;
            let at = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(tick);
            fs::File::options().write(true).open(&path).unwrap().set_modified(at).unwrap();
        })
        .unwrap_err();
        assert!(error.to_string().contains("changed while reading"), "{error}");
    }
}
