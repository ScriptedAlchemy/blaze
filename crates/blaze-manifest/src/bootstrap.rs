//! Shared build-script identity, lock closure and compatibility proofs.

use core::fmt::{self, Write};
use blaze_int::Int;

pub const TOOLCHAIN: &str = blaze_pins::TOOLCHAIN;

/// Whether a package's raw manifest has no input inherited from its enclosing workspace.
/// A non-root path package that returns false adds that workspace manifest to its source pin.
pub fn standalone_manifest(text: &str, alloc: blaze_own::Alloc<'_>) -> bool {
    let Ok(document) = blaze_toml::Document::parse_in(text, blaze_toml::Limits::CARGO, alloc) else { return false };
    let root = document.root();
    let inherited = |value: blaze_toml::Item<'_, '_, blaze_own::Alloc<'_>>| value.get("workspace").and_then(|field| field.as_bool()) == Some(true);
    if root.get("package").is_some_and(|package| package.children().any(inherited)) { return false; }
    for table in core::iter::once(root).chain(root.get("target").into_iter().flat_map(|target| target.children())) {
        for name in ["dependencies", "build-dependencies", "build_dependencies", "dev-dependencies", "dev_dependencies"] {
            if table.get(name).is_some_and(|deps| deps.children().any(inherited)) { return false; }
        }
    }
    true
}

/// A path script's source pin. Its package files already cover a root package's workspace
/// manifest. A non-root package that inherits settings adds the captured root manifest whole:
/// package metadata and dependency tables can each contribute inherited compilation inputs.
pub fn path_source(files: [u8; 64], workspace: Option<&[u8]>) -> [u8; 64] {
    let Some(workspace) = workspace else { return files };
    let mut hash = blaze_sha256::Sha256::new();
    hash.update(b"blaze build-script workspace source 1\n").update(&files).update(&workspace.len().widen_to::<u64>().to_le_bytes()).update(workspace);
    hex(hash.finish())
}

pub enum Runner<'a> {
    Wasm { shim: &'a str },
    Native { target: &'a str },
}

pub struct Wanted<'a> {
    pub krate: CrateId<'a>,
    pub toolchain: &'a str,
    pub runner: Runner<'a>,
}

#[derive(PartialEq, Eq)]
enum Field<'a> { Missing, Value(&'a str), Duplicate }

/// Absence and malformed duplication are distinct, including for forbidden fields.
fn field<'a>(identity: &'a str, name: &str) -> Field<'a> {
    let mut values = identity.lines().filter_map(|line| line.strip_prefix(name)?.strip_prefix(' '));
    match (values.next(), values.next()) {
        (None, _) => Field::Missing,
        (Some(value), None) => Field::Value(value),
        (Some(_), Some(_)) => Field::Duplicate,
    }
}

impl Wanted<'_> {
    fn accepts(&self, identity: &str) -> bool {
        use Field::{Duplicate, Missing, Value};
        if identity.lines().next() != Some("blaze build-script module 3")
            || field(identity, "name") != Value(self.krate.name)
            || field(identity, "version") != Value(self.krate.version)
            || field(identity, "deps") != Value(self.krate.deps)
            || field(identity, "toolchain") != Value(self.toolchain)
            || field(identity, "builder") != Value("bootstrap")
        { return false; }
        let sources = [field(identity, "checksum"), field(identity, "git"), field(identity, "files")];
        if sources.contains(&Duplicate) || sources.iter().filter(|source| matches!(source, Value(_))).count() != 1 { return false; }
        let source = match self.krate.source {
            Source::Registry { checksum } => sources[0] == Value(checksum),
            Source::Git { commit, path } => matches!(sources[1], Value(source) if source.split_once(' ') == Some((commit, path))),
            Source::Path { files_sha256 } => sources[2] == Value(files_sha256),
        };
        let Value(features) = field(identity, "features") else { return false };
        let features_match = self.krate.features.iter().all(|want| features.split(',').any(|got| got == *want))
            && features.split(',').filter(|got| !got.is_empty()).all(|got| self.krate.features.contains(&got));
        let runner = match self.runner {
            Runner::Wasm { shim } => field(identity, "shim") == Value(shim) && field(identity, "target") == Missing,
            Runner::Native { target } => field(identity, "target") == Value(target) && field(identity, "shim") == Missing,
        };
        source && features_match && runner
    }
}

/// Canonical required lines, used directly for task subjects and missing-record diagnostics.
/// Features are visited in lexical order without allocating a sorted copy.
impl fmt::Display for Wanted<'_> {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(out, "blaze build-script module 3\nname {}\nversion {}", self.krate.name, self.krate.version)?;
        match self.krate.source {
            Source::Registry { checksum } => writeln!(out, "checksum {checksum}")?,
            Source::Git { commit, path } => writeln!(out, "git {commit} {path}")?,
            Source::Path { files_sha256 } => writeln!(out, "files {files_sha256}")?,
        }
        write!(out, "deps {}\nfeatures ", self.krate.deps)?;
        let mut previous = None;
        while let Some(next) = self.krate.features.iter().copied().filter(|feature| previous.is_none_or(|before| *feature > before)).min() {
            if previous.is_some() { out.write_char(',')?; }
            out.write_str(next)?;
            previous = Some(next);
        }
        writeln!(out, "\ntoolchain {}", self.toolchain)?;
        match self.runner {
            Runner::Wasm { shim } => writeln!(out, "shim {shim}")?,
            Runner::Native { target } => writeln!(out, "target {target}")?,
        }
        out.write_str("builder bootstrap\n")
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum Refused {
    IdentityKey { record: usize },
    Ambiguous { first: [u8; 64], second: [u8; 64] },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Selection {
    key: HexDigest,
    native: bool,
}

impl Selection {
    pub fn key(&self) -> &str { self.key.as_str() }
    pub fn native(&self) -> bool { self.native }
    pub fn with_digest(self, digest: &str) -> Option<Selected> {
        Some(Selected { selection: self, digest: hex_digest(digest.as_bytes())? })
    }
}

/// The complete module task proof. Its fixed-size key and digest cross a worker or record
/// boundary without an allocator or any source-dependent runtime lookup.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Selected {
    selection: Selection,
    digest: HexDigest,
}

