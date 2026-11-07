//! Shutdown deadlines must survive a blocked keeper and unusable stderr.
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
                steps.send(Step::Push).unwrap();
                // Keep the receiver alive without consuming the queued shutdown.
                let _receiver = coming;
                request_end(steps, REQUESTED_EXIT, Duration::from_millis(25)).await
            }
            "closed" => {
                drop(coming);
                request_end(steps, REQUESTED_EXIT, LAST_PUSH).await
            }
            "graceful" => {
                tokio::spawn(async move {
                    let Some(Step::End(signal)) = coming.recv().await else {
                        panic!("no graceful ending request");
                    };
                    end_by(signal);
                });
                request_end(steps, REQUESTED_EXIT, LAST_PUSH).await
            }
            _ => panic!("unknown shutdown fixture"),
        }
    });
}

fn exits_by_requested_signal(mode: &str, stderr: Stdio) {
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "remote::keeper::shutdown_tests::shutdown_child", "--nocapture"])
        .env(CHILD, mode)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(stderr)
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(5));
            }
            result => {
                let _ = child.kill();
                let _ = child.wait();
                panic!("{mode}: child did not exit: {result:?}");
            }
        }
    };
    #[cfg(unix)]
    assert_eq!(status.signal(), Some(REQUESTED_EXIT), "unexpected child result: {status}");
    #[cfg(windows)]
    assert_eq!(status.code(), Some(REQUESTED_EXIT), "unexpected child result: {status}");
}

#[test]
fn a_blocked_prior_push_cannot_delay_the_signal_deadline() {
    exits_by_requested_signal("blocked", Stdio::inherit());
}

#[test]
fn a_dead_keeper_cannot_swallow_the_shutdown_signal() {
    exits_by_requested_signal("closed", Stdio::inherit());
}

#[test]
fn a_responsive_keeper_can_finish_before_the_signal_deadline() {
    exits_by_requested_signal("graceful", Stdio::inherit());
}

#[test]
fn an_unwritable_stderr_cannot_swallow_the_signal_deadline() {
    let sink = tempfile::NamedTempFile::new().unwrap();
    let read_only = File::open(sink.path()).unwrap();
    exits_by_requested_signal("blocked", Stdio::from(read_only));
}

#[cfg(unix)]
#[test]
fn stderr_backpressure_cannot_delay_the_signal_deadline() {
    let (mut writer, _reader) = UnixStream::pair().unwrap();
    writer.set_nonblocking(true).unwrap();
    loop {
        match writer.write(&[0]) {
            Ok(0) => panic!("stderr fixture unexpectedly stopped accepting bytes"),
            Ok(_) => {}
            Err(error) if error.kind() == ErrorKind::Interrupted => continue,
            Err(error) if error.kind() == ErrorKind::WouldBlock => break,
            Err(error) => panic!("filling stderr fixture: {error}"),
        }
    }
    // Leave no room for even a one-byte diagnostic, and never drain the receiver.
    writer.set_nonblocking(false).unwrap();
    let descriptor: OwnedFd = writer.into();
    exits_by_requested_signal("blocked", Stdio::from(descriptor));
}
