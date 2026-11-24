//! Distributed compilation and caching bridge for `blaze export` (D11).
//!
//! Connects the source compiler's whole-program code-record walk (`engine_export.rs`) to the
//! retained coordinator (`blaze-dist-local`) and content-addressed cache server (`blaze-serve`)
//! when `BLAZE_DIST_SOCKET` is set in the command's environment.
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
use blaze_dist_local::agent::{self, Arrived, Config, Grant, Granted, Source, Unit};
use blaze_dist_local::blobs::Blobs;
use blaze_dist_local::pack::{Value, ValueRef};
use blaze_dist_local::sys::{
    append_file, current_pid, path_is_file, random_id, read_file, write_file,
};
use blaze_dist_local::telemetry;
use blaze_ids::ContentId;
use blaze_ladder::body_ids::{self, RowKind, Scratch, Stepped, Walk};
use blaze_ladder::code_records::{Builder, CodeClass, CodeKey, CodeStore, StoreError};
use blaze_ladder::front2::keys::checker_of;
use blaze_ladder::run::RunError;
use blaze_ladder::run::make_one::{MOST_WALKED, Maker, NamesRecords, RecordsProgram, Typing};
use blaze_ladder::run::special::Roots;
use blaze_ladder::symbols::{encode_instance, fn_symbol_of_name};
use blaze_own::{AString, AVec, Alloc, BTreeMap, BTreeSet, String, Vec};
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

use crate::code_store::HomeCodeStore;
use crate::engine_seeds::CrateAt;
use crate::run_ids::RunIds;

/// Coordinator Unix socket environment variable.
const SOCKET_VAR: &str = "BLAZE_DIST_SOCKET";
/// Cache server `host:port` environment variable.
const BLOBS_VAR: &str = "BLAZE_DIST_BLOBS";
/// Local failure policy (`never` or `fallback`).
const LOCAL_VAR: &str = "BLAZE_DIST_LOCAL";
/// JSONL report output path.
const REPORT_VAR: &str = "BLAZE_DIST_REPORT";
/// Per-unit execution deadline in seconds.
const DEADLINE_VAR: &str = "BLAZE_DIST_DEADLINE_SECS";
/// Pull request namespace number.
const PR_VAR: &str = "BLAZE_DIST_PR";
/// Revision label within a pull request namespace.
const SOURCE_VAR: &str = "BLAZE_DIST_SOURCE";

/// Default per-unit execution deadline.
const DEFAULT_DEADLINE: Duration = Duration::from_secs(3_600);
/// Pack entry kind for a plain `ReservedTag::Code` record.
pub(crate) const KIND_CODE: u8 = 0;
/// Pack entry kind for a 32-byte `ReservedTag::Pointer` target (`CodeKey -> content CodeKey`).
pub(crate) const KIND_POINTER: u8 = 1;

/// Typed refusal from parsing distributed settings or running distributed units.
#[derive(Debug)]
pub(crate) enum DistError {
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
    Store { shown: AString<'static> },
}

impl fmt::Display for DistError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
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
            DistError::Store { shown } => {
                write!(f, "the distributed build failed: store: {}", shown.as_str())
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
    pub(crate) wire_alloc: WireAlloc,
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
            for name in [BLOBS_VAR, LOCAL_VAR, REPORT_VAR, DEADLINE_VAR, PR_VAR, SOURCE_VAR] {
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
        Ok(Some(DistConfig { config, report, wire_alloc }))
    }
}

/// Overlay `CodeStore`: committed values from peers/cache sit in front of `HomeCodeStore`, while
/// recording every read and write so both granted batches and lead tails export complete values.
pub(crate) struct DistStore<'s, 'm, D: Driver> {
    local: &'s mut HomeCodeStore<'m, D>,
    overlay: BTreeMap<Digest, (u8, WireVec<u8>), WireAlloc>,
    reads: RefCell<BTreeSet<CodeKey, WireAlloc>>,
    writes: WireVec<Value>,
    alloc: WireAlloc,
}

impl<'s, 'm, D: Driver> DistStore<'s, 'm, D> {
    pub(crate) fn new(local: &'s mut HomeCodeStore<'m, D>, alloc: WireAlloc) -> Self {
        Self {
            local,
            overlay: BTreeMap::new_in(alloc),
            reads: RefCell::new(BTreeSet::new_in(alloc)),
            writes: WireVec::new_in(alloc),
            alloc,
        }
    }

