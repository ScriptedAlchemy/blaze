//! The module record: a proc-macro crate built for `wasm32-wasip1` with blaze's shim linked in,
//! found by the crate's identity. A bootstrap builder writes it (decision D52:
//! scripts/src/bin/proc_macro_modules.rs), never `blaze`; a call whose record is absent is
//! [`ModuleError::Missing`], a finding naming the crate.
//!
//! The key is the SHA-256 of [`identity`]: the name, version and [`Source`] of the crate (the
//! lock's checksum, a git commit and path, or a path package's [`files_digest`]), the
//! dependencies the lock pins under it ([`LockedPackages::closure`]: two locks pinning another syn under
//! one serde_derive are two modules),
//! its features (the host side's unified set, sorted), the toolchain, the digests of the target
//! JSON and of the shim, and the builder. The builder computes the same text; the example
//! `tempo_derives` finds each built module by the key computed here.
//!
//! `<store>/proc-macro/modules/<key>/` holds `identity`, `module.wasm`, `wasm-sha256`,
//! `macros` (one line per macro: its index in the module, `derive`, `attr` or `bang`, its name,
//! a derive's helpers joined by commas) and `decls` (the exported global holding the list).
//! The runtime keeps the module's machine code apart, in the store's one compiled folder
//! `<store>/proc-macro/compiled/<code key>` and its `.seal`, keyed by the wasm's digest and the
//! runtime ([`COMPILED_DIR`]; `host.rs`, "Compiled records").

use std::path::{Path, PathBuf};

use blaze_build_script::served::{InPackage, in_package};
use blaze_expand::ProcKind;
use blaze_manifest::lock::Lock;

/// The target a module is built for.
pub const TARGET_JSON: &[u8] = include_bytes!("../guest/wasm32-wasip1-macro.json");
/// The shim linked into each module.
pub const SHIM: &[u8] = include_bytes!("../guest/shim.rs");

pub use blaze_manifest::bootstrap::{CrateId, Source};

/// Why a macro crate's dependency closure could not be read from a lock. A malformed lock never
/// reaches here: blaze-manifest's one reader ([`Lock::read`]) refuses it first, naming the line
/// of the entry (rule 30: one lock reader; rule 3: never a package named "").
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum ClosureError {
    InvalidDependency { package: usize },
    /// The lock has no package of this name and version.
    NotLocked { name: String, version: String },
    /// A path package in the closure: its sources are pinned by nothing in the lock.
    Unpinned { name: String, version: String },
}

impl std::fmt::Display for ClosureError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ClosureError::InvalidDependency { package } => write!(f, "dependency index {package} is outside the parsed lock"),
            ClosureError::NotLocked { name, version } => write!(f, "the lock has no package {name} {version}"),
            ClosureError::Unpinned { name, version } => {
                write!(f, "{name} {version} is a path package under a proc-macro crate: the lock pins nothing of its sources")
            }
        }
    }
}

/// A lock's packages as blaze-manifest read them ([`Lock::read`], read once a command), under
/// the source corrections: what [`LockedPackages::closure`] walks for every macro crate of the
/// lock.
pub struct LockedPackages<'l, 'a> {
    lock: &'l Lock<'a>,
    /// The source corrections (D58): a package a row corrects is pinned by its checksum and
    /// the row's digest.
    corrections: blaze_fetch::correct::Table<std::alloc::Global>,
}

impl<'l, 'a> LockedPackages<'l, 'a> {
    /// The packages of `lock`, under the checked-in source corrections.
    pub fn of(lock: &'l Lock<'a>) -> LockedPackages<'l, 'a> {
        LockedPackages::of_with(lock, blaze_fetch::correct::Table::checked_in(std::alloc::Global))
    }

    /// [`LockedPackages::of`] under another table of corrections (a test's edited row).
    pub fn of_with(lock: &'l Lock<'a>, corrections: blaze_fetch::correct::Table<std::alloc::Global>) -> LockedPackages<'l, 'a> {
        LockedPackages { lock, corrections }
    }

