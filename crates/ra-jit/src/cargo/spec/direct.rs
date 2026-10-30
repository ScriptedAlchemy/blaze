//! Ordinary Cargo commands use the original source graph and process environment.
//! The saved executor supplies the compiler and producer machinery; no snapshot is
//! materialized and no live input is represented as authenticated replay evidence.

use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::OsString,
    path::{Path, PathBuf},
};

use anyhow::{Context as _, ensure};
use blaze_metadata::units::TargetKind;
use blaze_spec::{
    assemble::Context,
    execution::Output,
    flags::{Request, Subcommand},
    replay::Prepared,
};
use blaze_wire::{RootRef, SourceRef};
use sha2::{Digest as _, Sha256};

use super::super::cli::{Invocation, Subcommand as CargoCommand};

pub(in crate::cargo) fn command(asked: &Invocation) -> anyhow::Result<i32> {
    let format = match super::super::messages::Format::of(&asked.flags.message_format) {
        Ok(format) => format,
        Err(error) => {
            eprintln!("error: {error}");
            return Ok(101);
        }
    };
    match build(asked, format) {
        Ok(code) => Ok(code),
        Err(error) => {
            if error.downcast_ref::<super::RuntimeFailure>().is_some() {
                eprintln!("error: {error:#}");
                return Ok(101);
            }
            let said = error
                .downcast_ref::<super::CompilerFailure>()
                .map(|failure| failure.0.clone())
                .unwrap_or_else(|| {
                    crate::errors::Said::one(crate::errors::of_none(&format!("{error:#}")))
                });
            let cwd = std::env::current_dir()?;
            let env = blaze_spec::environment::native_process()
                .into_iter()
                .filter_map(|(name, value)| Some((name, value.into_string().ok()?)))
                .collect();
            match super::super::metadata::no_deps_in(
                &cwd,
                asked.flags.manifest_path.as_deref(),
                &asked.flags.config,
                env,
            ) {
                Ok(metadata) => super::super::messages::turned_down(format, &[said], &metadata),
                Err(_) => {
                    eprintln!("{said}");
                    if format.is_json() {
                        println!("{{\"reason\":\"build-finished\",\"success\":false}}");
                    }
                }
            }
            Ok(101)
        }
    }
}

