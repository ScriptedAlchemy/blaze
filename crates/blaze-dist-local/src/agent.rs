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

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use blaze_dist::holder::{BatchCompiler, Compiled, Holder, HolderConfig, Plan, StopWhy};
use blaze_dist::jobs::{LocalAction, local_action};
use blaze_dist::{Ranges, Refusal};
use blaze_wire::{
    Attempt, Digest, DoneEvent, EntryKey, EntryRef, ExecutionId, ItemKey, ItemOutcome, ItemResult,
    JobEvent, JobRequest, LocalPolicy, Namespace, Outcome, PackRef, Reservation, RootSet, Set, Why,
    WorkKey, WorkName, WorkersEventsRequest, digest_of_encoded_name, encode,
};

use crate::Error;
use crate::blobs::Blobs;
use crate::client::Client;
use crate::driver::{DriverConfig, run_holder_until};
use crate::pack::{Pack, Value, hex, read};
use crate::protocol::{self as carrier, Answer, Binding, Command};
use crate::server::random_id;
use blaze_wire::pack_list::{decode as parse_list, encode as list_of};

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
    pub socket: PathBuf,
    /// The cache server.
    pub blobs: Blobs,
    /// This daemon: who holds a claim, across its units.
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
    /// How long one unit may wait for its execution.
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
    pub names: Vec<Digest>,
}

/// What compiles a grant: on the thread that called [`run`], which may own the front end.
pub trait Granted {
    /// The values of `names`, each index into [`Unit::names`]: from the store or compiled.
    /// `Err` gives the batch back: another executor or the final build gets to it, and a real
    /// compile error is reported there, by the build that links the unit.
    fn compile(&mut self, names: &[usize]) -> Result<Grant, String>;

    /// Complete the unit through its ordinary build with the committed phase values in
    /// front of its store. Only the current lead runs this tail; other subscribers take
    /// its full manifest. Returned values are uploaded before the manifest is committed.
    fn complete(
        &mut self,
        _values: &BTreeMap<Digest, (u8, Vec<u8>)>,
    ) -> Result<Vec<Value>, String> {
        Ok(Vec::new())
    }
}

/// One grant's values.
pub struct Grant {
    /// Every value the grant made or found.
    pub values: Vec<Value>,
    /// How many functions this executor compiled for it.
    pub compiled: u64,
    /// How many it found in its own store.
    pub stored: u64,
    /// Actual emitted store keys, in emission order, including repeated compiles.
    pub compiled_keys: Vec<Digest>,
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
    pub values: BTreeMap<Digest, (u8, Vec<u8>)>,
    /// The packs they came from: what this unit's published list names.
    pub packs: Vec<PackRef>,
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
    pub compiled_keys: Vec<Digest>,
}

impl Arrived {
    /// Nothing yet, from `source`.
    fn none(source: Source, execution: Option<u64>) -> Arrived {
        Arrived {
            values: BTreeMap::new(),
            packs: Vec::new(),
            source,
            execution,
            compiled_here: 0,
            stored_here: 0,
            grants: 0,
            joined: false,
            attempt: None,
            publication_attempt: None,
            compiled_keys: Vec::new(),
        }
    }
}

/// The coordinator's work identity for an opaque compiler task in one unit.
pub fn wire_name(unit: &Digest, name: &Digest) -> WorkName {
    WorkName::of(&WorkKey::Item(ItemKey { unit: *unit, stable: *name, env: [0; 32] }))
}

/// The set every executor of `unit` joins with.
pub fn root_set(unit: &Unit) -> RootSet {
    let pairs: Vec<(WorkName, Digest)> =
        unit.names.iter().map(|name| (wire_name(&unit.spec, name), *name)).collect();
    RootSet {
        spec: unit.spec,
        roots: digest_of_encoded_name("blaze/root-set", &encode(&pairs)),
        count: pairs.len() as u64,
        index: digest_of_encoded_name(
            "blaze/pre-index",
            &encode(&Vec::<(WorkName, Digest)>::new()),
        ),
        inputs: [0; 32],
    }
}

