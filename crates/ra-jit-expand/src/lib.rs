//! Proc-macro expansions kept in the store, so that a daemon that loads a workspace again
//! expands no macro it expanded before.
//!
//! An expansion is a function of what the macro is given, as long as it asks nothing of the
//! database while it runs. Its key is the content of the macro's dylib, the macro's name and
//! kind, the input (and the attribute's input) with each span replaced by its place in a
//! table of the distinct spans of the call (`def_site`, `call_site`, `mixed_site`, then the
//! input's), that table's spans, the environment and the current directory it is given, and
//! every variable of the server that expands it (by digest), which a macro reads without
//! asking as it reads rustc's: an expander that cannot name them is not kept. A
//! span is in the key as its byte range, which is from the item it is anchored to, and which
//! of the call's anchors and syntax contexts it has, numbered as they first appear: the
//! next call with the same key has other anchors and contexts, as another database or
//! another revision numbers them, in the same places. What a macro makes of spans without
//! asking (joins them, parses a string at the call site) is a function of the key, and the
//! value is the output with its spans written the same way. What else a macro learns of a
//! span (its text, file or line) it asks the database for, and an expansion that asked is not
//! kept.
//!
//! Neither is one that failed, nor one whose output has an anchor or a context the input has
//! not, which the next call could not be given back. A macro that reads files or the
//! environment by itself does so without asking. Derives and
//! attributes are kept, as rustc's `-Zcache-proc-macros` keeps derives; function-like macros,
//! where such reading is (a query checked against a database, a directory put in the
//! program), only with `RA_JIT_PROC_MACRO_CACHE=all`. A macro named in
//! `RA_JIT_PROC_MACRO_CACHE_SKIP` is never kept, `RA_JIT_PROC_MACRO_CACHE=check` expands every
//! call anyway and says which macro gave other output than it kept, and
//! `RA_JIT_PROC_MACRO_CACHE=off` keeps nothing and still counts.
//!
//! The store's own key adds the build of ra-jit and the toolchain, whose proc-macro server
//! expands.
//!
//! [`compare`] holds the toolchain's server against ra-jit's, and keeps nothing while it does.

mod compare;

use std::{
    any::Any,
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use base_db::{Env, SourceDatabase};
use hir_expand::proc_macro::{ProcMacroExpander, ProcMacroExpansionError, ProcMacroKind};
use paths::{AbsPath, AbsPathBuf};
use proc_macro_api::{
    legacy_protocol::msg::{FlatTree, SpanDataIndexMap},
    version::CURRENT_API_VERSION,
};
use ra_jit_store::{KEY, Store};
use sha2::{Digest, Sha256};
use span::{Span, SpanAnchor, SyntaxContext, TextRange};

/// What the key of an expansion starts with: another number when the key or the value is
/// made otherwise.
const DOMAIN: &[u8] = b"ra-jit proc-macro expansion 5";

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    Off,
    /// Derives and attributes: what a function-like macro is given is the code it reads, and
    /// the ones that read more than that (a query checked against a database, a directory
    /// put in the program) are of that kind.
    On,
    All,
    Check,
}

struct Config {
    mode: Mode,
    skip: Vec<Box<str>>,
    compare: bool,
}

fn config() -> &'static Config {
    static CONFIG: OnceLock<Config> = OnceLock::new();
    CONFIG.get_or_init(|| {
        let mode = match std::env::var("RA_JIT_PROC_MACRO_CACHE").as_deref() {
            Ok("off" | "0") => Mode::Off,
            Ok("all") => Mode::All,
            Ok("check") => Mode::Check,
            _ => Mode::On,
        };
        let skip = std::env::var("RA_JIT_PROC_MACRO_CACHE_SKIP").unwrap_or_default();
        let skip = skip.split(',').map(str::trim).filter(|it| !it.is_empty()).map(Into::into);
        let compare = std::env::var_os("RA_JIT_PROC_MACRO_COMPARE").is_some();
        Config { mode, skip: skip.collect(), compare }
    })
}

