//! What a build holds of the scripts it judged, for as long as the build lasts.
//!
//! A script's `OUT_DIR` is persistent and shared: cargo runs a script again in the same
//! directory, the script may read what its last run left, and its path is in what the script
//! prints and in what it generates. So a run cannot be given a directory of its own to keep
//! another build's output apart, and a copy of the output at the path of another would not be the
//! output the script described. What cargo does is hold the build directory's lock for the whole
//! build, so that no other build writes while one reads. A worker builds in requests, several to
//! a build, over minutes, and other workers share the directory, so the lock is taken per script
//! and shared among the builds that only read:
//!
//! - A build that judges a script (fresh, or run) takes a **shared lease** on the script's run
//!   directory (`flock` on `blaze-lease` in it) and keeps it until the build is retired. Builds
//!   that read the same output hold it together.
//! - A build that is to run a script takes the lease **exclusively** first, so it runs only when
//!   no build reads what the script wrote. It gives the exclusive lease up for a shared one, still
//!   under the transaction lock ([`super::lock`]), which every taker of an exclusive lease holds:
//!   no other run is between the two.
//! - What a build judged is kept with the lease, by the build's number and the run directory: a
//!   script is judged once in each build, whatever other builds judged between its requests, and
//!   a script that failed fails each request of the build with the same message.
//! - A run that finds the lease held does not wait here. [`Busy::Process`] is another build of
//!   this process, which only that build's end (or the daemon's saying so) releases, so the
//!   request is put off and asked again; [`Busy::Elsewhere`] is another worker's, whose build
//!   ends on its own, so it is waited for ([`Leases::wait`]) outside the transaction lock.
//!
//! The daemon retires a build when it has answered the client (`retire` to the worker); a worker
//! that ends releases what it holds with its descriptors. A request that names no build
//! (`build == 0`) keeps nothing: it is judged afresh and its output is held only by the
//! transaction lock, as before.

use std::{
    collections::HashMap,
    fs::File,
    io,
    path::{Path, PathBuf},
    sync::Mutex,
    time::{Duration, Instant},
};

/// The file in a script's run directory that its leases are on.
const FILE: &str = "blaze-lease";

/// How long a worker waits for another worker's build to end, unless
/// `RA_JIT_SCRIPT_LEASE_WAIT_SECS` says otherwise: cargo waits for the build directory without
/// end; a deadline turns two builds that wait for each other into a message.
const WAIT: Duration = Duration::from_secs(600);

/// Why a script cannot be run now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Busy {
    /// Another build of this process reads what it wrote.
    Process { run_dir: PathBuf, build: u64 },
    /// A build of another process does, or runs the script.
    Elsewhere { run_dir: PathBuf },
}

impl std::fmt::Display for Busy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Busy::Process { run_dir, build } => write!(
                f,
                "the output of the build script in {} is in use by build {build} of this worker",
                run_dir.display()
            ),
            Busy::Elsewhere { run_dir } => write!(
                f,
                "the output of the build script in {} is in use by another worker",
                run_dir.display()
            ),
        }
    }
}

impl std::error::Error for Busy {}

/// Why a lease was not given.
#[derive(Debug)]
pub(super) enum Refused {
    Busy(Busy),
    Io(String),
}

impl From<Busy> for Refused {
    fn from(busy: Busy) -> Refused {
        Refused::Busy(busy)
    }
}

/// The leases a process holds, and what its builds judged.
#[derive(Default)]
pub(super) struct Leases {
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    /// What each build judged of each run directory: `None` when its script stands (it was
    /// fresh or it ran), `Some` with the message when it failed.
    judged: HashMap<(u64, PathBuf), Option<String>>,
    /// The shared lease each build holds on a run directory whose script stands.
    held: HashMap<(u64, PathBuf), File>,
}