/// The job of a unit: what its executors agree on, and this subscriber's terms.
fn request(unit: &Unit, config: &Config, now: u64) -> JobRequest {
    JobRequest {
        unit: unit.spec,
        platform: config.platform,
        namespace: config.namespace.clone(),
        source: config.source,
        deadline_ms: now.saturating_add(config.deadline.as_millis() as u64),
        local: config.local.clone(),
    }
}

/// What the requester does about `failure`, under `policy`: `Ok` compiles the unit here, by the
/// one build; `Err` stops the build, naming what failed. A terminal goes by its contract
/// ([`local_action`]): a verdict is taken, anything else builds here only if asked. A failure
/// that names no outcome (like a disconnected carrier) is no verdict. A missing or corrupt
/// committed pack always fails, regardless of the requested local policy.
pub fn decide(policy: &LocalPolicy, failure: Error) -> Result<(), Error> {
    let action = match &failure {
        Error::Ended(termination) => local_action(policy, termination)
            .map_err(|why| Error::Protocol(format!("the terminal is refused: {why:?}")))?,
        Error::Committed(_) | Error::Cancelled(_) => LocalAction::Fail,
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

struct Batches<'a> {
    unit: &'a Unit,
    blobs: &'a Blobs,
    granted: &'a mut dyn Granted,
    compiled: u64,
    stored: u64,
    grants: u64,
    compiled_keys: Vec<Digest>,
}

impl BatchCompiler for Batches<'_> {
    fn compile(&mut self, plan: &Plan) -> Result<Compiled, String> {
        let ranges =
            Ranges::from_wire(&plan.grant.roots).ok_or("granted root ranges are not canonical")?;
        let indexes: Vec<u64> = ranges.indexes().collect();
        let mut names = Vec::with_capacity(indexes.len());
        for index in &indexes {
            let at = usize::try_from(*index).ok().filter(|at| *at < self.unit.names.len());
            names.push(at.ok_or_else(|| format!("granted root {index} is outside the root set"))?);
        }
        if std::env::var_os("BLAZE_DIST_EVENTS").is_some() {
            let work: Vec<String> = names
                .iter()
                .map(|at| hex(&wire_name(&self.unit.spec, &self.unit.names[*at]).0))
                .chain(plan.grant.names.iter().map(|name| hex(&name.0)))
                .collect();
            crate::telemetry::emit(
                "grant",
                &[
                    ("pid", crate::telemetry::json(&std::process::id())),
                    ("unit_digest", crate::telemetry::json(&hex(&self.unit.spec))),
                    ("execution", crate::telemetry::json(&plan.grant.lease.execution.0)),
                    ("attempt", crate::telemetry::json(&plan.grant.lease.attempt.0)),
                    ("holder", crate::telemetry::json(&plan.grant.lease.holder)),
                    ("generation", crate::telemetry::json(&plan.grant.lease.generation)),
                    ("seq", crate::telemetry::json(&plan.seq)),
                    ("work", crate::telemetry::json(&work)),
                ],
            );
        }
        self.grants += 1;
        let (outcome, packs): (fn(&Digest) -> ItemOutcome, Vec<PackRef>) =
            match self.granted.compile(&names) {
                Ok(grant) => {
                    self.compiled += grant.compiled;
                    self.stored += grant.stored;
                    self.compiled_keys.extend(grant.compiled_keys);
                    let pack = Pack::of(grant.values)?;
                    // The result is posted after this returns: a committed pack exists whole.
                    self.blobs.put_pack(&pack).map_err(|e| e.to_string())?;
                    (
                        |name| ItemOutcome::Exported(EntryRef { entry: EntryKey(*name) }),
                        vec![pack.reference],
                    )
                }
                Err(why) => {
                    tracing::warn!(target: "blaze", why, "a distributed grant goes back");
                    (
                        |_| ItemOutcome::Local(Why { why: "the batch was given back".to_owned() }),
                        Vec::new(),
                    )
                }
            };
        let item = |at: &usize| {
            let name = &self.unit.names[*at];
            ItemResult {
                name: wire_name(&self.unit.spec, name),
                outcome: outcome(name),
                found: Set::new(Vec::new()),
                unnamed: Set::new(Vec::new()),
            }
        };
        let lost = |name: &WorkName| ItemResult {
            name: *name,
            outcome: ItemOutcome::Lost(Why {
                why: "unknown-name: a V2 root set finds no names".to_owned(),
            }),
            found: Set::new(Vec::new()),
            unnamed: Set::new(Vec::new()),
        };
        Ok(Compiled {
            generation: plan.grant.lease.generation,
            roots: names.iter().map(item).collect(),
            names: plan.compile.iter().map(lost).collect(),
            packs,
            returned_roots: Ranges::new(),
            returned_names: Vec::new(),
        })
    }

    fn release(&mut self, _names: &[WorkName]) {}
}

