//! The exported image on disk and the deadline for its real host loader.
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::process::{Child, Command, ExitStatus};
use std::time::{Duration, Instant};
use std::io::Write;

pub fn wait(mut child: Child, what: &str, limit: Duration) -> ExitStatus {
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

pub fn run(name: &str, bytes: &[u8], command: impl FnOnce(&Path) -> Command) -> ExitStatus {
    let path = std::env::temp_dir().join(format!("blaze-export-{}-{name}", std::process::id()));
    let mut file = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o700).open(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    file.write_all(bytes).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    drop(file);
    let child = command(&path).spawn().unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    let status = wait(child, name, Duration::from_secs(20));
    std::fs::remove_file(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    status
}
