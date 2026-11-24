//! One native test driver owns the process-wide wait choice, scoped threads, and isolated
//! process cases. Every test that can enter a kernel wait initializes this driver first;
//! the allocator control and other native tests therefore agree even when run in parallel.

use core::time::Duration;
use std::io::Read;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::Once;

use blaze_core::wait::{Clock, Deadline, Monotonic};

/// Program threads use direct waits, including the allocation-free stub path. Choosing once
/// before any thread waits avoids changing drivers underneath an already parked waiter.
pub(crate) fn init() {
    static CHOSEN: Once = Once::new();
    CHOSEN.call_once(|| {
        #[cfg(target_os = "linux")]
        blaze_core::wait::choose(blaze_core::wait::Waits::Direct);
    });
}

/// A native scoped-thread case starts only after the shared process driver is initialized.
pub(crate) fn scope<'env, F, T>(f: F) -> T
where
    F: for<'scope> FnOnce(&'scope std::thread::Scope<'scope, 'env>) -> T,
{
    init();
    std::thread::scope(f)
}

#[cfg(not(blaze_check))]
#[derive(Clone, Copy, Debug)]
pub(crate) enum SignalCase { Handler, Retired, Default }

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum ArgumentCase { Installed, OldEnvironment }

#[derive(Clone, Copy, Debug)]
pub(crate) enum Case {
    #[cfg(not(blaze_check))]
    Signal(SignalCase),
    Arguments(ArgumentCase),
    #[cfg(target_arch = "aarch64")]
    NativeParentClosed,
}

impl Case {
    fn name(self) -> &'static str {
        match self {
            #[cfg(not(blaze_check))]
            Self::Signal(SignalCase::Handler) => "signal-handler",
            #[cfg(not(blaze_check))]
            Self::Signal(SignalCase::Retired) => "signal-retired",
            #[cfg(not(blaze_check))]
            Self::Signal(SignalCase::Default) => "signal-default",
            Self::Arguments(ArgumentCase::Installed) => "arguments-installed",
            Self::Arguments(ArgumentCase::OldEnvironment) => "arguments-old-environment",
            #[cfg(target_arch = "aarch64")]
            Self::NativeParentClosed => "native-parent-closed",
        }
    }

    fn parse(text: &str) -> Option<Case> {
        Some(match text {
            #[cfg(not(blaze_check))]
            "signal-handler" => Self::Signal(SignalCase::Handler),
            #[cfg(not(blaze_check))]
            "signal-retired" => Self::Signal(SignalCase::Retired),
            #[cfg(not(blaze_check))]
            "signal-default" => Self::Signal(SignalCase::Default),
            "arguments-installed" => Self::Arguments(ArgumentCase::Installed),
            "arguments-old-environment" => Self::Arguments(ArgumentCase::OldEnvironment),
            #[cfg(target_arch = "aarch64")]
            "native-parent-closed" => Self::NativeParentClosed,
            _ => return None,
        })
    }

    fn bound(self) -> Duration {
        match self {
            #[cfg(not(blaze_check))]
            Self::Signal(_) => Duration::from_secs(60),
            Self::Arguments(_) => Duration::from_secs(30),
            #[cfg(target_arch = "aarch64")]
            Self::NativeParentClosed => Duration::from_secs(5),
        }
    }
}

const CASE: &str = "BLAZE_LAUNCH_TEST_CASE";
const CHILD: &str = "driver_tests::child";

/// The sole environment dispatcher. With no case this is the ordinary parent run; every
/// parent control instead launches this exact test and requires the case's distinctive exit.
#[test]
fn child() {
    let Some(named) = std::env::var_os(CASE) else { return; };
    let case = named.to_str().and_then(Case::parse).unwrap_or_else(|| panic!("{CASE} names no case: {named:?}"));
    init();
    let status = match case {
        #[cfg(not(blaze_check))]
        Case::Signal(signal) => crate::signals::tests::child_case(signal),
        Case::Arguments(arguments) => crate::run_mode::entry_tests::process_args_tests::child_case(arguments),
        #[cfg(target_arch = "aarch64")]
        Case::NativeParentClosed => crate::run_mode::entry_tests::native_parent_closed(),
    };
    // C exit, so the argument case's atexit observer actually executes after its body returns.
    std::process::exit(status);
}

pub(crate) struct ChildOutput {
    pub status: ExitStatus,
    pub stderr: String,
}

/// A failed control owns its child until reap too. The caller reports cleanup failure beside
/// the original error, and every wait after SIGKILL has its own finite deadline.
fn kill_and_reap(child: &mut Child, clock: &Monotonic) -> std::io::Result<()> {
    child.kill()?;
    let deadline = Deadline::after(clock.now(), Duration::from_secs(5));
    loop {
        if child.try_wait()?.is_some() { return Ok(()); }
        let now = clock.now();
        if deadline.passed(now) {
            return Err(std::io::Error::new(std::io::ErrorKind::TimedOut,
                "the native test child was not reaped within five seconds after SIGKILL"));
        }
        let next = Deadline::at((now + Duration::from_millis(5)).min(deadline.get()));
        blaze_core::wait::sleep_until("reaping the killed native test child", next, clock)
            .map_err(std::io::Error::other)?;
    }
}

/// Runs a typed process case with the same explicit two-entry environment on both platforms.
/// The signal cases and argument cases share startup, deadline, failure cleanup and output.
pub(crate) fn run(case: Case) -> ChildOutput {
    init();
    let exe = std::env::current_exe().expect("the test binary's path");
    let mut child = Command::new(exe)
        .args([CHILD, "--exact", "--nocapture", "--test-threads=1"])
        .env_clear().env(CASE, case.name()).env("BLAZE_ARGS_PROBE", "before")
        .stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::piped())
        .spawn().expect("the native test child starts");
    let clock = Monotonic::new();
    let deadline = Deadline::after(clock.now(), case.bound());
    let status = loop {
        if let Some(status) = child.try_wait().expect("the native test child's status") { break status; }
        let now = clock.now();
        if deadline.passed(now) {
            let cleanup = kill_and_reap(&mut child, &clock);
            panic!("native test case {case:?} did not exit within {:?}; kill/reap: {cleanup:?}", case.bound());
        }
        let next = Deadline::at((now + Duration::from_millis(5)).min(deadline.get()));
        if let Err(why) = blaze_core::wait::sleep_until("the native test child's exit", next, &clock) {
            let cleanup = kill_and_reap(&mut child, &clock);
            panic!("native test case {case:?}: {why}; kill/reap: {cleanup:?}");
        }
    };
    let mut stderr = String::new();
    child.stderr.take().expect("the child's stderr").read_to_string(&mut stderr).expect("the child's stderr reads");
    ChildOutput { status, stderr }
}
