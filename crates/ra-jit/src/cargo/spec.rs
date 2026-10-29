//! Build specs from blaze's own resolution. No cargo or rustc is invoked here.

use std::{
    collections::BTreeMap,
    ffi::OsString,
    io::Write as _,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

use anyhow::{Context as _, bail, ensure};
use blaze_spec::{
    Refusal,
    assemble::{Context, Host, Spec},
    flags::{Request, Subcommand},
};
use blaze_wire::{BuildSpec, Namespace, ProfileSpec, RootRef, SourceRef, StoreIdentity};
use sha2::{Digest as _, Sha256};

/// The owned JSON compiler's global debug/link decisions, explicitly supplied by a
/// validated replay rather than inferred from its caller's environment.
pub(crate) fn project_profile(project: &Path) -> anyhow::Result<Option<crate::plan::Whole>> {
    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Bound {
        version: u32,
        profile: Vec<u8>,
        rustflags: Vec<String>,
    }
    if project.extension().is_none_or(|ext| ext != "json") {
        return Ok(None);
    }
    let json: serde_json::Value = serde_json::from_slice(&std::fs::read(project)?)?;
    let Some(value) = json.get("blaze_spec_profile") else { return Ok(None) };
    let bound: Bound = serde_json::from_value(value.clone())?;
    ensure!(bound.version == 1, "unknown owned replay profile version");
    let profile: ProfileSpec = blaze_wire::decode(&bound.profile, &blaze_wire::Limits::DOCUMENTED)?;
    ensure!(
        profile.opt_level <= 3
            && profile.debug <= 2
            && profile.panic <= 1
            && profile.strip <= 2
            && profile.split_debuginfo <= 2
            && profile.backend <= 1
            && profile.mir_opt <= 2
            && profile.backend_opt <= 3
            && profile.backend_emit <= 1,
        "owned replay profile contains an unsupported value"
    );
    let flags = blaze_spec::rustflags::read(&bound.rustflags)?;
    ensure!(flags.flags == bound.rustflags, "owned replay flags are not canonical");
    use crate::plan::{Described, Kept, Level, Whole};
    let level = match (profile.debug, profile.strip) {
        (0, _) | (_, 1 | 2) => Level::Nothing,
        (1, 0) => Level::Lines,
        (2, 0) => Level::Full,
        _ => bail!("unknown owned replay debug/strip profile"),
    };
    let kept = match profile.split_debuginfo {
        _ if level == Level::Nothing => Kept::Linked,
        0 => Kept::Linked,
        1 => Kept::Package,
        2 => Kept::Objects,
        _ => bail!("unknown owned replay split-debuginfo profile"),
    };
    let frame_pointers = flags
        .flags
        .iter()
        .rev()
        .find_map(|word| {
            word.strip_prefix("-Cforce-frame-pointers=")
                .map(|value| matches!(value, "yes" | "on" | "true" | "y" | "always" | "non-leaf"))
        })
        .unwrap_or(false);
    let lints_capped = flags
        .flags
        .iter()
        .any(|word| word.starts_with("--cap-lints=") || word.starts_with("--force-warn="));
    Ok(Some(Whole { debug: Described { level, kept, frame_pointers }, lto: false, lints_capped }))
}

pub(super) fn command(words: &[String]) -> anyhow::Result<i32> {
    if let [run, file] = words
        && run == "--run"
    {
        return replay(Path::new(file));
    }
    let mut graph = false;
    let mut out = None;
    let mut at = 0;
    while let Some(word) = words.get(at) {
        match word.as_str() {
            "--unit-graph" => graph = true,
            "--out" => {
                at += 1;
                out = Some(PathBuf::from(words.get(at).context("--out needs a file")?));
            }
            _ => break,
        }
        at += 1;
    }
    let subcommand = match words.get(at).map(String::as_str) {
        Some("build") => Subcommand::Build,
        Some("check") => Subcommand::Check,
        Some("run") => Subcommand::Run,
        Some("test") => Subcommand::Test,
        _ => bail!(
            "usage: cargo spec [--unit-graph] [--out FILE] build|check|run|test [flags]; cargo spec --run FILE"
        ),
    };
    let request = match Request::parse(subcommand, &words[at + 1..]) {
        Ok(request) => request,
        Err(why) => return refused(&why),
    };
    let cwd = std::env::current_dir()?;
    let native_env = blaze_spec::environment::native_process();
    let env = match blaze_spec::environment::text_view(&native_env) {
        Ok(env) => env,
        Err(why) => return refused(&why),
    };
    let (host, identity, _) = host()?;
    let ctx = Context {
        native_env: Some(&native_env),
        source_root: None,
        config_root: None,
        config_image: None,
        cwd: &cwd,
        env: &env,
        host: &host,
        source: SourceRef::Root(RootRef { root: [0; 32] }),
        toolchain: identity,
        offline: false,
        store: None,
        probe: &blaze_spec::platform::System,
    };
    if graph {
        println!("{}", unit_graph(&request, &ctx)?);
        return Ok(0);
    }
    let temporary = tempfile::tempdir()?;
    let out = out.map(|path| blaze_manifest::paths::normalize_path(&cwd.join(path)));
    let store = out
        .as_deref()
        .map(|path| companion(path, "inputs"))
        .unwrap_or_else(|| temporary.path().to_owned());
    std::fs::create_dir_all(&store)?;
    let outputs: Vec<_> = out
        .iter()
        .flat_map(|path| [path.clone(), companion(path, "inputs"), companion(path, "outputs")])
        .collect();
    let made = match blaze_spec::replay::capture(&request, &words[at + 1..], &ctx, &store, &outputs)
    {
        Ok(made) => made,
        Err(error) => match error.downcast_ref::<Refusal>() {
            Some(why) => return refused(why),
            None => return Err(error),
        },
    };
    if let Some(out) = &out {
        // Blobs are immutable and atomically installed before this final publication.
        // Replacing a spec leaves the previous spec's blobs usable by concurrent readers.
        let mut writing =
            tempfile::NamedTempFile::new_in(out.parent().context("--out has no parent")?)?;
        writing.write_all(&blaze_wire::encode(&made.build))?;
        writing.as_file().sync_all()?;
        writing.persist(out).map_err(|error| error.error)?;
    }
    println!("{}", rendering(&made)?);
    Ok(0)
}

fn refused(why: &Refusal) -> anyhow::Result<i32> {
    println!(
        "{}",
        serde_json::json!({
            "refused": {"what": why.what, "why": why.why},
            "digests": null, "summary": null, "spec": null, "platform": null,
        })
    );
    Ok(2)
}

fn rendering(made: &Spec) -> anyhow::Result<serde_json::Value> {
    let asked: Vec<_> = made
        .build
        .asked
        .iter()
        .map(|&at| {
            let unit = &made.build.units[at as usize];
            format!("{}@{}:{}:{}", unit.package.0, unit.package.1, unit.target.0, unit.target.1)
        })
        .collect();
    let command = match made.build.command {
        blaze_wire::Command::Build => "build",
        blaze_wire::Command::Check => "check",
        blaze_wire::Command::Run(_) => "run",
        blaze_wire::Command::Test(_) => "test",
    };
    Ok(serde_json::json!({
        "refused": null,
        "digests": {
            "spec": hex(&made.digest), "platform": hex(&made.platform_digest),
            "front_end": hex(&made.front_end.0), "config": hex(&made.config.0),
            "job_content": hex(&made.job_content(Namespace::Main).0),
        },
        "summary": {"command": command, "asked": asked, "units": made.build.units.len()},
        "spec": serde_json::from_str::<serde_json::Value>(&blaze_wire::to_json(&made.build))?["BuildSpec"],
        "platform": serde_json::from_str::<serde_json::Value>(&blaze_wire::to_json(&made.platform))?["ExecPlatform"],
    }))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn companion(file: &Path, suffix: &str) -> PathBuf {
    let mut name = file.as_os_str().to_owned();
    name.push(format!(".{suffix}"));
    PathBuf::from(name)
}

/// The shipped answer table reads toolchain records, never executes its rustc.
fn host() -> anyhow::Result<(Host, StoreIdentity, PathBuf)> {
    let home = std::env::var_os("RUSTUP_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|it| PathBuf::from(it).join(".rustup")))
        .context("cannot find the toolchain records")?;
    let mut dirs: Vec<_> = std::fs::read_dir(home.join("toolchains"))?
        .filter_map(Result::ok)
        .filter(|entry| {
            entry.file_name().to_string_lossy().starts_with(super::toolchain::OURS.trim())
        })
        .map(|entry| entry.path())
        .collect();
    dirs.sort();
    let sysroot = dirs.first().context("blaze's release is not installed in rustup")?;
    let rustc = sysroot.join("bin/rustc");
    let answer = |args: &[&str]| -> anyhow::Result<String> {
        let words: Vec<OsString> = args.iter().map(OsString::from).collect();
        let bytes = crate::answers::own_answer(&rustc, &words)
            .context("blaze has no ported answer for its toolchain records")?;
        Ok(String::from_utf8(bytes)?)
    };
    let verbose = answer(&["-vV"])?;
    let triple =
        verbose.lines().find_map(|line| line.strip_prefix("host: ")).context("no host")?.to_owned();
    let commit = verbose
        .lines()
        .find_map(|line| line.strip_prefix("commit-hash: "))
        .context("no toolchain commit")?;
    let cfg = answer(&["--print", "cfg", "--target", &triple])?
        .lines()
        .map(str::parse)
        .collect::<Result<Vec<_>, _>>()?;
    let build = match option_env!("RA_JIT_SOURCES") {
        Some(sources) => [&b"sources "[..], sources.as_bytes()].concat(),
        None => Sha256::digest(std::fs::read(std::env::current_exe()?)?).to_vec(),
    };
    let cpu = hir_ty_codegen::Target::select(&triple).map_err(anyhow::Error::msg)?.cpu();
    Ok((
        Host { triple, cfg, cpu: cpu.to_owned(), llvm: cfg!(feature = "llvm") },
        StoreIdentity::from_store(ra_jit_store::FORMAT, &build, commit),
        sysroot.clone(),
    ))
}

fn unit_graph(request: &Request, ctx: &Context<'_>) -> anyhow::Result<serde_json::Value> {
    let asked = blaze_metadata::Asked {
        cargo_home: ctx
            .native_env
            .and_then(|env| blaze_spec::environment::cargo_home(env, ctx.cwd)),
        cwd: ctx.cwd,
        env: ctx.env.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
        manifest_path: request.manifest_path.as_deref(),
        config: &request.config,
        features: &request.features,
        all_features: request.all_features,
        no_default_features: request.no_default_features,
        build: Some(blaze_metadata::Build {
            packages: &request.packages,
            workspace: request.workspace,
            excludes: &request.excludes,
            dev_units: request.needs_dev_dependencies(),
        }),
        platform: Some(blaze_metadata::TargetPlatform {
            triple: &ctx.host.triple,
            cfg: &ctx.host.cfg,
        }),
        offline: request.offline,
        store: None,
    };
    let targets = if request.doc {
        blaze_metadata::units::TargetFilter { doc_only: true, ..Default::default() }
    } else {
        request.targets.clone()
    };
    let units = blaze_metadata::units::UnitsAsked {
        asked: &asked,
        targets: &targets,
        profile: request.profile.as_deref(),
        command: match request.command {
            Some(Subcommand::Check) => blaze_metadata::units::Command::Check,
            Some(Subcommand::Test) => blaze_metadata::units::Command::Test,
            _ => blaze_metadata::units::Command::Build,
        },
    };
    Ok(blaze_metadata::units::unit_graph(&units)?.to_cargo_json())
}

fn replay(file: &Path) -> anyhow::Result<i32> {
    let file = file.canonicalize()?;
    let given: BuildSpec =
        blaze_wire::decode(&std::fs::read(&file)?, &blaze_wire::Limits::DOCUMENTED)?;
    let (host, identity, sysroot) = host()?;
    let store = companion(&file, "inputs");
    let outputs = companion(&file, "outputs");
    std::fs::create_dir_all(&outputs)?;
    let job = tempfile::Builder::new().prefix("replay-").tempdir_in(&outputs)?;
    let executor = blaze_spec::environment::native_process();
    let prepared = blaze_spec::replay::prepare(
        &given,
        &store,
        &job.path().join("source"),
        &host,
        identity,
        &blaze_spec::platform::System,
        &executor,
    )?;
    let programs =
        blaze_spec::execution::programs(&prepared, &sysroot, &job.path().join("artifacts"))?;
    // All command and unit support checks happen before the first compiler starts.
    let compiler_home = job.path().join("compiler-home");
    std::fs::create_dir(&compiler_home)?;
    let compiler_env = compiler_environment(&prepared, &compiler_home, &executor)?;
    for program in &programs {
        compile(program, &compiler_env)?;
        admit_inputs(program, &prepared, &store)?;
    }
    let result = serde_json::json!({
        "spec": hex(&blaze_wire::digest_of(&given)),
        "artifacts": programs.iter().map(|program| serde_json::json!({
            "unit": program.unit, "crate": program.crate_name,
            "executable": program.executable,
        })).collect::<Vec<_>>(),
    });
    std::fs::write(job.path().join("result.json"), serde_json::to_vec_pretty(&result)?)?;
    // The source copy and paths embedded for CARGO_BIN_EXE_* stay at the same names.
    // Failed compiles/admissions drop this temporary directory and all their artifacts.
    let _kept = job.keep();
    if !programs.iter().any(|program| program.run) {
        println!("{result}");
        return Ok(0);
    }
    let cargo = sysroot.join("bin").join(format!("cargo{}", std::env::consts::EXE_SUFFIX));
    let native = blaze_spec::assemble::canonical_units(&given, &prepared.resolved.graph)?;
    let mut failed = 0;
    for program in programs.iter().filter(|program| program.run) {
        let run = match &given.command {
            blaze_wire::Command::Run(run) | blaze_wire::Command::Test(run) => run,
            _ => unreachable!("the execution plan runs only run/test commands"),
        };
        let env = blaze_spec::execution::runtime_environment(
            &prepared,
            program,
            &executor,
            &cargo,
            &compiler_home,
        )?;
        let mut command = Command::new(&program.executable);
        command.env_clear().envs(&env).envs(run.env.iter().map(|(k, v)| (k, v)));
        if let blaze_wire::Command::Test(_) = &given.command {
            command.args(&run.filters);
            command.current_dir(&native[program.unit as usize].package.manifest_dir);
        } else {
            command.current_dir(&prepared.cwd);
        }
        command.args(&run.harness_args);
        let code = command.status()?.code().unwrap_or(1);
        if code != 0 {
            failed = code;
            if !run.no_fail_fast {
                break;
            }
        }
    }
    Ok(failed)
}

/// Fixed implementation policy plus the request's recorded values. A replay caller's
/// backend, rustflags, producer toggles, wrappers and arbitrary variables are never inherited.
fn compiler_environment(
    prepared: &blaze_spec::replay::Prepared,
    home: &Path,
    executor: &BTreeMap<String, OsString>,
) -> anyhow::Result<BTreeMap<String, OsString>> {
    let mut env = prepared.execution_environment()?;
    if !env.contains_key("PATH")
        && let Some(path) = executor.get("PATH")
    {
        env.insert("PATH".to_owned(), path.clone());
    }
    // Build-system flags have already been resolved into each JSON crate. Keep them
    // out of the owned standard library's separate source resolution.
    env.retain(|name, _| {
        !(name.starts_with("CARGO_")
            || name.starts_with("RA_JIT_")
            || name.starts_with("BLAZE_")
            || name.starts_with("RUSTC")
            || matches!(name.as_str(), "RUSTFLAGS" | "RUSTDOCFLAGS" | "CARGO"))
    });
    let profile = &prepared.resolved.spec.build.profile;
    env.insert("CARGO_HOME".to_owned(), home.join("cargo").into());
    env.insert("HOME".to_owned(), home.into());
    for (name, value) in [
        ("RA_JIT_OWN_RESOLVE", "1".to_owned()),
        ("BLAZE_OWN_RESOLVE", "1".to_owned()),
        ("RA_JIT_SPEC_REPLAY", "1".to_owned()),
        ("RA_JIT_ANSWERS", "own".to_owned()),
        ("BLAZE_ANSWERS", "own".to_owned()),
        ("RA_JIT_PROC_MACRO_SERVER", "0".to_owned()),
        ("RA_JIT_STD_FROM", "off".to_owned()),
        ("RA_JIT_CACHE", "off".to_owned()),
        ("RA_JIT_BACKEND", if profile.backend == 1 { "llvm" } else { "cranelift" }.to_owned()),
        ("RA_JIT_OPT", profile.backend_opt.to_string()),
        ("RA_JIT_EMIT", if profile.backend_emit == 1 { "fast" } else { "optimized" }.to_owned()),
        ("RA_JIT_MIR_OPT", profile.mir_opt.to_string()),
        ("RUST_BACKTRACE", "0".to_owned()),
    ] {
        env.insert(name.to_owned(), value.into());
    }
    // Memory budgets and the watchdog label are service policy, not compiler inputs.
    // They never enter per-crate env and cannot select a different code generator.
    for name in [
        "RA_JIT_MEMORY_MAX",
        "RA_JIT_WORKER_MEMORY",
        "RA_JIT_MEMORY_RESERVE",
        "RA_JIT_PRESSURE_GRACE_SECS",
        "RA_JIT_OWN_BASE_MEMORY",
        "RA_JIT_SPARE",
        "RA_JIT_RUN_ID",
    ] {
        if let Some(value) = executor.get(name) {
            env.insert(name.to_owned(), value.clone());
        }
    }
    Ok(env)
}

fn compile(
    program: &blaze_spec::execution::Program,
    env: &BTreeMap<String, OsString>,
) -> anyhow::Result<()> {
    let dir = program.executable.parent().context("artifact has no directory")?;
    std::fs::create_dir_all(dir)?;
    let project = dir.join("rust-project.json");
    std::fs::write(&project, serde_json::to_vec(&program.project)?)?;
    let mut command = Command::new(std::env::current_exe()?);
    command.env_clear().envs(env).current_dir(dir).stdin(Stdio::null());
    command.arg(&project).args(["--crate", &program.crate_name]);
    command.arg("--out").arg(dir.join("compiled.o"));
    command.arg("--exe").arg(&program.executable);
    command.arg("--inputs").arg(dir.join("inputs.cbor"));
    if program.harness {
        // The prebuilt std unit excludes libtest. Compile that runtime with the
        // harness, selecting its source even when the application is named `test`.
        let library = program.project["sysroot_src"]
            .as_str()
            .context("test compiler description has no sysroot sources")?;
        command.args(["--cfg-test", "--crate"]).arg(Path::new(library).join("test/src/lib.rs"));
    }
    let output = command.output().context("starting the owned replay compiler")?;
    std::fs::write(dir.join("compiler.log"), &output.stderr)?;
    let said = String::from_utf8_lossy(&output.stderr);
    ensure!(
        output.status.success(),
        "owned replay compiler failed for {}: {said}",
        program.crate_name
    );
    ensure!(
        !said.lines().any(|line| line.contains(" functions in it ra-jit could not compile")
            && !line.ends_with(", 0 functions in it ra-jit could not compile")),
        "owned replay compiler left uncompiled functions in {}: {said}",
        program.crate_name
    );
    ensure!(
        program.executable.is_file(),
        "owned compiler did not link {}",
        program.executable.display()
    );
    Ok(())
}

/// The complete non-language dependency trace includes modules and all include macros.
/// Admission happens before any executable is run or advertised. This is a correctness
/// check after compilation, not a filesystem sandbox or a claim of pre-read confinement.
fn admit_inputs(
    program: &blaze_spec::execution::Program,
    prepared: &blaze_spec::replay::Prepared,
    store: &Path,
) -> anyhow::Result<()> {
    let source = match &prepared.resolved.spec.build.source {
        SourceRef::Root(source) => source.root,
        _ => unreachable!("prepared replay has a snapshot"),
    };
    let dir = program.executable.parent().context("artifact has no directory")?;
    let trace = std::fs::read(dir.join("inputs.cbor"))
        .context("owned compiler did not report its complete source inputs")?;
    let inputs = crate::dep_info::DepInfo::decode_replay(&trace)?;
    ensure!(!inputs.files.is_empty(), "owned compiler reported an empty source trace");
    for path in &inputs.files {
        let path = dir.join(path).canonicalize()?;
        let relative = path.strip_prefix(&prepared.root).with_context(|| {
            format!("compiler read {} outside the immutable source snapshot", path.display())
        })?;
        let logical = blaze_wire::SourcePath::from_native(relative)?;
        let captured = blaze_spec::snapshot::Snapshot::read_file(source, store, &logical)?;
        ensure!(
            std::fs::read(&path)? == captured,
            "compiler input {} changed after snapshot validation",
            path.display()
        );
    }
    let crates =
        program.project["crates"].as_array().context("no source crates in compiler description")?;
    for (name, value) in &inputs.env {
        let declared = value.as_deref().is_some_and(|value| {
            crates.iter().any(|krate| {
                krate["env"].get(name).and_then(serde_json::Value::as_str) == Some(value)
            })
        });
        // Cargo derives these absences per crate. An arbitrary option_env! read is
        // not proof of a captured absence: the requester may have had a secret value.
        let derived_absence = value.is_none()
            && matches!(name.as_str(), "CARGO_PRIMARY_PACKAGE" | "CARGO_BIN_NAME" | "OUT_DIR");
        ensure!(declared || derived_absence, "compiler read undeclared environment value {name}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn profile() -> ProfileSpec {
        ProfileSpec {
            name: "custom".into(),
            opt_level: 1,
            debug: 2,
            debug_assertions: true,
            overflow_checks: true,
            panic: 0,
            strip: 0,
            split_debuginfo: 1,
            backend: 0,
            mir_opt: 1,
            backend_opt: 0,
            backend_emit: 0,
        }
    }

    fn project(dir: &Path, profile: &ProfileSpec, flags: &[&str]) -> PathBuf {
        let path = dir.join("rust-project.json");
        std::fs::write(
            &path,
            serde_json::to_vec(&serde_json::json!({
                "blaze_spec_profile": {"version": 1, "profile": blaze_wire::encode(profile),
                    "rustflags": flags},
            }))
            .unwrap(),
        )
        .unwrap();
        path
    }

    #[test]
    fn project_profile_binds_debug_split_frame_pointer_and_lint_decisions() {
        let dir = tempfile::tempdir().unwrap();
        let path =
            project(dir.path(), &profile(), &["-Cforce-frame-pointers=yes", "--cap-lints=warn"]);
        let whole = project_profile(&path).unwrap().unwrap();
        assert_eq!(whole.debug.level, crate::plan::Level::Full);
        assert_eq!(whole.debug.kept, crate::plan::Kept::Package);
        assert!(whole.debug.frame_pointers);
        assert!(whole.lints_capped);
        assert!(!whole.lto);
        let mut stripped = profile();
        stripped.strip = 1;
        let whole = project_profile(&project(dir.path(), &stripped, &[])).unwrap().unwrap();
        assert_eq!(whole.debug.level, crate::plan::Level::Nothing);
        assert_eq!(whole.debug.kept, crate::plan::Kept::Linked);
    }

    #[test]
    fn project_profile_rejects_unrepresentable_and_noncanonical_inputs() {
        let dir = tempfile::tempdir().unwrap();
        let mut bad = profile();
        bad.split_debuginfo = 9;
        assert!(project_profile(&project(dir.path(), &bad, &[])).is_err());
        assert!(project_profile(&project(dir.path(), &profile(), &["-C", "opt-level=1"])).is_err());
        let path = dir.path().join("rust-project.json");
        std::fs::write(&path, r#"{"crates":[]}"#).unwrap();
        assert!(project_profile(&path).unwrap().is_none());
        std::fs::write(
            &path,
            r#"{"blaze_spec_profile":{"version":7,"profile":[],"rustflags":[]}}"#,
        )
        .unwrap();
        assert!(project_profile(&path).is_err());
    }
}
