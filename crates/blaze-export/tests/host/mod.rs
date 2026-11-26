//! The exported image on disk and the deadline for its real host loader.
use blaze_core::temp::TempDir;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::process::{Child, Command, ExitStatus};
use std::time::{Duration, Instant};
use std::io::Write;

pub(super) fn wait(mut child: Child, what: &str, limit: Duration) -> ExitStatus {
    let until = Instant::now() + limit;
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            return status;
        }
        if Instant::now() >= until {
            child.kill().unwrap();
            child.wait().unwrap();
            panic!("{what} exceeded {} seconds", limit.as_secs());
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

pub(super) fn run(name: &str, bytes: &[u8], command: impl FnOnce(&Path) -> Command) -> ExitStatus {
    let dir = TempDir::for_test("blaze-export-");
    let path = dir.join(name);
    let mut file = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o700).open(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    file.write_all(bytes).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    drop(file);
    let child = command(&path).spawn().unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    wait(child, name, Duration::from_secs(20))
}
