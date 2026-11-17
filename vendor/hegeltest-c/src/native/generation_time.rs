//! The union of generator intervals in one test-case family, excluding its body.

use core::time::Duration;

use crate::sys::{Instant, sync::Mutex};

#[derive(Default)]
struct Intervals {
    active: usize,
    start: Duration,
    total: Duration,
}

impl Intervals {
    fn enter(&mut self, now: Duration) -> Result<(), &'static str> {
        let active = self.active.checked_add(1).ok_or("too many active generation scopes")?;
        if self.active == 0 {
            self.start = now;
        }
        self.active = active;
        Ok(())
    }

    fn exit(&mut self, now: Duration) -> Result<(), &'static str> {
        let active = self.active.checked_sub(1).ok_or("generation scope closed without opening")?;
        if active == 0 {
            self.total = self.elapsed(now);
        }
        self.active = active;
        Ok(())
    }

    fn elapsed(&self, now: Duration) -> Duration {
        if self.active == 0 {
            self.total
        } else {
            self.total.saturating_add(now.saturating_sub(self.start))
        }
    }
}

struct Timed {
    origin: Option<Instant>,
    intervals: Intervals,
    #[cfg(test)]
    clock: Option<Duration>,
}

impl Timed {
    fn now(&self) -> Duration {
        #[cfg(test)]
        if let Some(now) = self.clock {
            return now;
        }
        self.origin.map_or(Duration::ZERO, |start| start.elapsed())
    }
}

pub(crate) struct GenerationTime(Mutex<Timed>);

impl Default for GenerationTime {
    fn default() -> Self {
        Self(Mutex::new(Timed {
            origin: Instant::now(),
            intervals: Intervals::default(),
            #[cfg(test)]
            clock: None,
        }))
    }
}

impl GenerationTime {
    pub(crate) fn enter(&self) -> Result<(), &'static str> {
        let mut timed = self.0.lock();
        let now = timed.now();
        timed.intervals.enter(now)
    }

    pub(crate) fn exit(&self) -> Result<(), &'static str> {
        let mut timed = self.0.lock();
        let now = timed.now();
        timed.intervals.exit(now)
    }

    pub(crate) fn elapsed(&self) -> Duration {
        let timed = self.0.lock();
        timed.intervals.elapsed(timed.now())
    }

    #[cfg(test)]
    pub(crate) fn active(&self) -> usize {
        self.0.lock().intervals.active
    }

    #[cfg(test)]
    pub(crate) fn set_time(&self, now: Duration) {
        let mut timed = self.0.lock();
        assert!(timed.clock.is_none_or(|before| now >= before));
        timed.clock = Some(now);
    }

    pub(crate) fn scope(&self) -> Result<Scope<'_>, &'static str> {
        self.enter()?;
        Ok(Scope(self))
    }
}

pub(crate) struct Scope<'a>(&'a GenerationTime);

impl Drop for Scope<'_> {
    fn drop(&mut self) {
        self.0.exit().expect("a native operation closes its own generation scope");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec::Vec;

    fn agrees(ops: &[bool], double_charge: bool) -> bool {
        let mut actual = Intervals::default();
        let mut active = 0usize;
        let mut expected = Duration::ZERO;
        let mut excess = Duration::ZERO;
        let mut now = Duration::ZERO;
        for &enter in ops {
            let gap = Duration::from_secs(7);
            if active != 0 {
                expected += gap;
            }
            if double_charge && active > 1 {
                excess += gap;
            }
            now += gap;
            if enter {
                actual.enter(now).unwrap();
                active += 1;
            } else {
                let result = actual.exit(now);
                if active == 0 {
                    if result.is_ok() {
                        return false;
                    }
                } else {
                    result.unwrap();
                    active -= 1;
                }
            }
            if actual.elapsed(now) + excess != expected {
                return false;
            }
        }
        true
    }

    /// The invariant: nested and concurrent generator intervals are charged once, and
    /// body-only gaps are never charged, for every scope schedule through depth seven.
    #[test]
    fn generation_intervals_match_the_union_model() {
        fn visit(ops: &mut Vec<bool>, depth: usize) {
            assert!(agrees(ops, false), "scope schedule {ops:?}");
            if depth != 0 {
                for enter in [false, true] {
                    ops.push(enter);
                    visit(ops, depth - 1);
                    ops.pop();
                }
            }
        }
        visit(&mut Vec::new(), 7);
    }

    #[test]
    fn the_oracle_catches_double_charged_nested_intervals() {
        assert!(!agrees(&[true, true, false, false], true));
    }

    #[test]
    fn a_scope_unwinding_closes_its_interval() {
        let time = GenerationTime::default();
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _scope = time.scope().unwrap();
            panic!("generator rejected or panicked");
        }));
        let timed = time.0.lock();
        assert_eq!(timed.intervals.active, 0);
    }
}