fn build(asked: &Invocation, format: super::super::messages::Format) -> anyhow::Result<i32> {
    let command = match asked.subcommand {
        CargoCommand::Build => Subcommand::Build,
        CargoCommand::Run => Subcommand::Run,
        CargoCommand::Test => Subcommand::Test,
        CargoCommand::Check => Subcommand::Check,
    };
    let cwd = std::env::current_dir()?;
    let original = blaze_spec::environment::native_process();
    let options = Options::read(&asked.typed)?;
    let mut planning = planning_environment(&original);
    if let Some(path) = &options.target_dir {
        planning.insert("CARGO_TARGET_DIR".into(), cwd.join(path).into_os_string());
    }
    let text = blaze_spec::environment::text_view(&planning)?;
    let (host, identity, sysroot) = super::host()?;
    if let Some(target) = &options.target {
        ensure!(
            target == &host.triple,
            "owned direct cross-target execution is not implemented for {target}"
        );
    }
    let request = Request::parse(command, &options.words)?;
    let root = cwd.ancestors().last().context("working directory has no filesystem root")?;
    let resolved = blaze_spec::assemble::resolve_live(
        &request,
        &Context {
            source_root: Some(root),
            config_root: None,
            config_image: None,
            cwd: &cwd,
            env: &text,
            native_env: Some(&planning),
            host: &host,
            source: SourceRef::Root(RootRef { root: [0; 32] }),
            toolchain: identity,
            offline: false,
            store: None,
            probe: &blaze_spec::platform::System,
            rust_release: Some(&crate::answers::rust_release),
        },
    )?;
    let target = resolved.outputs.first().context("resolution has no target directory")?.clone();
    let prepared =
        Prepared::from_live(resolved, root.to_owned(), cwd.clone(), request, planning, host)?;
    // These values are live request inputs, not a portable spec's fixed environment list.
    // Producer and per-crate overlays remain authoritative over the caller's values.
    let mut effective = original.clone();
    effective.extend(prepared.execution_environment()?);
    let work = target.join("ra-jit/direct");
    std::fs::create_dir_all(&work)?;
    let work = tempfile::Builder::new().prefix("build-").tempdir_in(work)?;
    let home = work.path().join("compiler-home");
    std::fs::create_dir(&home)?;
    let mut compiler_env = super::compiler_environment(&prepared, &home, &original)?;
    compiler_env.remove("RA_JIT_SPEC_REPLAY");
    let has_macros =
        prepared.resolved.graph.units.iter().any(|unit| unit.target.kind == TargetKind::ProcMacro);
    let runtime = if has_macros {
        Some(
            super::runtime::pin(&original, &work.path().join("runtime"))?
                .context("source proc macros require an explicit owned macro runtime")?,
        )
    } else {
        None
    };
    let cargo = cargo_frontend(&target, identity, &original)?;
    let producers = super::producers::prepare_live(
        &prepared,
        &sysroot,
        &work.path().join("producers"),
        crate::process::Native::of(
            compiler_env.iter().map(|(name, value)| (name.into(), value.clone())),
        ),
        runtime.as_ref(),
        &effective,
        &cargo,
    )?;
    // Cache service credentials and policy are compiler controls. Add them only
    // after producer recipes/descriptions are prepared; saved replay never opts in.
    super::super::cache_service::configure(&mut compiler_env, &original, &cwd)?;
    let mut programs = blaze_spec::execution::programs_with(
        &prepared,
        &sysroot,
        &work.path().join("artifacts"),
        Some(&producers.units),
    )?;
    let native = blaze_spec::assemble::canonical_units(
        &prepared.resolved.spec.build,
        &prepared.resolved.graph,
    )?;
    let tests = super::test_policy::Policy::new(
        &prepared,
        &target.join("ra-jit/test-results"),
        &effective,
        &programs,
        Some(asked),
    )?;
    let destinations: Vec<_> = programs
        .iter()
        .map(|program| {
            let unit = native[program.unit as usize];
            destination(
                &target,
                options.target.as_deref(),
                &unit.profile.name,
                unit.target.kind,
                &unit.target.name,
                unit.test,
                program.output,
                &super::hex(&blaze_wire::digest_of(
                    &prepared.resolved.spec.build.units[program.unit as usize],
                ))[..16],
            )
        })
        .collect();
    let binaries: BTreeMap<_, _> = programs
        .iter()
        .zip(&destinations)
        .filter_map(|(program, destination)| {
            let unit = native[program.unit as usize];
            (!unit.test && unit.target.kind == TargetKind::Bin).then(|| {
                (format!("CARGO_BIN_EXE_{}", unit.target.name), destination.as_os_str().to_owned())
            })
        })
        .collect();
    for program in &mut programs {
        super::source_naming(program, &prepared)?;
        bind_binaries(&mut program.project, &binaries, &producers.units)?;
        crate::dep_info::ensure_replay_key(&mut program.project)?;
        program.project["blaze_direct_execution"] = serde_json::json!(1);
        if let Some(runtime) = &runtime {
            program.project[super::runtime::FIELD] = runtime.clone();
        }
        let environment =
            super::compiler_for(program, &prepared, &producers, &compiler_env, &sysroot)?;
        let reach = tests.as_ref().is_some_and(|tests| tests.reach(program));
        super::compile_at(program, &environment, Some(&prepared.resolved.graph.root), reach)?;
        super::admit_inputs(program)?;
    }
    let mut published_support = false;
    let result = (|| -> anyhow::Result<i32> {
        for (program, destination) in programs.iter_mut().zip(destinations) {
            if program.output == Output::Doctest {
                continue;
            }
            let unit = native[program.unit as usize];
            std::fs::create_dir_all(
                profile_root(&target, options.target.as_deref(), &unit.profile.name).join("deps"),
            )?;
            if tests.is_some() && program.run && program.output == Output::Executable {
                // Test execution and its reach metadata remain one private generation,
                // even if another client publishes to the shared target path meanwhile.
                publish_copy(&program.executable, &destination)?;
            } else {
                publish(&program.executable, &destination)?;
            }
            published_support |= matches!(program.output, Output::Object | Output::Executable);
            if program.output == Output::Object {
                let supporting = program.executable.with_extension("std.a");
                if supporting.is_file() {
                    publish(&supporting, &destination.with_extension("std.a"))?;
                }
            }
            program.executable = destination;
            if options.json {
                let unit = native[program.unit as usize];
                println!("{}", artifact(unit, &program.executable, program.output));
            }
        }
        super::super::messages::passed(format);
        super::execute(
            &programs,
            &prepared,
            &compiler_env,
            &effective,
            &producers,
            &sysroot,
            tests.as_ref(),
        )
    })();
    let mut retained = BTreeSet::new();
    if published_support {
        retained.extend(producers.prepared.outputs.values().map(|(out, _)| out.clone()));
        if producers.cargo.starts_with(work.path()) {
            retained.insert(producers.cargo.clone());
        }
    }
    drop(producers);
    if retained.is_empty() {
        work.close()?;
    } else {
        let directory = work.keep();
        prune_support(&directory, &retained)?;
    }
    result
}

