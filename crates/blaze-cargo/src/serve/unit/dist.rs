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

use std::{
    cell::RefCell,
    collections::{BTreeMap, BTreeSet},
    io::Write as _,
    path::PathBuf,
    sync::atomic::AtomicBool,
    time::Duration,
};

use blaze_cas::{Key, Kind};
use blaze_codegen::{
    CodegenError, InstanceKey,
    backend::Prepare,
    driver::{Build, Built, GrantedValues, Lookup, Put, WorkSet},
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

/// A causal compiler refusal retains its diagnostics; path failures follow LocalPolicy.
#[derive(Debug)]
pub(super) enum RunError {
    Compile(Vec<CodegenError>),
    Path(blaze_dist_local::Error),
}

/// Store reads with their real kind, for exporting warm tail hits as well as fresh puts.
pub(super) trait Export: Lookup {
    fn value(&self, key: &Key) -> Result<Option<Value>, String>;
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
        let blobs = var(env, BLOBS_VAR).ok_or_else(|| format!("{SOCKET_VAR} needs {BLOBS_VAR}"))?;
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
            Some(it) => {
                Duration::from_secs(it.parse().map_err(|e| format!("{DEADLINE_VAR} is {it}: {e}"))?)
            }
        };
        let namespace = match var(env, PR_VAR) {
            None => Namespace::Main,
            Some(it) => Namespace::Pr(PrNamespace {
                n: it.parse().map_err(|e| format!("{PR_VAR} is {it}: {e}"))?,
            }),
        };
        let source = match (var(env, SOURCE_VAR), &namespace) {
            (Some(it), Namespace::Pr(_)) => {
                digest_of_encoded_name("blaze/dist-source", it.as_bytes())
            }
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
    pub(super) fn run<'b, B, S, F>(
        &self,
        build: &mut Build<'b, B>,
        work: &WorkSet<B>,
        store: &S,
        cancelled: &AtomicBool,
        complete: F,
    ) -> Result<(Unit, Arrived, Option<Built>), RunError>
    where
        B: Prepare,
        S: Export,
        F: FnMut(&mut Build<'b, B>, &Recording<'_, S>) -> Result<Built, Vec<CodegenError>>,
    {
        let keys = work.names();
        let unit = Unit { spec: work.digest().0, names: keys.iter().map(|it| it.0.0).collect() };
        let mut granted = Compiling {
            build,
            work,
            keys: &keys,
            store,
            complete,
            completed: None,
            kept: BTreeMap::new(),
        };
        let arrived = agent::run(&self.config, &unit, &mut granted, cancelled);
        let built = match granted.completed {
            Some(Err(errors)) => return Err(RunError::Compile(errors)),
            Some(Ok(built)) => Some(built),
            None => None,
        };
        Ok((unit, arrived.map_err(RunError::Path)?, built))
    }

    /// Every imported value joins the normal store puts, including warm values exported by
    /// the lead. Identical local puts coalesce; a conflict names the key rather than hiding it.
    pub(super) fn finish(
        &self,
        name: &str,
        built: &mut Built,
        unit: &Unit,
        arrived: Arrived,
    ) -> Result<(), String> {
        self.report(name, unit, built, &arrived);
        let mut local: BTreeMap<Key, usize> =
            built.puts.iter().enumerate().map(|(at, put)| (put.key, at)).collect(); // alloc: one index per local put
        for (key, (kind, bytes)) in arrived.values {
            let kind = Kind::from_code(kind)
                .ok_or_else(|| format!("a committed value of `{name}` has the kind {kind}"))?;
            let key = Key(ContentId(key));
            match local.get(&key).copied() {
                Some(at) if built.puts[at].kind != kind || built.puts[at].bytes != bytes => {
                    return Err(format!(
                        "the local and committed values of `{name}` differ under {key:?}"
                    ));
                }
                Some(_) => {}
                None => {
                    local.insert(key, built.puts.len());
                    built.puts.push(Put { key, kind, bytes });
                }
            }
        }
        Ok(())
    }

    fn report(&self, name: &str, unit: &Unit, built: &Built, arrived: &Arrived) {
        let Some(path) = &self.report else { return };
        let source = match arrived.source {
            Source::Published => "published",
            Source::Executed => "executed",
        };
        let work_keys: Vec<String> =
            arrived.compiled_keys.iter().map(|key| blaze_dist_local::pack::hex(key)).collect();
        let tail_keys: Vec<String> = built
            .stats
            .compiled_keys
            .iter()
            .map(|key| blaze_dist_local::pack::hex(&key.0.0))
            .collect();
        let line = format!(
            "{{\"unit\":{name:?},\"source\":\"{source}\",\"execution\":{execution},\"joined\":{joined},\"grants\":{grants},\"compiled_for_work\":{compiled},\"stored_for_work\":{stored},\"arrived\":{values},\"tail_compiled\":{tail},\"from_store\":{hits},\"instances\":{instances},\"unit_digest\":\"{unit_digest}\",\"attempt\":{attempt},\"work_compiled_keys\":{work_keys},\"tail_compiled_keys\":{tail_keys}}}\n",
            unit_digest = blaze_dist_local::pack::hex(&unit.spec),
            attempt = arrived.attempt.map_or(-1, |it| it as i64),
            work_keys = blaze_dist_local::telemetry::json(&work_keys),
            tail_keys = blaze_dist_local::telemetry::json(&tail_keys),
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

/// Records the keys of successful reads, not copies of their bytes. Completion exports
/// their verified kinds and bytes once, including debug tables named by cached templates.
pub(super) struct Recording<'a, S> {
    overlay: Overlay<'a, S>,
    seen: RefCell<BTreeSet<Key>>,
}

impl<S: Lookup> Lookup for Recording<'_, S> {
    fn read(&self, key: &Key) -> Result<Option<&[u8]>, String> {
        let bytes = self.overlay.read(key)?;
        if bytes.is_some() {
            self.seen.borrow_mut().insert(*key);
        }
        Ok(bytes)
    }
}

impl<S: Export> Recording<'_, S> {
    fn value(&self, key: &Key) -> Result<Value, String> {
        if let Some(value) = self.overlay.store.value(key)? {
            return Ok(value);
        }
        self.overlay
            .values
            .get(&key.0.0)
            .map(|(kind, bytes)| Value { key: key.0.0, kind: *kind, bytes: bytes.clone() })
            .ok_or_else(|| {
                format!("the completed unit read {key:?}, which its store no longer holds")
            })
    }

    fn export(&self, built: &Built) -> Result<Vec<Value>, String> {
        // alloc: one owned value per consumed or produced key, moved into one output pack
        let mut values = BTreeMap::new();
        for put in &built.puts {
            values.insert(
                put.key,
                Value { key: put.key.0.0, kind: put.kind.code(), bytes: put.bytes.clone() },
            );
        }
        let mut pending = self.seen.borrow().clone(); // clone: the bounded key set, not value bytes
        for function in &built.functions {
            if let Some(side) = function.side {
                pending.insert(blaze_codegen::query::keys::side(&side));
            }
        }
        pending.extend(values.keys().copied());
        let mut visited = BTreeSet::new(); // alloc: one bounded mark per exported value
        while let Some(key) = pending.pop_first() {
            if !visited.insert(key) {
                continue;
            }
            if !values.contains_key(&key) {
                values.insert(key, self.value(&key)?);
            }
            let value = &values[&key];
            if value.kind == Kind::Code.code() {
                let side = match blaze_codegen::blob::decode(&value.bytes) {
                    Ok(template) => template.side,
                    Err(function) => {
                        blaze_codegen::blob::decode_static(&value.bytes).map_err(|static_| {
                            format!("the completed unit's code {key:?} is neither a function ({function}) nor a static ({static_})")
                        })?;
                        None
                    }
                };
                if let Some(side) = side {
                    pending.insert(blaze_codegen::query::keys::side(&side));
                }
            }
        }
        Ok(values.into_values().collect())
    }
}

