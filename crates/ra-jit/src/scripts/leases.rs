//! What a build holds of the scripts it judged, for as long as the build lasts.
//!
//! A script's `OUT_DIR` is persistent and shared: cargo runs a script again in the same
//! directory, the script may read what its last run left, and its path is in what the script
//! prints and in what it generates. So a run cannot be given a directory of its own to keep
//! another build's output apart, and a copy of the output at the path of another would not be the
//! output the script described. What cargo does is hold the build directory's lock for the whole
//! build. A worker builds in requests, several to a build, over minutes, and other workers share
//! the directory, so two things are held instead:
//!
//! - **A lease on each script's run directory** (`flock` on `blaze-lease` in it), shared by the
//!   builds that read the output and exclusive for the one that writes it. A build takes it when
//!   it judges the script and keeps it until it is retired.
//! - **The admission**, which is the transaction lock of the target's script directory
//!   (`.lock`), owned by a *build* and not by a call. A build needs it for as long as it judges,
//!   compiles or runs a script, and keeps it across the times it is put off, so that a build that
//!   waits for readers is the only build that is mid-way through the scripts.
//!
//! That is what makes the waits sound, and the rules are these:
//!
//! 1. A build takes the admission before it touches any script it has no judgment of, and so before
//!    it takes any lease. A build that cannot take it holds nothing and waits for it.
//! 2. A build that holds the admission may wait, for the readers of a script it must run to be
//!    retired. Nothing it waits for waits for it: a reader is a build that has judged all its
//!    scripts, and its later requests are answered from what it kept (judgments are immutable
//!    results, by (build, run directory)) and need no admission and no lease.
//! 3. A build that holds leases and is refused the admission or a lease it needs (a script
//!    group it did not judge before, a script a reader of another build holds) **yields**: it lets
//!    go of what it holds that no consumer of its is reading, and forgets what it judged, and then
//!    waits holding nothing. If one of its consumers is reading, it lets go when the last ends
//!    ([`Pin`]). So nothing waits while it holds what no one is reading, and a wait is never
//!    in a cycle: the leases a waiter holds protect a consumer that never waits.
//! 4. The admission is let go when an attempt ends in anything but a wait, and when the build is
//!    retired. The builds put off are asked again then. A build that *ran* a script is the exception:
//!    the output is the one run of it the build has, and no other build may be admitted to run the
//!    script over it and force the lease to be given up, so the admission is its until it is
//!    retired (it never yields, and no one waits for what it holds but for its end). A build that
//!    only found the output fresh yields, and judges the script afresh, which is its first run.
//!
//! What a consumer reads is a **world**: the run directories of the scripts of the checkouts the
//! database was loaded from, each with the token of the record the run left
//! (`blaze-run.json`) when the build judged it. A consumer takes its [`Pin`] on the world of
//! the database it reads, before its first read: shared leases on all of it, and the tokens are
//! looked at again, so that a database loaded from one output is never read together with
//! another's. A world that moved is not read: the request is answered that it did, and the
//! next judges again.
//!
//! Waits are not here: [`Busy`] is the answer, and the caller puts the request off. The worker
//! asks again when [`generation`] has moved since it began: a retirement, an admission let go and
//! a [`watch`]ed file of another process that was freed all move it, so that none of them is
//! lost when it comes while the request is on its way to the queue.
//!
//! Every consumer of a script's output has an owner: a numbered build, the request's own
//! anonymous number ([`anonymous`], released when the request is dropped), or the load of a
//! process that has no daemon (its first program is built under it; a checkout joined later is
//! loaded for the request that asked). A request that names no build (`build == 0` below)
//! keeps nothing.

use std::{
    collections::{HashMap, HashSet},
    fs::File,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use super::Done;

/// The file in a script's run directory that its leases are on.
const FILE: &str = "blaze-lease";

/// The admission: the transaction lock of the script directory.
pub(super) const ADMISSION: &str = ".lock";

/// Why a script cannot be run, or read, now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Busy {
    /// Another build of this process holds `path` (a lease, or the admission).
    Process { path: PathBuf, build: u64 },
    /// A build of another process does.
    Elsewhere { path: PathBuf },
}

impl Busy {
    /// The file that is held.
    pub(crate) fn path(&self) -> &Path {
        match self {
            Busy::Process { path, .. } | Busy::Elsewhere { path } => path,
        }
    }
}

impl std::fmt::Display for Busy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Busy::Process { path, build } => write!(
                f,
                "{} is in use by build {build} of this worker, which builds the scripts",
                path.display()
            ),
            Busy::Elsewhere { path } => {
                write!(f, "{} is in use by another worker", path.display())
            }
        }
    }
}

impl std::error::Error for Busy {}

/// Why a lease was not given.
#[derive(Debug)]
pub(super) enum Refused {
    Busy(Busy),
    /// An error, or a build that cannot wait for the admission without holding what it waits for.
    Failed(String),
}

impl From<Busy> for Refused {
    fn from(busy: Busy) -> Refused {
        Refused::Busy(busy)
    }
}

/// How often something that may be waited for has moved: a retirement, an admission let go, or
/// a file of another process freed.
static GENERATION: AtomicU64 = AtomicU64::new(0);

/// What to call when it moves: the worker's main loop is woken.
static WAKER: OnceLock<Box<dyn Fn() + Send + Sync>> = OnceLock::new();

pub(crate) fn generation() -> u64 {
    GENERATION.load(Ordering::SeqCst)
}

/// Has the worker told when something it was put off for may have moved.
pub(crate) fn set_waker(wake: impl Fn() + Send + Sync + 'static) {
    let _ = WAKER.set(Box::new(wake));
}

/// Something that may be waited for has moved, as far as the caller says: the worker's main loop
/// is woken. For an end that no release of a lease makes (a recovery whose last request is gone).
pub(crate) fn wake() {
    moved();
}

fn moved() {
    GENERATION.fetch_add(1, Ordering::SeqCst);
    if let Some(wake) = WAKER.get() {
        wake();
    }
}

/// A number for a consumer that no build of the daemon's owns.
pub(crate) fn anonymous() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(1 << 62);
    NEXT.fetch_add(1, Ordering::SeqCst)
}

/// Moves [`generation`] when `path`, held by another process, is free to be taken: a thread
/// of its own polls it, so that the worker's main loop does not. It is for `build` and ends
/// when no build that asked is left, however long the file stays held. A watch is a
/// registration with an identity of its own: the thread ends its own and no other, and does
/// so under the lock that registrations are made under, so that a build that asks as the last
/// one's watch ends gets a watch of its own.
pub(crate) fn watch(path: &Path, build: u64) {
    if let Some(id) = register(path, build) {
        let path = path.to_owned();
        std::thread::spawn(move || watching(&path, id));
    }
}

