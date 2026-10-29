//! The daemon of a user: one process that every `cargo ra-jit` of the user finds through one
//! socket. It loads nothing itself. It owns the workers, each a `ra-jit <project> --socket`
//! process with one database, starts them when a build asks, passes the builds on to them,
//! and stops them and itself when nobody has asked for a while.

use std::{
    collections::BTreeMap,
    ffi::OsString,
    fs::OpenOptions,
    io::{BufRead as _, BufReader, Write as _},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{
        Arc, Mutex, MutexGuard, PoisonError,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering::Relaxed},
    },
    time::{Duration, Instant, SystemTime},
};

use anyhow::Context as _;
use rustc_hash::FxHashMap;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

#[cfg(unix)]
use std::os::unix::net::UnixStream;
#[cfg(windows)]
use uds_windows::UnixStream;

use super::{plan::Plan, units::stamp_of};
use crate::socket::{self, NotServed, Served, private_dir, private_file, read_request};

/// How long the daemon and each worker live without being asked, unless
/// `RA_JIT_IDLE_SECS` says otherwise.
const IDLE: Duration = Duration::from_secs(1800);
/// How long a worker that is stopped has to finish writing to the store.
const SETTLE: Duration = Duration::from_secs(60);

/// How often the daemon looks for what is idle.
const TICK: Duration = Duration::from_secs(1);

/// How many checkers the daemon keeps loaded, each of another build: a check for several
/// targets, or checks of several workspaces in turn.
const CHECKERS: usize = 4;

/// The share of the machine's memory the workers may hold when `RA_JIT_MEMORY_MAX` does not
/// say: one part in this many.
const MEMORY_SHARE: u64 = 2;

/// Soft aggregate footprint target from `RA_JIT_MEMORY_MAX`: a size, or `off` for no
/// target. Half of the machine's memory when it does not say, and no limit where the system
/// does not tell how much that is.
pub(crate) fn memory_max() -> Result<Option<u64>, String> {
    match std::env::var("RA_JIT_MEMORY_MAX") {
        Err(_) => Ok(crate::memory::of_the_machine().map(|it| it / MEMORY_SHARE)),
        Ok(text) if text == "off" => Ok(None),
        Ok(text) => ra_jit_store::parse_size(&text)
            .map(Some)
            .ok_or_else(|| format!("RA_JIT_MEMORY_MAX=`{text}` is neither a size nor `off`")),
    }
}

/// The setting of when the daemon has a spare worker.
pub(crate) const SPARE: &str = "RA_JIT_SPARE";

/// When the daemon has a spare worker, from `RA_JIT_SPARE`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Spares {
    /// `0`: none. A workspace is loaded once cargo has built it.
    Never,
    /// Without it: one as the daemon starts, for the build that started it, and one for each
    /// build after it that the daemon is told of as cargo begins it. Each loads its workspace
    /// beside cargo; none waits for a build nobody has asked for.
    Asked,
    /// `ready`: as well, one is started again once a workspace is loaded, and waits with the
    /// library loaded for the next. The daemon holds it, 1.3 GB on Linux, for as long as it runs.
    Ready,
}

fn spares() -> Result<Spares, String> {
    match std::env::var(SPARE) {
        Err(_) => Ok(Spares::Asked),
        Ok(text) if text == "0" => Ok(Spares::Never),
        Ok(text) if text == "ready" => Ok(Spares::Ready),
        Ok(text) => Err(format!("RA_JIT_SPARE=`{text}` is neither `0` nor `ready`")),
    }
}

/// What a worker holds and how much it is in use.
struct Held {
    bytes: u64,
    known: bool,
    asked: Instant,
    /// Answering a request or loading a checkout.
    busy: bool,
}

/// Soft targets never discard an incremental cache. Persistent host critical pressure is
/// the only memory reason to stop a worker; busy workers own admission and progress.
fn over(holding: &[(String, Held)], most: u64, critical: bool) -> Vec<String> {
    if !critical {
        return Vec::new();
    }
    let mut all: u64 = holding.iter().map(|it| it.1.bytes).sum();
    let last = holding.iter().map(|it| it.1.asked).max();
    let mut idle: Vec<&(String, Held)> =
        holding.iter().filter(|it| it.1.known && !it.1.busy && Some(it.1.asked) != last).collect();
    idle.sort_by_key(|it| it.1.asked);
    let mut stopped = Vec::new();
    for (id, held) in idle {
        if all <= most {
            break;
        }
        all -= held.bytes;
        stopped.push(id.clone());
    }
    for (id, held) in holding {
        if held.known && !held.busy && held.bytes > most && !stopped.contains(id) {
            stopped.push(id.clone());
        }
    }
    stopped
}

/// Critical pressure must persist continuously, including while a live program prevents
/// eviction. A recovered or unreadable signal starts the grace period over.
fn pressure_persisted(
    since: &mut Option<Instant>,
    pressure: Option<hir_ty_codegen::budget::Pressure>,
    now: Instant,
    grace: Duration,
) -> bool {
    if pressure == Some(hir_ty_codegen::budget::Pressure::Critical) {
        now.saturating_duration_since(*since.get_or_insert(now)) >= grace
    } else {
        *since = None;
        false
    }
}

/// How often a worker that is loading is looked at.
const POLL: Duration = Duration::from_millis(20);

/// How long a worker that hung up without an answer is given to be seen to have exited: its
/// socket closes as it exits, a moment before it can be waited for.
const EXITING: Duration = Duration::from_secs(2);

/// How many of the last lines of a worker's log the daemon's log shows when it ends.
const LAST_LINES: usize = 12;

/// The variable that has a worker end when its standard input does, which is when the
/// daemon that started it is gone.
pub(crate) const ENDS_WITH_INPUT: &str = "RA_JIT_ENDS_WITH_INPUT";

/// The variable of a request that says which build of the daemon's it is for: a client's build
/// is one request to the daemon and several to its worker, which runs a build script that
/// cargo runs every build once for all of them.
pub(crate) const BUILD: &str = "RA_JIT_BUILD";

/// The variable that tells a worker which build started it, whose load runs the scripts for it.
pub(crate) const LOAD_BUILD: &str = "RA_JIT_LOAD_BUILD";

/// A worker with this marker reads its load snapshot from stdin before any reader starts.
/// Values stay in the pipe and in memory, never in the worker directory.
pub(crate) const LOAD_ENV: &str = "RA_JIT_LOAD_ENV";

/// Where a worker that listens at `socket` leaves the errors it found in the source as it
/// loaded the workspace, in JSON, before it ends.
pub(crate) fn errors_beside(socket: &Path) -> PathBuf {
    socket.with_file_name("errors")
}

/// A request with the version of the client that sends it.
#[derive(Serialize, Deserialize)]
pub(crate) struct Envelope {
    pub(crate) version: String,
    /// Whether the client starts a daemon of its own version when this one is of another.
    pub(crate) replaces: bool,
    pub(crate) request: Request,
}

#[derive(Serialize, Deserialize)]
pub(crate) enum Request {
    /// Bring the programs of a checkout up to date with its source, if the daemon is warm
    /// for it.
    Sync(Checkout),
    /// Load a checkout that cargo has just built, for the builds to come.
    Warm(Checkout),
    /// Load a checkout cargo has just built what stands in for the crates of, and answer
    /// when its programs are built.
    Build(Checkout),
    /// A workspace cargo is about to build what stands in for the crates of: a worker
    /// loads it meanwhile, for the [`Request::Build`] that follows.
    Ahead(Ahead),
    /// A line cargo printed of a build script it ran or a proc macro it built, for the
    /// worker of the [`Request::Ahead`] of this [`WorkerConfig::id`].
    Said {
        id: String,
        line: String,
    },
    /// No build follows the [`Request::Ahead`] of this [`WorkerConfig::id`].
    Forget(String),
    /// `cargo ra-jit check` of a build the daemon has no programs of: what a checker that
    /// loaded the workspace as it would finds in its crates, with no rustc.
    Check(Ahead),
    /// The legacy script wrapper hands its run environment to daemon memory, not disk.
    RememberScript {
        dir: PathBuf,
        env: crate::process::Native,
    },
    ScriptEnvironment(PathBuf),
    Status,
    Stop,
}

#[derive(Serialize, Deserialize, Debug)]
pub(crate) enum Reply {
    /// The daemon is of another version. It exits when the client said it replaces it.
    Restart {
        version: String,
    },
    /// The daemon has no part in this build, and why. Cargo builds.
    Cold {
        reason: String,
    },
    /// What the worker said of each program, in the order asked: its `ok` line, or why it
    /// did not build it.
    Synced {
        programs: Vec<Result<String, NotBuilt>>,
        #[serde(default)]
        runtime: Vec<Option<super::runtime_context::Context>>,
        /// The daemon's steps, for the client's trace. The worker's are in its `ok` lines.
        #[serde(default)]
        spans: Vec<crate::trace::Span>,
    },
    Warming,
    ScriptEnvironment(Option<crate::process::Native>),
    /// What the checker of a [`Request::Check`] found: no error, or the errors.
    Checked {
        errors: Option<crate::errors::Said>,
    },
    Status(Status),
    Stopping,
}

/// Why the worker did not build a program.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub(crate) enum NotBuilt {
    /// The source has errors: the build fails with them.
    Error(crate::errors::Said),
    /// The daemon failed.
    Failed(String),
    /// ra-jit does not build the source as it is, and cargo does.
    Cargos(String),
}

impl NotBuilt {
    pub(crate) fn why(self) -> String {
        match self {
            NotBuilt::Error(said) => said.to_string(),
            NotBuilt::Failed(why) | NotBuilt::Cargos(why) => why,
        }
    }

    /// What the worker's reply other than `ok` says.
    fn of(reply: &str) -> NotBuilt {
        if let Some(why) = reply.strip_prefix("cargo: ") {
            return NotBuilt::Cargos(why.to_owned());
        }
        match reply.strip_prefix("error: ") {
            Some(said) => NotBuilt::Error(crate::errors::Said::of_reply(said)),
            None => NotBuilt::Failed(crate::unfailed(reply)),
        }
    }
}

#[derive(Serialize, Deserialize, Debug)]
pub(crate) struct Status {
    pub(crate) pid: u32,
    pub(crate) version: String,
    pub(crate) idle_secs: Option<u64>,
    /// Soft aggregate footprint target, in bytes.
    #[serde(default)]
    pub(crate) memory_max: Option<u64>,
    pub(crate) workers: Vec<WorkerStatus>,
}

