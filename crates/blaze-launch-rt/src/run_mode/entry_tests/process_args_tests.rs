//! Process-global C environment behavior runs in a child, isolated from parallel test readers.
//! The child exits from its atexit callback only after checking pointers escaped from main.

use core::ffi::{CStr, c_char, c_int};
use core::sync::atomic::{AtomicPtr, Ordering};
use super::{Loaded, ProcessArgs};
use crate::driver_tests::{ArgumentCase, Case};

const PREFIX: &[u8] = b"BLAZE_ARGS_PROBE=";
static ESCAPED_ARGV: AtomicPtr<*const c_char> = AtomicPtr::new(core::ptr::null_mut());
static ESCAPED_ENV: AtomicPtr<c_char> = AtomicPtr::new(core::ptr::null_mut());

#[cfg(target_os = "linux")]
unsafe extern "C" { static mut environ: *mut *mut c_char; }

/// This child is the only caller, with no other test running or guest thread started.
fn environment() -> *mut *mut c_char {
    #[cfg(target_os = "linux")]
    // SAFETY: libc's environment word, read only in the isolated child's setup.
    unsafe { environ }
    #[cfg(target_os = "macos")]
    // SAFETY: the C runtime's environment word, read only in the isolated child's setup.
    unsafe { *blaze_sys::posix::proc::_NSGetEnviron() }
}

fn restore_environment(saved: *mut *mut c_char) {
    // SAFETY: the child's pre-capture list remains allocated; replacing the global word does
    // not free it. This is the planted omission of installation, before any guest thread.
    #[cfg(target_os = "linux")]
    unsafe { environ = saved; }
    #[cfg(target_os = "macos")]
    unsafe { *blaze_sys::posix::proc::_NSGetEnviron() = saved; }
}

extern "C" fn edit_environment(argc: c_int, argv: *const *const c_char, envp: *const *const c_char) {
    if argc < 1 { return; }
    // SAFETY: the child was launched with exactly CASE and PROBE, then removed its temporary
    // third key before capture. Both strings/arrays and argv[0] are writable process copies.
    unsafe {
        argv.read().cast_mut().write(b'Q' as c_char);
        for index in 0..2 {
            let text = envp.add(index).read();
            if CStr::from_ptr(text).to_bytes().starts_with(PREFIX) {
                text.cast_mut().add(PREFIX.len()).write(b'B' as c_char);
            }
        }
    }
}

extern "C" fn escaped_at_exit() {
    let argv = ESCAPED_ARGV.load(Ordering::Acquire);
    let env = ESCAPED_ENV.load(Ordering::Acquire);
    // SAFETY: capture's original arrays and strings live through process exit. These pointers
    // were saved before libc replaced environ; no saved pointer refers to a later libc-owned
    // replacement string, whose lifetime would instead follow setenv/unsetenv's rules.
    let intact = unsafe {
        !argv.is_null() && !env.is_null()
            && argv.read().read() == b'Q' as c_char
            && CStr::from_ptr(env).to_bytes() == b"BLAZE_ARGS_PROBE=Before"
    };
    // SAFETY: this ends only the isolated child. A distinctive success proves atexit ran;
    // ordinary exit(0) or a skipped callback cannot pass the parent assertion.
    unsafe { libc::_exit(if intact { 42 } else { 86 }) }
}

/// The shared process dispatcher invokes this body in an isolated child with exactly the
/// case selector and PROBE environment entries. Its status is checked by the parent.
pub(crate) fn child_case(case: ArgumentCase) -> i32 {
    // SAFETY: this isolated child has one test; it alone changes its environment. Force libc
    // to own an array before capture, then remove a launcher-like variable, as run setup does.
    unsafe {
        assert_eq!(libc::setenv(c"BLAZE_ARGS_TEMP".as_ptr(), c"temporary".as_ptr(), 1), 0);
        assert_eq!(libc::unsetenv(c"BLAZE_ARGS_TEMP".as_ptr()), 0);
    }
    let original = environment();
    {
        let args = ProcessArgs::capture(blaze_own::testing::heap().alloc()).expect("the process argument capture");
        if case == ArgumentCase::OldEnvironment { restore_environment(original); }
        let inits = [edit_environment as usize as u64];
        crate::native_lib::run_inits(Loaded {
            record: blaze_launch::wire::RecordId([0x73; 32]), base: 0,
            inits_at: inits.as_ptr() as u64, inits: 1,
        }, &args).expect("the C initializer");
        // SAFETY: this key is supplied by the parent and remains present after the initializer.
        let value = unsafe { libc::getenv(c"BLAZE_ARGS_PROBE".as_ptr()) };
        assert!(!value.is_null());
        // SAFETY: getenv returned the live value of the child's key; no concurrent mutation.
        let text = unsafe { CStr::from_ptr(value) }.to_bytes();
        if case == ArgumentCase::OldEnvironment {
            assert_eq!(text, b"before", "without installation, envp edits fail to reach getenv");
            return 43;
        }
        assert_eq!(text, b"Before", "the constructor's envp edit reaches getenv");
        assert_eq!(environment().cast_const().cast::<*const c_char>(), args.envp());
        let mut saved = core::ptr::null_mut();
        // SAFETY: the child's two captured environment entries and null are still live.
        unsafe {
            assert!(args.envp().add(2).read().is_null());
            for index in 0..2 {
                let text = args.envp().add(index).read();
                if CStr::from_ptr(text).to_bytes().starts_with(PREFIX) {
                    saved = text.cast_mut();
                }
            }
        }
        assert!(!saved.is_null());
        ESCAPED_ARGV.store(args.argv().cast_mut(), Ordering::Release);
        ESCAPED_ENV.store(saved, Ordering::Release);
        // SAFETY: this child's next libc environment growth copies our externally owned
        // array. Removing the key afterwards must not free the original process-owned bytes.
        unsafe {
            assert_eq!(libc::setenv(c"BLAZE_ARGS_ADDED".as_ptr(), c"added".as_ptr(), 1), 0);
            assert_ne!(environment().cast_const().cast::<*const c_char>(), args.envp());
            assert_eq!(libc::unsetenv(c"BLAZE_ARGS_PROBE".as_ptr()), 0);
            assert!(libc::getenv(c"BLAZE_ARGS_PROBE".as_ptr()).is_null());
            assert_eq!(CStr::from_ptr(saved).to_bytes(), b"BLAZE_ARGS_PROBE=Before");
            assert_eq!(libc::atexit(escaped_at_exit), 0);
        }
    }
    // ProcessArgs metadata has ended before the callback reads its escaped pointers.
    0
}

#[test]
fn installed_environment_aliases_getenv_and_escaped_arguments_survive_atexit() {
    let installed = crate::driver_tests::run(Case::Arguments(ArgumentCase::Installed));
    assert_eq!(installed.status.code(), Some(42), "the atexit observer checked the original process-owned storage: {}", installed.stderr);
    let restored = crate::driver_tests::run(Case::Arguments(ArgumentCase::OldEnvironment));
    assert_eq!(restored.status.code(), Some(43), "restoring the old environ removes the constructor/getenv alias: {}", restored.stderr);
}