impl Leases {
    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|it| it.into_inner())
    }

    /// What `build` judged of `run_dir` already: `None` when it has not, else whether it failed.
    pub(super) fn judged(&self, build: u64, run_dir: &Path) -> Option<Option<String>> {
        if build == 0 {
            return None;
        }
        self.state().judged.get(&(build, run_dir.to_owned())).cloned()
    }

    /// Has `build` hold the output of the script in `run_dir`, which was fresh: nothing writes
    /// it until the build is retired. The caller holds the transaction lock.
    pub(super) fn stands(&self, build: u64, run_dir: &Path) -> Result<(), Refused> {
        if build == 0 {
            return Ok(());
        }
        let mut state = self.state();
        let key = (build, run_dir.to_owned());
        if !state.held.contains_key(&key) {
            let file = open(run_dir)?;
            match file.try_lock_shared() {
                Ok(()) => {}
                Err(std::fs::TryLockError::WouldBlock) => {
                    return Err(Busy::Elsewhere { run_dir: run_dir.to_owned() }.into());
                }
                Err(std::fs::TryLockError::Error(err)) => return Err(Refused::Io(err.to_string())),
            }
            state.held.insert(key.clone(), file);
        }
        state.judged.insert(key, None);
        Ok(())
    }

    /// The right to run the script in `run_dir` for `build`, when no other build reads what it
    /// wrote. The caller holds the transaction lock.
    pub(super) fn writer(&self, build: u64, run_dir: &Path) -> Result<Writer<'_>, Refused> {
        let mut state = self.state();
        if let Some(((other, _), _)) =
            state.held.iter().find(|((it, dir), _)| *it != build && dir.as_path() == run_dir)
        {
            return Err(Busy::Process { run_dir: run_dir.to_owned(), build: *other }.into());
        }
        // What the build held of this script itself is given up for the run.
        if let Some(own) = state.held.remove(&(build, run_dir.to_owned())) {
            let _ = own.unlock();
        }
        state.judged.remove(&(build, run_dir.to_owned()));
        drop(state);
        let file = open(run_dir)?;
        match file.try_lock() {
            Ok(()) => {
                Ok(Writer { leases: self, file: Some(file), build, run_dir: run_dir.to_owned() })
            }
            Err(std::fs::TryLockError::WouldBlock) => {
                Err(Busy::Elsewhere { run_dir: run_dir.to_owned() }.into())
            }
            Err(std::fs::TryLockError::Error(err)) => Err(Refused::Io(err.to_string())),
        }
    }

    /// Ends `build`: its leases are released and what it judged is forgotten. How many leases
    /// it held.
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
        released
    }

    /// The builds that hold something, for the log and the tests.
    #[cfg(test)]
    pub(super) fn builds(&self) -> Vec<u64> {
        let state = self.state();
        let mut builds: Vec<u64> =
            state.held.keys().map(|it| it.0).chain(state.judged.keys().map(|it| it.0)).collect();
        builds.sort();
        builds.dedup();
        builds
    }

    /// Waits until no build of another process holds the run directory, for at most the
    /// deadline. It takes the lease and gives it back: the run that follows takes it again
    /// under the transaction lock.
    pub(super) fn wait(run_dir: &Path, deadline: Duration) -> Result<(), String> {
        let file = open(run_dir).map_err(|it| match it {
            Refused::Io(err) => err,
            Refused::Busy(busy) => busy.to_string(),
        })?;
        let began = Instant::now();
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
            if began.elapsed() >= deadline {
                return Err(format!(
                    "the output of the build script in {} stayed in use by another worker for {:?}",
                    run_dir.display(),
                    deadline
                ));
            }
            std::thread::sleep(pause);
            pause = (pause * 2).min(Duration::from_millis(250));
        }
    }
}

/// How long [`Leases::wait`] is given.
pub(super) fn deadline() -> Duration {
    std::env::var("RA_JIT_SCRIPT_LEASE_WAIT_SECS")
        .ok()
        .and_then(|it| it.parse().ok())
        .map_or(WAIT, Duration::from_secs)
}

