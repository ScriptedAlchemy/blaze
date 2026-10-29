//! The questions cargo, rust-analyzer and ra-jit ask rustc, answered by ra-jit: `-vV`,
//! `--print cfg`, `sysroot`, `target-libdir`, `target-spec-json`, the names of the files of
//! each kind of crate, and the rest of what a release of rustc says the same way every time
//! it is asked. ra-jit answers from what it knows of the release and its targets ([`own`]),
//! running no rustc. An unmodelled question fails with a named gap. It never executes the
//! compiler or reads a cached answer produced by it.
//!
//! cargo asks through its wrapper, which is ra-jit's in the builds of `cargo ra-jit` and in
//! the cargos ra-jit runs only to ask; rust-analyzer asks through [`ask`], which ra-jit hands
//! it; ra-jit itself through [`answer`].

use std::{
    ffi::{OsStr, OsString},
    io::{self, Read as _, Write as _},
    path::{Path, PathBuf},
    process::{Command, ExitStatus, Output},
};

mod own;
mod probe;

pub(crate) use own::{host_tuple, target_cfg};

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

/// The variable that names the wrapper cargo runs every rustc through.
const WRAPPER: &str = "RUSTC_WRAPPER";

/// The variable that tells ra-jit as cargo's wrapper only to answer questions, and to leave
/// what cargo compiles as it would be without ra-jit.
pub(crate) const ONLY_ASKS: &str = "RA_JIT_ONLY_ASKS";

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

