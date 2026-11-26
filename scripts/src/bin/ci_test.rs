//! ci_test: builds and runs the workspace's cargo test binaries in parallel with bounded per-binary
//! timeouts, keeping TMPDIR on `$HOME`'s filesystem (so shelf hard-links avoid EXDEV copies) and
//! disabling orphan daemons in non-daemon tests (`BLAZE_NO_DAEMON=1`). Replaces inline `jq` and
//! `xargs` shell pipelines in `.github/workflows/blaze.yaml`.
//!
//!     "$BLAZE_SCRIPTS/ci_test" '{"exclude": ["blaze-std"]}'
//!     "$BLAZE_SCRIPTS/ci_test" selftest

const DOC: &str = r#"ci_test: builds and runs workspace cargo test binaries in parallel.

    ci_test '{"exclude": ["blaze-std"], "jobs": 0, "threads": 4, "timeout_s": 600, "doc": true, "root": ".", "evidence": ""}'
    ci_test selftest

    exclude    workspace packages to pass to `--exclude` ([])
    jobs       parallel test binaries; 0 selects clamp(cpus / 4, 1, 16) (0)
    threads    RUST_TEST_THREADS per binary (4)
    timeout_s  per-binary timeout in seconds, process-group killed (600)
    doc        also run `cargo test --doc --no-fail-fast` (true)
    root       workspace root (".")
    evidence   directory for Cargo build JSON, executable inventory and per-binary test logs, also on failure ("")
    partitions total jobs, including base libraries/binaries/examples/doctests at partition 0 (1)
    partition  this job's zero-based partition; all jobs retain the same workspace feature roots (0)
    build_timeout_s  Cargo build deadline; kills the entire process group (2400)
    inventory_only  recover completed links from cargo-test.out after an outer kill (false)
"#;

use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    time::Duration,
};

use blaze_script::{
    child::{Ended, run_logged},
    expect, fail, fan_out,
    input::{self, Fields, Input, InputError},
    read_text,
};
use serde_json::Value;

const SCRIPT: &str = "ci_test";

#[path = "ci_test/build.rs"]
mod build;

struct Args {
    exclude: Vec<String>,
    jobs: usize,
    threads: usize,
    timeout_s: u64,
    doc: bool,
    root: String,
    evidence: String,
    partition: usize,
    partitions: usize,
    build_timeout_s: u64,
    inventory_only: bool,
}

impl Input for Args {
    const FIELDS: &'static [&'static str] = &[
        "exclude",
        "jobs",
        "threads",
        "timeout_s",
        "doc",
        "root",
        "evidence",
        "partition",
        "partitions",
        "build_timeout_s",
        "inventory_only",
    ];

    fn parse(f: &Fields<'_>) -> Result<Args, InputError> {
        let jobs = f.int("jobs", Some(0))?;
        let threads = f.int("threads", Some(4))?;
        let timeout_s = f.int("timeout_s", Some(600))?;
        let partition = f.int("partition", Some(0))?;
        let partitions = f.int("partitions", Some(1))?;
        let build_timeout_s = f.int("build_timeout_s", Some(2400))?;
        if partitions < 1 || partition < 0 || partition >= partitions || build_timeout_s <= 0 {
            return Err(InputError(
                "require 0 <= partition < partitions and build_timeout_s > 0".to_owned(),
            ));
        }
        if jobs < 0 {
            return Err(InputError("jobs must be >= 0".to_owned()));
        }
        if threads <= 0 {
            return Err(InputError("threads must be > 0".to_owned()));
        }
        if timeout_s <= 0 {
            return Err(InputError("timeout_s must be > 0".to_owned()));
        }
        Ok(Args {
            exclude: f.strs("exclude")?,
            jobs: usize::try_from(jobs).unwrap_or(0),
            threads: usize::try_from(threads).unwrap_or(4),
            timeout_s: u64::try_from(timeout_s).unwrap_or(600),
            doc: f.bool("doc", Some(true))?,
            root: f.str("root", Some("."))?,
            evidence: f.str("evidence", Some(""))?,
            partition: usize::try_from(partition).expect("nonnegative partition"),
            partitions: usize::try_from(partitions).expect("positive partition count"),
            build_timeout_s: u64::try_from(build_timeout_s).expect("positive build timeout"),
            inventory_only: f.bool("inventory_only", Some(false))?,
        })
    }
}

