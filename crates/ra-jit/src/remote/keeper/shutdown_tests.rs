//! A signal cannot wait behind an earlier remote operation before its grace period begins.
use super::*;
use std::fs::File;
#[cfg(unix)]
use std::io::{ErrorKind, Write};
#[cfg(unix)]
use std::os::fd::OwnedFd;
#[cfg(unix)]
use std::os::unix::{net::UnixStream, process::ExitStatusExt};
use std::process::{Command, Stdio};

const CHILD: &str = "BLAZE_REMOTE_SHUTDOWN_CHILD";
#[cfg(unix)]
const REQUESTED_EXIT: i32 = libc::SIGTERM;
#[cfg(windows)]
const REQUESTED_EXIT: i32 = CONTROL_C_EXIT;

#[test]
fn shutdown_child() {
    let Ok(mode) = std::env::var(CHILD) else { return };
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    runtime.block_on(async {
        let (steps, mut coming) = unbounded_channel();
        match mode.as_str() {
            "blocked" => {
                // A previously queued push owns the keeper. No receiver can reach End.
                steps.send(Step::Push).unwrap();
                request_end(steps, REQUESTED_EXIT, Duration::from_millis(25)).await;
                // Keep the receiver alive, without manufacturing successful publication.
                let _held = coming;
                std::future::pending::<()>().await;
            }
            "closed" => {
                drop(coming);
                request_end(steps, REQUESTED_EXIT, LAST_PUSH).await;
                std::future::pending::<()>().await;
            }
            "graceful" => {
                tokio::spawn(async move {
                    let Some(Step::End(signal)) = coming.recv().await else {
                        panic!("no graceful ending request");
                    };
                    end_by(signal);
                });
                request_end(steps, REQUESTED_EXIT, LAST_PUSH).await;
                std::future::pending::<()>().await;
            }
            _ => panic!("unknown shutdown fixture"),
        }
    });
}

fn exits_by_requested_signal(mode: &str) {
    exits_with_stderr(mode, Stdio::inherit());
}

fn exits_with_stderr(mode: &str, stderr: Stdio) {
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "remote::keeper::shutdown_tests::shutdown_child", "--nocapture"])
        .env(CHILD, mode)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(stderr)
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            #[cfg(unix)]
            assert_eq!(status.signal(), Some(REQUESTED_EXIT), "unexpected child result: {status}");
            #[cfg(windows)]
            assert_eq!(status.code(), Some(REQUESTED_EXIT), "unexpected child result: {status}");
            return;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("{mode}: remote keeper suppressed process shutdown");
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn a_blocked_prior_push_cannot_delay_the_signal_deadline() {
    exits_by_requested_signal("blocked");
}

#[test]
fn a_dead_keeper_cannot_swallow_the_shutdown_signal() {
    exits_by_requested_signal("closed");
}

#[test]
fn a_responsive_keeper_can_finish_before_the_signal_deadline() {
    exits_by_requested_signal("graceful");
}

#[test]
fn an_unwritable_stderr_cannot_swallow_the_signal_deadline() {
    let sink = tempfile::NamedTempFile::new().unwrap();
    let read_only = File::open(sink.path()).unwrap();
    exits_with_stderr("blocked", Stdio::from(read_only));
}

#[cfg(unix)]
#[test]
fn stderr_backpressure_cannot_delay_the_signal_deadline() {
    let (mut writer, _reader) = UnixStream::pair().unwrap();
    writer.set_nonblocking(true).unwrap();
    loop {
        match writer.write(&[0; 4096]) {
            Ok(0) => panic!("stderr fixture unexpectedly stopped accepting bytes"),
            Ok(_) => {}
            Err(error) if error.kind() == ErrorKind::Interrupted => continue,
            Err(error) if error.kind() == ErrorKind::WouldBlock => break,
            Err(error) => panic!("filling stderr fixture: {error}"),
        }
    }
    // Nothing drains the receiver while the child runs. An attempted diagnostic
    // write would block, so a deadline that depends on stderr cannot pass.
    writer.set_nonblocking(false).unwrap();
    let descriptor: OwnedFd = writer.into();
    exits_with_stderr("blocked", Stdio::from(descriptor));
}
