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
use blaze_wire::{Namespace, ProfileSpec, RootRef, SourceRef, StoreIdentity};
use sha2::{Digest as _, Sha256};

pub(super) mod direct;
mod producers;
mod runtime;
mod test_policy;

/// Install saved native admission independently of the shared source descriptor loader.
pub(crate) fn install_bindings(project: Option<&serde_json::Value>) -> anyhow::Result<()> {
    let bindings: Option<Vec<u8>> = project
        .and_then(|project| project.get(blaze_wire::native::BINDINGS_FIELD))
        .map(|value| {
            serde_json::from_value(value.clone())
                .map_err(|_| anyhow::anyhow!("invalid owned native bindings transport"))
        })
        .transpose()?;
    crate::native_inputs::set_replay_bindings(bindings.as_deref())
}

/// Validate the explicitly pinned runtime carried only in the private descriptor.
pub(crate) fn runtime_server(project: &serde_json::Value) -> anyhow::Result<Option<PathBuf>> {
    runtime::server(project)
}

pub(crate) fn runtime_environment(
    project: &serde_json::Value,
) -> anyhow::Result<Option<BTreeMap<OsString, OsString>>> {
    runtime::described_environment(project)
}

/// Only the replay creates this directory, under its fresh job directory. The existing
/// std-unit key distinguishes library graphs, cfgs, target and debug settings; the job
/// fixes the compiler, sysroot and backend. Never consult an ambient store for these units.
pub(crate) fn std_unit_cache(project: &Path, key: &[u8; 32]) -> anyhow::Result<Option<PathBuf>> {
    if project.extension().is_none_or(|extension| extension != "json") {
        return Ok(None);
    }
    let project = crate::compiler_json::read(project)?;
    let Some(bound) = project.get("blaze_spec_std_units") else { return Ok(None) };
    ensure!(
        project["compiler_mode"] == true && bound["version"] == 1,
        "unknown replay standard library transport"
    );
    let directory = Path::new(bound["directory"].as_str().context("no replay std directory")?);
    let directory = crate::compiler_json::physical_path(&project, directory)?;
    ensure!(directory.is_absolute() && directory.is_dir(), "invalid replay std directory");
    Ok(Some(directory.join(hex(key))))
}

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
    let json = crate::compiler_json::read(project)?;
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
    if let [mode, project, root, output, trace] = words
        && mode == "--compiler-check"
    {
        let args = crate::Args::parse(
            [project.clone(), "--crate".into(), root.clone(), "--out".into(), output.clone()]
                .into_iter(),
        )?;
        if let Err(error) = crate::check::once(args, Path::new(trace)) {
            let directory = Path::new(output)
                .parent()
                .context("check output has no directory")?
                .join("diagnostics");
            std::fs::create_dir_all(&directory)?;
            let said = crate::errors::Said::of_reply(&error.to_string());
            std::fs::write(directory.join("diagnostics.json"), serde_json::to_vec(&said)?)?;
            eprintln!("{error:#}");
            return Ok(101);
        }
        return Ok(0);
    }
    if let [mode, project, root] = words
        && mode == "--compiler-doctest"
    {
        return crate::doctest::saved(Path::new(project), root);
    }
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
        rust_release: Some(&crate::answers::rust_release),
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
        // The file names the replay record by its digest, as the spec names the sources.
        let mut writing =
            tempfile::NamedTempFile::new_in(out.parent().context("--out has no parent")?)?;
        writing.write_all(&blaze_wire::encode(&blaze_spec::replay::Saved::of(&made)?))?;
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
    use super::toolchain::Chosen;
    let (Chosen::Given(sysroot) | Chosen::Rustup(sysroot) | Chosen::Own(sysroot)) =
        super::toolchain::settle().map_err(anyhow::Error::msg)?;
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
        sysroot,
    ))
}

