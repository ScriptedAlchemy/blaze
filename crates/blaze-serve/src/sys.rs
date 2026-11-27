//! Thin OS bindings over `blaze-sys` for files, directories, locks, sockets, polling and clocks.

mod directory;
pub(crate) use directory::{list_dir, open_dir};

use core::alloc::Allocator;
use core::fmt;
use core::net::{Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4, SocketAddrV6};
use core::sync::atomic::{AtomicU32, Ordering};
use core::time::Duration;

use blaze_own::{String, Vec};
pub(crate) use blaze_sys::posix::io::{OwnedFd, PollEvents, PollFd};
use blaze_sys::posix::io::{
    CWD, ClockId, OpenFlags, clock_gettime as sys_read_clock, dup_at_least, fsync,
    openat, poll as sys_poll, pread, read as sys_read, write as sys_write,
};
#[cfg(target_os = "linux")]
use blaze_sys::posix::io::{PipeFlags, pipe2};
#[cfg(target_os = "macos")]
use blaze_sys::posix::io::{nanosleep, set_close_on_exec, set_nonblocking};
use blaze_sys::posix::net::{
    AF_INET, AF_INET6, IPPROTO_TCP, MsgFlags, SOCK_STREAM, SOL_SOCKET, SO_KEEPALIVE, SO_REUSEADDR,
    TCP_KEEPINTVL, TCP_NODELAY, bind as sys_bind, getsockname as sys_getsockname, in_addr,
    in6_addr, listen as sys_listen, recv as sys_recv, send as sys_send, setsockopt_int,
    sockaddr_in, sockaddr_in6, sockaddr_storage, socket as sys_socket,
};
use blaze_sys::posix::proc::{
    AT_SYMLINK_NOFOLLOW, S_IFDIR, S_IFLNK, S_IFMT, S_IFREG, Unlink, realpath_into, rename_at,
    stat_at, stat_fd, unlink_at,
};
use blaze_sys::posix::types::socklen_t;
pub(crate) use blaze_sys::{Errno, Fd, Timespec};

#[cfg(target_os = "linux")]
use blaze_sys::linux::{
    LockOp, Shutdown, SockFlags, accept4, flock as sys_flock, futex_wait_bitset, futex_wake,
    mkdirat, shutdown as linux_shutdown,
};
#[cfg(target_os = "linux")]
use blaze_sys::posix::net::{SOCK_CLOEXEC, SOCK_NONBLOCK, TCP_KEEPIDLE};

#[cfg(target_os = "macos")]
use blaze_sys::posix::net::{SO_NOSIGPIPE, TCP_KEEPALIVE};

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

/// Listen backlog for TCP listeners.
const LISTEN_BACKLOG: i32 = 1024;

/// Typed OS error carrying the kernel `Errno`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct SysError(pub Errno);

impl SysError {
    /// Underlying kernel error code.
    pub const fn errno(self) -> Errno {
        self.0
    }
}

impl From<Errno> for SysError {
    fn from(value: Errno) -> Self {
        Self(value)
    }
}

impl fmt::Display for SysError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} ({})", self.0.what(), self.0.name())
    }
}

impl core::error::Error for SysError {}

/// Error returned by [`read_exact_fd`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum ReadExactError {
    /// Fewer bytes remained than requested.
    UnexpectedEof,
    /// Underlying kernel error.
    Os(Errno),
}

/// File metadata extracted from `lstat` / `stat_at`.
#[derive(Clone, Copy, Debug)]
pub(crate) struct FileMeta {
    pub(crate) is_file: bool,
    pub(crate) is_dir: bool,
    pub(crate) is_symlink: bool,
    pub(crate) mtime_sec: i64,
    pub(crate) mtime_nsec: i64,
}

/// Monotonic + wall clock capability injected into `Store` and `Server`.
#[derive(Clone, Copy, Default, Debug)]
pub(crate) struct SystemClock;

impl SystemClock {
    /// Current monotonic instant as nanoseconds since an arbitrary boot epoch.
    pub(crate) fn monotonic_nanos(self) -> Result<u64, Errno> {
        let ts = sys_read_clock(ClockId::Monotonic)?;
        timespec_to_nanos(ts)
    }

    /// Current wall-clock time as `(seconds, nanoseconds)` since the Unix epoch.
    pub(crate) fn realtime(self) -> Result<Timespec, Errno> {
        sys_read_clock(ClockId::Realtime)
    }
}

