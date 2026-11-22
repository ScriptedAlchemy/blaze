//! Allocation ownership counters shared by the native codec and local carrier tests.

use std::alloc::{AllocError, Allocator, Global, Layout};
use std::cell::Cell;
use std::ptr::NonNull;

#[derive(Default)]
pub(crate) struct Counts {
    pub(crate) allocations: Cell<usize>,
    pub(crate) deallocations: Cell<usize>,
}

#[derive(Clone, Copy)]
pub(crate) struct Counted<'a>(pub(crate) &'a Counts);

// SAFETY: each allocation and deallocation delegates unchanged to Global. The
// borrowed counters outlive all returned buffers and do not own their storage.
unsafe impl Allocator for Counted<'_> {
    fn allocate(&self, layout: Layout) -> Result<NonNull<[u8]>, AllocError> {
        let allocation = Global.allocate(layout)?;
        self.0.allocations.set(self.0.allocations.get() + 1);
        Ok(allocation)
    }

    unsafe fn deallocate(&self, pointer: NonNull<u8>, layout: Layout) {
        self.0.deallocations.set(self.0.deallocations.get() + 1);
        // SAFETY: this allocator returns only Global's allocations with this layout.
        unsafe { Global.deallocate(pointer, layout) };
    }
}
