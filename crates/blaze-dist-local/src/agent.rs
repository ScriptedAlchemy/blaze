//! One daemon's side of a distributed unit, all of it: what `blaze`'s code generator calls
//! with the unit's canonical work names and a batch compiler, and gets back the values the
//! unit's executors committed. It compiles nothing itself and knows nothing of Rust.
//!
//! 1. A list published for this unit names everything an earlier build made: its packs are read
//!    and nothing is scheduled ([`Blobs::published`]).
//! 2. Otherwise the agent submits the unit (equal content joins the running execution), claims
//!    the executor assigned to it, and runs the holder ([`run_holder_until`]) while a watcher asks
//!    whether the compile phase is done. A batch is sealed into a pack and uploaded before its
//!    result is posted, so a committed result's bytes exist; a lease that moved is refused by the
//!    core, whatever this process believes.
//! 3. The current lead reads the committed packs, completes the ordinary unit build, and
//!    uploads its remaining values. The coordinator accepts its full manifest atomically
//!    against the lead attempt; only then is that immutable record mirrored to the cache.
//!    Helpers and late subscribers read that complete record instead of compiling a tail.
//!
//! The caller then runs its one build with these values in front of its store: what is here it
//! does not compile, and what is missing (a function the walk gave no executor, a batch an
//! executor refused) it compiles itself, by the same code, and reports. When the path fails,
//! [`decide`] says under the caller's [`LocalPolicy`] whether the build stops or compiles here.

use core::alloc::{Allocator, AllocatorClone};
use core::fmt;
use core::sync::atomic::{AtomicBool, Ordering};
use core::time::Duration;

use blaze_dist::holder::{BatchCompiler, Compiled, Holder, HolderConfig, Plan, StopWhy};
use blaze_dist::jobs::{LocalAction, MAX_ATTEMPTS, local_action};
use blaze_dist::{Ranges, Refusal};
use blaze_own::{BTreeMap, BTreeSet, String, Vec, format_in};
use blaze_wire::json::hex;
use blaze_wire::pack_list::{decode as parse_list, encode as list_of};
use blaze_wire::{
    Attempt, Digest, DoneEvent, EntryKey, EntryRef, ExecutionId, ItemKey, ItemOutcome, ItemResult,
    JobEvent, JobRequest, LocalPolicy, Namespace, Outcome, PackRef, Reservation, RootSet, Set, Why,
    WireAlloc, WireString, WireVec, WorkKey, WorkName, WorkersEventsRequest,
    digest_of_encoded_name, encode,
};

use crate::Error;
use crate::blobs::Blobs;
use crate::client::Client;
use crate::driver::{DriverConfig, run_holder_until};
use crate::pack::{Pack, Value, ValueRef, read};
use crate::protocol::{self as carrier, Answer, Binding, Command};
use crate::server::random_id;
use crate::sys::{SystemClock, ThreadFailed, current_pid, env_is_set, scope, sleep};

mod grant_events;
mod published_ahead;

use published_ahead::{BufferedPack, Publication, ReadAhead};

/// What every executor of a machine reserves: one holder's bookkeeping, not its compile, which
/// the daemon's own budget admits.
pub const NEED: Reservation = Reservation { memory: 1, cpus: 1, disk: 1 };

/// The machine's digest in admission: its triple's, so an agent of another host is not offered
/// a job of this one's. Keys cover the target, so this schedules; it does not make anything safe.
pub fn platform(triple: &str) -> Digest {
    digest_of_encoded_name("blaze/dist-platform", triple.as_bytes())
}

/// How a daemon reaches the coordinator and the packs.
#[derive(Clone, Debug)]
pub struct Config {
    /// The coordinator's socket.
    pub socket: WireString,
    /// The cache server.
    pub blobs: Blobs,
    /// This daemon's claim label, bound by the coordinator to each owned transport session.
    pub claimant: Digest,
    /// [`platform`] of the host triple.
    pub platform: Digest,
    /// What a failure of the distributed path means ([`decide`]).
    pub local: LocalPolicy,
    /// Whose build this is: the main line's, or a pull request's, which share no execution and
    /// run on the executors of their namespace. Published units are shared whatever it is.
    pub namespace: Namespace,
    /// Which build of a pull request this is ([`JobRequest::source`]).
    pub source: Digest,
    /// Execution budget for one main-line unit or for an entire pull request export.
    pub deadline: Duration,
    /// The holder's timings.
    pub holder: HolderConfig,
    /// How often the watcher and an unassigned agent ask.
    pub poll: Duration,
}

/// A unit's work: the digest all its executors agree on, and its names.
#[derive(Clone, Debug)]
pub struct Unit {
    /// What the unit is: covers everything its functions' keys cover, and its name.
    pub spec: Digest,
    /// The canonical work names: sorted, the same on every executor.
    pub names: WireVec<Digest>,
}

/// Which holders may pause at a unit's acceptance-test result gate.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ResultGateScope {
    /// Only the lead may pause.
    Lead,
    /// Each participant may pause in its own gate directory.
    All,
}

/// Acceptance-only configuration for a unit's real result.
#[derive(Clone, Copy, Debug)]
pub struct TestResultGate<'a> {
    /// Fresh absolute directory used to record and release the captured result.
    pub directory: &'a str,
    /// Which participants may capture a result.
    pub scope: ResultGateScope,
}

/// What compiles a grant: on the thread that called [`run`], which may own the front end.
pub trait Granted {
    /// The compiler/exporter's matchable refusal, retained until the carrier reports it.
    type Error: core::error::Error;

    /// Acceptance-only gate for this unit's real result, if configured.
    fn test_result_gate(&self) -> Option<TestResultGate<'_>> {
        None
    }

    /// The values of `names`, each index into [`Unit::names`]: from the store or compiled.
    /// `Err` gives the batch back: another executor or the final build gets to it, and a real
    /// compile error is reported there, by the build that links the unit.
    fn compile(&mut self, names: &[usize]) -> Result<Grant, Self::Error>;

    /// Complete the unit through its ordinary build with the committed phase values in
    /// front of its store. Only the current lead runs this tail; other subscribers take
    /// its full manifest. Emit any additional values through `output`; they are uploaded
    /// only after this call succeeds, before the manifest is committed.
    /// A repeated committed key must have the same kind and bytes; a mismatch refuses
    /// the entire tail before any of its values are uploaded.
    /// An error is a final compiler/export refusal under every local policy.
    fn complete(
        &mut self,
        _values: &mut dyn Iterator<Item = ValueRef<'_>>,
        _output: &mut dyn FnMut(Value),
    ) -> Result<(), Self::Error> {
        Ok(())
    }
}

/// One grant's values.
pub struct Grant {
    /// Every value the grant made or found.
    pub values: WireVec<Value>,
    /// How many functions this executor compiled for it.
    pub compiled: u64,
    /// How many it found in its own store.
    pub stored: u64,
    /// Actual store writes, in emission order, including repeated compiles.
    pub emissions: WireVec<Emission>,
}

/// A store write observed by the compiler.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Emission {
    /// The emitted store value's key.
    pub key: Digest,
    /// The machine output and issuing work name, when the renderer produced a native result.
    pub native: Option<NativeEmission>,
}

impl Emission {
    /// A store write that carries no optimized backend provenance.
    pub const fn plain(key: Digest) -> Self {
        Self { key, native: None }
    }
}

/// Provenance carried by the compiler that produced a native store value.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NativeEmission {
    /// The emitted machine record's role in the release plan.
    pub kind: BackendKind,
    /// The compiler's canonical grant name inside the unit.
    pub grant: Digest,
    /// The content key of the machine record this store value names.
    pub content: Digest,
}

/// A native release record's role as reported by its renderer.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BackendKind {
    /// Code emitted for a single release function and its imported definitions.
    ReleaseBlob,
    /// A shared folded body or the entry thunk for one member.
    FoldedOutput,
}

/// How a unit's values arrived.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    /// A list published by an earlier build: nothing was scheduled.
    Published,
    /// This build's execution.
    Executed,
}

/// What one unit's distributed path gave its build.
#[derive(Debug)]
pub struct Arrived {
    /// The committed values, by key.
    pub values: BTreeMap<Digest, (u8, WireVec<u8>), WireAlloc>,
    /// The packs they came from: what this unit's published list names.
    pub packs: WireVec<PackRef>,
    /// How they arrived.
    pub source: Source,
    /// The execution, for a [`Source::Executed`].
    pub execution: Option<u64>,
    /// Functions this executor compiled for the work set.
    pub compiled_here: u64,
    /// Functions it had already, given to the others.
    pub stored_here: u64,
    /// Grants this executor took.
    pub grants: u64,
    /// Whether this agent held an executor of the execution at all.
    pub joined: bool,
    /// The final observed execution attempt, or None for a published-cache hit.
    pub attempt: Option<u64>,
    /// The attempt that accepted the immutable full manifest, before any reassignment.
    pub publication_attempt: Option<u64>,
    /// Actual emitted store keys of this agent's grant compiles, never store hits.
    pub compiled_keys: WireVec<Digest>,
}

impl Arrived {
    /// Nothing yet, from `source`.
    fn none(source: Source, execution: Option<u64>, alloc: WireAlloc) -> Arrived {
        Arrived {
            values: BTreeMap::new_in(alloc),
            packs: Vec::new_in(alloc),
            source,
            execution,
            compiled_here: 0,
            stored_here: 0,
            grants: 0,
            joined: false,
            attempt: None,
            publication_attempt: None,
            compiled_keys: Vec::new_in(alloc),
        }
    }
}