    /// The digest of the macro crate `name` `version`'s dependency closure as the lock pins it
    /// (the `deps` line of [`identity`]): every package reachable from it through the lock's
    /// `dependencies` lists, the crate itself left out, each as `name version pin` (the
    /// checksum, else the source as written), sorted, one a line, SHA-256. The lock lists every
    /// kind of dependency of every feature, so the closure is a superset of what a build links,
    /// keyed from the lock's text alone. The builder computes the same
    /// (`scripts/src/bin/proc_macro_modules.rs`). Two packages of one name and version (two
    /// sources): the first in the file's order is the root, as it was before (`Lock::named`
    /// keeps the file's order within a name).
    pub fn closure(&self, name: &str, version: &str) -> Result<String, ClosureError> {
        self.closure_lines(name, version, None, *self.lock.packages.allocator()).map(|lines| blaze_manifest::bootstrap::lock::digest(&lines).as_str().to_owned())
    }

    /// The script's lock closure: only declared build-dependency roots, then all packages
    /// reachable from those roots. Normal path dependencies do not enter a script's key.
    pub fn script_closure(&self, name: &str, version: &str, build_dependencies: &[&str]) -> Result<crate::script_module::HexDigest, ClosureError> {
        self.closure_lines(name, version, Some(build_dependencies), *self.lock.packages.allocator()).map(|lines| blaze_manifest::bootstrap::lock::digest(&lines))
    }

    /// A checkout script whose local dependencies have captured portable source pins.
    pub fn script_closure_at(&self, root: blaze_manifest::lock::LockId, build_dependencies: &[&str], paths: &[blaze_manifest::bootstrap::lock::PathPin]) -> Result<crate::script_module::HexDigest, blaze_manifest::bootstrap::lock::Refused> {
        blaze_manifest::bootstrap::lock::closure_at(self.lock, root, Some(build_dependencies), paths, *self.lock.packages.allocator(), |name, version, pin| self.corrections.pin(name, version, pin))
            .map(|lines| blaze_manifest::bootstrap::lock::digest(&lines))
    }

    /// The same closure as inspectable pin lines, so a bootstrap can verify what its compiler
    /// actually linked before publishing the module record.
    pub fn closure_lines<'s>(&self, name: &str, version: &str, build_dependencies: Option<&[&str]>, alloc: blaze_own::Alloc<'s>) -> Result<blaze_own::AVec<'s, blaze_own::AString<'s>>, ClosureError> {
        blaze_manifest::bootstrap::lock::closure_lines(self.lock, name, version, build_dependencies, alloc, |name, version, pin| self.corrections.pin(name, version, pin))
            .map_err(|why| match why {
                blaze_manifest::bootstrap::lock::Refused::NotLocked => ClosureError::NotLocked { name: name.to_owned(), version: version.to_owned() },
                blaze_manifest::bootstrap::lock::Refused::Unpinned { package } => match self.lock.packages.get(package) {
                    Some(package) => ClosureError::Unpinned { name: package.name.as_str().to_owned(), version: package.version.to_string() },
                    None => ClosureError::InvalidDependency { package },
                },
                blaze_manifest::bootstrap::lock::Refused::OutsideLock { package } => ClosureError::InvalidDependency { package },
            })
    }
}

/// The digest a path package's sources are pinned by: SHA-256 over its files, in the byte
/// order of their paths relative to the package's folder, each as the path, a zero byte, its
/// length as 8 little-endian bytes and its bytes. `files` holds every regular file of the
/// package by the one package rule (blaze-build-script `served::in_package`); the builder and
/// blaze walk it alike (`examples/tempo_derives.rs` computes it from the checkout and finds the
/// builder's module by it).
pub fn files_digest(files: &mut [(String, Vec<u8>)]) -> String {
    files.sort_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));
    let mut all = Vec::new();
    for (path, bytes) in files.iter() {
        all.extend_from_slice(path.as_bytes());
        all.push(0);
        all.extend_from_slice(&(bytes.len() as u64).to_le_bytes());
        all.extend_from_slice(bytes);
    }
    sha256_hex(&all)
}