/// How long past a job's deadline its requester still waits for the coordinator to end it.
const GRACE: Duration = Duration::from_secs(5);

/// The reason reported when the caller cancels the distributed build.
const CANCELLED: &str = "the build was cancelled";

/// A cancelled request is terminal even if the carrier or local policy permits a retry.
fn ensure_not_cancelled(unit: &Unit, cancelled: &AtomicBool) -> Result<(), Error> {
    if cancelled.load(Ordering::Acquire) {
        let reason = format!("unit {}: {CANCELLED}", hex(&unit.spec)); // alloc: terminal reason
        Err(Error::Cancelled(reason))
    } else {
        Ok(())
    }
}

/// Reads the packs named, each checked against what was committed.
pub fn import(blobs: &Blobs, packs: &[PackRef]) -> Result<BTreeMap<Digest, (u8, Vec<u8>)>, Error> {
    import_using(packs, |digest| blobs.pack(digest))
}

/// The retrieval boundary is injected so loss and corruption of a committed pack can be
/// tested without replacing the real pack decoder or requiring a live cache server.
fn import_using(
    packs: &[PackRef],
    mut fetch: impl FnMut(&Digest) -> Result<Option<Vec<u8>>, Error>,
) -> Result<BTreeMap<Digest, (u8, Vec<u8>)>, Error> {
    let mut values = BTreeMap::new();
    for reference in packs {
        let name = hex(&reference.pack);
        let bytes = fetch(&reference.pack)
            .map_err(|error| {
                Error::Committed(format!("pack {name}: committed retrieval failed: {error}"))
            })?
            .ok_or_else(|| Error::Committed(format!("pack {name} was committed and is gone")))?;
        for value in read(&bytes, reference).map_err(Error::Committed)? {
            match values.entry(value.key) {
                std::collections::btree_map::Entry::Vacant(at) => {
                    at.insert((value.kind, value.bytes));
                }
                std::collections::btree_map::Entry::Occupied(at) => {
                    if at.get() != &(value.kind, value.bytes) {
                        return Err(Error::Committed(format!(
                            "two values under the key {} in the packs of one unit, the second in {name}",
                            hex(&value.key)
                        )));
                    }
                }
            }
        }
    }
    Ok(values)
}

/// A corrupted published manifest cannot authorize recompilation.
fn published_packs(unit: &Unit, list: &[u8]) -> Result<Vec<PackRef>, Error> {
    parse_list(list).map_err(|why| {
        Error::Committed(format!(
            "unit {}: published pack list is not canonical: {why}",
            hex(&unit.spec)
        ))
    })
}

/// How long an agent waits between asks at most, however long it has waited: what it asks is a
/// state of the coordinator's that a wait of this does not miss by much.
const POLL_MOST: Duration = Duration::from_millis(250);

fn answer(client: &mut Client, command: Command, what: &str) -> Result<Answer, Error> {
    match client.call(command)? {
        Answer::Error(error) => {
            Err(Error::Protocol(format!("{what}: {} ({:?})", error.message, error.code())))
        }
        other => Ok(other),
    }
}