fn duration_ms(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

fn elapsed_since(clock: SystemClock, started_ns: u64) -> Duration {
    let now_ns = clock.monotonic_nanos().unwrap_or(started_ns);
    Duration::from_nanos(now_ns.saturating_sub(started_ns))
}

/// The distributed execution budget of one export. For a pull request, all uncached units
/// reuse the deadline established by the first coordinator handshake. That deadline bounds
/// how long the coordinator must remember the source after it has been superseded.
pub struct ExportBudget<'a> {
    config: &'a Config,
    pr: Option<PrBudget>,
    ahead: Option<ReadAhead>,
}

struct PrBudget {
    epoch: Digest,
    deadline_ms: u64,
    started_ns: u64,
    limit: Duration,
}

impl PrBudget {
    fn remaining(&self, now_ns: u64) -> Duration {
        let elapsed = Duration::from_nanos(now_ns.saturating_sub(self.started_ns));
        self.limit.saturating_sub(elapsed)
    }
}

#[derive(Clone, Copy)]
struct UnitBudget {
    epoch: Option<Digest>,
    deadline_ms: u64,
    started_ns: u64,
    limit: Duration,
}

impl<'a> ExportBudget<'a> {
    /// A budget whose elapsed time starts when the export first needs the coordinator.
    pub fn new(config: &'a Config) -> Self {
        Self { config, pr: None, ahead: None }
    }

    fn admit(
        &mut self,
        unit: &Unit,
        welcome: &carrier::LocalWelcome,
        started_ns: u64,
        now_ms: u64,
    ) -> Result<UnitBudget, Error> {
        let limit = self.config.deadline;
        if matches!(self.config.namespace, Namespace::Main) {
            return Ok(UnitBudget {
                epoch: None,
                deadline_ms: now_ms.saturating_add(duration_ms(limit)),
                started_ns,
                limit,
            });
        }
        let pr = self.pr.get_or_insert_with(|| PrBudget {
            epoch: welcome.epoch,
            deadline_ms: welcome.now_ms.saturating_add(duration_ms(limit)),
            started_ns,
            limit,
        });
        let coordinator_left = Duration::from_millis(pr.deadline_ms.saturating_sub(welcome.now_ms));
        let remaining = pr.remaining(started_ns).min(coordinator_left);
        let budget = UnitBudget {
            epoch: Some(pr.epoch),
            deadline_ms: pr.deadline_ms,
            started_ns,
            limit: remaining,
        };
        let alloc = self.config.blobs.alloc;
        budget.check_epoch(unit, &welcome.epoch, alloc)?;
        if remaining.is_zero() {
            return Err(pr_expired(unit, alloc));
        }
        Ok(budget)
    }
}

impl UnitBudget {
    fn remaining(self, clock: SystemClock) -> Duration {
        self.limit.saturating_sub(elapsed_since(clock, self.started_ns))
    }

    fn check_epoch(self, unit: &Unit, epoch: &Digest, alloc: WireAlloc) -> Result<(), Error> {
        if self.epoch.is_some_and(|expected| &expected != epoch) {
            return Err(Error::Cancelled(format_in!(
                alloc,
                "unit {}: pull request export lost its coordinator epoch",
                hex(&unit.spec)
            )));
        }
        Ok(())
    }

    fn check_active(self, unit: &Unit, clock: SystemClock, alloc: WireAlloc) -> Result<(), Error> {
        if self.epoch.is_some() && self.remaining(clock).is_zero() {
            return Err(pr_expired(unit, alloc));
        }
        Ok(())
    }

    fn poll_wait(self, clock: SystemClock, wait: Duration) -> Duration {
        if self.epoch.is_some() { wait.min(self.remaining(clock)) } else { wait }
    }

    fn connect_worker(self, config: &Config, unit: &Unit) -> Result<Client, Error> {
        let alloc = config.blobs.alloc;
        let mut client = Client::connect(config.socket.as_str(), random_id()?, alloc)?;
        if let Err(error) = self.check_epoch(unit, &client.welcome.epoch, alloc) {
            let _ = client.release();
            return Err(error);
        }
        Ok(client)
    }
}

fn pr_expired(unit: &Unit, alloc: WireAlloc) -> Error {
    Error::Cancelled(format_in!(alloc, "unit {}: pull request export deadline expired", hex(&unit.spec)))
}

/// The coordinator's work identity for an opaque compiler task in one unit.
pub fn wire_name(unit: &Digest, name: &Digest, alloc: WireAlloc) -> WorkName {
    WorkName::of(&WorkKey::Item(ItemKey { unit: *unit, stable: *name, env: [0; 32] }), alloc)
}

/// The set every executor of `unit` joins with.
pub fn root_set(unit: &Unit, alloc: WireAlloc) -> RootSet {
    let mut pairs = Vec::with_capacity_in(unit.names.len(), alloc);
    for name in &unit.names {
        pairs.push((wire_name(&unit.spec, name, alloc), *name));
    }
    let count = u64::try_from(pairs.len()).unwrap_or(u64::MAX);
    let empty: WireVec<(WorkName, Digest)> = Vec::new_in(alloc);
    RootSet {
        spec: unit.spec,
        roots: digest_of_encoded_name("blaze/root-set", &encode(&pairs, alloc)),
        count,
        index: digest_of_encoded_name("blaze/pre-index", &encode(&empty, alloc)),
        inputs: [0; 32],
    }
}

/// The job of a unit: what its executors agree on, and this subscriber's terms.
fn request(unit: &Unit, config: &Config, budget: UnitBudget) -> JobRequest {
    JobRequest {
        unit: unit.spec,
        platform: config.platform,
        namespace: config.namespace.clone(),
        source: config.source,
        deadline_ms: budget.deadline_ms,
        local: config.local.clone(),
    }
}

/// What the requester does about `failure`, under `policy`: `Ok` compiles the unit here, by the
/// one build; `Err` stops the build, naming what failed. A terminal goes by its contract
/// ([`local_action`]): a verdict is taken, anything else builds here only if asked. A failure
/// that names no outcome (like a disconnected carrier) is no verdict. A missing or corrupt
/// committed pack and a final compiler/export refusal always fail, regardless of the
/// requested local policy.
pub fn decide(policy: &LocalPolicy, failure: Error, alloc: WireAlloc) -> Result<(), Error> {
    let action = match &failure {
        Error::Ended(termination) => local_action(policy, termination, alloc)
            .map_err(|why| Error::Protocol(format_in!(alloc, "the terminal is refused: {why:?}")))?,
        Error::Committed(_) | Error::Refused(_) | Error::Cancelled(_) => LocalAction::Fail,
        Error::Io(_) | Error::Protocol(_) => match policy {
            LocalPolicy::Never => LocalAction::Fail,
            LocalPolicy::Fallback => LocalAction::BuildLocally,
        },
    };
    match action {
        LocalAction::BuildLocally => Ok(()),
        LocalAction::Take | LocalAction::Fail => Err(failure),
    }
}

/// After a full manifest has been accepted, a delivery, heartbeat or cache
/// failure cannot authorize compiling the immutable unit again. Preserve an
/// explicit caller cancellation, a compiler refusal and an already typed committed-data refusal.
fn after_publication<T>(
    unit: &Unit,
    result: Result<T, Error>,
    alloc: WireAlloc,
) -> Result<T, Error> {
    result.map_err(|error| match error {
        Error::Committed(_) | Error::Refused(_) | Error::Cancelled(_) => error,
        other => Error::Committed(format_in!(
            alloc,
            "unit {}: immutable publication already accepted: {other}",
            hex(&unit.spec)
        )),
    })
}

struct Batches<'a, G: Granted + ?Sized> {
    unit: &'a Unit,
    blobs: &'a Blobs,
    granted: &'a mut G,
    compiled: u64,
    stored: u64,
    grants: u64,
    compiled_keys: WireVec<Digest>,
}

#[derive(Debug)]
enum BatchError {
    NoncanonicalRoots { unit: Digest },
    RootIndex { unit: Digest, index: u64 },
    EmissionGrant { unit: Digest, grant: Digest },
    ConflictingEmission { unit: Digest, key: Digest },
    Pack { unit: Digest, source: crate::pack::PackError },
    Upload { unit: Digest, source: Error },
}

impl fmt::Display for BatchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoncanonicalRoots { unit } => {
                write!(f, "unit {}: granted root ranges are not canonical", hex(unit))
            }
            Self::RootIndex { unit, index } => {
                write!(f, "unit {}: granted root {index} is outside the root set", hex(unit))
            }
            Self::EmissionGrant { unit, grant } => {
                write!(f, "unit {}: native output names a grant outside the batch: {}", hex(unit), hex(grant))
            }
            Self::ConflictingEmission { unit, key } => {
                write!(f, "unit {}: output {} has conflicting native receipts", hex(unit), hex(key))
            }
            Self::Pack { unit, source } => {
                write!(f, "unit {}: cannot pack the granted values: {source}", hex(unit))
            }
            Self::Upload { unit, source } => {
                write!(f, "unit {}: cannot upload the granted pack: {source}", hex(unit))
            }
        }
    }
}

impl core::error::Error for BatchError {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::Pack { source, .. } => Some(source),
            Self::Upload { source, .. } => Some(source),
            _ => None,
        }
    }
}