/// Puts the cache around every proc macro the workspaces load from here on.
pub fn install() {
    load_cargo::wrap_expanders(wrap);
}

fn wrap(
    inner: Arc<dyn ProcMacroExpander>,
    dylib: &AbsPath,
    name: &str,
    kind: ProcMacroKind,
) -> Arc<dyn ProcMacroExpander> {
    let skipped = match config().mode {
        Mode::Off => true,
        Mode::On => kind == ProcMacroKind::Bang,
        Mode::All | Mode::Check => false,
    } || config().compare
        || config().skip.iter().any(|it| **it == *name);
    let dylib = Dylib::at(dylib, inner.content_identity());
    Arc::new(Kept { inner, dylib, name: name.into(), kind, skipped })
}

/// A loaded dylib generation, fingerprinted when its expansion wrapper is made.
#[derive(Debug)]
struct Dylib {
    path: AbsPathBuf,
    hash: OnceLock<Option<[u8; KEY]>>,
}

#[cfg(test)]
mod dylib_identity_tests {
    use super::*;

    #[test]
    fn replacement_at_the_same_path_is_not_the_previous_expansion_library() {
        let path = std::env::temp_dir().join(format!("c4-macro-{}.dylib", std::process::id()));
        std::fs::write(&path, b"macro-one").unwrap();
        let absolute = AbsPathBuf::assert_utf8(path.clone());
        let first = Dylib::at(&absolute, None);
        let before = *first.hash().unwrap();
        let timestamp = std::fs::metadata(&path).unwrap().modified().unwrap();
        std::fs::write(&path, b"macro-two").unwrap();
        std::fs::File::options().write(true).open(&path).unwrap().set_modified(timestamp).unwrap();
        let second = Dylib::at(&absolute, None);
        assert_ne!(before, *second.hash().unwrap());
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn loaded_generation_identity_survives_producer_replacement_and_removal() {
        let path =
            std::env::temp_dir().join(format!("c4-loaded-macro-{}.dylib", std::process::id()));
        let absolute = AbsPathBuf::assert_utf8(path.clone());
        let loaded: [u8; KEY] = Sha256::digest(b"the server's immutable generation").into();
        // The producer can replace its original path after the server copied the
        // generation but before load-cargo wraps its expanders with this cache.
        std::fs::write(&path, b"the next producer generation").unwrap();
        assert_ne!(Dylib::at(&absolute, None).hash(), Some(&loaded));
        assert_eq!(Dylib::at(&absolute, Some(loaded)).hash(), Some(&loaded));
        std::fs::remove_file(path).unwrap();
        assert_eq!(Dylib::at(&absolute, Some(loaded)).hash(), Some(&loaded));
        assert_eq!(Dylib::at(&absolute, None).hash(), None);
    }
}

impl Dylib {
    /// Use the loader's immutable generation when available. Its original pathname
    /// may already name a newer producer by the time the expansion wrapper is made.
    fn at(path: &AbsPath, loaded: Option<[u8; KEY]>) -> Arc<Dylib> {
        let hash =
            loaded.or_else(|| std::fs::read(path).ok().map(|bytes| Sha256::digest(bytes).into()));
        Arc::new(Dylib { path: path.to_path_buf(), hash: OnceLock::from(hash) })
    }