impl Selected {
    pub fn key(&self) -> &str { self.selection.key() }
    pub fn native(&self) -> bool { self.selection.native() }
    pub fn digest(&self) -> &str { self.digest.as_str() }
    pub fn encode(&self) -> [u8; 129] {
        let mut bytes = [0; 129];
        bytes[0] = if self.native() { b'n' } else { b'w' };
        bytes[1..65].copy_from_slice(&self.selection.key.0);
        bytes[65..].copy_from_slice(&self.digest.0);
        bytes
    }
    /// Restore a proof from an authenticated task record, never from a module directory.
    pub fn from_task(payload: &[u8]) -> Option<Self> {
        let (&runner, bytes) = payload.split_first()?;
        if !matches!(runner, b'n' | b'w') || bytes.len() != 128 { return None; }
        let (key, digest) = bytes.split_at(64);
        Some(Self { selection: Selection { key: hex_digest(key)?, native: runner == b'n' }, digest: hex_digest(digest)? })
    }
}

pub fn select<'a>(wanted: &Wanted<'_>, records: impl IntoIterator<Item = (&'a str, &'a str)>) -> Result<Option<Selection>, Refused> {
    let mut selected: Option<Selection> = None;
    for (record, (key, identity)) in records.into_iter().enumerate() {
        if !wanted.accepts(identity) { continue; }
        let key = hex_digest(key.as_bytes()).ok_or(Refused::IdentityKey { record })?;
        if key.0 != digest_hex(identity.as_bytes()) { return Err(Refused::IdentityKey { record }); }
        if let Some(first) = selected {
            return Err(Refused::Ambiguous { first: first.key.min(key).0, second: first.key.max(key).0 });
        }
        selected = Some(Selection { key, native: matches!(wanted.runner, Runner::Native { .. }) });
    }
    Ok(selected)
}

pub fn digest_hex(bytes: &[u8]) -> [u8; 64] {
    hex(blaze_sha256::digest(bytes))
}

fn hex(bytes: [u8; 32]) -> [u8; 64] {
    let mut out = [0; 64];
    let digit = |nibble| if nibble < 10 { b'0' + nibble } else { b'a' + nibble - 10 };
    for (slot, nibble) in out.iter_mut().zip(bytes.into_iter().flat_map(|byte| [byte >> 4, byte & 15])) {
        *slot = digit(nibble);
    }
    out
}

fn hex_digest(bytes: &[u8]) -> Option<HexDigest> {
    if !bytes.iter().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(b)) { return None; }
    bytes.try_into().ok().map(HexDigest)
}

/// A fixed-size digest whose constructors admit only lowercase hexadecimal ASCII.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct HexDigest([u8; 64]);

impl HexDigest {
    pub fn parse(text: &str) -> Option<Self> { hex_digest(text.as_bytes()) }
    pub fn with_workspace(self, workspace: Option<&[u8]>) -> Self { Self(path_source(self.0, workspace)) }
    pub fn of(bytes: &[u8]) -> Self { Self(digest_hex(bytes)) }
    pub(crate) fn from_sha256(bytes: [u8; 32]) -> Self { Self(hex(bytes)) }
    pub fn as_str(&self) -> &str {
        // SAFETY: the private bytes come only from `hex` or the checked `hex_digest` parser,
        // both of which admit ASCII hex digits exclusively. No mutable bytes are exposed.
        unsafe { core::str::from_utf8_unchecked(&self.0) }
    }
}

/// A bootstrap module's source and dependency identity.
#[derive(Clone, Debug)]
pub struct CrateId<'s> {
    pub name: &'s str,
    pub version: &'s str,
    /// Where its sources are, as what pins their bytes.
    pub source: Source<'s>,
    /// The dependencies the lock pins under this program.
    pub deps: &'s str,
    /// The features the host side of the graph unifies to.
    pub features: &'s [&'s str],
}

/// What pins a bootstrap module's source bytes.
#[derive(Clone, Copy, Debug)]
pub enum Source<'s> {
    /// From a registry: the lock's `checksum` of the package (`checksum <hex>`).
    Registry { checksum: &'s str },
    /// From a git repository: the commit the lock pins and the package's folder in it, `.` at
    /// the root (`git <commit> <path>`).
    Git { commit: &'s str, path: &'s str },
    /// A workspace or path package: the source-file digest of its folder as it is
    /// (`files <hex>`), so an edit to the macro crate makes a new module.
    Path { files_sha256: &'s str },
}

pub mod lock;
