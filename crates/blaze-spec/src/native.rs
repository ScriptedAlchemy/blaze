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
    /// Prepare an immutable query scope reused across multiple native reads.
    pub fn admission(&self) -> Admission<'_> {
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
        Admission { closure: self, nodes: self.nodes(), aliases }
    }

    /// The bound identity of an input or absent search candidate, without filesystem reads.
    pub fn entry(&self, path: &Path) -> Option<Entry> {
        self.admission().entry(path)
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

    pub(crate) fn observation(&self) -> Observation {
        let mut roots = Vec::with_capacity(self.roots.len().min(64));
        for root in self.roots.iter().take(64) {
            let mut hash = Sha256::new();
            frame(&mut hash, b"blaze/native-role-observation/v1");
            frame(&mut hash, root.name.as_bytes());
            roots.push(RootObservation { role: hash.finalize().into(), tree: root.tree.summary() });
        }
        Observation { digest: self.digest(), total: self.roots.len(), roots }
    }

    /// Require an observed input to match an entry in a complete bound tree.
    /// No library name or filename extension confers authority.
    pub fn admit(&self, path: &Path, observed: &Entry) -> anyhow::Result<()> {
        self.admission().admit(path, observed)
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

/// Reusable reads against one immutable captured closure, without filesystem access.
pub struct Admission<'a> {
    closure: &'a Closure,
    nodes: BTreeMap<PathBuf, Option<Node>>,
    aliases: BTreeMap<&'a Path, Option<&'a Path>>,
}

impl Admission<'_> {
    /// The bound identity of an input or absent search candidate, without filesystem reads.
    pub fn entry(&self, path: &Path) -> Option<Entry> {
        match self.entry_at(path) {
            Lookup::Bound(entry) => return Some(entry),
            Lookup::Rejected => return None,
            Lookup::Unanchored => {}
        }
        let mut current = path.to_owned();
        let mut visited = BTreeSet::from([current.clone()]);
        while let Some((alias, resolved)) = self
            .aliases
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
            match self.entry_at(&current) {
                Lookup::Bound(entry) => return Some(entry),
                Lookup::Rejected => return None,
                Lookup::Unanchored => {}
            }
        }
        None
    }

    fn entry_at(&self, path: &Path) -> Lookup<Entry> {
        let mut prefix = PathBuf::new();
        let mut crossed_parent = false;
        for component in path.components() {
            prefix.push(component.as_os_str());
            crossed_parent |= matches!(component, Component::ParentDir);
            match observed_node(&self.nodes, &prefix) {
                Lookup::Bound(node) => {
                    if crossed_parent
                        && !self.closure.roots.iter().any(|root| root.tree.root == prefix)
                    {
                        return Lookup::Rejected;
                    }
                    return walk_captured(
                        &self.nodes,
                        node.resolved.clone(),
                        node.entry.clone(),
                        path.strip_prefix(&prefix).unwrap(),
                    );
                }
                Lookup::Rejected => return Lookup::Rejected,
                Lookup::Unanchored => {}
            }
        }
        if crossed_parent { Lookup::Rejected } else { Lookup::Unanchored }
    }

    /// Require an observed input to match an entry in a complete bound tree.
    /// No library name or filename extension confers authority.
    pub fn admit(&self, path: &Path, observed: &Entry) -> anyhow::Result<()> {
        if self.entry(path).as_ref() == Some(observed) {
            return Ok(());
        }
        anyhow::bail!("unbound native input {}", path.display())
    }
}

struct RootObservation {
    role: Digest,
    tree: crate::native_tree::Summary,
}

pub(crate) struct Observation {
    digest: Digest,
    total: usize,
    roots: Vec<RootObservation>,
}

