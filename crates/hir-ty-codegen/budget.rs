//! The memory a build may hold while every core compiles ahead of the object.
//!
//! Inside one revision of the database nothing a build computed can be let go of: every memo
//! stays until the next revision, and the build holds references into them until the object is
//! made. So what a build holds grows with every function it compiles (23 KB of live heap for each
//! in a cold build of tracedecay, docs/reports/LOADMEM.md), and the only bound that holds is one
//! that stops the build. Before it does, [`Governor`] gives back to the system the pages the
//! allocator keeps free, and lets fewer threads compile as the build nears the bound: less is in
//! flight and the bound comes later, though no less surely, as each function compiled still adds
//! what it adds. At the bound it sheds the build: the threads stop, and the build fails with
//! [`super::CodegenError::OverBudget`] rather than the machine running out of memory. Nothing is
//! handed to rustc or to cargo instead.
//!
//! The process sets the bound once with [`set_budget`], with how to read what it holds and how to
//! give back what is freed: those are ra-jit's (`memory.rs`), which this crate does not see.

use std::{
    sync::{
        LazyLock, OnceLock,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering::Relaxed},
    },
    time::Duration,
};

/// The most a process may hold while it builds, and how it tells.
#[derive(Clone, Copy)]
pub struct Budget {
    /// Bytes.
    pub most: u64,
    /// The bytes the process holds now, as its bound counts them.
    pub held: fn() -> Option<u64>,
    /// Gives the system back the pages of what was freed.
    pub give_back: fn(),
}

static BUDGET: OnceLock<Budget> = OnceLock::new();

/// Bounds every build of the process from now on. Only the first call counts.
pub fn set_budget(budget: Budget) {
    _ = BUDGET.set(budget);
}

/// The bound [`set_budget`] set, if any.
pub(super) fn budget() -> Option<Budget> {
    BUDGET.get().copied()
}

/// How often the threads look at what the process holds.
const LOOK_EVERY: Duration = Duration::from_millis(100);

/// How often the allocator is asked to give back what it keeps: a purge walks every arena.
const GIVE_BACK_EVERY: Duration = Duration::from_secs(1);

/// The share of the bound past which fewer threads compile: one part in this many is left.
const MARGIN: u64 = 4;

/// Whether what the threads are let do is told in the log: with `RA_JIT_MEMORY`, as the other
/// lines about memory are.
static TOLD: LazyLock<bool> = LazyLock::new(|| std::env::var_os("RA_JIT_MEMORY").is_some());

/// How many of `threads` may compile while the process holds `held` of `most` bytes: all of
/// them up to three quarters of the bound, then fewer in proportion to what is left of it, and
/// always one, which the thread that puts the functions in the object waits for.
pub(super) fn allowed(held: u64, most: u64, threads: usize) -> usize {
    let soft = most - most / MARGIN;
    if held <= soft {
        return threads;
    }
    let left = most.saturating_sub(held) as u128;
    let share = (threads as u128 * left / (most - soft).max(1) as u128) as usize;
    share.clamp(1, threads)
}

/// What the threads of one build that compile ahead look at before each function.
pub(super) struct Governor {
    budget: Option<Budget>,
    threads: usize,
    allowed: AtomicUsize,
    start: std::time::Instant,
    /// Milliseconds since `start` of the next look, and of the last purge.
    next_look: AtomicU64,
    gave_back: AtomicU64,
    /// Set once the process held more than the bound after a purge: the build is shed.
    shed: AtomicBool,
    /// What it held then.
    held: AtomicU64,
}

impl Governor {
    pub(super) fn new(budget: Option<Budget>, threads: usize) -> Self {
        Governor {
            budget,
            threads,
            allowed: AtomicUsize::new(threads),
            start: std::time::Instant::now(),
            next_look: AtomicU64::new(0),
            gave_back: AtomicU64::new(0),
            shed: AtomicBool::new(false),
            held: AtomicU64::new(0),
        }
    }

    /// How many threads may compile at once now.
    pub(super) fn allowed(&self) -> usize {
        self.allowed.load(Relaxed)
    }

    /// Whether the build is shed.
    pub(super) fn shed(&self) -> bool {
        self.shed.load(Relaxed)
    }