impl<G: Granted + ?Sized> BatchCompiler for Batches<'_, G> {
    type Error = BatchError;

    fn compile(&mut self, plan: &Plan) -> Result<Compiled, Self::Error> {
        let alloc = self.blobs.alloc;
        let ranges = Ranges::from_wire(&plan.grant.roots, alloc)
            .ok_or(BatchError::NoncanonicalRoots { unit: self.unit.spec })?;
        let mut indexes = Vec::new_in(alloc);
        indexes.extend(ranges.indexes());
        let mut names = Vec::with_capacity_in(indexes.len(), alloc);
        for index in &indexes {
            let at = usize::try_from(*index).ok().filter(|at| *at < self.unit.names.len());
            names.push(at.ok_or(BatchError::RootIndex { unit: self.unit.spec, index: *index })?);
        }
        let events = if env_is_set("BLAZE_DIST_EVENTS", alloc) {
            let fields = grant_events::issued(self.unit, plan, &names, current_pid(), alloc)?;
            crate::telemetry::emit("grant", &fields, alloc);
            Some(fields)
        } else {
            None
        };
        self.grants = self.grants.saturating_add(1);
        let (exported, packs): (bool, WireVec<PackRef>) = match self.granted.compile(&names) {
            Ok(grant) => {
                let native = grant_events::native_outputs(self.unit, &names, &grant.emissions, alloc)?;
                if let Some(mut fields) = events
                    && !native.is_empty()
                {
                    grant_events::backend(&mut fields, self.unit.spec, &grant.emissions, &native, alloc);
                    crate::telemetry::emit("backend_work", &fields, alloc);
                }
                self.compiled = self.compiled.saturating_add(grant.compiled);
                self.stored = self.stored.saturating_add(grant.stored);
                self.compiled_keys.extend(grant.emissions.into_iter().map(|output| output.key));
                let mut references = Vec::new_in(alloc);
                // Every pack is uploaded before this grant's result is posted.
                for pack in Pack::batches(grant.values, alloc)
                    .map_err(|source| BatchError::Pack { unit: self.unit.spec, source })?
                {
                    self.blobs
                        .put_pack(&pack, alloc)
                        .map_err(|source| BatchError::Upload { unit: self.unit.spec, source })?;
                    references.push(pack.reference);
                }
                (true, references)
            }
            Err(_) => (false, Vec::new_in(alloc)),
        };
        let mut root_results = Vec::with_capacity_in(names.len(), alloc);
        for at in &names {
            let Some(name) = self.unit.names.get(*at) else {
                return Err(BatchError::RootIndex {
                    unit: self.unit.spec,
                    index: u64::try_from(*at).unwrap_or(u64::MAX),
                });
            };
            let outcome = if exported {
                ItemOutcome::Exported(EntryRef { entry: EntryKey(*name) })
            } else {
                ItemOutcome::Local(Why {
                    why: WireString::from_str_in("the batch was given back", alloc),
                })
            };
            root_results.push(ItemResult {
                name: wire_name(&self.unit.spec, name, alloc),
                outcome,
                found: Set::new(Vec::new_in(alloc)),
                unnamed: Set::new(Vec::new_in(alloc)),
            });
        }
        let mut name_results = Vec::with_capacity_in(plan.compile.len(), alloc);
        for name in &plan.compile {
            name_results.push(ItemResult {
                name: *name,
                outcome: ItemOutcome::Lost(Why {
                    why: WireString::from_str_in(
                        "unknown-name: a V2 root set finds no names",
                        alloc,
                    ),
                }),
                found: Set::new(Vec::new_in(alloc)),
                unnamed: Set::new(Vec::new_in(alloc)),
            });
        }
        Ok(Compiled {
            generation: plan.grant.lease.generation,
            roots: root_results,
            names: name_results,
            packs,
            returned_roots: Ranges::new(alloc),
            returned_names: Vec::new_in(alloc),
        })
    }

    fn release(&mut self, _names: &[WorkName]) {}
}

/// How long past a job's deadline its requester still waits for the coordinator to end it.
const GRACE: Duration = Duration::from_secs(5);

/// The reason reported when the caller cancels the distributed build.
const CANCELLED: &str = "the build was cancelled";

/// A cancelled request is terminal even if the carrier or local policy permits a retry.
fn ensure_not_cancelled(
    unit: &Unit,
    cancelled: &AtomicBool,
    alloc: WireAlloc,
) -> Result<(), Error> {
    if cancelled.load(Ordering::Acquire) {
        Err(Error::Cancelled(format_in!(alloc, "unit {}: {CANCELLED}", hex(&unit.spec))))
    } else {
        Ok(())
    }
}

/// Reads the packs named, each checked against what was committed.
pub fn import<A: AllocatorClone>(
    blobs: &Blobs,
    packs: &[PackRef],
    alloc: A,
) -> Result<BTreeMap<Digest, (u8, Vec<u8, A>), A>, Error> {
    import_using(packs, |reference| blobs.committed_pack(reference, &alloc), alloc.clone(), blobs.alloc)
}

/// The retrieval boundary is injected so loss and corruption of a committed pack can be
/// tested without replacing the real pack decoder or requiring a live cache server.
fn import_using<A: AllocatorClone, B: Allocator>(
    packs: &[PackRef],
    mut fetch: impl FnMut(&PackRef) -> Result<Option<Vec<u8, B>>, Error>,
    alloc: A,
    error_alloc: WireAlloc,
) -> Result<BTreeMap<Digest, (u8, Vec<u8, A>), A>, Error> {
    let mut values: BTreeMap<Digest, (u8, Vec<u8, A>), A> = BTreeMap::new_in(alloc.clone());
    for reference in packs {
        let name = hex(&reference.pack);
        let bytes = fetch(reference)
            .map_err(|error| {
                Error::Committed(format_in!(
                    error_alloc,
                    "pack {name}: committed retrieval failed: {error}"
                ))
            })?
            .ok_or_else(|| {
                Error::Committed(format_in!(error_alloc, "pack {name} was committed and is gone"))
            })?;
        for value in read(&bytes, reference, bytes.allocator())
            .map_err(|error| Error::Committed(format_in!(error_alloc, "{error}")))?
        {
            if let Some((kind, existing)) = values.get(&value.key) {
                if *kind != value.kind || existing.as_slice() != value.bytes {
                    return Err(Error::Committed(format_in!(
                        error_alloc,
                        "two values under the key {} in the packs of one unit, the second in {name}",
                        hex(&value.key)
                    )));
                }
            } else {
                values.insert(
                    value.key,
                    (value.kind, Vec::from_slice_in(value.bytes, alloc.clone())),
                );
            }
        }
    }
    Ok(values)
}

/// A corrupted published manifest cannot authorize recompilation.
fn published_packs<A: Allocator>(
    spec: &Digest,
    list: &[u8],
    alloc: A,
    error_alloc: WireAlloc,
) -> Result<Vec<PackRef, A>, Error> {
    parse_list(list, alloc).map_err(|why| {
        Error::Committed(format_in!(
            error_alloc,
            "unit {}: published pack list is not canonical: {why}",
            hex(spec)
        ))
    })
}

/// How long an agent waits between asks at most, however long it has waited: what it asks is a
/// state of the coordinator's that a wait of this does not miss by much.
const POLL_MOST: Duration = Duration::from_millis(250);

fn answer(
    client: &mut Client,
    command: Command,
    what: &str,
    alloc: WireAlloc,
) -> Result<Answer, Error> {
    match client.call(command)? {
        Answer::Error(error) => Err(Error::Protocol(format_in!(
            alloc,
            "{what}: {} ({:?})",
            error.message,
            error.code()
        ))),
        other => Ok(other),
    }
}

/// Runs `unit` as far as the distributed path takes it. See the module's docs for the steps.
pub fn run(
    config: &Config,
    unit: &Unit,
    granted: &mut (impl Granted + ?Sized),
    cancelled: &AtomicBool,
) -> Result<Arrived, Error> {
    ExportBudget::new(config).run(unit, granted, cancelled)
}

impl ExportBudget<'_> {
    /// Runs one unit of this export. A published manifest requires no coordinator session.
    pub fn run(
        &mut self,
        unit: &Unit,
        granted: &mut (impl Granted + ?Sized),
        cancelled: &AtomicBool,
    ) -> Result<Arrived, Error> {
        self.run_with_next(unit, None, granted, cancelled)
    }

    /// Runs a unit in plan order, starting the next unit's bounded cache read before this
    /// unit uses the coordinator. The next unit still has to reach its own turn to use it.
    pub fn run_with_next(
        &mut self,
        unit: &Unit,
        next: Option<Digest>,
        granted: &mut (impl Granted + ?Sized),
        cancelled: &AtomicBool,
    ) -> Result<Arrived, Error> {
        let config = self.config;
        let alloc = config.blobs.alloc;
        ensure_not_cancelled(unit, cancelled, alloc)?;
        let ready = match &mut self.ahead {
            Some(ahead) => ahead.before(unit.spec, next)?,
            None => None,
        };
        let published = match ready {
            Some(published) => Some(published),
            None => config.blobs.published(&unit.spec, alloc)?
                .map(|list| {
                    let packs = published_packs(&unit.spec, &list, alloc, alloc)?;
                    Ok::<_, Error>(Publication { packs, first: None })
                })
                .transpose()?,
        };
        if let Some(Publication { packs, mut first }) = published {
            let values = import_using(
                &packs,
                |reference| {
                    if first.as_ref().is_some_and(|pack| pack.key == reference.pack) {
                        return Ok(first.take().map(|BufferedPack { bytes, .. }| bytes));
                    }
                    config.blobs.committed_pack(reference, alloc)
                },
                alloc,
                alloc,
            )?;
            ensure_not_cancelled(unit, cancelled, alloc)?;
            return Ok(Arrived { values, packs, ..Arrived::none(Source::Published, None, alloc) });
        }
        let clock = SystemClock;
        let before_handshake = clock.monotonic_nanos()?;
        if self.pr.as_ref().is_some_and(|pr| pr.remaining(before_handshake).is_zero()) {
            return Err(pr_expired(unit, alloc));
        }
        let mut client = Client::connect(config.socket.as_str(), random_id()?, alloc)?;
        let started_ns = if matches!(config.namespace, Namespace::Pr(_)) {
            clock.monotonic_nanos()?
        } else {
            before_handshake
        };
        let origin = client.welcome.now_ms;
        let now_ms = move || {
            let elapsed_ms = duration_ms(elapsed_since(clock, started_ns));
            origin.saturating_add(elapsed_ms)
        };
        let result = match self.admit(unit, &client.welcome, started_ns, now_ms()) {
            Ok(budget) => {
                run_connected(config, unit, granted, cancelled, clock, budget, &now_ms, &mut client)
            }
            Err(error) => Err(error),
        };
        let _ = client.release();
        result
    }
}

