//! Writes `expected.json` of each fixture of `tests/units`: cargo 1.99.0's own
//! `cargo <args> --unit-graph -Z unstable-options` for every run of the fixture's `case.json`,
//! which `tests/units.rs` compares blaze's unit graph with. cargo runs here and nowhere else:
//! the library and its tests never run it.
//!
//! usage: units_expected [FIXTURE]...      (no name: every fixture)
//!
//! Each fixture is copied to a scratch directory and run there with a `CARGO_HOME` of its own,
//! `--locked` and `--offline`, so that its hand-written `Cargo.lock` is checked and nothing is
//! fetched. The scratch root is `$ROOT` in what is written, the `CARGO_HOME` `$CARGO_HOME`.
//! `expected.json` has the host's triple and cfg (`rustc --print cfg`, which the test is given
//! rather than run rustc), and for each run its arguments, its environment, cargo's exit code
//! and either its graph or its error.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

const TOOLCHAIN: &str = "+1.99.0";

fn copy_dir(from: &Path, to: &Path) -> Result<()> {
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let name = entry.file_name();
        if name == "expected.json" || name == "case.json" {
            continue;
        }
        if entry.file_type()?.is_dir() {
            copy_dir(&entry.path(), &to.join(&name))?;
        } else {
            std::fs::copy(entry.path(), to.join(&name))?;
        }
    }
    Ok(())
}

fn output(command: &mut Command) -> Result<String> {
    let out = command.output().context("running a command")?;
    anyhow::ensure!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    Ok(String::from_utf8(out.stdout)?)
}

/// The error of `stderr`, from its first `error` line on, as the harnesses take it.
fn error_text(stderr: &str) -> String {
    let lines: Vec<&str> = stderr.lines().collect();
    let start = lines.iter().position(|l| l.starts_with("error")).unwrap_or(lines.len());
    lines[start..]
        .iter()
        .filter(|l| !l.trim().is_empty())
        .map(|l| l.trim_end())
        .collect::<Vec<_>>()
        .join("\n")
}

fn fixture(dir: &Path, host: &Value, cfg: &Value, version: &str) -> Result<()> {
    let name = dir.file_name().unwrap().to_string_lossy().into_owned();
    let case: Value = serde_json::from_str(&std::fs::read_to_string(dir.join("case.json"))?)?;
    let scratch =
        std::env::temp_dir().join(format!("blaze-units-expected-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&scratch);
    let root = scratch.join("root");
    copy_dir(dir, &root)?;
    let root = root.canonicalize()?;
    let home = scratch.join("cargo-home");
    std::fs::create_dir_all(&home)?;
    let home = home.canonicalize()?;
    let (root_text, home_text) = (root.display().to_string(), home.display().to_string());
    let lock_before = std::fs::read(root.join("Cargo.lock"))?;

    let mut runs = Vec::new();
    for run in case["runs"].as_array().context("`runs`")? {
        let args: Vec<String> = run["args"]
            .as_array()
            .context("`args`")?
            .iter()
            .map(|it| it.as_str().unwrap().to_owned())
            .collect();
        let mut command = Command::new("cargo");
        command.arg(TOOLCHAIN).args(&args);
        command.args(["--unit-graph", "-Z", "unstable-options", "--locked", "--offline"]);
        command.current_dir(&root).env_clear();
        for key in ["PATH", "HOME", "RUSTUP_HOME", "TMPDIR"] {
            if let Ok(value) = std::env::var(key) {
                command.env(key, value);
            }
        }
        // The unit graph is unstable: the toolchain is told it may use unstable options.
        command.env("RUSTC_BOOTSTRAP", "1").env("CARGO_HOME", &home);
        let mut env = serde_json::Map::new();
        for (key, value) in run["env"].as_object().into_iter().flatten() {
            command.env(key, value.as_str().unwrap());
            env.insert(key.clone(), value.clone());
        }
        let out = command.output().context("running cargo")?;
        let normalize =
            |text: &str| text.replace(&root_text, "$ROOT").replace(&home_text, "$CARGO_HOME");
        let mut entry = json!({ "args": args, "env": env });
        if out.status.success() {
            let graph: Value = serde_json::from_str(&normalize(&String::from_utf8(out.stdout)?))
                .with_context(|| format!("the unit graph of `{}`", args.join(" ")))?;
            entry["code"] = json!(0);
            entry["graph"] = graph;
        } else {
            entry["code"] = json!(out.status.code().unwrap_or(-1));
            entry["error"] = json!(normalize(&error_text(&String::from_utf8_lossy(&out.stderr))));
        }
        runs.push(entry);
    }
    anyhow::ensure!(
        std::fs::read(root.join("Cargo.lock"))? == lock_before,
        "cargo changed the lock file of `{name}`: write what it wrote"
    );

    // One run to a line: the graphs are large and the file is read by diffs.
    let mut text = String::new();
    writeln!(text, "{{")?;
    writeln!(text, " \"cargo\": {},", serde_json::to_string(version)?)?;
    writeln!(text, " \"host\": {},", serde_json::to_string(host)?)?;
    writeln!(text, " \"cfg\": {},", serde_json::to_string(cfg)?)?;
    writeln!(text, " \"runs\": [")?;
    for (at, run) in runs.iter().enumerate() {
        let comma = if at + 1 == runs.len() { "" } else { "," };
        writeln!(text, "  {}{comma}", serde_json::to_string(run)?)?;
    }
    writeln!(text, " ]\n}}")?;
    std::fs::write(dir.join("expected.json"), text)?;
    let failed = runs.iter().filter(|it| it["code"] != 0).count();
    println!("{name}: {} runs, {failed} refused by cargo", runs.len());
    let _ = std::fs::remove_dir_all(&scratch);
    Ok(())
}

fn main() -> Result<()> {
    let fixtures = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/units");
    let mut asked: Vec<String> = std::env::args().skip(1).collect();
    if asked.is_empty() {
        for entry in std::fs::read_dir(&fixtures)? {
            let entry = entry?;
            if entry.file_type()?.is_dir() {
                asked.push(entry.file_name().to_string_lossy().into_owned());
            }
        }
        asked.sort();
    }
    let verbose = output(Command::new("rustc").arg(TOOLCHAIN).arg("-vV"))?;
    let host = verbose
        .lines()
        .find_map(|l| l.strip_prefix("host: "))
        .context("`rustc -vV` names no host")?;
    let printed = output(Command::new("rustc").arg(TOOLCHAIN).args(["--print", "cfg"]))?;
    let cfg: Vec<&str> = printed.lines().filter(|it| !it.is_empty()).collect();
    let version = output(Command::new("cargo").arg(TOOLCHAIN).arg("--version"))?;
    if !version.starts_with("cargo 1.99.0") {
        bail!("expected cargo 1.99.0, got {version}");
    }
    for name in &asked {
        fixture(&fixtures.join(name), &json!(host), &json!(cfg), version.trim())?;
    }
    Ok(())
}
