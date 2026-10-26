//! Soft process budgets and admission under host memory pressure.
//!
//! One monitor samples independently of compile jobs. Budgets reduce concurrency and purge
//! freed pages; only persistent critical host pressure can fail admission. In-flight jobs
//! finish: this is not a bound on the allocation of a single function.
use std::{
    sync::{
        Condvar, LazyLock, Mutex, OnceLock,
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
    held: Option<u64>,
    stalled: Duration,
    progress: usize,
    failed: bool,
    told_pressure: bool,
    told_held: bool,
    paused: bool,
    active: usize,
}
impl Default for State {
    fn default() -> Self {
        Self {
            pressure: Pressure::Normal,
            held: None,
            stalled: Duration::ZERO,
            progress: 0,
            failed: false,
            told_pressure: false,
            told_held: false,
            paused: false,
            active: 0,
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
        {
            self.stalled = now;
        }
        self.failed = pressure == Pressure::Critical && now.saturating_sub(self.stalled) >= grace;
        if pressure != Pressure::Critical {
            self.paused = false;
        }
        self.pressure = pressure;
        self.progress = progress;
        self.held = held;
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
) {
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
        return;
    }
    let run = move || {
        let monitor = MONITOR.get().unwrap();
        let start = Instant::now();
        let mut purged = None;
        loop {
            let now = start.elapsed();
            let pressure = (monitor.pressure)();
            let held = monitor.budget.and_then(|it| (it.held)());
            let warn = pressure.is_some_and(|it| it != Pressure::Normal);
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
            let mut state = monitor.state.lock().unwrap_or_else(|e| e.into_inner());
            state.sample(
                pressure,
                held,
                now,
                super::compiled_functions(),
                monitor.grace,
                monitor.budget.is_some(),
            );
            monitor.changed.notify_all();
            drop(state);
            std::thread::sleep(Duration::from_millis(100));
        }
    };
    if let Err(why) = std::thread::Builder::new().name("memory-pressure".into()).spawn(run) {
        eprintln!("host memory pressure monitor could not start: {why}");
    }
}

pub(super) fn budget() -> Option<Budget> {
    MONITOR.get().and_then(|it| it.budget)
}

/// A transient failure is returned outside salsa queries so recovery can retry the build.
pub(super) fn admit() -> super::Result<()> {
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
        while state.pressure == Pressure::Critical
            || (acquire && state.active >= self.limit(&state))
        {
            if state.pressure == Pressure::Critical && !state.paused {
                eprintln!(
                    "host memory pressure: compile admission paused; {} functions completed",
                    state.progress
                );
                state.paused = true;
            }
            if state.pressure == Pressure::Critical
                && state.failed
                && entered.elapsed() >= self.grace
            {
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
            state.active += 1;
        }
        Ok(())
    }
}

/// Counts top-level jobs only: recursive salsa computations do not acquire another slot.
pub(super) struct Permit(Option<&'static Monitor>);
impl Drop for Permit {
    fn drop(&mut self) {
        if let Some(monitor) = self.0 {
            let mut state = monitor.state.lock().unwrap_or_else(|e| e.into_inner());
            state.active -= 1;
            monitor.changed.notify_all();
        }
    }
}
pub(super) fn enter() -> super::Result<Permit> {
    let monitor = MONITOR.get();
    if let Some(monitor) = monitor {
        monitor.wait(true)?;
    }
    Ok(Permit(monitor))
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
}

#[cfg(test)]
mod tests {
    use super::*;
    const GB: u64 = 1 << 30;
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
}
