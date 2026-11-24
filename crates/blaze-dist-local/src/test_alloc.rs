//! Allocation ownership counters shared by the native codec and local carrier tests.

use core::alloc::{AllocError, Allocator, Layout};
use core::cell::Cell;
use core::ptr::NonNull;

#[derive(Default)]
pub(crate) struct Counts {
    pub(crate) allocations: Cell<usize>,
    pub(crate) deallocations: Cell<usize>,
}

#[derive(Clone, Copy)]
pub(crate) struct Counted<'a>(pub(crate) &'a Counts);

// SAFETY: each allocation and deallocation delegates unchanged to `blaze_own::testing::heap()`.
// The borrowed counters outlive all returned buffers and do not own their storage.
unsafe impl Allocator for Counted<'_> {
    fn allocate(&self, layout: Layout) -> Result<NonNull<[u8]>, AllocError> {
        let heap = blaze_own::testing::heap().shared();
        let allocation = heap.allocate(layout)?;
        self.0.allocations.set(self.0.allocations.get() + 1);
        Ok(allocation)
    }

    unsafe fn deallocate(&self, pointer: NonNull<u8>, layout: Layout) {
        self.0.deallocations.set(self.0.deallocations.get() + 1);
        let heap = blaze_own::testing::heap().shared();
        // SAFETY: this allocator returns only the testing heap's allocations with this layout.
        unsafe { heap.deallocate(pointer, layout) };
    }
}