/// Extracts sorted, deduplicated `(manifest_dir, executable)` pairs from `cargo test --no-run
/// --message-format=json-render-diagnostics` stdout lines.
fn parse_artifacts(stdout: &str) -> Vec<(String, String)> {
    let mut found: BTreeSet<(String, String)> = BTreeSet::new();
    for line in stdout.lines() {
        let trimmed = line.trim();
        if !trimmed.starts_with('{') {
            continue;
        }
        let Ok(value) = serde_json::from_str::<Value>(trimmed) else { continue };
        if value.get("reason").and_then(Value::as_str) != Some("compiler-artifact") {
            continue;
        }
        if value.get("profile").and_then(|p| p.get("test")).and_then(Value::as_bool) != Some(true) {
            continue;
        }
        let Some(exe) = value.get("executable").and_then(Value::as_str) else { continue };
        let Some(manifest) = value.get("manifest_path").and_then(Value::as_str) else { continue };
        let dir = manifest
            .strip_suffix("/Cargo.toml")
            .map(str::to_owned)
            .or_else(|| Path::new(manifest).parent().map(|p| p.to_string_lossy().into_owned()))
            .unwrap_or_else(|| ".".to_owned());
        found.insert((dir, exe.to_owned()));
    }
    found.into_iter().collect()
}

/// Keep a failed build's partial inventory too: its completed links still occupied storage,
/// although no test binary ran. CI puts this directory outside the build filesystem.
fn preserve_build(
    directory: &Path,
    stdout: &[u8],
    bins: &[(String, String)],
    success: bool,
    exit_code: Option<i32>,
) -> std::io::Result<()> {
    std::fs::create_dir_all(directory)?;
    std::fs::write(directory.join("cargo-test.jsonl"), stdout)?;
    let executables: Vec<Value> = bins
        .iter()
        .map(|(manifest, executable)| {
            serde_json::json!({
                "manifest_directory": manifest,
                "path": executable,
                "bytes": std::fs::metadata(executable).ok().map(|m| m.len()),
            })
        })
        .collect();
    let inventory = serde_json::json!({
        "schema": 1,
        "build_success": success,
        "build_exit_code": exit_code,
        "executables": executables,
    });
    let bytes = serde_json::to_vec_pretty(&inventory).map_err(std::io::Error::other)?;
    std::fs::write(directory.join("executables.json"), bytes)
}

/// Default worker count: `clamp(cpus / 4, 1, 16)`.
fn default_jobs(cpus: usize) -> usize {
    (cpus / 4).clamp(1, 16)
}

/// Removes a temporary directory on drop.
struct TempCleanup(PathBuf);