impl Observation {
    pub(crate) fn difference(&self, actual: &Self) -> String {
        use std::fmt::Write as _;
        let mut out = String::with_capacity(8192);
        let _ = write!(out, "native closure expected ");
        hex(&mut out, &self.digest);
        out.push_str(" actual ");
        hex(&mut out, &actual.digest);
        let _ = writeln!(out, "\nroots expected {} actual {}", self.total, actual.total);
        let _ = writeln!(
            out,
            "observation roots omitted expected {} actual {}",
            self.total - self.roots.len(),
            actual.total - actual.roots.len()
        );
        let aligned = self.total == actual.total
            && self.roots.iter().zip(&actual.roots).all(|(a, b)| a.role == b.role);
        if !aligned {
            out.push_str("root alignment uncertain (role, order, insertion or removal)\n");
            out.push_str("output root rows omitted 0 (row limit); other rows suppressed by alignment uncertainty\n");
            return out;
        }
        let mut changed = 0_usize;
        for (ordinal, (expected, actual)) in self.roots.iter().zip(&actual.roots).enumerate() {
            let a = &expected.tree;
            let b = &actual.tree;
            if a.digest == b.digest {
                continue;
            }
            changed += 1;
            if changed > 16 {
                continue;
            }
            let _ = write!(out, "root {ordinal} expected ");
            hex(&mut out, &a.digest);
            out.push_str(" actual ");
            hex(&mut out, &b.digest);
            let _ = write!(
                out,
                "\n  counts missing/directory/file/cycle/link expected {}/{}/{}/{}/{} actual {}/{}/{}/{}/{}\n  changed",
                a.counts[0],
                a.counts[1],
                a.counts[2],
                a.counts[3],
                a.counts[4],
                b.counts[0],
                b.counts[1],
                b.counts[2],
                b.counts[3],
                b.counts[4]
            );
            if a.membership != b.membership {
                out.push_str(" membership");
            }
            if a.content != b.content {
                out.push_str(" content");
            }
            if a.mode != b.mode {
                out.push_str(" mode");
            }
            if a.links != b.links {
                out.push_str(" links");
            }
            out.push('\n');
        }
        let _ = writeln!(out, "output root rows omitted {}", changed.saturating_sub(16));
        out
    }
}

fn hex(out: &mut String, digest: &Digest) {
    use std::fmt::Write as _;
    for byte in digest {
        let _ = write!(out, "{byte:02x}");
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

enum Lookup<T> {
    Unanchored,
    Bound(T),
    Rejected,
}

fn observed_node<'a>(nodes: &'a BTreeMap<PathBuf, Option<Node>>, path: &Path) -> Lookup<&'a Node> {
    let node = match nodes.get(path) {
        Some(Some(node)) => node,
        Some(None) => return Lookup::Rejected,
        None => return Lookup::Unanchored,
    };
    for prefix in node.resolved.ancestors() {
        match nodes.get(prefix) {
            Some(None) => return Lookup::Rejected,
            Some(Some(observed)) => {
                let kind =
                    if prefix == node.resolved { &node.entry.kind } else { &Kind::Directory };
                if observed.resolved != prefix || &observed.entry.kind != kind {
                    return Lookup::Rejected;
                }
            }
            None => {}
        }
    }
    Lookup::Bound(node)
}

fn walk_captured(
    nodes: &BTreeMap<PathBuf, Option<Node>>,
    mut current: PathBuf,
    mut entry: Entry,
    suffix: &Path,
) -> Lookup<Entry> {
    for component in suffix.components() {
        if entry.kind == Kind::Missing {
            return Lookup::Bound(missing());
        }
        if entry.kind != Kind::Directory {
            return Lookup::Rejected;
        }
        let node = match component {
            Component::CurDir => continue,
            Component::ParentDir => {
                current.pop();
                match observed_node(nodes, &current) {
                    Lookup::Bound(node) if node.entry.kind == Kind::Directory => node,
                    _ => return Lookup::Rejected,
                }
            }
            Component::Normal(name) => match observed_node(nodes, &current.join(name)) {
                Lookup::Bound(node) => node,
                Lookup::Rejected => return Lookup::Rejected,
                Lookup::Unanchored => return Lookup::Bound(missing()),
            },
            _ => return Lookup::Rejected,
        };
        current.clone_from(&node.resolved);
        entry.clone_from(&node.entry);
        if matches!(component, Component::ParentDir) {
            entry.links.clear();
        }
    }
    Lookup::Bound(entry)
}

fn missing() -> Entry {
    Entry { links: Vec::new(), kind: Kind::Missing }
}

#[cfg(all(test, unix))]
mod observation_tests {
    use super::*;

    fn tree(bytes: &[u8]) -> Tree {
        Tree::from_files("/private-sentinel", [(PathBuf::from("secret-sentinel"), bytes.to_vec())])
            .unwrap()
    }

