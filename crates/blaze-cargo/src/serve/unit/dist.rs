//! A unit's codegen across executors (docs/distributed.md), when a build's request
//! names a coordinator (`BLAZE_DIST_SOCKET`) and a cache server (`BLAZE_DIST_BLOBS`).
//!
//! There is one compiler pipeline, [`Build`]. Each daemon that builds a unit walks it, and the
//! walk's keys name the work: equal keys are equal inputs, whatever machine computed them, so
//! two executors that disagree on anything a function's code depends on simply are not in the
//! same execution. Granted functions are compiled by [`Build::compile_granted`], the code a
//! store miss runs. Then the build runs as it always does, with the committed values in front
//! of its store ([`Overlay`]), as hits under the same key, reading and checking
//! them as the store's (`BLAZE_VERIFY_STORE` compiles sampled ones again), and what nothing
//! committed it compiles itself and says so ([`Dist::report`]). What it takes goes into its own
//! store with what it made ([`Dist::finish`]), so the link, the record and the next build have
//! one store to read.
//!
//! `BLAZE_DIST_LOCAL` is the job's `LocalPolicy`: by default a failure of the distributed path
//! stops the unit with an error naming it, as anything else blaze cannot do (no-fallback);
//! `fallback` asks for the unit to be compiled here instead, by the same [`Build`]. Neither runs
//! anything but blaze.

#![forbid(unsafe_code)]

use std::{collections::BTreeMap, io::Write as _, path::PathBuf, sync::atomic::AtomicBool, time::Duration};

use blaze_cas::{Key, Kind};
use blaze_codegen::{
    driver::{Build, Built, GrantedValues, Lookup, Put, WorkSet},
    InstanceKey,
    backend::Prepare,
};
use blaze_dist::holder::HolderConfig;
use blaze_dist_local::{
    agent::{self, Arrived, Grant, Granted, Source, Unit},
    blobs::Blobs,
    pack::Value,
    server::random_id,
};
use blaze_ids::ContentId;
use blaze_wire::{LocalPolicy, Namespace, PrNamespace, digest_of_encoded_name};

/// The coordinator's socket.
const SOCKET_VAR: &str = "BLAZE_DIST_SOCKET";
/// The cache server, `host:port`.
const BLOBS_VAR: &str = "BLAZE_DIST_BLOBS";
/// `never` (the default) or `fallback`.
const LOCAL_VAR: &str = "BLAZE_DIST_LOCAL";
/// A file each unit appends one line of JSON to: what the harness and a person read.
const REPORT_VAR: &str = "BLAZE_DIST_REPORT";
/// How long a unit may wait for its execution, in seconds.
const DEADLINE_VAR: &str = "BLAZE_DIST_DEADLINE_SECS";
/// The pull request this build is of: its executions are its own, and a newer
/// [`SOURCE_VAR`] of it supersedes an older one's. What it publishes is shared by content.
const PR_VAR: &str = "BLAZE_DIST_PR";
/// Which revision of [`PR_VAR`] this build is of: any text, such as a commit.
const SOURCE_VAR: &str = "BLAZE_DIST_SOURCE";

/// How long a unit waits for its execution by default: a long unit on a loaded machine.
const DEADLINE: Duration = Duration::from_secs(3_600);

/// A request's distributed settings.
pub(super) struct Dist {
    config: agent::Config,
    report: Option<PathBuf>,
}

fn var<'a>(env: &'a [(String, String)], name: &str) -> Option<&'a str> {
    env.iter().rev().find(|(it, _)| it == name).map(|(_, value)| value.as_str())
}