#[derive(Serialize, Deserialize, Debug)]
pub(crate) struct WorkerStatus {
    pub(crate) pid: u32,
    pub(crate) project: PathBuf,
    pub(crate) log: PathBuf,
    /// The bytes it holds, where the system tells.
    #[serde(default)]
    pub(crate) bytes: Option<u64>,
    /// Each checkout the worker holds and how warm it is.
    pub(crate) checkouts: Vec<(PathBuf, String)>,
}

/// What a worker is loaded with. Two builds with the same one share a worker.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub(crate) struct WorkerConfig {
    /// What the checkouts of one repository have in common: the directory of its `.git`.
    pub(crate) repository: PathBuf,
    /// The directory of the package being built, from the root of the checkout.
    pub(crate) project: PathBuf,
    pub(crate) cfg_test: bool,
    /// Which build the worker stands in for, as the daemon's own plan of it has it.
    pub(crate) selection: crate::plan::Selection,
    /// The variables the worker runs with that decide what it compiles and with what.
    pub(crate) env: BTreeMap<String, String>,
    /// Whether its programs are built whole, with nothing of rustc's ([`Plan::whole`]).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub(crate) whole: bool,
}

impl WorkerConfig {
    /// 16 hex digits: it names the worker's directory, and the path of the socket in it has
    /// to fit the 104 bytes macOS has for one, with the daemon's directory before it.
    pub(crate) fn id(&self) -> String {
        let text = serde_json::to_vec(self).unwrap_or_default();
        Sha256::digest(text).iter().take(8).map(|it| format!("{it:02x}")).collect()
    }
}

impl WorkerConfig {
    /// What a worker's command line says of the workspace under `root` and of `crates` of
    /// it: all that loading it and compiling its functions reads.
    fn args(&self, root: &Path, crates: &[PathBuf]) -> Vec<OsString> {
        let mut args: Vec<OsString> = vec![root.join(&self.project).into(), "--own-target".into()];
        args.extend(self.selection.flags().into_iter().map(Into::into));
        if self.cfg_test {
            args.push("--cfg-test".into());
        }
        if self.selection.dev_deps {
            args.push("--dev-deps".into());
        }
        if let Some(asked_in) = &self.selection.asked_in {
            args.extend(["--asked-in".into(), asked_in.into()]);
        }
        for krate in crates {
            args.push("--crate".into());
            args.push(krate.strip_prefix(root).unwrap_or(krate).into());
        }
        args
    }
}

/// What is known of a build before cargo has built anything of it.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub(crate) struct Ahead {
    pub(crate) config: WorkerConfig,
    #[serde(default)]
    pub(crate) native_env: Option<crate::process::Native>,
    /// The root of the checkout.
    pub(crate) root: PathBuf,
    /// Where the source of each crate the daemon compiles starts.
    pub(crate) crates: Vec<PathBuf>,
}

/// A checkout as cargo last built it, and the programs of it the daemon is asked for.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub(crate) struct Checkout {
    /// The build this is: the checkout, the command and its flags.
    pub(crate) key: String,
    /// The newest of rustc's output the programs are linked against. A worker that loaded
    /// the checkout with another one starts again.
    pub(crate) generation: u128,
    /// When cargo started, as a stamp: source newer than this is not what rustc compiled.
    pub(crate) built_at: u128,
    pub(crate) config: WorkerConfig,
    /// The root of the checkout.
    pub(crate) root: PathBuf,
    pub(crate) programs: Vec<Plan>,
    /// The source files rustc read. Only a request to warm names them.
    pub(crate) sources: Vec<PathBuf>,
    /// The variables `env!` reads in the checkout: the ones cargo set to say where its
    /// programs are, and in a request under them the ones of the process that asks.
    #[serde(default)]
    pub(crate) env: BTreeMap<String, String>,
    /// Full client snapshot, including native values and absence. Never rendered in logs.
    #[serde(default)]
    pub(crate) native_env: Option<crate::process::Native>,
    /// The file of what cargo said of the build scripts it ran and the proc macros it
    /// built, which a worker loads the checkout with.
    #[serde(default)]
    pub(crate) scripts: Option<PathBuf>,
}

impl Checkout {
    /// Whether one of the source files was written after `then`.
    fn written_since(&self, then: SystemTime) -> bool {
        let written = |file: &PathBuf| std::fs::metadata(file).and_then(|it| it.modified());
        self.sources.iter().any(|file| written(file).is_ok_and(|it| it > then))
    }

    fn project(&self) -> PathBuf {
        self.root.join(&self.config.project)
    }

    /// The line that asks a worker for `plan`.
    fn sync_line(&self, plan: &Plan) -> String {
        let named =
            |path: &PathBuf| path.strip_prefix(&self.root).unwrap_or(path).display().to_string();
        // Each with a comma after it: the program is of these crates and no others.
        let crates: String = plan.crates.iter().map(|it| named(it) + ",").collect();
        // A program built whole is linked by blaze into `binary`, with a line of the
        // worker's own, and its edits are spliced into that.
        let (splice, exe) = match plan.whole {
            true => (String::new(), plan.binary.display().to_string()),
            false => (plan.binary.display().to_string(), String::new()),
        };
        let request = if plan.whole { "sync-runtime-v1" } else { "sync" };
        let mut line = format!(
            "{request}\t{}\t{}\t{}\t{}\t{}\t{splice}\t{exe}",
            self.project().display(),
            crates,
            plan.object.display(),
            plan.patch.display(),
            plan.link_cmd,
        );
        for file in &plan.symbol_files {
            line.push('\t');
            line.push_str(&file.display().to_string());
        }
        line.push('\n');
        line
    }
}

/// How far a worker is with a checkout.
#[derive(Clone)]
enum Warmth {
    Loading(Instant),
    Ready,
    /// Why the worker has no part in the checkout as cargo last built it.
    Failed(String, SystemTime),
    /// The errors in the source the worker found loading it or building it first: each
    /// program's build fails with them until a source is written.
    Broken(crate::errors::Said, SystemTime),
}

impl Warmth {
    fn describe(&self) -> String {
        match self {
            Warmth::Loading(since) => format!("loading for {:.0?}", since.elapsed()),
            Warmth::Ready => "ready".to_owned(),
            Warmth::Failed(why, _) => format!("failed: {why}"),
            Warmth::Broken(..) => "the source has an error".to_owned(),
        }
    }
}

struct Loaded {
    generation: u128,
    root: PathBuf,
    request: String,
    warmth: Warmth,
}

fn request_key(checkout: &Checkout) -> String {
    // A fresh invocation's clock does not change the load. Including it would let an
    // unchanged build repeatedly bypass a pressure tombstone without changing its inputs.
    let mut inputs = checkout.clone();
    inputs.built_at = 0;
    Sha256::digest(serde_json::to_vec(&inputs).unwrap_or_default())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// A `ra-jit <project> --socket` process and the checkouts it holds.
struct Worker {
    project: PathBuf,
    socket: PathBuf,
    log: PathBuf,
    /// The variables the process runs with. A request says them with the ones of its
    /// checkout over them.
    env: crate::process::Native,
    child: Mutex<Child>,
    checkouts: Mutex<FxHashMap<String, Loaded>>,
    asked: Mutex<Instant>,
    /// The requests it is answering now.
    answering: AtomicUsize,
    /// Whether the daemon's log says it ended: it says so once.
    noted: AtomicBool,
    unknown_held: AtomicBool,
}

/// Counts a request among those a worker is answering for as long as it lives, also when the
/// thread that asks unwinds.
struct Answering<'a>(&'a AtomicUsize);

impl<'a> Answering<'a> {
    fn new(count: &'a AtomicUsize) -> Answering<'a> {
        count.fetch_add(1, Relaxed);
        Answering(count)
    }
}

impl Drop for Answering<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Relaxed);
    }
}

impl Worker {
    /// Starts a worker on the first program of `checkout`, with its files in `dir`.
    /// `spare` is a process that waits to be told a build, and is told this one.
    fn start(
        checkout: &Checkout,
        dir: PathBuf,
        spare: Option<Spare>,
        build: u64,
    ) -> anyhow::Result<Worker> {
        let first = checkout.programs.first().context("a checkout without programs")?;
        private_dir(&dir)?;
        let (socket, mut log) = (dir.join("sock"), dir.join("log"));
        // The worker of a daemon that was killed may still be there, on its way out: it
        // is not the one to connect to.
        let _ = std::fs::remove_file(&socket);
        let _ = std::fs::remove_file(errors_beside(&socket));
        let config = &checkout.config;
        let mut args = config.args(&checkout.root, &first.crates);
        let rustcs: [(&str, &Path); 5] = [
            ("--out", &first.object),
            ("--patch", &first.patch),
            ("--link-cmd", Path::new(&first.link_cmd)),
            ("--splice", &first.binary),
            ("--socket", &socket),
        ];
        let whole: [(&str, &Path); 4] = [
            ("--out", &first.object),
            ("--patch", &first.patch),
            ("--exe", &first.binary),
            ("--socket", &socket),
        ];
        let named: &[(&str, &Path)] = if first.whole { &whole } else { &rustcs };
        for &(flag, path) in named {
            args.push(flag.into());
            args.push(path.into());
        }
        if let Some(rustcs) = &first.rustc_binary {
            args.push("--rustc-binary".into());
            args.push(rustcs.into());
            // A plan of rustc's executable copies the files of crates that call by symbol
            // alone.
            let by_symbol = dir.join("by-symbol");
            let listed: String =
                first.copies.iter().map(|it| format!("{}\n", it.display())).collect();
            std::fs::write(&by_symbol, listed)?;
            args.push("--by-symbol-in".into());
            args.push(by_symbol.into());
        }
        // In a file: a program of many crates has more of them than fit the command line
        // of a process on Windows.
        let symbols = dir.join("symbols");
        let listed: String =
            first.symbol_files.iter().map(|it| format!("{}\n", it.display())).collect();
        std::fs::write(&symbols, listed)?;
        args.push("--symbols-in".into());
        args.push(symbols.into());
        if let Some(scripts) = &checkout.scripts {
            args.push("--scripts-from".into());
            args.push(scripts.into());
        }
        let env = checkout
            .native_env
            .clone()
            .unwrap_or_else(crate::process::Native::own)
            .over(config.env.iter().map(|(k, v)| (k.into(), v.into())))
            .over([(ENDS_WITH_INPUT.into(), "1".into())]);
        let asked = checkout.native_env.as_ref().unwrap_or(&env);
        let told = spare.and_then(|it| {
            it.told(
                &args,
                &checkout.config,
                &checkout.root,
                false,
                Some(env.clone()),
                Some(asked.clone()),
            )
        });
        let child = match told {
            Some(spare) => {
                log = spare.log;
                spare.child
            }
            None => {
                let mut command = Command::new(std::env::current_exe()?);
                env.apply(&mut command);
                clear_worker_memory(&mut command);
                command.args(&args);
                match store() {
                    Some(store) => command.env("RA_JIT_CACHE", store),
                    None => command.env_remove("RA_JIT_CACHE"),
                };
                let output =
                    private_file(OpenOptions::new().write(true).create(true).truncate(true), &log)?;
                command.envs(&config.env).env(ENDS_WITH_INPUT, "1");
                command.env(LOAD_BUILD, build.to_string());
                command.env(LOAD_ENV, "stdin");
                command.current_dir(&checkout.root);
                command.stdin(Stdio::piped()).stdout(output.try_clone()?).stderr(output);
                own_group(&mut command);
                let mut child = command.spawn().context("starting a worker")?;
                let sent = child.stdin.as_mut().context("worker load pipe")?.write_all(
                    format!("{}\n", crate::process::encode_native(asked, build)).as_bytes(),
                );
                if let Err(error) = sent {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(error).context("sending worker load environment");
                }
                child
            }
        };
        let loading = Loaded {
            generation: checkout.generation,
            root: checkout.root.clone(),
            request: request_key(checkout),
            warmth: Warmth::Loading(Instant::now()),
        };
        Ok(Worker {
            project: checkout.project(),
            socket,
            log,
            env,
            child: Mutex::new(child),
            checkouts: Mutex::new(FxHashMap::from_iter([(checkout.key.clone(), loading)])),
            asked: Mutex::new(Instant::now()),
            answering: AtomicUsize::new(0),
            noted: AtomicBool::new(false),
            unknown_held: AtomicBool::new(false),
        })
    }