    fn hash(&self) -> Option<&[u8; KEY]> {
        let hash = || Some(Sha256::digest(std::fs::read(&self.path).ok()?).into());
        self.hash.get_or_init(hash).as_ref()
    }
}

#[derive(Debug)]
struct Kept {
    inner: Arc<dyn ProcMacroExpander>,
    dylib: Arc<Dylib>,
    name: Box<str>,
    kind: ProcMacroKind,
    skipped: bool,
}

/// What the expansion of a call is kept under, and the anchors and contexts of its spans,
/// which the output is written against.
struct Call {
    key: [u8; KEY],
    spans: Spans,
    /// What each part of the key comes to, when `RA_JIT_KEYS_LOG` is set.
    parts: Option<Parts>,
}

/// The parts of a key, each by the first eight bytes of its SHA-256 in hex: two machines that
/// give one call different keys differ in one of these.
struct Parts {
    dylib: String,
    input: String,
    env: String,
    cwd: String,
    /// The server's environment.
    server_env: String,
}

/// A span as a key or a value has it: its byte range, and the numbers of its anchor and its
/// context among the call's.
type Shape = [u32; 4];

/// The anchors and syntax contexts of a call's spans, in the order they first appear.
#[derive(Default)]
struct Spans {
    anchors: Vec<SpanAnchor>,
    contexts: Vec<SyntaxContext>,
}

impl Spans {
    fn number<T: PartialEq>(all: &mut Vec<T>, it: T) -> u32 {
        let at = all.iter().position(|known| *known == it).unwrap_or_else(|| {
            all.push(it);
            all.len() - 1
        });
        at as u32
    }

    fn shape_new(&mut self, span: &Span) -> Shape {
        let anchor = Self::number(&mut self.anchors, span.anchor);
        let context = Self::number(&mut self.contexts, span.ctx);
        [span.range.start().into(), span.range.end().into(), anchor, context]
    }

    /// The shape of a span of the output, which has to have the call's anchors and contexts.
    fn shape(&self, span: &Span) -> Option<Shape> {
        let anchor = self.anchors.iter().position(|it| *it == span.anchor)?;
        let context = self.contexts.iter().position(|it| *it == span.ctx)?;
        Some([span.range.start().into(), span.range.end().into(), anchor as u32, context as u32])
    }

