//! The loaded ELF image's TLS geometry, lent by ld.so. The core uses this fact to refuse a
//! Rust pointer in a TLS template: an initial-exec allocation may already have copied its
//! zero word before dlopen returns. No image code runs here and no relocation is reimplemented.

use core::ffi::{c_int, c_void};

use blaze_launch::wire::RecordId;

use super::{NativeError, ThreadData};

struct Find {
    base: u64,
    found: bool,
    malformed: bool,
    tls: Option<ThreadData>,
}

unsafe extern "C" fn visit(info: *mut libc::dl_phdr_info, size: usize, data: *mut c_void) -> c_int {
    // The four original dl_phdr_info fields, present since this API was introduced. Later
    // libc versions append fields; none is read here.
    if size < core::mem::offset_of!(libc::dl_phdr_info, dlpi_phnum) + size_of::<libc::Elf64_Half>() {
        return 0;
    }
    // SAFETY: data is our live Find. Read only the checked prefix through raw field pointers:
    // a libc with a shorter info structure need not supply Rust's whole current struct size.
    let find = unsafe { &mut *data.cast::<Find>() };
    let base = unsafe { (&raw const (*info).dlpi_addr).read() };
    if base != find.base { return 0; }
    find.found = true;
    // SAFETY: both fields end inside the prefix whose byte size was checked above.
    let headers = unsafe { (&raw const (*info).dlpi_phdr).read() };
    let count = unsafe { (&raw const (*info).dlpi_phnum).read() };
    if count != 0 && headers.is_null() {
        find.malformed = true;
        return 1;
    }
    for index in 0..usize::from(count) {
        // SAFETY: ld.so's counted program-header array is readable throughout this callback.
        let header = unsafe { &*headers.add(index) };
        if header.p_type == libc::PT_TLS {
            if find.tls.is_some() || header.p_vaddr.checked_add(header.p_memsz).is_none() {
                find.malformed = true;
                return 1;
            }
            find.tls = Some(ThreadData { at: header.p_vaddr, len: header.p_memsz });
        }
    }
    1
}

pub(super) fn thread_data(record: RecordId, base: u64) -> Result<Option<ThreadData>, NativeError> {
    let mut find = Find { base, found: false, malformed: false, tls: None };
    // SAFETY: the callback keeps no loader pointer and writes only this stack local; ld.so
    // calls it synchronously. The native_so writer's first PT_LOAD has vaddr 0, so dladdr's
    // image base is this object's dlpi_addr (its load bias).
    unsafe { libc::dl_iterate_phdr(Some(visit), (&raw mut find).cast()) };
    if !find.found {
        return Err(NativeError::Layout { record, what: "ld.so listed no image at the descriptor's base" });
    }
    if find.malformed {
        return Err(NativeError::Layout { record, what: "missing program headers, more than one PT_TLS, or an overflowing TLS range" });
    }
    Ok(find.tls)
}