fn unit_graph(request: &Request, ctx: &Context<'_>) -> anyhow::Result<serde_json::Value> {
    let asked = blaze_metadata::Asked {
        locked: request.locked,
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
        rust_release: ctx.rust_release,
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
    let saved: blaze_spec::replay::Saved =
        blaze_wire::decode(&std::fs::read(&file)?, &blaze_wire::Limits::DOCUMENTED)
            .context("not a spec saved by this version of `cargo spec --out`")?;
    let given = &saved.build;
    let (host, identity, sysroot) = host()?;
    let store = companion(&file, "inputs");
    let outputs = companion(&file, "outputs");
    std::fs::create_dir_all(&outputs)?;
    let native_outputs = outputs.to_str().is_none();
    let job = tempfile::Builder::new().prefix("replay-").tempdir_in(if native_outputs {
        Path::new("/tmp")
    } else {
        &outputs
    })?;
    let executor = blaze_spec::environment::native_process();
    let prepared = blaze_spec::replay::prepare(
        &saved,
        &store,
        &job.path().join("source"),
        &host,
        identity,
        &blaze_spec::platform::System,
        Some(&crate::answers::rust_release),
        &executor,
    )?;
    blaze_spec::execution::preflight(&prepared, true)?;
    let compiler_home = job.path().join("compiler-home");
    std::fs::create_dir(&compiler_home)?;
    let compiler_env = compiler_environment(&prepared, &compiler_home, &executor)?;
    let std_units = job.path().join("std-units");
    std::fs::create_dir(&std_units)?;
    let has_macros = prepared
        .resolved
        .graph
        .units
        .iter()
        .any(|unit| unit.target.kind == blaze_metadata::units::TargetKind::ProcMacro);
    let runtime =
        if has_macros {
            Some(runtime::pin(&executor, &job.path().join("runtime"))?.context(
                "saved producer graph requires an explicitly pinned owned macro runtime",
            )?)
        } else {
            None
        };
    prepared.validate_sources(&store)?;
    let mut bindings = prepared.native_closure(&blaze_spec::platform::System)?;
    bindings.roots.push(blaze_spec::native::Root {
        name: "source".into(),
        tree: blaze_spec::native_tree::Tree::capture(&prepared.root)?,
    });
    bindings.roots.extend(toolchain_native_inputs(&sysroot)?.roots);
    prepared.validate_sources(&store)?;
    bindings.roots.push(blaze_spec::native::Root {
        name: "runtime/compiler".into(),
        tree: blaze_spec::native_tree::Tree::capture(&std::env::current_exe()?)?,
    });
    if let Some(runtime) = &runtime {
        let path = runtime::server(&serde_json::json!({ runtime::FIELD: runtime }))?
            .context("pinned owned macro runtime disappeared")?;
        bindings.roots.push(blaze_spec::native::Root {
            name: "runtime/macro-server".into(),
            tree: blaze_spec::native_tree::Tree::capture(&path)?,
        });
    }
    let producers = producers::prepare(
        &prepared,
        &sysroot,
        &job.path().join("producers"),
        crate::process::Native::of(
            compiler_env.iter().map(|(name, value)| (name.into(), value.clone())),
        ),
        runtime.as_ref(),
        &bindings,
    )?;
    prepared.validate_sources(&store)?;
    producers.admit_inputs()?;
    let mut programs = blaze_spec::execution::programs_with(
        &prepared,
        &sysroot,
        &job.path().join("artifacts"),
        Some(&producers.units),
    )?;
    let mut runtime_env = blaze_spec::execution::worker_base_environment(&executor, &compiler_home);
    runtime_env.extend(prepared.execution_environment()?);
    let tests = test_policy::Policy::new(
        &prepared,
        &outputs.join("test-results"),
        &runtime_env,
        &programs,
        None,
    )?;
    let SourceRef::Root(source) = &given.source else { bail!("replay requires a source snapshot") };
    for program in &mut programs {
        program.project["blaze_spec_std_units"] = serde_json::json!({
            "version": 1, "directory": crate::compiler_json::path(&std_units),
        });
        crate::dep_info::ensure_replay_key(&mut program.project)?;
        source_naming(program, &prepared)?;
        program.project[blaze_wire::native::BINDINGS_FIELD] =
            serde_json::json!(producers.bindings.encode_bindings());
        program.project["blaze_spec_generated_bindings"] =
            serde_json::json!(producers.generated.encode_bindings());
        if let Some(runtime) = &runtime {
            program.project[runtime::FIELD] = runtime.clone();
        }
        program.project["blaze_spec_requester_names"] =
            serde_json::json!(prepared.requester_names());
        program.project["blaze_spec_inputs"] = serde_json::json!({
            "root": crate::compiler_json::path(&prepared.root),
            "store": crate::compiler_json::path(&store),
            "source": source.root,
        });
    }
    // All command and unit support checks happen before the first compiler starts.
    for program in &mut programs {
        let environment = compiler_for(program, &prepared, &producers, &compiler_env, &sysroot)?;
        let reach = tests.as_ref().is_some_and(|tests| tests.reach(program));
        compile(program, &environment, reach)?;
        admit_inputs(program)?;
    }
    prepared.validate_sources(&store)?;
    producers.bindings.validate()?;
    let native = blaze_spec::assemble::canonical_units(given, &prepared.resolved.graph)?;
    let artifacts = programs.iter().map(|program| -> anyhow::Result<serde_json::Value> {
        let native_link = if program.output == blaze_spec::execution::Output::Object {
            let path = program.executable.parent().context("artifact has no directory")?.join("link.cbor");
            let plan = blaze_wire::native::LinkPlan::decode(&std::fs::read(&path)
                .context("owned compiler did not report native link requirements")?)?;
            Some(plan.render())
        } else { None };
        Ok(serde_json::json!({
            "unit": program.unit, "crate": program.crate_name,
            "executable": (program.output == blaze_spec::execution::Output::Executable).then_some(&program.executable),
            "artifact": program.executable,
            "runtime": (program.output == blaze_spec::execution::Output::Object)
                .then(|| ["std.a", "std.o"].into_iter()
                    .map(|extension| program.executable.with_extension(extension))
                    .find(|path| path.is_file())).flatten(),
            "native_link": native_link,
            "target": given.units[program.unit as usize].target.1,
            "target_kind": given.units[program.unit as usize].target.0,
            "test": native[program.unit as usize].test,
            "kind": match program.output {
                blaze_spec::execution::Output::Executable => "executable",
                blaze_spec::execution::Output::Object => "object",
                blaze_spec::execution::Output::Check => "check",
                blaze_spec::execution::Output::Doctest => "doctest",
            },
        }))
    }).collect::<anyhow::Result<Vec<_>>>()?;
    let result = serde_json::json!({
        "spec": hex(&blaze_wire::digest_of(given)),
        "artifacts": artifacts,
    });
    std::fs::write(job.path().join("result.json"), serde_json::to_vec_pretty(&result)?)?;
    // The source copy and paths embedded for CARGO_BIN_EXE_* stay at the same names.
    // Failed compiles/admissions drop this temporary directory and all their artifacts.
    if native_outputs {
        #[cfg(unix)]
        std::os::unix::fs::symlink(job.path(), outputs.join(job.path().file_name().unwrap()))?;
    }
    let _kept = job.keep();
    if !programs.iter().any(|program| program.run) {
        println!("{result}");
        return Ok(0);
    }
    execute(&programs, &prepared, &compiler_env, &runtime_env, &producers, &sysroot, tests.as_ref())
}

fn source_naming(
    program: &mut blaze_spec::execution::Program,
    prepared: &blaze_spec::replay::Prepared,
) -> anyhow::Result<()> {
    let native = blaze_spec::assemble::canonical_units(
        &prepared.resolved.spec.build,
        &prepared.resolved.graph,
    )?;
    let project = &mut program.project;
    project["blaze_compiler_source_root"] = serde_json::json!({"version":1, "unit":program.unit,
        "path":crate::compiler_json::path(&prepared.resolved.graph.root)});
    for krate in project["crates"].as_array_mut().context("no owned source crates")? {
        let at =
            krate["blaze_native_unit"].as_u64().context("source crate has no canonical unit")?;
        let at = usize::try_from(at)?;
        krate["is_workspace_member"] = native
            .get(at)
            .context("source crate unit is outside canonical graph")?
            .package
            .member
            .into();
    }
    Ok(())
}

#[derive(Debug)]
struct RuntimeFailure(anyhow::Error);

impl std::fmt::Display for RuntimeFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}", self.0)
    }
}

