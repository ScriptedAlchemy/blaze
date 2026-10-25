//! Where a package comes from, as cargo 1.99 names it (`SourceId` in
//! `src/workspace/source_id.rs`), and the names of the directories cargo keeps a source in,
//! which hash it with `rustc-stable-hash` as cargo has since 1.85.

use std::{
    fmt,
    hash::{Hash, Hasher},
    path::{Path, PathBuf},
};

use anyhow::{Context, bail};
use url::Url;

pub const CRATES_IO_INDEX: &str = "https://github.com/rust-lang/crates.io-index";
pub const CRATES_IO_HTTP_INDEX: &str = "sparse+https://index.crates.io/";
pub const CRATES_IO_REGISTRY: &str = "crates-io";

/// Which commit of a git repository a dependency asks for.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum GitReference {
    Tag(String),
    Branch(String),
    /// A commit, full or short, or a named reference such as `refs/pull/493/head`.
    Rev(String),
    /// The default branch, the reference named `HEAD`.
    DefaultBranch,
}

impl GitReference {
    /// From the query of a source's URL, as a lock file writes it (`?branch=main`).
    pub fn from_query(
        pairs: impl Iterator<Item = (impl AsRef<str>, impl AsRef<str>)>,
    ) -> GitReference {
        let mut reference = GitReference::DefaultBranch;
        for (key, value) in pairs {
            let value = value.as_ref().to_owned();
            match key.as_ref() {
                "branch" | "ref" => reference = GitReference::Branch(value),
                "rev" => reference = GitReference::Rev(value),
                "tag" => reference = GitReference::Tag(value),
                _ => {}
            }
        }
        reference
    }

    /// The reference as a lock file's URL writes it in its query, none for the default branch.
    pub fn pretty(&self) -> Option<String> {
        let (key, value) = match self {
            GitReference::Tag(it) => ("tag", it),
            GitReference::Branch(it) => ("branch", it),
            GitReference::Rev(it) => ("rev", it),
            GitReference::DefaultBranch => return None,
        };
        let value: String = url::form_urlencoded::byte_serialize(value.as_bytes()).collect();
        Some(format!("{key}={value}"))
    }
}

/// The kinds of source, in cargo's order: their discriminants are part of every directory name
/// cargo hashes from a source, so the variants keep cargo's order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SourceKind {
    Git(GitReference),
    Path,
    Registry,
    SparseRegistry,
    LocalRegistry,
    Directory,
}

impl Hash for SourceKind {
    fn hash<H: Hasher>(&self, state: &mut H) {
        std::mem::discriminant(self).hash(state);
        if let SourceKind::Git(git) = self {
            git.hash(state);
        }
    }
}

/// A source: its kind, its URL as written and its canonical URL, which is what two URLs of
/// one source have in common.
#[derive(Clone, Debug)]
pub struct SourceId {
    kind: SourceKind,
    url: Url,
    canonical: Url,
}

impl PartialEq for SourceId {
    fn eq(&self, other: &SourceId) -> bool {
        self.kind == other.kind && self.canonical == other.canonical
    }
}

impl Eq for SourceId {}

impl Hash for SourceId {
    fn hash<H: Hasher>(&self, into: &mut H) {
        self.kind.hash(into);
        self.canonical.as_str().hash(into);
    }
}

impl SourceId {
    pub fn new(kind: SourceKind, url: Url) -> anyhow::Result<SourceId> {
        if kind == SourceKind::SparseRegistry && !url.as_str().starts_with("sparse+") {
            bail!("a sparse registry's URL starts with `sparse+`: {url}");
        }
        let canonical = canonical_url(&url)?;
        Ok(SourceId { kind, url, canonical })
    }

    /// crates.io, read over its sparse index, as cargo reads it unless
    /// `registries.crates-io.protocol` is `git`.
    pub fn crates_io() -> SourceId {
        SourceId::for_registry(CRATES_IO_HTTP_INDEX).expect("crates.io's index is a URL")
    }

    /// crates.io, read from its git index.
    pub fn crates_io_git() -> SourceId {
        SourceId::for_registry(CRATES_IO_INDEX).expect("crates.io's index is a URL")
    }

    /// A remote registry: sparse when its URL starts with `sparse+`, else a git index.
    pub fn for_registry(url: &str) -> anyhow::Result<SourceId> {
        let url = Url::parse(url).with_context(|| format!("`{url}` is not a URL"))?;
        let kind = if url.as_str().starts_with("sparse+") {
            SourceKind::SparseRegistry
        } else {
            SourceKind::Registry
        };
        SourceId::new(kind, url)
    }

