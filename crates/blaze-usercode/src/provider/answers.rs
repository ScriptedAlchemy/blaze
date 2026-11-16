//! The sandbox randomness state machine. Provider construction hashes the run key at
//! the edge; this core receives its seed and never performs CPU discovery or other IO.

#![blaze::sans_io]
#![deny(blaze::sans_io_core)]

use std::alloc::Global;

use blaze_hash::HashMap;

use super::{Provider, Seed, derive};

/// The most opens of a random device one process is answered: each takes an index of the
/// process's 2^24.
pub const DEVICE_OPENS_MOST: u64 = 1 << 24;

/// What the sandbox answers the processes of one run that ask the kernel for randomness: a
/// state machine, with no IO and no clock.
///
/// Each process has a stream of its own, a function of the seed and of the process's number
/// alone, and is given the stream's next bytes each time it asks. What a process is given
/// therefore does not depend on when another process asked, in which order the supervisor
/// saw them, or how the process cut its reads.
///
/// The reference model: [`Answers::model`], which says what the `n`th byte a process is given
/// is with no state at all.
#[derive(Debug)]
pub struct Answers {
    seed: Seed,
    /// How many bytes of its `getrandom` stream each process has been given.
    taken: HashMap<u64, u64, Global>,
    /// How many opens of a random device each process has been answered.
    opened: HashMap<u64, u64, Global>,
    /// Test-only: the bug to plant.
    plant: Plant,
}

/// A bug [`Answers`] can be made to have, for its tests' controls.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Plant {
    #[default]
    Nothing,
    /// One count for all processes: what a process is given depends on who asked before it.
    SharedCount,
}

impl Answers {
    pub fn new(provider: &Provider) -> Answers {
        Answers::planted(provider, Plant::Nothing)
    }

    /// [`Answers::new`] with a bug planted: for tests' controls only.
    pub fn planted(provider: &Provider, plant: Plant) -> Answers {
        Answers {
            seed: provider.seed,
            taken: HashMap::new_in(Global),
            opened: HashMap::new_in(Global),
            plant,
        }
    }

    /// Answers a `getrandom` of `process` (its number in the run: its process id in the
    /// sandbox's namespace): the next `into.len()` bytes of its stream.
    pub fn random(&mut self, process: u64, into: &mut [u8]) {
        let counted = match self.plant {
            Plant::Nothing => process,
            Plant::SharedCount => 0,
        };
        // alloc: one entry per process that asks, for the run
        let taken = self.taken.entry(counted).or_insert(0);
        derive::fill(&self.seed, derive::SYSTEM, process, *taken, into);
        *taken = taken.wrapping_add(into.len() as u64);
    }

    /// Answers an open of `/dev/urandom` or `/dev/random` by `process`: the bytes the file it
    /// is handed holds, `into.len()` of them, another stream for each open. `None` once the
    /// process has opened [`DEVICE_OPENS_MOST`]: then the open is not answered.
    pub fn device(&mut self, process: u64, into: &mut [u8]) -> Option<()> {
        // alloc: one entry per process that opens one, for the run
        let opened = self.opened.entry(process).or_insert(0);
        if *opened >= DEVICE_OPENS_MOST {
            return None;
        }
        let stream = process.wrapping_mul(DEVICE_OPENS_MOST).wrapping_add(*opened);
        *opened += 1;
        derive::fill(&self.seed, derive::DEVICE, stream, 0, into);
        Some(())
    }

    /// The reference model: the byte at `offset` of what `process` is given by `getrandom`,
    /// whatever anyone asked before.
    pub fn model(seed: &Seed, process: u64, offset: u64) -> u8 {
        derive::word(seed, derive::SYSTEM, process, offset / 8).to_le_bytes()[(offset % 8) as usize]
    }
}
