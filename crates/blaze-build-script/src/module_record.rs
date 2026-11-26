//! The source receipt shared by D52 and its build-script module lookup.

use core::fmt;

/// The checkout sweep's file-content identity, shared with the bootstrap read receipt.
pub const CHECKOUT_FILE_DOMAIN: &str = "blaze v2 vfs file 1";

/// A 32-byte digest written as 64 lower-case hexadecimal digits in the D52 store. Record
/// keys and executable digests are SHA-256, and checkout file digests use the sweep's domain.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Digest([u8; 64]);

impl Digest {
    pub fn parse(hex: &str) -> Option<Self> {
        let bytes: [u8; 64] = hex.as_bytes().try_into().ok()?;
        bytes.iter().all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(byte)).then_some(Self(bytes))
    }

    pub fn text(&self) -> &str {
        // The parser admits only ASCII hexadecimal bytes.
        core::str::from_utf8(&self.0).expect("the digest was checked as ASCII")
    }

    /// The digest of bytes as the checkout sweep observes them.
    pub fn checkout_file(bytes: &[u8]) -> Self {
        Self::from_content(blaze_ids::ContentId::of(CHECKOUT_FILE_DOMAIN, bytes))
    }

    pub fn from_content(id: blaze_ids::ContentId) -> Self {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        let mut text = [0; 64];
        for (byte, pair) in id.0.into_iter().zip(text.chunks_exact_mut(2)) {
            pair[0] = HEX[usize::from(byte >> 4)];
            pair[1] = HEX[usize::from(byte & 15)];
        }
        Self(text)
    }

    pub fn content(self) -> blaze_ids::ContentId {
        let nibble = |byte| if byte <= b'9' { byte - b'0' } else { byte - b'a' + 10 };
        let mut id = [0; 32];
        for (pair, byte) in self.0.chunks_exact(2).zip(&mut id) {
            *byte = (nibble(pair[0]) << 4) | nibble(pair[1]);
        }
        blaze_ids::ContentId(id)
    }
}

/// The same source receipt spelling each D52 module has used since module format 3.
pub const FORMAT: &str = "blaze build-script module 3";

/// The input source whose bytes the module implements.
#[derive(Clone, Copy)]
pub enum Source<'s> {
    Checkout { files: &'s str },
    Registry { checksum: &'s str },
    Git { commit: &'s str, path: &'s str },
}

impl fmt::Display for Source<'_> {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Self::Checkout { files } => write!(out, "files {files}"),
            Self::Registry { checksum } => write!(out, "checksum {checksum}"),
            Self::Git { commit, path } => write!(out, "git {commit} {path}"),
        }
    }
}

/// One checkout file the compiler read or whose contents control its isolated resolution.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CheckoutRead<'s> {
    pub path: &'s str,
    pub content: Digest,
}

/// A confined checkout spelling: the package root is represented by `.`.
pub fn checkout_path(path: &str) -> bool {
    path == "." || (!path.is_empty() && !path.contains(['\\', '\n', '\r', '\0']) && path.split('/').all(|part| !matches!(part, "" | "." | "..")))
}

impl<'s> CheckoutRead<'s> {
    pub fn inputs(record: &'s str) -> impl Iterator<Item = Option<Self>> + 's {
        record.lines().filter_map(|line| line.strip_prefix("checkout-read ")).map(|line| {
            let (digest, path) = line.split_once(' ')?;
            (checkout_path(path) && path != ".").then_some(Self { path, content: Digest::parse(digest)? })
        })
    }
}

impl fmt::Display for CheckoutRead<'_> {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(out, "checkout-read {} {}", self.content.text(), self.path)
    }
}

/// Where the checkout placed the package this receipt was made for.
pub fn checkout_package(record: &str) -> Option<&str> {
    let path = one(record, "checkout-package").ok()??;
    checkout_path(path).then_some(path)
}

/// The package and location a checkout receipt belongs to.
pub fn checkout_owner(record: &str) -> Option<(&str, &str, &str)> {
    Some((one(record, "name").ok()??, one(record, "version").ok()??, checkout_package(record)?))
}

/// The package and its dependencies as they are given to the module compiler.
pub struct Identity<'s> {
    pub name: &'s str,
    pub version: &'s str,
    pub source: Source<'s>,
    pub dependencies: Digest,
    pub features: &'s [&'s str],
    pub toolchain: &'s str,
}

impl fmt::Display for Identity<'_> {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(out, "{FORMAT}\nname {}\nversion {}\n{}\ndeps {}\nfeatures {}\ntoolchain {}", self.name, self.version, self.source, self.dependencies.text(), Features(self.features), self.toolchain)
    }
}

struct Features<'s>(&'s [&'s str]);

impl fmt::Display for Features<'_> {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (index, feature) in self.0.iter().enumerate() {
            if index != 0 {
                out.write_str(",")?;
            }
            out.write_str(feature)?;
        }
        Ok(())
    }
}

/// The declared compiler and target that can execute a module.
#[derive(Clone, Copy)]
pub enum Runner<'s> {
    Wasi { shim: &'s str, recipe: &'s str },
    Native { target: &'s str, clang: Digest, sdk_settings: Digest },
}

/// A field may occur once, or be absent. A duplicate never establishes either fact.
fn one<'s>(record: &'s str, field: &str) -> Result<Option<&'s str>, ()> {
    let mut values = record.lines().filter_map(|line| line.strip_prefix(field)?.strip_prefix(' '));
    let first = values.next();
    if values.next().is_some() { Err(()) } else { Ok(first) }
}

/// A lookup for an exact revision, including the inherited and reached source inputs supplied
/// by its caller in `identity`. Feature names are sorted and unique on both sides.
pub struct Request<'s> {
    pub identity: Identity<'s>,
    pub runner: Runner<'s>,
}

