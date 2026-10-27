//! Soft process budgets and admission under host memory pressure.
//!
//! One monitor samples independently of compile jobs. Budgets reduce concurrency and purge
//! freed pages; only persistent critical host pressure can fail admission. In-flight jobs
//! finish: this is not a bound on the allocation of a single function.
use std::{
    cell::RefCell,
    collections::VecDeque,
    sync::{
        Arc, Condvar, LazyLock, Mutex, OnceLock,
        atomic::{AtomicBool, Ordering::Relaxed},
    },
    time::{Duration, Instant},
};

#[derive(Clone, Copy)]
pub struct Budget {
    pub most: u64,
    pub held: fn() -> Option<u64>,
    pub give_back: fn(),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pressure {
    Normal,
    Warning,
    Critical,
}

struct Monitor {
    budget: Option<Budget>,
    pressure: fn() -> Option<Pressure>,
    grace: Duration,
    threads: usize,
    state: Mutex<State>,
    changed: Condvar,
}

/// A fake clock and signal exercise the same transitions as the monitor thread.
struct State {
    pressure: Pressure,
    known_pressure: bool,
    held: Option<u64>,
    stalled: Duration,
    progress: usize,
    failed: bool,
    told_pressure: bool,
    told_held: bool,
    paused: bool,
    active: usize,
    completed: usize,
    next_ticket: u64,
    queue: VecDeque<u64>,
}
impl Default for State {
    fn default() -> Self {
        Self {
            pressure: Pressure::Normal,
            known_pressure: false,
            held: None,
            stalled: Duration::ZERO,
            progress: 0,
            failed: false,
            told_pressure: false,
            told_held: false,
            paused: false,
            active: 0,
            completed: 0,
            next_ticket: 0,
            queue: VecDeque::new(),
        }
    }
}
impl State {
    fn sample(
        &mut self,
        pressure: Option<Pressure>,
        held: Option<u64>,
        now: Duration,
        progress: usize,
        grace: Duration,
        has_budget: bool,
    ) {
        if pressure.is_none() && !self.told_pressure {
            eprintln!("host memory pressure unavailable: continuing with no pressure known");
            self.told_pressure = true;
        }
        if has_budget && held.is_none() && !self.told_held {
            eprintln!("build footprint unavailable: continuing without soft budget adaptation");
            self.told_held = true;
        }
        self.known_pressure = pressure.is_some();
        let pressure = pressure.unwrap_or(Pressure::Normal);
        if pressure != self.pressure {
            eprintln!(
                "host memory pressure: {pressure:?}{}",
                if pressure == Pressure::Critical {
                    "; pausing new compile work"
                } else {
                    "; compile admission resumed"
                }
            );
        }
        if pressure != Pressure::Critical
            || self.pressure != Pressure::Critical
            || progress != self.progress
            || self.active != 0
        {
            self.stalled = now;
        }
        self.failed = pressure == Pressure::Critical
            && self.active == 0
            && now.saturating_sub(self.stalled) >= grace;
        if pressure != Pressure::Critical {
            self.paused = false;
        }
        self.pressure = pressure;
        self.progress = progress;
        self.held = held;
        testing::sample(pressure, self.active, progress);
    }
}
static MONITOR: OnceLock<Monitor> = OnceLock::new();
static TOLD: LazyLock<bool> = LazyLock::new(|| std::env::var_os("RA_JIT_MEMORY").is_some());

/// Starts at most one monitor per process, including when the soft budget is disabled.
pub fn monitor(
    budget: Option<Budget>,
    pressure: fn() -> Option<Pressure>,
    give_back: fn(),
    grace: Duration,
) -> std::io::Result<()> {
    if MONITOR
        .set(Monitor {
            budget,
            pressure,
            grace,
            threads: std::thread::available_parallelism().map_or(1, |it| it.get()),
            state: Mutex::new(State::default()),
            changed: Condvar::new(),
        })
        .is_err()
    {
        return MONITOR
            .get()
            .unwrap()
            .admit()
            .map_err(|why| std::io::Error::other(why.to_string()));
    }
    // Publish the first signal before callers can admit work. Sampling and purging must
    // not be ordered the other way: a slow purge cannot delay the critical pause.
    let monitor = MONITOR.get().unwrap();
    monitor.state.lock().unwrap_or_else(|e| e.into_inner()).sample(
        (monitor.pressure)(),
        monitor.budget.and_then(|it| (it.held)()),
        Duration::ZERO,
        0,
        grace,
        monitor.budget.is_some(),
    );
    let run = move || {
        let monitor = MONITOR.get().unwrap();
        let start = Instant::now();
        let mut purged = None;
        let mut logged = Duration::ZERO;
        loop {
            let now = start.elapsed();
            let pressure = (monitor.pressure)();
            let held = monitor.budget.and_then(|it| (it.held)());
            let warn = pressure.is_some_and(|it| it != Pressure::Normal);
            let mut state = monitor.state.lock().unwrap_or_else(|e| e.into_inner());
            let progress = super::compiled_functions().saturating_add(state.completed);
            state.sample(pressure, held, now, progress, monitor.grace, monitor.budget.is_some());
            if state.pressure == Pressure::Critical
                && now.saturating_sub(logged) >= Duration::from_secs(5)
            {
                eprintln!(
                    "host memory pressure: still critical; {} jobs in flight, {} compile steps completed",
                    state.active, state.progress
                );
                logged = now;
            }
            monitor.changed.notify_all();
            drop(state);
            if let Some(budget) = monitor.budget {
                if (warn || held.is_some_and(|it| it > budget.most - budget.most / 4))
                    && purged.is_none_or(|at| now.saturating_sub(at) >= Duration::from_secs(1))
                {
                    (budget.give_back)();
                    purged = Some(now);
                    if *TOLD {
                        eprintln!("build budget: purged freed pages");
                    }
                }
            } else if warn
                && purged.is_none_or(|at| now.saturating_sub(at) >= Duration::from_secs(1))
            {
                give_back();
                purged = Some(now);
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    };
    if let Err(why) = std::thread::Builder::new().name("memory-pressure".into()).spawn(run) {
        eprintln!("host memory pressure monitor could not start: {why}");
        return Err(why);
    }
    // The caller starts loading its workspace after installing the monitor. That is
    // new work too: an already-critical host must pause before those allocations begin.
    monitor.admit().map_err(|why| std::io::Error::other(why.to_string()))
}

/// The daemon reads the same independent sampler as compilation, including with budgets off.
pub fn host_pressure() -> Option<Pressure> {
    MONITOR.get().and_then(|monitor| {
        let state = monitor.state.lock().unwrap_or_else(|e| e.into_inner());
        state.known_pressure.then_some(state.pressure)
    })
}

pub(super) fn budget() -> Option<Budget> {
    MONITOR.get().and_then(|it| it.budget)
}

/// A transient failure is returned outside salsa queries so recovery can retry the build.
pub(super) fn admit() -> super::Result<()> {
    if IN_JOB.with(|job| job.borrow().is_some()) {
        return Ok(());
    }
    let Some(monitor) = MONITOR.get() else { return Ok(()) };
    monitor.admit()
}

impl Monitor {
    fn admit(&self) -> super::Result<()> {
        self.wait(false)
    }

    fn limit(&self, state: &State) -> usize {
        if state.pressure != Pressure::Normal {
            return 1;
        }
        match (self.budget, state.held) {
            (Some(budget), Some(held)) => allowed(held, budget.most, self.threads),
            _ => self.threads,
        }
    }

    fn wait(&self, acquire: bool) -> super::Result<()> {
        let entered = Instant::now();
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let ticket = acquire.then(|| {
            let ticket = state.next_ticket;
            state.next_ticket += 1;
            state.queue.push_back(ticket);
            ticket
        });
        while state.pressure == Pressure::Critical
            || (acquire
                && (state.queue.front().copied() != ticket || state.active >= self.limit(&state)))
        {
            if state.pressure == Pressure::Critical && !state.paused {
                eprintln!(
                    "host memory pressure: compile admission paused; {} compile steps completed",
                    state.progress
                );
                state.paused = true;
            }
            if state.pressure == Pressure::Critical
                && state.failed
                && entered.elapsed() >= self.grace
            {
                if let Some(ticket) = ticket {
                    state.queue.retain(|it| *it != ticket);
                    self.changed.notify_all();
                }
                return Err(super::CodegenError::OverBudget(format!(
                    "host memory pressure remained critical for {} seconds without compile progress after pausing new work",
                    self.grace.as_secs()
                )));
            }
            state = self
                .changed
                .wait_timeout(state, Duration::from_millis(100))
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
        if acquire {
            assert_eq!(state.queue.pop_front(), ticket);
            state.active += 1;
            self.changed.notify_all();
        }
        Ok(())
    }
}

/// A job keeps its slot until its last dependent helper finishes, even on another thread.
struct Job {
    monitor: Option<&'static Monitor>,
    failed: AtomicBool,
}
impl Drop for Job {
    fn drop(&mut self) {
        if let Some(monitor) = self.monitor {
            let mut state = monitor.state.lock().unwrap_or_else(|e| e.into_inner());
            state.active -= 1;
            if !self.failed.load(Relaxed) && !std::thread::panicking() {
                state.completed = state.completed.saturating_add(1);
            }
            testing::event("job_exit");
            monitor.changed.notify_all();
        }
    }
}
/// A permit cannot migrate between threads: it restores that thread's previous context.
pub(super) struct Permit {
    job: Arc<Job>,
    previous: Option<Arc<Job>>,
    _local: std::marker::PhantomData<*mut ()>,
}
impl Drop for Permit {
    fn drop(&mut self) {
        if std::thread::panicking() {
            self.job.failed.store(true, Relaxed);
        }
        IN_JOB.with(|job| *job.borrow_mut() = self.previous.take());
    }
}
thread_local! {
    // Foreground definitions can ask other definitions while applying relocations.
    // Those are part of the same admitted job, not new work to pause mid-flight.
    static IN_JOB: RefCell<Option<Arc<Job>>> = const { RefCell::new(None) };
}
pub(super) fn enter() -> super::Result<Permit> {
    let previous = IN_JOB.with(|job| job.borrow().clone());
    let job = match &previous {
        Some(job) => job.clone(),
        None => {
            let monitor = MONITOR.get();
            if let Some(monitor) = monitor {
                monitor.wait(true)?;
            }
            Arc::new(Job { monitor, failed: AtomicBool::new(false) })
        }
    };
    IN_JOB.with(|current| *current.borrow_mut() = Some(job.clone()));
    let permit = Permit { job, previous, _local: std::marker::PhantomData };
    if permit.previous.is_none() {
        testing::entry();
    }
    Ok(permit)
}

/// Capture only dependent work of an admitted job, never an independent compile queue.
/// Helpers inherit its slot rather than block behind the parent waiting to join them.
#[derive(Clone)]
pub struct JobContext(Option<Arc<Job>>);

pub fn job_context() -> JobContext {
    JobContext(IN_JOB.with(|job| job.borrow().clone()))
}

impl JobContext {
    pub fn run<T>(&self, work: impl FnOnce() -> T) -> T {
        let Some(job) = &self.0 else { return work() };
        let previous = IN_JOB.with(|current| current.replace(Some(job.clone())));
        let _permit = Permit { job: job.clone(), previous, _local: std::marker::PhantomData };
        work()
    }
}

/// Explicit, file-controlled diagnostics for bounded pressure acceptance tests.
/// Without RA_JIT_TEST_PRESSURE_DIR there is no override, I/O or entry delay.
pub mod testing {
    use super::Pressure;
    use std::{
        path::PathBuf,
        sync::{LazyLock, Mutex},
        time::Duration,
    };

    static DIRECTORY: LazyLock<Option<PathBuf>> =
        LazyLock::new(|| std::env::var_os("RA_JIT_TEST_PRESSURE_DIR").map(PathBuf::from));
    static WRITING: Mutex<()> = Mutex::new(());

    pub fn pressure() -> Option<Option<Pressure>> {
        let dir = DIRECTORY.as_ref()?;
        Some(match std::fs::read_to_string(dir.join("pressure")).ok().as_deref().map(str::trim) {
            Some("normal") => Some(Pressure::Normal),
            Some("warning") => Some(Pressure::Warning),
            Some("critical") => Some(Pressure::Critical),
            _ => None,
        })
    }

    pub(super) fn event(text: &str) {
        let Some(dir) = DIRECTORY.as_ref() else { return };
        let _writing = WRITING.lock().unwrap_or_else(|e| e.into_inner());
        use std::io::Write;
        if let Ok(mut file) =
            std::fs::OpenOptions::new().create(true).append(true).open(dir.join("events"))
        {
            _ = writeln!(file, "{text}");
        }
    }

    pub(super) fn entry() {
        event("job_enter");
        checkpoint("admission");
    }

    pub(super) fn sample(pressure: Pressure, active: usize, progress: usize) {
        if DIRECTORY.is_some() {
            event(&format!("sample {pressure:?} active={active} completed={progress}"));
        }
    }

    pub fn checkpoint(name: &str) {
        let Some(dir) = DIRECTORY.as_ref() else { return };
        event(&format!("checkpoint {name}"));
        while std::fs::read_to_string(dir.join("hold")).ok().as_deref().map(str::trim) == Some(name)
        {
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}

pub(super) fn allowed(held: u64, most: u64, threads: usize) -> usize {
    let threads = threads.max(1);
    let soft = most - most / 4;
    if held <= soft {
        return threads;
    }
    ((threads as u128 * most.saturating_sub(held) as u128 / (most - soft).max(1) as u128) as usize)
        .clamp(1, threads)
}

pub(super) struct Governor {
    budget: Option<Budget>,
    threads: usize,
    shed: AtomicBool,
}
impl Governor {
    pub(super) fn new(budget: Option<Budget>, threads: usize) -> Self {
        Self { budget, threads, shed: AtomicBool::new(false) }
    }
    pub(super) fn allowed(&self) -> usize {
        let Some(monitor) = MONITOR.get() else { return self.threads.max(1) };
        let state = monitor.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.pressure != Pressure::Normal {
            return 1;
        }
        match (self.budget, state.held) {
            (Some(budget), Some(held)) => allowed(held, budget.most, self.threads),
            _ => self.threads.max(1),
        }
    }
    pub(super) fn shed(&self) -> bool {
        self.shed.load(Relaxed)
    }
    pub(super) fn why(&self) -> String {
        "host memory pressure remained critical without compile progress after pausing new work"
            .into()
    }
    pub(super) fn look(&self) -> bool {
        if self.shed() {
            return true;
        }
        if admit().is_err() {
            self.shed.store(true, Relaxed);
        }
        self.shed()
    }

    pub(super) fn stop(&self) {
        self.shed.store(true, Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    const GB: u64 = 1 << 30;
    #[test]
    fn monitor_startup_waits_for_critical_pressure_to_recover() {
        const CHILD: &str = "RA_JIT_TEST_PRESSURE_STARTUP";
        const TEST: &str = "budget::tests::monitor_startup_waits_for_critical_pressure_to_recover";
        // A process-wide monitor is isolated from the other tests' local monitors.
        if std::env::var_os(CHILD).is_none() {
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", TEST, "--nocapture"])
                .env(CHILD, "1")
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }
        static CRITICAL: AtomicBool = AtomicBool::new(true);
        static SAMPLED: AtomicBool = AtomicBool::new(false);
        fn pressure() -> Option<Pressure> {
            SAMPLED.store(true, Relaxed);
            Some(if CRITICAL.load(Relaxed) { Pressure::Critical } else { Pressure::Normal })
        }
        let (sent, received) = std::sync::mpsc::channel();
        let startup = std::thread::spawn(move || {
            sent.send(monitor(None, pressure, || (), Duration::from_secs(60))).unwrap();
        });
        let deadline = Instant::now() + Duration::from_secs(2);
        while !SAMPLED.load(Relaxed) {
            assert!(Instant::now() < deadline, "startup did not sample host pressure");
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(received.recv_timeout(Duration::from_millis(50)).is_err());
        CRITICAL.store(false, Relaxed);
        assert!(received.recv_timeout(Duration::from_secs(2)).unwrap().is_ok());
        startup.join().unwrap();
    }

    #[test]
    fn soft_budget_always_allows_progress_even_on_one_core() {
        for threads in [1, 18] {
            assert_eq!(allowed(12 * GB, 16 * GB, threads), threads);
            assert_eq!(allowed(40 * GB, 16 * GB, threads), 1);
        }
        assert_eq!(allowed(14 * GB, 16 * GB, 18), 9);
    }
    #[test]
    fn warning_critical_progress_failure_and_recovery() {
        let mut state = State::default();
        let mut tick = |pressure, seconds, progress| {
            state.sample(
                Some(pressure),
                Some(100 * GB),
                Duration::from_secs(seconds),
                progress,
                Duration::from_secs(60),
                true,
            );
            (state.pressure, state.failed)
        };
        assert_eq!(tick(Pressure::Warning, 0, 0), (Pressure::Warning, false));
        assert_eq!(tick(Pressure::Critical, 1, 0), (Pressure::Critical, false));
        assert_eq!(tick(Pressure::Critical, 59, 1), (Pressure::Critical, false));
        assert_eq!(tick(Pressure::Critical, 118, 1), (Pressure::Critical, false));
        assert_eq!(tick(Pressure::Critical, 119, 1), (Pressure::Critical, true));
        assert_eq!(tick(Pressure::Normal, 120, 1), (Pressure::Normal, false));
    }
    #[test]
    fn unavailable_signals_are_observable_once_and_unknown() {
        let mut state = State::default();
        for _ in 0..2 {
            state.sample(None, None, Duration::ZERO, 0, Duration::from_secs(60), true);
            assert!(state.told_pressure && state.told_held);
            assert!(!state.known_pressure);
            assert_eq!(state.pressure, Pressure::Normal);
            assert!(!state.failed);
        }
    }
    #[test]
    fn foreground_one_core_waits_for_independent_signal_and_recovers() {
        use std::sync::{Arc, mpsc};
        let monitor = Arc::new(Monitor {
            budget: None,
            pressure: || Some(Pressure::Critical),
            grace: Duration::from_secs(60),
            threads: 1,
            state: Mutex::new(State::default()),
            changed: Condvar::new(),
        });
        monitor.state.lock().unwrap().sample(
            Some(Pressure::Critical),
            None,
            Duration::ZERO,
            0,
            monitor.grace,
            false,
        );
        let (sent, received) = mpsc::channel();
        let worker = monitor.clone();
        // Exactly one foreground compiler, with no parallel callbacks to drive sampling.
        let thread = std::thread::spawn(move || {
            sent.send(worker.admit()).unwrap();
        });
        assert!(received.recv_timeout(Duration::from_millis(50)).is_err());
        monitor.state.lock().unwrap().sample(
            Some(Pressure::Normal),
            None,
            Duration::from_secs(1),
            0,
            monitor.grace,
            false,
        );
        monitor.changed.notify_all();
        assert!(received.recv_timeout(Duration::from_secs(2)).unwrap().is_ok());
        thread.join().unwrap();
    }

    #[test]
    fn persistent_critical_fails_admission_but_recovery_retries() {
        let monitor = Monitor {
            budget: None,
            pressure: || None,
            grace: Duration::ZERO,
            threads: 1,
            state: Mutex::new(State::default()),
            changed: Condvar::new(),
        };
        let sample = |pressure| {
            monitor.state.lock().unwrap().sample(
                Some(pressure),
                None,
                Duration::ZERO,
                0,
                Duration::ZERO,
                false,
            )
        };
        sample(Pressure::Critical);
        assert!(monitor.admit().unwrap_err().to_string().contains("host memory pressure"));
        sample(Pressure::Normal);
        assert!(monitor.admit().is_ok());
    }

    #[test]
    fn soft_limit_blocks_extra_jobs_and_recovers_without_failure() {
        use std::sync::{Arc, mpsc};
        let monitor = Arc::new(Monitor {
            budget: Some(Budget { most: 16 * GB, held: || Some(32 * GB), give_back: || () }),
            pressure: || Some(Pressure::Normal),
            grace: Duration::from_secs(60),
            threads: 18,
            state: Mutex::new(State::default()),
            changed: Condvar::new(),
        });
        monitor.state.lock().unwrap().held = Some(32 * GB);
        monitor.wait(true).unwrap(); // One job can always progress above the soft target.
        let worker = monitor.clone();
        let (sent, received) = mpsc::channel();
        let thread = std::thread::spawn(move || sent.send(worker.wait(true)).unwrap());
        assert!(received.recv_timeout(Duration::from_millis(50)).is_err());
        {
            let mut state = monitor.state.lock().unwrap();
            assert!(!state.failed);
            state.held = Some(8 * GB);
        }
        monitor.changed.notify_all();
        assert!(received.recv_timeout(Duration::from_secs(2)).unwrap().is_ok());
        thread.join().unwrap();
        assert_eq!(monitor.state.lock().unwrap().active, 2);
    }

    #[test]
    fn one_core_governor_has_an_admission_gate_without_parallel_callbacks() {
        let governor = Governor::new(None, 1);
        assert!(!governor.look());
        assert_eq!(governor.allowed(), 1);
        assert!(!governor.shed());
    }

    #[test]
    fn critical_pressure_never_fails_while_a_job_is_in_flight() {
        let mut state = State { active: 1, ..State::default() };
        let grace = Duration::from_secs(60);
        state.sample(Some(Pressure::Critical), None, Duration::ZERO, 0, grace, false);
        state.sample(Some(Pressure::Critical), None, grace * 3, 0, grace, false);
        assert!(!state.failed);
        state.active = 0;
        state.completed += 1;
        state.sample(Some(Pressure::Critical), None, grace * 3, 1, grace, false);
        state.sample(
            Some(Pressure::Critical),
            None,
            grace * 4 - Duration::from_millis(1),
            1,
            grace,
            false,
        );
        assert!(!state.failed);
        state.sample(Some(Pressure::Critical), None, grace * 4, 1, grace, false);
        assert!(state.failed);
    }

    #[test]
    fn production_monitor_fair_jobs_helpers_failure_and_recovery() {
        const TEST: &str =
            "budget::tests::production_monitor_fair_jobs_helpers_failure_and_recovery";
        const CHILD: &str = "RA_JIT_TEST_FAIR_PRESSURE";
        if std::env::var_os(CHILD).is_none() {
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", TEST, "--nocapture"])
                .env(CHILD, "1")
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }
        use std::sync::{atomic::AtomicUsize, mpsc};
        static SIGNAL: AtomicUsize = AtomicUsize::new(0);
        fn pressure() -> Option<Pressure> {
            match SIGNAL.load(Relaxed) {
                0 => Some(Pressure::Normal),
                1 => Some(Pressure::Critical),
                _ => None,
            }
        }
        monitor(
            Some(Budget { most: 1, held: || Some(100), give_back: || {} }),
            pressure,
            || {},
            Duration::from_millis(100),
        )
        .unwrap();
        let controller = MONITOR.get().unwrap();
        let until = |condition: &dyn Fn() -> bool| {
            let deadline = Instant::now() + Duration::from_secs(3);
            while !condition() {
                assert!(Instant::now() < deadline, "controller did not reach expected state");
                std::thread::sleep(Duration::from_millis(1));
            }
        };
        let parent = enter().unwrap();
        let context = job_context();
        let (started, ready) = mpsc::channel();
        let (relief, released) = mpsc::channel();
        let helper = std::thread::spawn(move || {
            context.run(|| {
                let _nested = enter().unwrap();
                started.send(()).unwrap();
                released.recv_timeout(Duration::from_secs(3)).unwrap();
            })
        });
        ready.recv_timeout(Duration::from_secs(3)).unwrap();
        drop(parent);
        assert_eq!(controller.state.lock().unwrap().active, 1, "helper owns parent slot");
        SIGNAL.store(1, Relaxed);
        until(&|| host_pressure() == Some(Pressure::Critical));
        std::thread::sleep(Duration::from_millis(150));
        assert!(!controller.state.lock().unwrap().failed, "active helper is not timed out");
        relief.send(()).unwrap();
        helper.join().unwrap();
        let governor = Governor::new(budget(), 1);
        assert!(governor.look(), "only drained critical work fails after grace");
        assert!(governor.shed());
        assert_eq!(controller.state.lock().unwrap().active, 0);
        SIGNAL.store(2, Relaxed);
        until(&|| host_pressure().is_none());
        assert!(!controller.state.lock().unwrap().failed);
        // A new build retries with the same process monitor; a stopped build's governor
        // remains terminal so it cannot publish a partial result after relief.
        let fresh = Governor::new(budget(), 1);
        assert!(!fresh.look());
        assert!(governor.shed());
        let blocker = enter().unwrap();
        let (sent, received) = mpsc::channel();
        let mut threads = Vec::new();
        for index in 0..6 {
            let sent = sent.clone();
            threads.push(std::thread::spawn(move || {
                let _permit = enter().unwrap();
                sent.send(index).unwrap();
            }));
            until(&|| controller.state.lock().unwrap().queue.len() == index + 1);
        }
        drop(blocker);
        for index in 0..6 {
            assert_eq!(received.recv_timeout(Duration::from_secs(3)).unwrap(), index);
        }
        for thread in threads {
            thread.join().unwrap();
        }
        assert_eq!(controller.state.lock().unwrap().active, 0);
        assert!(controller.state.lock().unwrap().queue.is_empty());
        SIGNAL.store(0, Relaxed);
        until(&|| host_pressure() == Some(Pressure::Normal));
        let completed = controller.state.lock().unwrap().completed;
        let parent = enter().unwrap();
        let context = job_context();
        let helper = std::thread::spawn(move || {
            std::panic::catch_unwind(|| {
                context.run(|| {
                    let _nested = enter().unwrap();
                    panic!("dependent work unwinds");
                })
            })
            .is_err()
        });
        assert!(helper.join().unwrap());
        drop(parent);
        assert_eq!(controller.state.lock().unwrap().active, 0);
        assert_eq!(
            controller.state.lock().unwrap().completed,
            completed,
            "an unwound helper must not count as compile progress"
        );
    }
}
