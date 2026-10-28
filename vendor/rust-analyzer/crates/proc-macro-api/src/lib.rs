//! Client-side Proc-Macro crate
//!
//! We separate proc-macro expanding logic to an extern program to allow
//! different implementations (e.g. wasm or dylib loading). And this crate
//! is used to provide basic infrastructure for communication between two
//! processes: Client (RA itself), Server (the external program)

#![cfg_attr(not(feature = "in-rust-tree"), allow(unused_crate_dependencies))]
#![cfg_attr(
    feature = "in-rust-tree",
    feature(proc_macro_internals, proc_macro_diagnostic, proc_macro_span, rustc_private)
)]
#![allow(internal_features, unused_features)]

#[cfg(feature = "in-rust-tree")]
extern crate rustc_driver as _;

pub mod bidirectional_protocol;
#[cfg(test)]
mod generation_tests;
pub mod legacy_protocol;
pub mod pool;
pub mod process;
pub mod producer_lifetime;
pub mod snapshot_dir;
pub mod transport;

use paths::{AbsPath, AbsPathBuf};
use semver::Version;
use span::{ErasedFileAstId, FIXUP_ERASED_FILE_AST_ID_MARKER, Span};
use std::{
    fmt, io,
    sync::{Arc, Mutex, Weak},
};

use crate::{
    bidirectional_protocol::SubCallback, pool::ProcMacroServerPool, process::ProcMacroServerProcess,
};

/// The versions of the server protocol
pub mod version {
    pub const NO_VERSION_CHECK_VERSION: u32 = 0;
    pub const VERSION_CHECK_VERSION: u32 = 1;
    pub const ENCODE_CLOSE_SPAN_VERSION: u32 = 2;
    pub const HAS_GLOBAL_SPANS: u32 = 3;
    pub const RUST_ANALYZER_SPAN_SUPPORT: u32 = 4;
    /// Whether literals encode their kind as an additional u32 field and idents their rawness as a u32 field.
    pub const EXTENDED_LEAF_DATA: u32 = 5;
    pub const HASHED_AST_ID: u32 = 6;

    /// Current API version of the proc-macro protocol.
    pub const CURRENT_API_VERSION: u32 = HASHED_AST_ID;
}

/// Protocol format for communication between client and server.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ProtocolFormat {
    /// JSON-based legacy protocol (newline-delimited JSON).
    JsonLegacy,
    /// Bidirectional postcard protocol with sub-request support.
    BidirectionalPostcardPrototype,
}

impl fmt::Display for ProtocolFormat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ProtocolFormat::JsonLegacy => write!(f, "json-legacy"),
            ProtocolFormat::BidirectionalPostcardPrototype => {
                write!(f, "bidirectional-postcard-prototype")
            }
        }
    }
}

/// Represents different kinds of procedural macros that can be expanded by the external server.
#[derive(Copy, Clone, Eq, PartialEq, Debug, serde_derive::Serialize, serde_derive::Deserialize)]
pub enum ProcMacroKind {
    /// A macro that derives implementations for a struct or enum.
    CustomDerive,
    /// An attribute-like procedural macro.
    Attr,
    // This used to be called FuncLike, so that's what the server expects currently.
    #[serde(alias = "Bang")]
    #[serde(rename(serialize = "FuncLike", deserialize = "FuncLike"))]
    Bang,
}

/// A handle to proc-macro server process pool which load dylibs with macros (.so or .dll)
/// and runs actual macro expansion functions.
#[derive(Clone)]
pub struct ProcMacroClient {
    /// Currently, the proc macro process expands all procedural macros sequentially.
    ///
    /// That means that concurrent salsa requests may block each other when expanding proc macros,
    /// which is unfortunate, but simple and good enough for the time being.
    epochs: Arc<Mutex<Epochs>>,
    /// New library generations get a pool whose lifetime follows their handles.
    /// This releases native server images without changing either wire protocol.
    spawn_generation: Option<Arc<dyn Fn() -> io::Result<ProcMacroServerPool> + Send + Sync>>,
    snapshot_root: Option<std::path::PathBuf>,
    /// The path to the proc-macro server binary.
    path: AbsPathBuf,
}

/// Only the unused first pool is held strongly. Serving pools belong to consumers.
struct Epochs {
    initial: Option<Arc<ProcMacroServerPool>>,
    current: Weak<ProcMacroServerPool>,
    live: Vec<Weak<ProcMacroServerPool>>,
}