    pub(crate) fn clear_tracking(&mut self) {
        self.reads.borrow_mut().clear();
        self.writes.clear();
    }

    pub(crate) fn insert_overlay(&mut self, key: Digest, kind: u8, bytes: &[u8]) {
        if !self.overlay.contains_key(&key) {
            self.overlay.insert(key, (kind, copy_wire_bytes(bytes, self.alloc)));
        }
    }

    pub(crate) fn merge_arrived(&mut self, arrived: &Arrived) -> Result<(), DistError> {
        for (digest, (kind, bytes)) in &arrived.values {
            let key = CodeKey(ContentId(*digest));
            if *kind == KIND_CODE {
                let _ = self.local.put(CodeClass::Program, key, bytes.as_slice());
            } else if *kind == KIND_POINTER && bytes.len() == 32 {
                let mut target = [0u8; 32];
                target.copy_from_slice(bytes.as_slice());
                let _ = self.local.point(CodeClass::Program, key, CodeKey(ContentId(target)));
            }
            self.insert_overlay(*digest, *kind, bytes.as_slice());
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
        let written = core::mem::replace(&mut self.writes, WireVec::new_in(self.alloc));
        for val in written {
            by_key.insert(val.key, val);
        }
        let seen: WireVec<CodeKey> = {
            let set = self.reads.borrow();
            let mut list = WireVec::with_capacity_in(set.len(), self.alloc);
            for key in set.iter() {
                list.push(*key);
            }
            list
        };
        for key in seen {
            self.export_key(key, &mut by_key)?;
        }
        self.reads.borrow_mut().clear();
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
            self.reads.borrow_mut().insert(key);
            if *kind == KIND_CODE {
                return Ok(Some(bytes.as_slice()));
            }
            return Ok(None);
        }
        let res = self.local.get(key)?;
        if res.is_some() {
            self.reads.borrow_mut().insert(key);
        }
        Ok(res)
    }

    fn put(&mut self, class: CodeClass, key: CodeKey, bytes: &[u8]) -> Result<(), StoreError> {
        self.local.put(class, key, bytes)?;
        let digest = key.0.0;
        self.overlay.insert(digest, (KIND_CODE, copy_wire_bytes(bytes, self.alloc)));
        self.writes.push(Value {
            key: digest,
            kind: KIND_CODE,
            bytes: copy_wire_bytes(bytes, self.alloc),
        });
        Ok(())
    }

    fn pointed(&self, key: CodeKey) -> Result<Option<CodeKey>, StoreError> {
        let digest = key.0.0;
        if let Some((kind, bytes)) = self.overlay.get(&digest) {
            self.reads.borrow_mut().insert(key);
            if *kind == KIND_POINTER && bytes.len() == 32 {
                let mut target = [0u8; 32];
                target.copy_from_slice(bytes.as_slice());
                return Ok(Some(CodeKey(ContentId(target))));
            }
            return Ok(None);
        }
        let res = self.local.pointed(key)?;
        if let Some(target) = res {
            self.reads.borrow_mut().insert(key);
            self.reads.borrow_mut().insert(target);
        }
        Ok(res)
    }

