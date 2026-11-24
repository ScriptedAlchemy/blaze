//! Thin `#![no_std]` OS bindings over `blaze-sys` for clocks, files, Unix and TCP sockets,
//! locks, bounded channels, scoped threads, and process arguments/environment.

use core::alloc::Allocator;
use core::cell::UnsafeCell;
use core::ffi::{c_char, c_void};
use core::fmt;
use core::marker::PhantomData;
use core::net::{Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4, SocketAddrV6};
use core::ops::{Deref, DerefMut};
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use core::time::Duration;

use blaze_int::Int as _;
use blaze_own::{Arc, Box, String, Vec, VecDeque, format_in};
pub(crate) use blaze_sys::posix::io::{OwnedFd, PollEvents, PollFd};
use blaze_sys::posix::io::{
    CWD, ClockId, OpenFlags, Timeval, clock_gettime as sys_read_clock, nanosleep, openat,
    poll as sys_poll, read as sys_read, set_nonblocking as sys_set_nonblocking,
    write as sys_write,
};
#[cfg(test)]
use blaze_sys::posix::io::{
    close_on_exec as sys_cloexec, status_flags as sys_status_flags,
};
use blaze_sys::posix::net::{
    AF_INET, AF_INET6, AF_UNIX, IPPROTO_TCP, MsgFlags, SOCK_STREAM, SOL_SOCKET, SO_RCVTIMEO,
    SO_REUSEADDR, SO_SNDTIMEO, TCP_NODELAY, bind as sys_bind, connect as sys_connect,
    getsockname as sys_getsockname, in_addr, in6_addr, listen as sys_listen, pending_error,
    recv as sys_recv, send as sys_send, setsockopt_bytes, setsockopt_int, sockaddr_in,
    sockaddr_in6, sockaddr_storage, sockaddr_un, socket as sys_socket,
};
use blaze_sys::posix::proc::{
    AT_SYMLINK_NOFOLLOW, S_IFDIR, S_IFLNK, S_IFMT, S_IFREG, Unlink, rename_at, stat_at, unlink_at,
};
use blaze_sys::posix::types::{sa_family_t, socklen_t};
pub use blaze_sys::Errno;
pub(crate) use blaze_sys::{Fd, Timespec};
use blaze_wire::{Digest, WireAlloc, WireString};

#[cfg(target_os = "linux")]
use blaze_sys::linux::{
    LockOp, SockFlags, accept4, fchmod as linux_fchmod, flock as sys_flock, futex_wait_bitset,
    futex_wake, getdents64, mkdirat,
};
#[cfg(target_os = "linux")]
use blaze_sys::posix::net::{SOCK_CLOEXEC, SOCK_NONBLOCK};
#[cfg(target_os = "macos")]
use blaze_sys::posix::net::SO_NOSIGPIPE;

/// Maximum path buffer length (including NUL terminator) for stack-allocated syscall paths.
const PATH_BUF_CAP: usize = 4096;

/// Maximum `EINTR` retries on a single blocking syscall (Rule 18: bounded progress).
const MAX_INTR_RETRIES: usize = 64;

/// Maximum write-loop iterations for a single `write_all` call.
const MAX_WRITE_ITERS: usize = 1024 * 1024;

/// Default file creation mode (`0o644`).
pub(crate) const FILE_MODE: u32 = 0o644;

/// Default directory creation mode (`0o755`).
pub(crate) const DIR_MODE: u32 = 0o755;

/// Default listen backlog for Unix and TCP sockets.
const LISTEN_BACKLOG: i32 = 128;

// ---------------------------------------------------------------------------------------------
// Typed OS error
// ---------------------------------------------------------------------------------------------

/// Typed OS IO error carrying a kernel [`Errno`] and optional owned context message.
#[derive(Clone, Debug)]
pub struct IoError {
    errno: Errno,
    message: Option<WireString>,
}

impl IoError {
    /// Creates an IO error from a bare kernel errno.
    pub const fn from_errno(errno: Errno) -> Self {
        Self { errno, message: None }
    }

    /// Creates an IO error with a descriptive context message.
    pub const fn with_message(errno: Errno, message: WireString) -> Self {
        Self { errno, message: Some(message) }
    }

    /// Underlying kernel error code.
    pub const fn errno(&self) -> Errno {
        self.errno
    }

    /// Whether the underlying errno is `EAGAIN` / `EWOULDBLOCK`.
    pub const fn is_would_block(&self) -> bool {
        matches!(self.errno, Errno::Again)
    }

    /// Whether the underlying errno is `ENOENT`.
    pub const fn is_not_found(&self) -> bool {
        matches!(self.errno, Errno::NoEnt)
    }

    /// Whether the underlying errno is `EEXIST`.
    pub const fn is_already_exists(&self) -> bool {
        matches!(self.errno, Errno::Exist)
    }

    /// Whether the underlying errno is `ETIMEDOUT`.
    pub const fn is_timed_out(&self) -> bool {
        matches!(self.errno, Errno::TimedOut)
    }

    /// Whether the underlying errno is `EINVAL`.
    pub const fn is_invalid_input(&self) -> bool {
        matches!(self.errno, Errno::Inval)
    }

    /// Whether the underlying errno is `EINTR`.
    pub const fn is_interrupted(&self) -> bool {
        matches!(self.errno, Errno::Intr)
    }

    /// Whether an `accept` error is transient and leaves the listener healthy.
    pub const fn is_transient_accept(&self) -> bool {
        matches!(self.errno, Errno::Again | Errno::Intr | Errno::ConnAborted | Errno::ConnReset)
    }
}

impl From<Errno> for IoError {
    fn from(errno: Errno) -> Self {
        Self::from_errno(errno)
    }
}

impl fmt::Display for IoError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(message) = &self.message {
            f.write_str(message.as_str())
        } else {
            write!(f, "{} ({})", self.errno.what(), self.errno.name())
        }
    }
}

impl core::error::Error for IoError {}

// ---------------------------------------------------------------------------------------------
// Clock, sleep, entropy, PID, args, env, stdio
// ---------------------------------------------------------------------------------------------

/// Monotonic system clock capability over `blaze-sys`.
#[derive(Clone, Copy, Default, Debug)]
pub struct SystemClock;

impl SystemClock {
    /// Current monotonic instant in nanoseconds.
    pub fn monotonic_nanos(self) -> Result<u64, Errno> {
        timespec_to_nanos(sys_read_clock(ClockId::Monotonic)?)
    }

    /// Current monotonic instant in milliseconds.
    pub fn monotonic_ms(self) -> u64 {
        self.monotonic_nanos().map_or(0, |ns| ns / 1_000_000)
    }
}

/// Converts a non-negative [`Timespec`] to nanoseconds, saturating at `u64::MAX`.
pub(crate) fn timespec_to_nanos(ts: Timespec) -> Result<u64, Errno> {
    let secs = u64::try_from(ts.sec).map_err(|_| Errno::Inval)?;
    let nsecs = u64::try_from(ts.nsec).map_err(|_| Errno::Inval)?;
    Ok(secs.saturating_mul(1_000_000_000).saturating_add(nsecs))
}

/// Converts a [`Duration`] into a [`Timespec`].
pub(crate) fn duration_to_timespec(duration: Duration) -> Timespec {
    let sec = i64::try_from(duration.as_secs()).unwrap_or(i64::MAX);
    let nsec = i64::from(duration.subsec_nanos());
    Timespec { sec, nsec }
}

/// Converts a [`Duration`] into nanoseconds clamped to `u64::MAX`.
pub(crate) fn duration_to_nanos(duration: Duration) -> u64 {
    u64::try_from(duration.as_nanos()).unwrap_or(u64::MAX)
}

/// Sleeps for `duration` using `nanosleep`, retrying `EINTR` up to a bounded cap.
pub fn sleep(duration: Duration) {
    if duration.is_zero() {
        return;
    }
    let ts = duration_to_timespec(duration);
    for _ in 0..MAX_INTR_RETRIES {
        match nanosleep(ts) {
            Ok(()) => return,
            Err(Errno::Intr) => continue,
            Err(_) => return,
        }
    }
}

/// Sleeps for `ms` milliseconds.
pub fn sleep_ms(ms: u64) {
    sleep(Duration::from_millis(ms));
}

/// Reads 32 OS-random bytes from `/dev/urandom`.
pub fn random_id() -> Result<Digest, IoError> {
    let fd = open_read("/dev/urandom")?;
    let mut id = [0u8; 32];
    let mut offset = 0usize;
    let mut iterations = 0usize;
    while offset < id.len() {
        iterations = iterations.saturating_add(1);
        if iterations > MAX_INTR_RETRIES {
            return Err(IoError::from_errno(Errno::Io));
        }
        let Some(dst) = id.get_mut(offset..) else {
            return Err(IoError::from_errno(Errno::Io));
        };
        match sys_read(fd.fd(), dst) {
            Ok(0) => return Err(IoError::from_errno(Errno::Io)),
            Ok(n) => offset = offset.saturating_add(n),
            Err(Errno::Intr) => continue,
            Err(errno) => return Err(IoError::from_errno(errno)),
        }
    }
    Ok(id)
}

/// Current process identifier from the kernel.
pub fn current_pid() -> u32 {
    u32::try_from(blaze_sys::posix::thread::getpid()).unwrap_or(0)
}

/// Writes `bytes` to `fd` (best-effort loop for stdout/stderr reporting).
pub(crate) fn write_fd_best_effort(fd: Fd, mut bytes: &[u8]) {
    let mut attempts = 0usize;
    while !bytes.is_empty() && attempts < MAX_WRITE_ITERS {
        attempts = attempts.saturating_add(1);
        match sys_write(fd, bytes) {
            Ok(0) => break,
            Ok(wrote) => match bytes.get(wrote..) {
                Some(rest) => bytes = rest,
                None => break,
            },
            Err(Errno::Intr) => continue,
            Err(_) => break,
        }
    }
}

/// Writes `bytes` to standard output (`fd 1`).
pub fn write_stdout(bytes: &[u8]) {
    write_fd_best_effort(Fd::new(1), bytes);
}

/// Writes `bytes` to standard error (`fd 2`).
pub fn write_stderr(bytes: &[u8]) {
    write_fd_best_effort(Fd::STDERR, bytes);
}

