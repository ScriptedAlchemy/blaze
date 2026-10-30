//! Input for a child process on its standard input. A command that prints for a reader
//! takes `SIGPIPE`'s default action (`end_with_reader`), so a plain write to a child that
//! stopped reading would end this process with the signal, before it could say what the
//! child said. Writes here fail with `BrokenPipe` instead, and the child's own exit and
//! standard error say why it stopped.

use std::{
    io::{self, Write as _},
    process::{Child, ChildStdin, Output},
};

/// Writes `input` to `child`'s standard input, closes it and waits for the child, as
/// `wait_with_output` does. A child that stops reading early is reported by its exit
/// status and standard error in the output; the failed write is the error only when the
/// child succeeded without reading what it was sent.
pub(crate) fn feed(mut child: Child, input: &[u8]) -> io::Result<Output> {
    let sent = match child.stdin.take() {
        Some(mut stdin) => send(&mut stdin, input),
        None => Err(io::Error::other("the child's standard input is not a pipe")),
    };
    let output = child.wait_with_output()?;
    match sent {
        Err(error) if output.status.success() => Err(error),
        _ => Ok(output),
    }
}

/// `write_all` to a pipe whose reader may be gone, without `SIGPIPE` for this process.
fn send(stdin: &mut ChildStdin, input: &[u8]) -> io::Result<()> {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        // A write's `SIGPIPE` goes to the writing thread: blocked here, it stays pending
        // and is taken back before the mask is restored, unless it was pending already.
        // SAFETY: plain signal-mask calls on initialized sets, for this thread only.
        unsafe {
            let mut pipe: libc::sigset_t = std::mem::zeroed();
            libc::sigemptyset(&mut pipe);
            libc::sigaddset(&mut pipe, libc::SIGPIPE);
            let mut before: libc::sigset_t = std::mem::zeroed();
            libc::pthread_sigmask(libc::SIG_BLOCK, &pipe, &mut before);
            let mut pending: libc::sigset_t = std::mem::zeroed();
            libc::sigpending(&mut pending);
            let was_pending = libc::sigismember(&pending, libc::SIGPIPE) == 1;
            let sent = stdin.write_all(input);
            if !was_pending && sent.as_ref().is_err_and(|it| it.kind() == io::ErrorKind::BrokenPipe)
            {
                let now = libc::timespec { tv_sec: 0, tv_nsec: 0 };
                loop {
                    let taken = libc::sigtimedwait(&pipe, std::ptr::null_mut(), &now);
                    if taken != -1
                        || io::Error::last_os_error().kind() != io::ErrorKind::Interrupted
                    {
                        break;
                    }
                }
            }
            libc::pthread_sigmask(libc::SIG_SETMASK, &before, std::ptr::null_mut());
            sent
        }
    }
    #[cfg(target_vendor = "apple")]
    {
        use std::os::fd::AsRawFd as _;
        /// `<sys/fcntl.h>`'s, which the `libc` crate does not name for Apple's systems.
        const F_SETNOSIGPIPE: libc::c_int = 73;
        // The kernel signals the whole process here, so a thread mask cannot hold it; the
        // descriptor itself is marked to fail with `EPIPE` alone.
        // SAFETY: `fcntl` on a descriptor this process owns.
        if unsafe { libc::fcntl(stdin.as_raw_fd(), F_SETNOSIGPIPE, 1) } == -1 {
            return Err(io::Error::last_os_error());
        }
        stdin.write_all(input)
    }
    #[cfg(not(any(target_os = "linux", target_os = "android", target_vendor = "apple")))]
    {
        stdin.write_all(input)
    }
}
