//! The D52 view of a nested build script's outputs. Cargo's sandboxed bootstrap build
//! and Blaze's Script record are compared in this form, independent of their OUT_DIRs.
#![blaze::sans_io]

use core::fmt;

use blaze_ids::{ContentId, Hasher};

use crate::{OutputsRef, instr::SearchKind, module_record::Digest};

const COMPILE_DOMAIN: &str = "blaze d52 producer compile inputs 1";
const SEARCH_DOMAIN: &str = "blaze d52 producer search paths 1";
const FEATURES_DOMAIN: &str = "blaze d52 producer features 1";

/// The compiler and its dependent build script use this logical root for an admitted output.
/// The bootstrap lays the sealed files there. Blaze serves the same spelling in the guest.
pub const ROOT_PREFIX: &str = "/tmp/blaze-d52-out-";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Receipt {
    pub compile: ContentId,
    pub search: ContentId,
}

impl Receipt {
    pub fn directory(self) -> Digest {
        Digest::from_content(self.compile)
    }
}

/// The WASI dependency source and output projection a consuming D52 module was compiled with.
/// The source pin is from the module's resolved dependency closure.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Binding<'a> {
    pub name: &'a str,
    pub version: &'a str,
    pub source: &'a str,
    pub features: Digest,
    pub outputs: Receipt,
}

impl fmt::Display for Binding<'_> {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        let compile = Digest::from_content(self.outputs.compile);
        let search = Digest::from_content(self.outputs.search);
        writeln!(out, "producer-output-wasi {} {} {} {} {} {}", self.name, self.version, self.source, self.features.text(), compile.text(), search.text())
    }
}

impl<'a> Binding<'a> {
    pub fn in_identity(identity: &'a str) -> impl Iterator<Item = Option<Self>> + 'a {
        identity.lines().filter_map(|line| line.strip_prefix("producer-output-wasi ")).map(|line| {
            let mut fields = line.split(' ');
            let name = fields.next().filter(|field| !field.is_empty())?;
            let version = fields.next().filter(|field| !field.is_empty())?;
            let source = fields.next().filter(|field| !field.is_empty())?;
            let features = Digest::parse(fields.next()?)?;
            let compile = Digest::parse(fields.next()?)?.content();
            let search = Digest::parse(fields.next()?)?.content();
            fields.next().is_none().then_some(Self { name, version, source, features, outputs: Receipt { compile, search } })
        })
    }
}

/// The producer unit's features, in the resolver's sorted order, once each.
pub fn features<'a>(names: impl IntoIterator<Item = &'a str>) -> Digest {
    let mut hash = Hasher::new(FEATURES_DOMAIN);
    for name in names {
        hash.part(name.as_bytes());
    }
    Digest::from_content(hash.finish())
}

/// Borrowed outputs in directive order, with generated files sorted by relative path.
/// C requests and stored/native files are admitted by other runners.
pub struct Observed<'a, C, E, F, S> {
    pub out_dir: &'a str,
    pub cfgs: C,
    pub env: E,
    pub files: F,
    pub search: S,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refused<'a> {
    OutDir(&'a str),
    File(&'a str),
    Env { name: &'a str, value: &'a str },
    Cfg { name: &'a str, value: &'a str },
    Search(&'a str),
    SearchDirectory(&'a str),
    StoredFiles,
    Native,
}

impl fmt::Display for Refused<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Self::OutDir(root) => write!(f, "{root:?} is not a valid producer OUT_DIR"),
            Self::File(path) => write!(f, "generated file {path:?} does not name one ordered file under OUT_DIR"),
            Self::Env { name, value } => write!(f, "rustc-env {name} contains an output path that cannot be normalized: {value:?}"),
            Self::Cfg { name, value } => write!(f, "rustc-cfg {name} contains an output path that cannot be normalized: {value:?}"),
            Self::Search(path) => write!(f, "rustc-link-search path {path:?} is outside the producer's OUT_DIR"),
            Self::SearchDirectory(path) => write!(f, "rustc-link-search directory {path:?} has no directory proof in the producer's outputs"),
            Self::StoredFiles => f.write_str("the producer's OUT_DIR contains stored files"),
            Self::Native => f.write_str("the producer's outputs require native compilation or linking"),
        }
    }
}

impl core::error::Error for Refused<'_> {}

/// A scalar or one rooted output path. Embedded path syntax needs a representation of its
/// own before it can be transported with the same spelling on both sides.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Value<'a> {
    Scalar(&'a str),
    OutputPath(&'a str),
}

impl<'a> Value<'a> {
    pub fn of(value: &'a str, out_dir: &str) -> Option<Self> {
        if let Some(relative) = under(value, out_dir) {
            Some(Self::OutputPath(relative))
        } else if value.contains(out_dir) {
            None
        } else {
            Some(Self::Scalar(value))
        }
    }

    fn hash(self, hasher: &mut Hasher) {
        match self {
            Self::Scalar(text) => { hasher.update(b"s").part(text.as_bytes()); }
            Self::OutputPath(path) => { hasher.update(b"p").part(path.as_bytes()); }
        }
    }
}

fn relative(path: &str) -> bool {
    !path.is_empty() && !path.contains(['\\', '\n', '\r', '\0']) && path.split('/').all(|part| !matches!(part, "" | "." | ".."))
}

/// An output-relative path with no climbing. Empty denotes the OUT_DIR itself.
pub fn under<'a>(path: &'a str, out_dir: &str) -> Option<&'a str> {
    let suffix = path.strip_prefix(out_dir)?;
    if suffix.is_empty() {
        return Some("");
    }
    let relative_path = suffix.strip_prefix('/')?;
    relative(relative_path).then_some(relative_path)
}