fn submission_error(unit: &Unit, error: &blaze_wire::WireError, alloc: WireAlloc) -> Error {
    match Refusal::of(error) {
        Some(Refusal::Superseded | Refusal::PrDeadlineExpired) => Error::Cancelled(format_in!(
            alloc,
            "unit {}: submit: {}",
            hex(&unit.spec),
            error.message
        )),
        _ => Error::Protocol(format_in!(alloc, "submit: {}", error.message)),
    }
}

#[allow(clippy::too_many_arguments)]
fn run_connected(
    config: &Config,
    unit: &Unit,
    granted: &mut (impl Granted + ?Sized),
    cancelled: &AtomicBool,
    clock: SystemClock,
    budget: UnitBudget,
    now_ms: &(impl Fn() -> u64 + Sync),
    client: &mut Client,
) -> Result<Arrived, Error> {
    let alloc = config.blobs.alloc;
    let job = request(unit, config, budget);
    let binding = loop {
        ensure_not_cancelled(unit, cancelled, alloc)?;
        budget.check_active(unit, clock, alloc)?;
        let submit = Command::Submit(carrier::submission(&job, NEED, alloc));
        match client.call(submit)? {
            Answer::Binding(binding) => break binding,
            Answer::Error(error) if error.code() == Some(blaze_wire::ErrorCode::Busy) => {
                ensure_not_cancelled(unit, cancelled, alloc)?;
                budget.check_active(unit, clock, alloc)?;
                if budget.epoch.is_none() && elapsed_since(clock, budget.started_ns) > budget.limit {
                    return Err(submission_error(unit, &error, alloc));
                }
                let retry = Duration::from_millis(error.retry_after_ms.unwrap_or(100));
                sleep(budget.poll_wait(clock, retry));
            }
            Answer::Error(error) => {
                return Err(submission_error(unit, &error, alloc));
            }
            other => {
                return Err(Error::Protocol(format_in!(alloc, "submit answered {other:?}")));
            }
        }
    };
    let mut assignments = BTreeSet::new_in(alloc);
    let result = execute(
        config,
        unit,
        granted,
        cancelled,
        clock,
        budget,
        now_ms,
        &binding,
        client,
        &mut assignments,
        alloc,
    );
    let result = ensure_not_cancelled(unit, cancelled, alloc).and(result);
    if result.is_err() {
        leave(client, &binding);
    }
    // execute has joined every compiler/heartbeat scope. Only its recorded slots
    // can be cleaned up. An owned lookup observes a promotion that cancellation
    // prevented the loop from seeing, without claiming any other assignment.
    // Retirement and the separate ACK still name the exact returned token.
    // Preserve the original failure if cleanup loses its carrier; successful
    // publication still requires its stop acknowledgment.
    let acknowledged = assignments.iter().try_for_each(|(executor, _)| {
        retire_owned(
            client,
            binding.subscription.content,
            binding.subscription.execution,
            executor.as_str(),
            config.claimant,
            alloc,
        )
    });
    result.and_then(|arrived| after_publication(unit, acknowledged, alloc).map(|()| arrived))
}

#[allow(clippy::too_many_arguments)]
fn execute<A: AllocatorClone>(
    config: &Config,
    unit: &Unit,
    granted: &mut (impl Granted + ?Sized),
    cancelled: &AtomicBool,
    clock: SystemClock,
    budget: UnitBudget,
    now_ms: &(impl Fn() -> u64 + Sync),
    binding: &Binding,
    client: &mut Client,
    assignments: &mut BTreeSet<(String<A>, Attempt), A>,
    alloc: A,
) -> Result<Arrived, Error> {
    let wire_alloc = config.blobs.alloc;
    let content = binding.subscription.content;
    let execution = binding.subscription.execution;
    let mut arrived = Arrived::none(Source::Executed, Some(execution.0), wire_alloc);
    let mut waited = config.poll;
    loop {
        ensure_not_cancelled(unit, cancelled, wire_alloc)?;
        budget.check_active(unit, clock, wire_alloc)?;
        if budget.epoch.is_none()
            && elapsed_since(clock, budget.started_ns) > budget.limit.saturating_add(GRACE)
        {
            return Err(Error::Protocol(format_in!(
                wire_alloc,
                "unit {}: its execution did not finish in {:?}",
                hex(&unit.spec),
                budget.limit
            )));
        }
        // Claim before reading phase completion: even a subscriber arriving after that
        // phase must know whether it is the current lead or a reader of the lead's tail.
        let claim =
            Command::Claim(carrier::Claim { content, execution, claimant: config.claimant });
        let claimed = match client.call(claim)? {
            Answer::Claimed(claimed) => Some(claimed),
            Answer::Error(error) if Refusal::of(&error) == Some(Refusal::Ended) => None,
            other => {
                return Err(Error::Protocol(format_in!(wire_alloc, "claim answered {other:?}")));
            }
        };
        if let Some(claimed) = &claimed
            && let Some(executor) = &claimed.executor
        {
            arrived.attempt = Some(u64::from(claimed.attempt.0));
            let label = String::from_str_in(executor.as_str(), alloc.clone());
            assignments.insert((label, claimed.attempt));
        }
        if let carrier::Publication {
            packs: Some(packs),
            attempt,
            accepted_attempt: Some(accepted),
        } = publication(client, content, execution, wire_alloc)?
        {
            arrived.attempt = Some(u64::from(attempt.0));
            arrived.publication_attempt = Some(u64::from(accepted.0));
            let take = || {
                arrived.values = import(&config.blobs, &packs, wire_alloc)?;
                arrived.packs = packs;
                ensure_not_cancelled(unit, cancelled, wire_alloc)?;
                // Delivery carries no new output authority, even after lead replacement.
                mirror(config, unit, &arrived.packs)?;
                ensure_not_cancelled(unit, cancelled, wire_alloc)?;
                deliver(client, binding, &arrived.packs, wire_alloc)?;
                Ok(())
            };
            let completed = if let Some(claimed) = &claimed
                && claimed.lead
                && let Some(executor) = &claimed.executor
            {
                heartbeat_during(
                    config,
                    unit,
                    content,
                    execution,
                    executor.as_str(),
                    claimed.attempt,
                    budget,
                    clock,
                    now_ms,
                    cancelled,
                    |_| take(),
                )
            } else {
                take()
            };
            after_publication(unit, completed, wire_alloc)?;
            return Ok(arrived);
        }
        let Some(claimed) = claimed else {
            return Err(ended(client, binding, wire_alloc));
        };
        let Some(executor) = claimed.executor else {
            sleep(budget.poll_wait(clock, waited));
            waited = waited.saturating_mul(2).min(POLL_MOST);
            continue;
        };
        arrived.joined = true;
        if claimed.lead {
            // A replacement must agree on the retained root set before taking the tail,
            // even when all work committed before it acquired the lead attempt.
            let join = Command::Join(carrier::Join {
                content,
                executor: executor.clone(),
                request: blaze_wire::WorkJoinRequest {
                    execution,
                    attempt: claimed.attempt,
                    roots: root_set(unit, wire_alloc),
                    rejected: Set::new(Vec::new_in(wire_alloc)),
                },
            });
            match answer(client, join, "the lead joining the unit", wire_alloc)? {
                Answer::Join(_) => {}
                other => {
                    return Err(Error::Protocol(format_in!(
                        wire_alloc,
                        "join answered {other:?}"
                    )));
                }
            }
        }
        if let Some(packs) = committed(client, content, execution, wire_alloc)?.packs {
            if !claimed.lead {
                // The helper's work is committed, but the whole unit is not. It must not
                // independently compile the tail during the lead's publication window.
                sleep(budget.poll_wait(clock, waited));
                waited = waited.saturating_mul(2).min(POLL_MOST);
                continue;
            }
            let publication_accepted = AtomicBool::new(false);
            let committed = heartbeat_during(
                config,
                unit,
                content,
                execution,
                executor.as_str(),
                claimed.attempt,
                budget,
                clock,
                now_ms,
                cancelled,
                |lost| {
                    arrived.values = import(&config.blobs, &packs, wire_alloc)?;
                    ensure_not_cancelled(unit, cancelled, wire_alloc)?;
                    budget.check_active(unit, clock, wire_alloc)?;
                    let mut refuse = |why: WireString| {
                        if !lost.load(Ordering::Acquire) {
                            let _ = verdict(
                                client,
                                content,
                                execution,
                                executor.as_str(),
                                claimed.attempt,
                                Outcome::Failed,
                                wire_alloc,
                            );
                        }
                        Error::Refused(format_in!(wire_alloc, "unit {}: {why}", hex(&unit.spec)))
                    };
                    let mut extra = Vec::new_in(wire_alloc);
                    {
                        let mut values = arrived.values.iter().map(|(key, (kind, bytes))| {
                            ValueRef { key: *key, kind: *kind, bytes: bytes.as_slice() }
                        });
                        granted
                            .complete(&mut values, &mut |value| extra.push(value))
                            .map_err(|error| refuse(format_in!(wire_alloc, "{error}")))?;
                    }
                    ensure_not_cancelled(unit, cancelled, wire_alloc)?;
                    if lost.load(Ordering::Acquire) {
                        return Ok(None);
                    }
                    // A tail's own packs may be canonical yet disagree with the phase it
                    // completed. Check the whole buffer before any upload or immutable
                    // publication, while these committed values are still borrowed here.
                    for value in &extra {
                        if let Some((kind, bytes)) = arrived.values.get(&value.key)
                            && (*kind != value.kind || bytes.as_slice() != value.bytes.as_slice())
                        {
                            return Err(refuse(format_in!(
                                wire_alloc,
                                "full output conflicts with the committed value under key {}",
                                hex(&value.key)
                            )));
                        }
                    }
                    let mut packs = packs;
                    if !extra.is_empty() {
                        for pack in Pack::batches(extra, wire_alloc).map_err(|why| {
                            refuse(format_in!(wire_alloc, "full output cannot be packed: {why}"))
                        })? {
                            config.blobs.put_pack(&pack, wire_alloc)?;
                            packs.push(pack.reference);
                        }
                    }
                    packs.sort();
                    packs.dedup();
                    ensure_not_cancelled(unit, cancelled, wire_alloc)?;
                    budget.check_active(unit, clock, wire_alloc)?;
                    let commit = Command::CommitPublication(carrier::CommitPublication {
                        content,
                        execution,
                        attempt: claimed.attempt,
                        executor: executor.clone(),
                        packs,
                    });
                    // The commit may have succeeded even if its reply was lost.
                    // A transport failure cannot permit a local rebuild under
                    // this unit's immutable key.
                    let reply = client.call(commit).map_err(|error| {
                        Error::Committed(format_in!(
                            wire_alloc,
                            "unit {}: publication commit outcome unknown: {error}",
                            hex(&unit.spec)
                        ))
                    })?;
                    match reply {
                        Answer::Publication(carrier::Publication {
                            packs: Some(packs),
                            accepted_attempt: Some(accepted),
                            ..
                        }) => {
                            publication_accepted.store(true, Ordering::Release);
                            arrived.publication_attempt = Some(u64::from(accepted.0));
                            after_publication(
                                unit,
                                (|| {
                                    ensure_not_cancelled(unit, cancelled, wire_alloc)?;
                                    mirror(config, unit, &packs)?;
                                    ensure_not_cancelled(unit, cancelled, wire_alloc)?;
                                    deliver(client, binding, &packs, wire_alloc)?;
                                    Ok(Some(packs))
                                })(),
                                wire_alloc,
                            )
                        }
                        Answer::Error(error) if Refusal::of(&error) == Some(Refusal::Ended) => {
                            Err(ended(client, binding, wire_alloc))
                        }
                        Answer::Error(error)
                            if matches!(
                                Refusal::of(&error),
                                Some(Refusal::AttemptMoved | Refusal::NotHolder)
                            ) =>
                        {
                            Ok(None)
                        }
                        other => Err(Error::Committed(format_in!(
                            wire_alloc,
                            "unit {}: publication commit outcome unknown: {other:?}",
                            hex(&unit.spec)
                        ))),
                    }
                },
            );
            // A failed heartbeat after successful publication is not evidence
            // the manifest was never accepted by the coordinator.
            let committed = if publication_accepted.load(Ordering::Acquire) {
                after_publication(unit, committed, wire_alloc)?
            } else {
                committed?
            };
            let Some(packs) = committed else {
                // This attempt was fenced during its tail. Uploaded immutable blobs are
                // harmless; only a current lead may make their manifest visible.
                continue;
            };
            arrived.values = import(&config.blobs, &packs, wire_alloc)?;
            ensure_not_cancelled(unit, cancelled, wire_alloc)?;
            arrived.packs = packs;
            return Ok(arrived);
        }
        waited = config.poll;
        let stopped = hold(
            config,
            unit,
            binding,
            executor.as_str(),
            claimed.lead,
            budget,
            clock,
            execution,
            claimed.attempt,
            now_ms,
            granted,
            cancelled,
            &mut arrived,
        )?;
        match stopped {
            StopWhy::Ended | StopWhy::Cancelled | StopWhy::AttemptMoved => {}
            StopWhy::FrontEndDiffers => {
                return Err(Error::Protocol(format_in!(
                    wire_alloc,
                    "unit {}: another executor's front end differs from this one's",
                    hex(&unit.spec)
                )));
            }
            StopWhy::Protocol(why) => return Err(Error::Protocol(why)),
        }
    }
}