/// Reads one `\n`-terminated line (without trailing `\r`/`\n`) from `fd`, or `Ok(None)` at EOF.
pub fn read_line_fd<A: Allocator>(fd: Fd, alloc: A) -> Result<Option<String<A>>, IoError> {
    let mut raw = Vec::new_in(alloc);
    let mut byte = [0u8; 1];
    loop {
        match read_some(fd, &mut byte) {
            Ok(0) => {
                if raw.is_empty() {
                    return Ok(None);
                }
                break;
            }
            Ok(_) => {
                if byte[0] == b'\n' {
                    break;
                }
                if raw.len() >= 64 * 1024 {
                    return Err(IoError::from_errno(Errno::NameTooLong));
                }
                raw.push(byte[0]);
            }
            Err(err) => return Err(IoError::from_errno(err)),
        }
    }
    if raw.last() == Some(&b'\r') {
        let _ = raw.pop();
    }
    String::from_utf8(raw).map(Some).map_err(|_| IoError::from_errno(Errno::Inval))
}

/// Reads one line from standard input (`fd 0`).
pub fn read_stdin_line<A: Allocator>(alloc: A) -> Result<Option<String<A>>, IoError> {
    read_line_fd(Fd::new(0), alloc)
}

/// Reads process command-line arguments without `std::env::args()`.
pub fn process_args<A: Allocator + Clone>(alloc: A) -> Result<Vec<String<A>, A>, Errno> {
    let mut args = Vec::new_in(alloc.clone());
    #[cfg(target_os = "linux")]
    {
        let fd = open_read("/proc/self/cmdline")?;
        let mut raw = Vec::new_in(alloc.clone());
        let mut buf = [0u8; 4096];
        loop {
            let n = read_some(fd.fd(), &mut buf)?;
            if n == 0 {
                break;
            }
            if let Some(chunk) = buf.get(..n) {
                raw.extend_from_slice(chunk);
            }
            if raw.len() >= 1024 * 1024 {
                return Err(Errno::NameTooLong);
            }
        }
        let mut start = 0usize;
        let mut idx = 0usize;
        while let Some(&b) = raw.get(idx) {
            if b == 0 {
                if let Some(slice) = raw.get(start..idx) {
                    let s = core::str::from_utf8(slice).map_err(|_| Errno::Inval)?;
                    let mut owned = String::with_capacity_in(s.len(), alloc.clone());
                    owned.push_str(s);
                    args.push(owned);
                }
                start = idx.saturating_add(1);
            }
            idx = idx.saturating_add(1);
        }
    }
    #[cfg(target_os = "macos")]
    {
        // SAFETY: `_NSGetArgc` and `_NSGetArgv` return valid pointers to dyld's argc/argv.
        unsafe {
            let argc_ptr = blaze_sys::posix::proc::_NSGetArgc();
            let argv_ptr = blaze_sys::posix::proc::_NSGetArgv();
            if argc_ptr.is_null() || argv_ptr.is_null() {
                return Err(Errno::Inval);
            }
            let argc = usize::try_from(*argc_ptr).map_err(|_| Errno::Inval)?;
            let argv = *argv_ptr;
            if argv.is_null() {
                return Err(Errno::Inval);
            }
            for i in 0..argc {
                let p = *argv.add(i);
                if p.is_null() {
                    break;
                }
                let cstr = core::ffi::CStr::from_ptr(p);
                let s = cstr.to_str().map_err(|_| Errno::Inval)?;
                let mut owned = String::with_capacity_in(s.len(), alloc.clone());
                owned.push_str(s);
                args.push(owned);
            }
        }
    }
    Ok(args)
}

/// Reads an environment variable by UTF-8 name without touching `std::env`.
pub fn env_var<A: Allocator + Clone>(name: &str, alloc: A) -> Option<String<A>> {
    if name.is_empty() || name.contains('=') || name.contains('\0') {
        return None;
    }
    #[cfg(target_os = "linux")]
    {
        let fd = open_read("/proc/self/environ").ok()?;
        let mut raw = Vec::new_in(alloc.clone());
        let mut buf = [0u8; 4096];
        loop {
            let n = read_some(fd.fd(), &mut buf).ok()?;
            if n == 0 {
                break;
            }
            if let Some(chunk) = buf.get(..n) {
                raw.extend_from_slice(chunk);
            }
            if raw.len() >= 4 * 1024 * 1024 {
                break;
            }
        }
        let target = name.as_bytes();
        let mut start = 0usize;
        let mut idx = 0usize;
        while let Some(&b) = raw.get(idx) {
            if b == 0 {
                if let Some(entry) = raw.get(start..idx)
                    && let Some(rest) = entry.strip_prefix(target)
                    && let Some(val_bytes) = rest.strip_prefix(b"=")
                    && let Ok(val) = core::str::from_utf8(val_bytes)
                {
                    let mut owned = String::with_capacity_in(val.len(), alloc);
                    owned.push_str(val);
                    return Some(owned);
                }
                start = idx.saturating_add(1);
            }
            idx = idx.saturating_add(1);
        }
        None
    }
    #[cfg(target_os = "macos")]
    {
        let target = name.as_bytes();
        // SAFETY: `_NSGetEnviron` returns a pointer to the process's NULL-terminated environ array.
        unsafe {
            let env_ptr = blaze_sys::posix::proc::_NSGetEnviron();
            if env_ptr.is_null() {
                return None;
            }
            let mut cursor = *env_ptr;
            if cursor.is_null() {
                return None;
            }
            let mut count = 0usize;
            while count < 65_536 {
                let entry_ptr = *cursor;
                if entry_ptr.is_null() {
                    break;
                }
                let bytes = core::ffi::CStr::from_ptr(entry_ptr).to_bytes();
                if let Some(rest) = bytes.strip_prefix(target)
                    && let Some(val_bytes) = rest.strip_prefix(b"=")
                    && let Ok(val) = core::str::from_utf8(val_bytes)
                {
                    let mut owned = String::with_capacity_in(val.len(), alloc);
                    owned.push_str(val);
                    return Some(owned);
                }
                cursor = cursor.add(1);
                count = count.saturating_add(1);
            }
        }
        None
    }
}

/// Whether the named environment variable is present (without copying its value).
pub(crate) fn env_is_set(name: &str, alloc: WireAlloc) -> bool {
    env_var(name, alloc).is_some()
}

// ---------------------------------------------------------------------------------------------
// Paths, files, directories, flock
// ---------------------------------------------------------------------------------------------

