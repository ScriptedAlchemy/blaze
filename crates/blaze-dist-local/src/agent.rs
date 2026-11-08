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
//! 3. Once done it reads every committed pack, checking each against what was committed
//!    ([`crate::pack::read`]), and the lead ends the execution.
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
}

/// One grant's values.
pub struct Grant {
    /// Every value the grant made or found.
    pub values: Vec<Value>,
    /// How many functions this executor compiled for it.
    pub compiled: u64,
    /// How many it found in its own store.
    pub stored: u64,
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
        }
    }
}

fn wire_name(unit: &Digest, name: &Digest) -> WorkName {
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
/// that names no outcome (the carrier gone, a pack that fails its check) is no verdict either.
pub fn decide(policy: &LocalPolicy, failure: Error) -> Result<(), Error> {
    let action = match &failure {
        Error::Ended(termination) => local_action(policy, termination)
            .map_err(|why| Error::Protocol(format!("the terminal is refused: {why:?}")))?,
        Error::Committed(_) => LocalAction::Fail,
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
        self.grants += 1;
        let (outcome, packs): (fn(&Digest) -> ItemOutcome, Vec<PackRef>) =
            match self.granted.compile(&names) {
                Ok(grant) => {
                    self.compiled += grant.compiled;
                    self.stored += grant.stored;
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

/// How a pack that is not on the server ends its error.
const CANCELLED: &str = "the build was cancelled";

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
        let bytes = fetch(&reference.pack)?.ok_or_else(|| {
            Error::Committed(format!("pack {name} was committed and is gone"))
        })?;
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

/// One line per pack of a unit's published list.
fn list_of(packs: &[PackRef]) -> Vec<u8> {
    let mut out = String::new();
    for it in packs {
        out.push_str(&format!("{} {} {}\n", hex(&it.pack), it.size, hex(&it.keys)));
    }
    out.into_bytes()
}

fn unhex(text: &str) -> Option<Digest> {
    let mut out = [0; 32];
    if text.len() != 64 {
        return None;
    }
    for (at, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(text.get(at * 2..at * 2 + 2)?, 16).ok()?;
    }
    Some(out)
}

fn parse_list(list: &[u8]) -> Option<Vec<PackRef>> {
    let text = std::str::from_utf8(list).ok()?;
    let mut packs = Vec::new();
    for line in text.lines() {
        let mut fields = line.split(' ');
        let (pack, size, keys) = (fields.next()?, fields.next()?, fields.next()?);
        packs.push(PackRef {
            pack: unhex(pack)?,
            size: size.parse().ok()?,
            keys: unhex(keys)?,
            packed: None,
            format: blaze_wire::DIST_PACK_FORMAT,
        });
    }
    // The published list names a whole durable unit: reject malformed, reordered
    // or duplicated records, not merely records whose pack bytes later fail.
    let canonical = list_of(&packs); // alloc: once per published unit, to verify its wire form
    if packs.windows(2).any(|pair| pair[0] >= pair[1]) || canonical.as_slice() != list {
        return None;
    }
    Some(packs)
}

/// A corrupted published manifest is an error, not permission to silently recompile.
fn published_packs(unit: &Unit, list: &[u8]) -> Result<Vec<PackRef>, Error> {
    parse_list(list).ok_or_else(|| {
        // alloc: a malformed published manifest's refusal
        Error::Committed(format!("unit {}: published pack list is not canonical", hex(&unit.spec)))
    })
}

/// Publishes what `unit` is made of for builds to come: `packs` and a pack of `extra`, what the
/// build made beyond its work set (its statics, its release functions). Only after the unit
/// linked: a list names a whole unit, or the next build would believe a part.
pub fn publish(
    blobs: &Blobs,
    unit: &Unit,
    mut packs: Vec<PackRef>,
    extra: Vec<Value>,
) -> Result<(), Error> {
    if !extra.is_empty() {
        let pack = Pack::of(extra).map_err(Error::Protocol)?;
        blobs.put_pack(&pack)?;
        packs.push(pack.reference);
    }
    packs.sort();
    packs.dedup();
    blobs.publish(&unit.spec, &list_of(&packs))
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
    if let Some(list) = config.blobs.published(&unit.spec)? {
        let packs = published_packs(unit, &list)?;
        let values = import(&config.blobs, &packs)?;
        return Ok(Arrived { values, packs, ..Arrived::none(Source::Published, None) });
    }
    // A unit whose functions all stay with the build that links them (a release unit's, a folded
    // group's) has nothing to distribute: its list above is all it can take.
    if unit.names.is_empty() {
        return Ok(Arrived::none(Source::Executed, None));
    }
    let started = Instant::now();
    let mut client = Client::connect(&config.socket, random_id()?)?;
    let origin = client.welcome.now_ms;
    let now_ms = move || origin.saturating_add(started.elapsed().as_millis() as u64);
    let job = request(unit, config, now_ms());
    let binding = loop {
        let submit = Command::Submit(carrier::submission(&job, NEED));
        match client.call(submit)? {
            Answer::Binding(binding) => break binding,
            Answer::Error(error) if error.code() == Some(blaze_wire::ErrorCode::Busy) => {
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
    let content = binding.subscription.content;
    let execution = binding.subscription.execution;
    let mut arrived = Arrived::none(Source::Executed, Some(execution.0));
    let mut last_claim: Option<(String, Attempt, bool)> = None;
    let mut waited = config.poll;
    let packs = loop {
        if cancelled.load(Ordering::Acquire) {
            leave(&mut client, &binding);
            return Err(Error::Protocol(CANCELLED.to_owned()));
        }
        // The coordinator's own deadline ends the execution first, and its terminal says so:
        // this is for a coordinator that stopped ending anything.
        if started.elapsed() > config.deadline.saturating_add(GRACE) {
            leave(&mut client, &binding);
            return Err(Error::Protocol(format!(
                "unit {}: its execution did not finish in {:?}",
                hex(&unit.spec),
                config.deadline
            )));
        }
        if let Some(packs) = committed(&mut client, content, execution)? {
            break packs;
        }
        let claim =
            Command::Claim(carrier::Claim { content, execution, claimant: config.claimant });
        let claimed = match client.call(claim)? {
            Answer::Claimed(claimed) => claimed,
            Answer::Error(error) if Refusal::of(&error) == Some(Refusal::Ended) => {
                return Err(ended(&mut client, &binding));
            }
            other => return Err(Error::Protocol(format!("claim answered {other:?}"))),
        };
        let Some(executor) = claimed.executor else {
            let _waiting =
                tracing::info_span!(target: "blaze", "dist: no executor assigned yet").entered();
            std::thread::sleep(waited);
            waited = (waited * 2).min(POLL_MOST);
            continue;
        };
        waited = config.poll;
        last_claim = Some((executor.clone(), claimed.attempt, claimed.lead));
        arrived.joined = true;
        let stopped = hold(
            config,
            unit,
            &binding,
            &executor,
            execution,
            claimed.attempt,
            &now_ms,
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
    };
    arrived.values = import(&config.blobs, &packs)?;
    arrived.packs = packs;
    if let Some((executor, attempt, lead)) = last_claim {
        finish(&mut client, config, content, execution, &executor, attempt, lead, &now_ms);
    }
    // The session's capacity goes back now, not at the end of its fixed lifetime.
    let _ = client.release();
    Ok(arrived)
}

/// This build no longer waits for its execution: when nobody else does, it ends, and its
/// executors go back. A failure here costs what the coordinator's own deadline takes back.
fn leave(client: &mut Client, binding: &Binding) {
    let _ = client.call(Command::Cancel(binding.clone()));
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
    let driver = DriverConfig::default();
    let mut batches =
        Batches { unit, blobs: &config.blobs, granted, compiled: 0, stored: 0, grants: 0 };
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
    let _ = client.release();
    Ok(stopped)
}

/// The lead ends the execution, and every holder gives its executor back. A stop nobody asked
/// for yet is refused, so a helper asks until the lead's end reached it, within a bound; past
/// it the coordinator's own lease and deadline take the executor back.
#[allow(clippy::too_many_arguments)]
fn finish(
    client: &mut Client,
    config: &Config,
    content: blaze_wire::JobContent,
    execution: ExecutionId,
    executor: &str,
    attempt: Attempt,
    lead: bool,
    now_ms: &impl Fn() -> u64,
) {
    if lead {
        let done = JobEvent::Done(DoneEvent { outcome: Outcome::Passed, counters: Vec::new() });
        let events = Command::WorkerEvents(carrier::WorkerEvents {
            content,
            executor: executor.to_owned(),
            request: WorkersEventsRequest { execution, attempt, events: vec![done] },
        });
        let _ = client.call(events);
    }
    let bound = now_ms().saturating_add(10_000);
    while now_ms() < bound {
        let ack = Command::KillAck(carrier::KillAck {
            content,
            executor: executor.to_owned(),
            execution,
            attempt,
            alive: true,
        });
        match client.call(ack) {
            Ok(Answer::KillAck(answer)) if !answer.accepted => {
                let _waiting =
                    tracing::info_span!(target: "blaze", "dist: the lead ending the execution")
                        .entered();
                std::thread::sleep(config.poll);
            }
            _ => return,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
