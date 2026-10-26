//! The handles of the database that other threads solve with, and what the collection of
//! interned types needs to know of them.
//!
//! The trait solver keeps a cache in a thread-local of each thread that solves. An entry holds
//! its goals by an `InternedRef`, which does not count as a use of the interned value, so a
//! cache that outlives the collection of the types points at what the collection freed. A
//! thread-local lives until its thread ends, and a thread that has dropped its clone of the
//! database is not over (a tier compile goes on into LLVM, a build sends its answer), and
//! `rayon::broadcast` reaches only the threads of the pool. So every other thread takes its
//! clone as a [`Handle`], which is counted as a solving scope from the thread that makes it to
//! the thread that ends it, and which clears the solver cache of the thread that ends it before
//! the clone is dropped. [`Scopes::idle`] says what the collection asks for: that no thread but
//! the caller holds the database, and that every one that did has cleared its cache.
//!
//! The caller's own thread and the pool's are cleared by [`clear_caller_and_pool`], which
//! [`collect_types`] does before it asks.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use ide_db::RootDatabase;

/// The solving scopes of the process: the [`Handle`]s that are held, and whether one of them
/// could not clear its cache.
pub(crate) struct Scopes {
    live: AtomicUsize,
    /// A scope that could not clear the cache of its thread, which leaves a cache nothing
    /// will clear: the collection is off for the rest of the process rather than trusted.
    lost: AtomicBool,
}

/// Why the collection of interned types may not run.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Busy {
    /// This many handles of the database are held by other threads.
    Held(usize),
    /// A handle could not clear the cache of its thread.
    Uncleared,
}

impl std::fmt::Display for Busy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Busy::Held(1) => write!(f, "a thread still holds the database"),
            Busy::Held(held) => write!(f, "{held} threads still hold the database"),
            Busy::Uncleared => write!(f, "a thread's solver cache could not be cleared"),
        }
    }
}

/// The scopes of the process, which every [`Handle::of`] joins.
pub(crate) static SCOPES: Scopes = Scopes::new();

impl Scopes {
    pub(crate) const fn new() -> Scopes {
        Scopes { live: AtomicUsize::new(0), lost: AtomicBool::new(false) }
    }

    /// Whether no thread solves: no handle is held and none failed to clear its cache. The
    /// collection of interned types runs only when it is, and checks it again once the write
    /// that cancels the other handles of the database is over.
    pub(crate) fn idle(&self) -> Result<(), Busy> {
        // Acquire pairs with the release of each scope that left: what it cleared is cleared.
        if self.lost.load(Ordering::Acquire) {
            return Err(Busy::Uncleared);
        }
        match self.live.load(Ordering::Acquire) {
            0 => Ok(()),
            held => Err(Busy::Held(held)),
        }
    }

    /// How many handles are held.
    pub(crate) fn held(&self) -> usize {
        self.live.load(Ordering::Acquire)
    }

    /// A scope that ends with `clear`, which clears the cache of the thread that drops it.
    fn enter(&'static self, clear: fn()) -> Solving {
        self.live.fetch_add(1, Ordering::AcqRel);
        Solving { scopes: self, clear }
    }
}

/// One solving scope: counted from the thread that made it to the thread that drops it, and
/// on that thread it clears the solver cache as it ends. Held beside the clone of the database
/// it came with, and declared after it, so that it ends first: see [`Handle::split`].
pub(crate) struct Solving {
    scopes: &'static Scopes,
    clear: fn(),
}

impl Drop for Solving {
    fn drop(&mut self) {
        // A panic must not leave without the count saying so: a cache that could not be
        // cleared (the thread-local is gone, or borrowed) stays counted, and the collection
        // does not run again, which is slower to reclaim but cannot free what a cache holds.
        let cleared = std::panic::catch_unwind(self.clear).is_ok();
        if cleared {
            self.scopes.live.fetch_sub(1, Ordering::AcqRel);
        } else {
            self.scopes.lost.store(true, Ordering::Release);
            eprintln!(
                "reclaim: a thread's trait solver cache could not be cleared, so the interned \
                 types are not collected again in this process"
            );
        }
    }
}