/// Invokes `f` with a stack-allocated NUL-terminated [`blaze_sys::Path`].
fn with_cpath<T>(
    path: &str,
    f: impl FnOnce(blaze_sys::Path<'_>) -> Result<T, Errno>,
) -> Result<T, Errno> {
    let mut buf = [0u8; PATH_BUF_CAP];
    let cpath = blaze_sys::Path::in_buffer(path.as_bytes(), &mut buf)?;
    f(cpath)
}

/// Joins two path segments with `/` into a new `String<A>`.
pub fn join_path<A: Allocator>(base: &str, child: &str, alloc: A) -> String<A> {
    let mut out =
        String::with_capacity_in(base.len().saturating_add(child.len()).saturating_add(1), alloc);
    out.push_str(base);
    if !base.is_empty() && !base.ends_with('/') && !child.starts_with('/') {
        out.push('/');
    }
    out.push_str(child);
    out
}

/// File metadata extracted from `lstat`.
#[derive(Clone, Copy, Debug)]
pub(crate) struct FileStat {
    pub(crate) dev: u64,
    pub(crate) ino: u64,
    pub(crate) is_file: bool,
    pub(crate) is_dir: bool,
    pub(crate) is_symlink: bool,
}

/// `lstat` on `path`.
pub(crate) fn stat_path(path: &str) -> Result<FileStat, Errno> {
    let st = with_cpath(path, |cpath| stat_at(CWD, cpath, AT_SYMLINK_NOFOLLOW))?;
    let kind = st.st_mode & S_IFMT;
    Ok(FileStat {
        dev: u64::try_from(st.st_dev).unwrap_or(0),
        ino: u64::try_from(st.st_ino).unwrap_or(0),
        is_file: kind == S_IFREG,
        is_dir: kind == S_IFDIR,
        is_symlink: kind == S_IFLNK,
    })
}

/// Whether `path` exists according to `lstat`.
pub fn path_exists(path: &str) -> bool {
    stat_path(path).is_ok()
}

/// Whether `path` is a regular file according to `lstat`.
pub fn path_is_file(path: &str) -> bool {
    stat_path(path).is_ok_and(|st| st.is_file)
}

/// Opens an existing file read-only (`O_CLOEXEC`).
pub(crate) fn open_read(path: &str) -> Result<OwnedFd, Errno> {
    with_cpath(path, |cpath| {
        openat(CWD, cpath, OpenFlags::READ.with(OpenFlags::CLOSE_ON_EXEC), 0)
    })
}

/// Creates a brand-new file (`O_WRONLY | O_CREAT | O_EXCL | O_CLOEXEC`).
pub(crate) fn open_create_new(path: &str) -> Result<OwnedFd, Errno> {
    let flags = OpenFlags::WRITE
        .with(OpenFlags::CREATE)
        .with(OpenFlags::EXCLUSIVE)
        .with(OpenFlags::CLOSE_ON_EXEC);
    with_cpath(path, |cpath| openat(CWD, cpath, flags, FILE_MODE))
}

/// Opens or creates a file for appending (`O_WRONLY | O_CREAT | O_APPEND | O_CLOEXEC`).
pub(crate) fn open_create_append(path: &str) -> Result<OwnedFd, Errno> {
    let flags = OpenFlags::WRITE
        .with(OpenFlags::CREATE)
        .with(OpenFlags::APPEND)
        .with(OpenFlags::CLOSE_ON_EXEC);
    with_cpath(path, |cpath| openat(CWD, cpath, flags, FILE_MODE))
}

/// Creates or truncates a file for writing (`O_WRONLY | O_CREAT | O_TRUNC | O_CLOEXEC`).
pub(crate) fn open_create_trunc(path: &str) -> Result<OwnedFd, Errno> {
    let flags = OpenFlags::WRITE
        .with(OpenFlags::CREATE)
        .with(OpenFlags::TRUNCATE)
        .with(OpenFlags::CLOSE_ON_EXEC);
    with_cpath(path, |cpath| openat(CWD, cpath, flags, FILE_MODE))
}

/// Reads sequentially from `fd`, retrying `EINTR`.
pub(crate) fn read_some(fd: Fd, buf: &mut [u8]) -> Result<usize, Errno> {
    for _ in 0..MAX_INTR_RETRIES {
        match sys_read(fd, buf) {
            Ok(n) => return Ok(n),
            Err(Errno::Intr) => continue,
            Err(err) => return Err(err),
        }
    }
    Err(Errno::Intr)
}

/// Writes all of `bytes` to `fd`, retrying `EINTR` and short writes.
pub(crate) fn write_all_fd(fd: Fd, mut bytes: &[u8]) -> Result<(), Errno> {
    let mut iters = 0usize;
    while !bytes.is_empty() {
        if iters >= MAX_WRITE_ITERS {
            return Err(Errno::Again);
        }
        iters = iters.saturating_add(1);
        match sys_write(fd, bytes) {
            Ok(0) => return Err(Errno::Pipe),
            Ok(wrote) => {
                bytes = bytes.get(wrote..).ok_or(Errno::Inval)?;
            }
            Err(Errno::Intr) => continue,
            Err(err) => return Err(err),
        }
    }
    Ok(())
}

/// Reads an entire file at `path` into a caller-allocated `Vec<u8, A>`.
pub fn read_file<A: Allocator>(path: &str, alloc: A) -> Result<Vec<u8, A>, IoError> {
    let fd = open_read(path).map_err(IoError::from_errno)?;
    let mut out = Vec::new_in(alloc);
    let mut buf = [0u8; 8192];
    loop {
        let n = read_some(fd.fd(), &mut buf).map_err(IoError::from_errno)?;
        if n == 0 {
            break;
        }
        if let Some(chunk) = buf.get(..n) {
            out.extend_from_slice(chunk);
        }
    }
    Ok(out)
}

/// Writes all of `bytes` to `path`, creating or truncating it.
pub fn write_file(path: &str, bytes: &[u8]) -> Result<(), IoError> {
    let fd = open_create_trunc(path).map_err(IoError::from_errno)?;
    write_all_fd(fd.fd(), bytes).map_err(IoError::from_errno)
}

/// Atomically renames `from` to `to`.
pub(crate) fn rename_path(from: &str, to: &str) -> Result<(), Errno> {
    let mut from_buf = [0u8; PATH_BUF_CAP];
    let mut to_buf = [0u8; PATH_BUF_CAP];
    let from_c = blaze_sys::Path::in_buffer(from.as_bytes(), &mut from_buf)?;
    let to_c = blaze_sys::Path::in_buffer(to.as_bytes(), &mut to_buf)?;
    rename_at(CWD, from_c, CWD, to_c)
}

/// Unlinks a file or symlink at `path`.
pub fn unlink_path(path: &str) -> Result<(), Errno> {
    with_cpath(path, |cpath| unlink_at(CWD, cpath, Unlink::File))
}

/// Removes an empty directory at `path`.
pub fn remove_empty_dir(path: &str) -> Result<(), Errno> {
    with_cpath(path, |cpath| unlink_at(CWD, cpath, Unlink::Dir))
}

/// Creates a single directory at `path` with `mode`.
pub fn create_dir(path: &str, mode: u32) -> Result<(), Errno> {
    with_cpath(path, |cpath| {
        #[cfg(target_os = "linux")]
        {
            mkdirat(CWD, cpath, mode)
        }
        #[cfg(target_os = "macos")]
        {
            let m = u16::try_from(mode).map_err(|_| Errno::Inval)?;
            // SAFETY: `cpath` is a live NUL-terminated path the kernel reads and does not keep.
            let rc = unsafe { blaze_sys::macos::cold::mkdir(cpath.as_ptr().cast(), m) };
            if rc < 0 { Err(blaze_sys::posix::signal::errno()) } else { Ok(()) }
        }
    })
}

/// Creates `path` and any missing parent directories iteratively (no recursion).
pub fn create_dir_all(path: &str) -> Result<(), Errno> {
    if path.is_empty() {
        return Err(Errno::Inval);
    }
    let bytes = path.as_bytes();
    let mut pos = 0usize;
    while pos < bytes.len() {
        while bytes.get(pos) == Some(&b'/') {
            pos = pos.saturating_add(1);
        }
        if pos >= bytes.len() {
            break;
        }
        while let Some(&b) = bytes.get(pos) {
            if b == b'/' {
                break;
            }
            pos = pos.saturating_add(1);
        }
        if let Some(prefix) = path.get(..pos) {
            match create_dir(prefix, DIR_MODE) {
                Ok(()) | Err(Errno::Exist) => {}
                Err(err) => return Err(err),
            }
        }
    }
    Ok(())
}

/// Opens a directory descriptor (`O_RDONLY | O_DIRECTORY | O_CLOEXEC`).
fn open_dir(path: &str) -> Result<OwnedFd, Errno> {
    let flags =
        OpenFlags::READ.with(OpenFlags::DIRECTORY).with(OpenFlags::CLOSE_ON_EXEC);
    with_cpath(path, |cpath| openat(CWD, cpath, flags, 0))
}

/// Lists all entry names inside directory `path`, skipping `.` and `..`.
fn list_dir<A: Allocator + Clone>(path: &str, alloc: A) -> Result<Vec<String<A>, A>, Errno> {
    let dir_fd = open_dir(path)?;
    let mut entries = Vec::new_in(alloc.clone());
    #[cfg(target_os = "linux")]
    {
        let mut buf = [0u8; 8192];
        loop {
            let mut count = 0usize;
            for entry in getdents64(dir_fd.fd(), &mut buf)? {
                count = count.saturating_add(1);
                if entry.name == b"." || entry.name == b".." {
                    continue;
                }
                if let Ok(name_str) = core::str::from_utf8(entry.name) {
                    let mut owned = String::with_capacity_in(name_str.len(), alloc.clone());
                    owned.push_str(name_str);
                    entries.push(owned);
                }
            }
            if count == 0 {
                break;
            }
        }
    }
    #[cfg(target_os = "macos")]
    {
        use blaze_sys::posix::proc::{
            ATTR_CMN_NAME, ATTR_CMN_RETURNED_ATTRS, attrlist, getattrlistbulk,
        };
        let list = attrlist {
            bitmapcount: 5,
            reserved: 0,
            commonattr: ATTR_CMN_RETURNED_ATTRS | ATTR_CMN_NAME,
            volattr: 0,
            dirattr: 0,
            fileattr: 0,
            forkattr: 0,
        };
        let mut buf = [0u8; 8192];
        loop {
            let count = getattrlistbulk(dir_fd.fd(), &list, &mut buf, 0)?;
            if count == 0 {
                break;
            }
            let mut pos = 0usize;
            for _ in 0..count {
                let Some(len_bytes) =
                    buf.get(pos..pos.saturating_add(4)).and_then(|s| s.first_chunk::<4>())
                else {
                    break;
                };
                let rec_len = usize::try_from(u32::from_ne_bytes(*len_bytes)).unwrap_or(0);
                if rec_len < 32 {
                    break;
                }
                let ref_at = pos.saturating_add(24);
                if let (Some(off_b), Some(nlen_b)) = (
                    buf.get(ref_at..ref_at.saturating_add(4)).and_then(|s| s.first_chunk::<4>()),
                    buf.get(ref_at.saturating_add(4)..ref_at.saturating_add(8))
                        .and_then(|s| s.first_chunk::<4>()),
                ) {
                    let off = i32::from_ne_bytes(*off_b);
                    let nlen = usize::try_from(u32::from_ne_bytes(*nlen_b)).unwrap_or(0);
                    if let Ok(off_u) = usize::try_from(off) {
                        let start = ref_at.saturating_add(off_u);
                        let end = start.saturating_add(nlen);
                        if let Some(raw_name) = buf.get(start..end) {
                            let trimmed = match raw_name.iter().position(|&b| b == 0) {
                                Some(nul) => raw_name.get(..nul).unwrap_or(raw_name),
                                None => raw_name,
                            };
                            if trimmed != b"." && trimmed != b".."
                                && let Ok(name_str) = core::str::from_utf8(trimmed)
                            {
                                let mut owned =
                                    String::with_capacity_in(name_str.len(), alloc.clone());
                                owned.push_str(name_str);
                                entries.push(owned);
                            }
                        }
                    }
                }
                pos = pos.saturating_add(rec_len);
            }
        }
    }
    Ok(entries)
}

/// Removes a directory tree iteratively without recursion.
pub fn remove_tree<A: Allocator + Clone>(root: &str, alloc: A) {
    let mut stack = Vec::new_in(alloc.clone());
    let mut root_owned = String::with_capacity_in(root.len(), alloc.clone());
    root_owned.push_str(root);
    stack.push((root_owned, false));
    while let Some((path, visited)) = stack.pop() {
        if visited {
            let _ = remove_empty_dir(path.as_str());
            continue;
        }
        let Ok(st) = stat_path(path.as_str()) else {
            continue;
        };
        if !st.is_dir || st.is_symlink {
            let _ = unlink_path(path.as_str());
            continue;
        }
        let children = list_dir(path.as_str(), alloc.clone()).unwrap_or_else(|_| Vec::new_in(alloc.clone()));
        let mut parent = String::with_capacity_in(path.len(), alloc.clone());
        parent.push_str(path.as_str());
        stack.push((parent, true));
        for child in children {
            let full = join_path(path.as_str(), child.as_str(), alloc.clone());
            stack.push((full, false));
        }
    }
}

/// Attempts a non-blocking exclusive `flock` on `fd`.
/// Returns `Ok(true)` if acquired, `Ok(false)` if contended (`EAGAIN`).
pub(crate) fn try_flock_exclusive(fd: Fd) -> Result<bool, Errno> {
    #[cfg(target_os = "linux")]
    {
        match sys_flock(fd, LockOp::EXCLUSIVE.with(LockOp::NONBLOCK)) {
            Ok(()) => Ok(true),
            Err(Errno::Again) => Ok(false),
            Err(err) => Err(err),
        }
    }
    #[cfg(target_os = "macos")]
    {
        const LOCK_EX_NB: i32 = 2 | 4;
        // SAFETY: `flock` takes two integers and no memory.
        let rc = unsafe { blaze_sys::macos::cold::flock(fd.raw(), LOCK_EX_NB) };
        if rc == 0 {
            Ok(true)
        } else {
            let err = blaze_sys::posix::signal::errno();
            if err == Errno::Again { Ok(false) } else { Err(err) }
        }
    }
}

/// Releases an `flock` held on `fd`.
pub(crate) fn unlock_flock(fd: Fd) -> Result<(), Errno> {
    #[cfg(target_os = "linux")]
    {
        sys_flock(fd, LockOp::UNLOCK)
    }
    #[cfg(target_os = "macos")]
    {
        const LOCK_UN: i32 = 8;
        // SAFETY: `flock` takes two integers and no memory.
        let rc = unsafe { blaze_sys::macos::cold::flock(fd.raw(), LOCK_UN) };
        if rc == 0 { Ok(()) } else { Err(blaze_sys::posix::signal::errno()) }
    }
}

// ---------------------------------------------------------------------------------------------
// Unix domain sockets
// ---------------------------------------------------------------------------------------------

/// Builds a checked `(sockaddr_storage, socklen_t)` for a Unix socket path.
pub(crate) fn unix_sockaddr(path: &[u8]) -> Result<(sockaddr_storage, socklen_t), Errno> {
    // SAFETY: `sockaddr_un` is a plain C struct; all zeroes is valid.
    let mut addr: sockaddr_un = unsafe { core::mem::zeroed() };
    if path.is_empty() || path.contains(&0) || path.len() >= addr.sun_path.len() {
        return Err(Errno::Inval);
    }
    addr.sun_family = sa_family_t::try_from(AF_UNIX).map_err(|_| Errno::Inval)?;
    for (dst, &src) in addr.sun_path.iter_mut().zip(path.iter()) {
        *dst = c_char::from_ne_bytes([src]);
    }
    let length = core::mem::offset_of!(sockaddr_un, sun_path)
        .checked_add(path.len())
        .and_then(|n| n.checked_add(1))
        .ok_or(Errno::Inval)?;
    #[cfg(target_os = "macos")]
    {
        addr.sun_len = u8::try_from(length).map_err(|_| Errno::Inval)?;
    }
    let socklen = socklen_t::try_from(length).map_err(|_| Errno::Inval)?;
    // SAFETY: `sockaddr_storage` is larger and at least as aligned as `sockaddr_un`.
    let mut storage: sockaddr_storage = unsafe { core::mem::zeroed() };
    unsafe {
        core::ptr::from_mut(&mut storage).cast::<sockaddr_un>().write(addr);
    }
    Ok((storage, socklen))
}

/// Applies `SO_RCVTIMEO` or `SO_SNDTIMEO` to a socket descriptor.
fn set_socket_timeout(fd: Fd, optname: i32, timeout: Option<Duration>) -> Result<(), Errno> {
    let tv = match timeout {
        Some(d) => {
            let mut secs = i64::try_from(d.as_secs()).unwrap_or(i64::MAX);
            let mut usecs = i64::from(d.subsec_micros());
            if secs == 0 && usecs == 0 && !d.is_zero() {
                usecs = 1;
            }
            if secs == 0 && usecs == 0 {
                // Zero Duration is invalid as a timeout; clamp to 1 microsecond.
                secs = 0;
                usecs = 1;
            }
            #[cfg(target_os = "linux")]
            {
                Timeval { sec: secs, usec: usecs }
            }
            #[cfg(target_os = "macos")]
            {
                Timeval { sec: secs, usec: i32::try_from(usecs).unwrap_or(i32::MAX) }
            }
        }
        None => Timeval { sec: 0, usec: 0 },
    };
    let size = size_of::<Timeval>();
    // SAFETY: `tv` is a live `Timeval` with no uninitialized padding bytes.
    let raw = unsafe { core::slice::from_raw_parts(core::ptr::from_ref(&tv).cast::<u8>(), size) };
    setsockopt_bytes(fd, SOL_SOCKET, optname, raw)
}

/// Polls `fds` for up to `timeout` (retrying `EINTR` up to a bounded cap).
pub(crate) fn poll_fds(fds: &mut [PollFd], timeout: Option<Timespec>) -> Result<usize, Errno> {
    for _ in 0..MAX_INTR_RETRIES {
        match sys_poll(fds, timeout) {
            Ok(n) => return Ok(n),
            Err(Errno::Intr) => continue,
            Err(err) => return Err(err),
        }
    }
    Ok(0)
}

/// Owned Unix domain stream socket over `blaze-sys`.
#[derive(Debug)]
pub struct UnixStream {
    fd: OwnedFd,
}

impl UnixStream {
    /// Creates an unbound non-blocking `AF_UNIX` `SOCK_STREAM` socket with `FD_CLOEXEC`.
    pub(crate) fn unbound_nonblocking() -> Result<Self, Errno> {
        #[cfg(target_os = "linux")]
        let kind = SOCK_STREAM | SOCK_CLOEXEC | SOCK_NONBLOCK;
        #[cfg(target_os = "macos")]
        let kind = SOCK_STREAM;

        let raw = sys_socket(AF_UNIX, kind, 0)?;
        // SAFETY: `sys_socket` returned a fresh owned descriptor.
        let fd = unsafe { OwnedFd::from_raw(raw.raw()) };
        #[cfg(target_os = "macos")]
        {
            sys_set_close_on_exec(fd.fd(), true)?;
            sys_set_nonblocking(fd.fd(), true)?;
            let _ = setsockopt_int(fd.fd(), SOL_SOCKET, SO_NOSIGPIPE, 1);
        }
        Ok(Self { fd })
    }

    /// Blocking connect to a Unix socket path (for test clients).
    pub fn connect(path: &str) -> Result<Self, IoError> {
        let (storage, len) = unix_sockaddr(path.as_bytes()).map_err(IoError::from_errno)?;
        #[cfg(target_os = "linux")]
        let kind = SOCK_STREAM | SOCK_CLOEXEC;
        #[cfg(target_os = "macos")]
        let kind = SOCK_STREAM;

        let raw = sys_socket(AF_UNIX, kind, 0).map_err(IoError::from_errno)?;
        // SAFETY: `sys_socket` returned a fresh owned descriptor.
        let fd = unsafe { OwnedFd::from_raw(raw.raw()) };
        #[cfg(target_os = "macos")]
        {
            sys_set_close_on_exec(fd.fd(), true).map_err(IoError::from_errno)?;
            let _ = setsockopt_int(fd.fd(), SOL_SOCKET, SO_NOSIGPIPE, 1);
        }
        sys_connect(fd.fd(), &storage, len).map_err(IoError::from_errno)?;
        Ok(Self { fd })
    }

    /// Underlying descriptor.
    pub(crate) fn fd(&self) -> Fd {
        self.fd.fd()
    }

    /// Toggles `O_NONBLOCK` on this socket.
    pub fn set_nonblocking(&self, on: bool) -> Result<(), IoError> {
        sys_set_nonblocking(self.fd.fd(), on).map_err(IoError::from_errno)
    }

    /// Sets `SO_RCVTIMEO`.
    pub fn set_read_timeout(&self, timeout: Option<Duration>) -> Result<(), IoError> {
        set_socket_timeout(self.fd.fd(), SO_RCVTIMEO, timeout).map_err(IoError::from_errno)
    }

    /// Sets `SO_SNDTIMEO`.
    pub fn set_write_timeout(&self, timeout: Option<Duration>) -> Result<(), IoError> {
        set_socket_timeout(self.fd.fd(), SO_SNDTIMEO, timeout).map_err(IoError::from_errno)
    }

    /// Checks `SO_ERROR` on the socket.
    pub(crate) fn take_error(&self) -> Result<Option<Errno>, Errno> {
        pending_error(self.fd.fd())
    }

    /// Verifies the socket has an actually connected peer.
    pub(crate) fn verify_peer(&self) -> Result<(), Errno> {
        #[cfg(target_os = "linux")]
        {
            let cred = blaze_sys::posix::net::peer_cred(self.fd.fd())?;
            if cred.pid <= 0 {
                return Err(Errno::NotConn);
            }
            Ok(())
        }
        #[cfg(target_os = "macos")]
        {
            let _ = blaze_sys::posix::net::peer_ids(self.fd.fd())?;
            Ok(())
        }
    }

    /// Queries `F_GETFL` status flags.
    #[cfg(test)]
    pub(crate) fn status_flags(&self) -> Result<OpenFlags, Errno> {
        sys_status_flags(self.fd.fd())
    }

    /// Queries `FD_CLOEXEC`.
    #[cfg(test)]
    pub(crate) fn close_on_exec(&self) -> Result<bool, Errno> {
        sys_cloexec(self.fd.fd())
    }

    /// Initiates a nonblocking `connect` to `(storage, len)`.
    pub(crate) fn connect_raw(&self, storage: &sockaddr_storage, len: socklen_t) -> Result<(), Errno> {
        sys_connect(self.fd.fd(), storage, len)
    }

    /// Reads up to `buf.len()` bytes (retrying `EINTR`).
    pub fn read(&mut self, buf: &mut [u8]) -> Result<usize, IoError> {
        for _ in 0..MAX_INTR_RETRIES {
            match sys_recv(self.fd.fd(), buf, MsgFlags::NONE) {
                Ok(n) => return Ok(n),
                Err(Errno::Intr) => continue,
                Err(err) => return Err(IoError::from_errno(err)),
            }
        }
        Err(IoError::from_errno(Errno::Intr))
    }

    /// Reads exact `buf.len()` bytes or fails on EOF/error.
    pub fn read_exact(&mut self, mut buf: &mut [u8]) -> Result<(), IoError> {
        while !buf.is_empty() {
            let n = self.read(buf)?;
            if n == 0 {
                return Err(IoError::from_errno(Errno::ConnReset));
            }
            let Some((_, rest)) = buf.split_at_mut_checked(n) else {
                return Err(IoError::from_errno(Errno::Inval));
            };
            buf = rest;
        }
        Ok(())
    }

    /// Writes up to `bytes.len()` bytes suppressing `SIGPIPE`.
    pub fn write(&mut self, bytes: &[u8]) -> Result<usize, IoError> {
        for _ in 0..MAX_INTR_RETRIES {
            match sys_send(self.fd.fd(), bytes, MsgFlags::NO_SIGNAL) {
                Ok(n) => return Ok(n),
                Err(Errno::Intr) => continue,
                Err(err) => return Err(IoError::from_errno(err)),
            }
        }
        Err(IoError::from_errno(Errno::Intr))
    }

    /// Writes all of `bytes` suppressing `SIGPIPE`.
    pub fn write_all(&mut self, mut bytes: &[u8]) -> Result<(), IoError> {
        let mut iters = 0usize;
        while !bytes.is_empty() {
            if iters >= MAX_WRITE_ITERS {
                return Err(IoError::from_errno(Errno::Again));
            }
            iters = iters.saturating_add(1);
            let n = self.write(bytes)?;
            if n == 0 {
                return Err(IoError::from_errno(Errno::Pipe));
            }
            bytes = bytes.get(n..).ok_or(IoError::from_errno(Errno::Inval))?;
        }
        Ok(())
    }
}

/// Owned Unix domain listening socket over `blaze-sys`.
pub struct UnixListener {
    fd: OwnedFd,
}

impl UnixListener {
    /// Binds a fresh owner-only (`0o600`) Unix socket at `path` and listens.
    pub fn bind_owner_only(path: &str) -> Result<Self, IoError> {
        Self::bind_with_mode(path, Some(0o600))
    }

    /// Binds a fresh Unix socket at `path` with default permissions (for tests).
    #[cfg(test)]
    pub(crate) fn bind(path: &str) -> Result<Self, IoError> {
        Self::bind_with_mode(path, None)
    }

    fn bind_with_mode(path: &str, mode: Option<u32>) -> Result<Self, IoError> {
        let (storage, len) = unix_sockaddr(path.as_bytes()).map_err(IoError::from_errno)?;
        #[cfg(target_os = "linux")]
        let kind = SOCK_STREAM | SOCK_CLOEXEC;
        #[cfg(target_os = "macos")]
        let kind = SOCK_STREAM;

        let raw = sys_socket(AF_UNIX, kind, 0).map_err(IoError::from_errno)?;
        // SAFETY: `sys_socket` returned a fresh owned descriptor.
        let fd = unsafe { OwnedFd::from_raw(raw.raw()) };
        #[cfg(target_os = "macos")]
        {
            sys_set_close_on_exec(fd.fd(), true).map_err(IoError::from_errno)?;
        }
        #[cfg(target_os = "linux")]
        if let Some(m) = mode {
            // Setting mode on the unbound AF_UNIX socket applies atomically at bind time.
            let _ = linux_fchmod(fd.fd(), m);
        }
        sys_bind(fd.fd(), &storage, len).map_err(IoError::from_errno)?;
        #[cfg(target_os = "macos")]
        if let Some(m) = mode {
            let m16 = u16::try_from(m).map_err(|_| IoError::from_errno(Errno::Inval))?;
            with_cpath(path, |cpath| {
                // SAFETY: `cpath` is a live NUL-terminated path the kernel reads and does not keep.
                let rc = unsafe {
                    blaze_sys::posix::proc::fchmodat(CWD.raw(), cpath.as_ptr().cast(), m16, 0)
                };
                if rc < 0 { Err(blaze_sys::posix::signal::errno()) } else { Ok(()) }
            })
            .map_err(IoError::from_errno)?;
        }
        sys_listen(fd.fd(), LISTEN_BACKLOG).map_err(IoError::from_errno)?;
        Ok(Self { fd })
    }

    /// Toggles `O_NONBLOCK`.
    pub fn set_nonblocking(&self, on: bool) -> Result<(), IoError> {
        sys_set_nonblocking(self.fd.fd(), on).map_err(IoError::from_errno)
    }

    /// Adjusts the socket's listen backlog (used by backlog saturation tests).
    #[cfg(test)]
    pub(crate) fn set_backlog(&self, backlog: i32) -> Result<(), Errno> {
        sys_listen(self.fd.fd(), backlog)
    }

    /// Accepts one connection with `FD_CLOEXEC`.
    pub fn accept(&self) -> Result<UnixStream, IoError> {
        #[cfg(target_os = "linux")]
        {
            let fd = accept4(self.fd.fd(), SockFlags::CLOSE_ON_EXEC).map_err(IoError::from_errno)?;
            Ok(UnixStream { fd })
        }
        #[cfg(target_os = "macos")]
        {
            // SAFETY: null peer-address pointers tell `accept` to omit the peer address.
            let rc = unsafe {
                blaze_sys::macos::cold::accept(
                    self.fd.fd().raw(),
                    core::ptr::null_mut(),
                    core::ptr::null_mut(),
                )
            };
            if rc < 0 {
                return Err(IoError::from_errno(blaze_sys::posix::signal::errno()));
            }
            // SAFETY: `rc >= 0` is a fresh owned descriptor.
            let fd = unsafe { OwnedFd::from_raw(rc) };
            sys_set_close_on_exec(fd.fd(), true).map_err(IoError::from_errno)?;
            let _ = setsockopt_int(fd.fd(), SOL_SOCKET, SO_NOSIGPIPE, 1);
            Ok(UnixStream { fd })
        }
    }
}

// ---------------------------------------------------------------------------------------------
// TCP sockets (for `blobs.rs` and tests)
// ---------------------------------------------------------------------------------------------

/// Converts a `SocketAddr` into a POSIX `(sockaddr_storage, socklen_t)`.
fn socket_addr_to_storage(addr: SocketAddr) -> Result<(sockaddr_storage, socklen_t), Errno> {
    // SAFETY: `sockaddr_storage` is a plain C byte-buffer struct; all zeroes is valid.
    let mut storage: sockaddr_storage = unsafe { core::mem::zeroed() };
    match addr {
        SocketAddr::V4(v4) => {
            let sin = sockaddr_in {
                #[cfg(target_os = "macos")]
                sin_len: u8::try_from(size_of::<sockaddr_in>()).map_err(|_| Errno::Inval)?,
                sin_family: sa_family_t::try_from(AF_INET).map_err(|_| Errno::Inval)?,
                sin_port: v4.port().to_be(),
                sin_addr: in_addr { s_addr: u32::from_ne_bytes(v4.ip().octets()) },
                sin_zero: [0; 8],
            };
            let len = socklen_t::try_from(size_of::<sockaddr_in>()).map_err(|_| Errno::Inval)?;
            // SAFETY: `sockaddr_storage` is larger and at least as aligned as `sockaddr_in`.
            unsafe {
                core::ptr::from_mut(&mut storage).cast::<sockaddr_in>().write(sin);
            }
            Ok((storage, len))
        }
        SocketAddr::V6(v6) => {
            let sin6 = sockaddr_in6 {
                #[cfg(target_os = "macos")]
                sin6_len: u8::try_from(size_of::<sockaddr_in6>()).map_err(|_| Errno::Inval)?,
                sin6_family: sa_family_t::try_from(AF_INET6).map_err(|_| Errno::Inval)?,
                sin6_port: v6.port().to_be(),
                sin6_flowinfo: v6.flowinfo(),
                sin6_addr: in6_addr { s6_addr: v6.ip().octets() },
                sin6_scope_id: v6.scope_id(),
            };
            let len = socklen_t::try_from(size_of::<sockaddr_in6>()).map_err(|_| Errno::Inval)?;
            // SAFETY: `sockaddr_storage` is larger and at least as aligned as `sockaddr_in6`.
            unsafe {
                core::ptr::from_mut(&mut storage).cast::<sockaddr_in6>().write(sin6);
            }
            Ok((storage, len))
        }
    }
}

/// Converts a POSIX `(sockaddr_storage, socklen_t)` back into a `SocketAddr`.
fn storage_to_socket_addr(storage: &sockaddr_storage, len: socklen_t) -> Result<SocketAddr, Errno> {
    let len = usize::try_from(len).map_err(|_| Errno::Inval)?;
    let family = i32::from(storage.ss_family);
    if family == AF_INET {
        if len < size_of::<sockaddr_in>() {
            return Err(Errno::Inval);
        }
        // SAFETY: `len >= size_of::<sockaddr_in>()` and `sockaddr_storage` is properly aligned.
        let sin = unsafe { core::ptr::from_ref(storage).cast::<sockaddr_in>().read() };
        let ip = Ipv4Addr::from(sin.sin_addr.s_addr.to_ne_bytes());
        let port = u16::from_be(sin.sin_port);
        Ok(SocketAddr::V4(SocketAddrV4::new(ip, port)))
    } else if family == AF_INET6 {
        if len < size_of::<sockaddr_in6>() {
            return Err(Errno::Inval);
        }
        // SAFETY: `len >= size_of::<sockaddr_in6>()` and `sockaddr_storage` is properly aligned.
        let sin6 = unsafe { core::ptr::from_ref(storage).cast::<sockaddr_in6>().read() };
        let ip = Ipv6Addr::from(sin6.sin6_addr.s6_addr);
        let port = u16::from_be(sin6.sin6_port);
        Ok(SocketAddr::V6(SocketAddrV6::new(ip, port, sin6.sin6_flowinfo, sin6.sin6_scope_id)))
    } else {
        Err(Errno::AfNoSupport)
    }
}

/// Blocking TCP stream with bounded connect/read/write timeouts over `blaze-sys`.
pub struct TcpStream {
    fd: OwnedFd,
}

impl TcpStream {
    /// Connects to `addr` with a bounded timeout, then restores blocking mode.
    pub fn connect_timeout(addr: SocketAddr, timeout: Duration) -> Result<Self, IoError> {
        let domain = match addr {
            SocketAddr::V4(_) => AF_INET,
            SocketAddr::V6(_) => AF_INET6,
        };
        #[cfg(target_os = "linux")]
        let kind = SOCK_STREAM | SOCK_CLOEXEC | SOCK_NONBLOCK;
        #[cfg(target_os = "macos")]
        let kind = SOCK_STREAM;

        let raw = sys_socket(domain, kind, 0).map_err(IoError::from_errno)?;
        // SAFETY: `sys_socket` returned a fresh owned descriptor.
        let fd = unsafe { OwnedFd::from_raw(raw.raw()) };
        #[cfg(target_os = "macos")]
        {
            sys_set_close_on_exec(fd.fd(), true).map_err(IoError::from_errno)?;
            sys_set_nonblocking(fd.fd(), true).map_err(IoError::from_errno)?;
            let _ = setsockopt_int(fd.fd(), SOL_SOCKET, SO_NOSIGPIPE, 1);
        }
        let (storage, len) = socket_addr_to_storage(addr).map_err(IoError::from_errno)?;
        match sys_connect(fd.fd(), &storage, len) {
            Ok(()) => {}
            Err(Errno::InProgress | Errno::Already | Errno::Intr) => {
                let mut fds = [PollFd::new(fd.fd(), PollEvents::OUT)];
                let ready = poll_fds(&mut fds, Some(duration_to_timespec(timeout)))
                    .map_err(IoError::from_errno)?;
                if ready == 0 {
                    return Err(IoError::from_errno(Errno::TimedOut));
                }
                if let Some(err) = pending_error(fd.fd()).map_err(IoError::from_errno)? {
                    return Err(IoError::from_errno(err));
                }
            }
            Err(err) => return Err(IoError::from_errno(err)),
        }
        sys_set_nonblocking(fd.fd(), false).map_err(IoError::from_errno)?;
        Ok(Self { fd })
    }

    /// Sets `SO_RCVTIMEO`.
    pub fn set_read_timeout(&self, timeout: Option<Duration>) -> Result<(), IoError> {
        set_socket_timeout(self.fd.fd(), SO_RCVTIMEO, timeout).map_err(IoError::from_errno)
    }

    /// Sets `SO_SNDTIMEO`.
    pub fn set_write_timeout(&self, timeout: Option<Duration>) -> Result<(), IoError> {
        set_socket_timeout(self.fd.fd(), SO_SNDTIMEO, timeout).map_err(IoError::from_errno)
    }

    /// Sets `TCP_NODELAY`.
    pub fn set_nodelay(&self, on: bool) -> Result<(), IoError> {
        setsockopt_int(self.fd.fd(), IPPROTO_TCP, TCP_NODELAY, i32::from(on))
            .map_err(IoError::from_errno)
    }

    /// Reads bytes from the stream.
    pub fn read(&mut self, buf: &mut [u8]) -> Result<usize, IoError> {
        for _ in 0..MAX_INTR_RETRIES {
            match sys_recv(self.fd.fd(), buf, MsgFlags::NONE) {
                Ok(n) => return Ok(n),
                Err(Errno::Intr) => continue,
                Err(err) => return Err(IoError::from_errno(err)),
            }
        }
        Err(IoError::from_errno(Errno::Intr))
    }

    /// Writes all bytes to the stream suppressing `SIGPIPE`.
    pub fn write_all(&mut self, mut bytes: &[u8]) -> Result<(), IoError> {
        let mut iters = 0usize;
        while !bytes.is_empty() {
            if iters >= MAX_WRITE_ITERS {
                return Err(IoError::from_errno(Errno::Again));
            }
            iters = iters.saturating_add(1);
            match sys_send(self.fd.fd(), bytes, MsgFlags::NO_SIGNAL) {
                Ok(0) => return Err(IoError::from_errno(Errno::Pipe)),
                Ok(n) => {
                    bytes = bytes.get(n..).ok_or(IoError::from_errno(Errno::Inval))?;
                }
                Err(Errno::Intr) => continue,
                Err(err) => return Err(IoError::from_errno(err)),
            }
        }
        Ok(())
    }
}

/// Non-blocking TCP listener over `blaze-sys` (used by proxy tests).
pub struct TcpListener {
    fd: OwnedFd,
    local: SocketAddr,
}

impl TcpListener {
    /// Binds and listens on `addr`.
    pub fn bind(addr: SocketAddr) -> Result<Self, IoError> {
        let domain = match addr {
            SocketAddr::V4(_) => AF_INET,
            SocketAddr::V6(_) => AF_INET6,
        };
        #[cfg(target_os = "linux")]
        let kind = SOCK_STREAM | SOCK_CLOEXEC;
        #[cfg(target_os = "macos")]
        let kind = SOCK_STREAM;

        let raw = sys_socket(domain, kind, 0).map_err(IoError::from_errno)?;
        // SAFETY: `sys_socket` returned a fresh owned descriptor.
        let fd = unsafe { OwnedFd::from_raw(raw.raw()) };
        #[cfg(target_os = "macos")]
        {
            sys_set_close_on_exec(fd.fd(), true).map_err(IoError::from_errno)?;
        }
        setsockopt_int(fd.fd(), SOL_SOCKET, SO_REUSEADDR, 1).map_err(IoError::from_errno)?;
        let (storage, len) = socket_addr_to_storage(addr).map_err(IoError::from_errno)?;
        sys_bind(fd.fd(), &storage, len).map_err(IoError::from_errno)?;
        sys_listen(fd.fd(), LISTEN_BACKLOG).map_err(IoError::from_errno)?;
        let (bound_storage, bound_len) = sys_getsockname(fd.fd()).map_err(IoError::from_errno)?;
        let local = storage_to_socket_addr(&bound_storage, bound_len).map_err(IoError::from_errno)?;
        Ok(Self { fd, local })
    }

    /// Resolved bound address.
    pub const fn local_addr(&self) -> SocketAddr {
        self.local
    }

    /// Toggles `O_NONBLOCK`.
    pub fn set_nonblocking(&self, on: bool) -> Result<(), IoError> {
        sys_set_nonblocking(self.fd.fd(), on).map_err(IoError::from_errno)
    }

    /// Accepts one connection with `FD_CLOEXEC`.
    pub fn accept(&self) -> Result<TcpStream, IoError> {
        #[cfg(target_os = "linux")]
        {
            let fd = accept4(self.fd.fd(), SockFlags::CLOSE_ON_EXEC).map_err(IoError::from_errno)?;
            Ok(TcpStream { fd })
        }
        #[cfg(target_os = "macos")]
        {
            // SAFETY: null peer-address pointers tell `accept` to omit the peer address.
            let rc = unsafe {
                blaze_sys::macos::cold::accept(
                    self.fd.fd().raw(),
                    core::ptr::null_mut(),
                    core::ptr::null_mut(),
                )
            };
            if rc < 0 {
                return Err(IoError::from_errno(blaze_sys::posix::signal::errno()));
            }
            // SAFETY: `rc >= 0` is a fresh owned descriptor.
            let fd = unsafe { OwnedFd::from_raw(rc) };
            sys_set_close_on_exec(fd.fd(), true).map_err(IoError::from_errno)?;
            let _ = setsockopt_int(fd.fd(), SOL_SOCKET, SO_NOSIGPIPE, 1);
            Ok(TcpStream { fd })
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Futex-backed Mutex, Channel, and pthread Threads / Scoped Threads
// ---------------------------------------------------------------------------------------------

/// Futex-backed mutual exclusion lock (zero `std::sync::Mutex`, no poisoning).
pub struct Mutex<T> {
    /// `0` = unlocked, `1` = locked uncontended, `2` = locked contended.
    state: AtomicU32,
    value: UnsafeCell<T>,
}

// SAFETY: `Mutex<T>` serializes all access to `value`, so `T: Send` suffices for `Send`/`Sync`.
unsafe impl<T: Send> Send for Mutex<T> {}
// SAFETY: `Mutex<T>` serializes all access to `value`, so `T: Send` suffices for `Send`/`Sync`.
unsafe impl<T: Send> Sync for Mutex<T> {}

impl<T> Mutex<T> {
    /// Creates an unlocked mutex owning `value`.
    pub const fn new(value: T) -> Self {
        Self { state: AtomicU32::new(0), value: UnsafeCell::new(value) }
    }

    /// Acquires the lock and returns an RAII guard.
    pub fn lock(&self) -> MutexGuard<'_, T> {
        if self.state.compare_exchange(0, 1, Ordering::Acquire, Ordering::Relaxed).is_ok() {
            return MutexGuard { mutex: self };
        }
        let mut spins = 0u32;
        loop {
            if self.state.compare_exchange(0, 2, Ordering::Acquire, Ordering::Relaxed).is_ok() {
                return MutexGuard { mutex: self };
            }
            if spins < 32 {
                spins = spins.saturating_add(1);
                core::hint::spin_loop();
                continue;
            }
            let _ = self.state.compare_exchange(1, 2, Ordering::Relaxed, Ordering::Relaxed);
            #[cfg(target_os = "linux")]
            {
                let _ = futex_wait_bitset(&self.state, 2, None, u32::MAX);
            }
            #[cfg(target_os = "macos")]
            {
                let _ = nanosleep(Timespec { sec: 0, nsec: 100_000 });
            }
        }
    }

    fn unlock(&self) {
        if self.state.swap(0, Ordering::Release) == 2 {
            #[cfg(target_os = "linux")]
            {
                let _ = futex_wake(&self.state, 1);
            }
        }
    }
}

/// RAII guard for [`Mutex`].
pub struct MutexGuard<'a, T> {
    mutex: &'a Mutex<T>,
}

impl<T> Deref for MutexGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &Self::Target {
        // SAFETY: the guard holds the mutex lock exclusively.
        unsafe { &*self.mutex.value.get() }
    }
}

impl<T> DerefMut for MutexGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        // SAFETY: the guard holds the mutex lock exclusively.
        unsafe { &mut *self.mutex.value.get() }
    }
}