/// Converts a non-negative [`Timespec`] to nanoseconds, saturating at `u64::MAX`.
pub(crate) fn timespec_to_nanos(ts: Timespec) -> Result<u64, Errno> {
    let secs = u64::try_from(ts.sec).map_err(|_| Errno::Inval)?;
    let nsecs = u64::try_from(ts.nsec).map_err(|_| Errno::Inval)?;
    Ok(secs.saturating_mul(1_000_000_000).saturating_add(nsecs))
}

/// Converts a [`Duration`] into a [`Timespec`], clamping to `i64::MAX` seconds.
pub(crate) fn duration_to_timespec(duration: Duration) -> Timespec {
    let sec = i64::try_from(duration.as_secs()).unwrap_or(i64::MAX);
    let nsec = i64::from(duration.subsec_nanos());
    Timespec { sec, nsec }
}

/// Elapsed nanoseconds between two monotonic timestamps (saturating at 0 if non-monotonic).
pub(crate) const fn elapsed_nanos(earlier: u64, later: u64) -> u64 {
    later.saturating_sub(earlier)
}

/// Duration in nanoseconds, clamped to `u64::MAX`.
pub(crate) fn duration_to_nanos(duration: Duration) -> u64 {
    u64::try_from(duration.as_nanos()).unwrap_or(u64::MAX)
}

/// Current process identifier from the kernel.
pub(crate) fn current_pid() -> i32 {
    blaze_sys::posix::thread::getpid()
}

/// Writes `bytes` to `fd` (used for stdout/stderr reporting in the binary).
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
pub(crate) fn join_path<A: Allocator>(base: &str, child: &str, alloc: A) -> String<A> {
    let mut out = String::with_capacity_in(base.len().saturating_add(child.len()).saturating_add(1), alloc);
    out.push_str(base);
    if !base.ends_with('/') && !child.starts_with('/') {
        out.push('/');
    }
    out.push_str(child);
    out
}

/// Creates a single directory at `path` with `mode`.
pub(crate) fn create_dir(path: &str, mode: u32) -> Result<(), Errno> {
    with_cpath(path, |cpath| {
        #[cfg(target_os = "linux")]
        {
            mkdirat(CWD, cpath, mode)
        }
        #[cfg(target_os = "macos")]
        {
            let m = u16::try_from(mode).map_err(|_| Errno::Inval)?;
            // SAFETY: `Path` keeps its NUL immediately after `bytes()`, in the same live
            // backing buffer. `mkdir` reads that C string and keeps no pointer.
            let rc = unsafe { blaze_sys::macos::cold::mkdir(cpath.bytes().as_ptr().cast(), m) };
            if rc < 0 {
                Err(blaze_sys::posix::signal::errno())
            } else {
                Ok(())
            }
        }
    })
}

