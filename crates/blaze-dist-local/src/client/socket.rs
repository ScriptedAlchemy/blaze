//! Owned nonblocking Unix connects. A stalled listener cannot strand agent cleanup.

use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::net::UnixStream;
use std::path::Path;

use blaze_core::wait::{self, Clock, Deadline, Waited};

fn timed_out() -> io::Error {
    io::Error::new(io::ErrorKind::TimedOut, "the connection deadline passed")
}

fn address(path: &Path) -> io::Result<(libc::sockaddr_un, libc::socklen_t)> {
    // SAFETY: every all-zero field is a valid sockaddr_un value; the active fields
    // and path length are set below before the kernel reads this address.
    let mut address: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    let bytes = path.as_os_str().as_bytes();
    if bytes.is_empty() || bytes.contains(&0) || bytes.len() >= address.sun_path.len() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "a Unix socket path must be nonempty, contain no NUL, and fit sun_path with its terminator",
        ));
    }
    address.sun_family = libc::AF_UNIX as libc::sa_family_t;
    for (to, from) in address.sun_path.iter_mut().zip(bytes) {
        *to = *from as libc::c_char;
    }
    let length = std::mem::offset_of!(libc::sockaddr_un, sun_path) + bytes.len() + 1;
    #[cfg(target_os = "macos")]
    {
        address.sun_len = u8::try_from(length)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "Unix address is too long"))?;
    }
    let length = libc::socklen_t::try_from(length)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "Unix address is too long"))?;
    Ok((address, length))
}

fn socket() -> io::Result<UnixStream> {
    #[cfg(target_os = "linux")]
    let kind = libc::SOCK_STREAM | libc::SOCK_CLOEXEC;
    #[cfg(not(target_os = "linux"))]
    let kind = libc::SOCK_STREAM;
    // SAFETY: no pointers; success returns a new owned descriptor.
    let raw = unsafe { libc::socket(libc::AF_UNIX, kind, 0) };
    if raw < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: socket returned this descriptor, and ownership is transferred exactly once.
    let stream = UnixStream::from(unsafe { OwnedFd::from_raw_fd(raw) });
    #[cfg(not(target_os = "linux"))]
    {
        // SAFETY: this stream still owns the open descriptor. On platforms without
        // SOCK_CLOEXEC this is the same close-on-exec setup as blaze-core's sockets.
        if unsafe { libc::fcntl(stream.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) } < 0 {
            return Err(io::Error::last_os_error());
        }
    }
    stream.set_nonblocking(true)?;
    Ok(stream)
}

/// Connect by a finite absolute deadline; every unsuccessful path drops the owned FD.
pub(super) fn connect_until(
    path: &Path,
    deadline: Deadline,
    clock: &impl Clock,
) -> io::Result<UnixStream> {
    let result = (|| {
        if deadline == Deadline::NEVER {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "connect needs a finite deadline"));
        }
        let (address, length) = address(path)?;
        if deadline.passed(clock.now()) {
            return Err(timed_out());
        }
        let stream = socket()?;
        // SAFETY: the stream owns an open nonblocking Unix socket; address is a
        // fully initialized sockaddr_un and length includes only its checked path.
        let connected = unsafe {
            libc::connect(
                stream.as_raw_fd(),
                (&raw const address).cast::<libc::sockaddr>(),
                length,
            )
        };
        if connected < 0 {
            let error = io::Error::last_os_error();
            match error.raw_os_error() {
                Some(libc::EINPROGRESS | libc::EALREADY | libc::EINTR) => {
                    let mut descriptors =
                        [libc::pollfd { fd: stream.as_raw_fd(), events: libc::POLLOUT, revents: 0 }];
                    match wait::readable(
                        "dist: connecting to the coordinator",
                        &mut descriptors,
                        Some(deadline),
                        clock,
                    )
                    .map_err(io::Error::from)?
                    {
                        Waited::Ready(_) => {}
                        Waited::Deadline => return Err(timed_out()),
                    }
                }
                Some(libc::EISCONN) => {}
                // Linux AF_UNIX reports a full accept backlog as EAGAIN without
                // starting a connection. Its unconnected socket can look writable:
                // refuse explicitly rather than mistake that for a completed connect.
                _ => return Err(error),
            }
        }
        if deadline.passed(clock.now()) {
            return Err(timed_out());
        }
        if let Some(error) = stream.take_error()? {
            return Err(error);
        }
        // Readiness alone is insufficient: require an actual connected peer.
        stream.peer_addr()?;
        stream.set_nonblocking(false)?;
        Ok(stream)
    })();
    result.map_err(|error: io::Error| {
        io::Error::new(error.kind(), format!("connecting to coordinator {}: {error}", path.display()))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::ffi::OsStringExt;
    use std::os::unix::net::UnixListener;
    use std::time::Duration;

    use blaze_core::wait::Monotonic;

    fn connect(path: &Path) -> io::Result<UnixStream> {
        let clock = Monotonic::new();
        connect_until(path, Deadline::after(clock.now(), Duration::from_secs(1)), &clock)
    }

    #[test]
    fn successful_connect_has_a_peer_and_restores_blocking_io() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("ready.sock");
        let listener = UnixListener::bind(&path).unwrap();
        listener.set_nonblocking(true).unwrap();
        let stream = connect(&path).unwrap();
        assert!(stream.peer_addr().is_ok());
        let _accepted = listener.accept().unwrap();
        // SAFETY: these read-only descriptor queries use the still-owned socket.
        let flags = unsafe { libc::fcntl(stream.as_raw_fd(), libc::F_GETFL) };
        assert!(flags >= 0);
        assert_eq!(flags & libc::O_NONBLOCK, 0);
        // SAFETY: as above.
        let descriptor = unsafe { libc::fcntl(stream.as_raw_fd(), libc::F_GETFD) };
        assert!(descriptor >= 0);
        assert_ne!(descriptor & libc::FD_CLOEXEC, 0);
    }

    #[test]
    fn absent_invalid_and_expired_connections_are_named_refusals() {
        let directory = tempfile::tempdir().unwrap();
        let absent = directory.path().join("absent.sock");
        let error = connect(&absent).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
        assert!(error.to_string().contains("absent.sock"));
        for path in [
            std::path::PathBuf::new(),
            std::path::PathBuf::from(std::ffi::OsString::from_vec(b"wrong\0socket".to_vec())),
            std::path::PathBuf::from("x".repeat(256)),
        ] {
            assert_eq!(connect(&path).unwrap_err().kind(), io::ErrorKind::InvalidInput);
        }
        let clock = Monotonic::new();
        assert_eq!(
            connect_until(&absent, Deadline::PASSED, &clock).unwrap_err().kind(),
            io::ErrorKind::TimedOut
        );
        assert_eq!(
            connect_until(&absent, Deadline::NEVER, &clock).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_saturated_unix_backlog_refuses_without_a_blocking_connect() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("full.sock");
        let listener = UnixListener::bind(&path).unwrap();
        // SAFETY: the test owns this listening socket; no thread accepts from it.
        assert_eq!(unsafe { libc::listen(listener.as_raw_fd(), 0) }, 0);
        let _queued = connect(&path).unwrap();
        let error = connect(&path).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::WouldBlock, "{error}");
        assert!(error.to_string().contains("full.sock"));
    }
}
