//! Garbage collection of interned values.
//!
//! The GC is a simple mark-and-sweep GC: you first mark all storages, then the
//! GC visits them, and each live value they refer, recursively, then removes
//! those not marked. The sweep phase is done in parallel.
//!
//! A value is a root when something besides its storage owns it (its strong count is
//! more than one), and every value reached from a root is marked, whatever its own count.
//! Owners may be dropped on other threads while the GC runs: a count can only fall then,
//! so a value seen as a root may be kept one collection longer than it needs to be, but a
//! value reached from a live one is never freed for having lost its other owners on the way.

use std::{hash::Hash, marker::PhantomData, ops::ControlFlow};

use dashmap::DashMap;
use hashbrown::raw::RawTable;
use rayon::iter::{IntoParallelRefIterator, ParallelIterator};
use rustc_hash::{FxBuildHasher, FxHashSet};
use triomphe::{Arc, ThinArc};

use crate::{Internable, InternedRef, InternedSliceRef, SliceInternable};

trait Storage {
    fn len(&self) -> usize;

    fn mark(&self, gc: &mut GarbageCollector);

    fn sweep(&self, gc: &GarbageCollector);
}

struct InternedStorage<T>(PhantomData<fn() -> T>);

impl<T: Internable + GcInternedVisit> Storage for InternedStorage<T> {
    fn len(&self) -> usize {
        T::storage().get().len()
    }

    fn mark(&self, gc: &mut GarbageCollector) {
        let storage = T::storage().get();
        for item in storage {
            let item = item.key();
            let addr = Arc::as_ptr(item).addr();
            if Arc::strong_count(item) > 1 && gc.alive.insert(addr) {
                item.visit_with(gc);
            }
        }
    }

    fn sweep(&self, gc: &GarbageCollector) {
        let storage = T::storage().get();
        gc.sweep_storage(storage, |item| item.as_ptr().addr());
    }
}

struct InternedSliceStorage<T>(PhantomData<fn() -> T>);

impl<T: SliceInternable + GcInternedSliceVisit> Storage for InternedSliceStorage<T> {
    fn len(&self) -> usize {
        T::storage().get().len()
    }

    fn mark(&self, gc: &mut GarbageCollector) {
        let storage = T::storage().get();
        for item in storage {
            let item = item.key();
            let addr = ThinArc::as_ptr(item).addr();
            if ThinArc::strong_count(item) > 1 && gc.alive.insert(addr) {
                T::visit_header(&item.header.header, gc);
                T::visit_slice(&item.slice, gc);
            }
        }
    }

    fn sweep(&self, gc: &GarbageCollector) {
        let storage = T::storage().get();
        gc.sweep_storage(storage, |item| item.as_ptr().addr());
    }
}

pub trait GcInternedVisit {
    fn visit_with(&self, gc: &mut GarbageCollector);
}

pub trait GcInternedSliceVisit: SliceInternable {
    fn visit_header(header: &Self::Header, gc: &mut GarbageCollector);
    fn visit_slice(slice: &[Self::SliceType], gc: &mut GarbageCollector);
}

#[derive(Default)]
pub struct GarbageCollector {
    alive: FxHashSet<usize>,
    storages: Vec<&'static (dyn Storage + Send + Sync)>,
}

impl GarbageCollector {
    pub fn add_storage<T: Internable + GcInternedVisit>(&mut self) {
        const { assert!(T::USE_GC) };

        self.storages.push(&InternedStorage::<T>(PhantomData));
    }

    pub fn add_slice_storage<T: SliceInternable + GcInternedSliceVisit>(&mut self) {
        const { assert!(T::USE_GC) };

        self.storages.push(&InternedSliceStorage::<T>(PhantomData));
    }

    /// Collects unreachable GC-managed interned values.
    ///
    /// # Safety
    ///
    ///  - This cannot be called if there are some not-yet-recorded type values.
    ///  - All storages that can contain live GC-managed values must have been added, and those
    ///    storages must be closed over the GC-managed values reachable from them.
    ///  - [`GcInternedVisit`] and [`GcInternedSliceVisit`] must mark all values reachable from the node.
    pub unsafe fn collect(mut self) {
        if cfg!(feature = "prevent-gc") {
            return;
        }

        let total_nodes = self.storages.iter().map(|storage| storage.len()).sum();
        self.alive.clear();
        self.alive.reserve(total_nodes);

        let storages = std::mem::take(&mut self.storages);

        for &storage in &storages {
            storage.mark(&mut self);
        }

        // Miri doesn't support rayon.
        if cfg!(miri) {
            storages.iter().for_each(|storage| storage.sweep(&self));
        } else {
            storages.par_iter().for_each(|storage| storage.sweep(&self));
        }
    }