    pub fn for_local_registry(path: &Path) -> anyhow::Result<SourceId> {
        SourceId::new(SourceKind::LocalRegistry, path_url(path)?)
    }

    pub fn for_directory(path: &Path) -> anyhow::Result<SourceId> {
        SourceId::new(SourceKind::Directory, path_url(path)?)
    }

    pub fn for_path(path: &Path) -> anyhow::Result<SourceId> {
        SourceId::new(SourceKind::Path, path_url(path)?)
    }

    pub fn for_git(url: &Url, reference: GitReference) -> anyhow::Result<SourceId> {
        SourceId::new(SourceKind::Git(reference), url.clone())
    }

    /// A source as a lock file writes it: `registry+<url>`, `sparse+<url>`,
    /// `git+<url>?<reference>#<commit>` (the commit is not part of the source and is returned
    /// beside it) or `path+<url>`.
    pub fn from_url(text: &str) -> anyhow::Result<(SourceId, Option<String>)> {
        let (kind, rest) =
            text.split_once('+').with_context(|| format!("invalid source `{text}`"))?;
        let parse = |it: &str| Url::parse(it).with_context(|| format!("`{it}` is not a URL"));
        match kind {
            "git" => {
                let mut url = parse(rest)?;
                let reference = GitReference::from_query(url.query_pairs());
                let precise = url.fragment().map(str::to_owned);
                url.set_fragment(None);
                url.set_query(None);
                Ok((SourceId::for_git(&url, reference)?, precise))
            }
            "registry" => Ok((SourceId::new(SourceKind::Registry, parse(rest)?)?, None)),
            "sparse" => Ok((SourceId::new(SourceKind::SparseRegistry, parse(text)?)?, None)),
            "path" => Ok((SourceId::new(SourceKind::Path, parse(rest)?)?, None)),
            kind => bail!("unsupported source protocol: {kind}"),
        }
    }

    pub fn kind(&self) -> &SourceKind {
        &self.kind
    }

    pub fn url(&self) -> &Url {
        &self.url
    }

    pub fn canonical_url(&self) -> &Url {
        &self.canonical
    }

    pub fn is_sparse(&self) -> bool {
        self.kind == SourceKind::SparseRegistry
    }

    pub fn is_remote_registry(&self) -> bool {
        matches!(self.kind, SourceKind::Registry | SourceKind::SparseRegistry)
    }

    pub fn is_crates_io(&self) -> bool {
        self.is_remote_registry()
            && matches!(self.url.as_str(), CRATES_IO_INDEX | CRATES_IO_HTTP_INDEX)
    }

    /// The name of the directories under `$CARGO_HOME/registry/{index,cache,src}` a registry
    /// is kept in: the host of its URL and a hash of the source, `index.crates.io-1949cf8c6b5b557f`
    /// for crates.io (cargo's `short_name` in `src/sources/registry/mod.rs`).
    pub fn registry_dir_name(&self) -> String {
        let host = self.url.host_str().unwrap_or("");
        format!("{host}-{}", short_hash(self))
    }
}

/// As cargo shows a source in its messages.
impl fmt::Display for SourceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.kind {
            SourceKind::Git(reference) => {
                write!(f, "{}", self.url)?;
                if let Some(pretty) = reference.pretty() {
                    write!(f, "?{pretty}")?;
                }
                Ok(())
            }
            SourceKind::Path => write!(f, "{}", url_display(&self.url)),
            SourceKind::Registry | SourceKind::SparseRegistry if self.is_crates_io() => {
                write!(f, "registry `{CRATES_IO_REGISTRY}`")
            }
            SourceKind::Registry | SourceKind::SparseRegistry | SourceKind::LocalRegistry => {
                write!(f, "registry `{}`", url_display(&self.url))
            }
            SourceKind::Directory => write!(f, "dir {}", url_display(&self.url)),
        }
    }
}

fn url_display(url: &Url) -> String {
    match url.scheme() {
        "file" => {
            url.to_file_path().map_or_else(|()| url.to_string(), |it| it.display().to_string())
        }
        _ => url.to_string(),
    }
}

/// The directory of a path, directory or local-registry source.
pub(crate) fn path_of(id: &SourceId) -> anyhow::Result<PathBuf> {
    id.url().to_file_path().map_err(|()| anyhow::anyhow!("{} is not a directory", id.url()))
}

/// Sixteen hex digits of a hash of `value` that are the same on every platform and release, as
/// cargo's `util::hex::short_hash`: the little-endian bytes of a 64-bit SipHash-1-3 of 128 bits.
pub fn short_hash<H: Hash + ?Sized>(value: &H) -> String {
    let mut hasher = rustc_stable_hash::StableSipHasher128::new();
    value.hash(&mut hasher);
    Hasher::finish(&hasher).to_le_bytes().iter().map(|byte| format!("{byte:02x}")).collect()
}

