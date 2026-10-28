//! The log lines of a build's leases, and a barrier the harness holds a worker at.
//!
//! `script-lease: <what> build=<n> pid=<pid> at_us=<epoch µs> <detail>` is one line a worker's
//! log has for each step of a build's leases (`holds`, `runs`, `put-off`, `waits`, `retired`), so
//! that a harness reads their order from the log (`harness/leasebarrier.py`).
//!
//! The barrier is the harness's hold on a worker at a named point, as `RA_JIT_RELOAD_PAUSE_MS`
//! and the other pauses are, but by files and not by time: it is armed while
//! `$RA_JIT_SCRIPT_BARRIER_DIR/<point>.hold` exists, a build that reaches it writes
//! `<point>.<build>.reached` and waits for `<point>.<build>.go`. The points are `consume`
//! (a build's compile and link are to begin, its scripts judged) and `retire` (the daemon has
//! answered the client and says the build is over). A wait ends at
//! `RA_JIT_SCRIPT_BARRIER_SECS` (300) with `<point>.<build>.timeout` written, so that a harness
//! that does not release a worker fails by name and a worker does not hang.

use std::{path::PathBuf, time::Duration};

/// One line of the worker's log.
pub(crate) fn event(what: &str, build: u64, detail: &str) {
    eprintln!(
        "script-lease: {what} build={build} pid={} at_us={} {detail}",
        std::process::id(),
        crate::live::epoch_us()
    );
}

/// Holds the caller at `point` for `build` while the harness says so.
pub(crate) fn hold(point: &str, build: u64) {
    let Some(dir) = std::env::var_os("RA_JIT_SCRIPT_BARRIER_DIR").map(PathBuf::from) else {
        return;
    };
    if build == 0 || !dir.join(format!("{point}.hold")).exists() {
        return;
    }
    let mark = |suffix: &str| dir.join(format!("{point}.{build}.{suffix}"));
    let _ = std::fs::write(mark("reached"), std::process::id().to_string());
    event("barrier-reached", build, point);
    let seconds = std::env::var("RA_JIT_SCRIPT_BARRIER_SECS").ok().and_then(|it| it.parse().ok());
    let deadline = std::time::Instant::now() + Duration::from_secs(seconds.unwrap_or(300));
    while !mark("go").exists() {
        if std::time::Instant::now() > deadline {
            let _ = std::fs::write(mark("timeout"), "");
            event("barrier-timeout", build, point);
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    event("barrier-released", build, point);
}