fn mirror(config: &Config, unit: &Unit, packs: &[PackRef]) -> Result<(), Error> {
    let alloc = config.blobs.alloc;
    let expected = list_of(packs, alloc);
    config.blobs.publish(&unit.spec, &expected, alloc)?;
    // A 412 is accepted only after the existing and proposed full outputs were
    // verified equal, even if their native pack partitions differ. Missing or
    // corrupt committed packs refuse delivery; they never start another compile.
    Ok(())
}

fn publication(
    client: &mut Client,
    content: blaze_wire::JobContent,
    execution: ExecutionId,
    alloc: WireAlloc,
) -> Result<carrier::Publication, Error> {
    match answer(
        client,
        Command::Publication(carrier::Packs { content, execution }),
        "publication",
        alloc,
    )? {
        Answer::Publication(value) => {
            if value.packs.is_some() != value.accepted_attempt.is_some() {
                return Err(Error::Protocol(WireString::from_str_in(
                    "publication lacks its acceptance provenance",
                    alloc,
                )));
            }
            Ok(value)
        }
        other => Err(Error::Protocol(format_in!(alloc, "publication answered {other:?}"))),
    }
}

/// A successful tail is not publishable when its independent heartbeat worker failed.
/// Preserve a compiler or committed-data refusal over a concurrent heartbeat error.
fn completion_with_heartbeat<T>(
    execution: ExecutionId,
    completed: Result<T, Error>,
    heartbeats: Result<Result<(), Error>, ThreadFailed>,
    alloc: WireAlloc,
) -> Result<T, Error> {
    let value = completed?;
    heartbeats.map_err(|_| {
        Error::Protocol(format_in!(
            alloc,
            "distributed execution {}: lead heartbeat thread panicked",
            execution.0
        ))
    })??;
    Ok(value)
}

/// Compilation stays on the compiler's thread while a scoped driver keeps the lead alive
/// through the unit's tail. Fencing is checked atomically by CommitPublication afterward.
#[allow(clippy::too_many_arguments)]
fn heartbeat_during<T>(
    config: &Config,
    unit: &Unit,
    content: blaze_wire::JobContent,
    execution: ExecutionId,
    executor: &str,
    attempt: Attempt,
    budget: UnitBudget,
    clock: SystemClock,
    now_ms: &(impl Fn() -> u64 + Sync),
    cancelled: &AtomicBool,
    complete: impl FnOnce(&AtomicBool) -> Result<T, Error>,
) -> Result<T, Error> {
    struct End<'a>(&'a AtomicBool);
    impl Drop for End<'_> {
        fn drop(&mut self) {
            self.0.store(true, Ordering::Release);
        }
    }
    let alloc = config.blobs.alloc;
    let finished = AtomicBool::new(false);
    let lost = AtomicBool::new(false);
    scope(alloc, |s| {
        let (finished, lost) = (&finished, &lost);
        let heartbeat = s.spawn(move || {
            let mut client = match budget.connect_worker(config, unit) {
                Ok(client) => client,
                Err(error) => {
                    lost.store(true, Ordering::Release);
                    return Err(error);
                }
            };
            let bound = if budget.epoch.is_some() {
                budget.deadline_ms
            } else {
                now_ms().saturating_add(duration_ms(config.deadline))
            };
            let mut next = now_ms();
            while !finished.load(Ordering::Acquire)
                && !cancelled.load(Ordering::Acquire)
                && now_ms() <= bound
            {
                if let Err(error) = budget.check_active(unit, clock, alloc) {
                    lost.store(true, Ordering::Release);
                    let _ = client.release();
                    return Err(error);
                }
                if now_ms() >= next {
                    let command = Command::WorkerEvents(carrier::WorkerEvents {
                        content,
                        executor: WireString::from_str_in(executor, alloc),
                        request: WorkersEventsRequest {
                            execution,
                            attempt,
                            events: Vec::new_in(alloc),
                        },
                    });
                    match client.call(command) {
                        Ok(Answer::WorkerEvents(answer)) if answer.verdict == 0 => {}
                        Err(error) => {
                            lost.store(true, Ordering::Release);
                            let _ = client.release();
                            return Err(error);
                        }
                        _ => {
                            lost.store(true, Ordering::Release);
                            break;
                        }
                    }
                    next = now_ms().saturating_add(config.holder.heartbeat_ms);
                }
                sleep(budget.poll_wait(clock, config.poll.min(Duration::from_millis(50))));
            }
            let _ = client.release();
            Ok::<(), Error>(())
        })?;
        let end = End(finished);
        let result = complete(lost);
        drop(end);
        let observed = heartbeat.join();
        completion_with_heartbeat(execution, result, observed, alloc)
    })
}

