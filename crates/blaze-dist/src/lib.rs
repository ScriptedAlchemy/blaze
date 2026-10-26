//! blaze's distributed compile and jobs as sans-IO state machines, and the deterministic
//! simulation that checks them ([docs/distributed.md](../../docs/distributed.md), sections 4
//! and 5).
//!
//! - [`work`]: the coordinator's work set of one execution, claimed discovery "(b′)": the root
//!   agreement, grants under leases, commits of batch results, fencing, finders and the lead's
//!   tail.
//! - [`holder`]: an executor's side of the work calls: join, lease, batch, result, retries,
//!   retained findings, `Lost` answers.
//! - [`jobs`]: executions of jobs (section 4, "Warm remote jobs"; unit E3): one execution per
//!   `JobContent`, subscribers with their own deadlines and policies, attempts, reassignment
//!   once, events with replay, cancellation when the last subscriber leaves.
//! - [`admission`]: section 5's admission: capacities, reservations, `Main` first, a bounded
//!   queue, trust matching, rebuilds by drain, stage timestamps.
//! - [`sim`]: the deterministic simulation: a seeded scheduler and network that loses,
//!   reorders, duplicates and partitions; executors that die at any step and clocks that skew;
//!   and the invariants it checks after every step and at the end.
//!
//! Nothing here does IO, starts a thread or reads a clock: every machine takes events and `now`
//! and returns its answers and effects. Every message is a `blaze-wire` type.

#![deny(missing_docs)]

pub mod admission;
pub mod holder;
pub mod jobs;
mod ranges;
mod refusal;
pub mod sim;
pub mod work;

pub use ranges::Ranges;
pub use refusal::Refusal;

/// A clock reading in milliseconds. The machines never read a clock; each call is given one.
pub type Millis = u64;
