//! The questions cargo, rust-analyzer and ra-jit ask rustc, answered by ra-jit: `-vV`,
//! `--print cfg`, `sysroot`, `target-libdir`, `target-spec-json`, the names of the files of
//! each kind of crate, and the rest of what a release of rustc says the same way every time
//! it is asked. ra-jit answers from what it knows of the release and its targets ([`own`]),
//! running no rustc. A question it does not know the answer of is asked of rustc once per
//! toolchain and the answer is kept, byte for byte; every later asker, the daemon's workers
//! and spares among them, is given that.
//!
//! cargo asks through its wrapper, which is ra-jit's in the builds of `cargo ra-jit` and in
//! the cargos ra-jit runs only to ask; rust-analyzer asks through [`ask`], which ra-jit hands
//! it; ra-jit itself through [`answer`].

use std::{
    ffi::{OsStr, OsString},
    io::{self, Read as _, Write as _},
    path::{Path, PathBuf},
    process::{Command, ExitStatus, Output, Stdio},
};

use sha2::{Digest as _, Sha256};

mod own;
mod probe;

pub(crate) use own::target_cfg;

/// Build-script probes name this launcher rather than a compiler outside blaze.
/// Unknown compile operations fail visibly; no wrapper fallback is reachable.
pub(crate) fn producer_launcher(path: &Path) -> io::Result<()> {
    let exe = std::env::current_exe()?;
    #[cfg(unix)]
    {
        use std::os::unix::{ffi::OsStrExt as _, fs::PermissionsExt as _};
        let mut script = b"#!/bin/sh\nexec '".to_vec();
        for &byte in exe.as_os_str().as_bytes() {
            if byte == b'\'' {
                script.extend_from_slice(b"'\\''");
            } else {
                script.push(byte);
            }
        }
        script.extend_from_slice(b"' producer-rustc \"$@\"\n");
        std::fs::write(path, script)?;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))?;
        Ok(())
    }
    #[cfg(not(unix))]
    {
        let _ = (path, exe);
        Err(io::Error::other("the owned producer probe launcher is not implemented on this host"))
    }
}

pub(crate) fn producer_probe(args: Vec<OsString>) -> anyhow::Result<()> {
    let sysroot = std::env::var_os("RUSTUP_TOOLCHAIN")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .ok_or_else(|| anyhow::anyhow!("the producer probe needs its absolute source toolchain"))?;
    let program = sysroot.join("bin").join(format!("rustc{}", std::env::consts::EXE_SUFFIX));
    if !is_question(&args) {
        match probe::check(&sysroot, &args) {
            Ok(true) => return Ok(()),
            Ok(false) => std::process::exit(1),
            Err(error) => {
                // Unknown operations are not proof that a language feature is absent.
                // A script swallowing that failure cannot publish incomplete output.
                if let Some(path) = std::env::var_os("BLAZE_PRODUCER_PROBE_FAILURE") {
                    std::fs::write(path, format!("{args:?}: {error:#}"))?;
                }
                anyhow::bail!("blaze could not check this producer's compiler probe: {error:#}");
            }
        }
    }
    let var = |name: &str| std::env::var_os(name);
    let stdout = own::answer(&program, &args, &[], &var)
        .ok_or_else(|| anyhow::anyhow!("blaze has no owned answer to the producer probe"))?;
    io::stdout().write_all(&stdout)?;
    Ok(())
}

/// What rustc can be asked to print without compiling, whatever the input is.
const ABOUT_RUSTC: [&str; 20] = [
    "all-target-specs-json",
    "backend-has-zstd",
    "calling-conventions",
    "cfg",
    "check-cfg",
    "code-models",
    "deployment-target",
    "host-tuple",
    "relocation-models",
    "split-debuginfo",
    "stack-protector-strategies",
    "supported-crate-types",
    "sysroot",
    "target-cpus",
    "target-features",
    "target-libdir",
    "target-list",
    "target-spec-json",
    "target-spec-json-schema",
    "tls-models",
];