impl std::error::Error for RuntimeFailure {}

fn execute(
    programs: &[blaze_spec::execution::Program],
    prepared: &blaze_spec::replay::Prepared,
    compiler_env: &BTreeMap<String, OsString>,
    env: &BTreeMap<String, OsString>,
    producers: &producers::Producers,
    sysroot: &Path,
    tests: Option<&test_policy::Policy>,
) -> anyhow::Result<i32> {
    execute_inner(programs, prepared, compiler_env, env, producers, sysroot, tests)
        .map_err(|error| RuntimeFailure(error).into())
}

fn execute_inner(
    programs: &[blaze_spec::execution::Program],
    prepared: &blaze_spec::replay::Prepared,
    compiler_env: &BTreeMap<String, OsString>,
    env: &BTreeMap<String, OsString>,
    producers: &producers::Producers,
    sysroot: &Path,
    tests: Option<&test_policy::Policy>,
) -> anyhow::Result<i32> {
    let given = &prepared.resolved.spec.build;
    let native = blaze_spec::assemble::canonical_units(given, &prepared.resolved.graph)?;
    let mut failed = 0;
    let (mut left, mut always_run) = (0, 0);
    let mut running: Vec<_> = programs.iter().filter(|program| program.run).collect();
    if matches!(given.command, blaze_wire::Command::Test(_)) {
        running.sort_by_key(|program| {
            let unit = native[program.unit as usize];
            (
                program.output == blaze_spec::execution::Output::Doctest,
                &unit.package.name,
                unit.target.kind,
                &unit.target.name,
            )
        });
    }
    for program in running {
        let run = match &given.command {
            blaze_wire::Command::Run(run) | blaze_wire::Command::Test(run) => run,
            _ => unreachable!("the execution plan runs only run/test commands"),
        };
        if matches!(given.command, blaze_wire::Command::Test(_)) {
            if program.output == blaze_spec::execution::Output::Doctest {
                super::say::status(false, "Doc-tests", &program.crate_name);
            }
        }
        let root = native[program.unit as usize];
        let mut runtime_unit = producers
            .runtime
            .get(&program.unit)
            .context("root has no runtime environment")?
            .clone();
        if matches!(
            root.target.kind,
            blaze_metadata::units::TargetKind::Test | blaze_metadata::units::TargetKind::Bench
        ) {
            for binary in programs.iter().filter(|binary| {
                let unit = native[binary.unit as usize];
                !unit.test
                    && unit.target.kind == blaze_metadata::units::TargetKind::Bin
                    && unit.package.id == root.package.id
            }) {
                runtime_unit.env.insert(
                    format!("CARGO_BIN_EXE_{}", native[binary.unit as usize].target.name),
                    binary.executable.as_os_str().to_owned(),
                );
            }
        }
        let directory = program.executable.parent().context("runtime artifact has no parent")?;
        let internal = directory.file_name().is_some_and(|name| {
            name == "deps"
                || name == "examples"
                || name.to_string_lossy().starts_with("unit-")
                || name.to_string_lossy().starts_with("doctest-")
        });
        let profile = if internal {
            directory.parent().context("runtime profile has no parent")?
        } else {
            directory
        };
        let mut inherited = env.clone();
        inherited.extend(run.env.iter().map(|(name, value)| (name.clone(), value.into())));
        let runtime_env = runtime::environment(
            &runtime_unit,
            &inherited,
            profile,
            &sysroot.join("lib/rustlib").join(&prepared.host.triple).join("lib"),
            &prepared.host.triple,
        )?;
        if let Some(tests) =
            tests.filter(|_| program.output != blaze_spec::execution::Output::Doctest)
        {
            let relative = root.target.src_path.strip_prefix(&root.package.manifest_dir)?;
            let prefix = if matches!(
                root.target.kind,
                blaze_metadata::units::TargetKind::Lib | blaze_metadata::units::TargetKind::Bin
            ) {
                "unittests "
            } else {
                ""
            };
            let label = format!("{prefix}{}", relative.display());
            let binaries: Vec<_> = runtime_unit
                .env
                .iter()
                .filter(|(name, _)| name.starts_with("CARGO_BIN_EXE_"))
                .map(|(name, path)| (name.clone(), PathBuf::from(path)))
                .collect();
            // A native command contains its whole environment. Only Cargo/configuration
            // overlays are authoritative inside a test sandbox; inherited values retain
            // the same declared/kept filtering as the daemon runner.
            let mut sandbox_env = BTreeMap::new();
            for name in given
                .config_env
                .iter()
                .map(|(name, _)| name)
                .chain(run.env.iter().map(|(name, _)| name))
                .chain(runtime_unit.env.keys())
            {
                if let Some(value) = runtime_env.get(name) {
                    sandbox_env.insert(name.clone(), value.clone());
                }
            }
            let loader = super::loader::variable(&prepared.host.triple);
            sandbox_env.insert(loader.into(), runtime_env[loader].clone());
            let result = tests.run(
                program,
                &given.units[program.unit as usize],
                &root.package.name,
                &root.package.manifest_dir,
                &label,
                &binaries,
                &runtime_env,
                &sandbox_env,
                |executable| {
                    let mut command = Command::new(executable);
                    command.env_clear().envs(&runtime_env).current_dir(&root.package.manifest_dir);
                    Ok(command)
                },
            )?;
            left += result.left;
            always_run += result.always_run;
            if !result.success {
                failed = 101;
                if !run.no_fail_fast {
                    break;
                }
            }
            continue;
        }
        let mut command = if program.output == blaze_spec::execution::Output::Doctest {
            let mut command = Command::new(std::env::current_exe()?);
            let environment = compiler_for(program, prepared, producers, compiler_env, sysroot)?;
            command.env_clear().envs(&environment);
            let variable = super::loader::variable(&prepared.host.triple);
            command.env(variable, &runtime_env[variable]);
            command.args(["cargo", "spec", "--compiler-doctest"]);
            command.arg(program.executable.parent().unwrap().join("rust-project.json"));
            command.arg(&program.crate_name);
            command
        } else {
            let mut command = Command::new(&program.executable);
            command.env_clear().envs(&runtime_env);
            command
        };
        if let blaze_wire::Command::Test(_) = &given.command {
            if program.output != blaze_spec::execution::Output::Doctest {
                command.args(&run.filters);
            }
            command.current_dir(&native[program.unit as usize].package.manifest_dir);
        } else {
            command.current_dir(&prepared.cwd);
        }
        if program.output != blaze_spec::execution::Output::Doctest {
            command.args(&run.harness_args);
        }
        let code = if program.output == blaze_spec::execution::Output::Doctest {
            let project = program.executable.parent().unwrap().join("rust-project.json");
            let mut description = program.project.clone();
            runtime::install_environment(&mut description, &runtime_env);
            let described = crate::compiler_json::write(&project, &description)?;
            command.env("RA_JIT_PROJECT_STDIN", "1").stdin(Stdio::piped());
            let child = command.spawn()?;
            let output =
                crate::child::feed(child, &described).context("sending owned doctest project")?;
            super::run::exit_code(output.status)
        } else {
            super::run::exit_code(command.status()?)
        };
        if code != 0 {
            failed = if matches!(given.command, blaze_wire::Command::Test(_)) { 101 } else { code };
            if !run.no_fail_fast {
                break;
            }
        }
    }
    if let Some(tests) = tests {
        super::run::test_summary(&tests.asked, left, always_run);
    }
    Ok(failed)
}