/// This build no longer waits for its execution: when nobody else does, it ends, and its
/// executors go back. A failure here costs what the coordinator's own deadline takes back.
fn leave(client: &mut Client, binding: &Binding) {
    let current = match client.call(Command::Resume(binding.subscription.clone())) {
        Ok(Answer::Binding(current)) => current,
        _ => binding.clone(),
    };
    let _ = client.call(Command::Cancel(current));
}

/// Why the execution ended with nothing committed: its terminal for this subscription.
fn ended(client: &mut Client, binding: &Binding, alloc: WireAlloc) -> Error {
    match client.call(Command::Terminal(binding.subscription.clone())) {
        Ok(Answer::Terminal(carrier::Terminal { value: Some(value), .. })) => {
            Error::Ended(value.termination)
        }
        Ok(other) => Error::Protocol(format_in!(
            alloc,
            "the execution ended and its terminal says {other:?}"
        )),
        Err(error) => error,
    }
}

/// The packs of a finished compile phase, or `None` while it runs. An execution that ended
/// and was collected answers `NoExecution`, which here means someone else's copy of the unit
/// is gone: the caller's final build compiles what is left.
fn committed(
    client: &mut Client,
    content: blaze_wire::JobContent,
    execution: ExecutionId,
    alloc: WireAlloc,
) -> Result<carrier::CommittedPacks, Error> {
    match answer(client, Command::Packs(carrier::Packs { content, execution }), "packs", alloc)? {
        Answer::Packs(packs) => Ok(packs),
        other => Err(Error::Protocol(format_in!(alloc, "packs answered {other:?}"))),
    }
}

#[allow(clippy::too_many_arguments)]
fn hold(
    config: &Config,
    unit: &Unit,
    binding: &Binding,
    executor: &str,
    lead: bool,
    budget: UnitBudget,
    clock: SystemClock,
    execution: ExecutionId,
    attempt: Attempt,
    now_ms: &(impl Fn() -> u64 + Sync),
    granted: &mut (impl Granted + ?Sized),
    cancelled: &AtomicBool,
    arrived: &mut Arrived,
) -> Result<StopWhy, Error> {
    let alloc = config.blobs.alloc;
    budget.check_active(unit, clock, alloc)?;
    let content = binding.subscription.content;
    let mut holder = Holder::new(
        executor,
        execution,
        attempt,
        root_set(unit, alloc),
        Set::new(Vec::new_in(alloc)),
        BTreeSet::new_in(alloc),
        config.holder.clone(),
        alloc,
    );
    let mut client = budget.connect_worker(config, unit)?;
    let mut watcher = budget.connect_worker(config, unit)?;
    budget.check_active(unit, clock, alloc)?;
    let gate_timeout = budget.remaining(clock);
    let leave = AtomicBool::new(false);
    let working = AtomicBool::new(true);
    let driver = DriverConfig {
        // The stale-attempt probe gates the lead because its intentional silence moves
        // the attempt. The shared-execution probe gates every participating holder.
        result_gate: granted
            .test_result_gate()
            .filter(|gate| lead || gate.scope == ResultGateScope::All)
            .map(|gate| crate::result_gate::ResultGate::new(WireString::from_str_in(gate.directory, alloc), gate_timeout, alloc))
            .transpose()?,
        ..DriverConfig::default()
    };
    let mut batches = Batches {
        unit,
        blobs: &config.blobs,
        granted,
        compiled: 0,
        stored: 0,
        grants: 0,
        compiled_keys: Vec::new_in(alloc),
    };
    let stopped = scope(alloc, |s| {
        let (leave, working) = (&leave, &working);
        let watching = s.spawn(move || {
            let mut waited = config.poll;
            while working.load(Ordering::Acquire) {
                if cancelled.load(Ordering::Acquire)
                    || budget.epoch.is_some_and(|_| budget.remaining(clock).is_zero())
                {
                    leave.store(true, Ordering::Release);
                    break;
                }
                match committed(&mut watcher, content, execution, alloc) {
                    Ok(carrier::CommittedPacks { packs: Some(_) }) => {
                        leave.store(true, Ordering::Release);
                        break;
                    }
                    Ok(carrier::CommittedPacks { packs: None }) => {}
                    // The holder's own calls say what this is; its heartbeat ends it.
                    Err(_) => {}
                }
                sleep(budget.poll_wait(clock, waited));
                waited = waited.saturating_mul(2).min(POLL_MOST);
            }
            let _ = watcher.release();
        })?;
        let stopped = run_holder_until(
            &mut client,
            config.socket.as_str(),
            &mut holder,
            content,
            executor,
            &driver,
            now_ms,
            &mut batches,
            leave,
        );
        working.store(false, Ordering::Release);
        let _ = watching.join();
        stopped
    })?;
    arrived.compiled_here = arrived.compiled_here.saturating_add(batches.compiled);
    arrived.stored_here = arrived.stored_here.saturating_add(batches.stored);
    arrived.grants = arrived.grants.saturating_add(batches.grants);
    arrived.compiled_keys.extend(batches.compiled_keys);
    let _ = client.release();
    Ok(stopped)
}

/// Report a compiler verdict before publication. Accepted full output is completed by delivery.
fn verdict(
    client: &mut Client,
    content: blaze_wire::JobContent,
    execution: ExecutionId,
    executor: &str,
    attempt: Attempt,
    outcome: Outcome,
    alloc: WireAlloc,
) -> Result<(), Error> {
    let mut event_list = Vec::with_capacity_in(1, alloc);
    event_list.push(JobEvent::Done(DoneEvent { outcome, counters: Vec::new_in(alloc) }));
    let events = Command::WorkerEvents(carrier::WorkerEvents {
        content,
        executor: WireString::from_str_in(executor, alloc),
        request: WorkersEventsRequest { execution, attempt, events: event_list },
    });
    match answer(client, events, "reporting the unit's compiler verdict", alloc)? {
        Answer::WorkerEvents(_) => Ok(()),
        other => Err(Error::Protocol(format_in!(alloc, "the final verdict answered {other:?}"))),
    }
}

fn deliver(
    client: &mut Client,
    binding: &Binding,
    packs: &[PackRef],
    alloc: WireAlloc,
) -> Result<(), Error> {
    let command = Command::DeliverPublication(carrier::DeliverPublication {
        content: binding.subscription.content,
        execution: binding.subscription.execution,
        packs: Vec::from_slice_in(packs, alloc),
    });
    match client.call(command)? {
        Answer::Error(error) if Refusal::of(&error) == Some(Refusal::Ended) => {
            Err(ended(client, binding, alloc))
        }
        Answer::Ack(_) => match client.call(Command::Terminal(binding.subscription.clone()))? {
            Answer::Terminal(carrier::Terminal { value: Some(value), .. })
                if value.termination.is_valid() && value.termination.outcome == Outcome::Passed =>
            {
                Ok(())
            }
            Answer::Terminal(carrier::Terminal { value: Some(value), .. }) => {
                Err(Error::Ended(value.termination))
            }
            other => Err(Error::Protocol(format_in!(
                alloc,
                "publication delivery has no terminal verdict: {other:?}"
            ))),
        },
        other => Err(Error::Protocol(format_in!(alloc, "publication delivery answered {other:?}"))),
    }
}

fn retire_owned(
    client: &mut Client,
    content: blaze_wire::JobContent,
    execution: ExecutionId,
    executor: &str,
    claimant: Digest,
    alloc: WireAlloc,
) -> Result<(), Error> {
    for _ in 0..MAX_ATTEMPTS {
        let lookup = Command::OwnedAssignment(carrier::OwnedAssignment {
            content,
            executor: WireString::from_str_in(executor, alloc),
            execution,
            claimant,
        });
        let attempt = match client.call(lookup)? {
            Answer::OwnedAssignment(owned) => owned.attempt,
            // Already acknowledged or reused: this owner must not follow the slot.
            Answer::Error(error)
                if matches!(Refusal::of(&error), Some(Refusal::NotHolder | Refusal::NoExecution)) =>
            {
                return Ok(());
            }
            other => {
                return Err(Error::Protocol(format_in!(
                    alloc,
                    "owned assignment answered {other:?}"
                )));
            }
        };
        let request = Command::RetireAssignment(carrier::RetireAssignment {
            content,
            executor: WireString::from_str_in(executor, alloc),
            execution,
            attempt,
            claimant,
        });
        match client.call(request)? {
            Answer::Ack(_) => {
                return acknowledge(client, content, execution, executor, attempt, alloc);
            }
            Answer::Error(error) if Refusal::of(&error) == Some(Refusal::AttemptMoved) => {
                // A promotion raced the read. Query the same owned slot again; an old
                // token never becomes authority over the replacement by itself.
            }
            Answer::Error(error)
                if matches!(
                    Refusal::of(&error),
                    Some(Refusal::NotHolder | Refusal::NoExecution | Refusal::Ended)
                ) =>
            {
                return Ok(());
            }
            other => {
                return Err(Error::Protocol(format_in!(
                    alloc,
                    "assignment retirement answered {other:?}"
                )));
            }
        }
    }
    Err(Error::Protocol(WireString::from_str_in(
        "owned assignment exceeded its reassignment bound during cleanup",
        alloc,
    )))
}

