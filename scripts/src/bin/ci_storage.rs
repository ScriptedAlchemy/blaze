//! CI storage observations: bounded readers, with evidence outside the measured volume.
//!
//! "$BLAZE_SCRIPTS/ci_storage" '{"volume":"/runner/blaze-fs","target":"/runner/blaze-fs/target","home":"/runner/blaze-fs/home","cargo_home":"/runner/cargo","output":"/runner/evidence","phase":"before-tests"}'
//! "$BLAZE_SCRIPTS/ci_storage" selftest

const DOC: &str = r#"ci_storage: record storage use before and after workspace test builds.

    ci_storage '{"volume":"/runner/blaze-fs","target":"/runner/blaze-fs/target","home":"/runner/blaze-fs/home","cargo_home":"/runner/cargo","output":"/runner/evidence","phase":"before-tests"}'
    ci_storage selftest

    volume       the mounted, job-local Btrfs volume (absolute path)
    target       Cargo's build directory (absolute path)
    home         the workspace tests' home (absolute path)
    cargo_home   Cargo's download/cache directory (absolute path)
    output       evidence directory outside volume, with an existing parent (absolute path)
    phase        lowercase words or digits separated by hyphens

All fields are required. Records each reader's status, deadline and output; a failed reader
makes the diagnostic incomplete, never a successful build. No files are deleted or capacity
changed. du reports file blocks, which may count reflink-shared extents more than once.
"#;

use std::{
    path::{Path, PathBuf},
    time::Duration,
};

use blaze_script::{
    TempDir,
    child::{Ended, run_logged},
    expect, fail,
    input::{self, Fields, Input, InputError},
};
use serde_json::{Value, json};

const SCRIPT: &str = "ci_storage";

struct Args {
    volume: String,
    target: String,
    home: String,
    cargo_home: String,
    output: String,
    phase: String,
}

impl Input for Args {
    const FIELDS: &'static [&'static str] = &["volume", "target", "home", "cargo_home", "output", "phase"];

    fn parse(f: &Fields<'_>) -> Result<Self, InputError> {
        let path = |name| {
            let value = f.str(name, None)?;
            if !Path::new(&value).is_absolute() {
                return Err(InputError(format!("{name} must be a nonempty absolute path")));
            }
            Ok(value)
        };
        let args = Args {
            volume: path("volume")?,
            target: path("target")?,
            home: path("home")?,
            cargo_home: path("cargo_home")?,
            output: path("output")?,
            phase: f.str("phase", None)?,
        };
        if args.phase.split('-').any(|part| part.is_empty() || !part.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())) {
            return Err(InputError("phase must contain lowercase words or digits separated by hyphens".to_owned()));
        }
        Ok(args)
    }
}

/// Resolve an existing output or its existing parent before creating anything, so an alias
/// into the volume cannot place the ENOSPC evidence on that same volume.
fn evidence_directory(args: &Args) -> Result<PathBuf, String> {
    let resolve = |path: &Path| std::fs::canonicalize(path).map_err(|e| format!("{}: {e}", path.display()));
    let volume = resolve(Path::new(&args.volume))?;
    let output = Path::new(&args.output);
    let output = if output.exists() {
        resolve(output)?
    } else {
        let parent = output.parent().ok_or("output has no parent")?;
        let name = output.file_name().ok_or("output has no directory name")?;
        resolve(parent)?.join(name)
    };
    if output.starts_with(volume) {
        return Err("storage evidence must be outside the observed volume".to_owned());
    }
    Ok(output)
}

fn log_text(path: &Path) -> Result<String, String> {
    std::fs::read(path)
        .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
        .map_err(|error| format!("{} could not be read: {error}", path.display()))
}