/// The URL two spellings of one source share, as cargo's `CanonicalUrl::new`: no slash at the
/// end, and for a git URL, GitHub's in lower case over https and none with `.git` at the end.
fn canonical_url(url: &Url) -> anyhow::Result<Url> {
    let mut url = url.clone();
    if url.cannot_be_a_base() {
        bail!("invalid url `{url}`: cannot-be-a-base-URLs are not supported");
    }
    if url.path().ends_with('/') {
        url.path_segments_mut().expect("a base URL has segments").pop_if_empty();
    }
    if !url.scheme().contains('+') {
        if url.host_str() == Some("github.com") {
            url = format!("https{}", &url[url::Position::AfterScheme..]).parse()?;
            let path = url.path().to_lowercase();
            url.set_path(&path);
        }
        if url.path().ends_with(".git") {
            let last = {
                let last = url.path_segments().and_then(|mut it| it.next_back()).unwrap_or("");
                last[..last.len() - 4].to_owned()
            };
            url.path_segments_mut().expect("a base URL has segments").pop().push(&last);
        }
    }
    Ok(url)
}

fn path_url(path: &Path) -> anyhow::Result<Url> {
    Url::from_file_path(path).map_err(|()| anyhow::anyhow!("invalid path url `{}`", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The values of cargo 1.99's `test_stable_hash` (`src/workspace/source_id.rs`), and the
    /// directory cargo 1.99 makes for crates.io.
    #[test]
    fn names_as_cargo() {
        assert_eq!(short_hash(&SourceId::crates_io_git()), "25cdd57fae9f0462");
        let mine = SourceId::for_registry("https://my-crates.io").unwrap();
        assert_eq!(short_hash(&mine), "b2d65deb64f05373");
        assert_eq!(SourceId::crates_io().registry_dir_name(), "index.crates.io-1949cf8c6b5b557f");
        assert_eq!(SourceId::crates_io_git().registry_dir_name(), "github.com-25cdd57fae9f0462");
    }

    #[test]
    fn stable_hashes_as_cargo() {
        // `gen_hash` of cargo's test: the source's `stable_hash`, which for these is its kind
        // and its URL as written, finished as a number.
        let stable = |id: &SourceId| {
            let mut hasher = rustc_stable_hash::StableSipHasher128::new();
            id.kind.hash(&mut hasher);
            id.url.as_str().hash(&mut hasher);
            Hasher::finish(&hasher)
        };
        assert_eq!(stable(&SourceId::crates_io_git()), 7062945687441624357);
        let mine = SourceId::for_registry("https://my-crates.io").unwrap();
        assert_eq!(stable(&mine), 8310250053664888498);
        let yours = SourceId::for_registry("https://your-crates.io").unwrap();
        assert_eq!(stable(&yours), 14149534903000258933);
    }

    #[test]
    fn canonical_urls() {
        let canonical = |it: &str| canonical_url(&Url::parse(it).unwrap()).unwrap().to_string();
        assert_eq!(canonical("https://github.com/Foo/Bar.git"), "https://github.com/foo/bar");
        assert_eq!(canonical("http://github.com/foo/bar/"), "https://github.com/foo/bar");
        // A path of only `/` keeps it: the url crate has no empty path for a URL with a host.
        assert_eq!(canonical("sparse+https://index.crates.io/"), "sparse+https://index.crates.io/");
        assert_eq!(
            canonical("sparse+https://example.com/index/"),
            "sparse+https://example.com/index"
        );
        assert_eq!(canonical("https://example.com/Repo.git"), "https://example.com/Repo");
    }

    #[test]
    fn lock_file_sources() {
        let (id, precise) =
            SourceId::from_url("registry+https://github.com/rust-lang/crates.io-index").unwrap();
        assert!(id.is_crates_io() && !id.is_sparse() && precise.is_none());
        let (id, _) = SourceId::from_url("sparse+https://index.crates.io/").unwrap();
        assert_eq!(id, SourceId::crates_io());
        let (id, precise) =
            SourceId::from_url("git+https://github.com/a/b?branch=dev#0123abcd").unwrap();
        assert_eq!(id.kind(), &SourceKind::Git(GitReference::Branch("dev".to_owned())));
        assert_eq!(id.url().as_str(), "https://github.com/a/b");
        assert_eq!(precise.as_deref(), Some("0123abcd"));
    }
}