    /// Whether the process has ended, and how: its exit status and what it said last.
    fn ended(&self) -> Option<String> {
        let status = exited(&mut lock(&self.child));
        status.map(|status| format!("{status}: {}", last_words(&self.log)))
    }

    /// Whether the process has ended, given `patience` to.
    fn ended_within(&self, patience: Duration) -> Option<String> {
        let asked = Instant::now();
        loop {
            match self.ended() {
                None if asked.elapsed() < patience => std::thread::sleep(POLL),
                ended => return ended,
            }
        }
    }

    /// Says in the daemon's log, once, that the process ended by itself: how, and the end
    /// of its own log, which has what it panicked with.
    fn note_end(&self) {
        let (pid, status) = {
            let mut child = lock(&self.child);
            (child.id(), exited(&mut child))
        };
        let Some(status) = status else { return };
        if self.noted.swap(true, Relaxed) {
            return;
        }
        eprintln!(
            "the worker of pid {pid} for {} ended ({status}); its log {} ends with:\n{}",
            self.project.display(),
            self.log.display(),
            log_tail(&self.log)
        );
    }

    fn stop(&self) {
        // One that ended by itself says so before it goes.
        self.note_end();
        kill(&mut lock(&self.child));
        let _ = std::fs::remove_file(&self.socket);
    }

    /// Ends its input, which ends it once the store has what its last build kept. After a
    /// build from nothing that takes a while, helix's about 20 s, and what is lost the next
    /// daemon compiles again.
    fn let_go(&self) {
        // Its end is then not news.
        self.note_end();
        self.noted.store(true, Relaxed);
        drop(lock(&self.child).stdin.take());
    }

    /// Stops it once it has gone by itself after [`Self::let_go`], or [`SETTLE`] after.
    fn finish(&self) {
        {
            let mut child = lock(&self.child);
            let given_up = Instant::now() + SETTLE;
            while matches!(child.try_wait(), Ok(None)) && Instant::now() < given_up {
                std::thread::sleep(Duration::from_millis(20));
            }
        }
        self.stop();
    }

    fn set(&self, key: &str, warmth: Warmth) {
        if let Some(loaded) = lock(&self.checkouts).get_mut(key) {
            loaded.warmth = warmth;
        }
    }

    /// Waits for the process to listen, then has it build every program of `checkout` once,
    /// for the daemon's `build` that asked.
    fn warm(&self, checkout: &Checkout, build: u64) {
        let warmth = match self.warmed(checkout, build) {
            Ok(None) => Warmth::Ready,
            Ok(Some(said)) => Warmth::Broken(said, SystemTime::now()),
            Err(why) => match std::fs::read(errors_beside(&self.socket)) {
                Ok(said) if self.ended().is_some() => match serde_json::from_slice(&said) {
                    Ok(said) => Warmth::Broken(said, SystemTime::now()),
                    Err(_) => Warmth::Failed(format!("{why:#}"), SystemTime::now()),
                },
                _ => Warmth::Failed(format!("{why:#}"), SystemTime::now()),
            },
        };
        eprintln!("{}: {}", checkout.root.display(), warmth.describe());
        self.set(&checkout.key, warmth);
        // Nobody could ask it for a build until now.
        *lock(&self.asked) = Instant::now();
    }

    /// Whether a checkout is still being loaded.
    fn loads(&self) -> bool {
        lock(&self.checkouts).values().any(|it| matches!(it.warmth, Warmth::Loading(_)))
    }

    /// The errors in the source, when the worker found any.
    fn warmed(
        &self,
        checkout: &Checkout,
        build: u64,
    ) -> anyhow::Result<Option<crate::errors::Said>> {
        self.connected()?;
        // A source written since cargo built is the daemon's like any edit: it compiles
        // every function and static of the workspace from what it reads. The program the
        // worker was started on is built already, and is asked for too: what the worker then
        // compiles again by itself, for the first edit to be a patch, is part of warming up.
        for plan in &checkout.programs {
            match self.ask(checkout, plan, build)? {
                Ok(_) => {}
                Err(NotBuilt::Error(said)) => return Ok(Some(said)),
                Err(why) => anyhow::bail!(why.why()),
            }
        }
        Ok(None)
    }

    /// A connection to the process, once it has loaded the workspace and listens.
    fn connected(&self) -> anyhow::Result<UnixStream> {
        loop {
            match socket::connect(&self.socket) {
                Ok(stream) => return Ok(stream),
                Err(_) => {
                    if let Some(how) = self.ended() {
                        self.note_end();
                        anyhow::bail!(
                            "the daemon could not load the workspace ({how}); the worker's log is {}",
                            self.log.display()
                        );
                    }
                    std::thread::sleep(POLL);
                }
            }
        }
    }

    /// Asks the process for `plan` of `checkout`, for the daemon's `build`, or `0` for none.
    /// The outer error is the worker's failing, the inner one its answer that it does not
    /// build this. The build is the request's own: two clients that ask at once are two
    /// builds, each of which runs a script cargo runs every build ([`BUILD`]).
    fn ask(
        &self,
        checkout: &Checkout,
        plan: &Plan,
        build: u64,
    ) -> anyhow::Result<Result<String, NotBuilt>> {
        let _answering = Answering::new(&self.answering);
        let failed = || match self.ended_within(EXITING) {
            Some(how) => {
                self.note_end();
                format!("the daemon's worker ended ({how}); its log is {}", self.log.display())
            }
            None => {
                format!("the daemon's worker did not answer; its log is {}", self.log.display())
            }
        };
        let mut stream = self.connected()?;
        // A client says all of its variables (`asking`): one it has not is not one the process
        // asking has, whatever the daemon, started by another, was started with. A request
        // that says none has the worker's own.
        let env = checkout
            .native_env
            .clone()
            .unwrap_or_else(|| self.env.clone())
            .over(checkout.env.iter().map(|(k, v)| (k.into(), v.into())));
        stream.write_all(checkout.sync_line(plan).as_bytes()).with_context(failed)?;
        stream
            .write_all(format!("{}\n", crate::process::encode_native(&env, build)).as_bytes())
            .with_context(failed)?;
        let mut reply = String::new();
        BufReader::new(stream).read_line(&mut reply).with_context(failed)?;
        let reply = reply.trim_end();
        match reply {
            _ if reply.is_empty() => Err(anyhow::anyhow!(failed())),
            _ if reply.starts_with("ok") => Ok(Ok(reply.to_owned())),
            _ => Ok(Err(NotBuilt::of(reply))),
        }
    }

    /// Tells the process that the daemon's `build` is over: every request of it has been answered
    /// by the worker, which answers once it has compiled and linked, so nothing of the build
    /// reads the output of the build scripts any more, and it is free to be written again. It is
    /// asked until the process hears it or ends: a process that is loading the workspace listens
    /// only when it has, and one that never heard would hold the build's leases until it ended.
    fn retire(&self, build: u64) {
        if build == 0 {
            return;
        }
        loop {
            if self.ended().is_some() {
                return;
            }
            if let Ok(mut stream) = socket::connect(&self.socket) {
                let _ = stream.set_read_timeout(Some(Duration::from_secs(30)));
                let _ = stream.write_all(format!("retire\t{build}\n\n").as_bytes());
                let mut reply = String::new();
                let _ = BufReader::new(stream).read_line(&mut reply);
                if reply.starts_with("ok") {
                    return;
                }
            }
            std::thread::sleep(POLL);
        }
    }

    /// Brings the programs of `checkout` up to date for the daemon's `build`, all at once: the
    /// worker builds the requests that wait for it together.
    fn sync(&self, checkout: &Checkout, build: u64) -> Reply {
        let spans = Mutex::new(Vec::new());
        let asked: Vec<anyhow::Result<Result<String, NotBuilt>>> = std::thread::scope(|scope| {
            let asking: Vec<_> = checkout
                .programs
                .iter()
                .map(|plan| {
                    scope.spawn(|| {
                        let start = Instant::now();
                        let asked = self.ask(checkout, plan, build);
                        let level = Some(crate::trace::ASKED);
                        let span =
                            crate::trace::span("daemon↔worker", level, start, Instant::now());
                        lock(&spans).push(span);
                        asked
                    })
                })
                .collect();
            let panicked = || Err(anyhow::anyhow!("asking the worker panicked"));
            asking.into_iter().map(|it| it.join().unwrap_or_else(|_| panicked())).collect()
        });
        let answered = asked
            .into_iter()
            .map(|asked| {
                let answer = asked?;
                match answer {
                    Ok(reply) => {
                        let (reply, runtime) = super::runtime_context::split_reply(&reply)?;
                        Ok((Ok(reply), runtime))
                    }
                    Err(why) => Ok((Err(why), None)),
                }
            })
            .collect::<anyhow::Result<Vec<_>>>();
        match answered {
            Ok(answers) => {
                let (programs, runtime) = answers.into_iter().unzip();
                Reply::Synced { programs, runtime, spans: spans.into_inner().unwrap_or_default() }
            }
            // The worker is gone or stuck, which says nothing of the checkout: the next
            // build that has nothing to compile has it loaded again.
            Err(why) => {
                lock(&self.checkouts).remove(&checkout.key);
                Reply::Cold { reason: format!("{why:#}") }
            }
        }
    }