/// Answer from owned release and target facts, without executing `command`. There is no
/// environment switch or cached compiler answer that can turn this into a subprocess.
pub(crate) fn answer(command: &mut Command, stdin: &[u8]) -> io::Result<Output> {
    let var = |name: &str| var_of(command, name);
    let args: Vec<OsString> = command.get_args().map(OsStr::to_owned).collect();
    let rustc = toolchain_rustc(command.get_program(), &var);
    if let Some(error) = rustc.as_ref().and_then(|it| own::deployment_error(it, &args, &var)) {
        #[cfg(unix)]
        let status = {
            use std::os::unix::process::ExitStatusExt as _;
            ExitStatus::from_raw(1 << 8)
        };
        #[cfg(windows)]
        let status = {
            use std::os::windows::process::ExitStatusExt as _;
            ExitStatus::from_raw(1)
        };
        // rustc reports target-construction errors but still emits requested prints
        // from the target's default options before ending with status 1.
        let defaults = |name: &str| {
            if name == "MACOSX_DEPLOYMENT_TARGET" { None } else { var(name) }
        };
        let stdout = rustc
            .as_ref()
            .and_then(|it| own::answer(it, &args, stdin, &defaults))
            .ok_or_else(|| io::Error::other("unmodelled print after deployment target error"))?;
        return Ok(Output { status, stdout, stderr: error.into_bytes() });
    }
    let stdout = rustc.as_ref().and_then(|rustc| own::answer(rustc, &args, stdin, &var));
    match stdout {
        Some(stdout) => Ok(Output { status: ExitStatus::default(), stdout, stderr: Vec::new() }),
        None => Err(io::Error::other(format!(
            "owned compiler query has no model: {:?} {args:?} (toolchain {rustc:?}); \
             rustc was not executed; Cargo was not executed",
            command.get_program()
        ))),
    }
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
    let Some(rustc) = toolchain_rustc(command.get_program(), &var) else {
        return Some(Err(io::Error::other(format!(
            "owned Cargo query has no toolchain model: {args:?}; Cargo was not executed"
        ))));
    };
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
        // Cargo appends its effective flags before the rustc-only tail. In particular,
        // --cfg participates in both program parsing and platform dependency resolution.
        let explicit_target = rustc_args
            .windows(2)
            .find(|args| args[0] == "--target")
            .and_then(|args| args[1].to_str())
            .or_else(|| rustc_args.iter().find_map(|arg| arg.to_str()?.strip_prefix("--target=")));
        // Past this point the query is ours: a missing model is a named gap, never Cargo's run.
        let gap = |what: &str| {
            Some(Err(io::Error::other(format!(
                "owned toolchain query has no model for {what}: {rustc_args:?}; Cargo was not executed"
            ))))
        };
        let Some(target) = explicit_target.map(str::to_owned).or_else(|| own::host_tuple(&rustc))
        else {
            return gap("the host target");
        };
        let Some(sysroot) = rustc.parent().and_then(Path::parent) else {
            return gap("the toolchain's sysroot");
        };
        let Some(builtin) = own::target_cfg(sysroot, &target, &env) else {
            return gap(&format!("target {target}'s built-in cfg"));
        };
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
                 Cargo was not executed (docs/no-rustc.md#unmodeled-cpu-cfg-queries)"
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
        // rustup does not export RUSTUP_HOME for its default installation. Use the
        // command's home, just as toolchain selection does, so worker queries see
        // the same installed compiler without starting a rustup proxy.
        let home = var("RUSTUP_HOME").map(PathBuf::from).or_else(|| {
            var(if cfg!(windows) { "USERPROFILE" } else { "HOME" })
                .map(|home| PathBuf::from(home).join(".rustup"))
        })?;
        home.join("toolchains").join(toolchain)
    };
    let rustc = dir.join("bin").join(format!("rustc{}", std::env::consts::EXE_SUFFIX));
    std::fs::canonicalize(rustc).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(super) fn synthetic_toolchain(dir: &Path) -> PathBuf {
        std::fs::create_dir_all(dir.join("bin")).unwrap();
        let rustlib = dir.join("lib/rustlib");
        std::fs::create_dir_all(&rustlib).unwrap();
        std::fs::write(dir.join("lib/librustc_driver-test.so"), "").unwrap();
        std::fs::write(rustlib.join("manifest-rustc-aarch64-apple-darwin"), "").unwrap();
        let release: serde_json::Value =
            serde_json::from_str(include_str!("answers/rustc-1.99.0.json")).unwrap();
        std::fs::write(
            rustlib.join("multirust-channel-manifest.toml"),
            format!("\n[pkg.rustc]\nversion = {:?}\n", release["version"].as_str().unwrap()),
        )
        .unwrap();
        let rustc = dir.join("bin/rustc");
        std::fs::write(&rustc, "this compiler must not execute").unwrap();
        rustc
    }

    #[cfg(unix)]
    #[test]
    fn rustup_queries_resolve_the_default_home_without_executing_the_proxy() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        let toolchain = "1.99.0-aarch64-apple-darwin";
        let default_root = home.join(".rustup/toolchains").join(toolchain);
        let custom_home = dir.path().join("custom-rustup");
        let custom_root = custom_home.join("toolchains").join(toolchain);
        synthetic_toolchain(&default_root);
        synthetic_toolchain(&custom_root);
        let bin = home.join(".cargo/bin");
        std::fs::create_dir_all(&bin).unwrap();
        let proxy = bin.join("rustup");
        std::fs::write(&proxy, "this rustup proxy must not execute").unwrap();
        let name = OsStr::new("rustc");
        let executable = bin.join(name);
        std::os::unix::fs::symlink(&proxy, &executable).unwrap();

        for rustup_home in [None, Some(&custom_home)] {
            let expected = if rustup_home.is_some() { &custom_root } else { &default_root };
            for question in ["--print=sysroot", "-vV"] {
                let mut command = Command::new(name);
                command
                    .env_remove("RUSTUP_HOME")
                    .env_remove("RUSTC_OVERRIDE_VERSION_STRING")
                    .env_remove("RUST_TARGET_PATH")
                    .env("HOME", &home)
                    .env("PATH", &bin)
                    .env("RUSTUP_TOOLCHAIN", toolchain)
                    .arg(question);
                if let Some(home) = rustup_home {
                    command.env("RUSTUP_HOME", home);
                }
                let output = answer(&mut command, &[]).unwrap();
                assert!(output.status.success());
                let text = String::from_utf8(output.stdout).unwrap();
                if question == "--print=sysroot" {
                    assert_eq!(text.trim(), expected.canonicalize().unwrap().to_str().unwrap());
                } else {
                    assert!(text.contains("host: aarch64-apple-darwin"), "{text}");
                }
            }
        }

        let mut command = Command::new(&executable);
        command
            .env("HOME", &home)
            .env("RUSTUP_HOME", dir.path().join("missing-rustup"))
            .env("RUSTUP_TOOLCHAIN", toolchain)
            .arg("--print=sysroot");
        assert!(answer(&mut command, &[]).is_err());
        command.env("RUSTUP_TOOLCHAIN", &custom_root);
        let output = answer(&mut command, &[]).unwrap();
        assert_eq!(
            String::from_utf8(output.stdout).unwrap().trim(),
            custom_root.canonicalize().unwrap().to_str().unwrap()
        );
    }

    #[test]
    fn compiler_queries_never_execute_or_use_cached_external_answers() {
        let dir = tempfile::tempdir().unwrap();
        let compiler = synthetic_toolchain(dir.path());
        let armed = std::env::var_os("RA_JIT_TEST_QUERY_TOOLCHAIN")
            .map(PathBuf::from)
            .map(|path| path.join("bin/rustc"));
        for compiler in std::iter::once(compiler).chain(armed) {
            for args in [
                vec!["--version"],
                vec!["--print=sysroot"],
                vec!["--print=target-libdir"],
                vec!["-vV"],
                vec!["--print", "cfg", "-O", "--target", "aarch64-apple-darwin"],
                vec!["-Z", "unstable-options", "--print", "target-spec-json"],
                vec![
                    "-",
                    "--crate-name",
                    "___",
                    "--print=file-names",
                    "--crate-type",
                    "rlib",
                    "--print=sysroot",
                    "--print=split-debuginfo",
                    "--print=crate-name",
                    "--print=cfg",
                ],
            ] {
                let mut command = Command::new(&compiler);
                command.args(&args).env("RUSTC_BOOTSTRAP", "1");
                command.env("RA_JIT_ANSWERS", "0");
                for name in [
                    "SDKROOT",
                    "MACOSX_DEPLOYMENT_TARGET",
                    "IPHONEOS_DEPLOYMENT_TARGET",
                    "TVOS_DEPLOYMENT_TARGET",
                    "WATCHOS_DEPLOYMENT_TARGET",
                    "XROS_DEPLOYMENT_TARGET",
                    "RUST_TARGET_PATH",
                    "RUSTC_OVERRIDE_VERSION_STRING",
                ] {
                    command.env_remove(name);
                }
                command.env("SDKROOT", "/deliberately/nonexistent/sdk");
                command.env("MACOSX_DEPLOYMENT_TARGET", "11.0");
                let output = answer(&mut command, &[])
                    .unwrap_or_else(|error| panic!("{compiler:?} {args:?}: {error}"));
                assert!(output.status.success() && !output.stdout.is_empty());
                command.args(["-Ctarget-cpu=native", "--print=cfg"]);
                let error = answer(&mut command, &[]).unwrap_err();
                assert!(error.to_string().contains("Cargo was not executed"));
            }
        }
        let mut command = Command::new(dir.path().join("nonexistent-rustc"));
        command.arg("--version");
        assert!(
            answer(&mut command, &[]).unwrap_err().to_string().contains("Cargo was not executed")
        );
    }

    #[test]
    fn deployment_versions_preserve_queries_and_invalid_versions_fail_without_values() {
        let dir = tempfile::tempdir().unwrap();
        let compiler = synthetic_toolchain(dir.path());
        for version in ["11.0", "14.2.1", ""] {
            let mut command = Command::new(&compiler);
            command.args(["--version"]).env("MACOSX_DEPLOYMENT_TARGET", version);
            assert!(answer(&mut command, &[]).unwrap().status.success());
            let mut command = Command::new(&compiler);
            command
                .args(["--print=cfg", "--target=aarch64-apple-darwin"])
                .env("MACOSX_DEPLOYMENT_TARGET", version);
            let output = answer(&mut command, &[]).unwrap();
            assert_eq!(output.status.success(), !version.is_empty());
        }
        let mut command = Command::new(&compiler);
        command
            .args(["--print=sysroot", "--target=aarch64-apple-darwin"])
            .env("MACOSX_DEPLOYMENT_TARGET", "not-a-real-secret");
        let output = answer(&mut command, &[]).unwrap();
        let stderr = String::from_utf8(output.stderr).unwrap();
        assert!(!output.status.success() && stderr.contains("invalid digit"));
        assert!(!stderr.contains("not-a-real-secret"));
    }

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
        // `cargo test` hands its children the toolchain's environment (SDKROOT on macOS),
        // which the owned answers decline to guess for a stand-in toolchain.
        let query = || {
            let mut command = Command::new(executable);
            command.current_dir(toolchain);
            for key in [
                "SDKROOT",
                "MACOSX_DEPLOYMENT_TARGET",
                "RUST_TARGET_PATH",
                "RUSTC_OVERRIDE_VERSION_STRING",
            ] {
                command.env_remove(key);
            }
            command
        };
        for kind in ["target-libdir", "sysroot", "target-spec-json", "file-names"] {
            let mut command = query();
            command.args(["rustc", "-Z", "unstable-options", "--print", kind]);
            if kind == "file-names" {
                command.args(["--", "-", "--crate-name", "___", "--crate-type", "rlib"]);
            } else if kind == "target-spec-json" {
                command.args(["--", "-Z", "unstable-options"]);
            }
            command.env("RUSTFLAGS", "-Ctarget-cpu=native -Crelocation-model=pic");
            command.env("RUSTC_BOOTSTRAP", "1");
            let answer = ask(&mut command).expect("must not hand the query to Cargo").unwrap();
            assert!(answer.status.success() && !answer.stdout.is_empty(), "{kind}");
        }
        for kind in ["sysroot", "target-libdir"] {
            let mut command = query();
            command.args(["rustc", "--print", kind]);
            command.env("RUSTFLAGS", "--sysroot /elsewhere -Ctarget-cpu=native");
            let answer = ask(&mut command).expect("sysroot flags stay owned").unwrap();
            let text = String::from_utf8(answer.stdout).unwrap();
            assert!(text.starts_with("/elsewhere"), "{kind}: {text}");
        }
        let mut command = query();
        command.args(["rustc", "--print", "cfg", "--", "-O"]);
        command.env("RUSTFLAGS", "-Ctarget-cpu=native");
        command.env("RUSTC_BOOTSTRAP", "1");
        let error = ask(&mut command).expect("cfg gap must not hand off to Cargo").unwrap_err();
        assert!(error.to_string().contains("Cargo was not executed"), "{error}");
        // A target or bootstrap setting the answers do not model is a named gap, not Cargo's run.
        for (target, bootstrap) in
            [(Some("bogus-unknown-none"), "1"), (None, "somecrate"), (None, "-1")]
        {
            let mut command = query();
            command.args(["rustc", "--print", "sysroot"]);
            if let Some(target) = target {
                command.args(["--target", target]);
            }
            command.env("RUSTC_BOOTSTRAP", bootstrap);
            let answer = ask(&mut command).expect("an unmodeled print must not hand off to Cargo");
            if let Err(error) = answer {
                assert!(error.to_string().contains("Cargo was not executed"), "{error}");
            }
        }
    }
}
