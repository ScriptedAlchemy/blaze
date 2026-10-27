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
//! 3. A build that holds leases and finds it needs the admission (its scripts changed within the
//!    build) is refused at once and does not wait, as it would hold what the build that has the
//!    admission waits for.
//! 4. The admission is let go when an attempt ends in anything but a wait, and when the build is
//!    retired. The builds put off are asked again then.
//!
//! Waits are not here: [`Busy`] is the answer, and the caller puts the request off. The worker
//! asks again when [`generation`] has moved since it began: a retirement, an admission let go and
//! a [`watch`]ed file of another process that was freed all move it, so that none of them is
//! lost when it comes while the request is on its way to the queue.
//!
//! Every consumer of a script's output has an owner: a numbered build, the request's own
//! anonymous number ([`anonymous`], released when the request is dropped), or the load of a
//! process that has no daemon (its first program is built under it). A request that names no
//! build (`build == 0` below) keeps nothing.

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

/// The files of other processes that are being watched for being freed.
static WATCHING: Mutex<Option<HashSet<PathBuf>>> = Mutex::new(None);

/// Moves [`generation`] when `path`, held by another process, is free to be taken: a thread
/// of its own polls it, so that the worker's main loop does not.
pub(crate) fn watch(path: &Path) {
    let mut watching = WATCHING.lock().unwrap_or_else(|it| it.into_inner());
    if !watching.get_or_insert_with(HashSet::default).insert(path.to_owned()) {
        return;
    }
    drop(watching);
    let path = path.to_owned();
    std::thread::spawn(move || {
        let _ = wait(&path, None);
        WATCHING
            .lock()
            .unwrap_or_else(|it| it.into_inner())
            .get_or_insert_with(HashSet::default)
            .remove(&path);
        moved();
    });
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
    /// The run directories of the scripts the process last judged: what its loaded workspace
    /// reads, which a build that judged nothing itself reads as well.
    loaded: Vec<PathBuf>,
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
        let reading = state.held.keys().any(|it| it.0 == build);
        let file = open(&path)?;
        match file.try_lock() {
            Ok(()) => {
                state.admission.insert(path, (build, file));
                Ok(())
            }
            Err(std::fs::TryLockError::WouldBlock) if reading => Err(Refused::Failed(
                "the build scripts of this build changed while it was reading their output, \
                 and another build is running scripts: ask again"
                    .to_owned(),
            )),
            Err(std::fs::TryLockError::WouldBlock) => Err(Busy::Elsewhere { path }.into()),
            Err(std::fs::TryLockError::Error(err)) => Err(Refused::Failed(err.to_string())),
        }
    }

    /// Lets go of the admission `build` has, if it has one.
    pub(super) fn release_admission(&self, build: u64) {
        let mut state = self.state();
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

    /// Ends `build`: its leases and its admission are released and what it judged is forgotten.
    /// How many leases it held.
    pub(super) fn retire(&self, build: u64) -> usize {
        let mut state = self.state();
        state.judged.retain(|(it, _), _| *it != build);
        let held: Vec<_> = state.held.keys().filter(|(it, _)| *it == build).cloned().collect();
        let released = held.len();
        for key in held {
            if let Some(file) = state.held.remove(&key) {
                // An unlock, not a close: a process forked meanwhile may hold the description.
                let _ = file.unlock();
            }
        }
        let admitted: Vec<_> =
            state.admission.iter().filter(|it| it.1.0 == build).map(|it| it.0.clone()).collect();
        for path in admitted {
            if let Some((_, file)) = state.admission.remove(&path) {
                let _ = file.unlock();
            }
        }
        drop(state);
        moved();
        released
    }

    /// The run directories of the scripts the last attempt judged.
    pub(super) fn set_loaded(&self, run_dirs: Vec<PathBuf>) {
        self.state().loaded = run_dirs;
    }

    /// Has `build`, a consumer that judged nothing, hold the output of the scripts the process
    /// last judged, waiting for a script that is running: shared, so as to wait for nothing a
    /// reader holds. A reader never waits for the admission, so this cannot be waited on by one.
    pub(super) fn hold_loaded(&self, build: u64) -> Result<usize, String> {
        if build == 0 {
            return Ok(0);
        }
        let dirs: Vec<PathBuf> = {
            let state = self.state();
            let held = |dir: &PathBuf| state.held.contains_key(&(build, dir.clone()));
            state.loaded.iter().filter(|dir| !held(dir)).cloned().collect()
        };
        let mut taken = 0;
        for dir in dirs {
            let path = dir.join(FILE);
            let file = open(&path).map_err(|it| match it {
                Refused::Failed(why) => why,
                Refused::Busy(busy) => busy.to_string(),
            })?;
            file.lock_shared().map_err(|it| format!("{}: {it}", path.display()))?;
            if let Some(old) = self.state().held.insert((build, dir), file) {
                let _ = old.unlock();
            }
            taken += 1;
        }
        Ok(taken)
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

    fn done(out: &Path) -> Arc<Done> {
        Arc::new(Done {
            out_dir: out.to_owned(),
            output: Default::default(),
            fingerprint: 0,
            made: None,
            links: None,
        })
    }

    fn busy<T>(refused: Result<T, Refused>) -> Busy {
        match refused {
            Err(Refused::Busy(busy)) => busy,
            Err(Refused::Failed(err)) => panic!("{err}"),
            Ok(_) => panic!("it was given"),
        }
    }

    /// A script run by `build` that stands: its admission, its run, its lease.
    fn runs(leases: &Leases, build: u64, base: &Path, run_dir: &Path) {
        leases.admit(build, base).unwrap();
        leases.writer(build, run_dir).unwrap().stands(done(run_dir)).unwrap();
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
    fn another_processs_admission_is_waited_for_and_a_reader_does_not_wait_for_it() {
        let (base, x) = (dir("elsewhere-admission"), dir("elsewhere-admission-x"));
        let (first, second) = (Leases::default(), Leases::default());
        first.admit(1, &base).unwrap();
        assert!(matches!(busy(second.admit(2, &base)), Busy::Elsewhere { .. }));
        assert!(wait(&base.join(ADMISSION), Some(Duration::from_millis(60))).is_err());
        // A build that reads output must not wait for the admission of another, which waits for
        // its output in turn: it is told so at once.
        second.hold(2, &x).unwrap();
        assert!(matches!(second.admit(2, &base), Err(Refused::Failed(_))));
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
        runs(&leases, 1, &base, &x);
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
        runs(&leases, 1, &base, &x);
        // A request reads the generation, is put off, and the build it waits for ends before it
        // is queued: it sees that the generation is not the one it read.
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

    #[test]
    fn a_consumer_that_judged_nothing_holds_what_was_last_judged_until_it_is_released() {
        let (base, x) = (dir("loaded"), dir("loaded-x"));
        let leases = Leases::default();
        runs(&leases, 1, &base, &x);
        leases.set_loaded(vec![x.clone()]);
        leases.retire(1);
        // The request that carries no build has one of its own.
        let own = anonymous();
        assert_eq!(leases.hold_loaded(own).unwrap(), 1);
        assert_eq!(leases.hold_loaded(own).unwrap(), 0);
        leases.admit(2, &base).unwrap();
        assert!(matches!(busy(leases.writer(2, &x)), Busy::Process { .. }));
        leases.retire(own);
        leases.writer(2, &x).unwrap();
        let _ = std::fs::remove_dir_all(&base);
        let _ = std::fs::remove_dir_all(&x);
    }

    /// The review's opposite orders: C reads X; A runs Y and then wants X; B finds X fresh and
    /// wants Y. With the admission B holds nothing while A waits, and neither waits for the other.
    #[test]
    fn builds_that_take_the_scripts_in_opposite_orders_cannot_wait_for_each_other() {
        let (base, x, y) = (dir("coab"), dir("coab-x"), dir("coab-y"));
        let (c, a, b) = (Leases::default(), Leases::default(), Leases::default());
        // C reads X, and its build goes on.
        runs(&c, 1, &base, &x);
        c.release_admission(1);
        // A runs Y, then must run X, which C reads: A waits, and keeps the admission.
        a.admit(2, &base).unwrap();
        a.writer(2, &y).unwrap().stands(done(&y)).unwrap();
        assert!(matches!(busy(a.writer(2, &x)), Busy::Elsewhere { .. }));
        // B would find X fresh and want Y: it cannot even begin, and holds nothing.
        assert!(matches!(busy(b.admit(3, &base)), Busy::Elsewhere { .. }));
        assert!(b.builds().is_empty());
        // C ends: A goes on, and ends.
        c.retire(1);
        a.writer(2, &x).unwrap().stands(done(&x)).unwrap();
        a.release_admission(2);
        // B begins: X is held by A and it reads it too, and Y it must run waits for A's build to
        // end, with nothing else waiting for B.
        b.admit(3, &base).unwrap();
        b.hold(3, &x).unwrap();
        assert!(matches!(busy(b.writer(3, &y)), Busy::Elsewhere { .. }));
        a.retire(2);
        b.writer(3, &y).unwrap().stands(done(&y)).unwrap();
        b.release_admission(3);
        let _ = std::fs::remove_dir_all(&base);
        let _ = std::fs::remove_dir_all(&x);
        let _ = std::fs::remove_dir_all(&y);
    }
}