    /// Whether it is loading a checkout.
    fn loading(&self) -> bool {
        lock(&self.checkouts).values().any(|it| matches!(it.warmth, Warmth::Loading(_)))
    }

    /// What it holds now, and how much it is in use.
    fn held(&self) -> Held {
        let pid = lock(&self.child).id();
        let bytes = crate::memory::held_by(pid);
        if bytes.is_none() && !self.unknown_held.swap(true, Relaxed) {
            eprintln!("worker {pid} footprint unavailable: aggregate memory usage is unknown");
        }
        Held {
            bytes: bytes.unwrap_or(0),
            known: bytes.is_some(),
            asked: *lock(&self.asked),
            busy: self.answering.load(Relaxed) != 0 || self.loads(),
        }
    }

    fn status(&self) -> WorkerStatus {
        let checkouts = lock(&self.checkouts);
        let pid = lock(&self.child).id();
        WorkerStatus {
            pid,
            project: self.project.clone(),
            log: self.log.clone(),
            bytes: crate::memory::held_by(pid),
            checkouts: checkouts
                .values()
                .map(|it| (it.root.clone(), it.warmth.describe()))
                .collect(),
        }
    }
}

/// A worker started before a build asked for one. It has the library of the toolchain
/// loaded and its names resolved, which the first build of a workspace then does not wait
/// for, and reads the build it is the worker of from its standard input.
struct Spare {
    child: Child,
    log: PathBuf,
}

impl Spare {
    /// Starts one with its files in `dir`.
    fn start(dir: &Path) -> anyhow::Result<Spare> {
        private_dir(dir)?;
        let log = dir.join("log");
        let mut command = Command::new(std::env::current_exe()?);
        clear_worker_memory(&mut command);
        command.arg("--spare").arg(dir.join("package"));
        match store() {
            Some(store) => command.env("RA_JIT_CACHE", store),
            None => command.env_remove("RA_JIT_CACHE"),
        };
        let output =
            private_file(OpenOptions::new().write(true).create(true).truncate(true), &log)?;
        command.stdin(Stdio::piped()).stdout(output.try_clone()?).stderr(output);
        own_group(&mut command);
        let child = command.spawn().context("starting a spare worker")?;
        Ok(Spare { child, log })
    }

    /// Tells it to be the worker `args` start for `checkout`. `None` when it can't be
    /// told: it has ended, or a path of the build is not text.
    /// One told a build `ahead` has what cargo has not built yet left out of `args`, and
    /// is told them whole afterwards.
    fn told(
        mut self,
        args: &[OsString],
        config: &WorkerConfig,
        cwd: &Path,
        ahead: bool,
        native_env: Option<crate::process::Native>,
        load_env: Option<crate::process::Native>,
    ) -> Option<Spare> {
        let told = || {
            let args: Option<Vec<String>> =
                args.iter().map(|it| it.to_str().map(str::to_owned)).collect();
            let told = crate::Told {
                args: args?,
                env: config.env.clone(),
                native_env,
                load_env,
                cwd: cwd.to_owned(),
                ahead,
            };
            let mut line = serde_json::to_string(&told).ok()?;
            line.push('\n');
            Some(line)
        };
        let sent = match (self.child.try_wait(), told()) {
            (Ok(None), Some(line)) => {
                self.child.stdin.as_mut().is_some_and(|it| it.write_all(line.as_bytes()).is_ok())
            }
            _ => false,
        };
        if !sent {
            self.stop();
            return None;
        }
        Some(self)
    }

    /// Passes on what cargo said of a script or a proc macro. One that has ended is found
    /// out when it is told its build.
    fn said(&mut self, line: String) {
        let Ok(mut line) = serde_json::to_string(&crate::Line::Said { said: line }) else { return };
        line.push('\n');
        if let Some(input) = self.child.stdin.as_mut() {
            let _ = input.write_all(line.as_bytes());
        }
    }

    fn stop(&mut self) {
        kill(&mut self.child);
    }
}

/// The process that loads a workspace to check it, as the worker of a build with its
/// [`WorkerConfig`] would, and says what rust-analyzer finds in its crates when asked.
struct Checker {
    /// The [`WorkerConfig::id`] it was started with.
    id: String,
    child: Child,
    socket: PathBuf,
    log: PathBuf,
    project: PathBuf,
    asked: Instant,
}

impl Checker {
    fn start(ahead: &Ahead, id: &str, dir: &Path) -> anyhow::Result<Checker> {
        private_dir(dir)?;
        let (socket, log) = (dir.join("sock"), dir.join("log"));
        let _ = std::fs::remove_file(&socket);
        let mut args: Vec<OsString> = vec!["--check".into()];
        args.extend(ahead.config.args(&ahead.root, &ahead.crates));
        args.extend(["--out".into(), "none".into(), "--socket".into(), socket.clone().into()]);
        let mut command = Command::new(std::env::current_exe()?);
        if let Some(env) = &ahead.native_env {
            env.apply(&mut command);
        }
        clear_worker_memory(&mut command);
        command.args(&args);
        match store() {
            Some(store) => command.env("RA_JIT_CACHE", store),
            None => command.env_remove("RA_JIT_CACHE"),
        };
        let output =
            private_file(OpenOptions::new().write(true).create(true).truncate(true), &log)?;
        command.envs(&ahead.config.env).env(ENDS_WITH_INPUT, "1");
        command.current_dir(&ahead.root);
        command.stdin(Stdio::piped()).stdout(output.try_clone()?).stderr(output);
        own_group(&mut command);
        let child = command.spawn().context("starting a checker")?;
        Ok(Checker {
            id: id.to_owned(),
            child,
            socket,
            log,
            project: ahead.root.join(&ahead.config.project),
            asked: Instant::now(),
        })
    }

    fn ended(&mut self) -> Option<String> {
        exited(&mut self.child)
    }

    /// Its answer: `ok`, the errors, or [`crate::check::RELOAD`]. Waits for it to have loaded
    /// the workspace and to listen.
    fn ask(&mut self) -> anyhow::Result<String> {
        let mut stream = loop {
            match socket::connect(&self.socket) {
                Ok(stream) => break stream,
                Err(_) => {
                    if let Some(how) = self.ended() {
                        anyhow::bail!(
                            "the daemon could not load the workspace to check it ({how}: {}); \
                             its log is {}",
                            last_words(&self.log),
                            self.log.display()
                        );
                    }
                    std::thread::sleep(POLL);
                }
            }
        };
        stream.write_all(b"check\n")?;
        let mut reply = String::new();
        BufReader::new(stream).read_line(&mut reply)?;
        match reply.trim_end() {
            "" => anyhow::bail!("the checker hung up; its log is {}", self.log.display()),
            reply => Ok(reply.to_owned()),
        }
    }

    fn status(&mut self) -> WorkerStatus {
        let pid = self.child.id();
        WorkerStatus {
            pid,
            project: self.project.clone(),
            log: self.log.clone(),
            bytes: crate::memory::held_by(pid),
            checkouts: vec![(self.project.clone(), "checks".to_owned())],
        }
    }

    fn stop(&mut self) {
        kill(&mut self.child);
        let _ = std::fs::remove_file(&self.socket);
    }
}

/// An unset request setting must not inherit the daemon's old budget. Spares are cleared
/// at spawn and receive the requested policy in Told before starting their monitor.
fn clear_worker_memory(command: &mut Command) {
    for name in super::cli::MEMORY_ENV {
        command.env_remove(name);
    }
}

/// Has a worker start in a process group of its own, for `kill` to end what it started with
/// it: the cargo it asks of a workspace, which would go on in the daemon's home.
fn own_group(command: &mut Command) {
    #[cfg(unix)]
    std::os::unix::process::CommandExt::process_group(command, 0);
    #[cfg(not(unix))]
    let _ = command;
}

/// Ends a worker, and on Unix the processes it started.
fn kill(child: &mut Child) {
    let pid = child.id();
    // One that has exited already has nothing to kill, and its pid may be another's by now.
    #[cfg(unix)]
    if let (Ok(None), Ok(group)) = (child.try_wait(), libc::pid_t::try_from(child.id())) {
        // SAFETY: it takes no pointer; the group is the worker's, which has not been waited for.
        unsafe { libc::killpg(group, libc::SIGKILL) };
    }
    let _ = child.kill();
    let _ = child.wait();
    if let Ok(home) = home() {
        let _ = load_cargo::proc_macro_snapshot_dir::remove_after_worker_exit(&home, pid);
    }
}

/// The daemon: its version, its workers, and when it was last asked.
struct Daemon {
    version: String,
    home: PathBuf,
    idle: Option<Duration>,
    /// Soft aggregate footprint target, in bytes.
    memory_max: Option<u64>,
    workers: Mutex<FxHashMap<String, Arc<Worker>>>,
    /// Critical-pressure evictions survive removal of a worker. Source/config/build/budget
    /// changes retry; an identical request cannot repeatedly reload the same workspace.
    pressure_stops: Mutex<FxHashMap<(String, String), PressureStop>>,
    /// The worker the next workspace is loaded by, when there is one.
    spare: Mutex<Option<Spare>>,
    spare_when: Spares,
    /// The workers told a build ahead of cargo's, by [`WorkerConfig::id`].
    ahead: Mutex<FxHashMap<String, Spare>>,
    /// How many spare workers were started: each has a directory of its own.
    spares: AtomicUsize,
    /// The checkers of the last [`Request::Check`]s, each of another build and loaded for
    /// the next check of it: a check for several targets asks one for each.
    checkers: Mutex<Vec<Checker>>,
    asked: Mutex<Instant>,
    /// How many builds clients have asked for: each is one for its worker's requests.
    builds: AtomicU64,
}