/// Runs `unit` as far as the distributed path takes it. See the module's docs for the steps.
pub fn run(
    config: &Config,
    unit: &Unit,
    granted: &mut dyn Granted,
    cancelled: &AtomicBool,
) -> Result<Arrived, Error> {
    ensure_not_cancelled(unit, cancelled)?;
    if let Some(list) = config.blobs.published(&unit.spec)? {
        let packs = published_packs(unit, &list)?;
        let values = import(&config.blobs, &packs)?;
        ensure_not_cancelled(unit, cancelled)?;
        return Ok(Arrived { values, packs, ..Arrived::none(Source::Published, None) });
    }
    let started = Instant::now();
    let mut client = Client::connect(&config.socket, random_id()?)?;
    let origin = client.welcome.now_ms;
    let now_ms = move || origin.saturating_add(started.elapsed().as_millis() as u64);
    let job = request(unit, config, now_ms());
    let binding = loop {
        ensure_not_cancelled(unit, cancelled)?;
        let submit = Command::Submit(carrier::submission(&job, NEED));
        match client.call(submit)? {
            Answer::Binding(binding) => break binding,
            Answer::Error(error) if error.code() == Some(blaze_wire::ErrorCode::Busy) => {
                ensure_not_cancelled(unit, cancelled)?;
                if started.elapsed() > config.deadline {
                    return Err(Error::Protocol(format!("submit: {}", error.message)));
                }
                let _waiting =
                    tracing::info_span!(target: "blaze", "dist: the coordinator is busy").entered();
                std::thread::sleep(Duration::from_millis(error.retry_after_ms.unwrap_or(100)));
            }
            Answer::Error(error) => {
                return Err(Error::Protocol(format!("submit: {}", error.message)));
            }
            other => return Err(Error::Protocol(format!("submit answered {other:?}"))),
        }
    };
    let result = execute(config, unit, granted, cancelled, started, &now_ms, &binding, &mut client);
    let result = ensure_not_cancelled(unit, cancelled).and(result);
    if result.is_err() {
        leave(&mut client, &binding);
    } else {
        let _ = client.release();
    }
    result
}