    /// Why it was shed, for the error.
    pub(super) fn why(&self) -> String {
        let most = self.budget.map_or(0, |it| it.most);
        format!(
            "the build held {} MB, more than the {} MB the process may (RA_JIT_WORKER_MEMORY, \
             else RA_JIT_MEMORY_MAX): it was stopped before it held more",
            self.held.load(Relaxed) >> 20,
            most >> 20,
        )
    }

    /// Looks at what the process holds, when it is time to: one thread at a time, every
    /// [`LOOK_EVERY`]. Returns whether that shed the build.
    pub(super) fn look(&self) -> bool {
        let Some(budget) = self.budget else { return false };
        let now = self.start.elapsed().as_millis() as u64;
        let next = self.next_look.load(Relaxed);
        if now < next
            || self
                .next_look
                .compare_exchange(next, now + LOOK_EVERY.as_millis() as u64, Relaxed, Relaxed)
                .is_err()
        {
            return false;
        }
        let Some(mut held) = (budget.held)() else { return false };
        let soft = budget.most - budget.most / MARGIN;
        let last = self.gave_back.load(Relaxed);
        if held > soft && (last == 0 || now >= last + GIVE_BACK_EVERY.as_millis() as u64) {
            self.gave_back.store(now.max(1), Relaxed);
            (budget.give_back)();
            let before = held;
            held = (budget.held)().unwrap_or(held);
            if *TOLD {
                eprintln!("build budget: gave back {} MB to {} MB", before >> 20, held >> 20);
            }
        }
        let allowed = allowed(held, budget.most, self.threads);
        if self.allowed.swap(allowed, Relaxed) != allowed && *TOLD {
            eprintln!(
                "build budget: {allowed} of {} threads compile, the process holds {} of {} MB",
                self.threads,
                held >> 20,
                budget.most >> 20,
            );
        }
        if held > budget.most && !self.shed.swap(true, Relaxed) {
            self.held.store(held, Relaxed);
            return true;
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GB: u64 = 1 << 30;

    #[test]
    fn all_threads_up_to_three_quarters() {
        assert_eq!(allowed(0, 16 * GB, 18), 18);
        assert_eq!(allowed(12 * GB, 16 * GB, 18), 18);
    }

    #[test]
    fn fewer_threads_nearer_the_bound_and_never_none() {
        assert_eq!(allowed(14 * GB, 16 * GB, 18), 9);
        assert_eq!(allowed(15 * GB, 16 * GB, 18), 4);
        assert_eq!(allowed(16 * GB, 16 * GB, 18), 1);
        assert_eq!(allowed(40 * GB, 16 * GB, 18), 1);
        assert_eq!(allowed(15 * GB, 16 * GB, 1), 1);
    }

    static HELD: AtomicU64 = AtomicU64::new(0);
    static PURGED: AtomicUsize = AtomicUsize::new(0);

    fn held() -> Option<u64> {
        Some(HELD.load(Relaxed))
    }

    fn give_back() {
        PURGED.fetch_add(1, Relaxed);
        // What the allocator kept goes back: a gigabyte.
        HELD.fetch_sub(GB, Relaxed);
    }

    #[test]
    fn purges_then_narrows_then_sheds() {
        let budget = Budget { most: 16 * GB, held, give_back };
        let governor = Governor::new(Some(budget), 18);
        let look = || {
            // Every look is due: the clock is not what is tested.
            governor.next_look.store(0, Relaxed);
            governor.gave_back.store(0, Relaxed);
            governor.look()
        };
        HELD.store(8 * GB, Relaxed);
        assert!(!look());
        assert_eq!((governor.allowed(), PURGED.load(Relaxed)), (18, 0));
        // Over three quarters, a purge brings it to 14 GB: half the threads.
        HELD.store(15 * GB, Relaxed);
        assert!(!look());
        assert_eq!((governor.allowed(), PURGED.load(Relaxed)), (9, 1));
        // Over the bound after a purge: shed, once.
        HELD.store(18 * GB, Relaxed);
        assert!(look());
        assert!(governor.shed());
        assert_eq!(governor.allowed(), 1);
        assert!(!look());
        assert!(governor.why().contains("17408 MB, more than the 16384 MB"), "{}", governor.why());
    }

    #[test]
    fn without_a_budget_nothing_is_looked_at() {
        let governor = Governor::new(None, 4);
        assert!(!governor.look());
        assert_eq!(governor.allowed(), 4);
        assert!(!governor.shed());
    }
}
