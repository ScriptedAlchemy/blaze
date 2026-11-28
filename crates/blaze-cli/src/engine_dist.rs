//! Distributed compilation and caching bridge for `blaze export` (D11).
//!
//! Connects the source compiler's whole-program code-record walk (`engine_export.rs`) to the
//! retained coordinator (`blaze-dist-local`) and content-addressed cache server (`blaze-serve`)
//! when `BLAZE_DIST_SOCKET` is set in the command's environment. Release export needs an
//! LLVM and fold pipeline in the worker before it can publish distributed release records.
//!
//! Each workspace crate is scheduled as its own content-addressed `Unit` in topological order:
//! - Concrete non-generic runnable functions of that crate form the canonical sorted `Unit::names`
//!   (`CodeKey` computed ahead of compilation via `blaze_ladder::body_ids::instance_key`).
//! - `Granted::compile` compiles granted batches through the single `Maker` + `HomeCodeStore`
//!   pipeline and returns both the content entry (`KIND_CODE = 0`) and pointer (`KIND_POINTER = 1`)
//!   records so every peer and cold cache consumer hits the exact same `CodeKey`s without
//!   recompiling.
//! - `Granted::complete` runs on the lead executor only, overlays committed values in front of
//!   `HomeCodeStore`, completes any remaining reachability tail for the final program unit, and
//!   emits every tail-read and tail-compiled value before manifest publication.
//! - Failures pass through `blaze_dist_local::agent::decide`: committed-pack corruption and
//!   compiler refusals always stop under every `LocalPolicy`; coordinator unavailability stops by
//!   default (`never`) or falls back to the local walk when `BLAZE_DIST_LOCAL=fallback`.

use core::cell::RefCell;
use core::fmt::{self, Write as _};
use core::net::SocketAddr;
use core::sync::atomic::AtomicBool;
use core::time::Duration;

use blaze_core::Driver;
use blaze_dist::holder::HolderConfig;
use blaze_dist_local::agent::{self, Arrived, Config, Emission, Grant, Granted, Source, Unit};
use blaze_dist_local::blobs::Blobs;
use blaze_dist_local::pack::{Value, ValueRef};
use blaze_dist_local::sys::{append_file, path_is_file, random_id, read_file, write_file};
use blaze_ids::ContentId;
use blaze_ladder::body_ids::{self, BodyIds, RowKind, Scratch, Stepped, Walk};
use blaze_ladder::code_records::{Builder, CodeClass, CodeKey, CodeStore, PanicStrategy, StoreError};
use blaze_ladder::front2::keys::checker_of;
use blaze_ladder::run::RunError;
use blaze_ladder::run::make_one::{MOST_WALKED, Maker, NamesRecords, RecordsProgram, Typing};
use blaze_ladder::run::special::{EntryForm, ProductRoots};
use blaze_ladder::symbols::{encode_instance, fn_symbol_of_name};
use blaze_own::{AString, AVec, Alloc, BTreeMap, BTreeSet, String, Vec, format_in};
use blaze_scope::items_record::{ItemsRecord, TextSpan};
use blaze_surface::ItemKind;
use blaze_typeck::{ItemKey, List, PortableTys};
use blaze_wire::json::hex;
use blaze_wire::{
    Digest, LocalPolicy, Namespace, PrNamespace, WireAlloc, WireString, WireVec,
    digest_of_encoded_name,
};
use bstd::env::{Env, VarName};
use bstd::mem::Frame;
use bstd::Cx;

use crate::code_store::HomeCodeStore;
use crate::engine_seeds::CrateAt;
use crate::run_ids::RunIds;

pub(crate) mod reach;
pub(crate) mod reach_inputs;

#[cfg(feature = "llvm")]
pub(crate) mod prepared;

/// Coordinator Unix socket environment variable.
const SOCKET_VAR: &str = "BLAZE_DIST_SOCKET";
/// Cache server `host:port` environment variable.
const BLOBS_VAR: &str = "BLAZE_DIST_BLOBS";
/// Local failure policy (`never` or `fallback`).
const LOCAL_VAR: &str = "BLAZE_DIST_LOCAL";
/// JSONL report output path.
const REPORT_VAR: &str = "BLAZE_DIST_REPORT";
pub(crate) const ARTIFACT_KEYS_VAR: &str = "BLAZE_DIST_ARTIFACT_KEYS";
/// Execution budget in seconds per main-line unit or per pull request export.
const DEADLINE_VAR: &str = "BLAZE_DIST_DEADLINE_SECS";
/// Pull request namespace number.
const PR_VAR: &str = "BLAZE_DIST_PR";
/// Revision label within a pull request namespace.
const SOURCE_VAR: &str = "BLAZE_DIST_SOURCE";
const RESULT_GATE_VAR: &str = "BLAZE_DIST_TEST_RESULT_GATE";
const RESULT_UNIT_VAR: &str = "BLAZE_DIST_TEST_RESULT_UNIT";
const RESULT_SCOPE_VAR: &str = "BLAZE_DIST_TEST_RESULT_SCOPE";

/// Default execution budget.
const DEFAULT_DEADLINE: Duration = Duration::from_secs(3_600);
/// Pack entry kind for a plain `ReservedTag::Code` record.
pub(crate) const KIND_CODE: u8 = 0;
/// Pack entry kind for a 32-byte `ReservedTag::Pointer` target (`CodeKey -> content CodeKey`).
pub(crate) const KIND_POINTER: u8 = 1;