/// Creates `path` and any missing parent directories iteratively (no recursion).
pub(crate) fn create_dir_all(path: &str) -> Result<(), Errno> {
    if path.is_empty() {
        return Err(Errno::Inval);
    }
    let bytes = path.as_bytes();
    let mut pos = 0usize;
    while pos < bytes.len() {
        // Skip consecutive slashes.
        while bytes.get(pos) == Some(&b'/') {
            pos = pos.saturating_add(1);
        }
        if pos >= bytes.len() {
            break;
        }
        // Advance to next slash or end.
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

/// `lstat` (does not follow the final symlink) on `path`.
pub(crate) fn lstat_info(path: &str) -> Result<FileMeta, Errno> {
    let st = with_cpath(path, |cpath| stat_at(CWD, cpath, AT_SYMLINK_NOFOLLOW))?;
    let kind = st.st_mode & S_IFMT;
    Ok(FileMeta {
        is_file: kind == S_IFREG,
        is_dir: kind == S_IFDIR,
        is_symlink: kind == S_IFLNK,
        mtime_sec: st.st_mtime,
        mtime_nsec: st.st_mtime_nsec,
    })
}

/// File length in bytes from an open file descriptor.
pub(crate) fn fstat_len(fd: Fd) -> Result<u64, Errno> {
    let st = stat_fd(fd)?;
    u64::try_from(st.st_size).map_err(|_| Errno::Inval)
}

/// Canonicalizes `path` into a `String<A>`.
pub(crate) fn canonical_path<A: Allocator>(path: &str, alloc: A) -> Result<String<A>, Errno> {
    let mut out = [0u8; PATH_BUF_CAP];
    let resolved = with_cpath(path, |cpath| {
        let slice = realpath_into(cpath, &mut out)?;
        Ok(slice.len())
    })?;
    let slice = out.get(..resolved).ok_or(Errno::NameTooLong)?;
    let text = core::str::from_utf8(slice).map_err(|_| Errno::Inval)?;
    let mut s = String::with_capacity_in(text.len(), alloc);
    s.push_str(text);
    Ok(s)
}

/// Opens an existing file read-only (`O_CLOEXEC`).
pub(crate) fn open_read(path: &str) -> Result<OwnedFd, Errno> {
    with_cpath(path, |cpath| {
        openat(CWD, cpath, OpenFlags::READ.with(OpenFlags::CLOSE_ON_EXEC), 0)
    })
}

/// Creates a brand-new file (`O_CREAT | O_EXCL | O_CLOEXEC`).
pub(crate) fn open_create_new(path: &str, read_write: bool) -> Result<OwnedFd, Errno> {
    let mode_flag = if read_write { OpenFlags::READ_WRITE } else { OpenFlags::WRITE };
    let flags = mode_flag
        .with(OpenFlags::CREATE)
        .with(OpenFlags::EXCLUSIVE)
        .with(OpenFlags::CLOSE_ON_EXEC);
    with_cpath(path, |cpath| openat(CWD, cpath, flags, FILE_MODE))
}

/// Duplicates `fd` with `FD_CLOEXEC`.
pub(crate) fn dup_cloexec(fd: Fd) -> Result<OwnedFd, Errno> {
    dup_at_least(fd, 0, true)
}

/// Reads from `fd` at `offset` (`pread`), retrying `EINTR`.
pub(crate) fn pread_some(fd: Fd, buf: &mut [u8], offset: u64) -> Result<usize, Errno> {
    for _ in 0..MAX_INTR_RETRIES {
        match pread(fd, buf, offset) {
            Ok(n) => return Ok(n),
            Err(Errno::Intr) => continue,
            Err(err) => return Err(err),
        }
    }
    Err(Errno::Intr)
}

/// Reads sequentially from `fd` (`read`), retrying `EINTR`.
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

/// Reads exactly `buf.len()` bytes from `fd` starting at `offset`.
pub(crate) fn pread_exact(fd: Fd, mut buf: &mut [u8], mut offset: u64) -> Result<(), ReadExactError> {
    while !buf.is_empty() {
        let got = pread_some(fd, buf, offset).map_err(ReadExactError::Os)?;
        if got == 0 {
            return Err(ReadExactError::UnexpectedEof);
        }
        offset = offset.saturating_add(u64::try_from(got).map_err(|_| ReadExactError::Os(Errno::Inval))?);
        let Some((_, rest)) = buf.split_at_mut_checked(got) else {
            return Err(ReadExactError::Os(Errno::Inval));
        };
        buf = rest;
    }
    Ok(())
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

/// Syncs file data and metadata to durable storage (`fsync`).
pub(crate) fn fsync_fd(fd: Fd) -> Result<(), Errno> {
    for _ in 0..MAX_INTR_RETRIES {
        match fsync(fd) {
            Ok(()) => return Ok(()),
            Err(Errno::Intr) => continue,
            Err(err) => return Err(err),
        }
    }
    Err(Errno::Intr)
}

/// Attempts a non-blocking exclusive `flock` on `fd`.
///
/// Returns `Ok(true)` if acquired, `Ok(false)` if another holder owns it (`EAGAIN`/`EWOULDBLOCK`).
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
        // LOCK_EX (2) | LOCK_NB (4)
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

/// Atomically renames `from` to `to` within the same filesystem.
pub(crate) fn rename_path(from: &str, to: &str) -> Result<(), Errno> {
    let mut from_buf = [0u8; PATH_BUF_CAP];
    let mut to_buf = [0u8; PATH_BUF_CAP];
    let from_c = blaze_sys::Path::in_buffer(from.as_bytes(), &mut from_buf)?;
    let to_c = blaze_sys::Path::in_buffer(to.as_bytes(), &mut to_buf)?;
    rename_at(CWD, from_c, CWD, to_c)
}

/// Unlinks a file or symlink at `path`.
pub(crate) fn unlink_path(path: &str) -> Result<(), Errno> {
    with_cpath(path, |cpath| unlink_at(CWD, cpath, Unlink::File))
}

/// Removes an empty directory at `path`.
#[cfg(test)]
pub(crate) fn remove_empty_dir(path: &str) -> Result<(), Errno> {
    with_cpath(path, |cpath| unlink_at(CWD, cpath, Unlink::Dir))
}

/// Converts a `SocketAddr` into a POSIX `(sockaddr_storage, socklen_t)`.
pub(crate) fn socket_addr_to_storage(addr: SocketAddr) -> Result<(sockaddr_storage, socklen_t), Errno> {
    // SAFETY: `sockaddr_storage` is a plain C byte-buffer struct; all zeroes is valid.
    let mut storage: sockaddr_storage = unsafe { core::mem::zeroed() };
    match addr {
        SocketAddr::V4(v4) => {
            let sin = sockaddr_in {
                #[cfg(target_os = "macos")]
                sin_len: u8::try_from(size_of::<sockaddr_in>()).map_err(|_| Errno::Inval)?,
                sin_family: blaze_sys::posix::types::sa_family_t::try_from(AF_INET)
                    .map_err(|_| Errno::Inval)?,
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
                sin6_family: blaze_sys::posix::types::sa_family_t::try_from(AF_INET6)
                    .map_err(|_| Errno::Inval)?,
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

/// Non-blocking TCP listener over `blaze-sys`.
pub(crate) struct TcpServer {
    fd: OwnedFd,
    local: SocketAddr,
}

impl TcpServer {
    /// Binds and listens on `addr` (`SO_REUSEADDR`, non-blocking, `FD_CLOEXEC`).
    pub(crate) fn bind_addr(addr: SocketAddr) -> Result<Self, Errno> {
        let domain = match addr {
            SocketAddr::V4(_) => AF_INET,
            SocketAddr::V6(_) => AF_INET6,
        };
        #[cfg(target_os = "linux")]
        let kind = SOCK_STREAM | SOCK_NONBLOCK | SOCK_CLOEXEC;
        #[cfg(target_os = "macos")]
        let kind = SOCK_STREAM;

        let raw_fd = sys_socket(domain, kind, 0)?;
        // SAFETY: `sys_socket` just created a fresh owned descriptor.
        let fd = unsafe { OwnedFd::from_raw(raw_fd.raw()) };

        #[cfg(target_os = "macos")]
        {
            set_nonblocking(fd.fd(), true)?;
            set_close_on_exec(fd.fd(), true)?;
        }

        setsockopt_int(fd.fd(), SOL_SOCKET, SO_REUSEADDR, 1)?;
        let (storage, len) = socket_addr_to_storage(addr)?;
        sys_bind(fd.fd(), &storage, len)?;
        sys_listen(fd.fd(), LISTEN_BACKLOG)?;

        let (bound_storage, bound_len) = sys_getsockname(fd.fd())?;
        let local = storage_to_socket_addr(&bound_storage, bound_len)?;
        Ok(Self { fd, local })
    }

    /// Resolved bound address (with concrete port if `addr` used port 0).
    pub(crate) const fn local_addr(&self) -> SocketAddr {
        self.local
    }

    /// Underlying socket descriptor.
    pub(crate) fn fd(&self) -> Fd {
        self.fd.fd()
    }

    /// Accepts one pending connection in non-blocking mode with `FD_CLOEXEC`.
    pub(crate) fn accept_one(&self) -> Result<OwnedFd, Errno> {
        #[cfg(target_os = "linux")]
        {
            accept4(self.fd.fd(), SockFlags::NONBLOCK.with(SockFlags::CLOSE_ON_EXEC))
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
                return Err(blaze_sys::posix::signal::errno());
            }
            // SAFETY: `rc >= 0` is a fresh descriptor owned by this process.
            let fd = unsafe { OwnedFd::from_raw(rc) };
            set_nonblocking(fd.fd(), true)?;
            set_close_on_exec(fd.fd(), true)?;
            Ok(fd)
        }
    }
}

/// Configures a newly accepted TCP connection (`TCP_NODELAY`, `SO_KEEPALIVE`, no `SIGPIPE`).
pub(crate) fn configure_peer_socket(fd: Fd) {
    let _ = setsockopt_int(fd, IPPROTO_TCP, TCP_NODELAY, 1);
    let _ = setsockopt_int(fd, SOL_SOCKET, SO_KEEPALIVE, 1);
    #[cfg(target_os = "linux")]
    let _ = setsockopt_int(fd, IPPROTO_TCP, TCP_KEEPIDLE, 30);
    #[cfg(target_os = "macos")]
    {
        let _ = setsockopt_int(fd, IPPROTO_TCP, TCP_KEEPALIVE, 30);
        let _ = setsockopt_int(fd, SOL_SOCKET, SO_NOSIGPIPE, 1);
    }
    let _ = setsockopt_int(fd, IPPROTO_TCP, TCP_KEEPINTVL, 10);
}

/// Non-blocking TCP send suppressing `SIGPIPE`.
pub(crate) fn send_nonblocking(fd: Fd, bytes: &[u8]) -> Result<usize, Errno> {
    sys_send(fd, bytes, MsgFlags::NO_SIGNAL)
}

/// Non-blocking TCP receive.
pub(crate) fn recv_nonblocking(fd: Fd, buf: &mut [u8]) -> Result<usize, Errno> {
    sys_recv(fd, buf, MsgFlags::NONE)
}

/// Half-closes the write side of a socket.
pub(crate) fn shutdown_write(fd: Fd) -> Result<(), Errno> {
    #[cfg(target_os = "linux")]
    {
        linux_shutdown(fd, Shutdown::Write)
    }
    #[cfg(target_os = "macos")]
    {
        // SAFETY: `shutdown` takes two integers and no memory (`SHUT_WR == 1`).
        let rc = unsafe { blaze_sys::posix::net::c::shutdown(fd.raw(), 1) };
        if rc < 0 { Err(blaze_sys::posix::signal::errno()) } else { Ok(()) }
    }
}

/// Creates a non-blocking, close-on-exec wakeup pipe `(read_end, write_end)`.
pub(crate) fn wakeup_pipe() -> Result<(OwnedFd, OwnedFd), Errno> {
    #[cfg(target_os = "linux")]
    {
        pipe2(PipeFlags::NONBLOCK.with(PipeFlags::CLOSE_ON_EXEC))
    }
    #[cfg(target_os = "macos")]
    {
        let (read, write) = blaze_sys::posix::io::pipe()?;
        for fd in [read.fd(), write.fd()] {
            set_close_on_exec(fd, true)?;
            set_nonblocking(fd, true)?;
        }
        Ok((read, write))
    }
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

/// Bounded mutual-exclusion gate for serializing manifest publication in `Store`.
///
/// Uses Linux futex wait/wake with a hard deadline (Rule 18) and never constructs a
/// `std::sync::Mutex` or ambient lock.
pub(crate) struct PublicationGate {
    /// `0` = unlocked, `1` = locked uncontended, `2` = locked contended.
    state: AtomicU32,
}

impl PublicationGate {
    /// Creates an unlocked gate.
    pub(crate) const fn new() -> Self {
        Self { state: AtomicU32::new(0) }
    }

    /// Acquires the gate within `deadline_ms`, returning `Err(Errno::TimedOut)` if exceeded.
    pub(crate) fn acquire(&self, clock: SystemClock, deadline_ms: u64) -> Result<GateGuard<'_>, Errno> {
        if self.state.compare_exchange(0, 1, Ordering::Acquire, Ordering::Relaxed).is_ok() {
            return Ok(GateGuard { gate: self });
        }
        let start = clock.monotonic_nanos()?;
        let limit_ns = deadline_ms.saturating_mul(1_000_000);
        let mut spins = 0u32;
        loop {
            if self.state.compare_exchange(0, 2, Ordering::Acquire, Ordering::Relaxed).is_ok() {
                return Ok(GateGuard { gate: self });
            }
            let now = clock.monotonic_nanos()?;
            let elapsed = elapsed_nanos(start, now);
            if elapsed >= limit_ns {
                return Err(Errno::TimedOut);
            }
            if spins < 32 {
                spins = spins.saturating_add(1);
                core::hint::spin_loop();
                continue;
            }
            let _ = self.state.compare_exchange(1, 2, Ordering::Relaxed, Ordering::Relaxed);
            let remaining_ns = limit_ns.saturating_sub(elapsed);
            let wait_ns = remaining_ns.min(10_000_000);
            #[cfg(target_os = "linux")]
            {
                let wake_at_ns = now.saturating_add(wait_ns);
                let ts = Timespec {
                    sec: i64::try_from(wake_at_ns / 1_000_000_000).unwrap_or(i64::MAX),
                    nsec: i64::try_from(wake_at_ns % 1_000_000_000).unwrap_or(0),
                };
                let _ = futex_wait_bitset(&self.state, 2, Some(ts), u32::MAX);
            }
            #[cfg(target_os = "macos")]
            {
                let _ = nanosleep(Timespec { sec: 0, nsec: i64::try_from(wait_ns.min(1_000_000)).unwrap_or(100_000) });
            }
        }
    }

    fn release(&self) {
        if self.state.swap(0, Ordering::Release) == 2 {
            #[cfg(target_os = "linux")]
            {
                let _ = futex_wake(&self.state, 1);
            }
        }
    }
}

/// RAII guard for [`PublicationGate`].
pub(crate) struct GateGuard<'a> {
    gate: &'a PublicationGate,
}

impl Drop for GateGuard<'_> {
    fn drop(&mut self) {
        self.gate.release();
    }
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
        // SAFETY: `_NSGetArgc` and `_NSGetArgv` return valid pointers to the process's
        // `argc`/`argv` storage initialized by dyld before `main`.
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