impl<T: fmt::Debug> fmt::Debug for MutexGuard<'_, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&**self, f)
    }
}

impl<T> Drop for MutexGuard<'_, T> {
    fn drop(&mut self) {
        self.mutex.unlock();
    }
}

/// Error returned when a channel receive times out or disconnects.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecvTimeoutError {
    /// No message arrived before the timeout elapsed.
    Timeout,
    /// All senders were dropped and no messages remain.
    Disconnected,
}

/// Error returned when a blocking channel receive finds all senders dropped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RecvError;

/// Error returned when sending on a channel whose receiver was dropped.
#[derive(Debug)]
pub struct SendError<T>(pub T);

struct ChannelState<T> {
    queue: VecDeque<T, WireAlloc>,
    senders: usize,
    receiver_alive: bool,
}

struct ChannelInner<T> {
    state: Mutex<ChannelState<T>>,
    /// Monotonic notification counter incremented on every push or disconnect.
    signal: AtomicU32,
}

impl<T> ChannelInner<T> {
    fn notify(&self) {
        let _ = self.signal.fetch_add(1, Ordering::Release);
        #[cfg(target_os = "linux")]
        {
            let _ = futex_wake(&self.signal, u32::MAX);
        }
    }
}

/// Sending half of a `#![no_std]` MPSC channel.
pub struct Sender<T> {
    inner: Arc<ChannelInner<T>, WireAlloc>,
}

