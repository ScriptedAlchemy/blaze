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
//!
//! A recovery that ends is **told to the worker after it has ended**: the token is a [`Token`],
//! whose last owner to go calls `wake` once the count of owners is already zero. The requests
//! that carry it end on threads of their own (a link that is detached from the turn ends by itself,
//! and an answer that succeeds sends the loop nothing), and a wake they make before their token
//! goes (the release of their owner) is one the loop can take while the recovery is still under
//! way, and sleep on.

use std::{
    path::PathBuf,
    sync::{Arc, Weak},
};

use crate::scripts::Stale;

/// The token of a recovery under way. It wakes the worker when the last of its owners is gone.
pub(crate) struct Token {
    wake: Box<dyn Fn() + Send + Sync>,
}

impl Drop for Token {
    fn drop(&mut self) {
        // The strong count is zero already: the recovery is over for whoever looks now.
        (self.wake)();
    }
}

/// What a request remembers of the recoveries it waited for.
#[derive(Default)]
pub(crate) struct Recovery {
    /// The recovery under way: it lasts as long as any of the requests that carry its token.
    asked: Weak<Token>,
    /// The script and the run the last recovery that ended found the output of.
    last: Option<(PathBuf, Option<u64>)>,
}

impl Recovery {
    /// Whether a recovery this request asked for is not over.
    pub(crate) fn under_way(&self) -> bool {
        self.asked.strong_count() > 0
    }

    /// The world moved: whether a recovery is to be asked for. Not when the last recovery was of
    /// this very output, and another would be the same.
    pub(crate) fn advances(&mut self, stale: &Stale) -> bool {
        let now = (stale.run_dir.clone(), stale.found);
        if self.last.as_ref() == Some(&now) {
            return false;
        }
        self.last = Some(now);
        true
    }

    /// The same, for a request that waits for the requests it queues: `Some(token)` if it asks
    /// for a recovery, which is over when the last holder of the token is dropped, and then `wake`
    /// is called.
    pub(crate) fn moved(
        &mut self,
        stale: &Stale,
        wake: impl Fn() + Send + Sync + 'static,
    ) -> Option<Arc<Token>> {
        if !self.advances(stale) {
            return None;
        }
        let token = Arc::new(Token { wake: Box::new(wake) });
        self.asked = Arc::downgrade(&token);
        Some(token)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    fn stale(dir: &str, found: u64) -> Stale {
        Stale {
            run_dir: PathBuf::from(dir),
            why: "it was run again".to_owned(),
            found: Some(found),
        }
    }

    fn quiet() {}

    #[test]
    fn a_recovery_is_under_way_until_every_request_that_carries_it_is_gone() {
        let mut recovery = Recovery::default();
        assert!(!recovery.under_way());
        let token = recovery.moved(&stale("r", 1), quiet).unwrap();
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
        drop(recovery.moved(&stale("r", 1), quiet).unwrap());
        // Over; another build ran the script meanwhile: a new recovery, however many there are.
        for found in 2..40 {
            drop(recovery.moved(&stale("r", found), quiet).unwrap());
        }
        // Over; the output is the run it was the last time: asking again asks for the same.
        assert!(recovery.moved(&stale("r", 39), quiet).is_none());
        // Another script is another thing to recover.
        assert!(recovery.moved(&stale("s", 39), quiet).is_some());
        assert!(recovery.moved(&Stale { found: None, ..stale("s", 0) }, quiet).is_some());
        assert!(recovery.moved(&Stale { found: None, ..stale("s", 0) }, quiet).is_none());
        assert!(!recovery.advances(&Stale { found: None, ..stale("s", 0) }));
        assert!(recovery.advances(&stale("s", 41)));
    }

    /// The order the worker needs: a request that ends releases its owner (a wake, which the loop
    /// may take at once), then drops its token; the wake of the token is made by the last owner,
    /// and after the recovery is over for anyone who looks.
    #[test]
    fn the_last_owner_of_a_recovery_wakes_the_worker_after_the_recovery_is_over() {
        let mut recovery = Recovery::default();
        // What the worker's loop sees at each wake: whether the recovery is under way.
        let seen = Arc::new(Mutex::new(Vec::new()));
        let asked = Arc::new(Mutex::new(Weak::<Token>::new()));
        let wake = {
            let (seen, asked) = (seen.clone(), asked.clone());
            move || {
                let under_way = asked.lock().unwrap().strong_count() > 0;
                seen.lock().unwrap().push(under_way);
            }
        };
        let first = recovery.moved(&stale("r", 1), wake).unwrap();
        *asked.lock().unwrap() = Arc::downgrade(&first);
        let second = first.clone();
        assert!(recovery.under_way());
        drop(first);
        assert!(seen.lock().unwrap().is_empty(), "a request that is not the last wakes no one");
        drop(second);
        assert_eq!(*seen.lock().unwrap(), [false], "once, and when it is over");
        assert!(!recovery.under_way());
    }
}