    /// Marks a value reached from a live one. Returns `Continue` when it was not marked
    /// before, and the caller should visit what it refers to.
    ///
    /// The value is marked even when it has owners of its own, which would make it a root:
    /// they may be dropped on another thread before its storage is scanned for roots, and the
    /// scan would then see no owner and skip it, while the value that reached it lives on.
    pub fn mark_interned_alive<T: Internable>(
        &mut self,
        interned: InternedRef<'_, T>,
    ) -> ControlFlow<()> {
        const { assert!(T::USE_GC) };

        let addr = interned.as_raw().addr();
        if !self.alive.insert(addr) { ControlFlow::Break(()) } else { ControlFlow::Continue(()) }
    }

    /// As [`GarbageCollector::mark_interned_alive`], for an interned slice.
    pub fn mark_interned_slice_alive<T: SliceInternable>(
        &mut self,
        interned: InternedSliceRef<'_, T>,
    ) -> ControlFlow<()> {
        const { assert!(T::USE_GC) };

        let addr = interned.as_raw().addr();
        if !self.alive.insert(addr) { ControlFlow::Break(()) } else { ControlFlow::Continue(()) }
    }

    fn sweep_storage<T: Hash + Eq + Send + Sync>(
        &self,
        storage: &DashMap<T, (), FxBuildHasher>,
        get_addr: impl Fn(&T) -> usize + Send + Sync,
    ) {
        // Miri doesn't support rayon.
        if cfg!(miri) {
            storage.shards().iter().for_each(|shard| {
                self.retain_only_alive(&mut *shard.write(), |item| get_addr(&item.0))
            });
        } else {
            storage.shards().par_iter().for_each(|shard| {
                self.retain_only_alive(&mut *shard.write(), |item| get_addr(&item.0))
            });
        }
    }