    fn point(&mut self, class: CodeClass, key: CodeKey, content: CodeKey) -> Result<(), StoreError> {
        self.local.point(class, key, content)?;
        let digest = key.0.0;
        self.overlay.insert(digest, (KIND_POINTER, copy_wire_bytes(&content.0.0, self.alloc)));
        self.writes.push(Value {
            key: digest,
            kind: KIND_POINTER,
            bytes: copy_wire_bytes(&content.0.0, self.alloc),
        });
        Ok(())
    }
}

/// One concrete, pre-keyed function inside a crate's distributed unit.
struct WorkItem<'a> {
    tabled: ItemKey,
    name: AVec<'a, u8>,
    key: CodeKey,
    fold_candidate: bool,
}

/// One workspace crate's distributed unit plan.
struct CratePlan<'a> {
    unit_name: AString<'static>,
    unit: Unit,
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

/// Reads a `TextSpan` from `names` for crate `krate` without any lossy casts.
fn slice_span<'a>(names: &NamesRecords<'a>, krate: usize, span: TextSpan) -> Option<&'a str> {
    let whole = names.records.whole_text(krate, span.at)?;
    let start = usize::try_from(span.start).ok()?;
    let len = usize::try_from(span.len).ok()?;
    let end = start.checked_add(len)?;
    whole.get(start..end)
}