/// Typed refusal from parsing distributed settings or running distributed units.
#[derive(Debug)]
pub(crate) enum DistError {
    /// The worker has no LLVM or fold emitter for distributed release export.
    ReleaseBackendUnavailable,
    /// A test result selector has an invalid scope, is missing its directory, or names no planned unit.
    BadResultGate { shown: AString<'static> },
    /// A distributed option was supplied without `BLAZE_DIST_SOCKET`.
    OptionWithoutSocket { name: &'static str, value: AString<'static> },
    /// `BLAZE_DIST_SOCKET` was set without `BLAZE_DIST_BLOBS`.
    MissingBlobs,
    /// `BLAZE_DIST_BLOBS` did not parse as a socket address.
    BadBlobs { value: AString<'static> },
    /// `BLAZE_DIST_LOCAL` was neither `never` nor `fallback`.
    BadPolicy { value: AString<'static> },
    /// `BLAZE_DIST_DEADLINE_SECS` was not a valid positive integer.
    BadDeadline { value: AString<'static> },
    /// `BLAZE_DIST_PR` was not a valid u64.
    BadPr { value: AString<'static> },
    /// `BLAZE_DIST_PR` was set without `BLAZE_DIST_SOURCE`.
    MissingSource,
    /// `BLAZE_DIST_SOURCE` was set without `BLAZE_DIST_PR`.
    SourceWithoutPr { value: AString<'static> },
    /// Could not draw a random claimant identifier.
    Claimant { shown: AString<'static> },
    /// A distributed unit failed and `LocalPolicy` (or corruption/compile refusal) stopped the build.
    UnitFailed { unit: AString<'static>, shown: AString<'static> },
    /// Local compilation inside a distributed unit failed.
    Compile { shown: AString<'static> },
    /// Reading or writing a committed overlay entry failed.
    Store(StoreError),
    /// The scoped read of published units could not start or finish.
    PublishedRead(blaze_dist_local::Error),
}

impl fmt::Display for DistError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DistError::ReleaseBackendUnavailable => {
                write!(f, "the distributed build failed: `blaze export --release` requires LLVM emission and folding in the distributed worker (engine_dist.rs)")
            }
            DistError::BadResultGate { shown } => {
                write!(f, "the distributed build failed: {}", shown.as_str())
            }
            DistError::OptionWithoutSocket { name, value } => {
                write!(f, "the distributed build failed: {name} is {}, with no {SOCKET_VAR} to run its units", value.as_str())
            }
            DistError::MissingBlobs => {
                write!(f, "the distributed build failed: {SOCKET_VAR} needs {BLOBS_VAR}")
            }
            DistError::BadBlobs { value } => {
                write!(f, "the distributed build failed: {BLOBS_VAR} is not a valid host:port: {}", value.as_str())
            }
            DistError::BadPolicy { value } => {
                write!(f, "the distributed build failed: {LOCAL_VAR} is {}: it is `never` or `fallback`", value.as_str())
            }
            DistError::BadDeadline { value } => {
                write!(f, "the distributed build failed: {DEADLINE_VAR} is not a valid seconds count: {}", value.as_str())
            }
            DistError::BadPr { value } => {
                write!(f, "the distributed build failed: {PR_VAR} is not a valid pull request number: {}", value.as_str())
            }
            DistError::MissingSource => {
                write!(f, "the distributed build failed: {PR_VAR} needs {SOURCE_VAR}")
            }
            DistError::SourceWithoutPr { value } => {
                write!(f, "the distributed build failed: {SOURCE_VAR} is {}, with no {PR_VAR} it is a revision of", value.as_str())
            }
            DistError::Claimant { shown } => {
                write!(f, "the distributed build failed: naming this agent: {}", shown.as_str())
            }
            DistError::UnitFailed { unit, shown } => {
                write!(
                    f,
                    "the distributed build failed: {}: {} ({LOCAL_VAR}=fallback compiles the unit here instead)",
                    unit.as_str(),
                    shown.as_str()
                )
            }
            DistError::Compile { shown } => {
                write!(f, "the distributed build failed: {}", shown.as_str())
            }
            DistError::Store(error) => {
                write!(f, "the distributed build failed: {error}")
            }
            DistError::PublishedRead(error) => {
                write!(f, "the distributed build failed while reading published units: {error}")
            }
        }
    }
}

impl core::error::Error for DistError {}

fn own_str(text: &str, alloc: Alloc<'static>) -> AString<'static> {
    let mut out = String::new_in(alloc);
    out.push_str(text);
    out
}

fn wire_str(text: &str, alloc: WireAlloc) -> WireString {
    let mut out = WireString::with_capacity_in(text.len(), alloc);
    out.push_str(text);
    out
}

fn copy_wire_bytes(bytes: &[u8], alloc: WireAlloc) -> WireVec<u8> {
    let mut out = WireVec::with_capacity_in(bytes.len(), alloc);
    out.extend_from_slice(bytes);
    out
}

fn env_get<'a>(env: Option<&'a Env>, name: &str) -> Option<&'a str> {
    let var = VarName::new(name).ok()?;
    env?.get(&var).ok().flatten()
}

/// Parsed distributed settings for one `blaze export` invocation.
pub(crate) struct DistConfig {
    pub(crate) config: Config,
    pub(crate) report: Option<AString<'static>>,
    pub(crate) artifact_keys: Option<AString<'static>>,
    pub(crate) wire_alloc: WireAlloc,
    result_gate: Option<TestResultGate>,
}

struct TestResultGate {
    directory: AString<'static>,
    unit: Option<AString<'static>>,
    scope: agent::ResultGateScope,
}

impl TestResultGate {
    fn for_unit(&self, name: &str) -> Option<agent::TestResultGate<'_>> {
        self.unit.as_ref().is_none_or(|unit| unit.as_str() == name).then_some(agent::TestResultGate {
            directory: self.directory.as_str(),
            scope: self.scope,
        })
    }
}

impl DistConfig {
    /// Parses `BLAZE_DIST_*` from `env`. Returns `Ok(None)` when running standalone (no
    /// `BLAZE_DIST_SOCKET` and no other distributed option).
    pub(crate) fn from_env(
        env: Option<&Env>,
        host_triple: &str,
        heap: &'static blaze_own::Heap,
    ) -> Result<Option<DistConfig>, DistError> {
        let alloc = heap.alloc();
        let wire_alloc = heap.shared();
        let Some(socket) = env_get(env, SOCKET_VAR) else {
            for name in [BLOBS_VAR, LOCAL_VAR, REPORT_VAR, ARTIFACT_KEYS_VAR, DEADLINE_VAR, PR_VAR, SOURCE_VAR, RESULT_GATE_VAR, RESULT_UNIT_VAR, RESULT_SCOPE_VAR] {
                if let Some(val) = env_get(env, name) {
                    return Err(DistError::OptionWithoutSocket { name, value: own_str(val, alloc) });
                }
            }
            return Ok(None);
        };
        let Some(blobs_text) = env_get(env, BLOBS_VAR) else {
            return Err(DistError::MissingBlobs);
        };
        let address: SocketAddr = blobs_text
            .parse()
            .map_err(|_| DistError::BadBlobs { value: own_str(blobs_text, alloc) })?;
        let local = match env_get(env, LOCAL_VAR) {
            None | Some("never") => LocalPolicy::Never,
            Some("fallback") => LocalPolicy::Fallback,
            Some(other) => return Err(DistError::BadPolicy { value: own_str(other, alloc) }),
        };
        let deadline = match env_get(env, DEADLINE_VAR) {
            None => DEFAULT_DEADLINE,
            Some(raw) => {
                let secs: u64 = raw
                    .parse()
                    .map_err(|_| DistError::BadDeadline { value: own_str(raw, alloc) })?;
                if secs == 0 {
                    return Err(DistError::BadDeadline { value: own_str(raw, alloc) });
                }
                Duration::from_secs(secs)
            }
        };
        let namespace = match env_get(env, PR_VAR) {
            None => Namespace::Main,
            Some(raw) => {
                let n: u64 = raw
                    .parse()
                    .map_err(|_| DistError::BadPr { value: own_str(raw, alloc) })?;
                Namespace::Pr(PrNamespace { n })
            }
        };
        let source = match (env_get(env, SOURCE_VAR), &namespace) {
            (Some(raw), Namespace::Pr(_)) => {
                digest_of_encoded_name("blaze/dist-source", raw.as_bytes())
            }
            (None, Namespace::Pr(_)) => return Err(DistError::MissingSource),
            (Some(raw), Namespace::Main) => {
                return Err(DistError::SourceWithoutPr { value: own_str(raw, alloc) });
            }
            (None, Namespace::Main) => [0u8; 32],
        };
        let claimant = random_id().map_err(|err| {
            let mut s = String::new_in(alloc);
            let _ = write!(s, "{err}");
            DistError::Claimant { shown: s }
        })?;
        let config = Config {
            socket: wire_str(socket, wire_alloc),
            blobs: Blobs { address, timeout: Duration::from_secs(120), alloc: wire_alloc },
            claimant,
            platform: agent::platform(host_triple),
            local,
            namespace,
            source,
            deadline,
            holder: HolderConfig { heartbeat_ms: 1_000, retry_ms: 5_000, idle_ms: 10 },
            poll: Duration::from_millis(10),
        };
        let report = env_get(env, REPORT_VAR).map(|p| own_str(p, alloc));
        let artifact_keys = env_get(env, ARTIFACT_KEYS_VAR).map(|p| own_str(p, alloc));
        let unit = env_get(env, RESULT_UNIT_VAR).map(|name| own_str(name, alloc));
        let scope_value = env_get(env, RESULT_SCOPE_VAR);
        let scope = match scope_value {
            None | Some("lead") => agent::ResultGateScope::Lead,
            Some("all") => agent::ResultGateScope::All,
            Some(value) => return Err(DistError::BadResultGate {
                shown: format_in!(alloc, "{RESULT_SCOPE_VAR} needs lead or all, got {value}"),
            }),
        };
        let result_gate = match env_get(env, RESULT_GATE_VAR) {
            Some(dir) => Some(TestResultGate { directory: own_str(dir, alloc), unit, scope }),
            None if unit.is_some() || scope_value.is_some() => return Err(DistError::BadResultGate {
                shown: format_in!(alloc, "{} requires {RESULT_GATE_VAR}", if unit.is_some() { RESULT_UNIT_VAR } else { RESULT_SCOPE_VAR }),
            }),
            None => None,
        };
        Ok(Some(DistConfig { config, report, artifact_keys, wire_alloc, result_gate }))
    }
}

/// Overlay `CodeStore`: committed values from peers/cache sit in front of `HomeCodeStore`, while
/// recording every read and write so both granted batches and lead tails export complete values.
pub(crate) struct DistStore<'s, 'm, D: Driver> {
    local: &'s mut HomeCodeStore<'m, D>,
    /// Only values borrowed from a peer's not-yet-imported grant need owned bytes here.
    overlay: BTreeMap<Digest, (u8, WireVec<u8>), WireAlloc>,
    /// Reads and successful writes name the current store value, never a copy of its bytes.
    tracked: RefCell<BTreeSet<CodeKey, WireAlloc>>,
    alloc: WireAlloc,
}

impl<'s, 'm, D: Driver> DistStore<'s, 'm, D> {
    pub(crate) fn new(local: &'s mut HomeCodeStore<'m, D>, alloc: WireAlloc) -> Self {
        Self {
            local,
            overlay: BTreeMap::new_in(alloc),
            tracked: RefCell::new(BTreeSet::new_in(alloc)),
            alloc,
        }
    }

    pub(crate) fn clear_tracking(&mut self) {
        self.tracked.get_mut().clear();
    }

    pub(crate) fn insert_overlay(&mut self, key: Digest, kind: u8, bytes: &[u8]) {
        if !self.overlay.contains_key(&key) {
            self.overlay.insert(key, (kind, copy_wire_bytes(bytes, self.alloc)));
        }
    }

    pub(crate) fn merge_arrived(&mut self, values: &BTreeMap<Digest, (u8, WireVec<u8>), WireAlloc>) -> Result<(), DistError> {
        // The wire map is sorted by key. A pointer can sort before its target, which the
        // home's store requires to be present before it will accept the pointer.
        for (digest, (kind, bytes)) in values {
            if *kind == KIND_CODE {
                self.local.put(CodeClass::Program, CodeKey(ContentId(*digest)), bytes.as_slice()).map_err(DistError::Store)?;
            }
        }
        for (digest, (kind, bytes)) in values {
            let key = CodeKey(ContentId(*digest));
            match *kind {
                KIND_CODE => {}
                KIND_POINTER => {
                    let target: Digest = bytes.as_slice().try_into().map_err(|_| {
                        DistError::Store(StoreError::Failed { key, what: "a distributed pointer must hold a 32-byte content key" })
                    })?;
                    self.local.point(CodeClass::Program, key, CodeKey(ContentId(target))).map_err(DistError::Store)?;
                }
                _ => return Err(DistError::Store(StoreError::Failed { key, what: "unknown distributed code entry kind" })),
            }
            // The accepted value now lives in the home. Retaining a peer copy would both
            // duplicate its bytes and shadow a newly accepted mutable pointer.
            self.overlay.remove(digest);
        }
        Ok(())
    }

    fn export_key(&self, mut key: CodeKey, out: &mut BTreeMap<Digest, Value, WireAlloc>) -> Result<(), StoreError> {
        for _ in 0..8 {
            let digest = key.0.0;
            if out.contains_key(&digest) {
                return Ok(());
            }
            if let Some((kind, bytes)) = self.overlay.get(&digest) {
                out.insert(
                    digest,
                    Value { key: digest, kind: *kind, bytes: copy_wire_bytes(bytes.as_slice(), self.alloc) },
                );
                if *kind == KIND_POINTER && bytes.len() == 32 {
                    let mut target = [0u8; 32];
                    target.copy_from_slice(bytes.as_slice());
                    key = CodeKey(ContentId(target));
                    continue;
                }
                return Ok(());
            }
            if let Some(target) = self.local.pointed(key)? {
                out.insert(
                    digest,
                    Value { key: digest, kind: KIND_POINTER, bytes: copy_wire_bytes(&target.0.0, self.alloc) },
                );
                key = target;
                continue;
            }
            if let Some(bytes) = self.local.get(key)? {
                out.insert(
                    digest,
                    Value { key: digest, kind: KIND_CODE, bytes: copy_wire_bytes(bytes, self.alloc) },
                );
            }
            return Ok(());
        }
        Ok(())
    }

    fn drain_tracked_values(&mut self) -> Result<WireVec<Value>, StoreError> {
        let mut by_key: BTreeMap<Digest, Value, WireAlloc> = BTreeMap::new_in(self.alloc);
        let seen = core::mem::replace(self.tracked.get_mut(), BTreeSet::new_in(self.alloc));
        for key in seen {
            self.export_key(key, &mut by_key)?;
        }
        let mut values = WireVec::with_capacity_in(by_key.len(), self.alloc);
        for (_, val) in by_key {
            values.push(val);
        }
        Ok(values)
    }
}

impl<D: Driver> CodeStore for DistStore<'_, '_, D> {
    fn get(&self, key: CodeKey) -> Result<Option<&[u8]>, StoreError> {
        let digest = key.0.0;
        if let Some((kind, bytes)) = self.overlay.get(&digest) {
            self.tracked.borrow_mut().insert(key);
            if *kind == KIND_CODE {
                return Ok(Some(bytes.as_slice()));
            }
            return Ok(None);
        }
        let res = self.local.get(key)?;
        if res.is_some() {
            self.tracked.borrow_mut().insert(key);
        }
        Ok(res)
    }

    fn put(&mut self, class: CodeClass, key: CodeKey, bytes: &[u8]) -> Result<(), StoreError> {
        self.local.put(class, key, bytes)?;
        self.overlay.remove(&key.0.0);
        self.tracked.get_mut().insert(key);
        Ok(())
    }

    fn pointed(&self, key: CodeKey) -> Result<Option<CodeKey>, StoreError> {
        let digest = key.0.0;
        if let Some((kind, bytes)) = self.overlay.get(&digest) {
            self.tracked.borrow_mut().insert(key);
            if *kind == KIND_POINTER && bytes.len() == 32 {
                let mut target = [0u8; 32];
                target.copy_from_slice(bytes.as_slice());
                return Ok(Some(CodeKey(ContentId(target))));
            }
            return Ok(None);
        }
        let res = self.local.pointed(key)?;
        if let Some(target) = res {
            self.tracked.borrow_mut().insert(key);
            self.tracked.borrow_mut().insert(target);
        }
        Ok(res)
    }

    fn point(&mut self, class: CodeClass, key: CodeKey, content: CodeKey) -> Result<(), StoreError> {
        self.local.point(class, key, content)?;
        self.overlay.remove(&key.0.0);
        self.tracked.get_mut().insert(key);
        Ok(())
    }
}

#[cfg(test)]
#[path = "engine_dist/tests.rs"]
mod tests;

/// One concrete, pre-keyed function inside a crate's distributed unit.
struct WorkItem<'a> {
    tabled: ItemKey,
    name: AVec<'a, u8>,
    key: CodeKey,
}

/// One workspace crate's distributed unit plan.
struct CratePlan<'a> {
    unit_name: AString<'static>,
    unit: Unit,
    inputs: AVec<'a, Digest>,
    items: AVec<'a, WorkItem<'a>>,
    is_final: bool,
}

/// Checks whether a source item or impl header has no generic parameter list `<...>` before
/// its parameter list `(` or body `{` / `for`.
fn header_is_nongeneric(text: &str, stop_at_paren: bool) -> bool {
    let bytes = text.as_bytes();
    let mut i = 0usize;
    let mut in_attr = 0u32;
    while let Some(&b) = bytes.get(i) {
        if b == b'#' && bytes.get(i + 1) == Some(&b'[') {
            in_attr += 1;
            i += 2;
            continue;
        }
        if in_attr > 0 {
            if b == b'[' {
                in_attr += 1;
            } else if b == b']' {
                in_attr -= 1;
            }
            i += 1;
            continue;
        }
        if stop_at_paren && b == b'(' {
            return true;
        }
        if b == b'{' || b == b';' {
            return true;
        }
        if b == b'<' {
            return false;
        }
        i += 1;
    }
    true
}

/// Computes the canonical, path-free digest of a distributed unit.
fn unit_digest_of(
    unit_name: &str,
    builder: &Builder,
    release: bool,
    work_names: &[Digest],
    extra: &[Digest],
    alloc: Alloc<'_>,
) -> Digest {
    let mut buf: AVec<'_, u8> = Vec::new_in(alloc);
    let name_bytes = unit_name.as_bytes();
    buf.extend_from_slice(&u64::try_from(name_bytes.len()).unwrap_or(0).to_le_bytes());
    buf.extend_from_slice(name_bytes);
    let triple = blaze_ladder::code_records::triple(builder.isa).as_bytes();
    buf.extend_from_slice(&u64::try_from(triple.len()).unwrap_or(0).to_le_bytes());
    buf.extend_from_slice(triple);
    buf.push(u8::from(builder.isa.lse));
    buf.push(u8::from(builder.settings.overflow_checks.on()));
    buf.push(u8::from(builder.settings.debug_assertions));
    buf.push(u8::from(release));
    buf.extend_from_slice(&builder.toolchain.0);
    buf.extend_from_slice(&builder.producer.id().0);
    buf.extend_from_slice(&u64::try_from(work_names.len()).unwrap_or(0).to_le_bytes());
    for d in work_names {
        buf.extend_from_slice(d);
    }
    buf.extend_from_slice(&u64::try_from(extra.len()).unwrap_or(0).to_le_bytes());
    for d in extra {
        buf.extend_from_slice(d);
    }
    digest_of_encoded_name("blaze/dist-unit", buf.as_slice())
}

/// Distinguishes the selected start and the explicit root closure of a final unit.
pub(crate) fn program_roots_digest(name: &str, roots: &ProductRoots<'_>, panic: PanicStrategy) -> Digest {
    let (form, entry) = match roots {
        ProductRoots::Executable(roots) => (match roots.form {
            EntryForm::Unit => b"unit".as_slice(),
            EntryForm::CMain => b"c-main".as_slice(),
            EntryForm::Hosted => b"hosted".as_slice(),
            EntryForm::Comptime => b"comptime".as_slice(),
        }, Some(roots.entry)),
        ProductRoots::Library(_) => (b"cdylib".as_slice(), None),
    };
    let panic = match panic {
        PanicStrategy::Unwind => b"unwind".as_slice(),
        PanicStrategy::Abort => b"abort".as_slice(),
    };
    let mut hash = blaze_ids::Hasher::new("blaze distributed program roots 1");
    hash.part(name.as_bytes()).part(form).part(panic);
    if let Some(entry) = entry {
        hash.part(&entry.0);
    }
    for index in 0..roots.len() {
        let (item, instance) = roots.row(index);
        hash.part(&item.0.to_le_bytes()).part(instance);
    }
    hash.finish().0
}

/// Reads a `TextSpan` from `names` for crate `krate` without any lossy casts.
fn slice_span<'a>(names: &NamesRecords<'a>, krate: usize, span: TextSpan) -> Option<&'a str> {
    let whole = names.records.whole_text(krate, span.at)?;
    let start = usize::try_from(span.start).ok()?;
    let len = usize::try_from(span.len).ok()?;
    let end = start.checked_add(len)?;
    whole.get(start..end)
}