#[allow(clippy::too_many_arguments)]
fn execute(
    config: &Config,
    unit: &Unit,
    granted: &mut dyn Granted,
    cancelled: &AtomicBool,
    started: Instant,
    now_ms: &(impl Fn() -> u64 + Sync),
    binding: &Binding,
    client: &mut Client,
) -> Result<Arrived, Error> {
    let content = binding.subscription.content;
    let execution = binding.subscription.execution;
    let mut arrived = Arrived::none(Source::Executed, Some(execution.0));
    let mut waited = config.poll;
    let mut last_claim = None;
    loop {
        ensure_not_cancelled(unit, cancelled)?;
        if started.elapsed() > config.deadline.saturating_add(GRACE) {
            return Err(Error::Protocol(format!(
                "unit {}: its execution did not finish in {:?}",
                hex(&unit.spec),
                config.deadline
            )));
        }
        // Claim before reading phase completion: even a subscriber arriving after that
        // phase must know whether it is the current lead or a reader of the lead's tail.
        let claim =
            Command::Claim(carrier::Claim { content, execution, claimant: config.claimant });
        let claimed = match client.call(claim)? {
            Answer::Claimed(claimed) => Some(claimed),
            Answer::Error(error) if Refusal::of(&error) == Some(Refusal::Ended) => None,
            other => return Err(Error::Protocol(format!("claim answered {other:?}"))),
        };
        if let Some(claimed) = &claimed
            && let Some(executor) = &claimed.executor
        {
            arrived.attempt = Some(claimed.attempt.0);
            last_claim = Some((executor.clone(), claimed.attempt));
        }
        let published = publication(client, content, execution)?;
        if let Some((packs, attempt, accepted)) = published {
            arrived.attempt = Some(attempt.0);
            arrived.publication_attempt = Some(accepted.0);
            let mut take = || {
                arrived.values = import(&config.blobs, &packs)?;
                arrived.packs = packs;
                ensure_not_cancelled(unit, cancelled)?;
                // Delivery carries no new output authority, even after lead replacement.
                mirror(config, unit, &arrived.packs)?;
                ensure_not_cancelled(unit, cancelled)?;
                deliver(client, binding, &arrived.packs)?;
                Ok(())
            };
            if let Some(claimed) = &claimed
                && claimed.lead
                && let Some(executor) = &claimed.executor
            {
                heartbeat_during(
                    config,
                    content,
                    execution,
                    executor,
                    claimed.attempt,
                    now_ms,
                    cancelled,
                    |_| take(),
                )?;
            } else {
                take()?;
            }
            if let Some((executor, attempt)) = last_claim {
                acknowledge(client, content, execution, &executor, attempt)?;
            }
            return Ok(arrived);
        }
        let Some(claimed) = claimed else { return Err(ended(client, binding)) };
        let Some(executor) = claimed.executor else {
            let _waiting =
                tracing::info_span!(target: "blaze", "dist: waiting for a unit's publication")
                    .entered();
            std::thread::sleep(waited);
            waited = (waited * 2).min(POLL_MOST);
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
                    roots: root_set(unit),
                    rejected: Set::new(Vec::new()),
                },
            });
            match answer(client, join, "the lead joining the unit")? {
                Answer::Join(_) => {}
                other => return Err(Error::Protocol(format!("join answered {other:?}"))),
            }
        }
        if let Some(packs) = committed(client, content, execution)? {
            if !claimed.lead {
                // The helper's work is committed, but the whole unit is not. It must not
                // independently compile the tail during the lead's publication window.
                let _waiting =
                    tracing::info_span!(target: "blaze", "dist: the lead completing the unit")
                        .entered();
                std::thread::sleep(waited);
                waited = (waited * 2).min(POLL_MOST);
                continue;
            }
            let committed = heartbeat_during(
                config,
                content,
                execution,
                &executor,
                claimed.attempt,
                now_ms,
                cancelled,
                |lost| {
                    arrived.values = import(&config.blobs, &packs)?;
                    ensure_not_cancelled(unit, cancelled)?;
                    let extra = match granted.complete(&arrived.values) {
                        Ok(values) => values,
                        Err(why) => {
                            if !lost.load(Ordering::Acquire) {
                                let _ = verdict(
                                    client,
                                    content,
                                    execution,
                                    &executor,
                                    claimed.attempt,
                                    Outcome::Failed,
                                );
                            }
                            return Err(Error::Protocol(format!("unit {}: {why}", hex(&unit.spec))));
                        }
                    };
                    ensure_not_cancelled(unit, cancelled)?;
                    if lost.load(Ordering::Acquire) {
                        return Ok(None);
                    }
                    let mut packs = packs;
                    if !extra.is_empty() {
                        let pack = Pack::of(extra).map_err(Error::Protocol)?;
                        config.blobs.put_pack(&pack)?;
                        packs.push(pack.reference);
                    }
                    packs.sort();
                    packs.dedup();
                    ensure_not_cancelled(unit, cancelled)?;
                    let commit = Command::CommitPublication(carrier::CommitPublication {
                        content,
                        execution,
                        attempt: claimed.attempt,
                        executor: executor.clone(),
                        packs,
                    });
                    match client.call(commit)? {
                        Answer::Publication(carrier::Publication {
                            packs: Some(packs),
                            accepted_attempt: Some(accepted),
                            ..
                        }) => {
                            arrived.publication_attempt = Some(accepted.0);
                            ensure_not_cancelled(unit, cancelled)?;
                            mirror(config, unit, &packs)?;
                            ensure_not_cancelled(unit, cancelled)?;
                            deliver(client, binding, &packs)?;
                            acknowledge(client, content, execution, &executor, claimed.attempt)?;
                            Ok(Some(packs))
                        }
                        Answer::Error(error) if Refusal::of(&error) == Some(Refusal::Ended) => {
                            Err(ended(client, binding))
                        }
                        Answer::Error(error)
                            if matches!(
                                Refusal::of(&error),
                                Some(Refusal::AttemptMoved | Refusal::NotHolder)
                            ) =>
                        {
                            Ok(None)
                        }
                        other => Err(Error::Protocol(format!("publication answered {other:?}"))),
                    }
                },
            )?;
            let Some(packs) = committed else {
                // This attempt was fenced during its tail. Uploaded immutable blobs are
                // harmless; only a current lead may make their manifest visible.
                continue;
            };
            arrived.values = import(&config.blobs, &packs)?;
            ensure_not_cancelled(unit, cancelled)?;
            arrived.packs = packs;
            return Ok(arrived);
        }
        waited = config.poll;
        let stopped = hold(
            config,
            unit,
            binding,
            &executor,
            claimed.lead,
            config.deadline.saturating_sub(started.elapsed()),
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
                return Err(Error::Protocol(format!(
                    "unit {}: another executor's front end differs from this one's",
                    hex(&unit.spec)
                )));
            }
            StopWhy::Protocol(why) => return Err(Error::Protocol(why)),
        }
    }
}