/// Receiving half of a `#![no_std]` MPSC channel.
pub struct Receiver<T> {
    inner: Arc<ChannelInner<T>, WireAlloc>,
}

/// Creates a `#![no_std]` MPSC channel using `alloc`.
pub fn channel<T>(alloc: WireAlloc) -> (Sender<T>, Receiver<T>) {
    let inner = Arc::new_in(
        ChannelInner {
            state: Mutex::new(ChannelState {
                queue: VecDeque::new_in(alloc),
                senders: 1,
                receiver_alive: true,
            }),
            signal: AtomicU32::new(0),
        },
        alloc,
    );
    (Sender { inner: inner.clone() }, Receiver { inner })
}

impl<T> Clone for Sender<T> {
    fn clone(&self) -> Self {
        let mut guard = self.inner.state.lock();
        guard.senders = guard.senders.saturating_add(1);
        drop(guard);
        Self { inner: self.inner.clone() }
    }
}

impl<T> Drop for Sender<T> {
    fn drop(&mut self) {
        let mut guard = self.inner.state.lock();
        guard.senders = guard.senders.saturating_sub(1);
        let notify = guard.senders == 0;
        drop(guard);
        if notify {
            self.inner.notify();
        }
    }
}

impl<T> Drop for Receiver<T> {
    fn drop(&mut self) {
        let mut guard = self.inner.state.lock();
        guard.receiver_alive = false;
        drop(guard);
        self.inner.notify();
    }
}

