//! Native input bindings shared by platform discovery and saved execution.

use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Component, Path, PathBuf},
};

use anyhow::{Context as _, ensure};
use blaze_wire::{ConfigEnvValue as EnvValue, Digest, InputText};
use sha2::{Digest as _, Sha256};

use crate::native_tree::{Entry, Kind, Tree};

pub use blaze_wire::native::{
    BINDINGS_FIELD, UNIT_FIELD, Unit, decode_units, encode_units, redact_transport,
};

/// One complete native input tree with a stable role in the platform.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Root {
    /// Relocation-independent role, such as the SDK or a compiler search directory.
    pub name: String,
    /// Complete observed contents at the physical root.
    pub tree: Tree,
}

/// Complete trees whose contents authorize native input reads.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Closure {
    /// Ordered roots; search order and missing roots are part of their identity.
    pub roots: Vec<Root>,
}

impl Closure {
    /// The bound identity of an input or absent search candidate, without filesystem reads.
    pub fn entry(&self, path: &Path) -> Option<Entry> {
        if let Some(entry) = self.entry_at(path) {
            return Some(entry);
        }
        let mut aliases = BTreeMap::<&Path, Option<&Path>>::new();
        for root in &self.roots {
            for (alias, resolved) in root.tree.ancestor_aliases() {
                aliases
                    .entry(alias)
                    .and_modify(|previous| {
                        if *previous != Some(resolved) {
                            *previous = None;
                        }
                    })
                    .or_insert(Some(resolved));
            }
        }
        let mut current = path.to_owned();
        let mut visited = BTreeSet::from([current.clone()]);
        while let Some((alias, resolved)) = aliases
            .iter()
            .filter(|(alias, _)| current.starts_with(alias))
            .max_by_key(|(alias, _)| alias.components().count())
        {
            let resolved = (*resolved)?;
            let relative = current.strip_prefix(alias).ok()?;
            current = resolved.join(relative);
            if !visited.insert(current.clone()) {
                return None;
            }
            if let Some(entry) = self.entry_at(&current) {
                return Some(entry);
            }
        }
        None
    }

    fn entry_at(&self, path: &Path) -> Option<Entry> {
        for root in &self.roots {
            let Ok(relative) = path.strip_prefix(&root.tree.root) else { continue };
            if relative.components().any(|part| matches!(part, Component::ParentDir)) {
                continue;
            }
            if let Some(entry) = root.tree.entry(relative) {
                return Some(entry.clone());
            }
            let mut ancestor = relative.parent();
            while let Some(parent) = ancestor {
                if let Some(entry) = root.tree.entry(parent) {
                    if matches!(entry.kind, Kind::Directory | Kind::Missing) {
                        return Some(missing());
                    }
                    break;
                }
                ancestor = parent.parent();
            }
        }
        let nodes = self.nodes();
        for root in &self.roots {
            let Some(initial) = root.tree.entry(Path::new("")) else { continue };
            let physical = root.tree.physical_root();
            for (prefix, at_physical) in [(Some(root.tree.root.as_path()), false), (physical, true)]
            {
                let Some(prefix) = prefix else { continue };
                let Ok(relative) = path.strip_prefix(prefix) else { continue };
                let Some(mut current) = physical.map(Path::to_owned) else {
                    if initial.kind == Kind::Missing {
                        return Some(missing());
                    }
                    continue;
                };
                let mut entry = initial.clone();
                if at_physical {
                    entry.links.clear();
                }
                let mut complete = true;
                for component in relative.components() {
                    if entry.kind == Kind::Missing {
                        return Some(missing());
                    }
                    if entry.kind != Kind::Directory {
                        complete = false;
                        break;
                    }
                    match component {
                        Component::CurDir => continue,
                        Component::ParentDir => {
                            current.pop();
                            let Some(Some(node)) = nodes.get(&current) else {
                                complete = false;
                                break;
                            };
                            current.clone_from(&node.resolved);
                            entry.clone_from(&node.entry);
                            entry.links.clear();
                        }
                        Component::Normal(name) => {
                            let candidate = current.join(name);
                            match nodes.get(&candidate) {
                                Some(Some(node)) => {
                                    current.clone_from(&node.resolved);
                                    entry.clone_from(&node.entry);
                                }
                                Some(None) => {
                                    complete = false;
                                    break;
                                }
                                None => return Some(missing()),
                            }
                        }
                        _ => {
                            complete = false;
                            break;
                        }
                    }
                }
                if complete {
                    return Some(entry);
                }
            }
        }
        None
    }