impl Request<'_> {
    pub fn executable(&self) -> Executable {
        match self.runner {
            Runner::Wasi { .. } => Executable::Wasm,
            Runner::Native { .. } => Executable::Native,
        }
    }

    /// Whether this record answers the exact request. Listing order cannot select a revision.
    pub fn accepts(&self, record: &str) -> bool {
        let identity = &self.identity;
        if record.lines().next() != Some(FORMAT)
            || one(record, "name") != Ok(Some(identity.name))
            || one(record, "version") != Ok(Some(identity.version))
            || one(record, "deps") != Ok(Some(identity.dependencies.text()))
            || one(record, "toolchain") != Ok(Some(identity.toolchain))
            || one(record, "builder") != Ok(Some("bootstrap"))
        {
            return false;
        }
        let Ok(Some(features)) = one(record, "features") else { return false };
        if !features.split_terminator(',').eq(identity.features.iter().copied()) || features.ends_with(',') {
            return false;
        }
        let Ok(files) = one(record, "files") else { return false };
        let Ok(checksum) = one(record, "checksum") else { return false };
        let Ok(git) = one(record, "git") else { return false };
        let source = match identity.source {
            Source::Checkout { files: expected } => files == Some(expected) && checksum.is_none() && git.is_none(),
            Source::Registry { checksum: expected } => checksum == Some(expected) && files.is_none() && git.is_none(),
            Source::Git { commit, path } => git.and_then(|git| git.split_once(' ')) == Some((commit, path)) && files.is_none() && checksum.is_none(),
        };
        source && match self.runner {
            Runner::Wasi { shim, recipe } => one(record, "target") == Ok(None)
                && one(record, "shim") == Ok(Some(shim))
                && one(record, crate::wasi_std::IDENTITY_FIELD) == Ok(Some(recipe)),
            Runner::Native { target, clang, sdk_settings } => one(record, "target") == Ok(Some(target))
                && one(record, "shim") == Ok(None)
                && one(record, crate::wasi_std::IDENTITY_FIELD) == Ok(None)
                && one(record, "linker").ok().flatten().and_then(|line| line.strip_prefix("clang ")) == Some(clang.text())
                && one(record, "sdk").ok().flatten().and_then(|line| line.strip_prefix("settings ")) == Some(sdk_settings.text()),
        }
    }
}

/// The executable form proved by the module lookup.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Executable {
    Wasm,
    Native,
}

impl Executable {
    pub fn filename(self) -> &'static str {
        match self {
            Self::Wasm => "module.wasm",
            Self::Native => "script.bin",
        }
    }

    pub fn digest_filename(self) -> &'static str {
        match self {
            Self::Wasm => "wasm-sha256",
            Self::Native => "bin-sha256",
        }
    }
}

/// The module key and digest passed from its lookup to the runtime. Decoding is for a
/// verified task payload. The driver still checks the executable bytes before running them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Selected {
    key: Digest,
    executable: Executable,
    bytes: Digest,
}

impl Selected {
    pub fn from_lookup(key: Digest, executable: Executable, bytes: Digest) -> Self {
        Self { key, executable, bytes }
    }

    pub fn key(&self) -> &str { self.key.text() }
    pub fn digest(&self) -> &str { self.bytes.text() }
    pub fn executable(&self) -> Executable { self.executable }

    pub fn encode(self) -> [u8; 129] {
        let mut record = [0; 129];
        record[0] = match self.executable { Executable::Wasm => b'w', Executable::Native => b'n' };
        record[1..65].copy_from_slice(&self.key.0);
        record[65..].copy_from_slice(&self.bytes.0);
        record
    }

    pub fn decode(record: &[u8]) -> Option<Self> {
        let bytes: &[u8; 129] = record.try_into().ok()?;
        let executable = match bytes[0] { b'w' => Executable::Wasm, b'n' => Executable::Native, _ => return None };
        let key = Digest::parse(core::str::from_utf8(&bytes[1..65]).ok()?)?;
        let contents = Digest::parse(core::str::from_utf8(&bytes[65..]).ok()?)?;
        Some(Self::from_lookup(key, executable, contents))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const OLD: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const NEW: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    fn identity(files: &str) -> Identity<'_> {
        Identity { name: "package_b", version: "0.1.0", source: Source::Checkout { files }, dependencies: Digest::parse(OLD).unwrap(), features: &["a", "b"], toolchain: "nightly" }
    }

    #[test]
    fn checkout_module_reads_exact_revision_and_round_trips_proof() {
        let ask = Request { identity: identity(NEW), runner: Runner::Wasi { shim: "shim", recipe: "recipe" } };
        let record = |files| std::format!("{}shim shim\n{} recipe\nbuilder bootstrap\n", identity(files), crate::wasi_std::IDENTITY_FIELD);
        let old = record(OLD);
        let new = record(NEW);
        let accepted: std::vec::Vec<_> = [old.as_str(), new.as_str()].into_iter().filter(|record| ask.accepts(record)).collect();
        assert_eq!(accepted, [new.as_str()]);
        assert!(!ask.accepts(&std::format!("{old}files {NEW}\n")));
        assert!(!ask.accepts(&record(NEW).replace("deps a", "deps b")));
        assert!(!ask.accepts(&record(NEW).replace("features a,b", "features b,a")));
        let proof = Selected::from_lookup(Digest::parse(OLD).unwrap(), Executable::Wasm, Digest::parse(NEW).unwrap());
        let encoded = proof.encode();
        assert_eq!(Selected::decode(&encoded), Some(proof));
        let mut invalid = encoded;
        invalid[1] = b'/';
        assert!(Selected::decode(&invalid).is_none());
    }
}