/// The flags of rustc that take a value, beyond those a crate's build is given.
const ALSO_VALUED: [&str; 9] = [
    "-o",
    "--sysroot",
    "--allow",
    "--warn",
    "--deny",
    "--forbid",
    "--force-warn",
    "--codegen",
    "--env-set",
];

/// What rustc prints of its input without compiling it: asked of standard input only, which
/// is part of the question.
const ABOUT_INPUT: [&str; 2] = ["file-names", "crate-name"];

/// The variables that change what rustc answers.
const READ_BY_RUSTC: [&str; 9] = [
    "RUSTC_BOOTSTRAP",
    "RUSTC_OVERRIDE_VERSION_STRING",
    "RUST_TARGET_PATH",
    "SDKROOT",
    "MACOSX_DEPLOYMENT_TARGET",
    "IPHONEOS_DEPLOYMENT_TARGET",
    "TVOS_DEPLOYMENT_TARGET",
    "WATCHOS_DEPLOYMENT_TARGET",
    "XROS_DEPLOYMENT_TARGET",
];

/// The variable that names the wrapper cargo runs every rustc through.
const WRAPPER: &str = "RUSTC_WRAPPER";

/// The variable that tells ra-jit as cargo's wrapper only to answer questions, and to leave
/// what cargo compiles as it would be without ra-jit.
pub(crate) const ONLY_ASKS: &str = "RA_JIT_ONLY_ASKS";

/// The variable that says which answers are given: `0` none, so rustc is asked every time;
/// `own` only ra-jit's own, so a question it has no answer of fails.
const OFF: &str = "RA_JIT_ANSWERS";

/// Whether rustc run with `args` compiles nothing and says only what its release, the
/// arguments and standard input decide.
pub(crate) fn is_question(args: &[OsString]) -> bool {
    let Some(args) = args.iter().map(|it| it.to_str()).collect::<Option<Vec<_>>>() else {
        return false;
    };
    if args.iter().any(|it| it.starts_with('@')) {
        return false;
    }
    if args.iter().any(|it| matches!(*it, "-V" | "-vV" | "--version")) {
        return true;
    }
    let mut kinds = Vec::new();
    let mut from_stdin = false;
    let mut inputs = 0;
    let mut words = args.iter().copied();
    while let Some(word) = words.next() {
        let (flag, joined) = match word.split_once('=') {
            Some((flag, value)) if flag.starts_with("--") => (flag, Some(value)),
            _ => (word, None),
        };
        let mut value = || joined.or_else(|| words.next());
        match flag {
            "--print" => match value() {
                // A print to a file writes it.
                Some(kind) if !kind.contains('=') => kinds.push(kind),
                _ => return false,
            },
            "--target" => match value() {
                // A target of the user's own is a file that can change.
                Some(target) if !target.ends_with(".json") => {}
                _ => return false,
            },
            "-" => from_stdin = true,
            _ if !flag.starts_with('-') => inputs += 1,
            _ if joined.is_none()
                && (crate::stub::VALUED.contains(&flag) || ALSO_VALUED.contains(&flag)) =>
            {
                words.next();
            }
            _ => {}
        }
    }
    let about_rustc = |kind: &&str| ABOUT_RUSTC.contains(kind);
    let about_input = |kind: &&str| ABOUT_INPUT.contains(kind);
    !kinds.is_empty()
        && kinds.iter().all(|it| about_rustc(it) || about_input(it))
        && (kinds.iter().all(about_rustc) || (from_stdin && inputs == 0))
}