impl<T> Sender<T> {
    /// Sends `value` to the receiver, or returns `Err(SendError(value))` if disconnected.
    pub fn send(&self, value: T) -> Result<(), SendError<T>> {
        let mut guard = self.inner.state.lock();
        if !guard.receiver_alive {
            return Err(SendError(value));
        }
        guard.queue.push_back(value);
        drop(guard);
        self.inner.notify();
        Ok(())
    }
}

impl<T> Receiver<T> {
    /// Waits for the next item until one arrives or all senders disconnect.
    pub fn recv(&self) -> Result<T, RecvError> {
        loop {
            let seq = self.inner.signal.load(Ordering::Acquire);
            {
                let mut guard = self.inner.state.lock();
                if let Some(item) = guard.queue.pop_front() {
                    return Ok(item);
                }
                if guard.senders == 0 {
                    return Err(RecvError);
                }
            }
            #[cfg(target_os = "linux")]
            {
                let _ = futex_wait_bitset(&self.inner.signal, seq, None, u32::MAX);
            }
            #[cfg(target_os = "macos")]
            {
                let _ = seq;
                let _ = nanosleep(Timespec { sec: 0, nsec: 200_000 });
            }
        }
    }

    /// Waits for the next item up to `timeout`.
    pub fn recv_timeout(&self, timeout: Duration) -> Result<T, RecvTimeoutError> {
        let clock = SystemClock;
        let start = clock.monotonic_nanos().unwrap_or(0);
        let limit = duration_to_nanos(timeout);
        loop {
            let seq = self.inner.signal.load(Ordering::Acquire);
            {
                let mut guard = self.inner.state.lock();
                if let Some(item) = guard.queue.pop_front() {
                    return Ok(item);
                }
                if guard.senders == 0 {
                    return Err(RecvTimeoutError::Disconnected);
                }
            }
            let now = clock.monotonic_nanos().unwrap_or(start);
            let elapsed = now.saturating_sub(start);
            if elapsed >= limit {
                return Err(RecvTimeoutError::Timeout);
            }
            let remaining = limit.saturating_sub(elapsed);
            #[cfg(target_os = "linux")]
            {
                let wake_at = now.saturating_add(remaining.min(50_000_000));
                let ts = Timespec {
                    sec: i64::try_from(wake_at / 1_000_000_000).unwrap_or(i64::MAX),
                    nsec: i64::try_from(wake_at % 1_000_000_000).unwrap_or(0),
                };
                let _ = futex_wait_bitset(&self.inner.signal, seq, Some(ts), u32::MAX);
            }
            #[cfg(target_os = "macos")]
            {
                let _ = seq;
                let wait_ns = i64::try_from(remaining.min(1_000_000)).unwrap_or(100_000);
                let _ = nanosleep(Timespec { sec: 0, nsec: wait_ns });
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Pthread threads & scoped threads
// ---------------------------------------------------------------------------------------------

#[cfg(target_os = "linux")]
#[allow(non_camel_case_types)]
type RawThread = usize;

#[cfg(target_os = "linux")]
unsafe extern "C" {
    fn pthread_create(
        thread: *mut RawThread,
        attr: *const c_void,
        start_routine: extern "C" fn(*mut c_void) -> *mut c_void,
        arg: *mut c_void,
    ) -> i32;
    fn pthread_join(thread: RawThread, retval: *mut *mut c_void) -> i32;
    fn pthread_self() -> RawThread;
}

/// Returns an opaque identifier for the calling OS thread.
pub fn current_thread_id() -> u64 {
    #[cfg(target_os = "linux")]
    {
        // SAFETY: `pthread_self` takes no pointers and always succeeds.
        let id = unsafe { pthread_self() };
        id.widen_to::<u64>()
    }
    #[cfg(target_os = "macos")]
    {
        // SAFETY: `pthread_self` takes no pointers and always succeeds.
        let id = unsafe { blaze_sys::posix::thread::pthread_self() };
        id.addr().widen_to::<u64>()
    }
}

/// Error returned when joining a thread whose closure exited abnormally (without storing a result).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ThreadFailed;

/// Join handle for a spawned OS thread (`'static` or scoped).
pub struct JoinHandle<'scope, T> {
    #[cfg(target_os = "linux")]
    handle: RawThread,
    #[cfg(target_os = "macos")]
    handle: blaze_sys::posix::thread::pthread_t,
    joined: AtomicBool,
    result_slot: *mut Option<T>,
    alloc: WireAlloc,
    _scope: PhantomData<&'scope ()>,
}

// SAFETY: `JoinHandle` owns the unique `result_slot` written once before join.
unsafe impl<T: Send> Send for JoinHandle<'_, T> {}
// SAFETY: `&JoinHandle` has no interior-mutable access to `result_slot` (only `AtomicBool`).
unsafe impl<T: Send> Sync for JoinHandle<'_, T> {}

struct Trampoline<F, T> {
    func: F,
    result_slot: *mut Option<T>,
    alloc: WireAlloc,
}

fn spawn_raw<'scope, F, T>(func: F, alloc: WireAlloc) -> Result<JoinHandle<'scope, T>, IoError>
where
    F: FnOnce() -> T + Send + 'scope,
    T: Send + 'scope,
{
    let slot_box = Box::new_in(None::<T>, alloc);
    let (slot_ptr, _) = Box::into_raw_with_allocator(slot_box);
    let payload = Box::new_in(Trampoline { func, result_slot: slot_ptr, alloc }, alloc);
    let (payload_ptr, _) = Box::into_raw_with_allocator(payload);

    extern "C" fn entry<F: FnOnce() -> T + Send, T: Send>(raw: *mut c_void) -> *mut c_void {
        // SAFETY: `raw` was produced by `Box::into_raw_with_allocator` with `WireAlloc`.
        let boxed: Box<Trampoline<F, T>, WireAlloc> = unsafe {
            let ptr = raw.cast::<Trampoline<F, T>>();
            let alloc = (*ptr).alloc;
            Box::from_raw_in(ptr, alloc)
        };
        let Trampoline { func, result_slot, .. } = Box::into_inner(boxed);
        let value = func();
        // SAFETY: `result_slot` remains uniquely owned until `JoinHandle::join`.
        unsafe {
            *result_slot = Some(value);
        }
        core::ptr::null_mut()
    }

    #[cfg(target_os = "linux")]
    {
        let mut handle: RawThread = 0;
        // SAFETY: valid thread handle pointer, null default attrs, valid entry + payload.
        let rc = unsafe {
            pthread_create(&raw mut handle, core::ptr::null(), entry::<F, T>, payload_ptr.cast())
        };
        if rc != 0 {
            // Reclaim payload and slot on spawn failure.
            // SAFETY: thread never started, so both boxes are uniquely ours.
            unsafe {
                drop(Box::from_raw_in(payload_ptr, alloc));
                drop(Box::from_raw_in(slot_ptr, alloc));
            }
            return Err(IoError::from_errno(Errno::Again));
        }
        Ok(JoinHandle {
            handle,
            joined: AtomicBool::new(false),
            result_slot: slot_ptr,
            alloc,
            _scope: PhantomData,
        })
    }
    #[cfg(target_os = "macos")]
    {
        let mut handle = core::ptr::null_mut();
        // SAFETY: valid thread handle pointer, null default attrs, valid entry + payload.
        let rc = unsafe {
            blaze_sys::posix::thread::pthread_create(
                &raw mut handle,
                core::ptr::null(),
                entry::<F, T>,
                payload_ptr.cast(),
            )
        };
        if rc != 0 {
            // SAFETY: thread never started, so both boxes are uniquely ours.
            unsafe {
                drop(Box::from_raw_in(payload_ptr, alloc));
                drop(Box::from_raw_in(slot_ptr, alloc));
            }
            return Err(IoError::from_errno(Errno::Again));
        }
        Ok(JoinHandle {
            handle,
            joined: AtomicBool::new(false),
            result_slot: slot_ptr,
            alloc,
            _scope: PhantomData,
        })
    }
}

/// Spawns a `'static` OS thread using `alloc`.
pub fn spawn<F, T>(func: F, alloc: WireAlloc) -> Result<JoinHandle<'static, T>, IoError>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    spawn_raw(func, alloc)
}

impl<T> JoinHandle<'_, T> {
    fn join_raw(&self) {
        if self.joined.swap(true, Ordering::AcqRel) {
            return;
        }
        #[cfg(target_os = "linux")]
        {
            // SAFETY: `self.handle` is a live joinable pthread joined at most once.
            let _ = unsafe { pthread_join(self.handle, core::ptr::null_mut()) };
        }
        #[cfg(target_os = "macos")]
        {
            // SAFETY: `self.handle` is a live joinable pthread joined at most once.
            let _ = unsafe {
                blaze_sys::posix::thread::pthread_join(self.handle, core::ptr::null_mut())
            };
        }
    }

    /// Joins the thread and returns its output.
    pub fn join(mut self) -> Result<T, ThreadFailed> {
        self.join_raw();
        let slot_ptr = core::mem::replace(&mut self.result_slot, core::ptr::null_mut());
        if slot_ptr.is_null() {
            return Err(ThreadFailed);
        }
        // SAFETY: the thread has joined, so `slot_ptr` is uniquely owned and can be reclaimed.
        let slot_box = unsafe { Box::from_raw_in(slot_ptr, self.alloc) };
        Box::into_inner(slot_box).ok_or(ThreadFailed)
    }
}

impl<T> Drop for JoinHandle<'_, T> {
    fn drop(&mut self) {
        self.join_raw();
        let slot_ptr = core::mem::replace(&mut self.result_slot, core::ptr::null_mut());
        if !slot_ptr.is_null() {
            // SAFETY: the thread has joined, so `slot_ptr` is uniquely owned and can be reclaimed.
            unsafe {
                drop(Box::from_raw_in(slot_ptr, self.alloc));
            }
        }
    }
}