struct CrateItems<'a> {
    functions: AVec<'a, WorkItem<'a>>,
    data: ContentId,
}

/// Collects grantable functions and the checked data bodies their compilation can read.
fn collect_crate_items<'a, 'b>(
    names: NamesRecords<'b>,
    krate: usize,
    rec: &ItemsRecord<'b>,
    ids: &RunIds<'b>,
    builder: &Builder,
    alloc: Alloc<'a>,
) -> CrateItems<'a> {
    let mut raw: AVec<'a, WorkItem<'a>> = Vec::new_in(alloc);
    let mut data = Vec::new_in(alloc);
    let tys = PortableTys::new_in(alloc);
    let ground = tys.view();

    let mut try_push = |item_key: ItemKey| {
        if ids.runnable(item_key).is_err() {
            return;
        }
        let mut encoded: AVec<'a, u8> = Vec::new_in(alloc);
        if encode_instance(item_key, List::EMPTY, ground, alloc, &mut encoded).is_err() {
            return;
        }
        let symbol = fn_symbol_of_name(encoded.as_slice());
        let Some(key) = body_ids::instance_key(
            symbol,
            item_key,
            encoded.as_slice(),
            ids,
            builder,
            &builder.producer,
            alloc,
        ) else {
            return;
        };
        raw.push(WorkItem { tabled: item_key, name: encoded, key });
    };

    for i in 0..rec.items() {
        let Ok(Some(item)) = rec.item_at(i) else { continue };
        match item.kind {
            ItemKind::Const | ItemKind::Static => data.push(checker_of(item.key)),
            ItemKind::Impl | ItemKind::Trait => {
                names.records.each_member_row(item.key, &mut |member| {
                    if member.kind == ItemKind::Const {
                        data.push(checker_of(member.key));
                    }
                });
            }
            _ => {}
        }
        if item.kind != ItemKind::Function || item.foreign.is_some() {
            continue;
        }
        let nongeneric = match item.text.and_then(|span| slice_span(&names, krate, span)) {
            Some(text) => header_is_nongeneric(text, true),
            None => false,
        };
        if nongeneric {
            try_push(checker_of(item.key));
        }
    }

    for i in 0..rec.impls() {
        let Ok(Some(imp)) = rec.impl_at(i) else { continue };
        if imp.refused_call || imp.header_unread {
            continue;
        }
        let impl_span = rec.item(imp.key).ok().flatten().and_then(|row| row.impl_header.or(row.text));
        let impl_nongeneric = match impl_span.and_then(|span| slice_span(&names, krate, span)) {
            Some(text) => header_is_nongeneric(text, false),
            None => true,
        };
        if !impl_nongeneric {
            continue;
        }
        let mut member_spans: AVec<'a, (blaze_id::ItemKey, Option<TextSpan>)> = Vec::new_in(alloc);
        names.records.each_member_row(imp.key, &mut |member| {
            if member.kind == ItemKind::Function {
                member_spans.push((member.key, member.text));
            }
        });
        for (mkey, mspan) in member_spans {
            let member_nongeneric = match mspan.and_then(|span| slice_span(&names, krate, span)) {
                Some(text) => header_is_nongeneric(text, true),
                None => true,
            };
            if member_nongeneric {
                try_push(checker_of(mkey));
            }
        }
    }

    raw.sort_unstable_by(|a, b| a.key.0.0.cmp(&b.key.0.0));
    let mut deduped: AVec<'a, WorkItem<'a>> = Vec::with_capacity_in(raw.len(), alloc);
    for item in raw {
        if deduped.last().is_some_and(|prev| prev.key == item.key) {
            continue;
        }
        deduped.push(item);
    }
    // A constant's checked body holds the reads of its initializer, including the bytes of
    // an included file. These inputs affect the unit even when no function's text changes.
    data.sort_unstable();
    data.dedup();
    let mut digest = blaze_ids::Hasher::new("blaze distributed checked data 1");
    for item in data {
        digest.part(&item.0.to_le_bytes());
        if let Some(body) = ids.body_check(item) {
            digest.part(&body.key.0);
            digest.part(&(body.reads.len() as u64).to_le_bytes());
            for read in body.reads {
                digest.part(&read.fact).part(&read.digest);
            }
        } else {
            digest.part(&[]);
        }
    }
    CrateItems { functions: deduped, data: digest.finish() }
}