impl Dist {
    /// What `env` asks for: `None` for a build of this daemon alone. `host` is this machine's
    /// triple: executors are machines, whatever the unit is compiled for, which its keys cover (a
    /// proc macro's unit is `wasm32-wasip1`, and no machine is one).
    pub(super) fn of(env: &[(String, String)], host: &str) -> Result<Option<Dist>, String> {
        let Some(socket) = var(env, SOCKET_VAR) else {
            for name in [BLOBS_VAR, LOCAL_VAR, REPORT_VAR, DEADLINE_VAR, PR_VAR, SOURCE_VAR] {
                if let Some(value) = var(env, name) {
                    return Err(format!("{name} is {value}, with no {SOCKET_VAR} to run its units")); // alloc: an error
                }
            }
            return Ok(None);
        };
        let blobs =
            var(env, BLOBS_VAR).ok_or_else(|| format!("{SOCKET_VAR} needs {BLOBS_VAR}"))?;
        let address = blobs.parse().map_err(|e| format!("{BLOBS_VAR} is {blobs}: {e}"))?;
        let local = match var(env, LOCAL_VAR) {
            None | Some("never") => LocalPolicy::Never,
            Some("fallback") => LocalPolicy::Fallback,
            Some(other) => {
                return Err(format!("{LOCAL_VAR} is {other}: it is `never` or `fallback`"));
            }
        };
        let deadline = match var(env, DEADLINE_VAR) {
            None => DEADLINE,
            Some(it) => Duration::from_secs(
                it.parse().map_err(|e| format!("{DEADLINE_VAR} is {it}: {e}"))?,
            ),
        };
        let namespace = match var(env, PR_VAR) {
            None => Namespace::Main,
            Some(it) => Namespace::Pr(PrNamespace {
                n: it.parse().map_err(|e| format!("{PR_VAR} is {it}: {e}"))?,
            }),
        };
        let source = match (var(env, SOURCE_VAR), &namespace) {
            (Some(it), Namespace::Pr(_)) => digest_of_encoded_name("blaze/dist-source", it.as_bytes()),
            (None, Namespace::Pr(_)) => return Err(format!("{PR_VAR} needs {SOURCE_VAR}")),
            (Some(it), Namespace::Main) => {
                return Err(format!("{SOURCE_VAR} is {it}, with no {PR_VAR} it is a revision of"));
            }
            (None, Namespace::Main) => [0; 32],
        };
        let claimant = random_id().map_err(|e| format!("naming this agent: {e}"))?;
        Ok(Some(Dist {
            config: agent::Config {
                socket: PathBuf::from(socket),
                blobs: Blobs { address, timeout: Duration::from_secs(120) },
                claimant,
                platform: agent::platform(host),
                local,
                namespace,
                source,
                deadline,
                holder: HolderConfig { heartbeat_ms: 1_000, retry_ms: 5_000, idle_ms: 10 },
                poll: Duration::from_millis(10),
            },
            report: var(env, REPORT_VAR).map(PathBuf::from),
        }))
    }

    /// What a failure of the distributed path means for the unit named `what`.
    pub(super) fn or_compile_here(
        &self,
        what: String,
        failure: blaze_dist_local::Error,
    ) -> Result<(), String> {
        match agent::decide(&self.config.local, failure) {
            Ok(()) => {
                tracing::warn!(target: "blaze", what, "dist: the unit compiles here");
                Ok(())
            }
            Err(failure) => Err(format!(
                "the distributed build failed: {what}: {failure} ({LOCAL_VAR}=fallback compiles the unit here instead)"
            )),
        }
    }

    /// The executors' values for `work`, its grants compiled by `build`. `store` is the
    /// reader of this daemon's store a grant finds its own values in.
    pub(super) fn run<B: Prepare>(
        &self,
        build: &Build<'_, B>,
        work: &WorkSet<B>,
        store: &impl Lookup,
        cancelled: &AtomicBool,
    ) -> Result<(Unit, Arrived), blaze_dist_local::Error> {
        let keys = work.names();
        let unit = Unit {
            spec: work.digest().0,
            names: keys.iter().map(|it| it.0.0).collect(),
        };
        let mut granted = Compiling { build, work, keys: &keys, store };
        let arrived = agent::run(&self.config, &unit, &mut granted, cancelled)?;
        Ok((unit, arrived))
    }

    /// Takes what arrived into what the build puts, publishes the unit for builds to come, and
    /// reports it. A build's store accepts a put it holds; two values under one key are refused
    /// there, naming the key.
    pub(super) fn finish(
        &self,
        name: &str,
        built: &mut Built,
        unit: &Unit,
        arrived: Arrived,
    ) -> Result<(), String> {
        let extra: Vec<Value> = built
            .puts
            .iter()
            .filter(|it| !arrived.values.contains_key(&it.key.0.0))
            .map(|it| Value { key: it.key.0.0, kind: it.kind.code(), bytes: it.bytes.clone() })
            .collect();
        self.report(name, built, &arrived);
        let published = arrived.source == Source::Published && extra.is_empty();
        if !published
            && let Err(why) = agent::publish(&self.config.blobs, unit, arrived.packs, extra)
        {
            // Nothing is wrong with this build: the next one does its work again.
            tracing::warn!(target: "blaze", unit = name, %why, "dist: the unit was not published");
        }
        for (key, (kind, bytes)) in arrived.values {
            let kind = Kind::from_code(kind)
                .ok_or_else(|| format!("a committed value of `{name}` has the kind {kind}"))?;
            built.puts.push(Put { key: Key(ContentId(key)), kind, bytes });
        }
        Ok(())
    }