    fn span(&self, [start, end, anchor, context]: Shape) -> Option<Span> {
        Some(Span {
            range: TextRange::new(start.into(), end.max(start).into()),
            anchor: *self.anchors.get(anchor as usize)?,
            ctx: *self.contexts.get(context as usize)?,
        })
    }
}

impl Kept {
    #[allow(clippy::too_many_arguments)]
    fn call(
        &self,
        subtree: &tt::TopSubtree,
        attrs: Option<&tt::TopSubtree>,
        env: &Env,
        def_site: Span,
        call_site: Span,
        mixed_site: Span,
        current_dir: &str,
    ) -> Option<Call> {
        let mut table = SpanDataIndexMap::default();
        for span in [def_site, call_site, mixed_site] {
            table.insert(span);
        }
        let input = FlatTree::from_subtree(subtree.view(), CURRENT_API_VERSION, &mut table);
        let attrs =
            attrs.map(|it| FlatTree::from_subtree(it.view(), CURRENT_API_VERSION, &mut table));
        let mut spans = Spans::default();
        let shapes: Vec<Shape> = table.iter().map(|span| spans.shape_new(span)).collect();
        let mut env: Vec<_> = env.iter().collect();
        env.sort_unstable();
        let mut hasher = Sha256::new();
        let mut field = |bytes: &[u8]| {
            hasher.update((bytes.len() as u64).to_le_bytes());
            hasher.update(bytes);
        };
        field(DOMAIN);
        let dylib = self.dylib.hash()?;
        field(dylib);
        field(self.name.as_bytes());
        field(&[self.kind as u8]);
        field(&CURRENT_API_VERSION.to_le_bytes());
        let input = [
            postcard::to_allocvec(&input).ok()?,
            postcard::to_allocvec(&attrs).ok()?,
            postcard::to_allocvec(&shapes).ok()?,
        ];
        input.iter().for_each(|it| field(it));
        field(&(env.len() as u64).to_le_bytes());
        // A macro can branch on a path's length or read a file beneath it without putting
        // that path in its output. Until observed reads/environment accesses are available,
        // raw paths must remain in the lookup key and in the diagnostic hashes.
        for (name, value) in &env {
            field(name.as_bytes());
            field(value.as_bytes());
        }
        field(current_dir.as_bytes());
        // The variables of the server, all of them: a macro reads one by itself, and one
        // changed under the same input is another expansion, as rustc's process would make it.
        let server_env = self.inner.environment_identity()?;
        field(&server_env);
        let key = hasher.finalize().into();
        let env: Vec<(&str, std::borrow::Cow<'_, str>)> = env
            .into_iter()
            .map(|(name, value)| (name, std::borrow::Cow::Borrowed(value)))
            .collect();
        let parts = keys_logged().then(|| Parts {
            dylib: short(dylib),
            input: short(&input.concat()),
            env: short(env_text(&env).as_bytes()),
            cwd: short(current_dir.as_bytes()),
            server_env: short(&server_env),
        });
        if parts.is_some() {
            log_env(&env);
        }
        Some(Call { key, spans, parts })
    }
}

/// The output with its spans as shapes of the call's, or `None` when it has an anchor or a
/// context the call has not.
fn written(output: &tt::TopSubtree, spans: &Spans) -> Option<Vec<u8>> {
    let mut table = SpanDataIndexMap::default();
    let flat = FlatTree::from_subtree(output.view(), CURRENT_API_VERSION, &mut table);
    let shapes: Option<Vec<Shape>> = table.iter().map(|span| spans.shape(span)).collect();
    postcard::to_allocvec(&(shapes?, flat)).ok()
}

fn read(bytes: &[u8], spans: &Spans) -> Option<tt::TopSubtree> {
    let (shapes, flat): (Vec<Shape>, FlatTree) = postcard::from_bytes(bytes).ok()?;
    let table: Option<SpanDataIndexMap> = shapes.into_iter().map(|it| spans.span(it)).collect();
    Some(flat.to_subtree_resolved(CURRENT_API_VERSION, &table?))
}

/// The store, once the daemon has told it the toolchain: asked before that, it would be
/// opened without one, which is not to have one for as long as the process runs.
fn store() -> Option<&'static Store> {
    ra_jit_store::toolchain()?;
    Store::global()
}

/// Where `RA_JIT_KEYS_LOG` says to list the calls of this process, as hir-ty-codegen lists the
/// entries it adds: `harness/dist/crossmachine.py` reads both.
fn keys_log() -> Option<&'static Mutex<std::fs::File>> {
    static LOG: OnceLock<Option<Mutex<std::fs::File>>> = OnceLock::new();
    let open = || {
        let path = std::env::var_os("RA_JIT_KEYS_LOG").filter(|it| !it.is_empty())?;
        let file = std::fs::OpenOptions::new().create(true).append(true).open(path).ok()?;
        Some(Mutex::new(file))
    };
    LOG.get_or_init(open).as_ref()
}

fn keys_logged() -> bool {
    keys_log().is_some()
}

fn write_log(line: &str) {
    use std::io::Write as _;
    if let Some(log) = keys_log() {
        _ = log.lock().unwrap_or_else(|e| e.into_inner()).write_all(line.as_bytes());
    }
}

/// The first eight bytes of the SHA-256 of `bytes`, in hex.
fn short(bytes: &[u8]) -> String {
    Sha256::digest(bytes)[..8].iter().map(|it| format!("{it:02x}")).collect()
}