/// Collects concrete non-generic runnable functions from one crate's `ItemsRecord`.
fn collect_crate_items<'a, 'b>(
    names: NamesRecords<'b>,
    krate: usize,
    rec: &ItemsRecord<'b>,
    ids: &RunIds<'b>,
    builder: &Builder,
    alloc: Alloc<'a>,
) -> AVec<'a, WorkItem<'a>> {
    let mut raw: AVec<'a, WorkItem<'a>> = Vec::new_in(alloc);
    let tys = PortableTys::new_in(alloc);
    let ground = tys.view();

    let mut try_push = |item_key: ItemKey, fold_candidate: bool| {
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
        raw.push(WorkItem { tabled: item_key, name: encoded, key, fold_candidate });
    };

    for i in 0..rec.items() {
        let Ok(Some(item)) = rec.item_at(i) else { continue };
        if item.kind != ItemKind::Function || item.foreign.is_some() {
            continue;
        }
        let nongeneric = match item.text.and_then(|span| slice_span(&names, krate, span)) {
            Some(text) => header_is_nongeneric(text, true),
            None => false,
        };
        if nongeneric {
            try_push(checker_of(item.key), true);
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
                try_push(checker_of(mkey), true);
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
    deduped
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

/// Emits one `backend_work` JSONL event on `BLAZE_DIST_EVENTS` when `--release` is active.
fn emit_backend_work(
    unit_digest: &Digest,
    work: &[Digest],
    compiled_keys: &[Digest],
    release_keys: &[Digest],
    folded_keys: &[Digest],
    alloc: WireAlloc,
) {
    let mut pid_s = WireString::new_in(alloc);
    let _ = write!(pid_s, "{}", current_pid());
    let mut ud_s = WireString::new_in(alloc);
    let _ = write!(ud_s, "\"{}\"", hex(unit_digest));
    let work_s = json_digests(work, alloc);
    let compiled_s = json_digests(compiled_keys, alloc);
    let release_s = json_digests(release_keys, alloc);
    let folded_s = json_digests(folded_keys, alloc);
    telemetry::emit(
        "backend_work",
        &[
            ("pid", pid_s.as_str()),
            ("unit_digest", ud_s.as_str()),
            ("work", work_s.as_str()),
            ("compiled_keys", compiled_s.as_str()),
            ("release_keys", release_s.as_str()),
            ("folded_keys", folded_s.as_str()),
        ],
        alloc,
    );
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

    let mut line = WireString::new_in(alloc);
    let _ = write!(
        line,
        "{{\"unit\":\"{}\",\"source\":\"{}\",\"execution\":{},\"joined\":{},\"grants\":{},\"compiled_for_work\":{},\"stored_for_work\":{},\"arrived\":{},\"tail_compiled\":{},\"from_store\":{},\"instances\":{},\"unit_digest\":\"{}\",\"attempt\":{},\"publication_attempt\":{},\"work_compiled_keys\":{},\"tail_compiled_keys\":{}}}\n",
        unit_name,
        source,
        execution,
        arrived.joined,
        arrived.grants,
        arrived.compiled_here,
        arrived.stored_here,
        arrived.values.len(),
        tail_compiled,
        from_store,
        instances,
        hex(&unit.spec),
        attempt,
        pub_attempt,
        work_keys.as_str(),
        tail_keys.as_str(),
    );
    let _ = append_file(path, line.as_bytes());
}

/// Result of running the distributed export pass across all workspace crate units.
pub(crate) enum DistExportOutcome {
    /// Output executable is already up to date (exact fingerprint hit); no unit was run.
    NoOp,
    /// Distributed units finished; `DistStore` holds all committed/compiled records for export.
    Completed { compiled: usize, cached: usize },
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
    plan: &'a CratePlan<'a>,
    ids: &'a RunIds<'b>,
    builder: &'a Builder,
    maker: &'a mut Maker<'b, crate::run_ids::Runnable, T>,
    store: &'a mut DistStore<'s, 'm, D>,
    roots: &'a Roots<'a>,
    release: bool,
    scratch_alloc: Alloc<'a>,
    wire_alloc: WireAlloc,
    heap: &'static blaze_own::Heap,
    tail_compiled: usize,
    tail_from_store: usize,
    tail_instances: usize,
    tail_compiled_keys: WireVec<Digest>,
    ran_complete: bool,
}

impl<'b, D: Driver, T: Typing<RecordsProgram<'b>>> Granted for UnitExecutor<'_, 'b, '_, '_, D, T> {
    type Error = DistError;

    fn compile(&mut self, names: &[usize]) -> Result<Grant, DistError> {
        let mut grant_values: BTreeMap<Digest, Value, WireAlloc> = BTreeMap::new_in(self.wire_alloc);
        let mut compiled_keys: WireVec<Digest> = WireVec::new_in(self.wire_alloc);
        let mut release_keys: WireVec<Digest> = WireVec::new_in(self.wire_alloc);
        let mut folded_keys: WireVec<Digest> = WireVec::new_in(self.wire_alloc);
        let mut work_digests: WireVec<Digest> = WireVec::with_capacity_in(names.len(), self.wire_alloc);
        let mut compiled = 0u64;
        let mut stored = 0u64;
        let mut scratch = Scratch::new_in(self.scratch_alloc);

        for &idx in names {
            let Some(item) = self.plan.items.get(idx) else {
                continue;
            };
            work_digests.push(item.key.0.0);
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
                        let digest = code_key.0.0;
                        compiled_keys.push(digest);
                        if self.release {
                            release_keys.push(digest);
                            if item.fold_candidate {
                                folded_keys.push(digest);
                            }
                        }
                    }
                    Stepped::Unkeyed(_) => {}
                },
                Err(why) => {
                    let mut s = String::new_in(self.heap.alloc());
                    let _ = write!(s, "compiling item: {why}");
                    return Err(DistError::Compile { shown: s });
                }
            }
            let tracked = self.store.drain_tracked_values().map_err(|why| {
                let mut s = String::new_in(self.heap.alloc());
                let _ = write!(s, "{why}");
                DistError::Store { shown: s }
            })?;
            for val in tracked {
                grant_values.insert(val.key, val);
            }
        }

        if self.release && !compiled_keys.is_empty() {
            emit_backend_work(
                &self.plan.unit.spec,
                work_digests.as_slice(),
                compiled_keys.as_slice(),
                release_keys.as_slice(),
                folded_keys.as_slice(),
                self.wire_alloc,
            );
        }

        let mut values = WireVec::with_capacity_in(grant_values.len(), self.wire_alloc);
        for (_, val) in grant_values {
            values.push(val);
        }
        Ok(Grant { values, compiled, stored, compiled_keys })
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
                }
                Err(why)
                    if row >= root_count
                        && kind == RowKind::Instance
                        && why.scope() == blaze_ladder::scope::Scope::One => {}
                Err(why) => {
                    let mut s = String::new_in(self.heap.alloc());
                    let _ = write!(s, "tail compile: {why}");
                    return Err(DistError::Compile { shown: s });
                }
            }
        }
        let tracked = self.store.drain_tracked_values().map_err(|why| {
            let mut s = String::new_in(self.heap.alloc());
            let _ = write!(s, "{why}");
            DistError::Store { shown: s }
        })?;
        for val in tracked {
            output(val);
        }
        Ok(())
    }
}