impl Epochs {
    fn new(pool: ProcMacroServerPool) -> Self {
        let pool = Arc::new(pool);
        Self {
            current: Arc::downgrade(&pool),
            live: vec![Arc::downgrade(&pool)],
            initial: Some(pool),
        }
    }
}

impl fmt::Debug for ProcMacroClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProcMacroClient").field("path", &self.path).finish()
    }
}

/// Represents a dynamically loaded library containing procedural macros.
pub struct MacroDylib {
    path: AbsPathBuf,
}

impl MacroDylib {
    /// Creates a new MacroDylib instance with the given path.
    pub fn new(path: AbsPathBuf) -> MacroDylib {
        MacroDylib { path }
    }
}

/// A handle to a specific proc-macro (a `#[proc_macro]` annotated function).
///
/// It exists within the context of a specific proc-macro server -- currently
/// we share a single expander process for all macros within a workspace.
#[derive(Debug, Clone)]
pub struct ProcMacro {
    pool: Arc<ProcMacroServerPool>,
    dylib_path: Arc<AbsPathBuf>,
    name: Box<str>,
    kind: ProcMacroKind,
    dylib_content: Option<[u8; 32]>,
    original_dylib_path: Arc<AbsPathBuf>,
    _snapshot: Arc<LibrarySnapshot>,
}

enum LibrarySnapshot {
    Temporary(stdx::tempfile::NamedTempDir),
    WorkerOwned(std::path::PathBuf),
}

impl LibrarySnapshot {
    fn copy(source: &AbsPath, root: Option<&std::path::Path>) -> io::Result<(Self, [u8; 32])> {
        // Open first: a missing producer must not leave an empty generation.
        let mut input = std::fs::File::open(source)?;
        // The daemon knows this private worker home and removes it only after
        // killing/waiting for that worker group. This covers stop without Drop.
        if let Some(root) = root {
            static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let dir = snapshot_dir::prepare(root, std::process::id())?;
            let path = dir.join(format!(
                "generation-{}",
                NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            ));
            let mut file = std::fs::File::options().write(true).create_new(true).open(&path)?;
            let snapshot = Self::WorkerOwned(path);
            let digest = Self::copy_and_hash(&mut input, &mut file)?;
            return Ok((snapshot, digest));
        }
        let dir = stdx::tempfile::NamedTempDir::new("proc-macro-content")?;
        let mut file = std::fs::File::create(dir.path().join("library"))?;
        let digest = Self::copy_and_hash(&mut input, &mut file)?;
        Ok((Self::Temporary(dir), digest))
    }

    fn copy_and_hash(
        input: &mut impl io::Read,
        output: &mut impl io::Write,
    ) -> io::Result<[u8; 32]> {
        use sha2::{Digest, Sha256};
        let mut hash = Sha256::new();
        let mut buffer = [0; 64 * 1024];
        loop {
            let count = input.read(&mut buffer)?;
            if count == 0 {
                break;
            }
            output.write_all(&buffer[..count])?;
            hash.update(&buffer[..count]);
        }
        Ok(hash.finalize().into())
    }

    fn snapshot_path(&self) -> std::path::PathBuf {
        match self {
            Self::Temporary(dir) => dir.path().join("library"),
            Self::WorkerOwned(path) => path.clone(),
        }
    }
}

impl Drop for LibrarySnapshot {
    fn drop(&mut self) {
        if let Self::WorkerOwned(path) = self {
            _ = std::fs::remove_file(path);
        }
    }
}

impl fmt::Debug for LibrarySnapshot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.snapshot_path().fmt(f)
    }
}

impl Eq for ProcMacro {}
impl PartialEq for ProcMacro {
    fn eq(&self, other: &Self) -> bool {
        self.name == other.name
            && self.kind == other.kind
            && self.original_dylib_path == other.original_dylib_path
            && self.dylib_content == other.dylib_content
    }
}

/// Represents errors encountered when communicating with the proc-macro server.
#[derive(Clone, Debug)]
pub struct ServerError {
    pub message: String,
    pub io: Option<Arc<io::Error>>,
}

impl fmt::Display for ServerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.message.fmt(f)?;
        if let Some(io) = &self.io {
            f.write_str(": ")?;
            io.fmt(f)?;
        }
        Ok(())
    }
}