/// Registers `build` as waiting for `path`. A watch whose thread has to be started is `Some`.
fn register(path: &Path, build: u64) -> Option<u64> {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    let mut map = interest();
    let map = map.get_or_insert_with(HashMap::default);
    match map.get_mut(path) {
        Some(registration) => {
            registration.owners.insert(build);
            None
        }
        None => {
            let id = NEXT.fetch_add(1, Ordering::SeqCst);
            map.insert(path.to_owned(), Registration { id, owners: HashSet::from([build]) });
            Some(id)
        }
    }
}

/// Whether the watch `id` of `path` ends: when the file is free (`freed`), or when no build is
/// left that asked. Decided, and its entry removed, under the lock that registrations are
/// made under, and only if the entry is this watch's: a build that registered since keeps
/// the watch it joined, and one that registered after the entry was removed has a new one.
fn ends(path: &Path, id: u64, freed: bool) -> bool {
    let mut map = interest();
    let map = map.get_or_insert_with(HashMap::default);
    let Some(registration) = map.get(path).filter(|it| it.id == id) else { return true };
    if !freed && !registration.owners.is_empty() {
        return false;
    }
    map.remove(path);
    true
}

/// The longest a watch waits between two looks at its file, in microseconds: a quarter of a second,
/// less in the tests that put its decisions under every scheduling there is.
static WATCH_PAUSE_MAX: AtomicU64 = AtomicU64::new(250_000);

/// The thread of the watch `id` of `path`.
fn watching(path: &Path, id: u64) {
    let Ok(file) = open(path) else {
        ends(path, id, false);
        return;
    };
    let most = || Duration::from_micros(WATCH_PAUSE_MAX.load(Ordering::SeqCst));
    let mut pause = Duration::from_millis(10).min(most());
    loop {
        match file.try_lock() {
            Ok(()) => {
                let _ = file.unlock();
                ends(path, id, true);
                moved();
                return;
            }
            Err(std::fs::TryLockError::WouldBlock) => {}
            Err(std::fs::TryLockError::Error(_)) => {
                ends(path, id, false);
                return;
            }
        }
        if ends(path, id, false) {
            return;
        }
        std::thread::sleep(pause);
        pause = (pause * 2).min(most());
    }
}

/// Waits until no process holds `path`, for at most `within` if that is said; none, without
/// end, as cargo waits for the build directory. A message every half minute says what is waited
/// for. It takes the file and gives it back: the attempt that follows takes it again.
pub(super) fn wait(path: &Path, within: Option<Duration>) -> Result<(), String> {
    let file = open(path).map_err(|it| match it {
        Refused::Failed(why) => why,
        Refused::Busy(busy) => busy.to_string(),
    })?;
    let began = Instant::now();
    let mut said = Duration::ZERO;
    let mut pause = Duration::from_millis(10);
    loop {
        match file.try_lock() {
            Ok(()) => {
                let _ = file.unlock();
                return Ok(());
            }
            Err(std::fs::TryLockError::WouldBlock) => {}
            Err(std::fs::TryLockError::Error(err)) => return Err(err.to_string()),
        }
        let waited = began.elapsed();
        if within.is_some_and(|it| waited >= it) {
            return Err(format!("{} stayed in use for {waited:?}", path.display()));
        }
        if waited >= said + Duration::from_secs(30) {
            said = waited;
            super::barrier::event("waits", 0, &format!("still {} {waited:.0?}", path.display()));
        }
        std::thread::sleep(pause);
        pause = (pause * 2).min(Duration::from_millis(250));
    }
}

/// What a build decided of a script.
pub(super) enum Judged {
    /// It ran, or was found fresh, and the build holds the output.
    Standing(Arc<Done>),
    /// The same, but the build gave the lease up to wait (it yielded).
    Released(Arc<Done>),
    /// It failed, with this message.
    Failed(String),
}

/// The leases a process holds, and what its builds judged.
#[derive(Default)]
pub(super) struct Leases {
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    /// What each build judged of each run directory: the result it keeps, or the message of
    /// the failure it keeps. Results are not changed once they are made.
    judged: HashMap<(u64, PathBuf), Result<Arc<Done>, String>>,
    /// The shared lease each build holds on a run directory whose script stands.
    held: HashMap<(u64, PathBuf), File>,
    /// The admissions, by the file: the build that has it and the lock.
    admission: HashMap<PathBuf, (u64, File)>,
    /// How many consumers of each build are reading its output now ([`Pin`]s).
    consumers: HashMap<u64, usize>,
    /// The builds that yielded while a consumer of theirs was reading: they let go when it ends.
    yielding: HashSet<u64>,
    /// How many operations of each build are under way: a judging of the scripts of all the
    /// checkouts it reads, which has the admission for as long as it lasts.
    operations: HashMap<u64, usize>,
    /// The builds that ran a script, and so wrote an output that stands only while they hold
    /// it: each has the admission until it is retired.
    executors: HashSet<u64>,
    /// The leases each build holds on the producer images it was given, which its macros are
    /// loaded from after its scripts are judged: none is collected until it is retired. A
    /// request that names no build has none here: the load that serves it keeps them until
    /// its proc-macro client has loaded the images ([`super::Images`]).
    images: HashMap<u64, super::Images>,
}

/// A watch of a file of another process: which one it is (a thread ends only the registration
/// it was made for), and the builds that wait for the file.
struct Registration {
    id: u64,
    owners: HashSet<u64>,
}

/// The files of other processes that builds wait for.
static INTEREST: Mutex<Option<HashMap<PathBuf, Registration>>> = Mutex::new(None);

fn interest() -> std::sync::MutexGuard<'static, Option<HashMap<PathBuf, Registration>>> {
    INTEREST.lock().unwrap_or_else(|it| it.into_inner())
}

