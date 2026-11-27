//! Scratch directory selection shared by the store and HTTP fixtures.

use core::ffi::{CStr, c_char};

use blaze_wire::{WireAlloc, WireString};

pub(crate) fn scratch_root(alloc: WireAlloc) -> WireString {
    unsafe extern "C" {
        fn getenv(name: *const c_char) -> *const c_char;
    }

    // The sandbox installs its writable TMPDIR before invoking test code. On Linux the
    // kernel's initial environment still contains the path from before that installation.
    // SAFETY: the name is NUL-terminated and these fixtures never modify the environment.
    let value = unsafe { getenv(c"TMPDIR".as_ptr()) };
    let base = if value.is_null() {
        ""
    } else {
        // SAFETY: getenv returned a NUL-terminated value and the fixtures do not change it.
        unsafe { CStr::from_ptr(value) }.to_str().expect("the test's TMPDIR must be UTF-8")
    };
    // Standalone Cargo tests can start without TMPDIR.
    WireString::from_str_in(if base.is_empty() { "/tmp" } else { base }, alloc)
}