struct PressureStop {
    generation: u128,
    request: String,
    most: u64,
    at: SystemTime,
}
impl PressureStop {
    fn blocks(&self, generation: u128, request: &str, most: u64, source_changed: bool) -> bool {
        self.generation == generation
            && self.request == request
            && self.most == most
            && !source_changed
    }
}

/// Where the daemon of this user keeps its socket, its lock, its log and its workers.
pub(crate) fn home() -> anyhow::Result<PathBuf> {
    if let Some(home) = std::env::var_os("RA_JIT_HOME").filter(|it| !it.is_empty()) {
        return Ok(PathBuf::from(home));
    }
    if let Some(runtime) = std::env::var_os("XDG_RUNTIME_DIR").filter(|it| !it.is_empty()) {
        return Ok(Path::new(&runtime).join("ra-jit"));
    }
    let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"));
    Ok(Path::new(&home.context("no RA_JIT_HOME, XDG_RUNTIME_DIR or HOME")?).join(".cache/ra-jit"))
}

/// The disk store of the user's builds: the directory `RA_JIT_CACHE` names, none when it
/// says `off`, and without it `ra-jit/store` in the user's cache directory. Unlike `home`
/// it is not in `XDG_RUNTIME_DIR`, which does not outlive the session.
pub(crate) fn store() -> Option<PathBuf> {
    if let Some(named) = std::env::var_os("RA_JIT_CACHE") {
        return (!ra_jit_store::is_off(&named)).then(|| PathBuf::from(named));
    }
    let caches = std::env::var_os("XDG_CACHE_HOME").filter(|it| !it.is_empty()).map(PathBuf::from);
    let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"));
    let caches = caches.or_else(|| Some(PathBuf::from(home?).join(".cache")))?;
    Some(caches.join("ra-jit/store"))
}

pub(crate) fn socket_in(home: &Path) -> PathBuf {
    home.join("daemon.sock")
}

/// What two processes have to agree on to talk: the version of the crate and the executable
/// itself, so that a daemon of an older build of the same version is replaced too. The
/// executable is told by where it is as well as by its size and time: every build in the nix
/// store has the same time, and two builds of a small change can have the same size, but no
/// two have the same path.
pub(crate) fn version() -> String {
    let executable = std::env::current_exe().and_then(std::fs::canonicalize);
    let place: String = executable.as_ref().map_or_else(
        |_| String::new(),
        |it| {
            let digest = Sha256::digest(it.as_os_str().as_encoded_bytes());
            digest.iter().take(4).map(|it| format!("{it:02x}")).collect()
        },
    );
    let built = executable.and_then(std::fs::metadata);
    let built = built.map(|it| (it.len(), it.modified().map_or(0, stamp_of)));
    let (len, written) = built.unwrap_or_default();
    format!("{}-{place}-{len:x}-{written:x}", env!("CARGO_PKG_VERSION"))
}

/// `ra-jit daemon`: serves the user's builds until it has been idle for long enough or is
/// told to stop. It starts:
///
/// 1. the thread that accepts connections, and a thread per connection that answers its
///    one request;
/// 2. the thread that stops idle workers and ends the daemon when it is idle itself;
/// 3. per worker, the process, and a thread that waits for it to load and has it build
///    its programs once.
pub(crate) fn serve() -> anyhow::Result<()> {
    crate::memory::bound_builds()?;
    let home = home()?;
    private_dir(&home).with_context(|| format!("the daemon's directory {}", home.display()))?;
    let memory_max = memory_max().map_err(anyhow::Error::msg)?;
    let spare_when = spares().map_err(anyhow::Error::msg)?;
    let socket = socket_in(&home);
    let served = match Served::bind(&socket) {
        Ok(served) => served,
        // Another daemon is the user's: the client that started this one finds it.
        Err(NotServed::Taken) => return Ok(()),
        Err(NotServed::Failed(error)) => {
            // On one line: a worker's log is read for its last line that starts `Error`.
            anyhow::bail!("listening on {}: {error}", socket.display());
        }
    };
    let idle = match std::env::var("RA_JIT_IDLE_SECS").ok().and_then(|it| it.parse::<u64>().ok()) {
        Some(0) => None,
        Some(secs) => Some(Duration::from_secs(secs)),
        None => Some(IDLE),
    };
    let daemon = Arc::new(Daemon {
        version: version(),
        home,
        idle,
        memory_max,
        workers: Mutex::default(),
        pressure_stops: Mutex::default(),
        spare: Mutex::default(),
        spare_when,
        ahead: Mutex::default(),
        spares: AtomicUsize::new(0),
        checkers: Mutex::default(),
        asked: Mutex::new(Instant::now()),
        builds: AtomicU64::new(0),
    });
    // A build starts the daemon, and is given this one.
    daemon.spare();
    eprintln!(
        "daemon {} ({}) of pid {} listens on {}",
        daemon.version,
        crate::version(),
        std::process::id(),
        socket.display()
    );
    {
        let daemon = daemon.clone();
        std::thread::spawn(move || daemon.reap());
    }
    for (stream, open) in served.incoming() {
        let daemon = daemon.clone();
        std::thread::spawn(move || {
            let _open = open;
            if let Err(why) = daemon.answer(stream) {
                eprintln!("a request is not answered: {why:#}");
            }
        });
    }
    Ok(())
}

impl Daemon {
    /// Reads the one request of a connection and answers it.
    fn answer(&self, stream: UnixStream) -> anyhow::Result<()> {
        let began = Instant::now();
        let line = read_request(&stream)?;
        // An editor's text, on a line of its own that is not JSON: it is long and comes
        // with every key typed. The worker answers as to `cargo ra-jit build`.
        if line.starts_with("edit\t") {
            let reply = self.edit(&line).unwrap_or_else(|why| format!("error: {why:#}"));
            // An editor's client reads the errors as text.
            let reply = crate::errors::Said::text_of_reply(&reply);
            return Ok((&stream).write_all(format!("{}\n", reply.replace('\n', " ")).as_bytes())?);
        }
        let envelope = serde_json::from_str::<Envelope>(&line);
        // A request that can't be read is one of another version too, and of a client
        // that waits for an answer it can read: it gets none, and the daemon stays.
        let replaced = envelope.as_ref().is_ok_and(|it| it.replaces && it.version != self.version);
        let read = Instant::now();
        let mut reply = match envelope {
            Ok(envelope) if envelope.version == self.version => self.handle(envelope.request),
            _ => Reply::Restart { version: self.version.clone() },
        };
        if let Reply::Synced { spans, .. } = &mut reply {
            let level = Some(crate::trace::DAEMON);
            spans.push(crate::trace::span("daemon read request", level, began, read));
            spans.push(crate::trace::span("daemon", level, read, Instant::now()));
        }
        let mut text = serde_json::to_string(&reply)?;
        text.push('\n');
        let sent = (&stream).write_all(text.as_bytes());
        if replaced || matches!(reply, Reply::Stopping) {
            self.end();
        }
        Ok(sent?)
    }

    /// Hands `line`, an `edit` of a source file, to the worker that has the checkout of the
    /// file loaded, and gives back its answer.
    fn edit(&self, line: &str) -> anyhow::Result<String> {
        *lock(&self.asked) = Instant::now();
        let escaped = line["edit\t".len()..].split('\t').next().unwrap_or_default();
        let path = PathBuf::from(ra_jit_live::format::unescape(escaped));
        let path = dunce::canonicalize(&path).unwrap_or(path);
        let worker = lock(&self.workers).values().find_map(|worker| {
            let checkouts = lock(&worker.checkouts);
            let has = checkouts.values().any(|it| {
                matches!(it.warmth, Warmth::Ready)
                    && dunce::canonicalize(&it.root).is_ok_and(|root| path.starts_with(root))
            });
            has.then(|| worker.clone())
        });
        let worker = worker.with_context(|| {
            format!("the daemon has no workspace loaded that has {}", path.display())
        })?;
        *lock(&worker.asked) = Instant::now();
        let mut stream = socket::connect(&worker.socket)?;
        // The worker builds it for the process its last build was for.
        stream.write_all(format!("{line}\n\n").as_bytes())?;
        let mut reply = String::new();
        BufReader::new(stream).read_line(&mut reply)?;
        match crate::trace::without_spans(reply.trim_end()) {
            "" => anyhow::bail!("the worker hung up; its log is {}", worker.log.display()),
            reply => Ok(reply.to_owned()),
        }
    }

    fn handle(&self, request: Request) -> Reply {
        *lock(&self.asked) = Instant::now();
        match request {
            Request::Sync(checkout) => self.sync(&checkout, 0),
            Request::Warm(checkout) => match self.warm(checkout, 0) {
                Ok(()) => Reply::Warming,
                Err(why) => Reply::Cold { reason: format!("{why:#}") },
            },
            Request::Ahead(ahead) => {
                self.ahead(ahead);
                Reply::Warming
            }
            Request::Said { id, line } => {
                if let Some(spare) = lock(&self.ahead).get_mut(&id) {
                    spare.said(line);
                }
                Reply::Warming
            }
            Request::Forget(id) => {
                if let Some(mut spare) = lock(&self.ahead).remove(&id) {
                    spare.stop();
                }
                Reply::Warming
            }
            Request::Build(checkout) => {
                // The number is this request's alone, carried with it to the worker.
                let build = self.next_build();
                let reply = match self.warm(checkout.clone(), build) {
                    Ok(()) => self.built(&checkout, build),
                    Err(why) => Reply::Cold { reason: format!("{why:#}") },
                };
                // The build ends with its answer: what its scripts wrote is no longer read.
                let worker = lock(&self.workers).get(&checkout.config.id()).cloned();
                if let Some(worker) = worker {
                    // From a thread of its own, once every request of the build is answered:
                    // a worker that is slow to hear it delays no client.
                    std::thread::spawn(move || worker.retire(build));
                }
                reply
            }
            Request::Check(ahead) => self.check(&ahead),
            Request::RememberScript { dir, env } => {
                super::script::remember(dir, env);
                Reply::Warming
            }
            Request::ScriptEnvironment(dir) => {
                Reply::ScriptEnvironment(super::script::remembered(&dir))
            }
            Request::Status => Reply::Status(Status {
                pid: std::process::id(),
                version: self.version.clone(),
                idle_secs: self.idle.map(|it| it.as_secs()),
                memory_max: self.memory_max,
                workers: lock(&self.workers)
                    .values()
                    .map(|it| it.status())
                    .chain(lock(&self.checkers).iter_mut().map(Checker::status))
                    .collect(),
            }),
            Request::Stop => Reply::Stopping,
        }
    }