impl Leases {
    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|it| it.into_inner())
    }

    /// What `build` judged of `run_dir` already.
    pub(super) fn judged(&self, build: u64, run_dir: &Path) -> Option<Result<Arc<Done>, String>> {
        if build == 0 {
            return None;
        }
        self.state().judged.get(&(build, run_dir.to_owned())).cloned()
    }

    /// What `build` decided of the script in `run_dir`, and whether it holds the output that
    /// decision is of. A failure is the build's for good; a result whose lease the build gave up
    /// (it yielded) is only as good as the output is: [`Leases::reacquire`] looks.
    pub(super) fn decision(&self, build: u64, run_dir: &Path) -> Option<Judged> {
        if build == 0 {
            return None;
        }
        let state = self.state();
        let key = (build, run_dir.to_owned());
        match state.judged.get(&key)? {
            Err(why) => Some(Judged::Failed(why.clone())),
            Ok(done) if state.held.contains_key(&key) => Some(Judged::Standing(done.clone())),
            Ok(done) => Some(Judged::Released(done.clone())),
        }
    }

    /// Has `build` hold the output of a result it gave up the lease of, if the output is still
    /// the run `generation` made. `false` when another build ran the script since, for a result
    /// of an output that build only found fresh (it ran nothing of it: judging it again is the
    /// first run it makes, if the script is to run); an error when a script is running, and
    /// when the output of a script *this build ran* is not the run it was. A build that ran a
    /// script keeps the admission and the lease until it is retired, so no other build can have
    /// written it, and a second run of the script in the build is never the way out.
    pub(super) fn reacquire(
        &self,
        build: u64,
        run_dir: &Path,
        generation: u64,
        ran: bool,
    ) -> Result<bool, Refused> {
        let path = run_dir.join(FILE);
        let file = open(&path)?;
        match file.try_lock_shared() {
            Ok(()) => {}
            Err(std::fs::TryLockError::WouldBlock) => return Err(Busy::Elsewhere { path }.into()),
            Err(std::fs::TryLockError::Error(err)) => return Err(Refused::Failed(err.to_string())),
        }
        let mut state = self.state();
        let key = (build, run_dir.to_owned());
        if super::generation_of(run_dir) == Some(generation) {
            if let Some(old) = state.held.insert(key, file) {
                let _ = old.unlock();
            }
            return Ok(true);
        }
        let _ = file.unlock();
        if ran {
            return Err(Refused::Failed(format!(
                "the output of the build script in {} was written again while the build that ran                  it was not over",
                run_dir.display()
            )));
        }
        state.judged.remove(&key);
        Ok(false)
    }

    /// Whether an operation of `build` is under way.
    pub(super) fn in_operation(&self, build: u64) -> bool {
        self.state().operations.get(&build).is_some_and(|it| *it > 0)
    }

    /// Begins an operation of `build`: the admission it takes is kept until it ends.
    pub(super) fn begin_operation(&self, build: u64) {
        if build != 0 {
            *self.state().operations.entry(build).or_default() += 1;
        }
    }

    /// Ends an operation of `build`. The admission goes with the last, unless the operation
    /// waits (`waiting`) and is asked again, which has it.
    pub(super) fn end_operation(&self, build: u64, waiting: bool) {
        if build == 0 {
            return;
        }
        let last = {
            let mut state = self.state();
            let Some(count) = state.operations.get_mut(&build) else { return };
            *count -= 1;
            let last = *count == 0;
            if last {
                state.operations.remove(&build);
            }
            last
        };
        if last && !waiting {
            self.release_admission(build);
        }
    }

    /// The admission of the script directory `base` for `build`. It is kept until the attempt
    /// ends in anything but a wait.
    pub(super) fn admit(&self, build: u64, base: &Path) -> Result<(), Refused> {
        if build == 0 {
            return Ok(());
        }
        let path = base.join(ADMISSION);
        let mut state = self.state();
        match state.admission.get(&path) {
            Some((owner, _)) if *owner == build => return Ok(()),
            Some((owner, _)) => {
                return Err(Busy::Process { path, build: *owner }.into());
            }
            None => {}
        }
        let file = open(&path)?;
        match file.try_lock() {
            Ok(()) => {
                state.yielding.remove(&build);
                state.admission.insert(path, (build, file));
                Ok(())
            }
            Err(std::fs::TryLockError::WouldBlock) => Err(Busy::Elsewhere { path }.into()),
            Err(std::fs::TryLockError::Error(err)) => Err(Refused::Failed(err.to_string())),
        }
    }

    /// Lets go of the admission `build` has, if it has one. A build that ran a script keeps it
    /// until it is retired ([`Leases::retire`]).
    pub(super) fn release_admission(&self, build: u64) {
        let mut state = self.state();
        if state.executors.contains(&build) {
            return;
        }
        let ours: Vec<_> =
            state.admission.iter().filter(|it| it.1.0 == build).map(|it| it.0.clone()).collect();
        if ours.is_empty() {
            return;
        }
        for path in ours {
            if let Some((_, file)) = state.admission.remove(&path) {
                let _ = file.unlock();
            }
        }
        drop(state);
        moved();
    }

    /// Has `build` hold the output of the script in `run_dir`: nothing writes it until the build
    /// is retired. The caller has the admission.
    pub(super) fn hold(&self, build: u64, run_dir: &Path) -> Result<(), Refused> {
        if build == 0 {
            return Ok(());
        }
        let mut state = self.state();
        let key = (build, run_dir.to_owned());
        if state.held.contains_key(&key) {
            return Ok(());
        }
        let path = run_dir.join(FILE);
        let file = open(&path)?;
        match file.try_lock_shared() {
            Ok(()) => {}
            Err(std::fs::TryLockError::WouldBlock) => {
                return Err(Busy::Elsewhere { path }.into());
            }
            Err(std::fs::TryLockError::Error(err)) => return Err(Refused::Failed(err.to_string())),
        }
        state.held.insert(key, file);
        Ok(())
    }

    /// Keeps `result` as what `build` judged of `run_dir`.
    pub(super) fn record(&self, build: u64, run_dir: &Path, result: Result<Arc<Done>, String>) {
        if build != 0 {
            self.state().judged.insert((build, run_dir.to_owned()), result);
        }
    }

    /// The right to run the script in `run_dir` for `build`, when no other build reads what it
    /// wrote. The caller has the admission.
    pub(super) fn writer(&self, build: u64, run_dir: &Path) -> Result<Writer<'_>, Refused> {
        let path = run_dir.join(FILE);
        let mut state = self.state();
        let other = state.held.keys().find(|it| it.0 != build && it.1 == run_dir).map(|it| it.0);
        if let Some(other) = other {
            return Err(Busy::Process { path, build: other }.into());
        }
        // What the build held of this script itself is given up for the run.
        if let Some(own) = state.held.remove(&(build, run_dir.to_owned())) {
            let _ = own.unlock();
        }
        state.judged.remove(&(build, run_dir.to_owned()));
        drop(state);
        let file = open(&path)?;
        match file.try_lock() {
            Ok(()) => {
                Ok(Writer { leases: self, file: Some(file), build, run_dir: run_dir.to_owned() })
            }
            Err(std::fs::TryLockError::WouldBlock) => Err(Busy::Elsewhere { path }.into()),
            Err(std::fs::TryLockError::Error(err)) => Err(Refused::Failed(err.to_string())),
        }
    }

    /// `build` holds `leases` on producer images until it is retired.
    pub(super) fn keep_images(&self, build: u64, leases: super::Images) {
        self.state().images.entry(build).or_default().extend(leases);
    }

    /// `build` holds `leases` on producer images it was given, until it is retired. A request
    /// that names no build is retired by nothing: the load that serves it holds them instead,
    /// until its proc-macro client has loaded the image, and while it serves it. What the
    /// caller is to hold is returned; a build keeps its own.
    pub(super) fn image_leases(&self, build: u64, leases: super::Images) -> super::Images {
        match build {
            0 => leases,
            build => {
                self.keep_images(build, leases);
                Vec::new()
            }
        }
    }

    /// Ends `build`: its leases and its admission are released and what it judged is forgotten.
    /// How many leases it held.
    pub(super) fn retire(&self, build: u64) -> usize {
        let mut state = self.state();
        state.judged.retain(|(it, _), _| *it != build);
        let released = release_leases(&mut state, build);
        let admitted: Vec<_> =
            state.admission.iter().filter(|it| it.1.0 == build).map(|it| it.0.clone()).collect();
        for path in admitted {
            if let Some((_, file)) = state.admission.remove(&path) {
                let _ = file.unlock();
            }
        }
        state.consumers.remove(&build);
        state.yielding.remove(&build);
        state.operations.remove(&build);
        state.executors.remove(&build);
        state.images.remove(&build);
        drop(state);
        if let Some(map) = interest().as_mut() {
            map.values_mut().for_each(|it| _ = it.owners.remove(&build));
        }
        moved();
        released
    }

    /// `build` is refused what it waits for: it lets go of the leases that no consumer of its is
    /// reading, so that it waits holding nothing; with a consumer reading, it does when the last
    /// ends. What it decided stays, a failure for good and a result as far as the output is the
    /// run it was ([`Leases::reacquire`]): only a build that another ran the script of meanwhile
    /// judges it again. Not the build that has the admission: what it holds is what the readers
    /// it waits for wait for nothing of. Whether it let go now.
    pub(super) fn yield_world(&self, build: u64) -> bool {
        if build == 0 {
            return false;
        }
        let mut state = self.state();
        if state.admission.values().any(|it| it.0 == build) {
            return false;
        }
        if !state.held.keys().any(|it| it.0 == build) {
            return false;
        }
        if state.consumers.get(&build).is_some_and(|it| *it > 0) {
            state.yielding.insert(build);
            return false;
        }
        release_leases(&mut state, build);
        drop(state);
        moved();
        true
    }

    /// Has `build` read the output of the scripts of `world`, each with the generation of the
    /// run it was loaded from: the consumer is counted first, so that no release can come
    /// between, then shared leases on all of it, taken before anything is read, and the
    /// generations looked at after. A script that is running is waited for, which it ends by
    /// itself. A world that moved since is [`Stale`], and nothing of it is held for this call.
    pub(super) fn pin(&self, build: u64, world: &[(PathBuf, u64)]) -> Result<(), Stale> {
        if build == 0 {
            return Ok(());
        }
        *self.state().consumers.entry(build).or_default() += 1;
        let mut taken = Vec::new();
        let mut failed = None;
        for (dir, generation) in world {
            let key = (build, dir.clone());
            let held = self.state().held.contains_key(&key);
            if !held {
                let path = dir.join(FILE);
                let file = match open(&path) {
                    Ok(it) => it,
                    Err(why) => {
                        failed = Some(Stale {
                            run_dir: dir.clone(),
                            why: format!("{why:?}"),
                            found: None,
                        });
                        break;
                    }
                };
                if let Err(err) = file.lock_shared() {
                    failed =
                        Some(Stale { run_dir: dir.clone(), why: err.to_string(), found: None });
                    break;
                }
                if let Some(old) = self.state().held.insert(key.clone(), file) {
                    let _ = old.unlock();
                }
                taken.push(key);
            }
            let found = super::generation_of(dir);
            if found != Some(*generation) {
                failed =
                    Some(Stale { run_dir: dir.clone(), why: "it was run again".to_owned(), found });
                break;
            }
        }
        if let Some(stale) = failed {
            let mut state = self.state();
            for key in taken {
                if let Some(file) = state.held.remove(&key) {
                    let _ = file.unlock();
                }
            }
            drop(state);
            self.unpin(build);
            return Err(stale);
        }
        Ok(())
    }

    /// One consumer of `build` is done reading.
    pub(super) fn unpin(&self, build: u64) {
        if build == 0 {
            return;
        }
        let mut state = self.state();
        let Some(count) = state.consumers.get_mut(&build) else { return };
        *count -= 1;
        if *count > 0 {
            return;
        }
        state.consumers.remove(&build);
        if state.yielding.remove(&build) {
            release_leases(&mut state, build);
            drop(state);
            moved();
        }
    }

    /// The builds that hold something, for the tests.
    #[cfg(test)]
    pub(super) fn builds(&self) -> Vec<u64> {
        let state = self.state();
        let mut builds: Vec<u64> = state
            .held
            .keys()
            .map(|it| it.0)
            .chain(state.judged.keys().map(|it| it.0))
            .chain(state.admission.values().map(|it| it.0))
            .collect();
        builds.sort();
        builds.dedup();
        builds
    }
}

