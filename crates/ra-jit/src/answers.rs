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

/// Only blaze's ported answer rules, with no kept answer or process fallback.
pub(crate) fn own_answer(rustc: &Path, args: &[OsString]) -> Option<Vec<u8>> {
    own::answer(rustc, args, &[], &|_| None)
}

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
    let rustc = toolchain_rustc(command.get_program(), command.get_current_dir(), &var);
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
pub(crate) fn cargo_query(command: &mut Command, rustc: &OsStr) -> io::Result<Output> {
    cargo_question_in(command, Some(rustc)).unwrap_or_else(|| {
        Err(io::Error::other("unsupported owned Cargo command; Cargo was not executed"))
    })
}

fn cargo_question(command: &mut Command) -> Option<io::Result<Output>> {
    cargo_question_in(command, None)
}

fn cargo_question_in(
    command: &mut Command,
    compiler: Option<&OsStr>,
) -> Option<io::Result<Output>> {
    let args: Vec<String> =
        command.get_args().map(|arg| arg.to_str().map(str::to_owned)).collect::<Option<_>>()?;
    let native = cargo_environment(command);
    let var = |name: &str| native.get(OsStr::new(name)).cloned();
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
    let compiler = || match compiler {
        Some(compiler) => {
            let found = toolchain_rustc(compiler, command.get_current_dir(), &var);
            found.filter(|rustc| own::host_tuple(rustc).is_some()).or_else(|| {
                // Producers name an owned probe launcher in RUSTC. Its installed source
                // toolchain remains explicit, and contains the facts rather than the launcher.
                let selected = PathBuf::from(var("RUSTUP_TOOLCHAIN")?);
                selected.is_absolute().then(|| {
                    selected.join("bin").join(format!("rustc{}", std::env::consts::EXE_SUFFIX))
                })
            })
        }
        None => toolchain_rustc(command.get_program(), command.get_current_dir(), &var)
            .map(|path| path.with_file_name(format!("rustc{}", std::env::consts::EXE_SUFFIX))),
    };
    if args == ["--version"] || args == ["-V"] {
        let version = compiler().and_then(|rustc| own::cargo_version(&rustc));
        return Some(version.map(|stdout| output(stdout, Vec::new(), 0)).ok_or_else(|| {
            io::Error::other("owned Cargo version facts unavailable; Cargo was not executed")
        }));
    }
    let env: std::collections::BTreeMap<String, String> =
        crate::process::unicode(native.iter().map(|(key, value)| (key.clone(), value.clone())))
            .collect();
    let cwd =
        command.get_current_dir().map(Path::to_owned).or_else(|| std::env::current_dir().ok())?;
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
    if args.first().is_some_and(|it| it == "metadata") {
        return Some(
            cargo_metadata(&args[1..], &cwd, env, &compiler, &var)
                .map(|stdout| output(stdout, Vec::new(), 0)),
        );
    }
    let Some(rustc) = compiler() else {
        return Some(Err(io::Error::other(format!(
            "owned Cargo query has no toolchain model: {args:?}; Cargo was not executed"
        ))));
    };
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

fn cargo_environment(command: &Command) -> std::collections::BTreeMap<OsString, OsString> {
    let mut env: std::collections::BTreeMap<_, _> = crate::process::native().vars().collect();
    for (name, value) in command.get_envs() {
        match value {
            Some(value) => env.insert(name.to_owned(), value.to_owned()),
            None => env.remove(name),
        };
    }
    env
}

fn cargo_metadata(
    args: &[String],
    cwd: &Path,
    env: std::collections::BTreeMap<String, String>,
    compiler: &dyn Fn() -> Option<PathBuf>,
    var: &dyn Fn(&str) -> Option<OsString>,
) -> io::Result<Vec<u8>> {
    let query = || -> anyhow::Result<Vec<u8>> {
        let target =
            var("CARGO_TARGET_DIR").map(|value| ("CARGO_TARGET_DIR", value)).or_else(|| {
                var("CARGO_BUILD_TARGET_DIR").map(|value| ("CARGO_BUILD_TARGET_DIR", value))
            });
        if let Some((name, value)) = target {
            anyhow::ensure!(
                value.to_str().is_some(),
                "path contains invalid UTF-8 characters in {name}"
            );
        }
        let (mut manifest, mut platform) = (None, None);
        let (mut no_deps, mut all_features, mut no_default_features) = (false, false, false);
        let (mut offline, mut locked) = (false, false);
        let (mut config, mut features) = (Vec::new(), Vec::new());
        let mut words = args.iter();
        while let Some(word) = words.next() {
            let (flag, joined) = word.split_once('=').unwrap_or((word, ""));
            let mut value = || -> anyhow::Result<String> {
                if word.contains('=') {
                    Ok(joined.to_owned())
                } else {
                    words.next().cloned().ok_or_else(|| anyhow::anyhow!("{flag} needs a value"))
                }
            };
            match flag {
                "--manifest-path" => manifest = Some(cwd.join(value()?)),
                "--filter-platform" => platform = Some(value()?),
                "--features" | "-F" => features.push(value()?),
                "--config" => config.push(value()?),
                "--format-version" => {
                    anyhow::ensure!(value()? == "1", "unsupported metadata format version")
                }
                "--no-deps" => no_deps = true,
                "--all-features" => all_features = true,
                "--no-default-features" => no_default_features = true,
                "--offline" => offline = true,
                "--locked" => locked = true,
                "--frozen" => {
                    locked = true;
                    offline = true;
                }
                "-v" | "-vv" | "--verbose" | "-q" | "--quiet" => {}
                "--color" => {
                    value()?;
                }
                _ => anyhow::bail!("unsupported owned Cargo metadata option {flag}"),
            }
        }
        let json = if no_deps {
            let mut config_reader = match var("CARGO_HOME").filter(|path| !path.is_empty()) {
                Some(home) => {
                    blaze_manifest::Config::load_with_home(cwd, env, &[], cwd.join(home))?
                }
                None => blaze_manifest::Config::load(cwd, env, &[])?,
            };
            config_reader.start("metadata", &config)?;
            let ctx = blaze_manifest::Context::new(config_reader);
            let root = blaze_manifest::metadata::root_manifest(manifest.as_deref(), &ctx)?;
            let workspace = blaze_manifest::Workspace::new(&root, &ctx)?;
            serde_json::to_string(&blaze_manifest::metadata::export_no_deps(&workspace))?
        } else {
            let cfg = platform
                .as_deref()
                .map(|target| -> anyhow::Result<Vec<cargo_platform::Cfg>> {
                    let rustc = compiler().ok_or_else(|| {
                        anyhow::anyhow!("owned Cargo metadata target facts unavailable")
                    })?;
                    let sysroot = rustc.parent().and_then(Path::parent).ok_or_else(|| {
                        anyhow::anyhow!("owned Cargo metadata sysroot unavailable")
                    })?;
                    let cfg = own::target_cfg(sysroot, target, &env).ok_or_else(|| {
                        anyhow::anyhow!("owned Cargo metadata target facts unavailable")
                    })?;
                    cfg.iter().map(|atom| atom.parse().map_err(anyhow::Error::from)).collect()
                })
                .transpose()?;
            blaze_metadata::metadata(&blaze_metadata::Asked {
                cargo_home: var("CARGO_HOME").map(PathBuf::from),
                cwd,
                env: env.into_iter().collect(),
                manifest_path: manifest.as_deref(),
                config: &config,
                features: &features,
                all_features,
                no_default_features,
                build: None,
                platform: platform
                    .as_deref()
                    .zip(cfg.as_deref())
                    .map(|(triple, cfg)| blaze_metadata::TargetPlatform { triple, cfg }),
                offline,
                locked,
                store: None,
                rust_release: Some(&rust_release),
            })?
        };
        Ok(format!("{json}\n").into_bytes())
    };
    query().map_err(|error| io::Error::other(format!("{error:#}; Cargo was not executed")))
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
/// The release of the rustc `program` names, run in `cwd` with the variables `env`, as cargo's
/// `rustc -vV` would find it, from the toolchain's records; nothing is run.
pub(crate) fn rust_release(program: &Path, cwd: &Path, env: &[(String, String)]) -> Option<String> {
    let var = |name: &str| {
        env.iter().find(|(key, _)| key == name).map(|(_, value)| OsString::from(value))
    };
    own::release(&toolchain_rustc(program.as_os_str(), Some(cwd), &var)?)
}

pub(crate) fn toolchain_rustc_of(program: &OsStr) -> Option<PathBuf> {
    toolchain_rustc(program, None, &|name| std::env::var_os(name))
}

/// Resolve rustup's installed selection from its records, without executing a proxy.
/// Direct CLI invocations do not inherit rustup's RUSTUP_HOME or expanded toolchain name.
fn toolchain_rustc(
    program: &OsStr,
    cwd: Option<&Path>,
    var: &dyn Fn(&str) -> Option<OsString>,
) -> Option<PathBuf> {
    let cwd = cwd.map(Path::to_owned).or_else(|| std::env::current_dir().ok())?;
    let found = if Path::new(program).components().count() > 1 {
        cwd.join(program)
    } else {
        let paths = var("PATH")?;
        let name = Path::new(program).with_extension(std::env::consts::EXE_EXTENSION);
        std::env::split_paths(&paths).map(|dir| dir.join(&name)).find(|it| it.is_file())?
    };
    let real = std::fs::canonicalize(&found).ok()?;
    let proxy = real.file_stem().and_then(OsStr::to_str).is_some_and(|it| it.starts_with("rustup"))
        || same_file::is_same_file(&real, found.with_file_name("rustup")).unwrap_or(false);
    if !proxy {
        return Some(real);
    }
    if let Some(selected) = var("RUSTUP_TOOLCHAIN")
        && Path::new(&selected).is_absolute()
    {
        return std::fs::canonicalize(
            PathBuf::from(selected)
                .join("bin")
                .join(format!("rustc{}", std::env::consts::EXE_SUFFIX)),
        )
        .ok();
    }
    let home = var("RUSTUP_HOME").map(PathBuf::from).or_else(|| {
        var(if cfg!(windows) { "USERPROFILE" } else { "HOME" })
            .map(|home| PathBuf::from(home).join(".rustup"))
    })?;
    let toolchain = installed_selection(&home, &cwd, var("RUSTUP_TOOLCHAIN"))?;
    let toolchain = toolchain.to_str()?;
    let dir = if Path::new(&toolchain).is_absolute() {
        PathBuf::from(toolchain)
    } else {
        let installed = home.join("toolchains");
        let exact = installed.join(&toolchain);
        if exact.is_dir() {
            exact
        } else {
            // rustup expands an unqualified channel/version using its default host.
            // The installed rustc manifest supplies the host; multiple matches are
            // ambiguous unless settings names a default host.
            let settings: toml::Value = toml::from_str(
                &std::fs::read_to_string(home.join("settings.toml")).unwrap_or_default(),
            )
            .ok()?;
            if let Some(host) = settings.get("default_host_triple").and_then(|it| it.as_str()) {
                installed.join(format!("{toolchain}-{host}"))
            } else {
                let prefix = format!("{toolchain}-");
                let mut matches =
                    std::fs::read_dir(installed).ok()?.filter_map(Result::ok).filter(|entry| {
                        let path = entry.path();
                        path.file_name().and_then(OsStr::to_str).is_some_and(|name| {
                            name.starts_with(&prefix)
                                && own::host_tuple(&path.join("bin/rustc"))
                                    .is_some_and(|host| name == format!("{prefix}{host}"))
                        })
                    });
                let selected = matches.next()?.path();
                if matches.next().is_some() {
                    return None;
                }
                selected
            }
        }
    };
    let rustc = dir.join("bin").join(format!("rustc{}", std::env::consts::EXE_SUFFIX));
    std::fs::canonicalize(rustc).ok()
}

fn installed_selection(home: &Path, cwd: &Path, env: Option<OsString>) -> Option<OsString> {
    if let Some(selected) = env.filter(|it| !it.is_empty()) {
        return Some(selected);
    }
    let settings: toml::Value =
        toml::from_str(&std::fs::read_to_string(home.join("settings.toml")).ok()?).ok()?;
    let cwd = std::fs::canonicalize(cwd).ok()?;
    for dir in cwd.ancestors() {
        if let Some(selected) = settings
            .get("overrides")
            .and_then(|it| it.get(dir.to_str()?))
            .and_then(|it| it.as_str())
        {
            return Some(selected.into());
        }
        for (file, is_toml) in [("rust-toolchain", false), ("rust-toolchain.toml", true)] {
            if let Ok(text) = std::fs::read_to_string(dir.join(file)) {
                if !is_toml && !text.trim_start().starts_with('[') {
                    return Some(text.trim().into());
                }
                let value: toml::Value = toml::from_str(&text).ok()?;
                return value.get("toolchain")?.get("channel")?.as_str().map(OsString::from);
            }
        }
    }
    settings.get("default_toolchain")?.as_str().map(OsString::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_proxy_resolves_home_short_channel_and_directory_selection() {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join(".rustup");
        let installed = home.join("toolchains/1.99.0-aarch64-apple-darwin");
        let compiler = synthetic_toolchain(&installed);
        std::fs::write(home.join("settings.toml"), "default_toolchain = '1.99.0'\n").unwrap();
        let bin = root.path().join("bin");
        std::fs::create_dir(&bin).unwrap();
        std::fs::write(bin.join("rustup"), "").unwrap();
        // rustup proxies can be hard links as well as symlinks.
        std::fs::hard_link(bin.join("rustup"), bin.join("rustc")).unwrap();
        let cwd = root.path().join("project");
        std::fs::create_dir(&cwd).unwrap();
        let vars = |name: &str| match name {
            "HOME" => Some(root.path().as_os_str().to_owned()),
            "PATH" => Some(bin.as_os_str().to_owned()),
            _ => None,
        };
        let expected = std::fs::canonicalize(compiler).unwrap();
        assert_eq!(toolchain_rustc(OsStr::new("rustc"), Some(&cwd), &vars), Some(expected.clone()));
        let selected = |name: &str| {
            if name == "RUSTUP_TOOLCHAIN" { Some("1.99.0".into()) } else { vars(name) }
        };
        assert_eq!(toolchain_rustc(OsStr::new("rustc"), Some(&cwd), &selected), Some(expected));
        std::fs::write(cwd.join("rust-toolchain.toml"), "[toolchain]\nchannel='missing'\n")
            .unwrap();
        assert!(toolchain_rustc(OsStr::new("rustc"), Some(&cwd), &vars).is_none());
        assert_eq!(installed_selection(&home, &cwd, Some("1.99.0".into())), Some("1.99.0".into()));
        let override_dir = installed.to_string_lossy();
        std::fs::write(
            home.join("settings.toml"),
            format!(
                "default_toolchain='missing'\n[overrides]\n'{}'='{}'\n",
                std::fs::canonicalize(&cwd).unwrap().display(),
                override_dir
            ),
        )
        .unwrap();
        assert_eq!(installed_selection(&home, &cwd, None), Some(installed.as_os_str().to_owned()));
    }

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

    #[test]
    fn component_toolchains_use_installed_host_and_release_facts() {
        let dir = tempfile::tempdir().unwrap();
        let rustc = synthetic_toolchain(dir.path());
        let rustlib = dir.path().join("lib/rustlib");
        std::fs::remove_file(rustlib.join("manifest-rustc-aarch64-apple-darwin")).unwrap();
        std::fs::remove_file(rustlib.join("multirust-channel-manifest.toml")).unwrap();
        let host = rustlib.join("x86_64-unknown-linux-gnu/bin");
        std::fs::create_dir_all(&host).unwrap();
        std::fs::write(host.join("rust-lld"), b"installed host linker").unwrap();
        let docs = dir.path().join("share/doc/rust/html");
        std::fs::create_dir_all(&docs).unwrap();
        let release: serde_json::Value =
            serde_json::from_str(include_str!("answers/rustc-1.99.0.json")).unwrap();
        let (version, rest) = release["version"].as_str().unwrap().split_once(" (").unwrap();
        let (commit, _) = rest.split_once(' ').unwrap();
        let info = format!("Rust</a> {version}</span><a>{commit}</a>");
        std::fs::write(docs.join("version_info.html"), &info).unwrap();
        assert_eq!(own::host_tuple(&rustc).as_deref(), Some("x86_64-unknown-linux-gnu"));
        let other = rustlib.join("aarch64-unknown-linux-gnu/bin");
        std::fs::create_dir_all(&other).unwrap();
        std::fs::write(other.join("rust-lld"), b"ambiguous second host").unwrap();
        assert!(own::host_tuple(&rustc).is_none());
        std::fs::remove_file(other.join("rust-lld")).unwrap();
        std::fs::write(docs.join("version_info.html"), info.replace(commit, "unknown")).unwrap();
        assert!(own::host_tuple(&rustc).is_none());
        std::fs::write(docs.join("version_info.html"), info.replace(version, "9.99.0")).unwrap();
        assert!(
            own::host_tuple(&rustc).is_none(),
            "a known commit cannot override a wrong version"
        );
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
    fn cargo_version_never_hands_off_when_installed_facts_are_absent() {
        let dir = tempfile::tempdir().unwrap();
        synthetic_toolchain(dir.path());
        let executable = dir.path().join("bin/cargo");
        std::fs::write(&executable, "this tool must not execute").unwrap();
        let query = || {
            let mut command = Command::new(&executable);
            command.current_dir(dir.path()).arg("--version");
            #[cfg(unix)]
            {
                use std::os::unix::ffi::OsStringExt as _;
                command.env("UNREAD_RAW_VALUE", OsString::from_vec(vec![0xff]));
            }
            ask(&mut command).expect("an owned version query must never execute Cargo")
        };
        let why = query().unwrap_err().to_string();
        assert_eq!(why, "owned Cargo version facts unavailable; Cargo was not executed");
        let manifest = dir.path().join("lib/rustlib/multirust-channel-manifest.toml");
        let rustc = std::fs::read_to_string(&manifest).unwrap();
        std::fs::write(
            &manifest,
            format!("{rustc}\n[pkg.cargo]\nversion = \"0.100.0 (123456789 2026-10-01)\"\n"),
        )
        .unwrap();
        let version = query().unwrap();
        assert!(version.status.success());
        assert_eq!(version.stdout, b"cargo 1.99.0 (123456789 2026-10-01)\n");
        std::fs::write(
            &manifest,
            format!("{rustc}\n[pkg.cargo]\nversion = \"0.99.2 (123456789 2026-10-01)\"\n"),
        )
        .unwrap();
        assert_eq!(query().unwrap().stdout, b"cargo 1.98.2 (123456789 2026-10-01)\n");
        std::fs::write(&manifest, format!("{rustc}\n[pkg.cargo]\nversion = \"malformed\"\n"))
            .unwrap();
        assert!(query().unwrap_err().to_string().contains("Cargo was not executed"));
        for malformed in [
            "0.0.0 (123456789 2026-10-01)",
            "1.99.0 (123456789 2026-10-01)",
            "0.100.0 arbitrary",
            "0.100.0 (123456789 2026-13-01)",
        ] {
            std::fs::write(&manifest, format!("{rustc}\n[pkg.cargo]\nversion = {malformed:?}\n"))
                .unwrap();
            assert!(query().unwrap_err().to_string().contains("Cargo was not executed"));
        }
    }

    #[test]
    fn owned_cargo_frontdoor_metadata_resolves_without_executing_itself() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::create_dir_all(dir.path().join("dependency/src")).unwrap();
        std::fs::write(dir.path().join("src/lib.rs"), "").unwrap();
        std::fs::write(dir.path().join("dependency/src/lib.rs"), "").unwrap();
        std::fs::write(
            dir.path().join("Cargo.toml"),
            "[package]\nname='query_root'\nversion='0.1.0'\nedition='2021'\n[dependencies]\nquery_dep={path='dependency'}\n",
        ).unwrap();
        std::fs::write(
            dir.path().join("dependency/Cargo.toml"),
            "[package]\nname='query_dep'\nversion='0.1.0'\nedition='2021'\n",
        )
        .unwrap();
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .current_dir(dir.path())
            .env("CARGO_HOME", dir.path().join("cargo-home"))
            .env_remove("RUSTUP_TOOLCHAIN")
            .args(["metadata", "--format-version=1", "--offline"]);
        let rustc = dir.path().join("missing-compiler");
        let full = cargo_query(&mut command, rustc.as_os_str()).unwrap();
        assert!(full.status.success());
        let full: serde_json::Value = serde_json::from_slice(&full.stdout).unwrap();
        assert_eq!(full["packages"].as_array().unwrap().len(), 2);
        assert_eq!(full["resolve"]["nodes"].as_array().unwrap().len(), 2);
        assert!(dir.path().join("Cargo.lock").is_file());
        command.arg("--locked");
        let locked = cargo_query(&mut command, rustc.as_os_str()).unwrap();
        assert_eq!(serde_json::from_slice::<serde_json::Value>(&locked.stdout).unwrap(), full);
        command.arg("--no-deps");
        let shallow = cargo_query(&mut command, rustc.as_os_str()).unwrap();
        let shallow: serde_json::Value = serde_json::from_slice(&shallow.stdout).unwrap();
        assert!(shallow["resolve"].is_null());
        assert!(shallow["packages"].as_array().unwrap().iter().any(|p| p["name"] == "query_root"));
    }

    #[cfg(unix)]
    #[test]
    fn owned_cargo_frontdoor_metadata_projects_opaque_environment_overrides() {
        use std::os::unix::ffi::OsStringExt as _;
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("src/lib.rs"), "").unwrap();
        std::fs::write(
            dir.path().join("Cargo.toml"),
            "[package]\nname='opaque_query'\nversion='0.1.0'\nedition='2021'\n",
        )
        .unwrap();
        let inherited = dir.path().join("inherited-target");
        let vars = crate::process::Native::of([
            ("HOME".into(), dir.path().as_os_str().to_owned()),
            ("CARGO_TARGET_DIR".into(), inherited.clone().into_os_string()),
            ("UNREAD_QUERY_OVERRIDE".into(), "inherited-text".into()),
        ]);
        let _scope = crate::process::enter_native(vars);
        let rustc = dir.path().join("missing-rustc");
        let mut command = Command::new(std::env::current_exe().unwrap());
        command.current_dir(dir.path()).env("CARGO_HOME", dir.path().join("cargo-home")).args([
            "metadata",
            "--format-version=1",
            "--offline",
        ]);
        let inherited_output = cargo_query(&mut command, rustc.as_os_str()).unwrap();
        let inherited_metadata: serde_json::Value =
            serde_json::from_slice(&inherited_output.stdout).unwrap();
        assert_eq!(inherited_metadata["target_directory"], inherited.to_str().unwrap());
        let opaque = OsString::from_vec(vec![0xff]);
        command
            .env("UNREAD_OPAQUE_QUERY_VALUE", &opaque)
            .env(opaque.clone(), "unused")
            .env("UNREAD_QUERY_OVERRIDE", &opaque);
        let effective = cargo_environment(&command);
        let projected: std::collections::BTreeMap<_, _> =
            crate::process::unicode(effective).collect();
        assert!(!projected.contains_key("UNREAD_QUERY_OVERRIDE"));
        let output = cargo_query(&mut command, rustc.as_os_str()).unwrap();
        let metadata: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(metadata["target_directory"], inherited.to_str().unwrap());
        command.env("CARGO_TARGET_DIR", &opaque);
        let error = cargo_query(&mut command, rustc.as_os_str()).unwrap_err().to_string();
        assert!(
            error.contains("path contains invalid UTF-8 characters in CARGO_TARGET_DIR"),
            "{error}"
        );
        assert!(error.contains("Cargo was not executed"));
        command.args(["--no-deps"]);
        assert!(
            cargo_query(&mut command, rustc.as_os_str())
                .unwrap_err()
                .to_string()
                .contains("invalid UTF-8")
        );
        command.env_remove("CARGO_TARGET_DIR").env("CARGO_BUILD_TARGET_DIR", opaque);
        assert!(
            cargo_query(&mut command, rustc.as_os_str())
                .unwrap_err()
                .to_string()
                .contains("CARGO_BUILD_TARGET_DIR")
        );
    }

    #[test]
    fn owned_cargo_frontdoor_uses_explicit_compiler_facts_away_from_its_executable() {
        let dir = tempfile::tempdir().unwrap();
        let toolchain = dir.path().join("toolchain");
        let rustc = synthetic_toolchain(&toolchain);
        let manifest = toolchain.join("lib/rustlib/multirust-channel-manifest.toml");
        let facts = std::fs::read_to_string(&manifest).unwrap();
        std::fs::write(
            &manifest,
            format!("{facts}\n[pkg.cargo]\nversion = \"0.100.0 (123456789 2026-10-01)\"\n"),
        )
        .unwrap();
        let mut command = Command::new(std::env::current_exe().unwrap());
        command.current_dir(dir.path()).arg("--version");
        let version = cargo_query(&mut command, rustc.as_os_str()).unwrap();
        assert_eq!(version.stdout, b"cargo 1.99.0 (123456789 2026-10-01)\n");
        // A producer's RUSTC is an owned probe launcher; source-toolchain facts
        // remain in the explicitly selected installation, not beside CARGO.
        command.env("RUSTUP_TOOLCHAIN", &toolchain);
        let launcher = dir.path().join("owned-probe-launcher");
        std::fs::write(&launcher, "must not execute").unwrap();
        assert_eq!(cargo_query(&mut command, launcher.as_os_str()).unwrap().stdout, version.stdout);
    }

    #[test]
    fn owned_cargo_frontdoor_missing_facts_and_unknown_commands_never_recurse() {
        let dir = tempfile::tempdir().unwrap();
        let rustc = dir.path().join("missing-rustc");
        let mut version = Command::new(std::env::current_exe().unwrap());
        version.arg("--version").env_remove("RUSTUP_TOOLCHAIN");
        let error = cargo_query(&mut version, rustc.as_os_str()).unwrap_err();
        assert_eq!(
            error.to_string(),
            "owned Cargo version facts unavailable; Cargo was not executed"
        );
        let compiler = synthetic_toolchain(&dir.path().join("toolchain"));
        let mut unknown = Command::new(std::env::current_exe().unwrap());
        unknown.arg("unknown-command");
        let error = cargo_query(&mut unknown, compiler.as_os_str()).unwrap_err();
        assert_eq!(error.to_string(), "unsupported owned Cargo command; Cargo was not executed");
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