    fn sync(&self, checkout: &Checkout, build: u64) -> Reply {
        let worker = lock(&self.workers).get(&checkout.config.id()).cloned();
        let Some(worker) = worker else {
            return Reply::Cold { reason: "the daemon has not loaded this workspace".to_owned() };
        };
        *lock(&worker.asked) = Instant::now();
        let warmth = match lock(&worker.checkouts).get(&checkout.key) {
            Some(loaded) if loaded.generation == checkout.generation => loaded.warmth.clone(),
            _ => Warmth::Failed(
                "the daemon has not loaded this build of the workspace".to_owned(),
                SystemTime::now(),
            ),
        };
        match warmth {
            Warmth::Ready => worker.sync(checkout, build),
            Warmth::Loading(since) => Reply::Cold {
                reason: format!(
                    "the daemon is still loading the workspace ({:.0?} so far)",
                    since.elapsed()
                ),
            },
            Warmth::Failed(reason, _) => Reply::Cold { reason },
            Warmth::Broken(said, _) => {
                let programs = checkout.programs.iter().map(|_| Err(NotBuilt::Error(said.clone())));
                Reply::Synced {
                    programs: programs.collect(),
                    runtime: Vec::new(),
                    spans: Vec::new(),
                }
            }
        }
    }

    /// What the worker that loads `checkout` has to say of its programs once it has, for the
    /// daemon's `build`.
    fn built(&self, checkout: &Checkout, build: u64) -> Reply {
        loop {
            match self.sync(checkout, build) {
                Reply::Cold { .. } if self.loads(checkout) => std::thread::sleep(POLL),
                reply => return reply,
            }
        }
    }

    /// Whether a worker is loading this build of `checkout`.
    fn loads(&self, checkout: &Checkout) -> bool {
        let worker = lock(&self.workers).get(&checkout.config.id()).cloned();
        worker.is_some_and(|worker| {
            lock(&worker.checkouts).get(&checkout.key).is_some_and(|it| {
                it.generation == checkout.generation && matches!(it.warmth, Warmth::Loading(_))
            })
        })
    }

    /// The number of a build a client asks for now.
    fn next_build(&self) -> u64 {
        self.builds.fetch_add(1, Relaxed) + 1
    }

    /// Has a worker load `checkout`: the one of its configuration when that has not loaded
    /// another build of the same checkout, and a new one otherwise.
    fn warm(&self, checkout: Checkout, build: u64) -> anyhow::Result<()> {
        let id = checkout.config.id();
        let mut workers = lock(&self.workers);
        let key = (id.clone(), checkout.key.clone());
        let mut stops = lock(&self.pressure_stops);
        if stops.get(&key).is_some_and(|stop| {
            stop.blocks(
                checkout.generation,
                &request_key(&checkout),
                self.memory_max.unwrap_or(u64::MAX),
                checkout.written_since(stop.at),
            )
        }) {
            anyhow::bail!(
                "host memory pressure stopped this load; change the source, build configuration or memory budget, or restart the daemon to retry"
            );
        }
        stops.remove(&key);
        drop(stops);
        // What a worker could not load once it can't load again, until cargo builds anew or
        // a source is written: the failure may be an error in the source.
        let failed = workers.get(&id).and_then(|it| match lock(&it.checkouts).get(&checkout.key) {
            Some(Loaded { generation, warmth, .. }) if *generation == checkout.generation => {
                match warmth {
                    Warmth::Failed(why, at) => Some((Some(why.clone()), *at)),
                    // Its builds say the errors.
                    Warmth::Broken(_, at) => Some((None, *at)),
                    _ => None,
                }
            }
            _ => None,
        });
        match failed {
            Some((Some(why), at)) if !checkout.written_since(at) => anyhow::bail!(why),
            Some((None, at)) if !checkout.written_since(at) => return Ok(()),
            Some(_) => {
                if let Some(old) = workers.remove(&id) {
                    old.stop();
                }
            }
            None => {}
        }
        let running = workers.get(&id).filter(|it| it.ended().is_none()).cloned();
        if let Some(worker) = &running {
            let mut checkouts = lock(&worker.checkouts);
            match checkouts.get(&checkout.key) {
                Some(loaded) if loaded.generation == checkout.generation => return Ok(()),
                // What it linked against is not what cargo has now.
                Some(_) => {}
                None => {
                    let loading = Loaded {
                        generation: checkout.generation,
                        root: checkout.root.clone(),
                        request: request_key(&checkout),
                        warmth: Warmth::Loading(Instant::now()),
                    };
                    checkouts.insert(checkout.key.clone(), loading);
                    let worker = worker.clone();
                    std::thread::spawn(move || worker.warm(&checkout, build));
                    return Ok(());
                }
            }
        }
        if let Some(old) = workers.remove(&id) {
            old.stop();
        }
        let ahead = lock(&self.ahead).remove(&id);
        // A spare loads its library before it is told the build, which a whole build that
        // asks at once would wait for.
        let spare = match checkout.config.whole {
            true => ahead,
            false => ahead.or_else(|| lock(&self.spare).take()),
        };
        let worker =
            Arc::new(Worker::start(&checkout, self.home.join("workers").join(&id), spare, build)?);
        eprintln!(
            "worker {id} of pid {} for {}",
            lock(&worker.child).id(),
            worker.project.display()
        );
        workers.insert(id, worker.clone());
        std::thread::spawn(move || worker.warm(&checkout, build));
        Ok(())
    }

    /// Stops the workers asked longest ago while together they hold more than they may, and
    /// those nobody has asked for the idle time; and the daemon when nobody has asked it.
    fn reap(&self) {
        let mut critical_since = None;
        let grace = crate::memory::pressure_grace().unwrap_or(Duration::from_secs(60));
        loop {
            std::thread::sleep(self.idle.map_or(TICK, |idle| idle.min(TICK)));
            // A worker that ended by itself is in the daemon's log when it ends, not when a
            // build next asks for it.
            for worker in lock(&self.workers).values() {
                worker.note_end();
            }
            // The next spare starts when the worker that was one has loaded its workspace.
            if self.spare_when == Spares::Ready {
                let loading = lock(&self.workers).values().any(|it| it.loading());
                if !loading && lock(&self.ahead).is_empty() {
                    self.spare();
                }
            }
            // A program that takes edits is being edited, however long since the last save:
            // a worker started again would link it again on the next one.
            let live = crate::live::any_running();
            let persisted = pressure_persisted(
                &mut critical_since,
                hir_ty_codegen::budget::host_pressure(),
                Instant::now(),
                grace,
            );
            if !live {
                self.fit(self.memory_max.unwrap_or(0), persisted);
            }
            let Some(idle) = self.idle else { continue };
            if live {
                *lock(&self.asked) = Instant::now();
                for worker in lock(&self.workers).values() {
                    *lock(&worker.asked) = Instant::now();
                }
                continue;
            }
            // Loading is what the last build asked for: the idle time starts after it.
            for worker in lock(&self.workers).values().filter(|it| it.loading()) {
                *lock(&worker.asked) = Instant::now();
                *lock(&self.asked) = Instant::now();
            }
            if lock(&self.asked).elapsed() >= idle {
                eprintln!("idle for {idle:.0?}");
                self.end();
            }
            lock(&self.workers).retain(|_, worker| {
                // Loading can take longer than the idle time, and is what was asked for.
                let keep = worker.loads() || lock(&worker.asked).elapsed() < idle;
                if !keep {
                    worker.stop();
                }
                keep
            });
            // One that is checking holds the lock, and is not idle.
            if let Ok(mut checkers) = self.checkers.try_lock() {
                checkers.retain_mut(|it| {
                    let keep = it.asked.elapsed() < idle;
                    if !keep {
                        it.stop();
                    }
                    keep
                });
            }
        }
    }

    /// Stops the workers asked longest ago until the rest hold no more than `most` bytes. The
    /// active build governs its own pause and failure. Idle caches stopped under critical
    /// pressure leave tombstones, so the same load is not restarted until it changes.
    fn fit(&self, most: u64, critical: bool) {
        let mut workers = lock(&self.workers);
        let holding: Vec<(String, Held)> =
            workers.iter().map(|(id, worker)| (id.clone(), worker.held())).collect();
        let all: u64 = holding.iter().map(|it| it.1.bytes).sum();
        for id in over(&holding, most, critical) {
            let Some(worker) = workers.remove(&id) else { continue };
            if critical {
                let mut stops = lock(&self.pressure_stops);
                for (key, loaded) in lock(&worker.checkouts).iter() {
                    stops.insert(
                        (id.clone(), key.clone()),
                        PressureStop {
                            generation: loaded.generation,
                            request: loaded.request.clone(),
                            most: self.memory_max.unwrap_or(u64::MAX),
                            at: SystemTime::now(),
                        },
                    );
                }
            }
            let alone = holding.iter().find(|it| it.0 == id).map_or(0, |it| it.1.bytes);
            let why = match alone > most {
                true => format!(
                    "host memory pressure stayed critical after the grace period; it alone holds {} MB",
                    alone >> 20
                ),
                false => "it was asked longest ago".to_owned(),
            };
            eprintln!(
                "worker {id} of {} stopped: the workers hold {} MB, over the {} MB of \
                 RA_JIT_MEMORY_MAX, and {why}",
                worker.project.display(),
                all >> 20,
                most >> 20
            );
            worker.stop();
        }
    }