/// The leases a build holds, let go. How many.
fn release_leases(state: &mut State, build: u64) -> usize {
    let held: Vec<_> = state.held.keys().filter(|(it, _)| *it == build).cloned().collect();
    let released = held.len();
    for key in held {
        if let Some(file) = state.held.remove(&key) {
            // An unlock, not a close: a process forked meanwhile may hold the description.
            let _ = file.unlock();
        }
    }
    released
}

/// A world that moved since the database was loaded from it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Stale {
    pub(crate) run_dir: PathBuf,
    pub(crate) why: String,
    /// The generation the output there is now of ([`super::generation_of`]): which run moved it.
    pub(crate) found: Option<u64>,
}

impl std::fmt::Display for Stale {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "the output of the build script in {} is not what the database was loaded from ({})",
            self.run_dir.display(),
            self.why
        )
    }
}

impl std::error::Error for Stale {}

fn open(path: &Path) -> Result<File, Refused> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|it| Refused::Failed(format!("{}: {it}", parent.display())))?;
    }
    File::options()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .map_err(|it| Refused::Failed(format!("{}: {it}", path.display())))
}

/// The right to write the output of a script: the lease is held exclusively until the run is
/// over, and then for the build, shared, or not at all.
pub(super) struct Writer<'a> {
    leases: &'a Leases,
    file: Option<File>,
    build: u64,
    run_dir: PathBuf,
}

impl Writer<'_> {
    /// The script ran and stands: the build keeps `done` and the output it wrote. The caller
    /// has the admission, so no other writer is between giving up the lease and taking it
    /// shared.
    pub(super) fn stands(mut self, done: Arc<Done>) -> Result<(), Refused> {
        if let Some(file) = self.file.take() {
            let _ = file.unlock();
        }
        self.leases.hold(self.build, &self.run_dir)?;
        self.leases.record(self.build, &self.run_dir, Ok(done));
        if self.build != 0 {
            // From here on the build keeps the admission until it is retired: the output is the
            // one run of the script the build has, and its later requests, whichever groups
            // they judge, are not to find it written by another build.
            self.leases.state().executors.insert(self.build);
        }
        Ok(())
    }

    /// The script failed: the build fails each request of it with `why`, and holds nothing.
    pub(super) fn failed(mut self, why: String) {
        if let Some(file) = self.file.take() {
            let _ = file.unlock();
        }
        self.leases.record(self.build, &self.run_dir, Err(why));
    }
}