    fn report(&self, name: &str, built: &Built, arrived: &Arrived) {
        let Some(path) = &self.report else { return };
        let source = match arrived.source {
            Source::Published => "published",
            Source::Executed => "executed",
        };
        let line = format!(
            "{{\"unit\":{name:?},\"source\":\"{source}\",\"execution\":{execution},\"joined\":{joined},\"grants\":{grants},\"compiled_for_work\":{compiled},\"stored_for_work\":{stored},\"arrived\":{values},\"tail_compiled\":{tail},\"from_store\":{hits},\"instances\":{instances}}}\n",
            execution = arrived.execution.map_or(-1, |it| it as i64),
            joined = arrived.joined,
            grants = arrived.grants,
            compiled = arrived.compiled_here,
            stored = arrived.stored_here,
            values = arrived.values.len(),
            tail = built.stats.compiled,
            hits = built.stats.stored,
            instances = built.stats.instances,
        );
        let written = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .and_then(|mut file| file.write_all(line.as_bytes()));
        if let Err(why) = written {
            tracing::warn!(target: "blaze", path = %path.display(), %why, "dist: the report");
        }
    }
}

/// The committed values in front of a store.
pub(super) struct Overlay<'a, S> {
    pub(super) store: &'a S,
    pub(super) values: &'a BTreeMap<[u8; 32], (u8, Vec<u8>)>,
}

impl<S: Lookup> Lookup for Overlay<'_, S> {
    fn read(&self, key: &Key) -> Result<Option<&[u8]>, String> {
        match self.store.read(key)? {
            Some(bytes) => Ok(Some(bytes)),
            None => Ok(self.values.get(&key.0.0).map(|(_, bytes)| bytes.as_slice())),
        }
    }
}

struct Compiling<'a, 'b, B: Prepare, S> {
    build: &'a Build<'b, B>,
    work: &'a WorkSet<B>,
    keys: &'a [InstanceKey],
    store: &'a S,
}

impl<B: Prepare, S: Lookup> Granted for Compiling<'_, '_, B, S> {
    fn compile(&mut self, names: &[usize]) -> Result<Grant, String> {
        let keys: Vec<InstanceKey> = names.iter().map(|at| self.keys[*at]).collect();
        let _span = tracing::info_span!(target: "blaze", "dist: a grant", functions = keys.len())
            .entered();
        let GrantedValues { compiled, stored, stats } = self
            .build
            .compile_granted(self.work, &keys, self.store)
            .map_err(|errors| format!("{} of the grant's functions failed: {:?}", errors.len(), errors.first()))?;
        let values = compiled
            .into_iter()
            .chain(stored)
            .map(|it| Value { key: it.key.0.0, kind: it.kind.code(), bytes: it.bytes })
            .collect();
        Ok(Grant {
            values,
            compiled: stats.compiled as u64,
            stored: keys.len().saturating_sub(stats.compiled) as u64,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn distributed_options_without_a_socket_are_refused_instead_of_silently_ignored() {
        assert!(
            matches!(Dist::of(&[], "aarch64-unknown-linux-gnu"), Ok(None)),
            "without distributed settings a local build is intentional"
        );
        for (name, value) in [
            (BLOBS_VAR, "127.0.0.1:4000"),
            (LOCAL_VAR, "fallback"),
            (REPORT_VAR, "dist.jsonl"),
            (DEADLINE_VAR, "30"),
            (PR_VAR, "81"),
            (SOURCE_VAR, "revision"),
        ] {
            let env = [(name.to_owned(), value.to_owned())]; // alloc: test fixture
            let why = Dist::of(&env, "aarch64-unknown-linux-gnu")
                .err()
                .expect("a distributed option cannot be ignored when the socket is missing");
            assert!(why.contains(name) && why.contains(SOCKET_VAR), "{why}");
        }
    }
}
