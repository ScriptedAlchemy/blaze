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
    let stdin = child.stdin.take();
    std::thread::scope(|scope| {
        // Drain stdout/stderr while sending the description. The compiler may print more
        // than a pipe can hold before it consumes stdin; sending first deadlocks both sides.
        let writer =
            std::thread::Builder::new().name("child-stdin".into()).spawn_scoped(scope, move || {
                match stdin {
                    Some(mut stdin) => send(&mut stdin, input),
                    None => Err(io::Error::other("the child's standard input is not a pipe")),
                }
            });
        let writer = match writer {
            Ok(writer) => writer,
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(error);
            }
        };
        let output = child.wait_with_output();
        let sent =
            writer.join().unwrap_or_else(|_| Err(io::Error::other("child input writer panicked")));
        let output = output?;
        match sent {
            Err(error) if output.status.success() => Err(error),
            _ => Ok(output),
        }
    })
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

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::{
        io::Read as _,
        process::{Command, Stdio},
        sync::mpsc,
        time::Duration,
    };

    const MODE: &str = "BLAZE_CHILD_FEED_TEST";
    const BYTES: usize = 2 * 1024 * 1024;

    fn helper(mode: &str) -> Command {
        let mut command = Command::new(std::env::current_exe().unwrap());
        command.args(["--exact", "child::tests::child_feed_helper", "--ignored", "--nocapture"]);
        command.env(MODE, mode).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
        command
    }

    fn bounded_feed(mode: &str) -> io::Result<Output> {
        let child = helper(mode).spawn().unwrap();
        let pid = child.id();
        let (done, wait) = mpsc::channel();
        // A regression must fail rather than hang the whole compiler test suite.
        let watchdog = std::thread::spawn(move || {
            if matches!(
                wait.recv_timeout(Duration::from_secs(15)),
                Err(mpsc::RecvTimeoutError::Timeout)
            ) {
                // SAFETY: this is the still-unreaped child owned by feed on the caller.
                unsafe {
                    libc::kill(pid as libc::pid_t, libc::SIGKILL);
                }
                true
            } else {
                false
            }
        });
        let result = feed(child, &vec![b'i'; BYTES]);
        let _ = done.send(());
        assert!(!watchdog.join().unwrap(), "child input/output deadlocked");
        result
    }

    #[test]
    fn drains_output_while_sending_large_input() {
        let output = bounded_feed("chatty").unwrap();
        assert!(output.status.success());
        assert!(output.stdout.windows(1024).any(|part| part == [b'o'; 1024]));
        assert_eq!(output.stderr, vec![b'e'; BYTES]);
    }

    #[test]
    fn preserves_verbose_rejection_before_reading_input() {
        let output = bounded_feed("reject").unwrap();
        assert_eq!(output.status.code(), Some(23));
        assert_eq!(output.stderr, vec![b'e'; BYTES]);
    }

    #[test]
    fn successful_early_exit_does_not_hide_unsent_input() {
        assert_eq!(bounded_feed("closed").unwrap_err().kind(), io::ErrorKind::BrokenPipe);
    }

    #[test]
    fn sending_remains_safe_with_default_sigpipe() {
        let output = helper("sigpipe").output().unwrap();
        assert!(output.status.success(), "{:?}", output);
    }

    #[test]
    #[ignore = "subprocess helper exercised by the tests above"]
    fn child_feed_helper() {
        match std::env::var(MODE).as_deref() {
            Ok("chatty") => {
                io::stdout().write_all(&vec![b'o'; BYTES]).unwrap();
                io::stderr().write_all(&vec![b'e'; BYTES]).unwrap();
                let mut input = Vec::new();
                io::stdin().read_to_end(&mut input).unwrap();
                assert_eq!(input, vec![b'i'; BYTES]);
            }
            Ok("reject") => {
                io::stderr().write_all(&vec![b'e'; BYTES]).unwrap();
                std::process::exit(23);
            }
            Ok("closed") => {
                // SAFETY: this helper runs in its own process and owns fd 0.
                unsafe {
                    libc::close(0);
                }
            }
            Ok("sigpipe") => {
                // SAFETY: only the dedicated helper's signal disposition is changed.
                unsafe {
                    libc::signal(libc::SIGPIPE, libc::SIG_DFL);
                }
                let output = bounded_feed("reject").unwrap();
                assert_eq!(output.status.code(), Some(23));
            }
            _ => panic!("missing child feed test mode"),
        }
        std::process::exit(0);
    }
}
