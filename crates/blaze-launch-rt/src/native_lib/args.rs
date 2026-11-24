//! The program's C arguments: one writable, process-lived copy for initializers and main.
//! No pointer here borrows libc's `environ` array: `setenv` and `unsetenv` may replace that
//! array after the launcher has removed its own variables. Guest code may retain these
//! pointers in an `atexit` function, so their backing storage ends only with the process.

use core::ffi::{CStr, c_char, c_int};
use core::fmt;

use blaze_own::{Alloc, Vec};

#[cfg(target_os = "linux")]
unsafe extern "C" {
    static mut environ: *mut *mut c_char;
}

/// Which C string list could not be prepared.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ArgumentList {
    Arguments,
    Environment,
}

impl fmt::Display for ArgumentList {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self { Self::Arguments => "argv", Self::Environment => "envp" })
    }
}

/// Why the process's arguments could not be represented as C's start arguments.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ArgsError {
    Count { len: usize },
    Nul { list: ArgumentList, index: usize },
    Size { list: ArgumentList },
    Alloc { what: &'static str },
}

impl fmt::Display for ArgsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Count { len } => write!(f, "the program's {len} arguments do not fit C's argc"),
            Self::Nul { list, index } => write!(f, "the program's {list}[{index}] holds a NUL before its end"),
            Self::Size { list } => write!(f, "the program's {list} does not fit an addressable C string list"),
            Self::Alloc { what } => write!(f, "the program's {what} could not be allocated"),
        }
    }
}

/// The byte strings to copy, without their terminating NUL, and their process-lived allocator.
pub struct ProcessArgsOpts<'a> {
    pub args: &'a [&'a [u8]],
    pub env: &'a [&'a [u8]],
    pub alloc: Alloc<'static>,
}

/// Writable C argument strings and pointer arrays kept until process exit. Access is through
/// raw pointers alone: an initializer may edit a string or replace an argv cell before main,
/// and no Rust reference to those words is kept across that foreign call.
pub struct ProcessArgs {
    argc: c_int,
    argv: *const *const c_char,
    envp: *const *const c_char,
}

impl ProcessArgs {
    pub fn new(opts: ProcessArgsOpts<'_>) -> Result<ProcessArgs, ArgsError> {
        let argc = c_int::try_from(opts.args.len()).map_err(|_| ArgsError::Count { len: opts.args.len() })?;
        let mut size = 0usize;
        for (list, strings) in [(ArgumentList::Arguments, opts.args), (ArgumentList::Environment, opts.env)] {
            for (index, bytes) in strings.iter().enumerate() {
                if bytes.contains(&0) {
                    return Err(ArgsError::Nul { list, index });
                }
                size = size.checked_add(bytes.len()).and_then(|n| n.checked_add(1)).ok_or(ArgsError::Size { list })?;
            }
        }
        let argc_cells = opts.args.len().checked_add(1).ok_or(ArgsError::Size { list: ArgumentList::Arguments })?;
        let env_cells = opts.env.len().checked_add(1).ok_or(ArgsError::Size { list: ArgumentList::Environment })?;
        let mut bytes = Vec::try_with_capacity_in(size, opts.alloc).map_err(|_| ArgsError::Alloc { what: "argv/envp strings" })?;
        let mut argv = Vec::try_with_capacity_in(argc_cells, opts.alloc).map_err(|_| ArgsError::Alloc { what: "argv cells" })?;
        let mut envp = Vec::try_with_capacity_in(env_cells, opts.alloc).map_err(|_| ArgsError::Alloc { what: "envp cells" })?;
        // All room is reserved before a pointer is taken; neither buffer can move as it fills.
        for (strings, cells) in [(opts.args, &mut argv), (opts.env, &mut envp)] {
            for text in strings {
                let at = bytes.len();
                bytes.extend_from_slice(text);
                bytes.push(0);
                // SAFETY: `at` starts the string just appended, including its NUL. Capacity
                // was reserved for both complete lists, so later appends never move it.
                cells.push(unsafe { bytes.as_mut_ptr().add(at) }.cast::<c_char>().cast_const());
            }
            cells.push(core::ptr::null());
        }
        // These are intentional process-lifetime OS arguments, the documented use of
        // leak_static. Constructors, main and atexit may all retain the same writable words.
        let _ = bytes.leak_static();
        let argv = argv.leak_static().as_mut_ptr().cast_const();
        let envp = envp.leak_static().as_mut_ptr().cast_const();
        Ok(ProcessArgs { argc, argv, envp })
    }

    pub fn argc(&self) -> c_int { self.argc }
    pub fn argv(&self) -> *const *const c_char { self.argv }
    pub fn envp(&self) -> *const *const c_char { self.envp }

    /// The native run edge, before its control/reader threads and before any guest code.
    /// The launcher has already stripped its variables; none of its drivers mutates the
    /// environment after that point. Copy every C environment entry, including ones without
    /// `=`, rather than rebuilding the list through the key/value environment API.
    pub(crate) fn capture(alloc: Alloc<'static>) -> Result<ProcessArgs, ArgsError> {
        let owned = Vec::from_iter_in(std::env::args_os(), alloc);
        let args = Vec::from_iter_in(owned.iter().map(|arg| arg.as_encoded_bytes()), alloc);
        let mut env = Vec::new_in(alloc);
        #[cfg(target_os = "macos")]
        // SAFETY: the C runtime's current environment, read before any guest can change it.
        let source = unsafe { *blaze_sys::posix::proc::_NSGetEnviron() };
        #[cfg(target_os = "linux")]
        // SAFETY: libc owns a null-terminated array here; it is copied before guest code
        // starts, and no launcher thread changes the environment during this capture.
        let source = unsafe { environ };
        if !source.is_null() {
            // The C array is NUL-terminated. The address-space limit also bounds the walk;
            // it introduces no quota smaller than a process can represent.
            let most = isize::MAX.unsigned_abs() / size_of::<*const c_char>();
            let mut ended = false;
            for index in 0..most {
                // SAFETY: libc's live array contains each pointer up to its first null.
                let pointer = unsafe { source.add(index).read() };
                if pointer.is_null() { ended = true; break; }
                // SAFETY: each entry of environ is a live NUL-terminated C string.
                env.push(unsafe { CStr::from_ptr(pointer) }.to_bytes());
            }
            if !ended { return Err(ArgsError::Size { list: ArgumentList::Environment }); }
        }
        let copied = ProcessArgs::new(ProcessArgsOpts { args: &args, env: &env, alloc })?;
        // Keep the C entry ABI's aliasing: an initializer editing an envp string also changes
        // what getenv sees. These arrays were never allocated by libc's environment API, so
        // setenv may replace environ but does not own or free our retained original array.
        // SAFETY: this is the single run edge, before guest code and before launcher threads;
        // every cell and string in the installed list is writable and lives until process exit.
        #[cfg(target_os = "linux")]
        unsafe { environ = copied.envp.cast_mut().cast::<*mut c_char>(); }
        #[cfg(target_os = "macos")]
        unsafe { *blaze_sys::posix::proc::_NSGetEnviron() = copied.envp.cast_mut().cast::<*mut c_char>(); }
        Ok(copied)
    }
}