/// Every probe owns its bounded process group and logs; neither nonzero exit nor timeout is
/// silently turned into success. These fixed tools need no inherited credentials or home.
fn capture(argv: &[&str], output: &Path, name: &str, limit: Duration) -> Value {
    let words: Vec<String> = argv.iter().map(|word| (*word).to_owned()).collect();
    let log = output.join(name);
    let env = [("PATH", "/usr/sbin:/usr/bin:/sbin:/bin"), ("LC_ALL", "C")];
    let ended = run_logged(&words, output, &env, &log, limit);
    let (exit_code, timed_out, error) = match ended {
        Ok(Ended::Exited(measured)) => (Some(measured.status), false, None),
        Ok(Ended::TimedOut { .. }) => (None, true, None),
        Err(error) => (None, false, Some(error)),
    };
    let mut errors: Vec<String> = error.into_iter().collect();
    let mut read = |extension| match log_text(&log.with_extension(extension)) {
        Ok(text) => Some(text),
        Err(error) => {
            errors.push(error);
            None
        }
    };
    let stdout = read("out");
    let stderr = read("err");
    let error = if errors.is_empty() { None } else { Some(errors.join("; ")) };
    json!({
        "argv": words,
        "limit_seconds": limit.as_secs_f64(),
        "exit_code": exit_code,
        "timed_out": timed_out,
        "error": error,
        "stdout": stdout,
        "stderr": stderr,
    })
}

fn record(args: &Args, commands: &[Value]) -> Value {
    let complete = commands.iter().all(|command| command["exit_code"] == 0 && command["timed_out"] == false && command["error"].is_null());
    json!({
        "schema": 1,
        "phase": args.phase,
        "paths": {
            "volume": args.volume, "target": args.target, "home": args.home,
            "cargo_home": args.cargo_home, "output": args.output,
        },
        "profile_environment": {
            "CARGO_PROFILE_DEV_DEBUG": std::env::var("CARGO_PROFILE_DEV_DEBUG").ok(),
            "CARGO_PROFILE_TEST_DEBUG": std::env::var("CARGO_PROFILE_TEST_DEBUG").ok(),
            "CARGO_INCREMENTAL": std::env::var("CARGO_INCREMENTAL").ok(),
            "CARGO_BUILD_JOBS": std::env::var("CARGO_BUILD_JOBS").ok(),
        },
        "complete": complete,
        "du_accounting": "allocated file blocks; reflink-shared extents can count in more than one file",
        "commands": commands,
    })
}

fn main_with(args: &Args) {
    let output = evidence_directory(args).unwrap_or_else(|e| fail(SCRIPT, &e));
    std::fs::create_dir_all(&output).unwrap_or_else(|e| fail(SCRIPT, &format!("{}: {e}", output.display())));
    let probe = |name, argv: &[&str], seconds| capture(argv, &output, &format!("{}-{name}", args.phase), Duration::from_secs(seconds));
    let commands = [
        // run_logged allows one equally bounded signal grace phase. These 34 seconds of
        // reader limits take at most 68 seconds before report writing, within CI's 90.
        probe("df", &["df", "-B1", "/", &args.volume, &args.target, &args.home, &args.cargo_home], 3),
        probe("mount", &["findmnt", "-T", &args.volume, "-o", "TARGET,SOURCE,FSTYPE,OPTIONS"], 3),
        probe("btrfs", &["btrfs", "filesystem", "usage", "-b", &args.volume], 5),
        probe("target", &["du", "-B1", "-x", "--max-depth=3", &args.target], 15),
        probe("caches", &["du", "-B1", "-x", "--max-depth=1", &args.home, &args.cargo_home], 8),
    ];
    let report = record(args, &commands);
    let path = output.join(format!("storage-{}.json", args.phase));
    let bytes = serde_json::to_vec_pretty(&report).unwrap_or_else(|e| fail(SCRIPT, &format!("storage JSON: {e}")));
    std::fs::write(&path, bytes).unwrap_or_else(|e| fail(SCRIPT, &format!("{}: {e}", path.display())));
    let state = if report["complete"] == true { "complete" } else { "incomplete (see recorded errors)" };
    println!("{SCRIPT}: {}; diagnostic commands {state}", path.display());
}

