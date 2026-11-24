//! Owned nonblocking Unix connects. A stalled listener cannot strand agent cleanup.

use core::time::Duration;

use blaze_own::format_in;
use blaze_wire::{WireAlloc, WireString};

use crate::sys::{
    Errno, IoError, PollEvents, PollFd, SystemClock, Timespec, UnixStream, duration_to_nanos,
    poll_fds, unix_sockaddr,
};

/// Finite or sentinel deadline for [`connect_until`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum ConnectDeadline {
    /// Absolute monotonic nanosecond deadline.
    At(u64),
    /// Already expired.
    #[cfg(test)]
    Passed,
    /// Infinite deadline (refused explicitly: connects must be bounded).
    Never,
}

impl ConnectDeadline {
    pub(super) fn after(now_ns: u64, duration: Duration) -> Self {
        match now_ns.checked_add(duration_to_nanos(duration)) {
            Some(at) => Self::At(at),
            None => Self::Never,
        }
    }

    fn remaining_ns(self, now_ns: u64) -> Option<u64> {
        match self {
            Self::At(at) => (now_ns < at).then_some(at.saturating_sub(now_ns)),
            #[cfg(test)]
            Self::Passed => None,
            Self::Never => None,
        }
    }
}

/// Clock source for [`connect_until`].
pub(super) trait ConnectClock {
    fn now_ns(&self) -> Result<u64, Errno>;
}

impl ConnectClock for SystemClock {
    fn now_ns(&self) -> Result<u64, Errno> {
        self.monotonic_nanos()
    }
}

fn timed_out(alloc: WireAlloc) -> IoError {
    IoError::with_message(
        Errno::TimedOut,
        WireString::from_str_in("the connection deadline passed", alloc),
    )
}

/// Connect by a finite absolute deadline; every unsuccessful path drops the owned FD.
pub(super) fn connect_until(
    path: &str,
    deadline: ConnectDeadline,
    clock: &impl ConnectClock,
    alloc: WireAlloc,
) -> Result<UnixStream, IoError> {
    let result = (|| -> Result<UnixStream, IoError> {
        if deadline == ConnectDeadline::Never {
            return Err(IoError::with_message(
                Errno::Inval,
                WireString::from_str_in("connect needs a finite deadline", alloc),
            ));
        }
        let (storage, length) = unix_sockaddr(path.as_bytes()).map_err(|_| {
            IoError::with_message(
                Errno::Inval,
                WireString::from_str_in(
                    "a Unix socket path must be nonempty, contain no NUL, and fit sun_path with its terminator",
                    alloc,
                ),
            )
        })?;
        let now = clock.now_ns().map_err(IoError::from_errno)?;
        let Some(remaining) = deadline.remaining_ns(now) else {
            return Err(timed_out(alloc));
        };
        let stream = UnixStream::unbound_nonblocking().map_err(IoError::from_errno)?;
        match stream.connect_raw(&storage, length) {
            Ok(()) | Err(Errno::IsConn) => {}
            Err(Errno::InProgress | Errno::Already | Errno::Intr) => {
                let ts = Timespec {
                    sec: i64::try_from(remaining / 1_000_000_000).unwrap_or(i64::MAX),
                    nsec: i64::try_from(remaining % 1_000_000_000).unwrap_or(0),
                };
                let mut descriptors = [PollFd::new(stream.fd(), PollEvents::OUT)];
                let ready = poll_fds(&mut descriptors, Some(ts)).map_err(IoError::from_errno)?;
                if ready == 0 {
                    return Err(timed_out(alloc));
                }
            }
            // Linux AF_UNIX reports a full accept backlog as EAGAIN without
            // starting a connection. Its unconnected socket can look writable:
            // refuse explicitly rather than mistake that for a completed connect.
            Err(err) => return Err(IoError::from_errno(err)),
        }
        let now = clock.now_ns().map_err(IoError::from_errno)?;
        if deadline.remaining_ns(now).is_none() {
            return Err(timed_out(alloc));
        }
        if let Some(err) = stream.take_error().map_err(IoError::from_errno)? {
            return Err(IoError::from_errno(err));
        }
        // Readiness alone is insufficient: require an actual connected peer.
        stream.verify_peer().map_err(IoError::from_errno)?;
        stream.set_nonblocking(false)?;
        Ok(stream)
    })();
    result.map_err(|error| {
        IoError::with_message(
            error.errno(),
            format_in!(alloc, "connecting to coordinator {path}: {error}"),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sys::{TempDir, UnixListener};
    use blaze_own::String;
    use blaze_sys::posix::io::OpenFlags;

    fn test_alloc() -> WireAlloc {
        blaze_own::testing::heap().shared()
    }

    fn connect(path: &str) -> Result<UnixStream, IoError> {
        let alloc = test_alloc();
        let clock = SystemClock;
        let now = clock.now_ns().unwrap();
        connect_until(path, ConnectDeadline::after(now, Duration::from_secs(1)), &clock, alloc)
    }

    #[test]
    fn successful_connect_has_a_peer_and_restores_blocking_io() {
        let alloc = test_alloc();
        let directory = TempDir::new("blaze-sock", alloc).unwrap();
        let path = directory.join("ready.sock");
        let listener = UnixListener::bind(path.as_str()).unwrap();
        listener.set_nonblocking(true).unwrap();
        let stream = connect(path.as_str()).unwrap();
        assert!(stream.verify_peer().is_ok());
        let _accepted = listener.accept().unwrap();
        let flags = stream.status_flags().unwrap();
        assert!(!flags.contains(OpenFlags::NONBLOCK));
        assert!(stream.close_on_exec().unwrap());
    }

    #[test]
    fn absent_invalid_and_expired_connections_are_named_refusals() {
        let alloc = test_alloc();
        let directory = TempDir::new("blaze-sock", alloc).unwrap();
        let absent = directory.join("absent.sock");
        let error = connect(absent.as_str()).unwrap_err();
        assert!(error.is_not_found());
        assert!(format_in!(alloc, "{error}").contains("absent.sock"));
        let mut long = String::with_capacity_in(256, alloc);
        for _ in 0..256 {
            long.push('x');
        }
        for path in ["", "wrong\0socket", long.as_str()] {
            assert!(connect(path).unwrap_err().is_invalid_input());
        }
        let clock = SystemClock;
        assert!(
            connect_until(absent.as_str(), ConnectDeadline::Passed, &clock, alloc)
                .unwrap_err()
                .is_timed_out()
        );
        assert!(
            connect_until(absent.as_str(), ConnectDeadline::Never, &clock, alloc)
                .unwrap_err()
                .is_invalid_input()
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_saturated_unix_backlog_refuses_without_a_blocking_connect() {
        let alloc = test_alloc();
        let directory = TempDir::new("blaze-sock", alloc).unwrap();
        let path = directory.join("full.sock");
        let listener = UnixListener::bind(path.as_str()).unwrap();
        listener.set_backlog(0).unwrap();
        let _queued = connect(path.as_str()).unwrap();
        let error = connect(path.as_str()).unwrap_err();
        assert!(error.is_would_block(), "{error}");
        assert!(format_in!(alloc, "{error}").contains("full.sock"));
    }
}