fn toolchain_native_inputs(sysroot: &Path) -> anyhow::Result<blaze_spec::native::Closure> {
    let mut inputs = blaze_spec::native::Closure::default();
    // The compiler reads std sources, native libraries and installed toolchain records.
    // Nix's composed sysroot also carries the complete Rust HTML documentation; binding
    // that tree makes unrelated pages inputs to every producer and every native link.
    for (name, relative) in
        [("libraries", "lib"), ("version", "share/doc/rust/html/version_info.html")]
    {
        inputs.roots.push(blaze_spec::native::Root {
            name: format!("toolchain/{name}"),
            tree: blaze_spec::native_tree::Tree::capture(&sysroot.join(relative))?,
        });
    }
    Ok(inputs)
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
    // The ambient cache is not service policy: even std's individual functions must
    // originate in this replay. Only its private std-unit directory is shared.
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

fn compiler_for(
    program: &blaze_spec::execution::Program,
    prepared: &blaze_spec::replay::Prepared,
    producers: &producers::Producers,
    inherited: &BTreeMap<String, OsString>,
    sysroot: &Path,
) -> anyhow::Result<BTreeMap<String, OsString>> {
    let libraries = producers
        .compiler_libraries
        .get(&program.unit)
        .context("root has no host library context")?;
    let mut inherited = inherited.clone();
    inherited.insert("CARGO".into(), producers.cargo.as_os_str().to_owned());
    if libraries.is_empty() {
        return Ok(inherited);
    }
    let profile = program.executable.parent().context("compiler artifact has no directory")?;
    runtime::environment(
        &runtime::Unit { env: BTreeMap::new(), libraries: libraries.clone() },
        &inherited,
        profile,
        &sysroot.join("lib/rustlib").join(&prepared.host.triple).join("lib"),
        &prepared.host.triple,
    )
}

fn compile(
    program: &mut blaze_spec::execution::Program,
    env: &BTreeMap<String, OsString>,
    reach: bool,
) -> anyhow::Result<()> {
    compile_at(program, env, None, reach)
}

#[derive(Debug)]
struct CompilerFailure(crate::errors::Said);

impl std::fmt::Display for CompilerFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}", self.0)
    }
}

impl std::error::Error for CompilerFailure {}

fn compiler_diagnostics(directory: &Path, stderr: &str) -> anyhow::Result<crate::errors::Said> {
    match std::fs::read(directory.join("diagnostics.json")) {
        Ok(bytes) => {
            serde_json::from_slice(&bytes).context("invalid owned compiler diagnostic receipt")
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let message =
                stderr.rsplit_once("Error: ").map_or(stderr, |(_, message)| message).trim();
            Ok(crate::errors::Said::one(crate::errors::of_none(message)))
        }
        Err(error) => Err(error).context("reading owned compiler diagnostic receipt"),
    }
}