/// A clone of the database for another thread to solve with, counted as a solving scope from
/// the moment it is made, on the thread that makes it, so that no moment between a thread being
/// started and it holding the database is uncounted.
///
/// The thread that receives it calls [`Handle::split`] first thing, and solves with the clone;
/// both bindings end on that thread, the scope first, which clears the cache of the thread's
/// trait solver before the clone is dropped. A handle dropped whole does the same, on the
/// thread that drops it.
///
/// Every clone of the database that leaves the thread that owns it is made with
/// [`Handle::of`]: `solving::tests::no_bare_clone_of_the_database` fails on one that is not.
pub(crate) struct Handle {
    // In this order, which is the order they are dropped in: the scope clears the cache and
    // stops counting, and then the clone is dropped, which is what a write to the database
    // waits for.
    solving: Solving,
    db: RootDatabase,
}

impl Handle {
    pub(crate) fn of(db: &RootDatabase) -> Handle {
        Handle::in_scopes(&SCOPES, db, hir::clear_tls_solver_cache)
    }

    fn in_scopes(scopes: &'static Scopes, db: &RootDatabase, clear: fn()) -> Handle {
        Handle { solving: scopes.enter(clear), db: db.clone() }
    }

    /// The clone to solve with and the scope it is solved in, in the order to bind them:
    /// `let (db, _solving) = handle.split();` drops `_solving` first, as bindings of a pattern
    /// are dropped in the reverse of the order they are written in. Nothing is solved with the
    /// clone after `_solving` is dropped.
    pub(crate) fn split(self) -> (RootDatabase, Solving) {
        (self.db, self.solving)
    }
}

/// Ends a split handle where it is not left to the end of a scope: the scope, then the clone.
pub(crate) fn release(db: RootDatabase, solving: Solving) {
    drop(solving);
    drop(db);
}

/// Clears the trait solver's cache on the thread that asks and on every thread of the global
/// pool, which hold no handle. Every other thread clears its own, in [`Handle`]'s drop.
pub(crate) fn clear_caller_and_pool() {
    rayon::broadcast(|_| hir::clear_tls_solver_cache());
    hir::clear_tls_solver_cache();
}

/// Collects the types, arguments and clauses interned for builds that are over, when nothing
/// can still hold one that the collection does not know of; and says why when it cannot.
///
/// The caches are cleared first, always: there is no way to collect here without. Then it asks
/// [`SCOPES`] twice whether any thread solves, before the write that cancels the other handles
/// of the database (so that a build under way is left to finish) and after it (when each
/// handle that was cancelled has been dropped, and has cleared its cache before it was).
pub(crate) fn collect_types(db: &mut RootDatabase) -> Result<(), Busy> {
    use ide_db::base_db::salsa::{Database, Durability};
    // A revision of its own, as rust-analyzer's `AnalysisHost::trigger_garbage_collection`
    // makes: the write cancels every other handle of the database and waits for each to be
    // dropped, so no query of it is under way and none of its results is held.
    let write = |db: &mut RootDatabase| db.synthetic_write(Durability::LOW);
    collect_in(db, &SCOPES, clear_caller_and_pool, write, || {
        // SAFETY: `collect_ty_garbage` needs no type value that is not yet recorded anywhere
        // the collection looks, no other handle of the database, and the database held
        // mutably from before it begins until it ends. In turn:
        //  - `collect_in` has `db` as `&mut`, held for the whole call, so no other thread can
        //    make a handle of it: borrowck.
        //  - Every other handle was dropped: the write before this closure runs waits for
        //    each (`zalsa_mut` waits for the clones of the storage to be one): salsa.
        //  - Every thread that solved with a clone held a [`Handle`], which cleared that
        //    thread's solver cache before it was dropped, and `SCOPES.idle()` has just said
        //    that none is held and none failed to clear: `Handle`, and the test that no
        //    clone of the database is made without one. The caller's thread and the global
        //    pool's, which hold none, were cleared by `clear_caller_and_pool` first.
        //  - What a build made of types is otherwise in salsa values, which the collection
        //    reaches from the memos it keeps, or in `Stored*` owners, which count as uses
        //    wherever they are held (a drop of one on another thread only lowers its count),
        //    or in a local of a build that ended with the thread of the `Handle` it ran on.
        unsafe { hir::collect_ty_garbage() }
    })
}