/// The files of the package at `dir` that [`files_digest`] covers, by their paths relative to
/// it (`/`-separated): each entry `served::in_package` takes. Walked with a stack of folders.
pub fn package_files(dir: &Path) -> Result<Vec<(String, Vec<u8>)>, ModuleError> {
    let io = |path: &Path, error: std::io::Error| ModuleError::Io { path: path.to_owned(), error };
    let mut out = Vec::new();
    let mut folders = vec![(dir.to_owned(), String::new())];
    while let Some((folder, relative)) = folders.pop() {
        for entry in std::fs::read_dir(&folder).map_err(|e| io(&folder, e))? {
            let entry = entry.map_err(|e| io(&folder, e))?;
            let name = entry.file_name().to_string_lossy().into_owned();
            let path = entry.path();
            let kind = entry.file_type().map_err(|e| io(&path, e))?;
            let place = if relative.is_empty() { InPackage::Root } else { InPackage::Beneath };
            if !in_package(entry.file_name().as_encoded_bytes(), kind.is_dir(), place) {
                continue;
            }
            let at = if relative.is_empty() { name.clone() } else { format!("{relative}/{name}") };
            if kind.is_dir() {
                folders.push((path, at));
            } else if kind.is_file() {
                out.push((at, std::fs::read(&path).map_err(|e| io(&path, e))?));
            }
        }
    }
    Ok(out)
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// The SHA-256 of `bytes`, as hex.
pub fn sha256_hex(bytes: &[u8]) -> String {
    hex(&blaze_sha256::digest(bytes))
}

/// The crate's identity as text: what the module key is the digest of.
pub fn identity(id: &CrateId<'_>) -> String {
    let mut features: Vec<&str> = id.features.to_vec();
    features.sort_unstable();
    features.dedup();
    let source = match id.source {
        Source::Registry { checksum } => format!("checksum {checksum}"),
        Source::Git { commit, path } => format!("git {commit} {path}"),
        Source::Path { files_sha256 } => format!("files {files_sha256}"),
    };
    format!(
        // Form 3 (2026-11-18): the profile is an input of the module. A macro's own code reads
        // `cfg(debug_assertions)` (tempo's `tempo-precompiles-macros` writes its `*_BYTES`
        // constants only under it), and `check` models cargo's dev profile, where it is on.
        // Form 2's modules were built without it and are another key.
        // Form 4 (2026-11-20): the standard library's crates (core, alloc, std, proc_macro and
        // the rest of std's own workspace) are built WITHOUT debug assertions, and no crate of
        // the module emits UB checks (`-Zub-checks=no`); the macro crate and its dependencies
        // keep `cfg(debug_assertions)`. Form 3's std carried both into every call. The builder
        // (scripts proc_macro_modules.rs) writes the same text.
        // History (2026-11-20): form 4 was first admitted at 21:20 before any module store held
        // it, and every macro of tempo then had "no compiled module in the store" (5,688 errors
        // on the acceptance project); the reader was held at form 3 until the store was rebuilt
        // (81 modules, notes port27/tempo-deps/module-store-f4, 22:00) and measured on tempo at
        // 22:55 (Names 28.3 s to 23.5 s wall, 69.5 s to 54.7 s cpu; the same 14,623 bodies
        // checked). A form of this text moves only together with a store that holds it.
        "blaze proc-macro module 4\nname {}\nversion {}\n{source}\ndeps {}\nfeatures {}\ntoolchain {}\ntarget {}\nshim {}\nprofile debug-assertions std-release ub-checks-off\nbuilder bootstrap\n",
        id.name,
        id.version,
        id.deps,
        features.join(","),
        // The pinned nightly: the bridge's sources and the std a module is built with.
        blaze_pins::TOOLCHAIN,
        sha256_hex(TARGET_JSON),
        sha256_hex(SHIM),
    )
}

/// The module record's key.
pub fn module_key(id: &CrateId<'_>) -> String {
    sha256_hex(identity(id).as_bytes())
}

/// One macro a module exports.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Exported {
    /// Its index in the harness's list, which the shim runs.
    pub index: u32,
    pub kind: ProcKind,
    pub name: String,
    /// A derive's helper attributes.
    pub helpers: Vec<String>,
}