struct Compiling<'a, 'b, B: Prepare, S, F> {
    build: &'a mut Build<'b, B>,
    work: &'a WorkSet<B>,
    keys: &'a [InstanceKey],
    store: &'a S,
    complete: F,
    completed: Option<Result<Built, Vec<CodegenError>>>,
    // alloc: this holder's outputs, available to later grants' memo templates.
    kept: BTreeMap<[u8; 32], (u8, Vec<u8>)>,
}

impl<'b, B, S, F> Granted for Compiling<'_, 'b, B, S, F>
where
    B: Prepare,
    S: Export,
    F: FnMut(&mut Build<'b, B>, &Recording<'_, S>) -> Result<Built, Vec<CodegenError>>,
{
    fn compile(&mut self, names: &[usize]) -> Result<Grant, String> {
        let keys: Vec<InstanceKey> = names.iter().map(|at| self.keys[*at]).collect();
        let _span =
            tracing::info_span!(target: "blaze", "dist: a grant", functions = keys.len()).entered();
        let GrantedValues { compiled, stored, stats } = self
            .build
            .compile_granted(self.work, &keys, &Overlay { store: self.store, values: &self.kept })
            .map_err(|errors| {
                format!("{} of the grant's functions failed: {:?}", errors.len(), errors.first())
            })?;
        if std::env::var_os("BLAZE_DIST_EVENTS").is_some() {
            let release = self.work.release_keys();
            let folded = self.work.folded_keys();
            let hex = |key: &Key| blaze_dist_local::pack::hex(&key.0.0);
            let actual: Vec<_> = stats.compiled_keys.iter().map(hex).collect();
            let release: Vec<_> = stats
                .compiled_keys
                .iter()
                .filter(|key| release.binary_search(key).is_ok())
                .map(hex)
                .collect();
            let folded: Vec<_> = stats
                .compiled_keys
                .iter()
                .filter(|key| folded.binary_search(key).is_ok())
                .map(hex)
                .collect();
            let work: Vec<_> =
                keys.iter().map(|key| blaze_dist_local::pack::hex(&key.0.0)).collect();
            blaze_dist_local::telemetry::emit(
                "backend_work",
                &[
                    ("pid", std::process::id().to_string()),
                    (
                        "unit_digest",
                        blaze_dist_local::telemetry::json(&blaze_dist_local::pack::hex(
                            &self.work.digest().0,
                        )),
                    ),
                    ("work", blaze_dist_local::telemetry::json(&work)),
                    ("compiled_keys", blaze_dist_local::telemetry::json(&actual)),
                    ("release_keys", blaze_dist_local::telemetry::json(&release)),
                    ("folded_keys", blaze_dist_local::telemetry::json(&folded)),
                ],
            );
        }
        let values: Vec<Value> = compiled
            .into_iter()
            .chain(stored)
            .map(|it| Value { key: it.key.0.0, kind: it.kind.code(), bytes: it.bytes })
            .collect();
        for value in &values {
            match self.kept.get(&value.key) {
                Some((kind, bytes)) if *kind != value.kind || *bytes != value.bytes => {
                    return Err(format!(
                        "successive grants disagree under {}",
                        blaze_dist_local::pack::hex(&value.key)
                    ));
                }
                Some(_) => {}
                None => {
                    self.kept.insert(value.key, (value.kind, value.bytes.clone()));
                }
            }
        }
        Ok(Grant {
            values,
            compiled: stats.compiled as u64,
            stored: stats.stored as u64,
            compiled_keys: stats.compiled_keys.into_iter().map(|key| key.0.0).collect(),
        })
    }

    fn complete(
        &mut self,
        values: &BTreeMap<[u8; 32], (u8, Vec<u8>)>,
    ) -> Result<Vec<Value>, String> {
        let recording = Recording {
            overlay: Overlay { store: self.store, values },
            seen: RefCell::new(BTreeSet::new()),
        };
        let built = (self.complete)(self.build, &recording);
        let exported = match &built {
            Ok(built) => recording.export(built),
            Err(errors) => {
                Err(format!("{} errors completing the unit: {:?}", errors.len(), errors.first()))
            }
        };
        self.completed = Some(built);
        exported
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use blaze_codegen::{
        Arch, Relocs, blob,
        template::{StaticTemplate, Template, TemplateInit},
    };

    struct Warm(BTreeMap<Key, Value>);
    impl Lookup for Warm {
        fn read(&self, key: &Key) -> Result<Option<&[u8]>, String> {
            Ok(self.0.get(key).map(|value| value.bytes.as_slice()))
        }
    }
    impl Export for Warm {
        fn value(&self, key: &Key) -> Result<Option<Value>, String> {
            Ok(self.0.get(key).cloned())
        }
    }

    #[test]
    fn a_warm_tail_exports_its_hits_and_debug_tables_to_an_empty_store() {
        let function = Key(ContentId([1; 32]));
        let summary = Key(ContentId([2; 32]));
        let static_ = Key(ContentId([3; 32]));
        let side = ContentId([4; 32]);
        let debug = blaze_codegen::query::keys::side(&side);
        let template = Template {
            arch: Arch::X86_64,
            align: 1,
            code: vec![0xc3],
            relocs: Relocs::new(),
            data: Vec::new(),
            inlined: Vec::new(),
            side: Some(side),
            unwind: None,
            lines: Vec::new(),
            refs: 0,
            externs: Vec::new(),
            coverage: None,
            profile: None,
        };
        let static_template = StaticTemplate {
            align: 8,
            init: TemplateInit::Zeroed { size: 8 },
            data: Vec::new(),
            mutable: false,
            thread_local: false,
            section: None,
            imports: Vec::new(),
            refs: 0,
        };
        let value = |key: Key, kind: Kind, bytes: Vec<u8>| {
            (key, Value { key: key.0.0, kind: kind.code(), bytes })
        };
        let warm = Warm(
            [
                value(function, Kind::Code, blob::encode(&template)),
                value(summary, Kind::Query, b"a cached summary".to_vec()),
                value(static_, Kind::Code, blob::encode_static(&static_template)),
                value(debug, Kind::Debug, b"a cached debug side table".to_vec()),
            ]
            .into(),
        );
        let empty = BTreeMap::new();
        let recording = Recording {
            overlay: Overlay { store: &warm, values: &empty },
            seen: RefCell::new(BTreeSet::new()),
        };
        for key in [function, summary, static_] {
            assert!(recording.read(&key).unwrap().is_some());
        }
        let exported = recording.export(&Built::default()).unwrap();
        let imported: BTreeMap<_, _> =
            exported.into_iter().map(|value| (value.key, (value.kind, value.bytes))).collect();
        let cold = Warm(BTreeMap::new());
        let overlay = Overlay { store: &cold, values: &imported };
        for (key, value) in &warm.0 {
            assert_eq!(overlay.read(key).unwrap(), Some(value.bytes.as_slice()));
            assert_eq!(imported[&key.0.0].0, value.kind);
        }
        assert_eq!(imported.len(), warm.0.len(), "all warm hits, including the unread side table");
    }
}

#[cfg(test)]
mod configuration_tests {
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