impl Drop for TempCleanup {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Runs `rustc +<tc> --print <what>` and returns the trimmed stdout line.
fn rustc_print(tc: &str, what: &str, cwd: &Path, evidence: &Path) -> String {
    let argv = ["rustc".to_owned(), format!("+{tc}"), "--print".to_owned(), what.to_owned()];
    build::query(&argv, cwd, &evidence.join(what))
}

/// Makes a fresh temp dir under `$HOME` so test shelf hard-links stay on `$HOME`'s filesystem.
fn make_tmp_dir() -> TempCleanup {
    let base = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
    let pid = std::process::id();
    for attempt in 0u32..1000 {
        let dir = base.join(format!(".ci-tmp.{pid}.{attempt}"));
        match std::fs::create_dir(&dir) {
            Ok(()) => return TempCleanup(dir),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => fail(SCRIPT, &format!("creating temp dir {}: {e}", dir.display())),
        }
    }
    fail(SCRIPT, "could not allocate a unique .ci-tmp directory under HOME")
}

struct RunOutcome {
    name: String,
    failed: Option<String>,
    output: String,
}

fn read_lossy(path: &Path) -> String {
    std::fs::read(path).map(|b| String::from_utf8_lossy(&b).into_owned()).unwrap_or_default()
}

fn main_with(args: &Args) {
    if args.inventory_only {
        build::recover(Path::new(&args.evidence));
        return;
    }
    let root = std::fs::canonicalize(&args.root).unwrap_or_else(|_| PathBuf::from(&args.root));
    let tc = read_text(&root.join("pins/toolchain"))
        .unwrap_or_else(|| fail(SCRIPT, "pins/toolchain could not be read"))
        .trim()
        .to_owned();
    let tmp = make_tmp_dir();

    let evidence =
        if args.evidence.is_empty() { tmp.0.as_path() } else { Path::new(&args.evidence) };
    let bins = build::run(args, &tc, &root, evidence);

    // Shared library path for rustc's sysroot & target-libdir.
    let sysroot_lib = format!("{}/lib", rustc_print(&tc, "sysroot", &root, evidence));
    let target_lib = rustc_print(&tc, "target-libdir", &root, evidence);
    let lib_var = if cfg!(target_os = "macos") { "DYLD_LIBRARY_PATH" } else { "LD_LIBRARY_PATH" };
    let mut base_env: BTreeMap<String, String> = std::env::vars().collect();
    let combined_lib = match base_env.get(lib_var).filter(|v| !v.is_empty()) {
        Some(prev) => format!("{sysroot_lib}:{target_lib}:{prev}"),
        None => format!("{sysroot_lib}:{target_lib}"),
    };
    base_env.insert(lib_var.to_owned(), combined_lib);
    base_env.insert("TMPDIR".to_owned(), tmp.0.to_string_lossy().into_owned());
    base_env.insert("BLAZE_NO_DAEMON".to_owned(), "1".to_owned());
    base_env.insert("RUST_TEST_THREADS".to_owned(), args.threads.to_string());

    let jobs = if args.jobs > 0 {
        args.jobs
    } else {
        let cpus = std::thread::available_parallelism().map(usize::from).unwrap_or(8);
        default_jobs(cpus)
    };
    let limit = Duration::from_secs(args.timeout_s);
    let retained_logs =
        (!args.evidence.is_empty()).then(|| Path::new(&args.evidence).join("tests"));
    if let Some(directory) = &retained_logs {
        std::fs::create_dir_all(directory)
            .unwrap_or_else(|e| fail(SCRIPT, &format!("test logs {}: {e}", directory.display())));
    }
    let log_dir = retained_logs.as_ref().unwrap_or(&tmp.0);
    let base_env = &base_env;

    let mut passed = 0usize;
    let mut failed: Vec<String> = Vec::new();
    let indexed: Vec<(usize, String, String)> =
        bins.into_iter().enumerate().map(|(i, (d, b))| (i, d, b)).collect();
    fan_out(
        jobs,
        &indexed,
        |(idx, dir, bin)| {
            let name = Path::new(bin)
                .file_name()
                .map_or_else(|| bin.clone(), |n| n.to_string_lossy().into_owned());
            let mut env: Vec<(&str, String)> =
                base_env.iter().map(|(k, v)| (k.as_str(), v.clone())).collect();
            env.push(("CARGO_MANIFEST_DIR", dir.clone()));
            let log = log_dir.join(format!("bin-{idx}"));
            let argv = [bin.clone()];
            let ended = run_logged(&argv, Path::new(dir), &env, &log, limit);
            let out_path = log.with_extension("out");
            let err_path = log.with_extension("err");
            let outcome = match ended {
                Ok(Ended::Exited(m)) if m.status == 0 => {
                    RunOutcome { name, failed: None, output: String::new() }
                }
                Ok(Ended::Exited(m)) => {
                    let output = format!("{}{}", read_lossy(&out_path), read_lossy(&err_path));
                    RunOutcome { name, failed: Some(format!("exit {}", m.status)), output }
                }
                Ok(Ended::TimedOut { after }) => {
                    let output = format!("{}{}", read_lossy(&out_path), read_lossy(&err_path));
                    RunOutcome {
                        name,
                        failed: Some(format!("timeout after {}s", after.as_secs())),
                        output,
                    }
                }
                Err(why) => RunOutcome {
                    name,
                    failed: Some(format!("did not start: {why}")),
                    output: String::new(),
                },
            };
            if retained_logs.is_none() {
                let _ = std::fs::remove_file(&out_path);
                let _ = std::fs::remove_file(&err_path);
            }
            outcome
        },
        |outcome| match outcome.failed {
            None => passed += 1,
            Some(why) => {
                println!("::error::FAIL {} ({why})", outcome.name);
                if !outcome.output.is_empty() {
                    print!("{}", outcome.output);
                }
                failed.push(outcome.name);
            }
        },
    );

    if args.doc && args.partition == 0 {
        let mut doc_argv = vec![
            "cargo".to_owned(),
            format!("+{tc}"),
            "test".to_owned(),
            "--locked".to_owned(),
            "--workspace".to_owned(),
            "--doc".to_owned(),
            "--no-fail-fast".to_owned(),
        ];
        for pkg in &args.exclude {
            doc_argv.push("--exclude".to_owned());
            doc_argv.push(pkg.clone());
        }
        let doc_env: Vec<(&str, String)> =
            base_env.iter().map(|(k, v)| (k.as_str(), v.clone())).collect();
        let doc_log = log_dir.join("doctests");
        let doc_out = doc_log.with_extension("out");
        let doc_err = doc_log.with_extension("err");
        match run_logged(&doc_argv, &root, &doc_env, &doc_log, limit) {
            Ok(Ended::Exited(m)) if m.status == 0 => {}
            Ok(Ended::Exited(m)) => {
                println!("::error::FAIL doctests (exit {})", m.status);
                print!("{}{}", read_lossy(&doc_out), read_lossy(&doc_err));
                failed.push("doctests".to_owned());
            }
            Ok(Ended::TimedOut { after }) => {
                println!("::error::FAIL doctests (timeout after {}s)", after.as_secs());
                print!("{}{}", read_lossy(&doc_out), read_lossy(&doc_err));
                failed.push("doctests".to_owned());
            }
            Err(why) => {
                println!("::error::FAIL doctests (did not start: {why})");
                failed.push("doctests".to_owned());
            }
        }
        if retained_logs.is_none() {
            let _ = std::fs::remove_file(&doc_out);
            let _ = std::fs::remove_file(&doc_err);
        }
    }

    println!("{SCRIPT}: {passed} passed, {} failed (jobs={jobs})", failed.len());
    if !failed.is_empty() {
        println!("::error::failing test binaries:");
        for name in &failed {
            println!("{name}");
        }
        drop(tmp);
        blaze_script::exit(1);
    }
}

fn selftest() {
    build::selftest();
    let sample = r#"
not json at all
{"reason":"compiler-artifact","manifest_path":"/w/crates/a/Cargo.toml","profile":{"test":false},"executable":"/t/a"}
{"reason":"compiler-artifact","manifest_path":"/w/crates/b/Cargo.toml","profile":{"test":true},"executable":null}
{"reason":"compiler-artifact","manifest_path":"/w/crates/b/Cargo.toml","profile":{"test":true},"executable":"/t/b-2"}
{"reason":"compiler-artifact","manifest_path":"/w/crates/a/Cargo.toml","profile":{"test":true},"executable":"/t/a-1"}
{"reason":"compiler-artifact","manifest_path":"/w/crates/a/Cargo.toml","profile":{"test":true},"executable":"/t/a-1"}
"#;
    let parsed = parse_artifacts(sample);
    expect(
        SCRIPT,
        parsed
            == vec![
                ("/w/crates/a".to_owned(), "/t/a-1".to_owned()),
                ("/w/crates/b".to_owned(), "/t/b-2".to_owned()),
            ],
        "filters test executables, strips /Cargo.toml, deduplicates and sorts",
    );
    expect(
        SCRIPT,
        default_jobs(1) == 1
            && default_jobs(4) == 1
            && default_jobs(64) == 16
            && default_jobs(128) == 16,
        "default_jobs clamps to 1..=16",
    );
    let tmp = make_tmp_dir();
    let executable = tmp.0.join("completed-test");
    std::fs::write(&executable, b"completed").expect("the completed test artifact writes");
    let partial = vec![("/w/crates/a".to_owned(), executable.to_string_lossy().into_owned())];
    preserve_build(&tmp.0, sample.as_bytes(), &partial, false, Some(101))
        .expect("failed-build evidence writes");
    let raw = std::fs::read(tmp.0.join("cargo-test.jsonl")).expect("raw Cargo JSON reads");
    let inventory: Value = serde_json::from_slice(
        &std::fs::read(tmp.0.join("executables.json")).expect("the inventory reads"),
    )
    .expect("the inventory is JSON");
    expect(SCRIPT, raw == sample.as_bytes(), "failed-build evidence keeps the exact Cargo stream");
    expect(
        SCRIPT,
        inventory["build_success"] == false
            && inventory["build_exit_code"] == 101
            && inventory["executables"][0]["manifest_directory"] == "/w/crates/a"
            && inventory["executables"][0]["bytes"] == 9,
        "a partial build records completed bytes without claiming execution or build success",
    );
    println!("{SCRIPT} selftest: ok");
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.as_slice() {
        [one] if one == "selftest" => selftest(),
        [one] => main_with(&input::parse::<Args>(one, SCRIPT)),
        _ => {
            eprintln!("usage: {SCRIPT} '<json>' | selftest\n\n{DOC}");
            blaze_script::exit(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_selftest_holds() {
        selftest();
    }
}
