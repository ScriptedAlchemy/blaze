//! A request that was put off because the world its database was loaded from has moved.
//!
//! Another build ran a script since, so the output on disk is not the one the database reads
//! ([`scripts::Stale`]). The worker loads the checkouts again, as a request for each program
//! would, and asks the request again. What the request remembers is the recovery it is waiting
//! for, so that it neither asks for a second one while the first is not over nor spends an
//! attempt for a turn of the worker's loop in which nothing happened:
//!
//! - a recovery that is **under way** is waited for, however long the build that holds what it
//!   needs takes: the request is asked again when it is over, and the wait is the worker's
//!   sleep, which a [`scripts::generation`] that moved ends;
//! - a recovery that is **over** and leaves the world moved *again* is another recovery when
//!   the output is another run's than the last time (another build ran the script meanwhile:
//!   a world that keeps moving is a build that keeps running scripts, and every one costs the
//!   other a recovery and not the request its answer), and is the end of the request when it
//!   is the same run's (the recovery loaded the world and the output is still not what it was
//!   loaded from: asking again would ask for the same).

use std::{
    path::PathBuf,
    sync::{Arc, Weak},
};

use crate::scripts::Stale;

/// What a request remembers of the recoveries it waited for.
#[derive(Default)]
pub(crate) struct Recovery {
    /// The recovery under way: it lasts as long as any of the requests that carry its token.
    asked: Weak<()>,
    /// The script and the run the last recovery that ended found the output of.
    last: Option<(PathBuf, Option<u64>)>,
}

impl Recovery {
    /// Whether a recovery this request asked for is not over.
    pub(crate) fn under_way(&self) -> bool {
        self.asked.strong_count() > 0
    }

    /// The world moved: `Some(token)` if the request asks for a recovery, which is over when the
    /// last holder of the token is dropped; `None` if the last recovery was of this very
    /// output, and another would be the same.
    pub(crate) fn moved(&mut self, stale: &Stale) -> Option<Arc<()>> {
        let now = (stale.run_dir.clone(), stale.found);
        if self.last.as_ref() == Some(&now) {
            return None;
        }
        self.last = Some(now);
        let token = Arc::new(());
        self.asked = Arc::downgrade(&token);
        Some(token)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stale(dir: &str, found: u64) -> Stale {
        Stale {
            run_dir: PathBuf::from(dir),
            why: "it was run again".to_owned(),
            found: Some(found),
        }
    }

    #[test]
    fn a_recovery_is_under_way_until_every_request_that_carries_it_is_gone() {
        let mut recovery = Recovery::default();
        assert!(!recovery.under_way());
        let token = recovery.moved(&stale("r", 1)).unwrap();
        let second = token.clone();
        assert!(recovery.under_way());
        drop(token);
        assert!(recovery.under_way(), "one of the requests is still there");
        drop(second);
        assert!(!recovery.under_way());
    }

    #[test]
    fn a_world_that_moved_again_is_another_recovery_and_the_same_run_is_the_end() {
        let mut recovery = Recovery::default();
        drop(recovery.moved(&stale("r", 1)).unwrap());
        // Over; another build ran the script meanwhile: a new recovery, however many there are.
        for found in 2..40 {
            drop(recovery.moved(&stale("r", found)).unwrap());
        }
        // Over; the output is the run it was the last time: asking again asks for the same.
        assert!(recovery.moved(&stale("r", 39)).is_none());
        // Another script is another thing to recover.
        assert!(recovery.moved(&stale("s", 39)).is_some());
        assert!(recovery.moved(&Stale { found: None, ..stale("s", 0) }).is_some());
        assert!(recovery.moved(&Stale { found: None, ..stale("s", 0) }).is_none());
    }
}
