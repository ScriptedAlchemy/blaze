//! Versioned client policy, consumed before command dispatch and never sent as cargo flags.

use std::ffi::OsString;

use super::{
    args::{Asked, Command},
    daemon::Start,
};

pub(super) const CONNECT_ONLY: &str = "--daemon-policy=connect-only-v1";

pub(super) fn mentioned(words: &[OsString]) -> bool {
    words
        .iter()
        .take_while(|it| *it != "--")
        .any(|it| it.to_str().is_some_and(|it| it.starts_with("--daemon-policy")))
}

/// A policy must be the first word after `blaze` (or `cargo blaze`). Refuse misplaced or
/// unknown versions, including on `stop`, whose legacy parser ignores trailing words.
/// Arguments after `--` belong to the test/program and are never read as client policy.
pub(super) fn take(words: &mut Vec<OsString>) -> Result<Start, String> {
    let policy = words.first().and_then(|it| it.to_str());
    let start = match policy {
        Some(CONNECT_ONLY) => {
            words.remove(0);
            Start::ConnectOnly
        }
        Some(it) if it.starts_with("--daemon-policy") => {
            return Err(format!(
                "unsupported client policy `{it}`; use `{CONNECT_ONLY}` before the command"
            ));
        }
        _ => Start::IfMissing,
    };
    if let Some(word) = words
        .iter()
        .take_while(|it| *it != "--")
        .find(|it| it.to_str().is_some_and(|it| it.starts_with("--daemon-policy")))
    {
        return Err(format!(
            "client policy `{}` must occur once, before the command: `{CONNECT_ONLY}`",
            word.to_string_lossy()
        ));
    }
    Ok(start)
}

/// v1 covers the bounded build/test/status/stop client. Fail closed for every other command,
/// including commands with their own start paths, rather than silently ignoring the policy.
pub(super) fn apply(asked: &mut Asked, start: Start) -> Result<(), String> {
    if start != Start::ConnectOnly {
        return Ok(());
    }
    match asked {
        Asked::Cargo(it) if matches!(it.command(), Command::Build | Command::Test) => {
            it.daemon_start = start;
        }
        Asked::Status(_) | Asked::Stop | Asked::Help(_) | Asked::Version => {}
        _ => {
            return Err(format!(
                "`{CONNECT_ONLY}` supports only build, test, status and stop (and help/version)"
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests;