    #[inline]
    fn retain_only_alive<T>(&self, map: &mut RawTable<T>, mut get_addr: impl FnMut(&T) -> usize) {
        // This code was copied from DashMap's retain() - which we can't use because we want to run in parallel.
        unsafe {
            // Here we only use `iter` as a temporary, preventing use-after-free
            for bucket in map.iter() {
                let item = bucket.as_mut();
                let addr = get_addr(item);
                if !self.alive.contains(&addr) {
                    map.erase(bucket);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        hash::{Hash, Hasher},
        sync::{
            Mutex, OnceLock,
            atomic::{AtomicUsize, Ordering},
            mpsc,
        },
        thread::JoinHandle,
    };

    use crate::{
        GarbageCollector, GcInternedSliceVisit, GcInternedVisit, Internable, Interned, InternedRef,
        InternedSliceRef,
    };

    /// Something for a visitor to run in the middle of the marking.
    type Hook = Mutex<Option<Box<dyn FnOnce() + Send>>>;

    /// Has another thread drop `owner` when a visitor runs `hook`, which returns once it is
    /// dropped: an owner that a thread the collector knows nothing of lets go of while the
    /// collector marks. Without the hook being run, `owner` is dropped when the hook is.
    fn drop_on_hook<T: Send + 'static>(hook: &'static Hook, owner: T) -> JoinHandle<()> {
        let (reached, on_reached) = mpsc::channel();
        let (dropped, on_dropped) = mpsc::channel();
        *hook.lock().unwrap() = Some(Box::new(move || {
            reached.send(()).unwrap();
            on_dropped.recv().unwrap();
        }));
        std::thread::spawn(move || {
            let ran = on_reached.recv().is_ok();
            drop(owner);
            if ran {
                dropped.send(()).unwrap();
            }
        })
    }

    fn run_hook(hook: &Hook) {
        let hook = hook.lock().unwrap().take();
        if let Some(hook) = hook {
            hook();
        }
    }

    /// Ends a test's use of `hook`: says whether it ran, and lets the dropper end if not.
    fn hook_ran(hook: &Hook, dropper: JoinHandle<()>) -> bool {
        let unused = hook.lock().unwrap().take();
        let ran = unused.is_none();
        drop(unused);
        dropper.join().unwrap();
        ran
    }

    crate::impl_internable!(String);

    #[test]
    fn simple_interned() {
        let a = Interned::new("abc".to_owned());
        let b = Interned::new("abc".to_owned());
        assert_eq!(a, b);
        assert_eq!(a.as_ref(), b.as_ref());
        assert_eq!(a.as_ref(), a.as_ref());
        assert_eq!(a, a.clone());
        assert_eq!(a, a.clone().clone());
        assert_eq!(b.clone(), a.clone().clone());
        assert_eq!(*a, "abc");
        assert_eq!(*b, "abc");
        assert_eq!(b.as_ref().to_owned(), a);
        let c = Interned::new("def".to_owned());
        assert_ne!(a, c);
        assert_ne!(b, c);
        assert_ne!(b.as_ref(), c.as_ref());
        assert_eq!(*c.as_ref(), "def");
        drop(c);
        assert_eq!(*a, "abc");
        assert_eq!(*b, "abc");
        drop(a);
        assert_eq!(*b, "abc");
        drop(b);
    }

    #[test]
    fn simple_gc() {
        #[derive(Debug, PartialEq, Eq, Hash)]
        struct GcString(String);

        crate::impl_internable!(gc; GcString);

        impl GcInternedVisit for GcString {
            fn visit_with(&self, _gc: &mut GarbageCollector) {}
        }

        crate::impl_slice_internable!(gc; StringSlice, String, u32);
        type InternedSlice = crate::InternedSlice<StringSlice>;

        impl GcInternedSliceVisit for StringSlice {
            fn visit_header(_header: &Self::Header, _gc: &mut GarbageCollector) {}

            fn visit_slice(_slice: &[Self::SliceType], _gc: &mut GarbageCollector) {}
        }

        let (a, d) = {
            let a = Interned::new_gc(GcString("abc".to_owned())).to_owned();
            let b = Interned::new_gc(GcString("abc".to_owned())).to_owned();
            assert_eq!(a, b);
            assert_eq!(a.as_ref(), b.as_ref());
            assert_eq!(a.as_ref(), a.as_ref());
            assert_eq!(a, a.clone());
            assert_eq!(a, a.clone().clone());
            assert_eq!(b.clone(), a.clone().clone());
            assert_eq!(a.0, "abc");
            assert_eq!(b.0, "abc");
            assert_eq!(b.as_ref().to_owned(), a);
            let c = Interned::new_gc(GcString("def".to_owned())).to_owned();
            assert_ne!(a, c);
            assert_ne!(b, c);
            assert_ne!(b.as_ref(), c.as_ref());
            assert_eq!(c.as_ref().0, "def");

            let d = InternedSlice::from_header_and_slice("abc".to_owned(), &[123, 456]);
            let e = InternedSlice::from_header_and_slice("abc".to_owned(), &[123, 456]);
            assert_eq!(d, e);
            assert_eq!(d.to_owned(), e.to_owned());
            assert_eq!(d.header.length, 2);
            assert_eq!(d.header.header, "abc");
            assert_eq!(d.slice, [123, 456]);
            (a, d.to_owned())
        };

        let mut gc = GarbageCollector::default();
        gc.add_slice_storage::<StringSlice>();
        gc.add_storage::<GcString>();
        unsafe { gc.collect() };

        if !cfg!(feature = "prevent-gc") {
            assert_eq!(<GcString as crate::Internable>::storage().get().len(), 1);
            assert_eq!(<StringSlice as crate::SliceInternable>::storage().get().len(), 1);
        }
        assert_eq!(a.0, "abc");
        assert_eq!(d.header.length, 2);
        assert_eq!(d.header.header, "abc");
        assert_eq!(d.slice, [123, 456]);

        drop(a);
        drop(d);

        let mut gc = GarbageCollector::default();
        gc.add_slice_storage::<StringSlice>();
        gc.add_storage::<GcString>();
        unsafe { gc.collect() };

        if !cfg!(feature = "prevent-gc") {
            assert_eq!(<GcString as crate::Internable>::storage().get().len(), 0);
            assert_eq!(<StringSlice as crate::SliceInternable>::storage().get().len(), 0);
        }
    }

    #[test]
    fn gc_visit() {
        #[derive(PartialEq, Eq, Hash)]
        struct GcInterned(InternedSliceRef<'static, StringSlice>);

        crate::impl_internable!(gc; GcInterned);

        impl GcInternedVisit for GcInterned {
            fn visit_with(&self, gc: &mut GarbageCollector) {
                _ = gc.mark_interned_slice_alive(self.0);
            }
        }

        crate::impl_slice_internable!(gc; StringSlice, String, i32);
        type InternedSlice = crate::InternedSlice<StringSlice>;

        impl GcInternedSliceVisit for StringSlice {
            fn visit_header(_header: &Self::Header, _gc: &mut GarbageCollector) {}

            fn visit_slice(_slice: &[Self::SliceType], _gc: &mut GarbageCollector) {}
        }

        let outer = {
            let inner = InternedSlice::from_header_and_slice("abc".to_owned(), &[123, 456, 789]);
            Interned::new_gc(GcInterned(inner)).to_owned()
        };

        let mut gc = GarbageCollector::default();
        gc.add_slice_storage::<StringSlice>();
        gc.add_storage::<GcInterned>();
        unsafe { gc.collect() };

        if !cfg!(feature = "prevent-gc") {
            assert_eq!(<GcInterned as crate::Internable>::storage().get().len(), 1);
            assert_eq!(<StringSlice as crate::SliceInternable>::storage().get().len(), 1);
        }
        assert_eq!(outer.0.header.header, "abc");
        assert_eq!(outer.0.slice, [123, 456, 789]);

        drop(outer);

        let mut gc = GarbageCollector::default();
        gc.add_slice_storage::<StringSlice>();
        gc.add_storage::<GcInterned>();
        unsafe { gc.collect() };

        if !cfg!(feature = "prevent-gc") {
            assert_eq!(<GcInterned as crate::Internable>::storage().get().len(), 0);
            assert_eq!(<StringSlice as crate::SliceInternable>::storage().get().len(), 0);
        }
    }

    #[test]
    fn a_child_whose_owner_is_dropped_while_marking_is_kept() {
        // A live parent is visited while another thread still owns its child, and that owner
        // is dropped before the child's storage is scanned for roots. The child is reachable
        // from the parent, so it must be kept, whatever its count was when the parent saw it.
        #[derive(PartialEq, Eq, Hash)]
        struct Child(u32);

        crate::impl_internable!(gc; Child);

        impl GcInternedVisit for Child {
            fn visit_with(&self, _gc: &mut GarbageCollector) {}
        }

        #[derive(PartialEq, Eq, Hash)]
        struct Parent(InternedRef<'static, Child>);

        crate::impl_internable!(gc; Parent);

        static HOOK: Hook = Mutex::new(None);

        impl GcInternedVisit for Parent {
            fn visit_with(&self, gc: &mut GarbageCollector) {
                if gc.mark_interned_alive(self.0).is_continue() {
                    self.0.visit_with(gc);
                }
                run_hook(&HOOK);
            }
        }

        let child = Interned::new_gc(Child(7)).to_owned();
        let parent = Interned::new_gc(Parent(child.as_ref().change_lifetime())).to_owned();
        let dropper = drop_on_hook(&HOOK, child);

        let mut gc = GarbageCollector::default();
        // The parent's storage is scanned first.
        gc.add_storage::<Parent>();
        gc.add_storage::<Child>();
        unsafe { gc.collect() };
        assert!(hook_ran(&HOOK, dropper), "the parent was visited");

        if !cfg!(feature = "prevent-gc") {
            assert_eq!(Parent::storage().len(), 1);
            // Only the table is read: the child is not dereferenced unless it is still there.
            assert_eq!(Child::storage().len(), 1, "the child of a live parent was freed");
        }
        assert_eq!(parent.0.0, 7);
    }

    #[test]
    fn a_child_slice_whose_owner_is_dropped_while_marking_is_kept() {
        // As above, with the child an interned slice.
        crate::impl_slice_internable!(gc; ChildSlice, u32, u32);
        type InternedSlice = crate::InternedSlice<ChildSlice>;

        impl GcInternedSliceVisit for ChildSlice {
            fn visit_header(_header: &Self::Header, _gc: &mut GarbageCollector) {}

            fn visit_slice(_slice: &[Self::SliceType], _gc: &mut GarbageCollector) {}
        }

        #[derive(PartialEq, Eq, Hash)]
        struct Parent(InternedSliceRef<'static, ChildSlice>);

        crate::impl_internable!(gc; Parent);

        static HOOK: Hook = Mutex::new(None);

        impl GcInternedVisit for Parent {
            fn visit_with(&self, gc: &mut GarbageCollector) {
                if gc.mark_interned_slice_alive(self.0).is_continue() {
                    ChildSlice::visit_header(&self.0.header.header, gc);
                    ChildSlice::visit_slice(&self.0.slice, gc);
                }
                run_hook(&HOOK);
            }
        }

        let child = InternedSlice::from_header_and_slice(7, &[1, 2, 3]).to_owned();
        let parent = Interned::new_gc(Parent(child.as_ref().change_lifetime())).to_owned();
        let dropper = drop_on_hook(&HOOK, child);

        let mut gc = GarbageCollector::default();
        gc.add_storage::<Parent>();
        gc.add_slice_storage::<ChildSlice>();
        unsafe { gc.collect() };
        assert!(hook_ran(&HOOK, dropper), "the parent was visited");

        if !cfg!(feature = "prevent-gc") {
            assert_eq!(Parent::storage().len(), 1);
            assert_eq!(
                <ChildSlice as crate::SliceInternable>::storage().get().len(),
                1,
                "the child of a live parent was freed"
            );
        }
        assert_eq!(parent.0.slice, [1, 2, 3]);
    }

    #[test]
    fn a_child_in_its_parents_storage_whose_owner_is_dropped_while_marking_is_kept() {
        // As above, with the parent and the child in one storage, as a type and the types it
        // holds are. Which of the two is scanned first is the order of the table, so many
        // graphs are tried; scanned child first, the parent's visit finds it marked already.
        struct Node {
            id: u32,
            child: Option<InternedRef<'static, Node>>,
        }

        impl PartialEq for Node {
            fn eq(&self, other: &Self) -> bool {
                self.id == other.id
            }
        }

        impl Eq for Node {}

        impl Hash for Node {
            fn hash<H: Hasher>(&self, state: &mut H) {
                self.id.hash(state)
            }
        }

        crate::impl_internable!(gc; Node);

        static HOOK: Hook = Mutex::new(None);

        impl GcInternedVisit for Node {
            fn visit_with(&self, gc: &mut GarbageCollector) {
                if let Some(child) = self.child {
                    if gc.mark_interned_alive(child).is_continue() {
                        child.visit_with(gc);
                    }
                    run_hook(&HOOK);
                }
            }
        }

        for trial in 0..100 {
            let child = Interned::new_gc(Node { id: 2 * trial, child: None }).to_owned();
            let child_ref = Some(child.as_ref().change_lifetime());
            let parent = Interned::new_gc(Node { id: 2 * trial + 1, child: child_ref }).to_owned();
            let dropper = drop_on_hook(&HOOK, child);

            let mut gc = GarbageCollector::default();
            gc.add_storage::<Node>();
            unsafe { gc.collect() };
            assert!(hook_ran(&HOOK, dropper), "trial {trial}: the parent was visited");

            if !cfg!(feature = "prevent-gc") {
                assert_eq!(
                    Node::storage().len(),
                    2,
                    "trial {trial}: a live parent's child was freed"
                );
            }
            drop(parent);

            let mut gc = GarbageCollector::default();
            gc.add_storage::<Node>();
            unsafe { gc.collect() };
            if !cfg!(feature = "prevent-gc") {
                assert_eq!(Node::storage().len(), 0, "trial {trial}: garbage was kept");
            }
        }
    }

    #[test]
    fn a_cycle_is_visited_once_and_kept_or_freed_whole() {
        // Interning cannot make a value that refers to one interned after it, so the values
        // rust-analyzer interns form no cycle; one with interior mutability can. The marking
        // still ends, visiting each value of the cycle once.
        struct Ring {
            id: u32,
            next: OnceLock<InternedRef<'static, Ring>>,
        }

        impl PartialEq for Ring {
            fn eq(&self, other: &Self) -> bool {
                self.id == other.id
            }
        }

        impl Eq for Ring {}

        impl Hash for Ring {
            fn hash<H: Hasher>(&self, state: &mut H) {
                self.id.hash(state)
            }
        }

        crate::impl_internable!(gc; Ring);

        static VISITS: AtomicUsize = AtomicUsize::new(0);

        impl GcInternedVisit for Ring {
            fn visit_with(&self, gc: &mut GarbageCollector) {
                VISITS.fetch_add(1, Ordering::Relaxed);
                if let Some(&next) = self.next.get()
                    && gc.mark_interned_alive(next).is_continue()
                {
                    next.visit_with(gc);
                }
            }
        }

        let ring: Vec<_> =
            (0..3).map(|id| Interned::new_gc(Ring { id, next: OnceLock::new() })).collect();
        for (at, value) in ring.iter().enumerate() {
            _ = value.next.set(ring[(at + 1) % ring.len()].change_lifetime());
        }
        let owner = ring[1].to_owned();
        drop(ring);

        let mut gc = GarbageCollector::default();
        gc.add_storage::<Ring>();
        unsafe { gc.collect() };
        if !cfg!(feature = "prevent-gc") {
            assert_eq!(VISITS.load(Ordering::Relaxed), 3);
            assert_eq!(Ring::storage().len(), 3);
        }

        drop(owner);
        let mut gc = GarbageCollector::default();
        gc.add_storage::<Ring>();
        unsafe { gc.collect() };
        if !cfg!(feature = "prevent-gc") {
            assert_eq!(Ring::storage().len(), 0);
        }
    }
}