/// Scope for spawning borrowed threads that are guaranteed to join before [`scope`] returns.
pub struct Scope<'scope, 'env: 'scope> {
    alloc: WireAlloc,
    _scope: PhantomData<&'scope mut &'scope ()>,
    _env: PhantomData<&'env mut &'env ()>,
}

impl<'scope, 'env: 'scope> Scope<'scope, 'env> {
    /// Spawns a thread borrowing data alive for `'scope`.
    ///
    /// Note: every returned [`JoinHandle`] automatically joins on `Drop` if not joined explicitly,
    /// ensuring no spawned thread outlives `'scope`.
    pub fn spawn<F, T>(&'scope self, func: F) -> Result<JoinHandle<'scope, T>, IoError>
    where
        F: FnOnce() -> T + Send + 'scope,
        T: Send + 'scope,
    {
        spawn_raw(func, self.alloc)
    }
}

/// Runs `f` with a [`Scope`] whose spawned [`JoinHandle`]s all join before `scope` returns.
pub fn scope<'env, F, T>(alloc: WireAlloc, f: F) -> T
where
    F: for<'scope> FnOnce(&'scope Scope<'scope, 'env>) -> T,
{
    let scope = Scope { alloc, _scope: PhantomData, _env: PhantomData };
    f(&scope)
}