fn compile_at(
    program: &mut blaze_spec::execution::Program,
    env: &BTreeMap<String, OsString>,
    cwd: Option<&Path>,
    reach: bool,
) -> anyhow::Result<()> {
    let dir = program.executable.parent().context("artifact has no directory")?;
    std::fs::create_dir_all(dir)?;
    // Reach metadata must describe this compilation, never a previously built artifact.
    for name in [crate::cargo::plan::REACH, crate::cargo::plan::REACH_WANTED] {
        match std::fs::remove_file(dir.join(name)) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    if reach {
        std::fs::write(dir.join(crate::cargo::plan::REACH_WANTED), b"1")?;
    }
    let project = dir.join("rust-project.json");
    let described = crate::compiler_json::write(&project, &program.project)?;
    program.project = serde_json::from_slice(&described)?;
    let mut command = Command::new(std::env::current_exe()?);
    command
        .env_clear()
        .envs(env)
        .current_dir(cwd.unwrap_or(dir))
        .env("RA_JIT_PROJECT_STDIN", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    match program.output {
        blaze_spec::execution::Output::Check | blaze_spec::execution::Output::Doctest => {
            command.args(["cargo", "spec", "--compiler-check"]).arg(&project);
            command.arg(&program.crate_name).arg(&program.executable).arg(dir.join("inputs.cbor"));
        }
        output => {
            command.arg(&project).args(["--crate", &program.crate_name]);
            command.arg("--out").arg(if output == blaze_spec::execution::Output::Object {
                program.executable.clone()
            } else {
                dir.join("compiled.o")
            });
            if output == blaze_spec::execution::Output::Executable {
                command.arg("--exe").arg(&program.executable);
            } else {
                command.arg("--link-plan").arg(dir.join("link.cbor"));
            }
            command.arg("--inputs").arg(dir.join("inputs.cbor"));
            command.arg("--diagnostics").arg(dir.join("diagnostics"));
        }
    }
    if program.harness && program.output == blaze_spec::execution::Output::Executable {
        // The prebuilt std unit excludes libtest. Compile that runtime with the
        // harness, selecting its source even when the application is named `test`.
        let rendered: serde_json::Value = serde_json::from_slice(&described)?;
        let library = rendered["sysroot_src"]
            .as_str()
            .context("test compiler description has no sysroot sources")?;
        command.args(["--cfg-test", "--crate"]).arg(Path::new(library).join("test/src/lib.rs"));
    }
    let child = command.spawn().context("starting the owned replay compiler")?;
    let output = crate::child::feed(child, &described).context("sending owned project")?;
    std::fs::write(dir.join("compiler.log"), &output.stderr)?;
    let said = String::from_utf8_lossy(&output.stderr);
    if !output.status.success() {
        let diagnostics = compiler_diagnostics(&dir.join("diagnostics"), &said)
            .with_context(|| format!("owned compiler failed:\n{said}"))?;
        return Err(CompilerFailure(diagnostics).into());
    }
    ensure!(
        !said.lines().any(|line| line.contains(" functions in it ra-jit could not compile")
            && !line.ends_with(", 0 functions in it ra-jit could not compile")),
        "owned replay compiler left uncompiled functions in {}: {said}",
        program.crate_name
    );
    ensure!(
        program.executable.is_file(),
        "owned compiler did not write {}",
        program.executable.display()
    );
    Ok(())
}

/// Whether a variable the compiler found unset is unset as the requester had it. The compiler
/// runs with the spec's environment alone, so an `option_env!` of a name the spec does not
/// carry reads absent whatever the worker has; that is Cargo's answer exactly when the
/// requester's process lacked the name too. One it had, which the spec does not carry (a
/// secret, say), would be answered `None` where Cargo answered the requester's value.
/// Cargo derives the three names of its own per crate.
fn absent_as_saved(name: &str, present: bool, requester_had: impl Fn(&str) -> bool) -> bool {
    !present
        && (matches!(name, "CARGO_PRIMARY_PACKAGE" | "CARGO_BIN_NAME" | "OUT_DIR")
            || !requester_had(name))
}

/// The complete non-language dependency trace includes modules and all include macros.
/// Admission happens before any executable is run or advertised. This is a correctness
/// check after compilation, not a filesystem sandbox or a claim of pre-read confinement.
fn admit_inputs(program: &blaze_spec::execution::Program) -> anyhow::Result<()> {
    let dir = program.executable.parent().context("artifact has no directory")?;
    let trace = std::fs::read(dir.join("inputs.cbor"))
        .context("owned compiler did not report its complete source inputs")?;
    let inputs = crate::dep_info::ReplayTrace::decode(&trace)?;
    admit_trace(&inputs, &program.project)
}

pub(crate) fn admit_trace(
    inputs: &crate::dep_info::ReplayTrace,
    project: &serde_json::Value,
) -> anyhow::Result<()> {
    if project.get("blaze_direct_execution").is_some() {
        return direct::admit_trace(inputs, project);
    }
    let bound = &project["blaze_spec_inputs"];
    let root = Path::new(bound["root"].as_str().context("no admitted source root")?);
    let store = Path::new(bound["store"].as_str().context("no admitted source store")?);
    let root = crate::compiler_json::physical_path(project, root)?;
    let store = crate::compiler_json::physical_path(project, store)?;
    let source: [u8; 32] = serde_json::from_value(bound["source"].clone())?;
    ensure!(!inputs.files.is_empty(), "owned compiler reported an empty source trace");
    let root = root.canonicalize()?;
    let generated = match project.get("blaze_spec_generated_bindings") {
        Some(value) => {
            let bytes: Vec<u8> = serde_json::from_value(value.clone())
                .map_err(|_| anyhow::anyhow!("invalid generated input bindings"))?;
            let closure = blaze_spec::native::Closure::from_bindings(&bytes)?;
            ensure!(
                closure.roots.iter().all(|root| root.name.starts_with("producer/out/")
                    || root.name.starts_with("producer/image/")),
                "unexpected generated input binding role"
            );
            closure
        }
        None => blaze_spec::native::Closure::default(),
    };
    let admission = generated.admission();
    for path in &inputs.files {
        let physical = crate::compiler_json::physical_path(project, Path::new(path))?;
        let path = physical.canonicalize()?;
        if let Ok(relative) = path.strip_prefix(&root) {
            let logical = blaze_wire::SourcePath::from_native(relative)?;
            let captured = blaze_spec::snapshot::Snapshot::read_file(source, &store, &logical)?;
            ensure!(
                std::fs::read(&path)? == captured,
                "compiler input {} changed after snapshot validation",
                path.display()
            );
        } else {
            let observed = blaze_spec::native_tree::Tree::capture(&physical)?;
            let entry =
                observed.entry(Path::new("")).context("generated input has no root entry")?;
            ensure!(
                matches!(entry.kind, blaze_spec::native_tree::Kind::File { .. }),
                "compiler input {} is not a generated file",
                physical.display()
            );
            admission.admit(&physical, entry).with_context(|| {
                format!(
                    "compiler read {} outside authenticated source and producer inputs",
                    physical.display()
                )
            })?;
        }
    }
    drop(admission);
    generated.validate()?;
    let environments = crate::compiler_json::source_environments(project)?;
    let trace_key = crate::dep_info::replay_key(project)?;
    let requester: Option<std::collections::BTreeSet<blaze_wire::Digest>> = project
        .get("blaze_spec_requester_names")
        .map(|names| serde_json::from_value(names.clone()))
        .transpose()
        .context("invalid saved requester environment names")?;
    for (name, value) in &inputs.env {
        let declared = value.is_some()
            && environments.iter().any(|env| {
                env.get(std::ffi::OsStr::new(name)).is_some_and(|declared| {
                    crate::dep_info::ReplayTrace::value_matches(
                        value.as_ref(),
                        Some(declared),
                        trace_key,
                    )
                })
            });
        let absent =
            absent_as_saved(name, value.is_some(), |name| {
                // A descriptor without authenticated requester presence cannot prove
                // arbitrary absence. Keep legacy/test descriptors conservative.
                let identity: blaze_wire::Digest = Sha256::digest(name.as_bytes()).into();
                requester.as_ref().is_none_or(|names| names.contains(&identity))
            }) && (matches!(name.as_str(), "CARGO_PRIMARY_PACKAGE" | "CARGO_BIN_NAME" | "OUT_DIR")
                || environments.iter().any(|env| !env.contains_key(std::ffi::OsStr::new(name))));
        ensure!(declared || absent, "compiler read undeclared environment value {name}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn std_units_are_shared_only_inside_one_replay_and_for_the_same_key() {
        let work = tempfile::tempdir().unwrap();
        let first = work.path().join("first");
        let second = work.path().join("second");
        std::fs::create_dir(&first).unwrap();
        std::fs::create_dir(&second).unwrap();
        let project = work.path().join("rust-project.json");
        let mut description = serde_json::json!({"compiler_mode": true, "crates": []});
        crate::compiler_json::write(&project, &description).unwrap();
        assert!(std_unit_cache(&project, &[1; 32]).unwrap().is_none());
        assert!(std_unit_cache(Path::new("lib.rs"), &[1; 32]).unwrap().is_none());
        description["blaze_spec_std_units"] = serde_json::json!({
            "version": 1, "directory": crate::compiler_json::path(&first),
        });
        crate::compiler_json::write(&project, &description).unwrap();
        let unit = std_unit_cache(&project, &[1; 32]).unwrap().unwrap();
        std::fs::write(&unit, b"this replay's std").unwrap();
        assert_eq!(
            std::fs::read(std_unit_cache(&project, &[1; 32]).unwrap().unwrap()).unwrap(),
            b"this replay's std"
        );
        assert!(!std_unit_cache(&project, &[2; 32]).unwrap().unwrap().exists());
        description["blaze_spec_std_units"]["directory"] = crate::compiler_json::path(&second);
        crate::compiler_json::write(&project, &description).unwrap();
        assert!(!std_unit_cache(&project, &[1; 32]).unwrap().unwrap().exists());
        for bound in [
            serde_json::json!({"version": 2, "directory": first}),
            serde_json::json!({"version": 1, "directory": "relative"}),
            serde_json::json!({"version": 1, "directory": work.path().join("missing")}),
            serde_json::json!({"version": 1}),
        ] {
            description["blaze_spec_std_units"] = bound;
            crate::compiler_json::write(&project, &description).unwrap();
            assert!(std_unit_cache(&project, &[1; 32]).is_err());
        }
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn replay_std_unit_directory_preserves_native_paths() {
        use std::os::unix::ffi::OsStringExt as _;
        let work = tempfile::tempdir().unwrap();
        let directory = work.path().join(OsString::from_vec(b"std-\xff".to_vec()));
        std::fs::create_dir(&directory).unwrap();
        let project = work.path().join("rust-project.json");
        let description = serde_json::json!({"compiler_mode": true, "crates": [],
            "blaze_spec_std_units": {"version": 1, "directory": crate::compiler_json::path(&directory)}});
        crate::compiler_json::write(&project, &description).unwrap();
        assert_eq!(
            std_unit_cache(&project, &[3; 32]).unwrap().unwrap(),
            directory.join(hex(&[3; 32]))
        );
    }

    #[test]
    fn toolchain_native_inputs_bind_libraries_and_records_without_unrelated_docs() {
        let dir = tempfile::tempdir().unwrap();
        let library = dir.path().join("lib/rustlib/src/rust/library/core/src/lib.rs");
        let native = dir.path().join("lib/rustlib/aarch64-apple-darwin/lib/libstd.dylib");
        let info = dir.path().join("share/doc/rust/html/version_info.html");
        let docs = dir.path().join("share/doc/rust/html/core/index.html");
        for path in [&library, &native, &info, &docs] {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, b"before").unwrap();
        }
        let inputs = toolchain_native_inputs(dir.path()).unwrap();
        inputs.validate().unwrap();
        std::fs::write(&docs, b"unrelated documentation changed").unwrap();
        inputs.validate().expect("Rust HTML documentation is not a compiler or linker input");
        for path in [&library, &native, &info] {
            std::fs::write(path, b"changed").unwrap();
            assert!(inputs.validate().is_err(), "{} must be authenticated", path.display());
            std::fs::write(path, b"before").unwrap();
            inputs.validate().unwrap();
        }
    }

    // APFS rejects these filename bytes before source admission can be exercised.
    #[test]
    #[cfg(target_os = "linux")]
    fn saved_input_admission_preserves_native_source_and_store_paths() {
        use std::os::unix::ffi::OsStringExt as _;

        let work = tempfile::tempdir().unwrap();
        let source = work.path().join(OsString::from_vec(b"source-\xff".to_vec()));
        let store = work.path().join(OsString::from_vec(b"inputs-\xfe".to_vec()));
        std::fs::create_dir(&source).unwrap();
        let file = source.join("lib.rs");
        let contents = b"pub fn answer() -> u8 { 42 }";
        std::fs::write(&file, contents).unwrap();
        let snapshot = blaze_spec::snapshot::Snapshot::capture(&source, &[], &[], &store).unwrap();
        let mut project = serde_json::json!({
            "blaze_spec_inputs": {
                "root": crate::compiler_json::path(&source),
                "store": crate::compiler_json::path(&store),
                "source": snapshot.digest(),
            },
            "crates": [{"root_module": crate::compiler_json::path(&file), "env": {}}],
        });
        crate::dep_info::ensure_replay_key(&mut project).unwrap();
        let rendered =
            crate::compiler_json::write(&work.path().join("rust-project.json"), &project).unwrap();
        let project: serde_json::Value = serde_json::from_slice(&rendered).unwrap();
        let trace = crate::dep_info::ReplayTrace {
            files: vec![project["crates"][0]["root_module"].as_str().unwrap().to_owned()],
            env: Vec::new(),
        };
        admit_trace(&trace, &project).unwrap();

        std::fs::write(&file, b"pub fn answer() -> u8 { 43 }").unwrap();
        assert!(admit_trace(&trace, &project).is_err());
        std::fs::write(&file, contents).unwrap();
        admit_trace(&trace, &project).unwrap();
        std::fs::write(store.join(hex(&snapshot.digest())), b"corrupt manifest").unwrap();
        assert!(admit_trace(&trace, &project).is_err());
    }

    #[test]
    fn compiler_failure_receipts_preserve_diagnostics_and_reject_corruption() {
        let work = tempfile::tempdir().unwrap();
        let diagnostic = crate::errors::Diagnostic {
            code: Some("E0308".into()),
            message: "expected u32, found bool".into(),
            span: None,
        };
        let said = crate::errors::Said::one(diagnostic);
        std::fs::write(work.path().join("diagnostics.json"), serde_json::to_vec(&said).unwrap())
            .unwrap();
        assert_eq!(compiler_diagnostics(work.path(), "unrelated compiler progress").unwrap(), said);
        std::fs::write(work.path().join("diagnostics.json"), b"{truncated").unwrap();
        assert!(compiler_diagnostics(work.path(), "error: replacement text").is_err());
        std::fs::remove_file(work.path().join("diagnostics.json")).unwrap();
        let failure =
            compiler_diagnostics(work.path(), "progress\nError: native linker could not start\n")
                .unwrap();
        assert_eq!(failure.0[0].code, None);
        assert_eq!(failure.0[0].message, "native linker could not start");
    }

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
    fn generated_source_admission_requires_the_complete_unchanged_bound_tree() {
        use blaze_spec::{
            native::{Closure, Root},
            native_tree::Tree,
        };
        let work = tempfile::tempdir().unwrap();
        let source = work.path().join("source");
        let generated = work.path().join("generated");
        std::fs::create_dir_all(&source).unwrap();
        std::fs::create_dir_all(&generated).unwrap();
        let file = generated.join("generated.rs");
        std::fs::write(&file, "pub const VALUE: u8 = 42;").unwrap();
        let closure = Closure {
            roots: vec![Root {
                name: "producer/out/0".into(),
                tree: Tree::capture(&generated).unwrap(),
            }],
        };
        let mut project = serde_json::json!({
            "blaze_spec_inputs": {"root": source, "store": work.path(), "source": vec![0_u8; 32]},
            "blaze_spec_generated_bindings": closure.encode_bindings(), "crates": [],
        });
        crate::dep_info::ensure_replay_key(&mut project).unwrap();
        let trace = crate::dep_info::ReplayTrace {
            files: vec![file.to_str().unwrap().to_owned()],
            env: Vec::new(),
        };
        admit_trace(&trace, &project).unwrap();
        project.as_object_mut().unwrap().remove("blaze_spec_generated_bindings");
        assert!(admit_trace(&trace, &project).is_err());
        project["blaze_spec_generated_bindings"] = serde_json::json!(closure.encode_bindings());
        std::fs::write(generated.join("later.rs"), "unexpected membership").unwrap();
        assert!(admit_trace(&trace, &project).is_err());
        std::fs::remove_file(generated.join("later.rs")).unwrap();
        std::fs::write(&file, "pub const VALUE: u8 = 43;").unwrap();
        assert!(admit_trace(&trace, &project).is_err());
    }

    #[test]
    #[cfg(unix)]
    fn saved_raw_reads_require_referenced_frontend_markers() {
        use blaze_spec::{
            native::{Closure, Root},
            native_tree::Tree,
        };
        use std::{ffi::OsString, os::unix::ffi::OsStringExt as _};
        let work = tempfile::tempdir().unwrap();
        let source = work.path().join("source");
        let file = work.path().join("generated.rs");
        std::fs::create_dir(&source).unwrap();
        std::fs::write(&file, "const VALUE: u8 = 42;").unwrap();
        let closure = Closure {
            roots: vec![Root {
                name: "producer/out/0".into(),
                tree: Tree::capture(&file).unwrap(),
            }],
        };
        let units: std::collections::BTreeMap<_, _> = [0xff, 0xfe]
            .into_iter()
            .enumerate()
            .map(|(id, byte)| {
                (
                    id as u32,
                    blaze_wire::native::Unit {
                        env: [
                            (
                                "RAW".into(),
                                OsString::from_vec([b"native-value-".as_slice(), &[byte]].concat()),
                            ),
                            (
                                "UNPROJECTED".into(),
                                OsString::from_vec(b"native-value-\xff".to_vec()),
                            ),
                        ]
                        .into(),
                        flags: Vec::new(),
                    },
                )
            })
            .collect();
        let mut project = serde_json::json!({
            "blaze_spec_inputs": {"root": source, "store": work.path(), "source": vec![0_u8;32]},
            "blaze_spec_generated_bindings": closure.encode_bindings(),
            "blaze_spec_requester_names": [],
            crate::dep_info::TRACE_KEY_FIELD: vec![7_u8;32],
            blaze_wire::native::UNIT_FIELD: blaze_wire::native::encode_units(&units),
            "crates": [
                {"env": {}, "blaze_invalid_env": ["RAW"], "blaze_native_unit": 0},
                {"env": {}, "blaze_invalid_env": ["RAW"], "blaze_native_unit": 1},
            ],
        });
        let trace = crate::dep_info::ReplayTrace {
            files: vec![file.to_str().unwrap().into()],
            env: vec![
                (
                    "RAW".into(),
                    Some([
                        219, 147, 173, 155, 170, 214, 121, 78, 123, 253, 55, 204, 97, 28, 214, 8,
                        17, 48, 54, 203, 16, 74, 122, 109, 161, 114, 210, 32, 32, 74, 1, 79,
                    ]),
                ),
                (
                    "RAW".into(),
                    Some([
                        197, 210, 32, 124, 230, 204, 50, 112, 113, 76, 18, 26, 100, 74, 181, 84,
                        71, 104, 99, 90, 210, 192, 83, 54, 90, 95, 158, 143, 103, 214, 27, 117,
                    ]),
                ),
            ],
        };
        admit_trace(&trace, &project).unwrap();
        let mut unreferenced = project.clone();
        unreferenced["crates"].as_array_mut().unwrap().pop();
        assert!(admit_trace(&trace, &unreferenced).is_err());
        let mut wrong = project.clone();
        wrong["crates"][1]["blaze_native_unit"] = serde_json::json!(0);
        assert!(admit_trace(&trace, &wrong).is_err());
        wrong["crates"][1]["blaze_native_unit"] = serde_json::json!(99);
        assert!(admit_trace(&trace, &wrong).is_err());
        let mut missing = project.clone();
        missing["crates"][0]["blaze_invalid_env"] = serde_json::json!([]);
        assert!(admit_trace(&trace, &missing).is_err());
        let mut forged =
            crate::dep_info::ReplayTrace { files: trace.files.clone(), env: Vec::new() };
        forged.env = vec![(
            "UNPROJECTED".into(),
            Some([
                219, 147, 173, 155, 170, 214, 121, 78, 123, 253, 55, 204, 97, 28, 214, 8, 17, 48,
                54, 203, 16, 74, 122, 109, 161, 114, 210, 32, 32, 74, 1, 79,
            ]),
        )];
        assert!(admit_trace(&forged, &project).is_err());
        forged.env = vec![("RAW".into(), None)];
        assert!(admit_trace(&forged, &project).is_err());
        project["crates"].as_array_mut().unwrap().push(serde_json::json!({"env": {}}));
        admit_trace(&forged, &project).unwrap();
        let mut key = project.clone();
        key[crate::dep_info::TRACE_KEY_FIELD] = serde_json::json!(vec![8_u8; 32]);
        assert!(admit_trace(&trace, &key).is_err());
    }

    #[test]
    fn requester_absence_and_keyed_values_share_saved_admission() {
        use blaze_spec::{
            native::{Closure, Root},
            native_tree::Tree,
        };
        let work = tempfile::tempdir().unwrap();
        let source = work.path().join("source");
        let generated = work.path().join("generated.rs");
        std::fs::create_dir(&source).unwrap();
        std::fs::write(&generated, "const VALUE: u8 = 42;").unwrap();
        let closure = Closure {
            roots: vec![Root {
                name: "producer/out/0".into(),
                tree: Tree::capture(&generated).unwrap(),
            }],
        };
        let secret: blaze_wire::Digest = Sha256::digest(b"SECRET").into();
        let mut project = serde_json::json!({
            "blaze_spec_inputs": {"root": source, "store": work.path(), "source": vec![0_u8; 32]},
            "blaze_spec_generated_bindings": closure.encode_bindings(),
            "blaze_spec_requester_names": [secret],
            "crates": [{"env": {"CAPTURED": "saved-value"}}],
        });
        crate::dep_info::ensure_replay_key(&mut project).unwrap();
        let inputs = |name: &str, value: Option<&str>| {
            crate::dep_info::ReplayTrace::from_inputs(
                &crate::dep_info::DepInfo {
                    files: vec![generated.to_str().unwrap().into()],
                    env: vec![(name.into(), value.map(str::to_owned))],
                },
                crate::dep_info::replay_key(&project).unwrap(),
            )
        };
        let absent = inputs("NEVER_SET", None);
        let present = inputs("CAPTURED", Some("saved-value"));
        admit_trace(&absent, &project).unwrap();
        admit_trace(&present, &project).unwrap();
        assert!(admit_trace(&inputs("SECRET", None), &project).is_err());
        assert!(admit_trace(&inputs("CAPTURED", None), &project).is_err());
        assert!(admit_trace(&inputs("CAPTURED", Some("worker-value")), &project).is_err());
        assert!(admit_trace(&inputs("NEVER_SET", Some("worker-value")), &project).is_err());
        let mut mixed = project.clone();
        mixed["crates"] = serde_json::json!([{"env": {"CAPTURED": "saved-value"}}, {"env": {}}]);
        admit_trace(&inputs("CAPTURED", None), &mixed).unwrap();
        mixed["blaze_spec_requester_names"] =
            serde_json::json!([Sha256::digest(b"CAPTURED").to_vec()]);
        assert!(admit_trace(&inputs("CAPTURED", None), &mixed).is_err());
        for name in ["OUT_DIR", "CARGO_PRIMARY_PACKAGE", "CARGO_BIN_NAME"] {
            mixed["crates"] = serde_json::json!([{"env": {(name): "derived"}}]);
            mixed["blaze_spec_requester_names"] =
                serde_json::json!([Sha256::digest(name.as_bytes()).to_vec()]);
            admit_trace(&inputs(name, None), &mixed).unwrap();
        }
        // A captured value remains tied to the descriptor's private trace key.
        let mut altered = project.clone();
        altered[crate::dep_info::TRACE_KEY_FIELD] = serde_json::json!([9_u8; 32].to_vec());
        assert!(admit_trace(&present, &altered).is_err());
        altered["blaze_spec_requester_names"] = serde_json::json!([[0, 1]]);
        assert!(admit_trace(&absent, &altered).is_err());
        altered.as_object_mut().unwrap().remove("blaze_spec_requester_names");
        assert!(admit_trace(&absent, &altered).is_err());
    }

    #[test]
    fn an_unset_variable_reads_as_the_requester_had_it() {
        let had = |name: &str| name == "SECRET";
        // Unset for the requester and for the compiler: Cargo's answer, whatever the worker has.
        assert!(absent_as_saved("NEVER_SET", false, had));
        assert!(absent_as_saved("OUT_DIR", false, |_| true));
        // The requester had it and the spec does not carry it: not reproducible.
        assert!(!absent_as_saved("SECRET", false, had));
        // A value the compiler found is declared by the spec or refused, never absent.
        assert!(!absent_as_saved("NEVER_SET", true, had));
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