/// Formats a `WireVec<Digest>` as a JSON array of 64-char hex strings.
fn json_digests(digests: &[Digest], alloc: WireAlloc) -> WireString {
    let mut out = WireString::new_in(alloc);
    out.push('[');
    for (idx, d) in digests.iter().enumerate() {
        if idx > 0 {
            out.push(',');
        }
        let _ = write!(out, "\"{}\"", hex(d));
    }
    out.push(']');
    out
}

struct PlanReport<'a> {
    name: &'a str,
    unit: &'a Unit,
    inputs: &'a [Digest],
    work: Option<DevWorkReport<'a>>,
}

struct DevWorkReport<'a> {
    items: &'a [WorkItem<'a>],
    ids: &'a RunIds<'a>,
}

/// Write the complete plan before a worker can be held or stopped by the coordinator.
fn write_unit_plans<'a>(report_path: Option<&str>, plans: impl Iterator<Item = PlanReport<'a>>, alloc: WireAlloc) {
    let Some(path) = report_path else { return };
    let mut file = WireString::from_str_in(path, alloc);
    file.push_str(".plans");
    let mut body = WireString::from_str_in("[", alloc);
    for (index, plan) in plans.enumerate() {
        if index != 0 {
            body.push(',');
        }
        let names = json_digests(plan.unit.names.as_slice(), alloc);
        let inputs = json_digests(plan.inputs, alloc);
        let _ = write!(body, "{{\"unit\":\"{}\",\"unit_digest\":\"{}\",\"plan_names\":{},\"plan_inputs\":{},\"work\":[",
            plan.name, hex(&plan.unit.spec), names.as_str(), inputs.as_str());
        for (index, (item, ids)) in plan.work.into_iter().flat_map(|work| work.items.iter().map(move |item| (item, work.ids))).enumerate() {
            if index != 0 {
                body.push(',');
            }
            let symbol = fn_symbol_of_name(item.name.as_slice());
            let _ = write!(body, "{{\"item\":\"{}\",\"symbol\":\"{}\",\"work_name\":\"{}\"",
                hex(&item.tabled.0.to_le_bytes()), hex(&symbol.0), hex(&item.key.0.0));
            if let Some(check) = ids.body_check(item.tabled) {
                let _ = write!(body, ",\"body_key\":\"{}\",\"body_reads\":[", hex(&check.key.0));
                for (index, read) in check.reads.iter().enumerate() {
                    if index != 0 {
                        body.push(',');
                    }
                    let _ = write!(body, "[\"{}\",\"{}\"]", hex(&read.fact), hex(&read.digest));
                }
                body.push(']');
            }
            // Planned work has no generic arguments, so the owner is the name's only item.
            if let Some(read) = ids.instance_fact(item.tabled) {
                let _ = write!(body, ",\"instance_fact\":[\"{}\",\"{}\"]", hex(&read.fact), hex(&read.digest));
            }
            body.push('}');
        }
        body.push_str("]}");
    }
    body.push_str("]\n");
    let _ = write_file(file.as_str(), body.as_bytes());
}