/// rustc's answer to what `command` asks it, given `stdin`: ra-jit's own, the kept answer, or
/// rustc's now, kept when it is one. `command` runs rustc and asks a question ([`is_question`]).
pub(crate) fn answer(command: &mut Command, stdin: &[u8]) -> io::Result<Output> {
    let given = std::env::var_os(OFF);
    if given.as_ref().is_none_or(|it| it != "0") {
        let var = |name: &str| var_of(command, name);
        let args: Vec<OsString> = command.get_args().map(OsStr::to_owned).collect();
        let rustc = toolchain_rustc(command.get_program(), &var);
        if let Some(stdout) = rustc.and_then(|rustc| own::answer(&rustc, &args, stdin, &var)) {
            return Ok(Output { status: ExitStatus::default(), stdout, stderr: Vec::new() });
        }
    }
    if given.is_some_and(|it| it == "own") {
        return Err(io::Error::other("ra-jit has no answer of its own to this question"));
    }
    let key = key(command, stdin);
    let file = key.as_ref().and_then(|key| Some(dir()?.join(key)));
    if let Some(stdout) = file.as_deref().and_then(kept) {
        return Ok(Output { status: ExitStatus::default(), stdout, stderr: Vec::new() });
    }
    // The workers and spares of one load ask the same questions at once: one asks rustc, and
    // the others wait for its answer.
    let _held = file.as_deref().and_then(hold);
    if let Some(stdout) = file.as_deref().and_then(kept) {
        return Ok(Output { status: ExitStatus::default(), stdout, stderr: Vec::new() });
    }
    let mut child =
        command.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn()?;
    if let Some(mut input) = child.stdin.take() {
        // rustc may answer without reading what it was given.
        _ = input.write_all(stdin);
    }
    let output = child.wait_with_output()?;
    // A question rustc did not answer is asked again, and what it warns of is said each time.
    if let Some(file) = file
        && output.status.success()
        && output.stderr.is_empty()
    {
        keep(&file, &output.stdout);
    }
    Ok(output)
}

/// What rust-analyzer runs to learn of the toolchain, answered here when it asks rustc.
/// A cargo it runs only to ask is given ra-jit as the workspace's wrapper, which answers
/// what cargo asks rustc in turn.
pub(crate) fn ask(command: &mut Command) -> Option<io::Result<Output>> {
    let program = Path::new(command.get_program()).file_stem()?.to_str()?.to_owned();
    match program.as_str() {
        "cargo" => {
            if let Some(answer) = cargo_question(command) {
                return Some(answer);
            }
            ready_cargo(command);
            None
        }
        "rustc" => {
            let args: Vec<OsString> = command.get_args().map(OsStr::to_owned).collect();
            is_question(&args).then(|| answer(command, &[]))
        }
        _ => None,
    }
}