/// Builds the ordered per-workspace-crate distributed unit plans, checks the output no-op
/// fingerprint, and executes every unit through `blaze_dist_local::agent::run`.
#[expect(clippy::too_many_arguments, reason = "explicit distributed export capabilities")]
pub(crate) fn run_distributed<'a, 'b, 's, 'm, D: Driver, T: Typing<RecordsProgram<'b>>>(
    dist: &DistConfig,
    names: NamesRecords<'b>,
    closure: &[CrateAt],
    program_crate_name: &str,
    ids: &'a RunIds<'b>,
    builder: &'a Builder,
    maker: &'a mut Maker<'b, crate::run_ids::Runnable, T>,
    store: &'a mut DistStore<'s, 'm, D>,
    roots: &'a Roots<'a>,
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
        let items = collect_crate_items(names, idx, rec, ids, builder, scratch_alloc);
        let mut work_names = WireVec::with_capacity_in(items.len(), wire_alloc);
        for item in items.iter() {
            work_names.push(item.key.0.0);
        }
        let is_final = idx == 0 || cname == program_crate_name;
        let mut extra: AVec<'a, Digest> = Vec::with_capacity_in(1 + if is_final { prior_digests.len() } else { 0 }, scratch_alloc);
        extra.push(key.0.0);
        if is_final {
            extra.extend_from_slice(prior_digests.as_slice());
        }
        let spec = unit_digest_of(cname, builder, release, work_names.as_slice(), extra.as_slice(), scratch_alloc);
        prior_digests.push(spec);
        plans.push(CratePlan {
            unit_name: own_str(cname, alloc),
            unit: Unit { spec, names: work_names },
            items,
            is_final,
        });
    }

    // Whole-program output fingerprint covering every unit digest and output path.
    let fp = unit_digest_of(output_path, builder, release, prior_digests.as_slice(), &[], scratch_alloc);
    if matches_output_fingerprint(output_path, &fp, wire_alloc) {
        return Ok((DistExportOutcome::NoOp, fp));
    }

    let cancelled = AtomicBool::new(false);
    let mut total_compiled = 0usize;
    let mut total_cached = 0usize;

    for plan in plans.iter() {
        let mut exec = UnitExecutor {
            plan,
            ids,
            builder,
            maker,
            store,
            roots,
            release,
            scratch_alloc,
            wire_alloc,
            heap,
            tail_compiled: 0,
            tail_from_store: 0,
            tail_instances: 0,
            tail_compiled_keys: WireVec::new_in(wire_alloc),
            ran_complete: false,
        };
        let arrived = match agent::run(&dist.config, &plan.unit, &mut exec, &cancelled) {
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
        store.merge_arrived(&arrived)?;
        append_report_row(
            dist.report.as_ref().map(|s| s.as_str()),
            plan.unit_name.as_str(),
            &plan.unit,
            &arrived,
            tail_compiled,
            tail_from_store,
            tail_instances,
            tail_keys.as_slice(),
            wire_alloc,
        );
    }

    Ok((DistExportOutcome::Completed { compiled: total_compiled, cached: total_cached }, fp))
}
