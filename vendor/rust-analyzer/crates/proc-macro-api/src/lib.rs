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
pub mod transport;

use paths::{AbsPath, AbsPathBuf};
use semver::Version;
use span::{ErasedFileAstId, FIXUP_ERASED_FILE_AST_ID_MARKER, Span};
use std::{fmt, io, sync::Arc};

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
    pool: Arc<ProcMacroServerPool>,
    /// New library generations get a pool whose lifetime follows their handles.
    /// This releases native server images without changing either wire protocol.
    spawn_generation: Option<Arc<dyn Fn() -> io::Result<ProcMacroServerPool> + Send + Sync>>,
    /// The path to the proc-macro server binary.
    path: AbsPathBuf,
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
    pool: ProcMacroServerPool,
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
    fn copy(source: &AbsPath) -> io::Result<Self> {
        // Open first: a missing producer must not leave an empty generation.
        let mut input = std::fs::File::open(source)?;
        // The daemon knows this private worker home and removes it only after
        // killing/waiting for that worker group. This covers stop without Drop.
        if std::env::var_os("RA_JIT_ENDS_WITH_INPUT").is_some()
            && let Some(home) = std::env::var_os("RA_JIT_HOME")
        {
            static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let pid = std::process::id();
            let dir = std::path::PathBuf::from(home).join("macro-snapshots").join(pid.to_string());
            for ancestor in dir.ancestors().take(2) {
                if std::fs::symlink_metadata(ancestor)
                    .is_ok_and(|metadata| metadata.file_type().is_symlink())
                {
                    return Err(io::Error::other("symlink in owned macro snapshot directory"));
                }
            }
            std::fs::create_dir_all(&dir)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
            }
            let marker = dir.join("owner");
            if std::fs::symlink_metadata(&marker)
                .is_ok_and(|metadata| metadata.file_type().is_symlink())
            {
                return Err(io::Error::other("symlink macro snapshot owner marker"));
            }
            if marker.exists() && std::fs::read_to_string(&marker)? != pid.to_string() {
                return Err(io::Error::other("macro snapshot owner mismatch"));
            }
            std::fs::write(marker, pid.to_string())?;
            let path = dir.join(format!(
                "generation-{}",
                NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            ));
            let mut file = std::fs::File::options().write(true).create_new(true).open(&path)?;
            let snapshot = Self::WorkerOwned(path);
            io::copy(&mut input, &mut file)?;
            return Ok(snapshot);
        }
        let dir = stdx::tempfile::NamedTempDir::new("proc-macro-content")?;
        let mut file = std::fs::File::create(dir.path().join("library"))?;
        io::copy(&mut input, &mut file)?;
        Ok(Self::Temporary(dir))
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
            pool: Arc::new(pool),
            path: process_path.to_owned(),
            spawn_generation: Some(Arc::new(factory)),
        })
    }

    /// Invokes `spawn` and returns a client connected to the resulting read and write handles.
    ///
    /// The `process_path` is used for `Self::server_path`. This function is mainly used for testing.
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
            pool: Arc::new(ProcMacroServerPool::new(workers)),
            path: process_path.to_owned(),
            spawn_generation: None,
        })
    }

    /// Test transport with replayable streams for each owned generation.
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
            pool: Arc::new(factory()?),
            path: process_path.to_owned(),
            spawn_generation: Some(Arc::new(factory)),
        })
    }

    /// Returns the absolute path to the proc-macro server.
    pub fn server_path(&self) -> &AbsPath {
        &self.path
    }

    /// Loads a proc-macro dylib into the server process returning a list of `ProcMacro`s loaded.
    pub fn load_dylib(&self, dylib: MacroDylib) -> Result<Vec<ProcMacro>, ServerError> {
        let Some(factory) = &self.spawn_generation else {
            return self.pool.load_dylib(&dylib);
        };
        let pool =
            factory().map_err(|error| ServerError { message: error.to_string(), io: None })?;
        pool.load_dylib(&dylib)
    }

    /// Checks if the proc-macro server has exited.
    pub fn exited(&self) -> Option<&ServerError> {
        self.pool.exited()
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