/// Cargo's query-only front door. Answer with the owned configuration reader and
/// toolchain records; never start Cargo merely to have it ask our rustc stand-in.
fn cargo_question(command: &mut Command) -> Option<io::Result<Output>> {
    let args: Vec<String> =
        command.get_args().map(|arg| arg.to_str().map(str::to_owned)).collect::<Option<_>>()?;
    let var = |name: &str| var_of(command, name);
    let mut env: std::collections::BTreeMap<String, String> = crate::process::vars().collect();
    for (name, value) in command.get_envs() {
        let name = name.to_str()?.to_owned();
        match value {
            Some(value) => env.insert(name, value.to_str()?.to_owned()),
            None => env.remove(&name),
        };
    }
    let cwd =
        command.get_current_dir().map(Path::to_owned).or_else(|| std::env::current_dir().ok())?;
    let output = |stdout: Vec<u8>, stderr: Vec<u8>, code: u8| {
        #[cfg(unix)]
        let status = {
            use std::os::unix::process::ExitStatusExt;
            ExitStatus::from_raw(i32::from(code) << 8)
        };
        #[cfg(windows)]
        let status = {
            use std::os::windows::process::ExitStatusExt;
            ExitStatus::from_raw(u32::from(code))
        };
        Output { status, stdout, stderr }
    };
    if args.windows(2).any(|args| args == ["config", "get"]) {
        let mut normalized = Vec::new();
        let mut at = 0;
        while at < args.len() {
            if args[at] == "-Z" && args.get(at + 1).is_some_and(|it| it == "unstable-options") {
                normalized.push("-Zunstable-options".to_owned());
                at += 2;
            } else {
                normalized.push(args[at].clone());
                at += 1;
            }
        }
        let (code, said) = blaze_manifest::config::get::main(&cwd, env, &normalized);
        return Some(Ok(output(said.stdout.into_bytes(), said.stderr.into_bytes(), code)));
    }
    let rustc = toolchain_rustc(command.get_program(), &var)?;
    // A resolved Cargo path points into the same installation, but it is not rustc.
    let rustc = rustc.with_file_name(format!("rustc{}", std::env::consts::EXE_SUFFIX));
    if args == ["--version"] {
        return own::cargo_version(&rustc).map(|stdout| Ok(output(stdout, Vec::new(), 0)));
    }
    if args.first().is_some_and(|it| it == "rustc")
        && args.iter().any(|it| it == "--print" || it.starts_with("--print="))
    {
        let mut rustc_args = Vec::new();
        let mut at = 1;
        while at < args.len() {
            let arg = &args[at];
            at += 1;
            if arg == "--" {
                rustc_args.extend(args[at..].iter().map(OsString::from));
                break;
            }
            if arg == "-Z" && args.get(at).is_some_and(|it| it == "unstable-options") {
                at += 1;
                continue;
            }
            if matches!(arg.as_str(), "--print" | "--target") {
                rustc_args.push(OsString::from(arg));
                rustc_args.push(OsString::from(args.get(at)?));
                at += 1;
            } else if arg.starts_with("--print=") || arg.starts_with("--target=") {
                rustc_args.push(OsString::from(arg));
            } else {
                return None;
            }
        }
        if !is_question(&rustc_args) {
            return None;
        }
        if !own::flag_sensitive(&rustc_args) {
            // These answers do not depend on Cargo's CPU, features or relocation
            // choices. Query-tail arguments that do affect a name remain in argv.
            return Some(match own::answer(&rustc, &rustc_args, &[], &var) {
                Some(stdout) => Ok(output(stdout, Vec::new(), 0)),
                None => Err(io::Error::other(format!(
                    "owned toolchain print is not modeled: {rustc_args:?}; Cargo was not executed"
                ))),
            });
        }
        // Cargo appends its effective flags before the rustc-only tail. In particular,
        // --cfg participates in both program parsing and platform dependency resolution.
        let explicit_target = rustc_args
            .windows(2)
            .find(|args| args[0] == "--target")
            .and_then(|args| args[1].to_str())
            .or_else(|| rustc_args.iter().find_map(|arg| arg.to_str()?.strip_prefix("--target=")));
        let target = explicit_target.map(str::to_owned).or_else(|| own::host_tuple(&rustc))?;
        let builtin = own::target_cfg(rustc.parent()?.parent()?, &target, &env)?;
        let cfg = builtin
            .iter()
            .map(|cfg| match cfg.split_once('=') {
                Some((key, value)) => cfg::CfgAtom::KeyValue {
                    key: intern::Symbol::intern(key),
                    value: intern::Symbol::intern(value.trim_matches('"')),
                },
                None => cfg::CfgAtom::Flag(intern::Symbol::intern(cfg)),
            })
            .collect::<Vec<_>>();
        let configured = || -> anyhow::Result<Vec<String>> {
            let mut config = blaze_manifest::Config::load(&cwd, env.clone(), &[])?;
            config.start("rustc", &[])?;
            crate::scripts::rustflags(&config, &target, &cfg)
        };
        let flags = match configured() {
            Ok(flags) => flags,
            Err(error) => return Some(Err(io::Error::other(format!("{error:#}")))),
        };
        let mut effective = flags.into_iter().map(OsString::from).collect::<Vec<_>>();
        effective.extend(rustc_args);
        return Some(match own::answer(&rustc, &effective, &[], &var) {
            Some(stdout) => Ok(output(stdout, Vec::new(), 0)),
            None => Err(io::Error::other(format!(
                "owned toolchain query has no model for its effective flags: {effective:?}; \
                 Cargo was not executed"
            ))),
        });
    }
    None
}

