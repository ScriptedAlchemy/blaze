//! Callback obligations survive forgotten handles, failed starts, and parent unwinding.

use core::sync::atomic::AtomicUsize;

use blaze_own::{Arc, Vec};
use blaze_sim::{Broken, Config, Tape};
#[cfg(all(not(blaze), panic = "unwind"))]
use blaze_sim::Verdict;

use super::*;

const FIXTURE_DEADLINE: Duration = Duration::from_secs(20);

fn test_alloc() -> WireAlloc {
    blaze_own::testing::heap().shared()
}

#[derive(Debug)]
enum ScopeBroken {
    ReleasedLiveBorrow,
}

impl fmt::Display for ScopeBroken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a forgotten handle released a callback's live scoped borrow")
    }
}

blaze_sim::plants! {
    enum ScopePlant breaks ScopeBroken {
        None,
        HandleOwnsCompletion => ScopeBroken::ReleasedLiveBorrow,
    }
}

/// Drive the production registration tickets without OS scheduling. A live callback may
/// spawn children; failed starts and completed callbacks release the same obligation.
/// Handle disposition cannot release a callback's obligation.
fn obligations(tape: &Tape, plant: ScopePlant) -> Result<(), Broken<ScopeBroken>> {
    let alloc = test_alloc();
    let scope = Scope {
        alloc,
        threads: Arc::new_in(ScopeThreads { pending: AtomicU32::new(0) }, alloc),
        _scope: PhantomData,
        _env: PhantomData,
    };
    let wait = ScopeWait { threads: &scope.threads };
    let mut callbacks = Vec::new_in(alloc);
    callbacks.push(scope.register().unwrap());
    let children = tape.below(8, "nested callback count");

    // The caller forgets the root handle while its callback still holds a borrow. The
    // planted old ownership attaches the obligation to that forgettable handle instead.
    if matches!(plant, ScopePlant::HandleOwnsCompletion) {
        drop(callbacks.pop().unwrap());
    }
    if scope.threads.pending.load(Ordering::Acquire) == 0 {
        return Err(Broken::Oracle(ScopeBroken::ReleasedLiveBorrow));
    }

    for _ in 0..children {
        let registration = scope.register().unwrap();
        if tape.coin("pthread creation is refused after registration") {
            drop(registration);
        } else {
            callbacks.push(registration);
        }
        assert_eq!(scope.threads.pending.load(Ordering::Acquire), u32::try_from(callbacks.len()).unwrap());
    }

    while !callbacks.is_empty() {
        let count = u64::try_from(callbacks.len()).unwrap();
        let at = usize::try_from(tape.below(count, "next callback completion")).unwrap();
        drop(callbacks.remove_swap(at));
        assert_eq!(scope.threads.pending.load(Ordering::Acquire), u32::try_from(callbacks.len()).unwrap());
        assert!(callbacks.is_empty() || scope.threads.pending.load(Ordering::Acquire) != 0);
    }
    // No callback can register a child now. The same guard used by scope() must neither
    // stop early nor wait for a refused start forever.
    drop(wait);
    assert_eq!(scope.threads.pending.load(Ordering::Acquire), 0);
    Ok(())
}

/// The invariant: every live callback retains a scope obligation independently of its handle, and failed starts release theirs exactly once.
#[test]
fn scope_completion_obligations_survive_handle_disposition_and_failed_starts() {
    blaze_sim::check(Config::cases(128), |tape| obligations(tape, ScopePlant::None));
}

#[test]
fn the_oracle_catches_completion_owned_by_a_forgotten_handle() {
    blaze_sim::controls(Config::cases(128), obligations);
}

#[test]
fn scope_registration_refuses_counter_overflow() {
    let alloc = test_alloc();
    let scope = Scope {
        alloc,
        threads: Arc::new_in(ScopeThreads { pending: AtomicU32::new(u32::MAX) }, alloc),
        _scope: PhantomData,
        _env: PhantomData,
    };
    assert!(matches!(scope.register(), Err(error) if error.errno() == Errno::Again));
    assert_eq!(scope.threads.pending.load(Ordering::Acquire), u32::MAX);
}

#[test]
fn scope_wait_rechecks_completion_before_parking() {
    let alloc = test_alloc();
    let scope = Scope {
        alloc,
        threads: Arc::new_in(ScopeThreads { pending: AtomicU32::new(0) }, alloc),
        _scope: PhantomData,
        _env: PhantomData,
    };
    let completed = scope.register().unwrap();
    let observed = scope.threads.pending.load(Ordering::Acquire);
    drop(completed);
    // The final wake already happened. Waiting against the earlier nonzero observation
    // must return; waiting against a refreshed zero instead would lose that wake.
    wait_on_counter(&scope.threads.pending, observed);
    drop(ScopeWait { threads: &scope.threads });
    assert_eq!(scope.threads.pending.load(Ordering::Acquire), 0);
}

struct BorrowOnDrop<'a>(&'a AtomicUsize);

impl Drop for BorrowOnDrop<'_> {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::Release);
    }
}

#[test]
fn scope_waits_for_forgotten_borrowed_callbacks() {
    let alloc = test_alloc();
    let completed = AtomicUsize::new(0);
    let (release, start) = channel(alloc);
    scope(alloc, |scope| {
        let borrowed = BorrowOnDrop(&completed);
        let handle = scope
            .spawn(move || {
                start.recv_timeout(FIXTURE_DEADLINE).unwrap();
                drop(borrowed);
            })
            .unwrap();
        core::mem::forget(handle);
        release.send(()).unwrap();
    });
    assert_eq!(completed.load(Ordering::Acquire), 1);
}