fn acknowledge(
    client: &mut Client,
    content: blaze_wire::JobContent,
    execution: ExecutionId,
    executor: &str,
    attempt: Attempt,
    alloc: WireAlloc,
) -> Result<(), Error> {
    let ack = Command::KillAck(carrier::KillAck {
        content,
        executor: WireString::from_str_in(executor, alloc),
        execution,
        attempt,
        alive: true,
    });
    match client.call(ack)? {
        // Only a pending stop of this exact assignment is accepted. A false ACK
        // may mean another subscriber still owns the execution, or this token
        // was retired or replaced; it must not free a live/newer assignment.
        Answer::KillAck(_) => Ok(()),
        Answer::Error(error) if Refusal::of(&error) == Some(Refusal::NoExecution) => Ok(()),
        other => Err(Error::Protocol(format_in!(alloc, "kill acknowledgment answered {other:?}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pack::test_alloc::{Counted, Counts};
    use crate::sys::{Errno, IoError};
    use blaze_rng::SplitMix64;

    fn test_alloc() -> WireAlloc {
        blaze_own::testing::heap().shared()
    }

    #[test]
    fn a_pull_request_export_reuses_the_first_deadline_in_its_epoch() {
        let alloc = test_alloc();
        let unit = Unit { spec: [3; 32], names: Vec::new_in(alloc) };
        let mut config = Config {
            socket: WireString::from_str_in("unused", alloc),
            blobs: Blobs {
                address: "127.0.0.1:1".parse().unwrap(),
                timeout: Duration::from_secs(1),
                alloc,
            },
            claimant: [4; 32],
            platform: [5; 32],
            local: LocalPolicy::Fallback,
            namespace: Namespace::Pr(blaze_wire::PrNamespace { n: 81 }),
            source: [6; 32],
            deadline: Duration::from_secs(1),
            holder: HolderConfig { heartbeat_ms: 100, retry_ms: 100, idle_ms: 10 },
            poll: Duration::from_millis(5),
        };
        let welcome = carrier::LocalWelcome {
            transport: carrier::TRANSPORT_VERSION,
            codec: 1,
            terminal: 1,
            session: [7; 32],
            epoch: [8; 32],
            now_ms: 100,
            expires_ms: 2_000,
            max_frame: 4_096,
            next_request: 1,
            created: true,
        };
        let next = carrier::LocalWelcome { now_ms: 800, session: [9; 32], ..welcome.clone() };
        let restarted = carrier::LocalWelcome { epoch: [10; 32], ..next.clone() };
        let expired = carrier::LocalWelcome { now_ms: 1_100, ..next.clone() };
        {
            let mut export = ExportBudget::new(&config);
            let first = export.admit(&unit, &welcome, 1_000_000, 100).unwrap();
            let second = export.admit(&unit, &next, 501_000_000, 800).unwrap();
            assert_eq!(request(&unit, &config, first).deadline_ms, 1_100);
            assert_eq!(request(&unit, &config, second).deadline_ms, 1_100);
            assert_eq!(second.limit, Duration::from_millis(300));
            assert!(matches!(
                export.admit(&unit, &restarted, 502_000_000, 800),
                Err(Error::Cancelled(reason)) if reason.as_str().contains("coordinator epoch")
            ));
            assert!(matches!(
                export.admit(&unit, &expired, 503_000_000, 1_100),
                Err(Error::Cancelled(reason)) if reason.as_str().contains("deadline expired")
            ));
            assert!(matches!(
                export.admit(&unit, &next, 1_001_000_000, 800),
                Err(Error::Cancelled(reason)) if reason.as_str().contains("deadline expired")
            ));
        }

        config.namespace = Namespace::Main;
        let mut export = ExportBudget::new(&config);
        let first = export.admit(&unit, &welcome, 1_000_000, 100).unwrap();
        let second = export.admit(&unit, &restarted, 501_000_000, 800).unwrap();
        assert_eq!(request(&unit, &config, first).deadline_ms, 1_100);
        assert_eq!(request(&unit, &config, second).deadline_ms, 1_800);
        assert_eq!(second.limit, config.deadline);
    }

    #[test]
    fn a_superseded_request_cannot_fall_back_to_local_compilation() {
        let alloc = test_alloc();
        let unit = Unit { spec: [3; 32], names: Vec::new_in(alloc) };
        for refusal in [Refusal::Superseded, Refusal::PrDeadlineExpired] {
            let wire = refusal.to_wire(None, alloc);
            let error = submission_error(&unit, &wire, alloc);
            assert!(matches!(
                decide(&LocalPolicy::Fallback, error, alloc),
                Err(Error::Cancelled(reason)) if reason.as_str().contains(refusal.token())
            ));
        }
    }

    #[test]
    fn imported_pack_storage_uses_the_callers_allocator() {
        let base_alloc = test_alloc();
        let value = |key, bytes: &[u8]| Value {
            key: [key; 32],
            kind: key,
            bytes: Vec::from_slice_in(bytes, base_alloc),
        };
        let mut p0_vals = Vec::new_in(base_alloc);
        p0_vals.push(value(1, b"one"));
        let mut p1_vals = Vec::new_in(base_alloc);
        p1_vals.push(value(1, b"one"));
        p1_vals.push(value(2, b"two"));
        let packs = [
            Pack::of(p0_vals, base_alloc).unwrap(),
            Pack::of(p1_vals, base_alloc).unwrap(),
        ];
        let references = [packs[0].reference.clone(), packs[1].reference.clone()];
        let counts = Counts::default();
        let alloc = Counted(&counts);
        let values = import_using(
            &references,
            |reference| {
                Ok(packs
                    .iter()
                    .find(|pack| pack.reference.pack == reference.pack)
                    .map(|pack| Vec::from_slice_in(pack.bytes.as_slice(), &alloc)))
            },
            &alloc,
            base_alloc,
        )
        .unwrap();
        assert_eq!(values.len(), 2, "an identical value in two packs is retained only once");
        assert!(
            counts.allocations.get() - counts.deallocations.get() > values.len(),
            "the map's nodes and each retained payload use the supplied allocator"
        );
        for (key, (kind, bytes)) in &values {
            assert_eq!(*kind, key[0]);
            assert!(core::ptr::eq(bytes.allocator().0, &counts));
        }
        assert_eq!(values[&[1; 32]].1.as_slice(), b"one");
        assert_eq!(values[&[2; 32]].1.as_slice(), b"two");
        let before = counts.allocations.get();
        let control = import_using(
            &references,
            |reference| {
                Ok(packs
                    .iter()
                    .find(|pack| pack.reference.pack == reference.pack)
                    .map(|pack| Vec::from_slice_in(pack.bytes.as_slice(), base_alloc)))
            },
            base_alloc,
            base_alloc,
        )
        .unwrap();
        assert_eq!(control.len(), values.len());
        for (key, (kind, bytes)) in &control {
            assert_eq!(*kind, values[key].0);
            assert_eq!(bytes.as_slice(), values[key].1.as_slice());
        }
        assert_eq!(counts.allocations.get(), before);
        drop(values);
        assert_eq!(counts.allocations.get(), counts.deallocations.get());

        let mut broken = references;
        broken[1].keys = [9; 32];
        let refused = import_using(
            &broken,
            |reference| {
                Ok(packs
                    .iter()
                    .find(|pack| pack.reference.pack == reference.pack)
                    .map(|pack| Vec::from_slice_in(pack.bytes.as_slice(), &alloc)))
            },
            &alloc,
            base_alloc,
        )
        .unwrap_err();
        assert!(matches!(refused, Error::Committed(_)));
        assert_eq!(counts.allocations.get(), counts.deallocations.get());

        for (changed, duplicate) in [
            ("bytes", value(1, b"other")),
            ("kind", Value { kind: 9, ..value(1, b"one") }),
        ] {
            // Each pack is valid by itself. The refusal must come from combining
            // different values under one key, after the first pack allocated its map.
            let mut dup_vals = Vec::new_in(base_alloc);
            dup_vals.push(duplicate);
            let conflicting = Pack::of(dup_vals, base_alloc).unwrap();
            let references = [packs[0].reference.clone(), conflicting.reference.clone()];
            let before = counts.allocations.get();
            let refused = import_using(
                &references,
                |reference| {
                    let source = if reference.pack == packs[0].reference.pack {
                        &packs[0].bytes
                    } else {
                        assert_eq!(reference.pack, conflicting.reference.pack);
                        &conflicting.bytes
                    };
                    Ok(Some(Vec::from_slice_in(source.as_slice(), &alloc)))
                },
                &alloc,
                base_alloc,
            )
            .unwrap_err();
            let key_hex = format_in!(base_alloc, "{}", hex(&[1; 32]));
            let pack_hex = format_in!(base_alloc, "{}", hex(&conflicting.reference.pack));
            assert!(
                matches!(&refused, Error::Committed(why)
                    if why.contains("two values under the key")
                        && why.contains(key_hex.as_str())
                        && why.contains(pack_hex.as_str())),
                "conflicting {changed}: {refused}"
            );
            assert!(counts.allocations.get() > before, "the import must allocate caller storage");
            assert_eq!(
                counts.allocations.get(),
                counts.deallocations.get(),
                "a conflicting {changed} must release every imported payload and map node"
            );
        }
    }

    /// The invariant: a final compiler refusal retains its diagnostic and remains terminal
    /// under every local policy despite a concurrent heartbeat failure; a successful tail
    /// cannot hide a failed or panicking heartbeat worker.
    #[test]
    fn a_failed_heartbeat_cannot_publish_a_successful_tail() {
        let alloc = test_alloc();
        let mut rng = SplitMix64::new(0x4845_4152_5442_4541);
        for _ in 0..64 {
            let completion = rng.next_u64() % 3;
            let heartbeat_kind = rng.next_u64() % 3;
            let completed = match completion {
                0 => Ok(7),
                1 => Err(Error::Committed(WireString::from_str_in(
                    "the native pack was corrupt",
                    alloc,
                ))),
                _ => Err(Error::Refused(WireString::from_str_in(
                    "the compiler's tail failed",
                    alloc,
                ))),
            };
            let heartbeat: Result<Result<(), Error>, ThreadFailed> = match heartbeat_kind {
                0 => Ok(Ok(())),
                1 => Ok(Err(Error::Io(IoError::with_message(
                    Errno::ConnRefused,
                    WireString::from_str_in("the heartbeat carrier disconnected", alloc),
                )))),
                _ => Err(ThreadFailed),
            };
            let observed = completion_with_heartbeat(ExecutionId(9), completed, heartbeat, alloc);
            match (completion, heartbeat_kind, observed) {
                (0, 0, Ok(7)) => {}
                (0, 1, Err(Error::Io(_))) => {}
                (0, 2, Err(Error::Protocol(why))) if why.contains("heartbeat thread panicked") => {}
                (1, _, Err(Error::Committed(_))) => {}
                (2, _, Err(Error::Refused(why))) if why.contains("compiler's tail failed") => {
                    for policy in [LocalPolicy::Never, LocalPolicy::Fallback] {
                        assert!(matches!(
                            decide(&policy, Error::Refused(why.clone()), alloc),
                            Err(Error::Refused(retained)) if retained == why
                        ));
                    }
                }
                other => {
                    panic!("a result lost its cause or heartbeat failure became success: {other:?}")
                }
            }
        }
    }

    #[test]
    fn the_oracle_catches_a_dropped_heartbeat_failure() {
        let alloc = test_alloc();
        let failed = Ok(Err(Error::Io(IoError::from_errno(Errno::ConnRefused))));
        let rejects_failed_heartbeat = |result| matches!(result, Err(Error::Io(_)));
        assert!(rejects_failed_heartbeat(completion_with_heartbeat(
            ExecutionId(2),
            Ok(1),
            failed,
            alloc,
        )));
        // Plant the original unchecked join, which returned the successful tail alone.
        assert!(!rejects_failed_heartbeat(Ok(1)));
    }

    #[test]
    fn the_oracle_catches_a_compiler_refusal_reclassified_as_transport_loss() {
        let alloc = test_alloc();
        let why = "fixture::tail: conflicting output under one immutable key";
        let refuses_rebuild = |failure| decide(&LocalPolicy::Fallback, failure, alloc).is_err();
        assert!(refuses_rebuild(Error::Refused(WireString::from_str_in(why, alloc))));
        // Plant the previous classification of the exact same compiler diagnostic.
        assert!(!refuses_rebuild(Error::Protocol(WireString::from_str_in(why, alloc))));
    }

    #[test]
    fn errors_after_publication_cannot_rebuild_an_immutable_unit() {
        let alloc = test_alloc();
        let unit = Unit { spec: [0x71; 32], names: Vec::new_in(alloc) };
        let spec_hex = format_in!(alloc, "{}", hex(&unit.spec));
        for error in [
            Error::Io(IoError::from_errno(Errno::ConnReset)),
            Error::Protocol(WireString::from_str_in("delivery was refused", alloc)),
        ] {
            // A prior Protocol/Io error would have triggered Fallback despite
            // the coordinator having accepted this unit's immutable manifest.
            let refused = after_publication(&unit, Err::<(), _>(error), alloc).unwrap_err();
            assert!(matches!(
                &refused,
                Error::Committed(why) if why.contains(spec_hex.as_str())
                    && why.contains("already accepted")
            ));
            assert!(matches!(
                decide(&LocalPolicy::Fallback, refused, alloc),
                Err(Error::Committed(_))
            ));
        }
        let already_committed = after_publication(
            &unit,
            Err::<(), _>(Error::Committed(WireString::from_str_in("bad pack", alloc))),
            alloc,
        )
        .unwrap_err();
        assert!(matches!(
            already_committed,
            Error::Committed(why) if why.as_str() == "bad pack"
        ));
        let cancelled = after_publication(
            &unit,
            Err::<(), _>(Error::Cancelled(WireString::from_str_in("caller stopped", alloc))),
            alloc,
        )
        .unwrap_err();
        assert!(matches!(cancelled, Error::Cancelled(_)));
        assert_eq!(after_publication(&unit, Ok(42), alloc).unwrap(), 42);
        // Control: the old classification would have permitted this rebuild.
        assert!(
            decide(
                &LocalPolicy::Fallback,
                Error::Protocol(WireString::from_str_in("delivery was refused", alloc)),
                alloc,
            )
            .is_ok()
        );
    }

    #[test]
    fn caller_cancellation_cannot_trigger_local_recompilation() {
        let alloc = test_alloc();
        let unit = Unit { spec: [0x42; 32], names: Vec::new_in(alloc) };
        let spec_hex = format_in!(alloc, "{}", hex(&unit.spec));
        let cancelled = AtomicBool::new(false);
        assert!(ensure_not_cancelled(&unit, &cancelled, alloc).is_ok());
        cancelled.store(true, Ordering::Release);
        let refused = ensure_not_cancelled(&unit, &cancelled, alloc).unwrap_err();
        assert!(matches!(&refused, Error::Cancelled(why) if why.contains(spec_hex.as_str())));
        assert!(matches!(
            decide(&LocalPolicy::Fallback, refused, alloc),
            Err(Error::Cancelled(_))
        ));
    }

    #[test]
    fn published_pack_list_requires_exact_sorted_unique_records() {
        let alloc = test_alloc();
        let unit = Unit { spec: [0x33; 32], names: Vec::new_in(alloc) };
        let first = PackRef {
            pack: [0x11; 32],
            size: 10,
            keys: [0x44; 32],
            packed: None,
            format: blaze_wire::DIST_PACK_FORMAT,
        };
        let second = PackRef { pack: [0x22; 32], ..first.clone() };
        let canonical = list_of(&[first.clone(), second.clone()], alloc);
        assert!(published_packs(&unit.spec, &canonical, alloc, alloc).is_ok());
        assert!(published_packs(&unit.spec, b"", alloc, alloc).is_ok(), "an empty unit is canonical");

        let mut with_extra = Vec::from_slice_in(canonical.as_slice(), alloc);
        with_extra.extend_from_slice(b"extra\n");
        let mut with_nl = Vec::from_slice_in(canonical.as_slice(), alloc);
        with_nl.extend_from_slice(b"\n");
        let noncanonical = [
            Vec::from_slice_in(&canonical[..canonical.len() - 1], alloc),
            with_extra,
            with_nl,
            list_of(&[second, first.clone()], alloc),
            list_of(&[first.clone(), first], alloc),
            Vec::from_slice_in(b"not-a-pack\n", alloc),
            Vec::from_slice_in(&[0xff], alloc),
        ];
        for bytes in noncanonical {
            assert!(
                matches!(published_packs(&unit.spec, &bytes, alloc, alloc), Err(Error::Committed(_))),
                "a damaged published manifest must be refused, not rebuilt"
            );
        }
    }

    #[test]
    fn a_committed_pack_missing_or_corrupt_must_not_rebuild_even_with_fallback() {
        let alloc = test_alloc();
        let reference = PackRef {
            pack: [0x22; 32],
            size: 10,
            keys: [0x44; 32],
            packed: None,
            format: blaze_wire::DIST_PACK_FORMAT,
        };
        let pack_hex = format_in!(alloc, "{}", hex(&reference.pack));
        let missing = import_using::<WireAlloc, WireAlloc>(
            &[reference.clone()],
            |requested| {
                assert_eq!(requested, &reference);
                Ok(None)
            },
            alloc,
            alloc,
        )
        .unwrap_err();
        assert!(matches!(&missing, Error::Committed(why) if why.contains(pack_hex.as_str())));
        assert!(matches!(decide(&LocalPolicy::Fallback, missing, alloc), Err(Error::Committed(_))));

        for error in [
            Error::Io(IoError::from_errno(Errno::TimedOut)),
            Error::Protocol(WireString::from_str_in("GET returned 500", alloc)),
        ] {
            let mut error = Some(error);
            let failed = import_using::<WireAlloc, WireAlloc>(
                &[reference.clone()],
                |_| Err(error.take().unwrap()),
                alloc,
                alloc,
            )
            .unwrap_err();
            assert!(matches!(
                decide(&LocalPolicy::Fallback, failed, alloc),
                Err(Error::Committed(_))
            ));
        }

        let corrupt = import_using::<WireAlloc, WireAlloc>(
            &[reference],
            |_| Ok(Some(Vec::from_slice_in(b"bad pack", alloc))),
            alloc,
            alloc,
        )
        .unwrap_err();
        assert!(matches!(decide(&LocalPolicy::Fallback, corrupt, alloc), Err(Error::Committed(_))));

        let unit = Unit { spec: [0x33; 32], names: Vec::new_in(alloc) };
        let malformed = published_packs(&unit.spec, b"not a pack", alloc, alloc).unwrap_err();
        assert!(matches!(
            decide(&LocalPolicy::Fallback, malformed, alloc),
            Err(Error::Committed(_))
        ));
    }
}
