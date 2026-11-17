//! The deterministic simulation of the work protocol and of jobs and admission.
//!
//! One thread, one seeded scheduler and a virtual clock (roadmap.md, "A sans-IO core"): every
//! message, timer and compile completion is an event in one queue ordered by time and then by
//! the order it was made, so a seed replays exactly. The scheduler loses, duplicates, delays
//! (and so reorders) messages and partitions executors until a fault horizon, after which the
//! network delivers everything; executors die at any step, clocks run fast and slow, front ends
//! differ, findings are dropped. A checker beside the machines holds what the coordinator must
//! never do, from what it can see on the wire, and fails the run with the seed that replays it.
//!
//! - [`work`]: the coordinator's work set inside an execution, and two to four executors
//!   running [`crate::holder::Holder`] over a [`program::Program`].
//! - [`jobs`]: the server's executions and admission under a random load of jobs.

pub mod jobs;
pub mod program;
mod rng;
pub mod work;

pub use rng::Rng;

use crate::Millis;

/// An invariant a run broke: which, when, and what the checker saw.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Violation {
    /// The seed that replays the run.
    pub seed: u64,
    /// The invariant, as the module names it.
    pub invariant: &'static str,
    /// The simulated time it was found at.
    pub at: Millis,
    /// What was seen.
    pub detail: String,
}

impl std::fmt::Display for Violation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "seed {}: {} broken at {} ms: {}",
            self.seed, self.invariant, self.at, self.detail
        )
    }
}