/// A diagnostic fingerprint for each variable. Values may hold credentials; naming which
/// variable differs between machines needs its digest, not its contents in a shared log.
fn env_text(env: &[(&str, std::borrow::Cow<'_, str>)]) -> String {
    env.iter().map(|(name, value)| format!("{name}={}\n", short(value.as_bytes()))).collect()
}

#[cfg(test)]
mod environment_key_tests {
    use super::*;

    /// A macro that reads a variable of its process: the expander says which process.
    #[derive(Debug)]
    struct ReadsItsProcess(Mutex<Option<[u8; 32]>>);

    impl ProcMacroExpander for ReadsItsProcess {
        fn expand(
            &self,
            _: &dyn SourceDatabase,
            subtree: &tt::TopSubtree,
            _: Option<&tt::TopSubtree>,
            _: &Env,
            _: Span,
            _: Span,
            _: Span,
            _: String,
        ) -> Result<tt::TopSubtree, ProcMacroExpansionError> {
            Ok(subtree.clone())
        }

        fn eq_dyn(&self, _: &dyn ProcMacroExpander) -> bool {
            false
        }

        fn environment_identity(&self) -> Option<[u8; 32]> {
            *self.0.lock().unwrap()
        }
    }

    /// The same input with PR15_PRIVATE's value changed between builds is another key, so the
    /// second build expands afresh; a third with the first value has the first build's key.
    #[test]
    fn an_expansion_is_kept_under_the_environment_of_its_server() {
        let inner = Arc::new(ReadsItsProcess(Mutex::new(None)));
        let loaded: [u8; KEY] = Sha256::digest(b"the macro library").into();
        let kept = Kept {
            inner: inner.clone(),
            dylib: Dylib::at(&AbsPathBuf::assert_utf8(std::env::temp_dir()), Some(loaded)),
            name: "Private".into(),
            kind: ProcMacroKind::CustomDerive,
            skipped: false,
        };
        let span = Span {
            range: TextRange::empty(0.into()),
            anchor: SpanAnchor {
                file_id: span::EditionedFileId::new(
                    span::FileId::from_raw(0),
                    span::Edition::Edition2024,
                ),
                ast_id: span::ROOT_ERASED_FILE_AST_ID,
            },
            ctx: SyntaxContext::root(span::Edition::Edition2024),
        };
        let input = tt::TopSubtree::empty(tt::DelimSpan::from_single(span));
        let env = Env::default();
        let key = |server: Option<&[u8]>| {
            *inner.0.lock().unwrap() = server.map(|it| Sha256::digest(it).into());
            kept.call(&input, None, &env, span, span, span, "/work").map(|call| call.key)
        };
        let first = key(Some(b"PR15_PRIVATE=old-private-value")).unwrap();
        let second = key(Some(b"PR15_PRIVATE absent")).unwrap();
        assert_ne!(first, second, "the second build's expansion is not the first's");
        assert_eq!(key(Some(b"PR15_PRIVATE=old-private-value")), Some(first));
        assert_eq!(key(None), None, "an expander that cannot name its process is not kept");
    }
}

#[cfg(test)]
mod diagnostic_tests {
    use super::env_text;

    #[test]
    fn environment_diagnostics_fingerprint_values_without_printing_them() {
        let text = env_text(&[("TOKEN", "private-token-value".into())]);
        assert!(text.starts_with("TOKEN="));
        assert!(!text.contains("private-token-value"));
        assert_eq!(text, env_text(&[("TOKEN", "private-token-value".into())]));
        assert_ne!(
            env_text(&[("VALUE", "first\nsecond".into())]),
            env_text(&[("VALUE", "first second".into())]),
            "a newline changes the environment key too"
        );
    }
}

/// The environment of a call, once for each: its parts as the key has them, which say which
/// variable still holds something of the machine when two machines give a call two keys.
fn log_env(env: &[(&str, std::borrow::Cow<'_, str>)]) {
    static SEEN: Mutex<Option<rustc_hash::FxHashSet<String>>> = Mutex::new(None);
    let text = env_text(env);
    let id = short(text.as_bytes());
    let mut seen = SEEN.lock().unwrap_or_else(|e| e.into_inner());
    if seen.get_or_insert_default().insert(id.clone()) {
        let vars: Vec<String> = text.lines().map(|it| it.replace('\0', "$")).collect();
        write_log(&format!("macro-env\t{id}\t{}\n", vars.join("\t")));
    }
}

impl Call {
    /// One line of the log for a call and what became of it: the macro, the parts of its key and
    /// the key as it is on disk.
    fn log(&self, kept: &Kept, store: &Store, outcome: &str) {
        let Some(parts) = &self.parts else { return };
        let disk: String = store.disk_key(&self.key).iter().map(|it| format!("{it:02x}")).collect();
        write_log(&format!(
            "macro\t{}\t{}\t{disk}\tdylib={}\tinput={}\tenv={}\tcwd={}\tsenv={}\t{outcome}\n",
            kind_name(kept.kind),
            kept.name,
            parts.dylib,
            parts.input,
            parts.env,
            parts.cwd,
            parts.server_env,
        ));
    }
}

fn kind_name(kind: ProcMacroKind) -> &'static str {
    ["derive", "attribute", "function-like"][kind_index(kind)]
}

impl ProcMacroExpander for Kept {
    fn expand(
        &self,
        db: &dyn SourceDatabase,
        subtree: &tt::TopSubtree,
        attrs: Option<&tt::TopSubtree>,
        env: &Env,
        def_site: Span,
        call_site: Span,
        mixed_site: Span,
        current_dir: String,
    ) -> Result<tt::TopSubtree, ProcMacroExpansionError> {
        if compare::comparing() {
            return self.inner.expand(
                db,
                subtree,
                attrs,
                env,
                def_site,
                call_site,
                mixed_site,
                current_dir,
            );
        }
        let start = Instant::now();
        let mode = config().mode;
        let store = (!self.skipped).then(store).flatten();
        let call = store.and_then(|_| {
            self.call(subtree, attrs, env, def_site, call_site, mixed_site, &current_dir)
        });
        if call.is_some() {
            COUNTS.keyed.add(start.elapsed());
        }
        let found = store.zip(call.as_ref()).and_then(|(store, call)| store.get(&call.key));
        if let (Some(bytes), Some(call)) = (&found, &call)
            && mode != Mode::Check
            && let Some(output) = read(bytes, &call.spans)
        {
            COUNTS.found.add(start.elapsed());
            if let Some(store) = store {
                call.log(self, store, "found");
            }
            return Ok(output);
        }
        // What was found and could not be read is written again.
        let found = found.filter(|_| mode == Mode::Check);
        let asked = load_cargo::asked();
        let expand = Instant::now();
        let expanded = self.inner.expand(
            db,
            subtree,
            attrs,
            env,
            def_site,
            call_site,
            mixed_site,
            current_dir.clone(),
        );
        COUNTS.expanded.add(expand.elapsed());
        COUNTS.kinds[kind_index(self.kind)].add(expand.elapsed());
        if config().compare {
            compare::again(
                &self.dylib.path,
                &self.name,
                self.kind,
                db,
                subtree,
                attrs,
                env,
                def_site,
                call_site,
                mixed_site,
                &current_dir,
                &expanded,
            );
        }
        let (Some(store), Some(call), Ok(output)) = (store, &call, &expanded) else {
            if expanded.is_err() {
                COUNTS.failed.add(Duration::ZERO);
            }
            if let (Some(store), Some(call)) = (store, &call) {
                call.log(self, store, "failed");
            }
            return expanded;
        };
        if load_cargo::asked() != asked {
            COUNTS.asked.add(Duration::ZERO);
            call.log(self, store, "asked the database");
            return expanded;
        }
        let Some(bytes) = written(output, &call.spans) else {
            COUNTS.unspanned.add(Duration::ZERO);
            call.log(self, store, "output has spans the call has not");
            return expanded;
        };
        match found {
            Some(kept) if kept != bytes => {
                COUNTS.differed.add(Duration::ZERO);
                eprintln!("proc macros: `{}` expanded otherwise than it was kept", self.name);
            }
            Some(_) => COUNTS.found.add(Duration::ZERO),
            None => {
                store.insert(&call.key, &bytes);
                COUNTS.kept.add(Duration::ZERO);
            }
        }
        call.log(self, store, "expanded");
        expanded
    }

    fn eq_dyn(&self, other: &dyn ProcMacroExpander) -> bool {
        (other as &dyn Any)
            .downcast_ref::<Kept>()
            .is_some_and(|other| self.name == other.name && self.inner.eq_dyn(&*other.inner))
    }

    fn content_identity(&self) -> Option<[u8; 32]> {
        self.inner.content_identity().or_else(|| self.dylib.hash().copied())
    }

    fn environment_identity(&self) -> Option<[u8; 32]> {
        self.inner.environment_identity()
    }
}

/// How many, and the time they took on the threads that expanded them.
struct Count {
    calls: AtomicU64,
    nanos: AtomicU64,
}

impl Count {
    const fn new() -> Count {
        Count { calls: AtomicU64::new(0), nanos: AtomicU64::new(0) }
    }

    fn add(&self, took: Duration) {
        self.calls.fetch_add(1, Ordering::Relaxed);
        self.nanos.fetch_add(took.as_nanos() as u64, Ordering::Relaxed);
    }

    fn get(&self) -> (u64, f64) {
        (self.calls.load(Ordering::Relaxed), self.nanos.load(Ordering::Relaxed) as f64 / 1e9)
    }
}

struct Counts {
    /// Calls given a key, with the time that took.
    keyed: Count,
    /// Expanded by the proc-macro server, kept or not.
    expanded: Count,
    /// The same, by [`kind_index`].
    kinds: [Count; 3],
    /// Read from the store instead (and, under `check`, expanded and the same).
    found: Count,
    kept: Count,
    asked: Count,
    unspanned: Count,
    failed: Count,
    differed: Count,
}

static COUNTS: Counts = Counts {
    keyed: Count::new(),
    expanded: Count::new(),
    kinds: [Count::new(), Count::new(), Count::new()],
    found: Count::new(),
    kept: Count::new(),
    asked: Count::new(),
    unspanned: Count::new(),
    failed: Count::new(),
    differed: Count::new(),
};

fn kind_index(kind: ProcMacroKind) -> usize {
    match kind {
        ProcMacroKind::CustomDerive => 0,
        ProcMacroKind::Attr => 1,
        ProcMacroKind::Bang => 2,
    }
}

/// The expansions so far, for the daemon's log: `None` before the first.
pub fn report() -> Option<String> {
    let (expanded, expanding) = COUNTS.expanded.get();
    let (found, finding) = COUNTS.found.get();
    if expanded + found == 0 {
        return None;
    }
    let (_, keying) = COUNTS.keyed.get();
    let mut said = format!(
        "proc macros: {expanded} expanded in {expanding:.2}s, {found} from the store in \
         {finding:.2}s, {keying:.2}s to key them"
    );
    let kinds = ["derives", "attributes", "function-like"].iter().zip(&COUNTS.kinds);
    let kinds: Vec<_> = kinds
        .filter_map(|(what, count)| {
            let (calls, took) = count.get();
            (calls != 0).then(|| format!("{calls} {what} in {took:.2}s"))
        })
        .collect();
    if !kinds.is_empty() {
        said += &format!(" ({})", kinds.join(", "));
    }
    let rest = [
        ("kept", &COUNTS.kept),
        ("not kept, as they asked the database", &COUNTS.asked),
        ("not kept, as their output has spans the call has not", &COUNTS.unspanned),
        ("failed", &COUNTS.failed),
        ("expanded otherwise than kept", &COUNTS.differed),
    ];
    for (what, count) in rest {
        let (calls, _) = count.get();
        if calls != 0 {
            said += &format!("; {calls} {what}");
        }
    }
    if let Some(compared) = compare::report() {
        said += &format!("; {compared}");
    }
    Some(said)
}