/// Has a cargo ask rustc through ra-jit, unless the user named a wrapper of their own: one
/// that only asks (`cargo metadata`, `cargo rustc --print`, `--unit-graph`), and one whose
/// build is not ra-jit's (rust-analyzer's `cargo check` of build scripts, `cargo ra-jit
/// check`), which compiles as it would without ra-jit. It is `RUSTC_WRAPPER`, not the
/// workspace's: cargo runs the print of `cargo rustc --print` through that one alone.
pub(crate) fn ready_cargo(command: &mut Command) {
    let named = [WRAPPER, "CARGO_BUILD_RUSTC_WRAPPER"].iter().any(|name| {
        command.get_envs().any(|(it, value)| it == *name && value.is_some())
            || std::env::var_os(name).is_some_and(|it| !it.is_empty())
    });
    let wrapper = crate::cargo::switch::wrapper_path().ok().filter(|it| it.is_file());
    if let Some(wrapper) = wrapper
        && !named
    {
        command.env(WRAPPER, wrapper).env(ONLY_ASKS, "1");
    }
}

/// Answers the question cargo asked the wrapper, and ends as rustc would have. `None` when
/// it is no question.
pub(crate) fn reply(rustc: &OsStr, args: &[OsString]) -> Option<io::Result<i32>> {
    if !is_question(args) {
        return None;
    }
    let mut stdin = Vec::new();
    if args.iter().any(|it| it == "-")
        && let Err(e) = io::stdin().read_to_end(&mut stdin)
    {
        return Some(Err(e));
    }
    let output = answer(Command::new(rustc).args(args), &stdin);
    Some(output.and_then(|output| {
        io::stdout().write_all(&output.stdout)?;
        io::stderr().write_all(&output.stderr)?;
        Ok(output.status.code().unwrap_or(1))
    }))
}

/// Where the answers are kept: in `RA_JIT_HOME` when it is set, else in the user's cache
/// directory, which outlives the session.
fn dir() -> Option<PathBuf> {
    if std::env::var_os(OFF).is_some_and(|it| it == "0") {
        return None;
    }
    if let Some(home) = std::env::var_os("RA_JIT_HOME").filter(|it| !it.is_empty()) {
        return Some(Path::new(&home).join("answers"));
    }
    let caches = std::env::var_os("XDG_CACHE_HOME").filter(|it| !it.is_empty()).map(PathBuf::from);
    let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"));
    let caches = caches.or_else(|| Some(PathBuf::from(home?).join(".cache")))?;
    Some(caches.join("ra-jit/answers"))
}

/// The name of the question: the rustc that is asked, its arguments, what it reads of its
/// environment and of standard input. `None` when which rustc runs is not known.
fn key(command: &Command, stdin: &[u8]) -> Option<String> {
    let var = |name: &str| var_of(command, name);
    let rustc = toolchain_rustc(command.get_program(), &var)?;
    let is = std::fs::metadata(&rustc).ok()?;
    let mut hasher = Sha256::new();
    hasher.update(b"ra-jit answer 1\0");
    hasher.update(rustc.as_os_str().as_encoded_bytes());
    hasher.update([0]);
    hasher.update(is.len().to_le_bytes());
    let modified = is.modified().ok()?.duration_since(std::time::UNIX_EPOCH).ok()?;
    hasher.update(modified.as_nanos().to_le_bytes());
    for arg in command.get_args() {
        hasher.update(arg.as_encoded_bytes());
        hasher.update([0]);
    }
    for name in READ_BY_RUSTC {
        hasher.update(name);
        match var(name) {
            Some(value) => {
                hasher.update([1]);
                hasher.update(value.as_encoded_bytes());
            }
            None => hasher.update([0]),
        }
    }
    hasher.update(stdin);
    Some(hasher.finalize().iter().map(|it| format!("{it:02x}")).collect())
}

/// The variable `name` as `command` will have it.
fn var_of(command: &Command, name: &str) -> Option<OsString> {
    match command.get_envs().find(|(it, _)| *it == name) {
        Some((_, value)) => value.map(OsStr::to_owned),
        None => std::env::var_os(name),
    }
}