/// The steps of [`collect_types`] and the order they are checked in, with the steps passed in
/// for a test to watch.
fn collect_in(
    db: &mut RootDatabase,
    scopes: &Scopes,
    clear: impl FnOnce(),
    write: impl FnOnce(&mut RootDatabase),
    collect: impl FnOnce(),
) -> Result<(), Busy> {
    clear();
    scopes.idle()?;
    write(db);
    scopes.idle()?;
    collect();
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::{cell::Cell, sync::mpsc};

    use super::*;

    thread_local! {
        static CLEARED: Cell<usize> = const { Cell::new(0) };
    }

    fn cleared() {
        CLEARED.with(|it| it.set(it.get() + 1));
    }

    fn cannot_clear() {
        panic!("the thread-local is gone");
    }

    /// Scopes of a test's own, which no other test's handles are counted in.
    fn scopes() -> &'static Scopes {
        Box::leak(Box::new(Scopes::new()))
    }

    #[test]
    fn a_held_handle_is_a_live_scope() {
        let (scopes, db) = (scopes(), RootDatabase::new(None));
        assert_eq!(scopes.idle(), Ok(()));
        let handle = Handle::in_scopes(scopes, &db, cleared);
        assert_eq!(scopes.idle(), Err(Busy::Held(1)));
        let second = Handle::in_scopes(scopes, &db, cleared);
        assert_eq!(scopes.idle(), Err(Busy::Held(2)));
        drop(handle);
        assert_eq!(scopes.idle(), Err(Busy::Held(1)));
        drop(second);
        assert_eq!(scopes.idle(), Ok(()));
    }

    #[test]
    fn a_handle_clears_the_cache_of_the_thread_that_drops_it() {
        let (scopes, db) = (scopes(), RootDatabase::new(None));
        let handle = Handle::in_scopes(scopes, &db, cleared);
        let (sender, receiver) = mpsc::channel();
        // Made here and dropped on the thread that solves, as every site does.
        std::thread::spawn(move || {
            assert_eq!(CLEARED.with(Cell::get), 0);
            drop(handle);
            sender.send(CLEARED.with(Cell::get)).unwrap();
        });
        assert_eq!(receiver.recv().unwrap(), 1, "the thread that dropped it cleared its cache");
        assert_eq!(CLEARED.with(Cell::get), 0, "and not this one");
        assert_eq!(scopes.idle(), Ok(()));
    }

    #[test]
    fn the_scope_counts_until_the_cache_is_cleared() {
        // A scope that is gone has cleared: the count drops after the clear, not before.
        thread_local! {
            static SEEN: Cell<Option<usize>> = const { Cell::new(None) };
        }
        static SCOPES: Scopes = Scopes::new();
        fn clear() {
            SEEN.with(|it| it.set(Some(SCOPES.live.load(Ordering::Acquire))));
        }
        let db = RootDatabase::new(None);
        drop(Handle::in_scopes(&SCOPES, &db, clear));
        assert_eq!(SEEN.with(Cell::get), Some(1), "the scope still counted while it cleared");
        assert_eq!(SCOPES.idle(), Ok(()));
    }

    #[test]
    fn a_cache_that_could_not_be_cleared_keeps_the_collection_off() {
        let (scopes, db) = (scopes(), RootDatabase::new(None));
        drop(Handle::in_scopes(scopes, &db, cannot_clear));
        assert_eq!(scopes.idle(), Err(Busy::Uncleared));
        assert_eq!(scopes.held(), 1, "it stays counted");
        // Even a scope that clears afterwards does not lift it.
        drop(Handle::in_scopes(scopes, &db, cleared));
        assert_eq!(scopes.idle(), Err(Busy::Uncleared));
    }

    #[test]
    fn a_live_scope_makes_the_collection_skip() {
        use std::cell::RefCell;
        let (scopes, mut db) = (scopes(), RootDatabase::new(None));
        let steps = RefCell::new(Vec::new());
        let run = |db: &mut RootDatabase| {
            collect_in(
                db,
                scopes,
                || steps.borrow_mut().push("clear"),
                |_| steps.borrow_mut().push("write"),
                || steps.borrow_mut().push("collect"),
            )
        };
        // Nothing solves: it clears, writes, and then collects.
        assert_eq!(run(&mut db), Ok(()));
        assert_eq!(*steps.borrow(), ["clear", "write", "collect"]);
        steps.borrow_mut().clear();
        // A thread holds a handle: it clears what it can reach, and neither writes (which
        // would cancel that thread's queries) nor collects.
        let handle = Handle::in_scopes(scopes, &db, cleared);
        assert_eq!(run(&mut db), Err(Busy::Held(1)));
        assert_eq!(*steps.borrow(), ["clear"]);
        steps.borrow_mut().clear();
        // The thread is over with its handle: it collects again.
        drop(handle);
        assert_eq!(run(&mut db), Ok(()));
        assert_eq!(*steps.borrow(), ["clear", "write", "collect"]);
    }

    #[test]
    fn a_scope_that_is_live_once_the_write_is_over_makes_the_collection_skip() {
        // The second look is for a handle the first could not see, such as one whose queries
        // the write cancelled and that has not finished: the write waits for its clone, and
        // the count it left says it had not cleared.
        use std::cell::RefCell;
        let (scopes, mut db) = (scopes(), RootDatabase::new(None));
        let steps = RefCell::new(Vec::new());
        let straggler = RefCell::new(None);
        let other = db.clone();
        let result = collect_in(
            &mut db,
            scopes,
            || steps.borrow_mut().push("clear"),
            |_| {
                steps.borrow_mut().push("write");
                *straggler.borrow_mut() = Some(Handle::in_scopes(scopes, &other, cleared));
            },
            || steps.borrow_mut().push("collect"),
        );
        assert_eq!(result, Err(Busy::Held(1)));
        assert_eq!(*steps.borrow(), ["clear", "write"], "collected with a scope live");
        drop(straggler.take());
        assert_eq!(scopes.idle(), Ok(()));
    }

    #[test]
    fn a_cache_that_could_not_be_cleared_makes_the_collection_skip_for_good() {
        use std::cell::RefCell;
        let (scopes, mut db) = (scopes(), RootDatabase::new(None));
        drop(Handle::in_scopes(scopes, &db, cannot_clear));
        let steps = RefCell::new(Vec::new());
        for _ in 0..2 {
            let result = collect_in(
                &mut db,
                scopes,
                || steps.borrow_mut().push("clear"),
                |_| steps.borrow_mut().push("write"),
                || steps.borrow_mut().push("collect"),
            );
            assert_eq!(result, Err(Busy::Uncleared));
        }
        assert_eq!(*steps.borrow(), ["clear", "clear"]);
    }

    #[test]
    fn a_handle_as_the_daemon_makes_it_ends_on_the_thread_that_solved() {
        // With the solver's own clear, on a thread that has never solved, and then on one that
        // drops the handle whole: neither leaves a scope counted. The global scopes are used by
        // this test alone.
        let db = RootDatabase::new(None);
        let (first, second) = (Handle::of(&db), Handle::of(&db));
        assert_eq!(SCOPES.held(), 2);
        std::thread::scope(|scope| {
            scope.spawn(move || {
                let (db, solving) = first.split();
                release(db, solving);
            });
            scope.spawn(move || drop(second));
        });
        assert_eq!(SCOPES.idle(), Ok(()));
    }

    #[test]
    fn split_binds_the_scope_to_be_dropped_before_the_clone() {
        // What `Handle::split` relies on of the language: the bindings of a pattern are dropped
        // in the reverse of the order they are written in, so the one written second goes first.
        struct Note(&'static str, &'static std::sync::Mutex<Vec<&'static str>>);
        impl Drop for Note {
            fn drop(&mut self) {
                self.1.lock().unwrap().push(self.0);
            }
        }
        static DROPPED: std::sync::Mutex<Vec<&'static str>> = std::sync::Mutex::new(Vec::new());
        {
            let (_db, _solving) = (Note("clone", &DROPPED), Note("scope", &DROPPED));
        }
        assert_eq!(*DROPPED.lock().unwrap(), ["scope", "clone"]);
    }

    #[test]
    fn no_bare_clone_of_the_database() {
        // A clone of the database that a thread solves with and the collection does not
        // count is a cache nothing clears. So the clones in this crate are all handles.
        fn sources(dir: &std::path::Path, found: &mut Vec<std::path::PathBuf>) {
            for entry in std::fs::read_dir(dir).unwrap().map(Result::unwrap) {
                let path = entry.path();
                if path.is_dir() {
                    sources(&path, found);
                } else if path.extension().is_some_and(|it| it == "rs") {
                    found.push(path);
                }
            }
        }
        let mut files = Vec::new();
        sources(&std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src"), &mut files);
        let mut bare = Vec::new();
        for file in files {
            if file.file_name().is_some_and(|it| it == "solving.rs") {
                continue;
            }
            let text = std::fs::read_to_string(&file).unwrap();
            for (at, line) in text.lines().enumerate() {
                let code = line.split("//").next().unwrap_or(line);
                if code.contains("db.clone()") || code.contains("database.clone()") {
                    bare.push(format!("{}:{}: {}", file.display(), at + 1, line.trim()));
                }
            }
        }
        assert!(
            bare.is_empty(),
            "a clone of the database for another thread is made with `solving::Handle::of`:\n{}",
            bare.join("\n")
        );
    }
}