/// Appends one JSONL unit report row to `BLAZE_DIST_REPORT`.
#[expect(clippy::too_many_arguments, reason = "explicit report row fields, zero hidden globals")]
fn append_report_row(
    report_path: Option<&str>,
    unit_name: &str,
    unit: &Unit,
    arrived: &Arrived,
    tail_compiled: usize,
    from_store: usize,
    instances: usize,
    tail_compiled_keys: &[Digest],
    reach: Option<&reach::Closure>,
    alloc: WireAlloc,
) {
    let Some(path) = report_path else { return };
    let source = match arrived.source {
        Source::Published => "published",
        Source::Executed => "executed",
    };
    let execution = arrived.execution.and_then(|v| i64::try_from(v).ok()).unwrap_or(-1);
    let attempt = arrived.attempt.and_then(|v| i64::try_from(v).ok()).unwrap_or(-1);
    let pub_attempt = arrived.publication_attempt.and_then(|v| i64::try_from(v).ok()).unwrap_or(-1);
    let work_keys = json_digests(arrived.compiled_keys.as_slice(), alloc);
    let tail_keys = json_digests(tail_compiled_keys, alloc);
    let reach_digest = hex(reach.map_or(&[][..], |closure| closure.digest.as_slice()));
    let mut line = WireString::new_in(alloc);
    let _ = write!(
        line,
        concat!(
            "{{\"unit\":\"{}\",\"source\":\"{}\",\"execution\":{},\"joined\":{},\"grants\":{},",
            "\"compiled_for_work\":{},\"stored_for_work\":{},\"arrived\":{},\"tail_compiled\":{},\"from_store\":{},\"instances\":{},",
            "\"unit_digest\":\"{}\",\"attempt\":{},\"publication_attempt\":{},\"work_compiled_keys\":{},\"tail_compiled_keys\":{},",
            "\"verified_reach_rows\":{},\"verified_reach_digest\":\"{}\"}}\n",
        ),
        unit_name,
        source,
        execution,
        arrived.joined,
        arrived.grants,
        arrived.compiled_here,
        arrived.stored_here,
        arrived.values.len() + usize::from(reach.is_some()),
        tail_compiled,
        from_store,
        instances,
        hex(&unit.spec),
        attempt,
        pub_attempt,
        work_keys.as_str(),
        tail_keys.as_str(),
        reach.map_or(0, reach::Closure::len),
        reach_digest,
    );
    let _ = append_file(path, line.as_bytes());
}