    fn closure(trees: Vec<(&str, Tree)>) -> Closure {
        Closure {
            roots: trees.into_iter().map(|(name, tree)| Root { name: name.into(), tree }).collect(),
        }
    }

    fn report(a: &Closure, b: &Closure) -> String {
        a.observation().difference(&b.observation())
    }

    #[test]
    fn dimensions_and_privacy() {
        let initial = closure(vec![("role-secret-sentinel", tree(b"one"))]);
        let same = closure(vec![("role-secret-sentinel", tree(b"one"))]);
        let unchanged = report(&initial, &same);
        assert!(unchanged.contains("output root rows omitted 0\n"));
        assert!(!unchanged.contains("root 0 expected"));
        let content = closure(vec![("role-secret-sentinel", tree(b"two"))]);
        let changed = report(&initial, &content);
        assert!(changed.contains("  changed content\n"));
        assert!(!changed.contains("secret-sentinel"));
        let member = closure(vec![(
            "role-secret-sentinel",
            Tree::from_files("/private-sentinel", [(PathBuf::from("other"), b"one".to_vec())])
                .unwrap(),
        )]);
        assert!(report(&initial, &member).contains("  changed membership content mode\n"));
        let mut executable = tree(b"one");
        if let Kind::File { executable, .. } =
            &mut executable.entries.get_mut(Path::new("secret-sentinel")).unwrap().kind
        {
            *executable = true;
        }
        assert!(
            report(&initial, &closure(vec![("role-secret-sentinel", executable)]))
                .contains("  changed mode\n")
        );
    }

    #[test]
    fn links_use_relocated_identity() {
        let mut a = tree(b"one");
        a.entries
            .get_mut(Path::new("secret-sentinel"))
            .unwrap()
            .links
            .push(PathBuf::from("/private-sentinel/inside"));
        let mut b =
            Tree::from_files("/relocated", [(PathBuf::from("secret-sentinel"), b"one".to_vec())])
                .unwrap();
        b.entries
            .get_mut(Path::new("secret-sentinel"))
            .unwrap()
            .links
            .push(PathBuf::from("/relocated/inside"));
        assert!(
            !report(&closure(vec![("r", a)]), &closure(vec![("r", b.clone())]))
                .contains("root 0 expected")
        );
        b.entries.get_mut(Path::new("secret-sentinel")).unwrap().links[0] =
            PathBuf::from("/external-secret");
        let mut c = tree(b"one");
        c.entries
            .get_mut(Path::new("secret-sentinel"))
            .unwrap()
            .links
            .push(PathBuf::from("/private-sentinel/inside"));
        let output = report(&closure(vec![("r", c)]), &closure(vec![("r", b)]));
        assert!(output.contains("  changed links\n"));
        assert!(!output.contains("external-secret"));
    }

    #[test]
    fn alignment_and_bounds() {
        let a = closure(vec![("a", tree(b"a")), ("b", tree(b"b"))]);
        let b = closure(vec![("b", tree(b"b")), ("a", tree(b"a"))]);
        let output = report(&a, &b);
        assert!(output.contains("root alignment uncertain"));
        assert!(!output.contains("root 0 expected"));
        let renamed = closure(vec![("changed-role", tree(b"a")), ("b", tree(b"b"))]);
        assert!(report(&a, &renamed).contains("root alignment uncertain"));
        let inserted = closure(vec![("new", tree(b"a")), ("a", tree(b"a")), ("b", tree(b"b"))]);
        assert!(report(&a, &inserted).contains("root alignment uncertain"));
        let many = |bytes: &[u8]| Closure {
            roots: (0..70).map(|i| Root { name: format!("r{i}"), tree: tree(bytes) }).collect(),
        };
        let expected = many(b"a").observation();
        let actual = many(b"b").observation();
        assert!(
            std::mem::size_of::<Observation>()
                + expected.roots.capacity() * std::mem::size_of::<RootObservation>()
                <= 32 * 1024
        );
        let output = expected.difference(&actual);
        assert!(output.contains("observation roots omitted expected 6 actual 6"));
        assert!(output.contains("output root rows omitted 48"));
        assert!(output.len() <= 8192);
        assert!(!output.contains("secret-sentinel"));
    }
}