impl ProcMacroClient {
    /// Spawns an external process as the proc macro server and returns a client connected to it.
    pub fn spawn<'a>(
        process_path: &AbsPath,
        env: impl IntoIterator<
            Item = (impl AsRef<std::ffi::OsStr>, &'a Option<impl 'a + AsRef<std::ffi::OsStr>>),
        > + Clone,
        version: Option<&Version>,
        num_process: usize,
    ) -> io::Result<ProcMacroClient> {
        let path = process_path.to_owned();
        let vars: Vec<(std::ffi::OsString, Option<std::ffi::OsString>)> = env
            .into_iter()
            .map(|(key, value)| {
                (key.as_ref().to_owned(), value.as_ref().map(|value| value.as_ref().to_owned()))
            })
            .collect();
        let version = version.cloned();
        let factory = move || {
            let mut workers = Vec::with_capacity(num_process);
            for _ in 0..num_process {
                workers.push(ProcMacroServerProcess::spawn(
                    &path,
                    vars.iter().map(|(key, value)| (key, value)),
                    version.as_ref(),
                )?);
            }
            Ok(ProcMacroServerPool::new(workers))
        };
        let pool = factory()?;
        Ok(ProcMacroClient {
            epochs: Arc::new(Mutex::new(Epochs::new(pool))),
            path: process_path.to_owned(),
            spawn_generation: Some(Arc::new(factory)),
            snapshot_root: None,
        })
    }

    /// Invokes `spawn` and returns a client connected to the resulting read and write handles.
    ///
    /// The `process_path` is used for `Self::server_path`. This function is mainly used for testing.
    /// This non-replayable transport cannot rotate a native pool; use `spawn` in production.
    pub fn with_io_channels(
        process_path: &AbsPath,
        spawn: impl Fn(
            Option<ProtocolFormat>,
        ) -> io::Result<(
            Box<dyn process::ProcessExit>,
            Box<dyn io::Write + Send + Sync>,
            Box<dyn io::BufRead + Send + Sync>,
        )> + Clone,
        version: Option<&Version>,
        num_process: usize,
    ) -> io::Result<ProcMacroClient> {
        let mut workers = Vec::with_capacity(num_process);
        for _ in 0..num_process {
            workers.push(ProcMacroServerProcess::run(spawn.clone(), version, || {
                "<unknown>".to_owned()
            })?);
        }
        Ok(ProcMacroClient {
            epochs: Arc::new(Mutex::new(Epochs::new(ProcMacroServerPool::new(workers)))),
            path: process_path.to_owned(),
            spawn_generation: None,
            snapshot_root: None,
        })
    }

    /// Replayable test transport for exercising epoch lifetime without a native server.
    #[cfg(test)]
    pub fn with_generation_io_channels(
        process_path: &AbsPath,
        spawn: impl Fn(
            Option<ProtocolFormat>,
        ) -> io::Result<(
            Box<dyn process::ProcessExit>,
            Box<dyn io::Write + Send + Sync>,
            Box<dyn io::BufRead + Send + Sync>,
        )> + Clone
        + Send
        + Sync
        + 'static,
        version: Option<&Version>,
        num_process: usize,
    ) -> io::Result<ProcMacroClient> {
        let version = version.cloned();
        let factory = move || {
            let mut workers = Vec::with_capacity(num_process);
            for _ in 0..num_process {
                workers.push(ProcMacroServerProcess::run(spawn.clone(), version.as_ref(), || {
                    "<unknown>".to_owned()
                })?);
            }
            Ok(ProcMacroServerPool::new(workers))
        };
        Ok(ProcMacroClient {
            epochs: Arc::new(Mutex::new(Epochs::new(factory()?))),
            path: process_path.to_owned(),
            spawn_generation: Some(Arc::new(factory)),
            snapshot_root: None,
        })
    }

    /// Use a private worker root whose owner performs post-wait cleanup after abrupt exit.
    /// Other clients use self-cleaning temporary snapshots and never read daemon environment.
    pub fn with_snapshot_root(mut self, root: Option<std::path::PathBuf>) -> Self {
        self.snapshot_root = root;
        self
    }

    /// A workspace reload can replace content-addressed producer *paths*, not
    /// just bytes at an existing path. Retire its native epoch at the next load.
    /// Existing consumers still own the previous epoch; an unused cold pool is reused.
    pub fn begin_load_epoch(&self) {
        if self.spawn_generation.is_some() {
            let mut epochs = self.epochs.lock().unwrap();
            if epochs.initial.is_none() {
                epochs.current = Weak::new();
            }
        }
    }

    /// Returns the absolute path to the proc-macro server.
    pub fn server_path(&self) -> &AbsPath {
        &self.path
    }

    /// Loads a proc-macro dylib into the server process returning a list of `ProcMacro`s loaded.
    pub fn load_dylib(&self, dylib: MacroDylib) -> Result<Vec<ProcMacro>, ServerError> {
        let prepared = pool::PreparedLibrary::copy(&dylib, self.snapshot_root.as_deref())?;
        // Serialize registration and rotation: concurrent loads must agree on an epoch.
        let mut epochs = self.epochs.lock().unwrap();
        let mut pool = epochs.current.upgrade();
        if let Some(factory) = &self.spawn_generation {
            if pool.as_ref().is_none_or(|pool| pool.replaces(&dylib.path, prepared.content)) {
                let next = Arc::new(factory().map_err(|error| ServerError {
                    message: "spawning proc-macro epoch".into(),
                    io: Some(Arc::new(error)),
                })?);
                epochs.current = Arc::downgrade(&next);
                epochs.live.retain(|pool| pool.strong_count() != 0);
                epochs.live.push(Arc::downgrade(&next));
                pool = Some(next);
            }
        }
        let pool = pool.expect("non-replayable transport retains its initial pool");
        let handles = pool.load_dylib(&dylib, prepared)?;
        if self.spawn_generation.is_some() {
            epochs.initial = None;
        }
        Ok(handles)
    }

    /// Checks if the proc-macro server has exited.
    pub fn exited(&self) -> Option<ServerError> {
        let epochs = match self.epochs.try_lock() {
            Ok(epochs) => epochs,
            Err(std::sync::TryLockError::WouldBlock) => return None,
            Err(std::sync::TryLockError::Poisoned(_)) => {
                return Some(ServerError {
                    message: "proc-macro epoch state poisoned".into(),
                    io: None,
                });
            }
        };
        epochs.live.iter().filter_map(Weak::upgrade).find_map(|pool| pool.exited().cloned())
    }
}