/// A test's Cargo frontend is an effective environment input. Giving it a new temporary
/// name on every compile would invalidate every sandboxed test even when nothing changed.
fn cargo_frontend(
    target: &Path,
    identity: blaze_wire::StoreIdentity,
    environment: &BTreeMap<String, OsString>,
) -> anyhow::Result<PathBuf> {
    let compiler = std::env::current_exe()?;
    let mut key = Sha256::new();
    key.update(blaze_wire::encode(&identity));
    for value in [Some(compiler.as_os_str()), environment.get("PATH").map(OsString::as_os_str)] {
        let bytes = value.map_or(&[][..], |value| value.as_encoded_bytes());
        key.update((bytes.len() as u64).to_le_bytes());
        key.update(bytes);
    }
    let digest: [u8; 32] = key.finalize().into();
    let directory = target.join("ra-jit/runtime").join(super::hex(&digest));
    std::fs::create_dir_all(&directory)?;
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(directory.join("lock"))?;
    lock.lock().context("locking owned Cargo frontend")?;
    super::runtime::cargo_frontend(&compiler, &directory)
}

fn planning_environment(original: &BTreeMap<String, OsString>) -> BTreeMap<String, OsString> {
    original
        .iter()
        .filter(|(name, value)| {
            !blaze_spec::environment::REFUSED.iter().any(|(key, _)| *key == name.as_str())
                && (!name.starts_with("RA_JIT_")
                    || blaze_spec::environment::text_view(&BTreeMap::from([(
                        (*name).clone(),
                        (*value).clone(),
                    )]))
                    .is_ok())
        })
        .map(|(name, value)| (name.clone(), value.clone()))
        .collect()
}

#[derive(Default)]
struct Options {
    words: Vec<String>,
    target_dir: Option<PathBuf>,
    target: Option<String>,
    json: bool,
}

impl Options {
    fn read(words: &[String]) -> anyhow::Result<Self> {
        let mut result = Self::default();
        let mut words = words.iter();
        while let Some(word) = words.next() {
            if word == "--" {
                result.words.push(word.clone());
                result.words.extend(words.cloned());
                break;
            }
            let (name, attached) = word
                .split_once('=')
                .map_or((word.as_str(), None), |(name, value)| (name, Some(value)));
            match name {
                "--target-dir" | "--target" => {
                    let value = attached
                        .map(str::to_owned)
                        .or_else(|| words.next().cloned())
                        .context("output/target flag requires a value")?;
                    if name == "--target-dir" {
                        result.target_dir = Some(value.into());
                    } else {
                        result.target = Some(value);
                    }
                }
                "--message-format" => {
                    let value = attached
                        .map(str::to_owned)
                        .or_else(|| words.next().cloned())
                        .context("--message-format requires a value")?;
                    result.json = value
                        .split(',')
                        .any(|value| value.to_ascii_lowercase().starts_with("json"));
                }
                _ => result.words.push(word.clone()),
            }
        }
        Ok(result)
    }
}