    fn nodes(&self) -> BTreeMap<PathBuf, Option<Node>> {
        let mut nodes = BTreeMap::new();
        let mut insert = |path: &Path, node: Node| match nodes.entry(path.to_owned()) {
            std::collections::btree_map::Entry::Vacant(entry) => {
                entry.insert(Some(node));
            }
            std::collections::btree_map::Entry::Occupied(mut entry) => {
                if entry.get().as_ref() != Some(&node) {
                    entry.insert(None);
                }
            }
        };
        for root in &self.roots {
            for (requested, resolved, entry) in root.tree.observations() {
                insert(requested, Node { resolved: resolved.to_owned(), entry: entry.clone() });
                if requested != resolved && !matches!(entry.kind, Kind::Missing | Kind::Cycle) {
                    insert(
                        resolved,
                        Node {
                            resolved: resolved.to_owned(),
                            entry: Entry { links: Vec::new(), kind: entry.kind.clone() },
                        },
                    );
                }
            }
        }
        nodes
    }

    /// A digest of every tree and its role, excluding physical root locations.
    pub fn digest(&self) -> Digest {
        let mut hash = Sha256::new();
        frame(&mut hash, b"blaze/native-closure/v1");
        for root in &self.roots {
            frame(&mut hash, root.name.as_bytes());
            frame(&mut hash, &root.tree.digest());
        }
        hash.finalize().into()
    }

    /// Require an observed input to match an entry in a complete bound tree.
    /// No library name or filename extension confers authority.
    pub fn admit(&self, path: &Path, observed: &Entry) -> anyhow::Result<()> {
        if self.entry(path).as_ref() == Some(observed) {
            return Ok(());
        }
        anyhow::bail!("unbound native input {}", path.display())
    }

    /// Encode root locations and identities in the existing deterministic CBOR format.
    /// File bytes and process environment values are not staged here.
    pub fn encode_bindings(&self) -> Vec<u8> {
        let roots: Vec<_> = self
            .roots
            .iter()
            .map(|root| {
                (
                    InputText(root.name.clone()),
                    EnvValue::from_native(root.tree.root.as_os_str()),
                    root.tree.digest(),
                )
            })
            .collect();
        blaze_wire::encode(&(1_u32, roots))
    }

    /// Decode and recapture bindings, rejecting changed contents before admission.
    pub fn from_bindings(bytes: &[u8]) -> anyhow::Result<Self> {
        let (version, roots): (u32, Vec<(InputText, EnvValue, Digest)>) =
            blaze_wire::decode(bytes, &blaze_wire::Limits::DOCUMENTED)?;
        ensure!(version == 1, "unknown native bindings version {version}");
        let mut names = BTreeSet::new();
        let mut bindings = Vec::new();
        let mut out = Self::default();
        for (name, path, expected) in roots {
            ensure!(names.insert(name.0.clone()), "duplicate native root {}", name.0);
            ensure!(
                !matches!(path, EnvValue::WorkspaceRelative(_)),
                "native binding root uses a relative encoding"
            );
            let path = PathBuf::from(path.materialize(Path::new("/"))?);
            ensure!(path.is_absolute(), "native binding root is not absolute");
            bindings.push((name, path, expected));
        }
        let paths: Vec<_> = bindings.iter().map(|(_, path, _)| path.clone()).collect();
        let trees = Tree::capture_many(&paths).context("recapturing native bindings")?;
        for ((name, path, expected), tree) in bindings.into_iter().zip(trees) {
            ensure!(tree.digest() == expected, "native root {} changed", path.display());
            out.roots.push(Root { name: name.0, tree });
        }
        Ok(out)
    }

    /// Verify complete root identities again after a compiler or producer has finished.
    pub fn validate(&self) -> anyhow::Result<()> {
        let paths: Vec<_> = self.roots.iter().map(|root| root.tree.root.clone()).collect();
        for (root, current) in self.roots.iter().zip(Tree::capture_many(&paths)?) {
            ensure!(
                current.digest() == root.tree.digest(),
                "native root {} changed during compilation",
                root.tree.root.display()
            );
        }
        Ok(())
    }
}

fn frame(hash: &mut Sha256, bytes: &[u8]) {
    hash.update((bytes.len() as u64).to_le_bytes());
    hash.update(bytes);
}

#[derive(PartialEq, Eq)]
struct Node {
    resolved: PathBuf,
    entry: Entry,
}

fn missing() -> Entry {
    Entry { links: Vec::new(), kind: Kind::Missing }
}