pub fn admit<'a, C, E, F, S>(offered: Observed<'a, C, E, F, S>) -> Result<Receipt, Refused<'a>>
where
    C: IntoIterator<Item = (&'a str, Option<&'a str>)>,
    E: IntoIterator<Item = (&'a str, &'a str)>,
    F: IntoIterator<Item = (&'a str, &'a [u8])>,
    S: IntoIterator<Item = (SearchKind, &'a str)>,
{
    let Observed { out_dir, cfgs, env, files, search } = offered;
    if !out_dir.strip_prefix('/').is_some_and(relative) {
        return Err(Refused::OutDir(out_dir));
    }
    let mut compile = Hasher::new(COMPILE_DOMAIN);
    for (name, value) in cfgs {
        compile.update(b"c").part(name.as_bytes());
        if let Some(value) = value {
            Value::of(value, out_dir).ok_or(Refused::Cfg { name, value })?.hash(&mut compile);
        } else {
            compile.update(b"-");
        }
    }
    for (name, value) in env {
        compile.update(b"e").part(name.as_bytes());
        Value::of(value, out_dir).ok_or(Refused::Env { name, value })?.hash(&mut compile);
    }
    let mut previous = None;
    for (path, bytes) in files {
        if !relative(path) || previous.is_some_and(|before| before >= path) {
            return Err(Refused::File(path));
        }
        compile.update(b"f").part(path.as_bytes()).part(bytes);
        previous = Some(path);
    }
    let mut found = Hasher::new(SEARCH_DOMAIN);
    for (kind, path) in search {
        let path = under(path, out_dir).ok_or(Refused::Search(path))?;
        found.update(&[kind as u8]).part(path.as_bytes());
    }
    Ok(Receipt { compile: compile.finish(), search: found.finish() })
}

pub fn from_record<'a>(record: &OutputsRef<'a>) -> Result<Receipt, Refused<'a>> {
    if record.stored().next().is_some() {
        return Err(Refused::StoredFiles);
    }
    if record.c_requests().next().is_some() || record.link_libs().next().is_some() || record.link_args().next().is_some() {
        return Err(Refused::Native);
    }
    for (_, path) in record.link_search() {
        let relative_path = under(path, record.out_dir()).ok_or(Refused::Search(path))?;
        if !relative_path.is_empty() && !record.files().any(|(file, _)| inside_directory(file, relative_path)) {
            return Err(Refused::SearchDirectory(path));
        }
    }
    admit(Observed { out_dir: record.out_dir(), cfgs: record.cfgs(), env: record.env(), files: record.files(), search: record.link_search() })
}

pub fn inside_directory(file: &str, directory: &str) -> bool {
    file.strip_prefix(directory).is_some_and(|tail| tail.starts_with('/'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bind_the_compiled_view_and_search_view_separately() {
        let seen = |out_dir, env, path, contents, search: bool| admit(Observed {
            out_dir,
            cfgs: [("has_code", None)],
            env: [("SEARCH_PATH", env)],
            files: [("found/generated.rs", contents)],
            search: search.then_some((SearchKind::Native, path)),
        }).unwrap();
        let host = seen("/bootstrap/out", "/bootstrap/out/found", "/bootstrap/out/found", b"const N: u8 = 4;", true);
        let guest = seen("/target/debug/build/dep/out", "/target/debug/build/dep/out/found", "/target/debug/build/dep/out/found", b"const N: u8 = 4;", true);
        assert_eq!(host, guest);
        let no_search = seen("/bootstrap/out", "/bootstrap/out/found", "/bootstrap/out/found", b"const N: u8 = 4;", false);
        assert_eq!(host.compile, no_search.compile);
        assert_ne!(host.search, no_search.search);
        assert_ne!(host.compile, seen("/bootstrap/out", "/bootstrap/out/found", "/bootstrap/out/found", b"const N: u8 = 5;", true).compile);
        assert_eq!(Value::of("/bootstrap/out/../escape", "/bootstrap/out"), None);
        assert_eq!(Value::of("prefix:/bootstrap/out/found", "/bootstrap/out"), None);
        assert!(under("/bootstrap/outer", "/bootstrap/out").is_none());
        let binding = Binding { name: "dep", version: "0.1.0", source: "files:pinned", features: features(["std"]), outputs: host };
        let identity = std::format!("{binding}");
        assert!(Binding::in_identity(&identity).eq([Some(binding)]));
        assert_ne!(binding.features, features([]));
    }
}