/// The rustc of the toolchain that `program`, as this process would run it, is.
pub(crate) fn toolchain_rustc_of(program: &OsStr) -> Option<PathBuf> {
    toolchain_rustc(program, &|name| std::env::var_os(name))
}

/// The executable of the toolchain that `program` runs. rustup's proxy runs the toolchain
/// rustup set for the process it started, and is not known without that.
fn toolchain_rustc(program: &OsStr, var: &dyn Fn(&str) -> Option<OsString>) -> Option<PathBuf> {
    let found = if Path::new(program).components().count() > 1 {
        PathBuf::from(program)
    } else {
        let paths = var("PATH")?;
        let name = Path::new(program).with_extension(std::env::consts::EXE_EXTENSION);
        std::env::split_paths(&paths).map(|dir| dir.join(&name)).find(|it| it.is_file())?
    };
    let real = std::fs::canonicalize(found).ok()?;
    let proxy = real.file_stem().and_then(OsStr::to_str).is_some_and(|it| it.starts_with("rustup"));
    if !proxy {
        return Some(real);
    }
    let toolchain = PathBuf::from(var("RUSTUP_TOOLCHAIN")?);
    let dir = if toolchain.is_absolute() {
        toolchain
    } else {
        PathBuf::from(var("RUSTUP_HOME")?).join("toolchains").join(toolchain)
    };
    let rustc = dir.join("bin").join(format!("rustc{}", std::env::consts::EXE_SUFFIX));
    std::fs::canonicalize(rustc).ok()
}

fn kept(file: &Path) -> Option<Vec<u8>> {
    std::fs::read(file).ok()
}

/// The lock of the question whose answer is `file`, held until it is dropped.
fn hold(file: &Path) -> Option<std::fs::File> {
    std::fs::create_dir_all(file.parent()?).ok()?;
    let lock = std::fs::File::create(file.with_extension("lock")).ok()?;
    lock.lock().ok()?;
    Some(lock)
}