// ---------------------------------------------------------------------------------------------
// Temporary directory helper (respecting TMPDIR, falling back to /tmp)
// ---------------------------------------------------------------------------------------------

/// Owned temporary directory respecting `TMPDIR`, cleaned up on `Drop`.
pub struct TempDir {
    path: WireString,
    alloc: WireAlloc,
}

impl TempDir {
    /// Creates a unique temporary directory under `$TMPDIR` (or `/tmp`).
    pub fn new(prefix: &str, alloc: WireAlloc) -> Result<Self, IoError> {
        let base = env_var("TMPDIR", alloc)
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| WireString::from_str_in("/tmp", alloc));
        let tag = random_id()?;
        let pid = current_pid();
        let mut hex = String::with_capacity_in(16, alloc);
        for &b in tag.get(..8).unwrap_or(&[]) {
            let _ = core::fmt::write(&mut hex, format_args!("{b:02x}"));
        }
        let name = format_in!(alloc, "{prefix}-{pid}-{hex}");
        let path = join_path(base.as_str(), name.as_str(), alloc);
        create_dir_all(path.as_str()).map_err(IoError::from_errno)?;
        Ok(Self { path, alloc })
    }

    /// Path of the temporary directory.
    pub fn path(&self) -> &str {
        self.path.as_str()
    }

    /// Joins `child` under this temporary directory.
    pub fn join(&self, child: &str) -> WireString {
        join_path(self.path.as_str(), child, self.alloc)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        remove_tree(self.path.as_str(), self.alloc);
    }
}

// ---------------------------------------------------------------------------------------------
// Child process helper over `blaze-sys` (`fork` + `execv` + `pipe` + `wait_pid` + `kill`)
// ---------------------------------------------------------------------------------------------

/// Child process spawned via POSIX `fork` + `execv`, with optional piped stdin/stdout.
pub struct Child {
    pid: blaze_sys::posix::types::pid_t,
    stdin: Option<OwnedFd>,
    stdout: Option<OwnedFd>,
}

/// Exit status of a child process.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ExitStatus {
    exited_zero: bool,
}

impl ExitStatus {
    /// Whether the child exited normally with code `0`.
    pub const fn success(self) -> bool {
        self.exited_zero
    }
}

impl Child {
    /// Spawns `program` with `args` (excluding `argv[0]`) and optional piped stdin/stdout.
    pub fn spawn(
        program: &str,
        args: &[&str],
        pipe_stdin: bool,
        pipe_stdout: bool,
        alloc: WireAlloc,
    ) -> Result<Self, IoError> {
        let stdin_pipe = if pipe_stdin {
            Some(blaze_sys::posix::io::pipe().map_err(IoError::from_errno)?)
        } else {
            None
        };
        let stdout_pipe = if pipe_stdout {
            Some(blaze_sys::posix::io::pipe().map_err(IoError::from_errno)?)
        } else {
            None
        };
        let mut c_strings: Vec<Vec<u8, WireAlloc>, WireAlloc> =
            Vec::with_capacity_in(args.len().saturating_add(1), alloc);
        let mut prog_bytes = Vec::with_capacity_in(program.len().saturating_add(1), alloc);
        prog_bytes.extend_from_slice(program.as_bytes());
        prog_bytes.push(0);
        c_strings.push(prog_bytes);
        for arg in args {
            let mut bytes = Vec::with_capacity_in(arg.len().saturating_add(1), alloc);
            bytes.extend_from_slice(arg.as_bytes());
            bytes.push(0);
            c_strings.push(bytes);
        }

        #[cfg(target_os = "linux")]
        let pid = {
            let mut linux_args: Vec<blaze_sys::linux::Arg<'_>, WireAlloc> =
                Vec::with_capacity_in(c_strings.len().saturating_add(1), alloc);
            for item in &c_strings {
                let cstr = core::ffi::CStr::from_bytes_with_nul(item)
                    .map_err(|_| IoError::from_errno(Errno::Inval))?;
                linux_args.push(blaze_sys::linux::Arg::new(cstr));
            }
            linux_args.push(blaze_sys::linux::Arg::END);
            let argv = blaze_sys::linux::Args::new(&linux_args).map_err(IoError::from_errno)?;
            let empty_env = [blaze_sys::linux::Arg::END];
            let envp = blaze_sys::linux::Args::new(&empty_env).map_err(IoError::from_errno)?;
            let prog_cstr = core::ffi::CStr::from_bytes_with_nul(&c_strings[0])
                .map_err(|_| IoError::from_errno(Errno::Inval))?;
            let prog_path = blaze_sys::Path::new(prog_cstr);

            // SAFETY: `argv`, `envp`, and `prog_path` are prepared before `fork()`; the child only
            // invokes async-signal-safe `dup2`, `close`, `execve`, and `exit_group`.
            match unsafe { blaze_sys::linux::task::fork() }.map_err(IoError::from_errno)? {
                blaze_sys::linux::Cloned::Child => {
                    if let Some((read_end, write_end)) = stdin_pipe {
                        let _ = blaze_sys::posix::io::dup2(read_end.fd(), Fd::new(0));
                        drop(read_end);
                        drop(write_end);
                    }
                    if let Some((read_end, write_end)) = stdout_pipe {
                        let _ = blaze_sys::posix::io::dup2(write_end.fd(), Fd::new(1));
                        drop(read_end);
                        drop(write_end);
                    }
                    let _ = blaze_sys::linux::task::execve(prog_path, argv, envp);
                    blaze_sys::linux::exit_group(127);
                }
                blaze_sys::linux::Cloned::Parent(pid) => pid,
            }
        };

        #[cfg(target_os = "macos")]
        let pid = {
            let mut argv: Vec<*const c_char, WireAlloc> =
                Vec::with_capacity_in(c_strings.len().saturating_add(1), alloc);
            for item in &c_strings {
                argv.push(item.as_ptr().cast::<c_char>());
            }
            argv.push(core::ptr::null());
            let prog_ptr = argv.first().copied().unwrap_or(core::ptr::null());
            // SAFETY: `argv` and `c_strings` are prepared before `fork()`; the child only invokes
            // async-signal-safe `dup2`, `close`, `execv`, and `_exit`.
            let pid = unsafe { blaze_sys::posix::proc::fork() };
            if pid < 0 {
                return Err(IoError::from_errno(Errno::Again));
            }
            if pid == 0 {
                if let Some((read_end, write_end)) = stdin_pipe {
                    let _ = blaze_sys::posix::io::dup2(read_end.fd(), Fd::new(0));
                    drop(read_end);
                    drop(write_end);
                }
                if let Some((read_end, write_end)) = stdout_pipe {
                    let _ = blaze_sys::posix::io::dup2(write_end.fd(), Fd::new(1));
                    drop(read_end);
                    drop(write_end);
                }
                // SAFETY: `prog_ptr` and `argv` are null-terminated C strings.
                unsafe {
                    let _ = blaze_sys::posix::proc::execv(prog_ptr, argv.as_ptr());
                    blaze_sys::posix::proc::_exit(127);
                }
            }
            pid
        };

        let stdin = stdin_pipe.map(|(read_end, write_end)| {
            drop(read_end);
            write_end
        });
        let stdout = stdout_pipe.map(|(read_end, write_end)| {
            drop(write_end);
            read_end
        });
        Ok(Self { pid, stdin, stdout })
    }

    /// Writes `bytes` to the child's piped stdin and closes it so EOF is observed.
    pub fn write_stdin_and_close(&mut self, bytes: &[u8]) -> Result<(), IoError> {
        let Some(fd) = self.stdin.take() else {
            return Err(IoError::from_errno(Errno::Pipe));
        };
        write_all_fd(fd.fd(), bytes)?;
        drop(fd);
        Ok(())
    }

    /// Reads one `\n`-terminated line from the child's piped stdout.
    pub fn read_stdout_line<A: Allocator>(&mut self, alloc: A) -> Result<Option<String<A>>, IoError> {
        let Some(fd) = &self.stdout else {
            return Err(IoError::from_errno(Errno::Pipe));
        };
        read_line_fd(fd.fd(), alloc)
    }

    /// Sends `SIGKILL` to the child process.
    pub fn kill(&mut self) -> Result<(), IoError> {
        blaze_sys::posix::signal::kill(self.pid, blaze_sys::posix::signal::SIGKILL)
            .map_err(IoError::from_errno)
    }

    /// Waits for the child process to exit and returns its [`ExitStatus`].
    pub fn wait(&mut self) -> Result<ExitStatus, IoError> {
        for _ in 0..10_000 {
            match blaze_sys::posix::proc::wait_pid(
                self.pid,
                blaze_sys::posix::proc::WaitFlags::from_bits(0),
            ) {
                Ok(Some((_, status))) => {
                    return Ok(ExitStatus { exited_zero: status.exit_code() == Some(0) });
                }
                Ok(None) => sleep_ms(1),
                Err(Errno::Intr) => continue,
                Err(errno) => return Err(IoError::from_errno(errno)),
            }
        }
        Err(IoError::from_errno(Errno::TimedOut))
    }
}
