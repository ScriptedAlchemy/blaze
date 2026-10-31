//! Exercise retries and cache settlement together through the shared production runner.
use super::{TestRun, affected, test_with};
use std::{path::Path, process::Command};

fn context<'a>(root: &'a Path, script: &'a Path) -> TestRun<'a> {
    TestRun {
        cache: affected::TestContext {
            root,
            directory: Some(root),
            package: "p",
            programs: Ok(String::new()),
            enabled: true,
            passed_env: "",
            inherit_env: false,
        },
        package_dir: root,
        executable: script,
        label: "retry/cache regression",
        sandbox: None,
        sandbox_env: None,
        attempts: affected::Attempts::with_flaky(&["flaky"]),
    }
}

fn command(root: &Path, script: &Path) -> anyhow::Result<Command> {
    let mut command = Command::new("/bin/sh");
    command.arg(script).current_dir(root).env_clear();
    Ok(command)
}

#[test]
fn crashes_never_become_flaky_successes_or_partial_cached_passes() {
    for ending in ["kill -KILL $$", "exit 137", "exit 96"] {
        let work = tempfile::tempdir().unwrap();
        let root = work.path();
        let script = root.join("test.sh");
        std::fs::write(&script, format!(r#"
if [ -f attempted ]; then
    printf '\ntest flaky ... ok\n\ntest result: ok. 1 passed; 0 failed\n'
    exit 0
fi
printf yes >attempted
printf '\nfailures:\n    flaky\n\ntest result: FAILED. 1 passed; 1 failed\n'
{ending}
"#)).unwrap();
        std::fs::write(root.join("tests-reach"), "new\tflaky\nnew\tother\n").unwrap();
        std::fs::write(root.join("tests-passed"), "old\tflaky\nold\tother\n").unwrap();
        let asked = crate::cargo::cli::of_cargo(&["test".into()]).unwrap();
        let context = context(root, &script);
        let mut starts = 0;
        let result = test_with(&asked, &context, || command(root, &script), |cmd| {
            starts += 1;
            cmd.spawn()
        }).unwrap();
        assert!(!result.success, "a crashed process passed after retry: {ending}");
        assert_eq!(starts, 1, "a crash is not an assertion failure to retry");
        assert!(std::fs::read_to_string(root.join("tests-passed")).unwrap_or_default().is_empty());
    }
}

#[test]
fn command_construction_error_revokes_earlier_passes() {
    let work = tempfile::tempdir().unwrap();
    let root = work.path();
    let script = root.join("test.sh");
    let saved = root.join("tests-passed");
    std::fs::write(&saved, "old\tfirst\n").unwrap();
    let asked = crate::cargo::cli::of_cargo(&["test".into()]).unwrap();
    let context = context(root, &script);
    let result = test_with(&asked, &context, || anyhow::bail!("fixture command error"), Command::spawn);
    assert!(result.is_err());
    assert!(!saved.exists(), "a failed invocation retained an older success");
}

#[test]
fn failed_retry_spawn_revokes_earlier_passes() {
    let work = tempfile::tempdir().unwrap();
    let root = work.path();
    let script = root.join("test.sh");
    std::fs::write(&script, "printf '\\nfailures:\\n    flaky\\n\\ntest result: FAILED. 1 passed; 1 failed\\n'; exit 101\n").unwrap();
    std::fs::write(root.join("tests-reach"), "new\tflaky\nnew\tother\n").unwrap();
    let saved = root.join("tests-passed");
    std::fs::write(&saved, "old\tflaky\nold\tother\n").unwrap();
    let asked = crate::cargo::cli::of_cargo(&["test".into()]).unwrap();
    let context = context(root, &script);
    let mut starts = 0;
    let result = test_with(&asked, &context, || command(root, &script), |cmd| {
        starts += 1;
        if starts == 2 {
            return Err(std::io::Error::other("fixture retry spawn failure"));
        }
        cmd.spawn()
    });
    assert!(result.is_err());
    assert_eq!(starts, 2);
    assert!(!saved.exists(), "an interrupted retry retained earlier successes");
}