fn keep(file: &Path, stdout: &[u8]) {
    let Some(dir) = file.parent() else { return };
    if std::fs::create_dir_all(dir).is_ok() {
        // An answer not kept is asked again.
        _ = ra_jit_store::write_file_whole(file, stdout);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn question(args: &str) -> bool {
        let args: Vec<OsString> = args.split_whitespace().map(OsString::from).collect();
        is_question(&args)
    }

    #[test]
    fn what_is_a_question() {
        assert!(question("-vV"));
        assert!(question("--version"));
        assert!(question("--print sysroot"));
        assert!(question("--print=target-libdir"));
        assert!(question("--print cfg --target aarch64-apple-darwin -C opt-level=3"));
        assert!(question(
            "-Z unstable-options --print target-spec-json --target x86_64-unknown-linux-gnu"
        ));
        assert!(question(
            "- --crate-name ___ --print=file-names --crate-type bin --crate-type rlib \
             --print=sysroot --print=split-debuginfo --print=crate-name --print=cfg -Wwarnings"
        ));
        // rust-analyzer's `cargo rustc -- --print cfg` names the library, which rustc does
        // not read for that.
        assert!(question(
            "--crate-name p --edition=2024 src/lib.rs --crate-type lib --emit=dep-info,metadata,link \
             -C metadata=d4ed28d9543870f2 --out-dir target/debug/deps --print cfg"
        ));
    }

    #[test]
    fn what_is_not() {
        assert!(!question("--crate-name p src/lib.rs --crate-type lib --emit=link"));
        assert!(!question("src/lib.rs --print file-names"));
        assert!(!question("--print link-args src/main.rs"));
        assert!(!question("--print=link-args=out.txt src/main.rs"));
        assert!(!question("--print cfg --target my-target.json"));
        assert!(!question("@args.txt"));
        assert!(!question(""));
    }

    #[test]
    fn cargo_config_queries_are_answered_from_the_request_without_execution() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join(".cargo")).unwrap();
        std::fs::write(
            dir.path().join(".cargo/config.toml"),
            "[build]\nrustflags = [\"--cfg\", \"from_file\"]\n",
        )
        .unwrap();
        // A deliberately nonexistent executable: this query must be answered in-process.
        let executable = dir.path().join("absent-query-tool");
        let mut command = Command::new(&executable);
        command.current_dir(dir.path()).args([
            "-Z",
            "unstable-options",
            "config",
            "get",
            "--format",
            "json-value",
            "build.rustflags",
        ]);
        let result = cargo_question(&mut command).expect("owned config query").unwrap();
        assert!(result.status.success());
        let got: serde_json::Value = serde_json::from_slice(&result.stdout).unwrap();
        assert_eq!(got, serde_json::json!(["--cfg", "from_file"]));
        command.env("CARGO_BUILD_RUSTFLAGS", "--cfg request");
        let result = cargo_question(&mut command).unwrap().unwrap();
        assert!(result.status.success());
        assert_ne!(result.stdout, b"");
    }

    #[test]
    fn independent_cargo_prints_stay_owned_with_unmodeled_codegen_flags() {
        let dir = tempfile::tempdir().unwrap();
        let toolchain = dir.path();
        std::fs::create_dir_all(toolchain.join("bin")).unwrap();
        let rustlib = toolchain.join("lib/rustlib");
        std::fs::create_dir_all(&rustlib).unwrap();
        std::fs::write(toolchain.join("lib/librustc_driver-test.so"), "").unwrap();
        std::fs::write(rustlib.join("manifest-rustc-aarch64-unknown-linux-gnu"), "").unwrap();
        let release: serde_json::Value =
            serde_json::from_str(include_str!("answers/rustc-1.99.0.json")).unwrap();
        std::fs::write(
            rustlib.join("multirust-channel-manifest.toml"),
            format!("\n[pkg.rustc]\nversion = {:?}\n", release["version"].as_str().unwrap()),
        )
        .unwrap();
        let executable = toolchain.join("bin/cargo");
        std::fs::write(&executable, "this tool must not be executed").unwrap();
        exercise_independent_prints(&executable, toolchain);
        // The targeted ownership harness supplies an armed norustc stand-in tree.
        // The synthetic non-executable control above always runs as well.
        if let Some(path) = std::env::var_os("RA_JIT_TEST_QUERY_TOOLCHAIN") {
            let path = PathBuf::from(path);
            exercise_independent_prints(&path.join("bin/cargo"), &path);
        }
    }

    fn exercise_independent_prints(executable: &Path, toolchain: &Path) {
        for kind in ["target-libdir", "sysroot", "target-spec-json", "file-names"] {
            let mut command = Command::new(executable);
            command.current_dir(toolchain).args([
                "rustc",
                "-Z",
                "unstable-options",
                "--print",
                kind,
            ]);
            if kind == "file-names" {
                command.args(["--", "-", "--crate-name", "___", "--crate-type", "rlib"]);
            } else if kind == "target-spec-json" {
                command.args(["--", "-Z", "unstable-options"]);
            }
            command.env("RUSTFLAGS", "-Ctarget-cpu=native -Crelocation-model=pic");
            command.env("RUSTC_BOOTSTRAP", "1");
            for key in [
                "SDKROOT",
                "MACOSX_DEPLOYMENT_TARGET",
                "RUST_TARGET_PATH",
                "RUSTC_OVERRIDE_VERSION_STRING",
            ] {
                command.env_remove(key);
            }
            let answer = ask(&mut command).expect("must not hand the query to Cargo").unwrap();
            assert!(answer.status.success() && !answer.stdout.is_empty(), "{kind}");
        }
        let mut command = Command::new(executable);
        command.current_dir(toolchain).args(["rustc", "--print", "cfg", "--", "-O"]);
        command.env("RUSTFLAGS", "-Ctarget-cpu=native");
        command.env("RUSTC_BOOTSTRAP", "1");
        let error = ask(&mut command).expect("cfg gap must not hand off to Cargo").unwrap_err();
        assert!(error.to_string().contains("Cargo was not executed"), "{error}");
    }
}