    /// Has the checker of `ahead` say what it finds in the source, started when there is
    /// none of its build or it ended, and started again when it can only take the source in
    /// by loading it again. The one asked longest ago makes way for it when there are
    /// [`CHECKERS`].
    fn check(&self, ahead: &Ahead) -> Reply {
        // Checkers have no per-request source/environment turn. A different client
        // snapshot must load a different checker, not inherit its predecessor's values.
        let id = Sha256::digest(
            serde_json::to_vec(&(&ahead.config, &ahead.native_env)).expect("checker identity"),
        )
        .iter()
        .take(8)
        .map(|it| format!("{it:02x}"))
        .collect::<String>();
        let mut held = lock(&self.checkers);
        held.retain_mut(|it| {
            let ended = it.ended().is_some();
            if ended {
                it.stop();
            }
            !ended
        });
        for _ in 0..2 {
            let at = match held.iter().position(|it| it.id == id) {
                Some(at) => at,
                None => {
                    if held.len() >= CHECKERS
                        && let Some(oldest) = (0..held.len()).min_by_key(|&at| held[at].asked)
                    {
                        held.remove(oldest).stop();
                    }
                    let dir = self.home.join("checkers").join(&id);
                    match Checker::start(ahead, &id, &dir) {
                        Ok(checker) => held.push(checker),
                        Err(why) => return Reply::Cold { reason: format!("{why:#}") },
                    }
                    held.len() - 1
                }
            };
            let checker = &mut held[at];
            checker.asked = Instant::now();
            match checker.ask() {
                Ok(reply) if reply == "ok" => return Reply::Checked { errors: None },
                Ok(reply) if reply == crate::check::RELOAD => held.remove(at).stop(),
                Ok(reply) => match NotBuilt::of(&reply) {
                    NotBuilt::Error(said) => return Reply::Checked { errors: Some(said) },
                    why => return Reply::Cold { reason: why.why() },
                },
                Err(why) => {
                    held.remove(at).stop();
                    return Reply::Cold { reason: format!("{why:#}") };
                }
            }
        }
        Reply::Cold {
            reason: "the checker loaded the workspace twice and still had to again".to_owned(),
        }
    }

    /// Has the spare worker load the workspace of a build cargo has only started.
    fn ahead(&self, ahead: Ahead) {
        let id = ahead.config.id();
        if lock(&self.workers).contains_key(&id) {
            return;
        }
        let waiting = lock(&self.spare).take();
        let Some(spare) = waiting.or_else(|| self.new_spare()) else { return };
        let mut args = ahead.config.args(&ahead.root, &ahead.crates);
        args.extend(["--out".into(), "none".into()]);
        if let Some(spare) =
            spare.told(&args, &ahead.config, &ahead.root, true, ahead.native_env.clone(), None)
            && let Some(mut other) = lock(&self.ahead).insert(id, spare)
        {
            other.stop();
        }
    }

    /// Starts a spare worker when there is none.
    fn spare(&self) {
        let mut spare = lock(&self.spare);
        if let Some(old) = spare.as_mut() {
            let Some(status) = exited(&mut old.child) else { return };
            eprintln!(
                "the spare worker of pid {} ended ({status}); its log {} ends with:\n{}",
                old.child.id(),
                old.log.display(),
                log_tail(&old.log)
            );
        }
        *spare = self.new_spare();
    }

    /// A spare worker started now, unless `RA_JIT_SPARE=0` says there are none.
    fn new_spare(&self) -> Option<Spare> {
        if self.spare_when == Spares::Never {
            return None;
        }
        let dir = self.home.join("spares").join(self.spares.fetch_add(1, Relaxed).to_string());
        Spare::start(&dir).inspect_err(|why| eprintln!("no spare worker: {why:#}")).ok()
    }

    /// Stops the workers and the daemon. A client told it ends waits for its connection to
    /// close, which it does when this process exits.
    fn end(&self) -> ! {
        // Held until the process exits, so that no other thread starts a worker or a spare once
        // these are stopped: one started then would write in the home and outlive the daemon.
        // In the order a worker that loads a workspace takes them.
        let workers = lock(&self.workers);
        let mut ahead = lock(&self.ahead);
        let mut spare = lock(&self.spare);
        if let Some(spare) = spare.as_mut() {
            spare.stop();
        }
        for checker in lock(&self.checkers).iter_mut() {
            checker.stop();
        }
        for spare in ahead.values_mut() {
            spare.stop();
        }
        for worker in workers.values() {
            worker.let_go();
        }
        for worker in workers.values() {
            worker.finish();
        }
        let _ = std::fs::remove_file(socket_in(&self.home));
        eprintln!("daemon of pid {} ends at {:?}", std::process::id(), SystemTime::now());
        std::process::exit(0)
    }
}

/// The exit status of a process that has ended, or why it can't be told.
fn exited(child: &mut Child) -> Option<String> {
    match child.try_wait() {
        Ok(None) => None,
        Ok(Some(status)) => Some(status.to_string()),
        Err(error) => Some(format!("not known: {error}")),
    }
}

/// The line of a worker's log that says what went wrong: what it panicked with, else the
/// last error, else its last line.
fn last_words(log: &Path) -> String {
    let log = std::fs::read_to_string(log).unwrap_or_default();
    let lines: Vec<&str> = log.lines().collect();
    if let Some(at) = lines.iter().rposition(|it| it.contains(" panicked at ")) {
        // `thread '…' panicked at <place>:`, and the message on the line after it.
        let place = lines[at].split_once(" panicked at ").map_or(lines[at], |it| it.1);
        let message = lines.get(at + 1).copied().unwrap_or_default();
        return format!("panicked at {place} {message}");
    }
    let last = lines.iter().rev().find(|it| it.starts_with("Error"));
    last.or(lines.last()).copied().unwrap_or("no output").to_owned()
}

/// The end of a worker's log, for the daemon's: where it panicked and with what, then its
/// last lines without the frames of a backtrace, each indented.
fn log_tail(log: &Path) -> String {
    let text = std::fs::read_to_string(log).unwrap_or_default();
    let lines: Vec<&str> = text.lines().collect();
    let frame = |line: &str| {
        let line = line.trim_start();
        let numbered = line.split_once(':').is_some_and(|(number, _)| {
            !number.is_empty() && number.bytes().all(|it| it.is_ascii_digit())
        });
        numbered || line.starts_with("at ") || line.to_lowercase().starts_with("stack backtrace")
    };
    let mut kept: Vec<&str> = match lines.iter().rposition(|it| it.contains(" panicked at ")) {
        Some(at) => lines[at..].iter().take(2).copied().collect(),
        None => Vec::new(),
    };
    let mut last: Vec<&str> =
        lines.iter().rev().copied().filter(|it| !frame(it)).take(LAST_LINES).collect();
    last.reverse();
    for line in last {
        if !kept.contains(&line) {
            kept.push(line);
        }
    }
    if kept.is_empty() {
        return "  (nothing)".to_owned();
    }
    kept.iter().map(|it| format!("  {it}")).collect::<Vec<_>>().join("\n")
}