impl ProcMacro {
    pub fn content_identity(&self) -> Option<[u8; 32]> {
        self.dylib_content
    }

    /// Returns the name of the procedural macro.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Returns the type of procedural macro.
    pub fn kind(&self) -> ProcMacroKind {
        self.kind
    }

    fn needs_fixup_change(&self) -> bool {
        let version = self.pool.version();
        (version::RUST_ANALYZER_SPAN_SUPPORT..version::HASHED_AST_ID).contains(&version)
    }

    /// On some server versions, the fixup ast id is different than ours. So change it to match.
    fn change_fixup_to_match_old_server(&self, tt: &mut tt::TopSubtree) {
        const OLD_FIXUP_AST_ID: ErasedFileAstId = ErasedFileAstId::from_raw(!0 - 1);
        tt.change_every_ast_id(|ast_id| {
            if *ast_id == FIXUP_ERASED_FILE_AST_ID_MARKER {
                *ast_id = OLD_FIXUP_AST_ID;
            } else if *ast_id == OLD_FIXUP_AST_ID {
                // Swap between them, that means no collision plus the change can be reversed by doing itself.
                *ast_id = FIXUP_ERASED_FILE_AST_ID_MARKER;
            }
        });
    }

    /// Expands the procedural macro by sending an expansion request to the server.
    /// This includes span information and environmental context.
    pub fn expand(
        &self,
        subtree: tt::SubtreeView<'_>,
        attr: Option<tt::SubtreeView<'_>>,
        env: Vec<(String, String)>,
        def_site: Span,
        call_site: Span,
        mixed_site: Span,
        current_dir: String,
        callback: Option<SubCallback<'_>>,
    ) -> Result<Result<tt::TopSubtree, String>, ServerError> {
        let (mut subtree, mut attr) = (subtree, attr);
        let (mut subtree_changed, mut attr_changed);
        if self.needs_fixup_change() {
            subtree_changed = tt::TopSubtree::from_subtree(subtree);
            self.change_fixup_to_match_old_server(&mut subtree_changed);
            subtree = subtree_changed.view();

            if let Some(attr) = &mut attr {
                attr_changed = tt::TopSubtree::from_subtree(*attr);
                self.change_fixup_to_match_old_server(&mut attr_changed);
                *attr = attr_changed.view();
            }
        }

        self.pool.pick_process()?.expand(
            self,
            subtree,
            attr,
            env,
            def_site,
            call_site,
            mixed_site,
            current_dir,
            callback,
        )
    }
}