/// A module record: its key, its macros and the digest its `wasm-sha256` states. The module's
/// wasm is NOT held: it is read only by a compile ([`ModuleRecord::wasm`]), on the one load
/// whose compiled record is missing (host.rs `load_now`), checked against the digest there,
/// and dropped with the compile. A load from a compiled record (every command after the
/// first) reads no wasm at all: the record's seal names the digest it was compiled from.
/// (Before, every command read and hashed every macro crate's wasm and kept it to the end:
/// 114 MB in 79 blocks on tempo's 2026-11-18 heap log.)
#[derive(Debug)]
pub struct ModuleRecord {
    pub key: String,
    pub name: String,
    pub version: String,
    /// The digest of `module.wasm` as `wasm-sha256` states it; [`ModuleRecord::wasm`]
    /// checks the bytes against it when it reads them.
    pub wasm_sha256: String,
    pub macros: Vec<Exported>,
    /// The name of the global holding the address of the harness's list.
    pub decls: String,
    /// The record's folder in the module store, `<store>/proc-macro/modules/<key>`.
    pub dir: PathBuf,
    /// The store's folder of compiled code, `<store>/`[`COMPILED_DIR`]: where the runtime keeps
    /// the module's machine code, keyed by what it is made from (host.rs `Runtime::code_key`),
    /// not by this record's key.
    pub compiled: PathBuf,
    /// When `Some`, this proc-macro module is a native host `.dylib`/`.so` executed directly
    /// through `NativeServerPool` (zero Wasmtime, zero `-Zbuild-std`, D175 fast path).
    pub native_dylib: Option<PathBuf>,
}

/// A store's folder of compiled module code, under the store (host.rs, "Compiled records").
pub const COMPILED_DIR: &str = "proc-macro/compiled";

impl ModuleRecord {
    /// The macro `name` of `kind`.
    pub fn find(&self, kind: ProcKind, name: &str) -> Option<&Exported> {
        self.macros.iter().find(|it| it.kind == kind && it.name == name)
    }

    /// The module's wasm, `module.wasm` in its folder, checked against [`Self::wasm_sha256`]:
    /// what a compile reads, once, and drops when it is done. Not kept here (the type's docs).
    pub fn wasm(&self) -> Result<Vec<u8>, ModuleError> {
        let path = self.dir.join("module.wasm");
        let wasm = std::fs::read(&path).map_err(|error| ModuleError::Io { path, error })?;
        let found = sha256_hex(&wasm);
        if found != self.wasm_sha256 {
            return Err(ModuleError::Digest { key: self.key.clone(), held: self.wasm_sha256.clone(), found });
        }
        Ok(wasm)
    }
}

/// Why a module could not be had.
#[derive(Debug)]
pub enum ModuleError {
    /// No record under the crate's key: built by nobody yet.
    Missing { name: String, version: String, key: String },
    /// A file of the record could not be read.
    Io { path: PathBuf, error: std::io::Error },
    /// The record's wasm is not the bytes its digest names.
    Digest { key: String, held: String, found: String },
    /// A line of `macros` that is not one.
    Macros { key: String, line: String },
}

impl std::fmt::Display for ModuleError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ModuleError::Missing { name, version, key } => write!(
                f,
                "{name} {version}: no compiled module in the store or the cache (module key {key}); build it with scripts/src/bin/proc_macro_modules.rs (D52)"
            ),
            ModuleError::Io { path, error } => write!(f, "{}: {error}", path.display()),
            ModuleError::Digest { key, held, found } => {
                write!(f, "module {key}: its wasm-sha256 says {held}, its module.wasm is {found}")
            }
            ModuleError::Macros { key, line } => write!(f, "module {key}: `macros` has the line {line:?}, which lists no macro"),
        }
    }
}