fn destination(
    target: &Path,
    triple: Option<&str>,
    profile: &str,
    kind: TargetKind,
    name: &str,
    test: bool,
    output: Output,
    hash: &str,
) -> PathBuf {
    let mut root = profile_root(target, triple, profile);
    if kind == TargetKind::Example {
        root.push("examples");
    } else if test || matches!(output, Output::Check | Output::Object) {
        root.push("deps");
    }
    let source_name = name;
    let name = name.replace('-', "_");
    match output {
        Output::Executable if test => {
            root.join(format!("{name}-{hash}{}", std::env::consts::EXE_SUFFIX))
        }
        Output::Executable => root.join(format!("{source_name}{}", std::env::consts::EXE_SUFFIX)),
        Output::Object => root.join(format!("lib{name}-{hash}.o")),
        Output::Check => root.join(format!("{name}-{hash}.checked.json")),
        Output::Doctest => root.join(format!("doctest-{name}-{hash}/checked.json")),
    }
}

fn profile_root(target: &Path, triple: Option<&str>, profile: &str) -> PathBuf {
    let mut root = target.to_owned();
    if let Some(triple) = triple {
        root.push(triple);
    }
    root.push(match profile {
        "dev" | "test" => "debug",
        "bench" => "release",
        other => other,
    });
    root
}

fn artifact(unit: &blaze_metadata::units::Unit, path: &Path, output: Output) -> serde_json::Value {
    let kind = match unit.target.kind {
        TargetKind::Lib => "lib",
        TargetKind::Bin => "bin",
        TargetKind::Example => "example",
        TargetKind::Test => "test",
        TargetKind::Bench => "bench",
        TargetKind::CustomBuild => "custom-build",
        TargetKind::ProcMacro => "proc-macro",
    };
    serde_json::json!({
        "reason": "compiler-artifact", "package_id": unit.package.id,
        "manifest_path": unit.package.manifest_dir.join("Cargo.toml").to_string_lossy(),
        "target": {"name": unit.target.name, "kind": [kind], "crate_types": unit.target.crate_types,
            "src_path": unit.target.src_path.to_string_lossy(), "edition": unit.target.edition.to_string(),
            "doc": unit.target.doc, "doctest": unit.target.doctest, "test": unit.target.tested},
        "profile": {"opt_level": unit.profile.opt_level, "debuginfo": if unit.profile.debuginfo.is_turned_on() { 2 } else { 0 },
            "debug_assertions": unit.profile.debug_assertions, "overflow_checks": unit.profile.overflow_checks, "test": unit.test},
        "features": unit.features, "filenames": [path.to_string_lossy()], "fresh": false,
        "executable": (output == Output::Executable).then(|| path.to_string_lossy()),
    })
}

fn bind_binaries(
    project: &mut serde_json::Value,
    binaries: &BTreeMap<String, OsString>,
    produced: &BTreeMap<u32, blaze_spec::execution::ProducedUnit>,
) -> anyhow::Result<()> {
    let bytes: Vec<u8> = serde_json::from_value(project[blaze_wire::native::UNIT_FIELD].clone())?;
    let mut units = blaze_wire::native::decode_units(&bytes)?;
    for (id, unit) in &mut units {
        let explicit =
            &produced.get(id).context("native source unit has no producer provenance")?.rustc_env;
        for (name, value) in binaries {
            if unit.env.contains_key(std::ffi::OsStr::new(name)) && !explicit.contains_key(name) {
                unit.env.insert(name.into(), value.clone());
            }
        }
    }
    project[blaze_wire::native::UNIT_FIELD] =
        serde_json::json!(blaze_wire::native::encode_units(&units));
    for krate in project["crates"].as_array_mut().context("no direct source crates")? {
        let id = u32::try_from(
            krate["blaze_native_unit"].as_u64().context("source crate has no native unit")?,
        )?;
        let explicit =
            &produced.get(&id).context("source crate has no producer provenance")?.rustc_env;
        for (name, value) in binaries {
            if krate["env"].get(name).is_some() && !explicit.contains_key(name) {
                if let Some(value) = value.to_str() {
                    krate["env"][name] = value.into();
                }
            }
        }
    }
    Ok(())
}

fn publish(source: &Path, destination: &Path) -> anyhow::Result<()> {
    let directory = destination.parent().context("artifact has no parent")?;
    std::fs::create_dir_all(directory)?;
    match std::fs::rename(source, destination) {
        Ok(()) => return Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::CrossesDevices => {}
        Err(error) => return Err(error.into()),
    }
    publish_copy(source, destination)?;
    std::fs::remove_file(source)?;
    Ok(())
}