fn selftest() {
    let tmp = TempDir::new("blaze-ci-storage-").expect("the private fixture directory");
    let volume = tmp.path().join("volume");
    std::fs::create_dir(&volume).expect("the observed volume fixture");
    let given = json!({
        "volume": volume,
        "target": volume.join("target"),
        "home": volume.join("home"),
        "cargo_home": tmp.path().join("cargo"),
        "output": tmp.path().join("evidence"),
        "phase": "after-tests",
    });
    let args: Args = input::of(&given, "").expect("the complete typed input");
    for (field, value) in [
        ("extra", json!("unreported input")),
        ("target", json!(true)),
        ("target", json!("relative")),
        ("phase", json!("../escape")),
        ("phase", json!("")),
    ] {
        let mut wrong = given.clone();
        wrong[field] = value;
        expect(SCRIPT, input::of::<Args>(&wrong, "").is_err(), "unknown, untyped, relative and unsafe inputs are refused");
    }
    let mut missing = given.clone();
    missing.as_object_mut().expect("the input object").remove("target");
    expect(SCRIPT, input::of::<Args>(&missing, "").is_err(), "every path must be explicit");
    let alias = tmp.path().join("volume-alias");
    std::os::unix::fs::symlink(&volume, &alias).expect("a symlink to the observed volume");
    for destination in [&volume, &volume.join("evidence"), &alias.join("evidence")] {
        let mut wrong = given.clone();
        wrong["output"] = json!(destination);
        let wrong: Args = input::of(&wrong, "").expect("the path is typed");
        expect(SCRIPT, evidence_directory(&wrong).is_err(), "evidence cannot live on the full volume, even through a symlink");
    }
    let output = evidence_directory(&args).expect("evidence is outside the observed volume");
    std::fs::create_dir(&output).expect("the private evidence directory");
    let failed = capture(
        &["/bin/sh", "-c", "printf 'before failure\\n'; printf 'diagnosis\\n' >&2; exit 23"],
        &output, "failed", Duration::from_secs(5),
    );
    expect(
        SCRIPT,
        failed["exit_code"] == 23 && failed["timed_out"] == false
            && failed["stdout"] == "before failure\n" && failed["stderr"] == "diagnosis\n",
        "a real failure retains both output streams and its status",
    );
    let began = std::time::Instant::now();
    let timed = capture(
        &["/bin/sh", "-c", "printf 'before timeout\\n'; exec sleep 30"],
        &output, "timed", Duration::from_millis(300),
    );
    expect(
        SCRIPT,
        timed["exit_code"].is_null() && timed["timed_out"] == true
            && timed["stdout"] == "before timeout\n" && began.elapsed() < Duration::from_secs(5),
        "a real deadline preserves partial output and is never success",
    );
    let absent = output.join("missing-reader");
    let absent = capture(&[absent.to_str().expect("the fixture path")], &output, "absent", Duration::from_secs(1));
    expect(SCRIPT, absent["exit_code"].is_null() && absent["error"].is_string(), "a missing reader is an explicit error");
    let lost = capture(
        &["/bin/sh", "-c", "rm missing-logs.out missing-logs.err"],
        &output, "missing-logs", Duration::from_secs(5),
    );
    expect(
        SCRIPT,
        lost["exit_code"] == 0 && lost["error"].is_string() && lost["stdout"].is_null()
            && record(&args, &[lost])["complete"] == false,
        "a zero-exit reader with removed evidence remains an incomplete observation",
    );
    let report = record(&args, &[failed, timed, absent]);
    expect(
        SCRIPT,
        report["complete"] == false && report["commands"][0]["exit_code"] == 23
            && report["profile_environment"].as_object().expect("profile observations").len() == 4,
        "partial diagnostic errors and the declared profile observations remain in the report",
    );
    #[cfg(target_os = "linux")]
    {
        let target = volume.join("target");
        std::fs::create_dir(&target).expect("the measured target");
        let words = ["du", "-B1", "-x", "--max-depth=3", target.to_str().expect("the fixture path")];
        let before = capture(&words, &output, "du-before", Duration::from_secs(5));
        let payload = vec![0x6d; 1024 * 1024];
        let artifact = target.join("completed-test");
        std::fs::write(&artifact, &payload).expect("the completed test artifact");
        let after = capture(&words, &output, "du-after", Duration::from_secs(5));
        let bytes = |result: &Value| {
            result["stdout"].as_str().and_then(|text| text.split_whitespace().next()).and_then(|word| word.parse::<u64>().ok())
                .expect("du reports bytes")
        };
        expect(
            SCRIPT,
            before["exit_code"] == 0 && after["exit_code"] == 0 && bytes(&after) > bytes(&before)
                && std::fs::read(&artifact).expect("the unchanged artifact") == payload,
            "real du observes artifact growth without mutating it",
        );
    }
    println!("{SCRIPT} selftest: ok");
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.as_slice() {
        [one] if one == "selftest" => selftest(),
        [one] => main_with(&input::parse::<Args>(one, SCRIPT)),
        _ => fail(SCRIPT, DOC),
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_selftest_holds() {
        super::selftest();
    }
}