impl std::error::Error for ModuleError {}

fn native_dylib_filename() -> &'static str {
    if cfg!(target_os = "macos") {
        "native.dylib"
    } else if cfg!(target_os = "windows") {
        "native.dll"
    } else {
        "native.so"
    }
}

/// Whether `store` holds `id`'s module, by its key: the native `.dylib`'s digest (zero Wasm!)
/// when present, else the wasm's digest as the record states it.
pub fn find_module(store: &Path, id: &CrateId<'_>) -> Result<(String, String), ModuleError> {
    let key = module_key(id);
    let dir = store.join("proc-macro/modules").join(&key);
    let dylib_digest = dir.join("dylib-sha256");
    if let Ok(held) = std::fs::read(&dylib_digest) {
        if dir.join(native_dylib_filename()).is_file() {
            return Ok((key, String::from_utf8_lossy(&held).trim().to_owned()));
        }
    }
    let path = dir.join("wasm-sha256");
    match std::fs::read(&path) {
        Ok(held) => Ok((key, String::from_utf8_lossy(&held).trim().to_owned())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Err(ModuleError::Missing { name: id.name.to_owned(), version: id.version.to_owned(), key }),
        Err(error) => Err(ModuleError::Io { path, error }),
    }
}

/// The record of `id` in `store` (prefers native `.dylib`/`.so` over `module.wasm` when present).
pub fn read_module(store: &Path, id: &CrateId<'_>) -> Result<ModuleRecord, ModuleError> {
    let key = module_key(id);
    let dir = store.join("proc-macro/modules").join(&key);
    let native_path = dir.join(native_dylib_filename());
    let is_native = dir.join("dylib-sha256").is_file() && native_path.is_file();
    if !is_native && !dir.join("wasm-sha256").exists() {
        return Err(ModuleError::Missing { name: id.name.to_owned(), version: id.version.to_owned(), key });
    }
    let read = |file: &str| -> Result<Vec<u8>, ModuleError> {
        let path = dir.join(file);
        std::fs::read(&path).map_err(|error| ModuleError::Io { path, error })
    };
    let text = |file: &str| -> Result<String, ModuleError> { Ok(String::from_utf8_lossy(&read(file)?).into_owned()) };
    let held = text(if is_native { "dylib-sha256" } else { "wasm-sha256" })?.trim().to_owned();
    let mut macros = Vec::new();
    for line in text("macros")?.lines() {
        let bad = || ModuleError::Macros { key: key.clone(), line: line.to_owned() };
        let mut parts = line.split(' ');
        let index = parts.next().and_then(|it| it.parse().ok()).ok_or_else(bad)?;
        let kind = match parts.next() {
            Some("derive") => ProcKind::Derive,
            Some("attr") => ProcKind::Attr,
            Some("bang") => ProcKind::FnLike,
            _ => return Err(bad()),
        };
        let name = parts.next().filter(|it| !it.is_empty()).ok_or_else(bad)?.to_owned();
        let helpers = parts.next().unwrap_or("").split(',').filter(|it| !it.is_empty()).map(str::to_owned).collect();
        macros.push(Exported { index, kind, name, helpers });
    }
    let decls = if is_native {
        dir.join("decls")
            .is_file()
            .then(|| text("decls").ok())
            .flatten()
            .unwrap_or_default()
            .trim()
            .to_owned()
    } else {
        text("decls")?.trim().to_owned()
    };
    Ok(ModuleRecord {
        key,
        name: id.name.to_owned(),
        version: id.version.to_owned(),
        wasm_sha256: held,
        macros,
        decls,
        dir,
        compiled: store.join(COMPILED_DIR),
        native_dylib: is_native.then_some(native_path),
    })
}

#[cfg(test)]
#[path = "module_lock_tests.rs"]
mod lock_tests;