#[test]
fn scope_preserves_explicit_join_drop_and_nested_spawn() {
    let alloc = test_alloc();
    let completed = AtomicUsize::new(0);
    scope(alloc, |scope| {
        let joined = scope.spawn(|| {
            completed.fetch_add(1, Ordering::Release);
            17
        }).unwrap();
        assert_eq!(joined.join(), Ok(17));
        drop(scope.spawn(|| {
            completed.fetch_add(1, Ordering::Release);
        }).unwrap());
        let parent = scope.spawn(|| {
            let nested = scope.spawn(|| {
                completed.fetch_add(1, Ordering::Release);
            }).unwrap();
            core::mem::forget(nested);
            completed.fetch_add(1, Ordering::Release);
        }).unwrap();
        core::mem::forget(parent);
    });
    assert_eq!(completed.load(Ordering::Acquire), 4);
}

/// The native simulation runner already catches parent panics; no new production unwind
/// runtime is introduced. Callback panics still follow the existing extern-C abort boundary.
#[cfg(all(not(blaze), panic = "unwind"))]
#[test]
fn scope_releases_registration_when_spawn_setup_unwinds() {
    for payload_drop_panics in [false, true] {
        let verdict = blaze_sim::search(Config::cases(1).shrink_runs(0), |tape| -> () {
            let _ = tape.coin("spawn setup unwind tape");
            let alloc = test_alloc();
            scope(alloc, |scope| {
                let completed = scope.register().unwrap();
                if payload_drop_panics {
                    struct FailedPayload;
                    impl Drop for FailedPayload {
                        fn drop(&mut self) {
                            panic!("injected payload drop failure");
                        }
                    }
                    let payload = Trampoline::<_, ()> {
                        func: FailedPayload,
                        result_slot: core::ptr::null_mut(),
                        completed: Some(completed),
                        alloc,
                    };
                    drop(payload);
                } else {
                    let _reserved = completed;
                    panic!("injected allocation failure after registration");
                }
            });
        });
        assert!(matches!(verdict, Verdict::Failed(_)), "the injected unwind was not caught");
    }
}

#[cfg(all(not(blaze), panic = "unwind"))]
#[test]
fn scope_waits_for_forgotten_callbacks_when_parent_unwinds() {
    let completed = AtomicUsize::new(0);
    let verdict = blaze_sim::search(Config::cases(1).shrink_runs(0), |tape| -> () {
        let _ = tape.coin("parent panic tape");
        completed.store(0, Ordering::Release);
        let alloc = test_alloc();
        let (release, start) = channel(alloc);
        struct ReleaseOnDrop(Sender<()>);
        impl Drop for ReleaseOnDrop {
            fn drop(&mut self) {
                self.0.send(()).unwrap();
            }
        }
        scope(alloc, |scope| {
            let _release_on_unwind = ReleaseOnDrop(release);
            let borrowed = BorrowOnDrop(&completed);
            let handle = scope.spawn(move || {
                start.recv_timeout(FIXTURE_DEADLINE).unwrap();
                drop(borrowed);
            }).unwrap();
            core::mem::forget(handle);
            panic!("injected parent unwind");
        });
    });
    assert!(matches!(verdict, Verdict::Failed(_)), "the injected parent unwind was not caught");
    assert_eq!(completed.load(Ordering::Acquire), 1);
}

#[test]
fn failed_self_join_does_not_mark_a_live_thread_joined() {
    let alloc = test_alloc();
    scope(alloc, |scope| {
        let (send_handle, own_handle) = channel::<JoinHandle<'_, u8>>(alloc);
        let (return_handle, returned_handle) = channel(alloc);
        let handle = scope.spawn(move || {
            let handle = own_handle.recv_timeout(FIXTURE_DEADLINE).unwrap();
            let refused = handle.join_raw();
            let marked_joined = handle.joined.load(Ordering::Acquire);
            assert!(return_handle.send((handle, refused, marked_joined)).is_ok());
            42
        }).unwrap();
        assert!(send_handle.send(handle).is_ok());
        let (handle, refused, marked_joined) = returned_handle.recv_timeout(FIXTURE_DEADLINE).unwrap();
        assert_eq!(refused, Err(ThreadFailed));
        assert!(!marked_joined, "a refused OS join cannot prove callback completion");
        assert_eq!(handle.join(), Ok(42));
    });
}

#[test]
fn self_join_and_self_drop_keep_the_callback_result_slot_alive() {
    let alloc = test_alloc();
    for explicit_join in [false, true] {
        let completed = AtomicBool::new(false);
        scope(alloc, |scope| {
            let (send_handle, own_handle) = channel::<JoinHandle<'_, ()>>(alloc);
            let completed = &completed;
            let handle = scope.spawn(move || {
                let handle = own_handle.recv_timeout(FIXTURE_DEADLINE).unwrap();
                if explicit_join {
                    assert_eq!(handle.join(), Err(ThreadFailed));
                } else {
                    drop(handle);
                }
                completed.store(true, Ordering::Release);
            }).unwrap();
            assert!(send_handle.send(handle).is_ok());
        });
        assert!(completed.load(Ordering::Acquire));
    }
}