fn open(run_dir: &Path) -> Result<File, Refused> {
    std::fs::create_dir_all(run_dir)
        .map_err(|it| Refused::Io(format!("{}: {it}", run_dir.display())))?;
    let path = run_dir.join(FILE);
    File::options()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)
        .map_err(|it: io::Error| Refused::Io(format!("{}: {it}", path.display())))
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
    /// The script ran and stands: the build keeps the output it wrote. The caller holds the
    /// transaction lock, so no other writer is between giving up the lease and taking it shared.
    pub(super) fn stands(mut self) -> Result<(), Refused> {
        if let Some(file) = self.file.take() {
            let _ = file.unlock();
        }
        self.leases.stands(self.build, &self.run_dir)
    }

    /// The script failed: the build fails each request of it with `why`, and holds nothing.
    pub(super) fn failed(mut self, why: String) {
        if let Some(file) = self.file.take() {
            let _ = file.unlock();
        }
        if self.build != 0 {
            self.leases.state().judged.insert((self.build, self.run_dir.clone()), Some(why));
        }
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
    use super::*;

    fn dir(test: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("ra-jit-leases-{test}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn busy(refused: Result<Writer<'_>, Refused>) -> Busy {
        match refused {
            Err(Refused::Busy(busy)) => busy,
            Err(Refused::Io(err)) => panic!("{err}"),
            Ok(_) => panic!("it was given the lease"),
        }
    }

    #[test]
    fn what_a_build_judged_survives_the_builds_between_its_requests() {
        let run = dir("judged");
        let leases = Leases::default();
        assert_eq!(leases.judged(1, &run), None);
        leases.writer(1, &run).unwrap().stands().unwrap();
        // Another build asks to run it: it is held for 1, so 2 is told so, and judges nothing.
        assert!(matches!(busy(leases.writer(2, &run)), Busy::Process { build: 1, .. }));
        assert_eq!(leases.judged(2, &run), None);
        assert_eq!(leases.judged(1, &run), Some(None));
        // Build 1 retires: 2 may run, and 1's judgment is gone with it.
        leases.retire(1);
        assert_eq!(leases.judged(1, &run), None);
        leases.writer(2, &run).unwrap().stands().unwrap();
        assert_eq!(leases.judged(2, &run), Some(None));
        leases.retire(2);
        assert!(leases.builds().is_empty());
        let _ = std::fs::remove_dir_all(&run);
    }

    #[test]
    fn a_failure_is_kept_for_the_build_and_holds_nothing() {
        let run = dir("failed");
        let leases = Leases::default();
        leases.writer(1, &run).unwrap().failed("exit status: 1".to_owned());
        assert_eq!(leases.judged(1, &run), Some(Some("exit status: 1".to_owned())));
        // Nothing reads what it wrote, so another build runs it, and 1 still fails.
        leases.writer(2, &run).unwrap().stands().unwrap();
        assert_eq!(leases.judged(1, &run), Some(Some("exit status: 1".to_owned())));
        assert_eq!(leases.judged(2, &run), Some(None));
        leases.retire(1);
        assert_eq!(leases.judged(1, &run), None);
        let _ = std::fs::remove_dir_all(&run);
    }

    #[test]
    fn builds_that_only_read_hold_one_output_together() {
        let run = dir("shared");
        let leases = Leases::default();
        leases.stands(1, &run).unwrap();
        leases.stands(2, &run).unwrap();
        leases.stands(1, &run).unwrap();
        assert_eq!(leases.builds(), [1, 2]);
        // A writer waits for both.
        assert!(matches!(busy(leases.writer(3, &run)), Busy::Process { .. }));
        leases.retire(1);
        assert!(matches!(busy(leases.writer(3, &run)), Busy::Process { build: 2, .. }));
        leases.retire(2);
        leases.writer(3, &run).unwrap().stands().unwrap();
        let _ = std::fs::remove_dir_all(&run);
    }

    #[test]
    fn a_build_without_a_number_holds_and_keeps_nothing() {
        let run = dir("anonymous");
        let leases = Leases::default();
        leases.writer(0, &run).unwrap().stands().unwrap();
        assert_eq!(leases.judged(0, &run), None);
        leases.writer(0, &run).unwrap().failed("no".to_owned());
        assert_eq!(leases.judged(0, &run), None);
        // Nothing is held, so any build may write.
        leases.writer(1, &run).unwrap();
        let _ = std::fs::remove_dir_all(&run);
    }

    #[test]
    fn another_process_s_lease_is_waited_for_not_mistaken_for_this_ones() {
        // Two sets of leases stand for two workers: `flock` conflicts between descriptions,
        // which is what it does between processes.
        let run = dir("elsewhere");
        let (first, second) = (Leases::default(), Leases::default());
        first.writer(1, &run).unwrap().stands().unwrap();
        assert!(matches!(busy(second.writer(7, &run)), Busy::Elsewhere { .. }));
        assert!(Leases::wait(&run, Duration::from_millis(60)).is_err());
        let waited = {
            let run = run.clone();
            std::thread::spawn(move || Leases::wait(&run, Duration::from_secs(10)))
        };
        std::thread::sleep(Duration::from_millis(50));
        first.retire(1);
        waited.join().unwrap().unwrap();
        second.writer(7, &run).unwrap().stands().unwrap();
        // And a reader of the other worker's lease is not a writer's to take over.
        assert!(matches!(busy(first.writer(2, &run)), Busy::Elsewhere { .. }));
        let _ = std::fs::remove_dir_all(&run);
    }
}