/// Result of running the distributed export pass across all workspace crate units.
pub(crate) enum DistExportOutcome {
    /// Output executable is already up to date (exact fingerprint hit); no unit was run.
    NoOp,
    /// Distributed units finished; `DistStore` holds all committed/compiled records for export.
    Completed { compiled: usize, cached: usize, reach: reach::Closure },
    /// Coordinator was unavailable under `BLAZE_DIST_LOCAL=fallback`; caller runs local walk.
    FallbackLocal,
}

/// Computes the sidecar fingerprint path `<output>.blaze-dist-fp`.
fn fingerprint_path(output_path: &str, alloc: WireAlloc) -> WireString {
    let mut s = WireString::with_capacity_in(output_path.len() + 15, alloc);
    s.push_str(output_path);
    s.push_str(".blaze-dist-fp");
    s
}

/// Checks if `output_path` already exists with a matching sidecar fingerprint.
pub(crate) fn matches_output_fingerprint(
    output_path: &str,
    fp: &Digest,
    alloc: WireAlloc,
) -> bool {
    if !path_is_file(output_path) {
        return false;
    }
    let fp_path = fingerprint_path(output_path, alloc);
    let Ok(bytes) = read_file(fp_path.as_str(), alloc) else {
        return false;
    };
    let mut expected = WireString::with_capacity_in(64, alloc);
    let _ = write!(expected, "{}", hex(fp));
    bytes.as_slice() == expected.as_bytes()
}

/// Writes the sidecar fingerprint after `output_path` has been published.
pub(crate) fn write_output_fingerprint(output_path: &str, fp: &Digest, alloc: WireAlloc) {
    let fp_path = fingerprint_path(output_path, alloc);
    let mut expected = WireString::with_capacity_in(64, alloc);
    let _ = write!(expected, "{}", hex(fp));
    let _ = write_file(fp_path.as_str(), expected.as_bytes());
}

/// `Granted` implementation for one workspace crate unit.
struct UnitExecutor<'a, 'b, 's, 'm, D: Driver, T: Typing<RecordsProgram<'b>>> {
    cx: &'a Cx,
    plan: &'a CratePlan<'a>,
    ids: &'a RunIds<'b>,
    builder: &'a Builder,
    maker: &'a mut Maker<'b, crate::run_ids::Runnable, T>,
    store: &'a mut DistStore<'s, 'm, D>,
    roots: &'a ProductRoots<'a>,
    scratch_alloc: Alloc<'a>,
    wire_alloc: WireAlloc,
    heap: &'static blaze_own::Heap,
    tail_compiled: usize,
    tail_from_store: usize,
    tail_instances: usize,
    tail_compiled_keys: WireVec<Digest>,
    ran_complete: bool,
    result_gate: Option<agent::TestResultGate<'a>>,
}