fn publish_copy(source: &Path, destination: &Path) -> anyhow::Result<()> {
    let directory = destination.parent().context("artifact has no parent")?;
    std::fs::create_dir_all(directory)?;
    let mut writing = tempfile::NamedTempFile::new_in(directory)?;
    std::io::copy(&mut std::fs::File::open(source)?, writing.as_file_mut())?;
    writing.as_file().set_permissions(std::fs::metadata(source)?.permissions())?;
    writing.persist(destination).map_err(|error| error.error)?;
    Ok(())
}

fn prune_support(path: &Path, retained: &BTreeSet<PathBuf>) -> anyhow::Result<()> {
    if retained.contains(path) {
        return Ok(());
    }
    let metadata = std::fs::symlink_metadata(path)?;
    if retained.iter().any(|retained| retained.starts_with(path)) {
        if metadata.is_dir() {
            for entry in std::fs::read_dir(path)? {
                prune_support(&entry?.path(), retained)?;
            }
        }
    } else if metadata.is_dir() {
        std::fs::remove_dir_all(path)?;
    } else {
        std::fs::remove_file(path)?;
    }
    Ok(())
}

pub(super) fn admit_trace(
    inputs: &crate::dep_info::ReplayTrace,
    project: &serde_json::Value,
) -> anyhow::Result<()> {
    ensure!(
        project["blaze_direct_execution"] == serde_json::json!(1),
        "unknown direct execution version"
    );
    ensure!(!inputs.files.is_empty(), "owned direct compiler reported no source inputs");
    for path in &inputs.files {
        let physical = crate::compiler_json::physical_path(project, Path::new(path))?;
        ensure!(std::fs::metadata(&physical)?.is_file(), "direct source input is not a file");
    }
    // Live ambient reads are valid direct inputs. Their keyed trace is retained, not
    // reclassified as the portable replay contract's declared-value proof.
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_publication_preserves_the_private_compiled_generation() {
        let work = tempfile::tempdir().unwrap();
        let source = work.path().join("compiled");
        let destination = work.path().join("target/tests");
        std::fs::write(&source, b"the admitted test generation").unwrap();
        publish_copy(&source, &destination).unwrap();
        assert_eq!(std::fs::read(&source).unwrap(), std::fs::read(&destination).unwrap());
        std::fs::write(&destination, b"another client's generation").unwrap();
        assert_eq!(std::fs::read(&source).unwrap(), b"the admitted test generation");
    }

    #[cfg(unix)]
    #[test]
    fn test_cargo_frontend_is_stable_and_serializes_concurrent_creation() {
        let work = tempfile::tempdir().unwrap();
        let environment = blaze_spec::environment::native_process();
        let identity = blaze_wire::StoreIdentity([7; 32]);
        let made = std::thread::scope(|scope| {
            let first =
                scope.spawn(|| cargo_frontend(work.path(), identity, &environment).unwrap());
            let second =
                scope.spawn(|| cargo_frontend(work.path(), identity, &environment).unwrap());
            [first.join().unwrap(), second.join().unwrap()]
        });
        assert_eq!(made[0], made[1]);
        assert!(made[0].is_file());
        assert_eq!(cargo_frontend(work.path(), identity, &environment).unwrap(), made[0]);
    }

    #[test]
    fn publication_moves_the_artifact_and_retains_only_referenced_support() {
        let work = tempfile::tempdir().unwrap();
        let job = work.path().join("job");
        let artifacts = job.join("artifacts");
        let support = job.join("producers/generation/out");
        let cargo = job.join("producers/cargo-runtime/blaze");
        std::fs::create_dir_all(&artifacts).unwrap();
        std::fs::create_dir_all(&support).unwrap();
        std::fs::create_dir_all(cargo.parent().unwrap()).unwrap();
        std::fs::create_dir(job.join("compiler-home")).unwrap();
        std::fs::write(support.join("runtime-data"), b"current producer data").unwrap();
        std::fs::write(&cargo, b"owned Cargo launcher").unwrap();
        std::fs::write(artifacts.join("program"), b"executable").unwrap();
        std::fs::write(artifacts.join("inputs.cbor"), b"temporary trace").unwrap();
        let destination = work.path().join("target/debug/program");
        publish(&artifacts.join("program"), &destination).unwrap();
        assert_eq!(std::fs::read(&destination).unwrap(), b"executable");
        assert!(!artifacts.join("program").exists());
        prune_support(&job, &BTreeSet::from([support.clone(), cargo.clone()])).unwrap();
        assert!(!artifacts.exists());
        assert!(!job.join("compiler-home").exists());
        assert!(cargo.is_file());
        assert_eq!(std::fs::read(support.join("runtime-data")).unwrap(), b"current producer data");
    }

    #[test]
    fn jobs_without_published_support_do_not_retain_a_cache_directory() {
        let work = tempfile::tempdir().unwrap();
        let job = work.path().join("job");
        std::fs::create_dir_all(job.join("artifacts")).unwrap();
        std::fs::write(job.join("artifacts/check.json"), b"checked").unwrap();
        prune_support(&job, &BTreeSet::new()).unwrap();
        assert!(!job.exists());
    }
    #[test]
    fn destinations_follow_target_profile_and_cargo_target_kinds() {
        let path = |profile, kind, test, output| {
            destination(
                Path::new("/target"),
                Some("host"),
                profile,
                kind,
                "my-bin",
                test,
                output,
                "identity",
            )
        };
        assert_eq!(
            path("test", TargetKind::Bin, true, Output::Executable),
            PathBuf::from(format!(
                "/target/host/debug/deps/my_bin-identity{}",
                std::env::consts::EXE_SUFFIX
            ))
        );
        assert_eq!(
            path("release", TargetKind::Example, false, Output::Executable),
            PathBuf::from(format!(
                "/target/host/release/examples/my-bin{}",
                std::env::consts::EXE_SUFFIX
            ))
        );
        assert_eq!(
            path("custom", TargetKind::Lib, false, Output::Object),
            PathBuf::from("/target/host/custom/deps/libmy_bin-identity.o")
        );
        assert_eq!(
            path("dev", TargetKind::Lib, false, Output::Check),
            PathBuf::from("/target/host/debug/deps/my_bin-identity.checked.json")
        );
    }

    #[test]
    fn same_named_library_units_publish_distinct_objects_and_supporting_archives() {
        let destination = |hash| {
            destination(
                Path::new("/target"),
                None,
                "dev",
                TargetKind::Lib,
                "shared",
                false,
                Output::Object,
                hash,
            )
        };
        let first = destination("package-a");
        let second = destination("package-b");
        assert_ne!(first, second);
        assert_ne!(first.with_extension("std.a"), second.with_extension("std.a"));
        assert_eq!(first.parent(), Some(Path::new("/target/debug/deps")));
    }
    #[test]
    fn location_options_do_not_consume_test_harness_arguments() {
        let words: Vec<_> =
            ["--target-dir", "output", "--target=host", "--no-run", "--", "--target-dir", "filter"]
                .map(str::to_owned)
                .into();
        let parsed = Options::read(&words).unwrap();
        assert_eq!(parsed.target_dir, Some("output".into()));
        assert_eq!(parsed.target.as_deref(), Some("host"));
        assert_eq!(parsed.words, ["--no-run", "--", "--target-dir", "filter"]);
    }
    #[test]
    fn direct_private_environment_and_keyed_trace_never_enter_public_descriptor() {
        let work = tempfile::tempdir().unwrap();
        let original = work.path().join("original-source.rs");
        std::fs::write(&original, "fn main() {}\n").unwrap();
        let native = BTreeMap::from([(
            7,
            blaze_wire::native::Unit {
                env: BTreeMap::from([
                    ("USER_INPUT".into(), "private-live-input".into()),
                    ("CARGO_MANIFEST_DIR".into(), work.path().as_os_str().to_owned()),
                    ("CARGO_BIN_EXE_app".into(), "old-job/app".into()),
                ]),
                flags: Vec::new(),
            },
        )]);
        let mut project = serde_json::json!({
            "compiler_mode": true, "blaze_direct_execution": 1,
            "blaze_native_units": blaze_wire::native::encode_units(&native),
            "crates": [{"root_module": original, "blaze_native_unit": 7,
                "env": {"USER_INPUT": "private-live-input", "CARGO_BIN_EXE_app": "old-job/app"}}],
        });
        bind_binaries(
            &mut project,
            &BTreeMap::from([("CARGO_BIN_EXE_app".into(), "target/debug/app".into())]),
            &BTreeMap::from([(
                7,
                blaze_spec::execution::ProducedUnit {
                    cfg: Vec::new(),
                    codegen_flags: Vec::new(),
                    native: native[&7].clone(),
                    proc_macro: None,
                    rustc_env: BTreeMap::new(),
                },
            )]),
        )
        .unwrap();
        assert_eq!(project["crates"][0]["env"]["CARGO_BIN_EXE_app"], "target/debug/app");
        assert_eq!(project["crates"][0]["root_module"], serde_json::json!(original));
        crate::dep_info::ensure_replay_key(&mut project).unwrap();
        let file = work.path().join("rust-project.json");
        let private = crate::compiler_json::write(&file, &project).unwrap();
        let public = std::fs::read_to_string(&file).unwrap();
        assert!(!public.contains("private-live-input"));
        assert!(!public.contains(crate::dep_info::TRACE_KEY_FIELD));
        assert!(!public.contains(blaze_wire::native::UNIT_FIELD));
        assert!(String::from_utf8(private).unwrap().contains("private-live-input"));
        let inputs = crate::dep_info::DepInfo {
            files: vec![original.to_str().unwrap().into()],
            env: vec![("USER_INPUT".into(), Some("private-live-input".into()))],
        };
        let trace = crate::dep_info::ReplayTrace::from_inputs(
            &inputs,
            crate::dep_info::replay_key(&project).unwrap(),
        );
        let encoded = trace.encode();
        assert!(
            !encoded.windows(b"private-live-input".len()).any(|part| part == b"private-live-input")
        );
        admit_trace(&trace, &project).unwrap();
    }

    #[test]
    fn explicit_binary_environment_wins_even_when_it_matches_a_derived_path() {
        let native = blaze_wire::native::Unit {
            env: BTreeMap::from([("CARGO_BIN_EXE_app".into(), "/derived/app".into())]),
            flags: Vec::new(),
        };
        let produced = BTreeMap::from([(
            3,
            blaze_spec::execution::ProducedUnit {
                cfg: Vec::new(),
                codegen_flags: Vec::new(),
                native: native.clone(),
                proc_macro: None,
                rustc_env: BTreeMap::from([("CARGO_BIN_EXE_app".into(), "/derived/app".into())]),
            },
        )]);
        let mut project = serde_json::json!({"blaze_native_units": blaze_wire::native::encode_units(&BTreeMap::from([(3,native)])),
            "crates":[{"blaze_native_unit":3,"env":{"CARGO_BIN_EXE_app":"/derived/app"}}]});
        bind_binaries(
            &mut project,
            &BTreeMap::from([("CARGO_BIN_EXE_app".into(), "/published/app".into())]),
            &produced,
        )
        .unwrap();
        assert_eq!(project["crates"][0]["env"]["CARGO_BIN_EXE_app"], "/derived/app");
        let bytes: Vec<u8> =
            serde_json::from_value(project[blaze_wire::native::UNIT_FIELD].clone()).unwrap();
        assert_eq!(
            blaze_wire::native::decode_units(&bytes).unwrap()[&3].env
                [std::ffi::OsStr::new("CARGO_BIN_EXE_app")],
            "/derived/app"
        );
    }

    #[test]
    fn planner_omits_service_overrides_but_preserves_live_compile_inputs() {
        let original = BTreeMap::from([
            ("RUSTUP_TOOLCHAIN".into(), "service-toolchain".into()),
            ("RA_JIT_PROC_MACRO_SERVER".into(), "private-runtime".into()),
            ("RA_JIT_BACKEND".into(), "llvm".into()),
            ("USER_VALUE".into(), "ordinary-input".into()),
        ]);
        let planning = planning_environment(&original);
        assert!(!planning.contains_key("RUSTUP_TOOLCHAIN"));
        assert!(!planning.contains_key("RA_JIT_PROC_MACRO_SERVER"));
        assert_eq!(planning["RA_JIT_BACKEND"], "llvm");
        assert_eq!(planning["USER_VALUE"], "ordinary-input");
        assert_eq!(original["RA_JIT_PROC_MACRO_SERVER"], "private-runtime");
    }
}