impl Drop for Writer<'_> {
    fn drop(&mut self) {
        if let Some(file) = self.file.take() {
            let _ = file.unlock();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    fn dir(test: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("ra-jit-leases-{test}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// What a run leaves of its generation: `marker` names it. Its number.
    fn record(run_dir: &Path, marker: &str) -> u64 {
        let generation = blaze_build_script::fresh::hash_bytes(marker.as_bytes());
        std::fs::create_dir_all(run_dir).unwrap();
        std::fs::write(run_dir.join("blaze-generation"), generation.to_string()).unwrap();
        generation
    }

    fn done(out: &Path, token: u64) -> Arc<Done> {
        Arc::new(Done {
            out_dir: out.to_owned(),
            output: Default::default(),
            fingerprint: 0,
            generation: token,
            ran: true,
            made: None,
            links: None,
        })
    }

    /// The output of an earlier run, found fresh by `build`, which holds it and ran nothing.
    fn found(out: &Path, token: u64) -> Arc<Done> {
        let mut done = done(out, token);
        Arc::get_mut(&mut done).unwrap().ran = false;
        done
    }

    fn busy<T>(refused: Result<T, Refused>) -> Busy {
        match refused {
            Err(Refused::Busy(busy)) => busy,
            Err(Refused::Failed(err)) => panic!("{err}"),
            Ok(_) => panic!("it was given"),
        }
    }

    /// A script run by `build` that stands: its admission, its run, its lease. Its token.
    fn runs(leases: &Leases, build: u64, base: &Path, run_dir: &Path, marker: &str) -> u64 {
        leases.admit(build, base).unwrap();
        let writer = leases.writer(build, run_dir).unwrap();
        let token = record(run_dir, marker);
        writer.stands(done(run_dir, token)).unwrap();
        token
    }

    /// A script found fresh by `build`: its admission, its lease, nothing run. Its token.
    fn reads(leases: &Leases, build: u64, base: &Path, run_dir: &Path, marker: &str) -> u64 {
        leases.admit(build, base).unwrap();
        let token = record(run_dir, marker);
        leases.hold(build, run_dir).unwrap();
        leases.record(build, run_dir, Ok(found(run_dir, token)));
        token
    }

    #[test]
    fn the_admission_is_a_builds_across_its_waits_and_not_a_calls() {
        let base = dir("admission");
        let leases = Leases::default();
        leases.admit(1, &base).unwrap();
        leases.admit(1, &base).unwrap();
        assert!(matches!(busy(leases.admit(2, &base)), Busy::Process { build: 1, .. }));
        leases.release_admission(1);
        leases.admit(2, &base).unwrap();
        // The end of the build lets go of it too.
        leases.retire(2);
        leases.admit(3, &base).unwrap();
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn another_processs_admission_is_waited_for_and_never_a_failure_of_a_reader() {
        let (base, x) = (dir("elsewhere-admission"), dir("elsewhere-admission-x"));
        let (first, second) = (Leases::default(), Leases::default());
        first.admit(1, &base).unwrap();
        assert!(matches!(busy(second.admit(2, &base)), Busy::Elsewhere { .. }));
        assert!(wait(&base.join(ADMISSION), Some(Duration::from_millis(60))).is_err());
        // A build that reads output and needs the admission of another is put off like any,
        // not refused: it yields what it holds and waits for it.
        second.hold(2, &x).unwrap();
        assert!(matches!(busy(second.admit(2, &base)), Busy::Elsewhere { .. }));
        assert!(second.yield_world(2), "it reads nothing now, so it lets go");
        assert!(second.builds().is_empty());
        first.release_admission(1);
        wait(&base.join(ADMISSION), Some(Duration::from_secs(5))).unwrap();
        let _ = std::fs::remove_dir_all(&base);
        let _ = std::fs::remove_dir_all(&x);
    }

    #[test]
    fn what_a_build_judged_is_an_immutable_result_kept_until_it_ends() {
        let (base, x) = (dir("judged"), dir("judged-x"));
        let leases = Leases::default();
        assert!(leases.judged(1, &x).is_none());
        reads(&leases, 1, &base, &x, "one");
        let first = leases.judged(1, &x).unwrap().unwrap();
        // Another build asks to run it: it is held, so it is put off, and the result stands.
        leases.release_admission(1);
        leases.admit(2, &base).unwrap();
        assert!(matches!(busy(leases.writer(2, &x)), Busy::Process { build: 1, .. }));
        assert!(Arc::ptr_eq(&first, &leases.judged(1, &x).unwrap().unwrap()));
        leases.retire(1);
        assert!(leases.judged(1, &x).is_none());
        leases.writer(2, &x).unwrap().failed("exit status: 3".to_owned());
        assert!(matches!(leases.judged(2, &x), Some(Err(why)) if why == "exit status: 3"));
        let _ = std::fs::remove_dir_all(&base);
        let _ = std::fs::remove_dir_all(&x);
    }

    #[test]
    fn what_moves_is_seen_by_a_request_that_slept_on_the_generation_it_read() {
        let (base, x) = (dir("generation"), dir("generation-x"));
        let leases = Leases::default();
        runs(&leases, 1, &base, &x, "one");
        let read = generation();
        leases.retire(1);
        assert_ne!(generation(), read);
        leases.admit(2, &base).unwrap();
        let read = generation();
        leases.release_admission(2);
        assert_ne!(generation(), read);
        let _ = std::fs::remove_dir_all(&base);
        let _ = std::fs::remove_dir_all(&x);
    }

    /// The review's multi-group sequence: a build holds the group it judged, another build owns
    /// the admission and waits for what the first reads, and the first needs another group. In
    /// one process it was a wait without end; in two, a refusal.
    #[test]
    fn a_reader_that_needs_another_group_yields_what_it_holds_and_the_writer_goes_on() {
        let (base, r, s) = (dir("growth"), dir("growth-r"), dir("growth-s"));
        for processes in [false, true] {
            let _ = std::fs::remove_dir_all(&r);
            let (reader, writer) = match processes {
                false => (Arc::new(Leases::default()), None),
                true => (Arc::new(Leases::default()), Some(Leases::default())),
            };
            let writer_side = writer.as_ref().unwrap_or(&reader);
            // M found R fresh and its admission is over: it holds R.
            reads(&reader, 1, &base, &r, "r1");
            reader.release_admission(1);
            // W owns the admission and must run R, which M reads: it waits.
            writer_side.admit(2, &base).unwrap();
            let waiting = busy(writer_side.writer(2, &r));
            assert!(matches!(waiting, Busy::Process { .. } | Busy::Elsewhere { .. }));
            // M's next request needs another group: the admission is W's, and M is put off,
            // not failed. M reads nothing at the moment, so it lets go of R and waits.
            let refused = busy(reader.admit(1, &base));
            assert!(matches!(refused, Busy::Process { .. } | Busy::Elsewhere { .. }));
            assert!(reader.yield_world(1));
            // It holds no lease, and what it decided stays.
            assert!(matches!(reader.decision(1, &r), Some(Judged::Released(_))));
            let old = super::super::generation_of(&r).unwrap();
            // W goes on, and ends; M asks again and gets the admission.
            let token = record(&r, "r2");
            writer_side.writer(2, &r).unwrap().stands(done(&r, token)).unwrap();
            // W ran a script: it has the admission until its build ends.
            writer_side.release_admission(2);
            assert!(matches!(
                busy(reader.admit(1, &base)),
                Busy::Process { .. } | Busy::Elsewhere { .. }
            ));
            writer_side.retire(2);
            reader.admit(1, &base).unwrap();
            // W ran R meanwhile: M only found R fresh, ran nothing of it, so its result is of an
            // output that is not there, and is forgotten: judging it again is its first run.
            assert!(!reader.reacquire(1, &r, old, false).unwrap());
            assert!(reader.decision(1, &r).is_none());
            reader.release_admission(1);
            reader.retire(1);
        }
        let _ = std::fs::remove_dir_all(&base);
        let _ = std::fs::remove_dir_all(&r);
        let _ = std::fs::remove_dir_all(&s);
    }

    /// Two direct loads with `--also` in opposite orders over one target directory: each
    /// judged its first group and needs the second, which the other judged.
    #[test]
    fn opposite_group_orders_both_finish_whoever_holds_what() {
        let (base, r, s) = (dir("opposite"), dir("opposite-r"), dir("opposite-s"));
        let (a, b) = (Leases::default(), Leases::default());
        // A found R fresh, B found S; each let go of the admission between its groups.
        reads(&a, 1, &base, &r, "r-a");
        a.release_admission(1);
        let on_s = reads(&b, 2, &base, &s, "s-b");
        b.release_admission(2);
        // A's second group is S, which it must run: B reads it, so A waits, and keeps the
        // admission.
        a.admit(1, &base).unwrap();
        assert!(matches!(busy(a.writer(1, &s)), Busy::Elsewhere { .. }));
        assert!(!a.yield_world(1), "it has the admission: what it holds is waited for by no one");
        // B's second group is R: the admission is A's, so B is put off (no failure), and it
        // yields S, which A waits for.
        assert!(matches!(busy(b.admit(2, &base)), Busy::Elsewhere { .. }));
        assert!(b.yield_world(2));
        // A goes on and ends its run, and keeps the admission: it ran a script. B waits holding
        // nothing, so A's later groups find nothing of B's in the way.
        let token = record(&s, "s-a");
        a.writer(1, &s).unwrap().stands(done(&s, token)).unwrap();
        a.release_admission(1);
        assert!(matches!(busy(b.admit(2, &base)), Busy::Elsewhere { .. }));
        assert!(b.builds().len() == 1, "B holds no lease; its decision stays");
        // A's build ends; B follows, over what A left: it runs R, which A no longer reads.
        a.retire(1);
        b.admit(2, &base).unwrap();
        assert!(!b.reacquire(2, &s, on_s, false).unwrap(), "A ran S meanwhile; B only read it");
        let token = record(&r, "r-b");
        b.writer(2, &r).unwrap().stands(done(&r, token)).unwrap();
        b.retire(2);
        let _ = std::fs::remove_dir_all(&base);
        let _ = std::fs::remove_dir_all(&r);
        let _ = std::fs::remove_dir_all(&s);
    }

    /// The review's sequence at the leases: a build ran a script and answered, and has more to
    /// judge. Another build may not take the admission between its requests to wait for the
    /// output, and so the first never has to give it up: the script is not run a second time.
    #[test]
    fn a_build_that_ran_a_script_keeps_the_output_and_the_admission_until_it_is_retired() {
        let (base, r, s) = (dir("executor"), dir("executor-r"), dir("executor-s"));
        for processes in [false, true] {
            let _ = std::fs::remove_dir_all(&r);
            let (a, b) = match processes {
                false => {
                    let one = Arc::new(Leases::default());
                    (one.clone(), one)
                }
                true => (Arc::new(Leases::default()), Arc::new(Leases::default())),
            };
            // A's first operation runs R and answers; its attempt ends and lets go of the
            // admission, which a build that ran a script does not.
            let token = runs(&a, 1, &base, &r, "r-a");
            a.release_admission(1);
            // B asks for the admission to run R after it: it is not given, so it holds nothing.
            let refused = busy(b.admit(2, &base));
            assert!(matches!(refused, Busy::Process { build: 1, .. } | Busy::Elsewhere { .. }));
            assert!(!a.yield_world(1), "A has the admission: nothing waits for what it holds");
            // A's later operation judges another group, and R again: both are A's.
            a.admit(1, &base).unwrap();
            let before = a.judged(1, &r).unwrap().unwrap();
            assert!(matches!(a.decision(1, &r), Some(Judged::Standing(_))));
            reads(&a, 1, &base, &s, "s-a");
            a.release_admission(1);
            assert_eq!(super::super::generation_of(&r), Some(token), "nobody wrote R");
            assert!(Arc::ptr_eq(&before, &a.judged(1, &r).unwrap().unwrap()));
            // B waits for the end of A's build, and then runs.
            assert!(matches!(
                busy(b.admit(2, &base)),
                Busy::Process { build: 1, .. } | Busy::Elsewhere { .. }
            ));
            a.retire(1);
            b.admit(2, &base).unwrap();
            let again = record(&r, "r-b");
            b.writer(2, &r).unwrap().stands(done(&r, again)).unwrap();
            b.retire(2);
        }
        let _ = std::fs::remove_dir_all(&base);
        let _ = std::fs::remove_dir_all(&r);
        let _ = std::fs::remove_dir_all(&s);
    }

    /// A build that ran a script is never given another run of it: if its output is not the run
    /// it was, that is an error and not a second execution.
    #[test]
    fn the_output_of_a_script_a_build_ran_is_never_forgotten_for_a_second_run() {
        let (base, r) = (dir("ran-forgotten"), dir("ran-forgotten-r"));
        let leases = Leases::default();
        let token = runs(&leases, 1, &base, &r, "one");
        // Whatever took its lease (nothing in the protocol does), the result is not dropped.
        release_leases(&mut leases.state(), 1);
        record(&r, "two");
        let refused = leases.reacquire(1, &r, token, true);
        assert!(matches!(refused, Err(Refused::Failed(why)) if why.contains("written again")));
        assert!(matches!(leases.decision(1, &r), Some(Judged::Released(_))));
        let _ = std::fs::remove_dir_all(&base);
        let _ = std::fs::remove_dir_all(&r);
    }

    #[test]
    fn a_consumer_reads_nothing_it_has_not_pinned_and_a_build_that_yields_waits_for_its_end() {
        let (base, x) = (dir("pin"), dir("pin-x"));
        let leases = Leases::default();
        let token = reads(&leases, 1, &base, &x, "one");
        leases.release_admission(1);
        let world = vec![(x.clone(), token)];
        leases.pin(1, &world).unwrap();
        // Another build is put off; the one that holds lets go only when its consumer is done.
        leases.admit(2, &base).unwrap();
        assert!(matches!(busy(leases.writer(2, &x)), Busy::Process { build: 1, .. }));
        assert!(!leases.yield_world(1), "a consumer reads it");
        assert!(matches!(busy(leases.writer(2, &x)), Busy::Process { build: 1, .. }));
        leases.unpin(1);
        leases.writer(2, &x).unwrap();
        assert!(matches!(leases.decision(1, &x), Some(Judged::Released(_))));
        let _ = std::fs::remove_dir_all(&base);
        let _ = std::fs::remove_dir_all(&x);
    }

    #[test]
    fn a_world_that_moved_is_not_read_and_holds_nothing() {
        let (base, x) = (dir("moved"), dir("moved-x"));
        let leases = Leases::default();
        let token = runs(&leases, 1, &base, &x, "one");
        leases.release_admission(1);
        leases.retire(1);
        // An anonymous consumer of the database loaded from the first run.
        let world = vec![(x.clone(), token)];
        let reader = anonymous();
        leases.pin(reader, &world).unwrap();
        leases.unpin(reader);
        leases.retire(reader);
        // Another worker ran the script again meanwhile: the database is not of this output.
        record(&x, "two");
        let stale = leases.pin(reader, &world).unwrap_err();
        assert_eq!(stale.run_dir, x);
        assert!(leases.builds().is_empty(), "nothing is held for a world that is refused");
        let _ = std::fs::remove_dir_all(&base);
        let _ = std::fs::remove_dir_all(&x);
    }

    #[test]
    fn a_script_that_is_running_is_waited_for_and_what_it_left_is_then_looked_at() {
        let (base, x) = (dir("running"), dir("running-x"));
        let leases = Arc::new(Leases::default());
        let token = record(&x, "one");
        leases.admit(2, &base).unwrap();
        let writer = leases.writer(2, &x).unwrap();
        let reader = {
            let (leases, world) = (leases.clone(), vec![(x.clone(), token)]);
            std::thread::spawn(move || leases.pin(7, &world))
        };
        std::thread::sleep(Duration::from_millis(150));
        assert!(!reader.is_finished(), "a script is running: the consumer waits for it");
        let after = record(&x, "two");
        writer.stands(done(&x, after)).unwrap();
        let stale = reader.join().unwrap().unwrap_err();
        assert_eq!(stale.run_dir, x, "and finds it is not the output its database is of");
        let _ = std::fs::remove_dir_all(&base);
        let _ = std::fs::remove_dir_all(&x);
    }

    #[test]
    fn a_watch_ends_when_no_build_that_asked_is_left() {
        let (base, x) = (dir("watch"), dir("watch-x"));
        let leases = Leases::default();
        runs(&leases, 1, &base, &x, "one");
        let held = x.join(FILE);
        let other = Leases::default();
        // Another process reads it: a build waits for it and a thread watches.
        assert!(matches!(busy(other.writer(2, &x)), Busy::Elsewhere { .. }));
        watch(&held, 2);
        assert!(interest().as_ref().is_some_and(|it| it.contains_key(&held)));
        // The build that asked is retired; the file is held still, and the watch ends.
        other.retire(2);
        let began = Instant::now();
        while interest().as_ref().is_some_and(|it| it.contains_key(&held)) {
            assert!(began.elapsed() < Duration::from_secs(5), "the watch outlived its build");
            std::thread::sleep(Duration::from_millis(20));
        }
        let _ = std::fs::remove_dir_all(&base);
        let _ = std::fs::remove_dir_all(&x);
    }

    #[test]
    fn a_result_that_was_yielded_is_the_builds_again_while_the_output_is_the_run_it_was_of() {
        let (base, x) = (dir("released"), dir("released-x"));
        let leases = Leases::default();
        let generation = reads(&leases, 1, &base, &x, "one");
        leases.release_admission(1);
        assert!(matches!(leases.decision(1, &x), Some(Judged::Standing(_))));
        assert!(leases.yield_world(1));
        assert!(matches!(leases.decision(1, &x), Some(Judged::Released(_))));
        // Another build reads it meanwhile, which is nothing to the result.
        let other = anonymous();
        leases.hold(other, &x).unwrap();
        assert!(leases.reacquire(1, &x, generation, false).unwrap());
        assert!(
            matches!(leases.decision(1, &x), Some(Judged::Standing(done)) if done.generation == generation)
        );
        // A failure is the build's, with or without a lease.
        leases.admit(1, &base).unwrap();
        leases.retire(other);
        leases.writer(1, &x).unwrap().failed("exit status: 3".to_owned());
        assert!(leases.yield_world(1) || !leases.builds().is_empty());
        assert!(
            matches!(leases.decision(1, &x), Some(Judged::Failed(why)) if why == "exit status: 3")
        );
        let _ = std::fs::remove_dir_all(&base);
        let _ = std::fs::remove_dir_all(&x);
    }

    /// The second check's lease finding: a request that names no build is retired by nothing,
    /// so the load that serves it holds the images it admitted, until its proc-macro client
    /// has loaded them and while it serves them; a build's images are its own until it is
    /// retired. Either way the image is collected once the last lease is gone.
    #[test]
    fn an_image_of_a_request_that_names_no_build_is_leased_by_its_load_until_it_lets_go() {
        let root = dir("image-owner");
        let generation = |name: &str| {
            let generation = root.join(name);
            let file = load_cargo::proc_macro_producer_lifetime::lease(&generation).unwrap();
            std::fs::write(generation.join("library"), b"bytes").unwrap();
            (generation, Arc::new(file))
        };
        let collect = |current: &Path| {
            load_cargo::proc_macro_producer_lifetime::collect(&root, current).unwrap()
        };
        let leases = Leases::default();
        // A pass that names no build: the leases are the load's, which keeps them.
        let (first, held) = generation("first");
        let kept = leases.image_leases(0, vec![held]);
        assert_eq!(kept.len(), 1, "the load holds what no build can retire");
        let (second, _) = generation("second");
        assert!(collect(&second).is_empty(), "a leased image is never collected");
        assert!(first.join("library").is_file());
        drop(kept);
        assert_eq!(collect(&second), [first], "the load let go: the image is collected");
        // A build's images are its own until it is retired.
        let (third, held) = generation("third");
        assert!(leases.image_leases(7, vec![held]).is_empty(), "a build keeps its own");
        assert!(collect(&second).is_empty(), "a build's image is leased until it is retired");
        leases.retire(7);
        assert_eq!(collect(&second), [third], "a retired build lets go of its images");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_pin_that_is_taking_its_leases_is_a_consumer_before_it_has_them() {
        let (base, x, y) = (dir("inflight"), dir("inflight-x"), dir("inflight-y"));
        let leases = Arc::new(Leases::default());
        let token = reads(&leases, 1, &base, &x, "one");
        leases.release_admission(1);
        // Y is being run by another build: a pin that wants it waits.
        let other = Leases::default();
        let on_y = record(&y, "y");
        other.admit(2, &base).unwrap();
        let writer = other.writer(2, &y).unwrap();
        let pinning = {
            let (leases, world) = (leases.clone(), vec![(x.clone(), token), (y.clone(), on_y)]);
            std::thread::spawn(move || leases.pin(1, &world))
        };
        std::thread::sleep(Duration::from_millis(150));
        assert!(!pinning.is_finished());
        // The build is refused something and yields while that pin is not done: it is a
        // consumer already, so nothing it holds is let go under it.
        assert!(!leases.yield_world(1));
        assert!(matches!(leases.decision(1, &x), Some(Judged::Standing(_))));
        writer.stands(done(&y, on_y)).unwrap();
        pinning.join().unwrap().unwrap();
        // Its end is the release the yield waited for.
        leases.unpin(1);
        assert!(matches!(leases.decision(1, &x), Some(Judged::Released(_))));
        let _ = std::fs::remove_dir_all(&base);
        let _ = std::fs::remove_dir_all(&x);
        let _ = std::fs::remove_dir_all(&y);
    }

    #[test]
    fn an_operation_has_the_admission_for_all_its_groups_and_no_build_is_between_them() {
        let (base, r, s) = (dir("operation"), dir("operation-r"), dir("operation-s"));
        let (a, b) = (Leases::default(), Leases::default());
        a.begin_operation(1);
        // A judges R and S, both found fresh; its first group ends an attempt, and the
        // admission is its still.
        reads(&a, 1, &base, &r, "r-a");
        assert!(a.in_operation(1));
        // B asks for S first: it holds nothing, so it has nothing for A to wait for.
        assert!(matches!(busy(b.admit(2, &base)), Busy::Elsewhere { .. }));
        assert!(b.builds().is_empty());
        reads(&a, 1, &base, &s, "s-a");
        a.end_operation(1, false);
        assert!(!a.in_operation(1));
        // The operation is over and the admission with it; A reads both, and B runs after it.
        b.admit(2, &base).unwrap();
        assert!(matches!(busy(b.writer(2, &s)), Busy::Elsewhere { .. }));
        a.retire(1);
        b.writer(2, &s).unwrap();
        // One that waits keeps it.
        b.begin_operation(2);
        b.end_operation(2, true);
        assert!(matches!(busy(a.admit(3, &base)), Busy::Elsewhere { .. }));
        let _ = std::fs::remove_dir_all(&base);
        let _ = std::fs::remove_dir_all(&r);
        let _ = std::fs::remove_dir_all(&s);
    }

    /// The interleavings of the window, one at a time: the exit is decided on an empty set of
    /// builds and a build registers before it is carried out; and the entry is removed and a build
    /// registers, and the watch that was ending makes its late exit.
    #[test]
    fn an_exit_decided_on_no_builds_leaves_the_entry_of_one_that_registered_since() {
        let x = dir("ends-x");
        let held = x.join(FILE);
        let first = register(&held, 1).unwrap();
        // Its build retires: nobody is left, and the thread has not looked yet.
        interest().as_mut().unwrap().get_mut(&held).unwrap().owners.clear();
        // A build asks for the file now: it joins the watch there is.
        assert_eq!(register(&held, 2), None);
        // The thread looks: a build is there, and it goes on.
        assert!(!ends(&held, first, false));
        assert_eq!(interest().as_ref().unwrap().get(&held).unwrap().owners.len(), 1);
        // The other order. Nobody is left and the thread ends and removes its entry.
        interest().as_mut().unwrap().get_mut(&held).unwrap().owners.clear();
        assert!(ends(&held, first, false));
        assert!(interest().as_ref().unwrap().get(&held).is_none());
        // A build asks: it has a watch of its own, with a thread to start.
        let second = register(&held, 3).unwrap();
        assert_ne!(first, second);
        // A late exit of the first (its file was free) removes nothing of the second's.
        assert!(ends(&held, first, true));
        assert_eq!(interest().as_ref().unwrap().get(&held).unwrap().id, second);
        // The second ends when the file is free.
        assert!(ends(&held, second, true));
        assert!(interest().as_ref().unwrap().get(&held).is_none());
        let _ = std::fs::remove_dir_all(&x);
    }

    /// The window the review named, with threads: the last build that asked for a file retires, the thread
    /// that watches it decides to end, a build asks for the same file, and the thread ends. The
    /// build that asked is woken when the file is free, whichever way the threads are scheduled.
    #[test]
    fn a_build_that_asks_as_a_watch_ends_is_woken_when_the_file_is_free() {
        let (base, x) = (dir("watch-window"), dir("watch-window-x"));
        let holder = Leases::default();
        runs(&holder, 1, &base, &x, "one");
        let held = x.join(FILE);
        // The watch looks again every few microseconds, so that it is awake for the rounds.
        WATCH_PAUSE_MAX.store(100, Ordering::SeqCst);
        let first = 100;
        watch(&held, first);
        for round in 0..200u64 {
            // The retirement of the build that asked, and the next asker, close together.
            let retiring = first + round;
            let retired = std::thread::spawn(move || {
                Leases::default().retire(retiring);
            });
            watch(&held, first + round + 1);
            retired.join().unwrap();
        }
        let last = first + 200;
        // The asker that is left has a watch of its own: the entry is there, and it has it.
        let owners = || {
            interest()
                .as_ref()
                .and_then(|it| it.get(&held))
                .map(|it| it.owners.iter().copied().collect::<Vec<_>>())
        };
        WATCH_PAUSE_MAX.store(250_000, Ordering::SeqCst);
        assert_eq!(owners(), Some(vec![last]), "the watch of the build that is left was lost");
        // The holder lets go: the thread that has the entry sees the file free and ends it.
        holder.retire(1);
        let began = Instant::now();
        while owners().is_some() {
            assert!(
                began.elapsed() < Duration::from_secs(5),
                "no thread woke the build that asked"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        let _ = std::fs::remove_dir_all(&base);
        let _ = std::fs::remove_dir_all(&x);
    }
}