impl<'b, D: Driver, T: Typing<RecordsProgram<'b>>> Granted for UnitExecutor<'_, 'b, '_, '_, D, T> {
    type Error = DistError;

    fn test_result_gate(&self) -> Option<agent::TestResultGate<'_>> {
        self.result_gate
    }

    fn compile(&mut self, names: &[usize]) -> Result<Grant, DistError> {
        let mut grant_values: BTreeMap<Digest, Value, WireAlloc> = BTreeMap::new_in(self.wire_alloc);
        let mut emissions = WireVec::new_in(self.wire_alloc);
        let mut compiled = 0u64;
        let mut stored = 0u64;
        let mut scratch = Scratch::new_in(self.scratch_alloc);

        for &idx in names {
            let Some(item) = self.plan.items.get(idx) else {
                continue;
            };
            let proof = self.ids.runnable(item.tabled).map_err(|why| {
                let mut s = String::new_in(self.heap.alloc());
                let _ = write!(s, "item not runnable: {why}");
                DistError::Compile { shown: s }
            })?;
            self.store.clear_tracking();
            let mut walk = Walk::new_in(self.scratch_alloc);
            walk.root::<RunError>((item.tabled, item.name.as_slice()), MOST_WALKED).map_err(|why| {
                let mut s = String::new_in(self.heap.alloc());
                let _ = write!(s, "walk root: {why}");
                DistError::Compile { shown: s }
            })?;
            match walk.one(
                0,
                self.ids,
                self.store,
                self.builder,
                &self.builder.producer,
                self.maker,
                &proof,
                &body_ids::EntryForm,
                MOST_WALKED,
                &mut scratch,
            ) {
                Ok(one) => match one.stepped {
                    Stepped::Found(_) => {
                        stored += 1;
                    }
                    Stepped::Made(code_key) => {
                        compiled += 1;
                        emissions.push(Emission::plain(code_key.0.0));
                    }
                    Stepped::Unkeyed(_) => {}
                },
                Err(why) => {
                    let mut s = String::new_in(self.heap.alloc());
                    let _ = write!(s, "compiling item: {why}");
                    return Err(DistError::Compile { shown: s });
                }
            }
            let tracked = self.store.drain_tracked_values().map_err(DistError::Store)?;
            for val in tracked {
                grant_values.insert(val.key, val);
            }
        }

        let mut values = WireVec::with_capacity_in(grant_values.len(), self.wire_alloc);
        for (_, val) in grant_values {
            values.push(val);
        }
        Ok(Grant { values, compiled, stored, emissions })
    }

    fn complete(
        &mut self,
        values: &mut dyn Iterator<Item = ValueRef<'_>>,
        output: &mut dyn FnMut(Value),
    ) -> Result<(), DistError> {
        self.ran_complete = true;
        for v in values {
            self.store.insert_overlay(v.key, v.kind, v.bytes);
        }
        if !self.plan.is_final {
            return Ok(());
        }
        let _span = self.cx.span("export: verify distributed reachability at publisher");
        let mut closure = reach::Writer::new(self.plan.unit.spec, self.wire_alloc);
        self.store.clear_tracking();
        let mut walk = Walk::new_in(self.scratch_alloc);
        for i in 0..self.roots.len() {
            walk.root::<RunError>(self.roots.row(i), MOST_WALKED).map_err(|why| {
                let mut s = String::new_in(self.heap.alloc());
                let _ = write!(s, "tail walk root: {why}");
                DistError::Compile { shown: s }
            })?;
        }
        let mut scratch = Scratch::new_in(self.scratch_alloc);
        let root_count = self.roots.len();
        let mut next_row = 0usize;
        while next_row < walk.len() {
            let row = next_row;
            next_row += 1;
            let tabled = walk.tabled(row);
            let kind = walk.kind(row);
            let proof = self.ids.runnable(tabled).map_err(|why| {
                let mut s = String::new_in(self.heap.alloc());
                let _ = write!(s, "tail item not runnable: {why}");
                DistError::Compile { shown: s }
            })?;
            match walk.one(
                row,
                self.ids,
                self.store,
                self.builder,
                &self.builder.producer,
                self.maker,
                &proof,
                &body_ids::EntryForm,
                MOST_WALKED,
                &mut scratch,
            ) {
                Ok(one) => {
                    self.tail_instances += 1;
                    match one.stepped {
                        Stepped::Found(_) => {
                            self.tail_from_store += 1;
                        }
                        Stepped::Made(code_key) => {
                            self.tail_compiled += 1;
                            self.tail_compiled_keys.push(code_key.0.0);
                        }
                        Stepped::Unkeyed(_) => {}
                    }
                    let entry = match one.stepped {
                        Stepped::Found(key) | Stepped::Made(key) => {
                            let recipe = scratch.queried().ok_or_else(|| {
                                reach::refused(self.heap, self.plan.unit.spec, "lost the checked instance key")
                            })?;
                            let content = if recipe == key {
                                let bytes = self.store.get(key).map_err(DistError::Store)?
                                    .ok_or(DistError::Store(StoreError::Failed { key, what: "missing verified reachability entry" }))?;
                                blaze_ladder::code_records::content_key(bytes)
                            } else {
                                key
                            };
                            reach::Step::Keyed { recipe, key, content }
                        }
                        Stepped::Unkeyed(_) => reach::Step::Unkeyed(blaze_ladder::code_records::content_key(scratch.made())),
                    };
                    closure.push(tabled, walk.symbol(row), entry);
                }
                Err(why)
                    if row >= root_count
                        && kind == RowKind::Instance
                        && why.scope() == blaze_ladder::scope::Scope::One => {
                    closure.push(tabled, walk.symbol(row), reach::Step::Stub);
                }
                Err(why) => {
                    let mut s = String::new_in(self.heap.alloc());
                    let _ = write!(s, "tail compile: {why}");
                    return Err(DistError::Compile { shown: s });
                }
            }
        }
        let tracked = self.store.drain_tracked_values().map_err(DistError::Store)?;
        for val in tracked {
            output(val);
        }
        output(closure.finish());
        Ok(())
    }
}

