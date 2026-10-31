//! Regression checks on the real test-output reader, not a model of its parser.
use super::failures;

#[test]
fn incomplete_or_duplicate_failure_lists_do_not_authorize_partial_passes() {
    for text in [
        "\nfailures:\n    one\n\ntest result: FAILED. 1 passed; 2 failed\n",
        "\nfailures:\n    one\n    one\n\ntest result: FAILED. 1 passed; 2 failed\n",
        "\nfailures:\n    \n\ntest result: FAILED. 0 passed; 1 failed\n",
        "\nfailures:\n    one\n\ntest result: FAILED. 0 passed; 1 failed\nmore output\n",
        "\nfailures:\n    one\n\ntest result: FAILED. 0 passed; 1 failed\n\ntest result: ok. 1 passed\n",
    ] {
        assert!(failures(text).is_none(), "incomplete evidence: {text:?}");
    }
}

#[test]
fn complete_colored_failure_summary_is_still_accepted() {
    let text = "\nfailures:\n    first\n    second\n\ntest result: \x1b[31mFAILED\x1b[0m. 3 passed; 2 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.01s\n\n";
    assert_eq!(failures(text), Some(vec!["first".into(), "second".into()]));
}

#[cfg(unix)]
fn printed_summary_then(ending: &str) -> (std::process::ExitStatus, Option<Vec<String>>) {
    use std::process::Command;
    let script = format!(
        "printf '\\nfailures:\\n    flaky\\n\\ntest result: FAILED. 0 passed; 1 failed\\n'; {ending}"
    );
    let mut command = Command::new("/bin/sh");
    command.args(["-c", &script]);
    super::listing_failures(&mut command, Command::spawn).unwrap()
}

#[cfg(unix)]
#[test]
fn killed_process_cannot_supply_retry_names() {
    use std::os::unix::process::ExitStatusExt as _;
    let (status, names) = printed_summary_then("kill -KILL $$");
    assert_eq!(status.signal(), Some(9));
    assert!(names.is_none());
}

#[cfg(unix)]
#[test]
fn abnormal_exit_cannot_supply_retry_names() {
    for code in [1, 2, 96, 137] {
        let (status, names) = printed_summary_then(&format!("exit {code}"));
        assert_eq!(status.code(), Some(code));
        assert!(names.is_none(), "exit {code} was mistaken for libtest failure");
    }
}

#[cfg(unix)]
#[test]
fn successful_process_is_not_failed_by_a_nested_summary() {
    let (status, names) = printed_summary_then("exit 0");
    assert!(status.success());
    assert!(names.is_none());
}

#[cfg(unix)]
#[test]
fn normal_assertion_failure_retains_retry_names() {
    let (status, names) = printed_summary_then("exit 101");
    assert_eq!(status.code(), Some(101));
    assert_eq!(names, Some(vec!["flaky".into()]));
}