fn mirror(config: &Config, unit: &Unit, packs: &[PackRef]) -> Result<(), Error> {
    let expected = list_of(packs);
    config.blobs.publish(&unit.spec, &expected)?;
    // A 412 is accepted only after the existing and proposed full outputs were
    // verified equal, even if their native pack partitions differ. Missing or
    // corrupt committed packs refuse delivery; they never start another compile.
    Ok(())
}

fn publication(
    client: &mut Client,
    content: blaze_wire::JobContent,
    execution: ExecutionId,
) -> Result<Option<(Vec<PackRef>, Attempt, Attempt)>, Error> {
    match answer(
        client,
        Command::Publication(carrier::Packs { content, execution }),
        "publication",
    )? {
        Answer::Publication(value) => match (value.packs, value.accepted_attempt) {
            (Some(packs), Some(accepted)) => Ok(Some((packs, value.attempt, accepted))),
            (None, None) => Ok(None),
            _ => Err(Error::Protocol("publication lacks its acceptance provenance".to_owned())),
        },
        other => Err(Error::Protocol(format!("publication answered {other:?}"))),
    }
}

/// Compilation stays on the compiler's thread while a scoped driver keeps the lead alive
/// through the unit's tail. Fencing is checked atomically by CommitPublication afterward.
#[allow(clippy::too_many_arguments)]
fn heartbeat_during<T>(
    config: &Config,
    content: blaze_wire::JobContent,
    execution: ExecutionId,
    executor: &str,
    attempt: Attempt,
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
    let finished = AtomicBool::new(false);
    let lost = AtomicBool::new(false);
    std::thread::scope(|scope| {
        let (finished, lost) = (&finished, &lost);
        let heartbeat = scope.spawn(move || {
            let mut client = match Client::connect(&config.socket, random_id()?) {
                Ok(client) => client,
                Err(error) => {
                    lost.store(true, Ordering::Release);
                    return Err(error);
                }
            };
            let bound = now_ms().saturating_add(config.deadline.as_millis() as u64);
            let mut next = now_ms();
            while !finished.load(Ordering::Acquire)
                && !cancelled.load(Ordering::Acquire)
                && now_ms() <= bound
            {
                if now_ms() >= next {
                    let command = Command::WorkerEvents(carrier::WorkerEvents {
                        content,
                        executor: executor.to_owned(),
                        request: WorkersEventsRequest { execution, attempt, events: Vec::new() },
                    });
                    match client.call(command) {
                        Ok(Answer::WorkerEvents(answer)) if answer.verdict == 0 => {}
                        _ => {
                            lost.store(true, Ordering::Release);
                            break;
                        }
                    }
                    next = now_ms().saturating_add(config.holder.heartbeat_ms);
                }
                std::thread::sleep(config.poll.min(Duration::from_millis(50)));
            }
            let _ = client.release();
            Ok::<(), Error>(())
        });
        let end = End(finished);
        let result = complete(lost);
        drop(end);
        let _ = heartbeat.join();
        result
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
    let _ = client.release();
}

/// Why the execution ended with nothing committed: its terminal for this subscription.
fn ended(client: &mut Client, binding: &Binding) -> Error {
    match client.call(Command::Terminal(binding.subscription.clone())) {
        Ok(Answer::Terminal(carrier::Terminal { value: Some(value), .. })) => {
            Error::Ended(value.termination)
        }
        Ok(other) => {
            Error::Protocol(format!("the execution ended and its terminal says {other:?}"))
        }
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
) -> Result<Option<Vec<PackRef>>, Error> {
    match answer(client, Command::Packs(carrier::Packs { content, execution }), "packs")? {
        Answer::Packs(packs) => Ok(packs.packs),
        other => Err(Error::Protocol(format!("packs answered {other:?}"))),
    }
}

#[allow(clippy::too_many_arguments)]
fn hold(
    config: &Config,
    unit: &Unit,
    binding: &Binding,
    executor: &str,
    lead: bool,
    gate_timeout: Duration,
    execution: ExecutionId,
    attempt: Attempt,
    now_ms: &(impl Fn() -> u64 + Sync),
    granted: &mut dyn Granted,
    cancelled: &AtomicBool,
    arrived: &mut Arrived,
) -> Result<StopWhy, Error> {
    let content = binding.subscription.content;
    let mut holder = Holder::new(
        executor,
        execution,
        attempt,
        root_set(unit),
        Set::new(Vec::new()),
        BTreeSet::new(),
        config.holder.clone(),
    );
    let mut client = Client::connect(&config.socket, random_id()?)?;
    let mut watcher = Client::connect(&config.socket, random_id()?)?;
    let leave = AtomicBool::new(false);
    let working = AtomicBool::new(true);
    let driver = DriverConfig {
        // A lead's intentional silence moves the attempt. A same-attempt helper can
        // legitimately receive Ended after the survivor finishes, which is not the
        // stale-attempt refusal this acceptance probe is intended to demonstrate.
        result_gate: if lead {
            crate::result_gate::ResultGate::from_env(gate_timeout)?
        } else {
            None
        },
        ..DriverConfig::default()
    };
    let mut batches = Batches {
        unit,
        blobs: &config.blobs,
        granted,
        compiled: 0,
        stored: 0,
        grants: 0,
        compiled_keys: Vec::new(),
    };
    let stopped = std::thread::scope(|scope| {
        let (leave, working) = (&leave, &working);
        let watching = scope.spawn(move || {
            let mut waited = config.poll;
            while working.load(Ordering::Acquire) {
                if cancelled.load(Ordering::Acquire) {
                    leave.store(true, Ordering::Release);
                    break;
                }
                match committed(&mut watcher, content, execution) {
                    Ok(Some(_)) => {
                        leave.store(true, Ordering::Release);
                        break;
                    }
                    Ok(None) => {}
                    // The holder's own calls say what this is; its heartbeat ends it.
                    Err(_) => {}
                }
                std::thread::sleep(waited);
                waited = (waited * 2).min(POLL_MOST);
            }
            let _ = watcher.release();
        });
        let stopped = run_holder_until(
            &mut client,
            &config.socket,
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
    arrived.compiled_here += batches.compiled;
    arrived.stored_here += batches.stored;
    arrived.grants += batches.grants;
    arrived.compiled_keys.extend(batches.compiled_keys);
    let _ = client.release();
    Ok(stopped)
}

/// The final verdict follows the full manifest commit and successful cache delivery.
fn verdict(
    client: &mut Client,
    content: blaze_wire::JobContent,
    execution: ExecutionId,
    executor: &str,
    attempt: Attempt,
    outcome: Outcome,
) -> Result<(), Error> {
    let events = Command::WorkerEvents(carrier::WorkerEvents {
        content,
        executor: executor.to_owned(),
        request: WorkersEventsRequest {
            execution,
            attempt,
            events: vec![JobEvent::Done(DoneEvent { outcome, counters: Vec::new() })],
        },
    });
    match answer(client, events, "ending the published unit")? {
        Answer::WorkerEvents(_) => Ok(()),
        other => Err(Error::Protocol(format!("the final verdict answered {other:?}"))),
    }
}

fn deliver(client: &mut Client, binding: &Binding, packs: &[PackRef]) -> Result<(), Error> {
    let command = Command::DeliverPublication(carrier::DeliverPublication {
        content: binding.subscription.content,
        execution: binding.subscription.execution,
        packs: packs.to_vec(), // alloc: the bounded immutable delivery record
    });
    match client.call(command)? {
        Answer::Error(error) if Refusal::of(&error) == Some(Refusal::Ended) => {
            Err(ended(client, binding))
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
            other => Err(Error::Protocol(format!(
                "publication delivery has no terminal verdict: {other:?}"
            ))),
        },
        other => Err(Error::Protocol(format!("publication delivery answered {other:?}"))),
    }
}

fn acknowledge(
    client: &mut Client,
    content: blaze_wire::JobContent,
    execution: ExecutionId,
    executor: &str,
    attempt: Attempt,
) -> Result<(), Error> {
    let ack = Command::KillAck(carrier::KillAck {
        content,
        executor: executor.to_owned(),
        execution,
        attempt,
        alive: true,
    });
    match client.call(ack)? {
        // Delivery already requested every assignment's stop. A false acknowledgment
        // means that this old assignment was retired or replaced; it must not free the new one.
        Answer::KillAck(_) => Ok(()),
        Answer::Error(error) if Refusal::of(&error) == Some(Refusal::NoExecution) => Ok(()),
        other => Err(Error::Protocol(format!("kill acknowledgment answered {other:?}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn caller_cancellation_cannot_trigger_local_recompilation() {
        let unit = Unit { spec: [0x42; 32], names: Vec::new() }; // alloc: cancellation fixture
        let cancelled = AtomicBool::new(false);
        assert!(ensure_not_cancelled(&unit, &cancelled).is_ok());
        cancelled.store(true, Ordering::Release);
        let refused = ensure_not_cancelled(&unit, &cancelled).unwrap_err();
        assert!(matches!(&refused, Error::Cancelled(why) if why.contains(&hex(&unit.spec))));
        assert!(matches!(decide(&LocalPolicy::Fallback, refused), Err(Error::Cancelled(_))));
    }

    #[test]
    fn published_pack_list_requires_exact_sorted_unique_records() {
        let unit = Unit { spec: [0x33; 32], names: Vec::new() }; // alloc: test fixture
        let first = PackRef {
            pack: [0x11; 32],
            size: 10,
            keys: [0x44; 32],
            packed: None,
            format: blaze_wire::DIST_PACK_FORMAT,
        };
        let second = PackRef { pack: [0x22; 32], ..first.clone() };
        let canonical = list_of(&[first.clone(), second.clone()]);
        assert!(published_packs(&unit, &canonical).is_ok());
        assert!(published_packs(&unit, b"").is_ok(), "an empty unit is canonical");

        let noncanonical = [
            canonical[..canonical.len() - 1].to_vec(),
            [canonical.as_slice(), b"extra\n"].concat(),
            [canonical.as_slice(), b"\n"].concat(),
            list_of(&[second, first.clone()]),
            list_of(&[first.clone(), first]),
            b"not-a-pack\n".to_vec(),
            vec![0xff],
        ]; // alloc: corrupted manifest fixtures
        for bytes in noncanonical {
            assert!(
                matches!(published_packs(&unit, &bytes), Err(Error::Committed(_))),
                "a damaged published manifest must be refused, not rebuilt"
            );
        }
    }

    #[test]
    fn a_committed_pack_missing_or_corrupt_must_not_rebuild_even_with_fallback() {
        let reference = PackRef {
            pack: [0x22; 32],
            size: 10,
            keys: [0x44; 32],
            packed: None,
            format: blaze_wire::DIST_PACK_FORMAT,
        };
        let missing = import_using(&[reference.clone()], |name| {
            assert_eq!(*name, reference.pack);
            Ok(None)
        })
        .unwrap_err();
        assert!(matches!(&missing, Error::Committed(why) if why.contains(&hex(&reference.pack))));
        assert!(matches!(decide(&LocalPolicy::Fallback, missing), Err(Error::Committed(_))));

        for error in [
            Error::Io(std::io::Error::from(std::io::ErrorKind::TimedOut)),
            Error::Protocol("GET returned 500".to_owned()),
        ] {
            let mut error = Some(error);
            let failed =
                import_using(&[reference.clone()], |_| Err(error.take().unwrap())).unwrap_err();
            assert!(matches!(decide(&LocalPolicy::Fallback, failed), Err(Error::Committed(_))));
        }

        let corrupt = import_using(&[reference], |_| {
            Ok(Some(b"bad pack".to_vec())) // alloc: deliberately corrupt committed pack fixture
        })
        .unwrap_err();
        assert!(matches!(decide(&LocalPolicy::Fallback, corrupt), Err(Error::Committed(_))));

        let unit = Unit { spec: [0x33; 32], names: Vec::new() }; // alloc: test fixture
        let malformed = published_packs(&unit, b"not a pack").unwrap_err();
        assert!(matches!(decide(&LocalPolicy::Fallback, malformed), Err(Error::Committed(_))));
    }
}
