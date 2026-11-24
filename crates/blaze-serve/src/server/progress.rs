//! Pure sans-IO write-progress stall tracker for streaming HTTP responses.
//!
//! A stalled socket arms an idle deadline; only writing at least one byte clears it.
//! Read wakes and zero-byte flushes never extend an armed deadline, while a slow but
//! steadily progressing response outlasts `idle` indefinitely.

#![blaze::sans_io]

/// Tracks write-stall deadlines in monotonic nanoseconds.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct WriteProgress {
    idle_ns: u64,
    armed_deadline_ns: Option<u64>,
}

impl WriteProgress {
    /// Creates a fresh tracker with stall limit `idle_ns`.
    pub(crate) const fn new(idle_ns: u64) -> Self {
        Self { idle_ns, armed_deadline_ns: None }
    }

    /// Records bytes actually written to the socket.
    ///
    /// Writing `> 0` bytes clears any armed stall deadline; `0` bytes leaves it intact.
    pub(crate) fn on_written(&mut self, bytes: usize) {
        if bytes > 0 {
            self.armed_deadline_ns = None;
        }
    }

    /// Records that a write attempt stalled (`WouldBlock` / `Again`) at `now_ns`.
    ///
    /// Arms the deadline if not already armed; preserves an existing deadline.
    pub(crate) fn on_stalled(&mut self, now_ns: u64) {
        if self.armed_deadline_ns.is_none() {
            self.armed_deadline_ns = Some(now_ns.saturating_add(self.idle_ns));
        }
    }

    /// Records an unrelated event (such as a read wake or no-op flush): does NOT reset the deadline.
    #[cfg(test)]
    pub(crate) fn on_non_progress_wake(&mut self) {}

    /// Whether an armed stall deadline has expired at `now_ns`.
    pub(crate) const fn is_expired(&self, now_ns: u64) -> bool {
        match self.armed_deadline_ns {
            Some(deadline) => now_ns >= deadline,
            None => false,
        }
    }

    /// Currently armed deadline in monotonic nanoseconds, if any.
    pub(crate) const fn deadline_ns(&self) -> Option<u64> {
        self.armed_deadline_ns
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn progressing_writes_outlast_idle_limit() {
        let mut p = WriteProgress::new(100);
        let mut now = 0u64;
        for _ in 0..10 {
            p.on_stalled(now);
            now = now.saturating_add(60);
            assert!(!p.is_expired(now));
            p.on_written(4096);
            assert!(p.deadline_ns().is_none());
        }
        // Total elapsed (600) >> idle (100), yet never expired because every stall made progress.
        assert!(!p.is_expired(now));
    }

    #[test]
    fn read_wakes_and_zero_flushes_do_not_reset_armed_deadline() {
        let mut p = WriteProgress::new(100);
        p.on_stalled(50);
        assert_eq!(p.deadline_ns(), Some(150));

        p.on_non_progress_wake();
        p.on_written(0);
        p.on_stalled(120);
        assert_eq!(p.deadline_ns(), Some(150));
        assert!(!p.is_expired(149));
        assert!(p.is_expired(150));
    }
}
