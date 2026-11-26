//! The Linux program world's driver. A world that submits no effects never starts SQPOLL.

use std::alloc::Global;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::wait::{Deadline, Monotonic};
use crate::{Boot, Completion, Done, Driver, Effect, IoError, NoBoot, Os};

use super::Uring;

enum State {
    Idle,
    Active(Box<Uring>),
    Refused { why: IoError, ready: Vec<Completion> },
}

/// Opens the same io_uring backend on the first queued effect.
pub struct DeferredUring {
    root: Arc<Path>,
    start: Instant,
    boot: Result<Boot, NoBoot>,
    state: State,
}

impl DeferredUring {
    /// Acquires the world's clock and boot facts without creating a ring.
    pub fn new(_os: &Os, root: &Path) -> Self {
        Self { root: Arc::from(root), start: Instant::now(), boot: crate::boot::system(), state: State::Idle }
    }

    /// The same monotonic clock before and after the ring is opened.
    pub fn clock(&self) -> Monotonic {
        Monotonic::since(self.start)
    }
}

impl Driver for DeferredUring {
    fn submit(&mut self, effects: &mut Vec<Effect>) {
        if effects.is_empty() {
            return;
        }
        if matches!(self.state, State::Idle) {
            self.state = match Uring::open_ring(&self.root, self.start, self.boot.clone()) {
                Ok(ring) => State::Active(Box::new_in(ring, Global)),
                Err(error) => State::Refused { why: IoError(error.to_string()), ready: Vec::new_in(Global) },
            };
        }
        match &mut self.state {
            State::Active(ring) => ring.submit(effects),
            State::Refused { why, ready } => ready.extend(effects.drain(..).map(|effect| Completion {
                tag: effect.tag(),
                done: Done::Failed(why.clone()),
            })),
            State::Idle => unreachable!("the first submission sets the ring's state"),
        }
    }

    fn poll(&mut self, out: &mut Vec<Completion>, until: Deadline) -> usize {
        match &mut self.state {
            State::Idle => 0,
            State::Active(ring) => ring.poll(out, until),
            State::Refused { ready, .. } => {
                let count = ready.len();
                out.append(ready);
                count
            }
        }
    }

    fn in_flight(&self) -> usize {
        match &self.state {
            State::Idle => 0,
            State::Active(ring) => ring.in_flight(),
            State::Refused { ready, .. } => ready.len(),
        }
    }

    fn now(&self) -> Duration {
        self.start.elapsed()
    }

    fn wall_ns(&self) -> u64 {
        crate::system_wall_ns()
    }

    fn boot(&self) -> Result<Boot, NoBoot> {
        self.boot.clone()
    }

    fn confined_files(&self) -> Option<crate::fs::ConfinedFiles> {
        Some(crate::fs::ConfinedFiles::new(&self.root))
    }
}