/// Builds the ordered per-workspace-crate distributed unit plans, checks the output no-op
/// fingerprint, and executes every unit through `blaze_dist_local::agent::ExportBudget`.
#[expect(clippy::too_many_arguments, reason = "explicit distributed export capabilities")]
pub(crate) fn run_distributed<'a, 'b, 's, 'm, D: Driver, T: Typing<RecordsProgram<'b>>>(
    cx: &'a Cx,
    dist: &DistConfig,
    names: NamesRecords<'b>,
    closure: &[CrateAt],
    program_crate_name: &str,
    ids: &'a RunIds<'b>,
    builder: &'a Builder,
    maker: &'a mut Maker<'b, crate::run_ids::Runnable, T>,
    store: &'a mut DistStore<'s, 'm, D>,
    roots: &'a ProductRoots<'a>,
    native_image: blaze_drive::Key,
    checked: ContentId,
    release: bool,
    output_path: &str,
    scratch_alloc: Alloc<'a>,
    _frame: &Frame,
    heap: &'static blaze_own::Heap,
) -> Result<(DistExportOutcome, Digest), DistError> {
    let alloc = heap.alloc();
    let wire_alloc = dist.wire_alloc;

    let mut plans: AVec<'a, CratePlan<'a>> = Vec::new_in(scratch_alloc);
    let mut prior_digests: AVec<'a, Digest> = Vec::new_in(scratch_alloc);

    // Traverse crates in dependency order (records[0] is the program crate, so iterate in reverse
    // so dependencies come before dependents and the program crate is last).
    for idx in (0..closure.len()).rev() {
        let Some(rec) = names.records.items_record(idx) else { continue };
        let key = rec.key();
        if closure.iter().any(|c| c.key == key && c.toolchain) {
            continue;
        }
        let Ok(cname) = rec.name() else { continue };
        let CrateItems { functions: items, data } = collect_crate_items(names, idx, rec, ids, builder, scratch_alloc);
        let mut work_names = WireVec::with_capacity_in(items.len(), wire_alloc);
        for item in items.iter() {
            work_names.push(item.key.0.0);
        }
        let is_final = idx == 0;
        let mut extra: AVec<'a, Digest> = Vec::with_capacity_in(2 + if is_final { prior_digests.len() + 4 } else { 0 }, scratch_alloc);
        extra.push(key.0.0);
        extra.push(data.0);
        if is_final {
            extra.extend_from_slice(prior_digests.as_slice());
            extra.push(program_roots_digest(program_crate_name, roots, builder.settings.panic));
            extra.push(checked.0);
            extra.push(native_image.0.0);
            extra.push(reach::protocol());
        }
        let spec = unit_digest_of(cname, builder, release, work_names.as_slice(), extra.as_slice(), scratch_alloc);
        prior_digests.push(spec);
        plans.push(CratePlan {
            unit_name: own_str(cname, alloc),
            unit: Unit { spec, names: work_names },
            inputs: extra,
            items,
            is_final,
        });
    }

    let report = plans.iter().map(|plan| PlanReport {
        name: plan.unit_name.as_str(), unit: &plan.unit, inputs: &plan.inputs,
        work: Some(DevWorkReport { items: &plan.items, ids }),
    });
    write_unit_plans(dist.report.as_ref().map(|path| path.as_str()), report, wire_alloc);

    // The local executable also incorporates the checked native archives and export bindings.
    let fp = unit_digest_of(output_path, builder, release, prior_digests.as_slice(), &[native_image.0.0], scratch_alloc);
    if matches_output_fingerprint(output_path, &fp, wire_alloc) {
        return Ok((DistExportOutcome::NoOp, fp));
    }
    if let Some(unit) = dist.result_gate.as_ref().and_then(|gate| gate.unit.as_ref())
        && !plans.iter().any(|plan| plan.unit_name.as_str() == unit.as_str())
    {
        let mut shown = String::new_in(alloc);
        let _ = write!(shown, "{RESULT_UNIT_VAR} names {unit}, which has no distributed unit");
        return Err(DistError::BadResultGate { shown });
    }

    agent::ExportBudget::with_published_ahead(&dist.config, |budget| {
        let cancelled = AtomicBool::new(false);
        let mut total_compiled = 0usize;
        let mut total_cached = 0usize;
        let mut verified = None;

        for (index, plan) in plans.iter().enumerate() {
            let next = plans.get(index + 1).filter(|plan| !plan.is_final).map(|plan| plan.unit.spec);
            let mut exec = UnitExecutor {
                cx,
                plan,
                ids,
                builder,
                maker,
                store,
                roots,
                scratch_alloc,
                wire_alloc,
                heap,
                tail_compiled: 0,
                tail_from_store: 0,
                tail_instances: 0,
                tail_compiled_keys: WireVec::new_in(wire_alloc),
                ran_complete: false,
                result_gate: dist.result_gate.as_ref()
                    .and_then(|gate| gate.for_unit(plan.unit_name.as_str())),
            };
            let mut arrived = match budget.run_with_next(&plan.unit, next, &mut exec, &cancelled) {
                Ok(arrived) => arrived,
                Err(err) => match agent::decide(&dist.config.local, err, wire_alloc) {
                    Ok(()) => return Ok((DistExportOutcome::FallbackLocal, fp)),
                    Err(fatal) => {
                        let mut shown = String::new_in(alloc);
                        let _ = write!(shown, "{fatal}");
                        return Err(DistError::UnitFailed {
                            unit: own_str(plan.unit_name.as_str(), alloc),
                            shown,
                        });
                    }
                },
            };
            let tail_compiled = exec.tail_compiled;
            let tail_from_store = exec.tail_from_store;
            let tail_instances = exec.tail_instances;
            let tail_keys = exec.tail_compiled_keys;
            let work_compiled = usize::try_from(arrived.compiled_here).unwrap_or(0);
            let work_stored = usize::try_from(arrived.stored_here).unwrap_or(0);
            total_compiled += work_compiled + tail_compiled;
            total_cached += work_stored + tail_from_store;
            if plan.is_final {
                let _span = cx.span("export: authenticate distributed reachability entries");
                verified = Some(reach::Closure::read(plan.unit.spec, &mut arrived.values, heap)?);
            }
            store.merge_arrived(&arrived.values)?;
            append_report_row(
                dist.report.as_ref().map(|s| s.as_str()),
                plan.unit_name.as_str(),
                &plan.unit,
                &arrived,
                tail_compiled,
                tail_from_store,
                tail_instances,
                tail_keys.as_slice(),
                verified.as_ref(),
                wire_alloc,
            );
        }

        let reach = verified.ok_or_else(|| reach::refused(heap, fp, "has no program unit"))?;
        Ok((DistExportOutcome::Completed { compiled: total_compiled, cached: total_cached, reach }, fp))
    })
    .map_err(DistError::PublishedRead)?
}