/// The lock, also after a thread panicked with it: what it guards is whole between any two
/// statements here.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_workers_asked_longest_ago_are_stopped_for_the_rest_to_fit() {
        let now = Instant::now();
        let held = |bytes: u64, ago: u64, busy: bool| Held {
            bytes,
            known: true,
            asked: now - Duration::from_secs(ago),
            busy,
        };
        let holding = vec![
            ("oldest".to_owned(), held(4, 30, false)),
            ("old".to_owned(), held(4, 20, false)),
            ("answering".to_owned(), held(4, 40, true)),
            ("last".to_owned(), held(4, 0, false)),
        ];
        assert_eq!(over(&holding, 16, false), Vec::<String>::new());
        assert_eq!(over(&holding, 3, false), Vec::<String>::new());
        assert_eq!(over(&holding, 12, true), ["oldest"]);
        assert_eq!(over(&holding, 8, true), ["oldest", "old"]);
        // The busy one and the one asked last stay while neither alone holds more than all may.
        assert_eq!(over(&holding, 4, true), ["oldest", "old"]);
        assert_eq!(over(&holding, 3, true), ["oldest", "old", "last"]);
    }

    #[test]
    fn live_program_recovery_and_unknown_pressure_reset_the_eviction_grace() {
        use hir_ty_codegen::budget::Pressure::{Critical, Normal, Warning};
        let start = Instant::now();
        let grace = Duration::from_secs(60);
        for recovered in [Some(Normal), Some(Warning), None] {
            let mut since = None;
            let mut tick = |seconds, pressure| {
                pressure_persisted(
                    &mut since,
                    pressure,
                    start + Duration::from_secs(seconds),
                    grace,
                )
            };
            assert!(!tick(0, Some(Critical)));
            // The sampler still updates the window while the live program prevents eviction.
            assert!(!tick(30, recovered));
            assert!(!tick(120, Some(Critical)));
            assert!(!tick(179, Some(Critical)));
            assert!(tick(180, Some(Critical)));
        }
    }

    #[test]
    fn a_lone_worker_keeps_its_cache_past_the_soft_target() {
        let holding = [(
            "loading".into(),
            Held { bytes: 30, known: true, asked: Instant::now(), busy: false },
        )];
        assert!(over(&holding, 16, false).is_empty());
        assert_eq!(over(&holding, 16, true), ["loading"]);
    }

    #[test]
    fn workers_clear_inherited_memory_policy_before_applying_the_request() {
        let mut command = Command::new("unused-worker");
        command.env("RA_JIT_WORKER_MEMORY", "old-budget");
        clear_worker_memory(&mut command);
        for name in super::super::cli::MEMORY_ENV {
            assert!(command.get_envs().any(|(key, value)| key == *name && value.is_none()));
        }
        command.env("RA_JIT_WORKER_MEMORY", "32G");
        assert!(command.get_envs().any(|(key, value)| key == "RA_JIT_WORKER_MEMORY"
            && value == Some(std::ffi::OsStr::new("32G"))));
    }

    #[test]
    fn pressure_request_identity_ignores_the_invocation_clock_but_keeps_load_inputs() {
        let mut checkout: Checkout = serde_json::from_value(serde_json::json!({
            "key": "checkout", "generation": 42, "built_at": 100,
            "root": "/workspace", "programs": [], "sources": [], "env": {},
            "config": {
                "repository": "/repository", "project": "", "cfg_test": false, "env": {},
                "selection": { "profile": "dev", "features": [], "no_default_features": false,
                    "all_features": false, "packages": [], "workspace": false,
                    "target": null, "dev_deps": false }
            }
        }))
        .unwrap();
        let original = request_key(&checkout);
        checkout.built_at += 1;
        assert_eq!(request_key(&checkout), original);
        checkout.env.insert("INPUT".into(), "changed".into());
        assert_ne!(request_key(&checkout), original);
        checkout.env.clear();
        checkout.config.env.insert("RA_JIT_WORKER_MEMORY".into(), "32G".into());
        assert_ne!(request_key(&checkout), original);
    }

    #[test]
    fn pressure_stop_retries_only_changed_source_build_or_budget() {
        let stop = PressureStop {
            generation: 42,
            request: "request".into(),
            most: 16,
            at: SystemTime::now(),
        };
        assert!(stop.blocks(42, "request", 16, false));
        assert!(stop.blocks(42, "request", 16, false));
        assert!(!stop.blocks(43, "request", 16, false));
        assert!(!stop.blocks(42, "changed request", 16, false));
        assert!(!stop.blocks(42, "request", 32, false));
        assert!(!stop.blocks(42, "request", 16, true));
    }

    #[test]
    fn busy_worker_owns_its_pause_and_failure_while_idle_cache_can_be_reclaimed() {
        let now = Instant::now();
        let loading = Held { bytes: 30, known: true, asked: now, busy: true };
        assert!(over(&[("loading".to_owned(), loading)], 16, true).is_empty());
        let idle = Held { bytes: 30, known: true, asked: now, busy: false };
        assert_eq!(over(&[("idle".to_owned(), idle)], 16, true), ["idle"]);
        let under = Held { bytes: 16, known: true, asked: now, busy: true };
        assert_eq!(over(&[("loading".to_owned(), under)], 16, true), Vec::<String>::new());
    }

    #[cfg(unix)]
    #[test]
    fn pressure_eviction_preserves_unknown_workers_and_retries_changed_inputs() {
        use hir_ty_codegen::budget::Pressure::{Critical, Normal};
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("lib.rs");
        std::fs::write(&source, "pub fn value() -> u32 { 1 }\n").unwrap();
        let checkout: Checkout = serde_json::from_value(serde_json::json!({
            "key": "checkout", "generation": 42, "built_at": 100,
            "root": dir.path(), "programs": [], "sources": [source], "env": {},
            "config": {
                "repository": dir.path(), "project": "", "cfg_test": false, "env": {},
                "selection": { "profile": "dev", "features": [], "no_default_features": false,
                    "all_features": false, "packages": [], "workspace": false,
                    "target": null, "dev_deps": false }
            }
        }))
        .unwrap();
        let worker = Arc::new(Worker {
            project: checkout.project(),
            socket: dir.path().join("unused.sock"),
            log: dir.path().join("log"),
            env: crate::process::Native::own(),
            child: Mutex::new(Command::new("sleep").arg("30").spawn().unwrap()),
            checkouts: Mutex::new(FxHashMap::from_iter([(
                checkout.key.clone(),
                Loaded {
                    generation: checkout.generation,
                    root: checkout.root.clone(),
                    request: request_key(&checkout),
                    warmth: Warmth::Ready,
                },
            )])),
            asked: Mutex::new(Instant::now()),
            answering: AtomicUsize::new(0),
            noted: AtomicBool::new(true),
            unknown_held: AtomicBool::new(false),
        });
        let id = checkout.config.id();
        let daemon = Daemon {
            version: "test".into(),
            home: dir.path().into(),
            idle: None,
            memory_max: Some(1),
            workers: Mutex::new(FxHashMap::from_iter([(id.clone(), worker.clone())])),
            pressure_stops: Mutex::default(),
            spare: Mutex::default(),
            spare_when: Spares::Never,
            ahead: Mutex::default(),
            spares: AtomicUsize::new(0),
            checkers: Mutex::default(),
            asked: Mutex::new(Instant::now()),
            builds: AtomicU64::new(0),
        };
        let now = Instant::now();
        let grace = Duration::from_secs(60);
        let mut since = None;
        assert!(!pressure_persisted(&mut since, Some(Critical), now, grace));
        assert!(!pressure_persisted(&mut since, None, now + grace, grace));
        assert!(!pressure_persisted(&mut since, Some(Critical), now + grace * 2, grace));
        daemon.fit(1, false);
        assert!(lock(&daemon.workers).contains_key(&id), "soft target retains the cache");
        assert!(pressure_persisted(&mut since, Some(Critical), now + grace * 3, grace));
        daemon.fit(1, true);
        assert!(!lock(&daemon.workers).contains_key(&id));
        let stopped = worker.held();
        assert!(!stopped.known);
        assert!(worker.unknown_held.load(Relaxed), "unreadable footprint is observable");
        assert!(over(&[(id.clone(), stopped)], 0, true).is_empty());
        let retry = daemon.warm(checkout.clone(), 0).unwrap_err().to_string();
        assert!(retry.contains("host memory pressure stopped this load"), "{retry}");
        let mut changed = checkout.clone();
        changed.env.insert("INPUT".into(), "changed".into());
        let retry = daemon.warm(changed, 0).unwrap_err().to_string();
        assert!(!retry.contains("host memory pressure stopped this load"), "{retry}");
        assert!(lock(&daemon.pressure_stops).is_empty(), "changed request clears tombstone");
        let stop = PressureStop {
            generation: 42,
            request: request_key(&checkout),
            most: 1,
            at: SystemTime::UNIX_EPOCH,
        };
        assert!(!stop.blocks(42, &request_key(&checkout), 1, checkout.written_since(stop.at)));
        // Exercise warm's source invalidation, not only the blocks predicate. No real
        // compiler is launched: this checkout deliberately has no program, so passing
        // the tombstone reaches the normal request-validation error.
        lock(&daemon.pressure_stops).insert((id.clone(), checkout.key.clone()), stop);
        let retry = daemon.warm(checkout.clone(), 0).unwrap_err().to_string();
        assert!(!retry.contains("host memory pressure stopped this load"), "{retry}");
        assert!(lock(&daemon.pressure_stops).is_empty());
        let mut config = checkout.config.clone();
        config.env.insert("RA_JIT_WORKER_MEMORY".into(), "off".into());
        assert_ne!(config.id(), id);
        lock(&daemon.pressure_stops).insert(
            (id.clone(), checkout.key.clone()),
            PressureStop {
                generation: 42,
                request: request_key(&checkout),
                most: 1,
                at: SystemTime::now(),
            },
        );
        let mut changed = checkout.clone();
        changed.config = config;
        let retry = daemon.warm(changed, 0).unwrap_err().to_string();
        assert!(!retry.contains("host memory pressure stopped this load"), "{retry}");
        assert_eq!(lock(&daemon.pressure_stops).len(), 1, "other identity's tombstone stays");
        assert!(!pressure_persisted(&mut since, Some(Normal), now + grace * 4, grace));
        assert!(!pressure_persisted(&mut since, Some(Critical), now + grace * 5, grace));
        assert!(!pressure_persisted(
            &mut since,
            Some(Critical),
            now + grace * 6 - Duration::from_millis(1),
            grace
        ));
    }

    /// Two clients that ask one worker at once are two builds, and each request says its own
    /// (p05w-verify, finding 1): a worker that kept the number of the last build asked told
    /// both requests the second's, and a script cargo runs every build ran once for the two.
    #[cfg(unix)]
    #[test]
    fn requests_that_ask_at_once_each_say_their_own_build() {
        use std::os::unix::ffi::OsStringExt as _;
        use std::os::unix::net::UnixListener;
        let dir = std::env::temp_dir().join(format!("ra-jit-daemon-builds-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let socket = dir.join("sock");
        let listener = UnixListener::bind(&socket).unwrap();
        // A worker that answers each request once both have come, with the build it said.
        let said = std::thread::spawn(move || {
            let mut streams = Vec::new();
            let mut builds = Vec::new();
            for stream in listener.incoming().take(2) {
                let stream = stream.unwrap();
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let (mut sync, mut env) = (String::new(), String::new());
                reader.read_line(&mut sync).unwrap();
                reader.read_line(&mut env).unwrap();
                let (env, build) = crate::process::decode_native(env.trim_end()).unwrap();
                let env = env.vars().collect::<BTreeMap<_, _>>();
                assert_eq!(
                    env[std::ffi::OsStr::new("BYTE_VALUE")],
                    OsString::from_vec(vec![0xff, build as u8])
                );
                assert!(!env.contains_key(std::ffi::OsStr::new("DAEMON_ONLY")));
                builds.push(build);
                streams.push(stream);
            }
            for mut stream in streams {
                stream.write_all(b"ok\n").unwrap();
            }
            builds.sort_unstable();
            builds
        });
        let child = Command::new("sleep").arg("30").spawn().unwrap();
        let worker = Worker {
            project: dir.clone(),
            socket,
            log: dir.join("log"),
            env: crate::process::Native::of([("DAEMON_ONLY".into(), "not-the-client".into())]),
            child: Mutex::new(child),
            checkouts: Mutex::new(FxHashMap::default()),
            asked: Mutex::new(Instant::now()),
            answering: AtomicUsize::new(0),
            noted: AtomicBool::new(false),
            unknown_held: AtomicBool::new(false),
        };
        let plan = Plan {
            crates: vec![dir.join("src/main.rs")],
            object: dir.join("probe.o"),
            patch: dir.join("probe.patch.o"),
            binary: dir.join("probe"),
            rustc_binary: None,
            link_cmd: String::new(),
            symbol_files: Vec::new(),
            unit_test: false,
            linked_as_built: Vec::new(),
            copies: Vec::new(),
            package: None,
            stamp: 0,
            whole: true,
        };
        let checkout = Checkout {
            key: "probe-whole".to_owned(),
            generation: 0,
            built_at: 0,
            config: WorkerConfig {
                repository: dir.clone(),
                project: PathBuf::new(),
                cfg_test: false,
                selection: crate::plan::Selection::default(),
                env: BTreeMap::new(),
                whole: true,
            },
            root: dir.clone(),
            programs: vec![plan.clone()],
            sources: Vec::new(),
            env: BTreeMap::from([("A".to_owned(), "1".to_owned())]),
            native_env: None,
            scripts: None,
        };
        let checkout_for = |build| {
            let native_env = Some(crate::process::Native::of([
                ("BYTE_VALUE".into(), OsString::from_vec(vec![0xff, build])),
                ("EMPTY".into(), OsString::new()),
            ]));
            let checkout = Checkout { native_env, ..checkout.clone() };
            let wire = serde_json::to_string(&Envelope {
                version: "test".into(),
                replaces: false,
                request: Request::Build(checkout),
            })
            .unwrap();
            let Envelope { request: Request::Build(checkout), .. } =
                serde_json::from_str(&wire).unwrap()
            else {
                panic!("build envelope")
            };
            checkout
        };
        let first_checkout = checkout_for(5);
        let second_checkout = checkout_for(6);
        let answers = std::thread::scope(|scope| {
            let first = scope.spawn(|| worker.ask(&first_checkout, &plan, 5));
            let second = scope.spawn(|| worker.ask(&second_checkout, &plan, 6));
            [first.join().unwrap().unwrap(), second.join().unwrap().unwrap()]
        });
        assert!(answers.iter().all(|it| it.as_ref().is_ok_and(|it| it == "ok")));
        assert_eq!(said.join().unwrap(), [5, 6]);
        let _ = lock(&worker.child).kill();
        let _ = std::fs::remove_dir_all(&dir);
    }
}
