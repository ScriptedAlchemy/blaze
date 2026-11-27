//! The Linux program world's driver. A world that submits no effects never starts SQPOLL.

use core::time::Duration;

use blaze_own::{Box, Vec};

use crate::wait::{Clock, Deadline, Monotonic};
use crate::{Boot, Completion, Done, Driver, IoError, NoBoot, Os};

use super::{Arc, CompletionQueue, DriverClock, EffectQueue, Global, Path, Uring};

enum State {
    Idle,
    Active(Box<Uring, Global>),
    Refused { why: IoError, ready: Vec<Completion, Global> },
}

/// Opens the same io_uring backend on the first queued effect.
pub struct DeferredUring {
    root: Arc<Path>,
    clock: DriverClock,
    boot: Result<Boot, NoBoot>,
    state: State,
}

impl DeferredUring {
    /// Acquires the world's clock and boot facts without creating a ring.
    pub fn new(_os: &Os, root: &Path) -> Self {
        Self { root: Arc::from(root), clock: DriverClock::new(), boot: crate::boot::system(), state: State::Idle }
    }

    /// The same monotonic clock before and after the ring is opened.
    pub fn clock(&self) -> Monotonic {
        self.clock.0
    }
}

fn refused<'a>(effects: &'a mut EffectQueue, why: &'a IoError) -> impl Iterator<Item = Completion> + 'a {
    effects.drain(..).map(move |effect| Completion { tag: effect.tag(), done: Done::Failed(IoError::from_display(why)) })
}

impl Driver for DeferredUring {
    fn submit(&mut self, effects: &mut EffectQueue) {
        if effects.is_empty() {
            return;
        }
        match &mut self.state {
            State::Active(ring) => ring.submit(effects),
            State::Refused { why, ready } => ready.extend(refused(effects, why)),
            State::Idle => {
                self.state = match Uring::open_ring(&self.root, self.clock, self.boot()) {
                    Ok(ring) => {
                        let mut ring = Box::new_in(ring, Global);
                        ring.submit(effects);
                        State::Active(ring)
                    }
                    Err(error) => {
                        let why = IoError::from_display(&error);
                        let mut ready = Vec::new_in(Global);
                        ready.extend(refused(effects, &why));
                        State::Refused { why, ready }
                    }
                };
            }
        }
    }

    fn poll(&mut self, out: &mut CompletionQueue, until: Deadline) -> usize {
        match &mut self.state {
            State::Idle => 0,
            State::Active(ring) => ring.poll(out, until),
            State::Refused { ready, .. } => {
                let count = ready.len();
                out.extend(ready.drain(..));
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
        self.clock.now()
    }

    fn wall_ns(&self) -> u64 {
        DriverClock::wall_ns()
    }

    fn boot(&self) -> Result<Boot, NoBoot> {
        self.boot.as_ref().copied().map_err(|why| NoBoot {
            source: why.source,
            why: IoError::from_display(&why.why).0,
        })
    }

    fn confined_files(&self) -> Option<crate::fs::ConfinedFiles> {
        Some(crate::fs::ConfinedFiles::new(&self.root))
    }
}
