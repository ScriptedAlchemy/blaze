use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::OsString,
    path::Path,
};

use anyhow::{Context as _, ensure};
use blaze_build_script::unit as script;
use blaze_metadata::units::{Mode, TargetKind, Unit};
use blaze_spec::{
    execution::{ProducedUnit, flags, package_env, unit_cfg},
    replay::Prepared,
};

use crate::{cargo::own_script::Planned, scripts::captured};

pub(super) struct Producers {
    pub(super) units: BTreeMap<u32, ProducedUnit>,
    pub(super) prepared: captured::Prepared,
    pub(super) bindings: blaze_spec::native::Closure,
    pub(super) generated: blaze_spec::native::Closure,
    pub(super) runtime: BTreeMap<u32, super::runtime::Unit>,
    pub(super) cargo: std::path::PathBuf,
    pub(super) compiler_libraries: BTreeMap<u32, Vec<std::path::PathBuf>>,
}

impl Producers {
    pub(super) fn admit_inputs(&self) -> anyhow::Result<()> {
        for path in self
            .prepared
            .inputs
            .iter()
            .chain(&self.prepared.runtime_inputs)
            .chain(&self.prepared.generated_inputs)
        {
            let path = blaze_manifest::paths::normalize_path(path);
            let current = blaze_spec::native_tree::Tree::capture(&path)?;
            let entry =
                current.entry(Path::new("")).context("producer input has no observed root")?;
            self.bindings.admit(&path, entry).with_context(|| format!("producer input {} is outside authenticated source, runtime and generated inputs", path.display()))?;
        }
        self.bindings.validate()
    }
}

fn target(unit: &Unit) -> script::Target {
    let kind = match unit.target.kind {
        TargetKind::Lib => "lib",
        TargetKind::Bin => "bin",
        TargetKind::Example => "example",
        TargetKind::Test => "test",
        TargetKind::Bench => "bench",
        TargetKind::CustomBuild => "custom-build",
        TargetKind::ProcMacro => "proc-macro",
    };
    script::Target {
        name: unit.target.name.clone(),
        kind: vec![kind.into()],
        crate_types: unit.target.crate_types.clone(),
    }
}

fn package(unit: &Unit, manifests: &blaze_manifest::Context) -> anyhow::Result<script::Package> {
    use blaze_manifest::manifest::{EitherManifest, read_manifest};
    let path = unit.package.manifest_dir.join("Cargo.toml");
    let source = blaze_manifest::source::SourceId::for_manifest_path(&path)?;
    let EitherManifest::Real(package) = read_manifest(&path, source, manifests)? else {
        anyhow::bail!("{} is not a package", path.display());
    };
    let meta = &package.metadata;
    let targets = package
        .targets
        .iter()
        .map(|target| {
            let kinds = serde_json::to_value(&target.kind)?;
            let kind: Vec<String> = serde_json::from_value(kinds)?;
            Ok(script::Target {
                name: target.name.clone(),
                kind,
                crate_types: target
                    .kind
                    .rustc_crate_types()
                    .iter()
                    .map(ToString::to_string)
                    .collect(),
            })
        })
        .collect::<anyhow::Result<_>>()?;
    Ok(script::Package {
        name: unit.package.name.clone(),
        version: unit.package.version.clone(),
        descr: Some(if unit.package.source.starts_with("path+") {
            format!(
                "{} v{} ({})",
                unit.package.name,
                unit.package.version,
                unit.package.manifest_dir.display()
            )
        } else {
            format!("{} v{}", unit.package.name, unit.package.version)
        }),
        manifest_dir: unit.package.manifest_dir.clone(),
        manifest_path: path,
        authors: meta.authors.clone(),
        description: meta.description.clone(),
        homepage: meta.homepage.clone(),
        repository: meta.repository.clone(),
        license: meta.license.clone(),
        license_file: meta.license_file.clone(),
        readme: meta.readme.clone(),
        rust_version: meta.rust_version.as_ref().map(ToString::to_string),
        links: meta.links.clone(),
        targets,
    })
}

/// Adapt the reproduced canonical graph directly; this never rediscovers a Cargo workspace.
pub(super) fn prepare(
    replay: &Prepared,
    sysroot: &Path,
    work: &Path,
    process_environment: crate::process::Native,
    runtime: Option<&serde_json::Value>,
    bindings: &blaze_spec::native::Closure,
) -> anyhow::Result<Producers> {
    prepare_with(replay, sysroot, work, process_environment, runtime, Some(bindings), None, None)
}

pub(super) fn prepare_live(
    replay: &Prepared,
    sysroot: &Path,
    work: &Path,
    process_environment: crate::process::Native,
    runtime: Option<&serde_json::Value>,
    environment: &BTreeMap<String, OsString>,
    cargo: &Path,
) -> anyhow::Result<Producers> {
    prepare_with(
        replay,
        sysroot,
        work,
        process_environment,
        runtime,
        None,
        Some(environment),
        Some(cargo),
    )
}

fn prepare_with(
    replay: &Prepared,
    sysroot: &Path,
    work: &Path,
    process_environment: crate::process::Native,
    runtime: Option<&serde_json::Value>,
    bindings: Option<&blaze_spec::native::Closure>,
    environment: Option<&BTreeMap<String, OsString>>,
    cargo: Option<&Path>,
) -> anyhow::Result<Producers> {
    let build = &replay.resolved.spec.build;
    let canonical = blaze_spec::assemble::canonical_units(build, &replay.resolved.graph)?;
    let manifests = replay.manifests()?;
    let graph_units = &replay.resolved.graph.units;
    let canonical_of_graph: BTreeMap<_, _> = canonical
        .iter()
        .enumerate()
        .map(|(at, unit)| {
            let original = graph_units
                .iter()
                .position(|original| std::ptr::eq(original, *unit))
                .expect("canonical units refer to the reproduced graph");
            (original, at)
        })
        .collect();
    let run_indices: BTreeMap<_, _> = graph_units
        .iter()
        .enumerate()
        .filter(|(_, unit)| unit.mode == Mode::RunCustomBuild)
        .enumerate()
        .map(|(run, (at, _))| (at, run))
        .collect();
    let effective = environment.cloned().unwrap_or(replay.execution_environment()?);
    let cargo = match cargo {
        Some(cargo) => cargo.to_owned(),
        None => {
            super::runtime::cargo_frontend(&std::env::current_exe()?, &work.join("cargo-runtime"))?
        }
    };
    let path = process_environment.vars().find(|(key, _)| key == "PATH").map(|(_, value)| value);
    let mut graph = captured::Graph { units: Vec::new(), runs: Vec::new() };
    let mut native = BTreeMap::new();
    let mut host_closure = BTreeSet::new();
    fn mark(at: usize, units: &[Unit], seen: &mut BTreeSet<usize>) {
        if seen.insert(at) {
            for dep in &units[at].dependencies {
                if units[dep.index].mode != Mode::RunCustomBuild {
                    mark(dep.index, units, seen);
                }
            }
        }
    }
    // Cargo may share a native library unit between the target and host. Reachability
    // from a host producer is the proof; for_host alone cannot identify that sharing.
    for (at, unit) in graph_units.iter().enumerate() {
        if unit.mode != Mode::RunCustomBuild
            && matches!(unit.target.kind, TargetKind::ProcMacro | TargetKind::CustomBuild)
        {
            mark(at, graph_units, &mut host_closure);
        }
    }
    for (source, unit) in canonical.iter().copied().enumerate() {
        let at = source;
        let spec = &build.units[at];
        let primary = build
            .asked
            .iter()
            .any(|&asked| canonical[asked as usize].package.id == unit.package.id);
        let mut env = effective.clone();
        if !env.contains_key("PATH")
            && let Some(path) = &path
        {
            env.insert("PATH".into(), path.clone());
        }
        env.extend(package_env(unit, &manifests, primary)?);
        env.insert("CARGO".into(), cargo.as_os_str().to_owned());
        let native_env =
            env.iter().map(|(name, value)| (OsString::from(name), value.clone())).collect();
        let text_env = env
            .iter()
            .filter_map(|(name, value)| Some((name.clone(), value.to_str()?.to_owned())))
            .collect();
        let codegen_flags = flags(&spec.profile, build);
        native.insert(
            source,
            captured::NativeUnit {
                env: native_env,
                flags: codegen_flags.iter().map(OsString::from).collect(),
            },
        );
        let runs: Vec<_> = unit
            .dependencies
            .iter()
            .filter_map(|dep| run_indices.get(&dep.index).copied())
            .collect();
        ensure!(runs.len() <= 1, "canonical unit has more than one configuring build-script run");
        let deps = unit
            .dependencies
            .iter()
            .filter_map(|dep| {
                canonical_of_graph
                    .get(&dep.index)
                    .filter(|&&index| {
                        matches!(
                            canonical[index].target.kind,
                            TargetKind::Lib | TargetKind::ProcMacro
                        )
                    })
                    .map(|&index| (dep.extern_crate_name.clone(), index))
            })
            .collect();
        graph.units.push(captured::Unit {
            source: Planned { out_dir: None, workspace_member: false, name: unit.target.name.replace('-', "_"), root: unit.target.src_path.clone(), edition: unit.target.edition.to_string(), cfg: unit_cfg(replay, at), codegen_flags, env: text_env, deps, proc_macro: None },
            kind: match unit.target.kind { TargetKind::CustomBuild => captured::Kind::BuildScript, TargetKind::ProcMacro => captured::Kind::ProcMacro, _ => captured::Kind::Rust },
            host: graph_units.iter().enumerate().any(|(original, other)| std::ptr::eq(other, unit) && host_closure.contains(&original)), target: target(unit), run: runs.first().copied(),
            profile: Some(serde_json::json!({"version": 1, "profile": blaze_wire::encode(&spec.profile), "rustflags": build.rustflags.iter().map(|flag| flag.0.clone()).collect::<Vec<_>>()})),
        });
    }
    for (&at, &run) in &run_indices {
        ensure!(graph.runs.len() == run, "canonical run ordering changed");
        let unit = &graph_units[at];
        let compilers: Vec<_> = unit
            .dependencies
            .iter()
            .filter(|dep| {
                graph_units[dep.index].target.kind == TargetKind::CustomBuild
                    && graph_units[dep.index].mode != Mode::RunCustomBuild
            })
            .collect();
        ensure!(compilers.len() == 1, "canonical build-script run has no unique compiler");
        let mut dependencies = Vec::new();
        for dep in &unit.dependencies {
            if let Some(&other) = run_indices.get(&dep.index) {
                dependencies.push((dep.extern_crate_name.clone(), other));
            }
        }
        let profile = &unit.profile;
        graph.runs.push(captured::Run {
            compiler: canonical_of_graph[&compilers[0].index],
            dependencies,
            unit: script::Unit {
                package: package(unit, &manifests)?,
                features: unit.features.clone(),
                profile: script::Profile {
                    name: profile.name.clone(),
                    release: profile.root == blaze_manifest::profiles::ProfileRoot::Release,
                    opt_level: profile.opt_level.clone(),
                    debuginfo: profile.debuginfo.is_turned_on(),
                    debug_assertions: profile.debug_assertions,
                },
                target: replay.host.triple.clone(),
                host: replay.host.triple.clone(),
                target_cfgs: replay.host.cfg.iter().map(ToString::to_string).collect(),
                rustflags: build.rustflags.iter().map(|flag| flag.0.clone()).collect(),
                jobs: std::thread::available_parallelism().map_or(1, |jobs| jobs.get() as u32),
                tools: script::Tools {
                    cargo: cargo.clone(),
                    linker: manifests.config.target_linker(
                        &blaze_manifest::config::CompileKind::Host,
                        &replay.host.triple,
                        &replay.host.cfg,
                    )?,
                    ..Default::default()
                },
                workspace_member: unit.package.member,
                dylib_search: script::DylibSearch {
                    sysroot_libdir: sysroot
                        .join("lib/rustlib")
                        .join(&replay.host.triple)
                        .join("lib"),
                    ..Default::default()
                },
                ..Default::default()
            },
        });
    }
    let own_runs: Vec<_> = graph.units.iter().map(|unit| unit.run).collect();
    fn linked_runs(
        graph: &captured::Graph,
        at: usize,
        seen: &mut BTreeSet<usize>,
        runs: &mut Vec<usize>,
    ) {
        if !seen.insert(at) || graph.units[at].kind != captured::Kind::Rust {
            return;
        }
        if let Some(run) = graph.units[at].run {
            if !runs.contains(&run) {
                runs.push(run);
            }
        }
        for (_, dep) in &graph.units[at].source.deps {
            linked_runs(graph, *dep, seen, runs);
        }
    }
    let runtime_runs: Vec<_> = (0..graph.units.len())
        .map(|at| {
            let mut runs = Vec::new();
            linked_runs(&graph, at, &mut BTreeSet::new(), &mut runs);
            runs
        })
        .collect();
    let compiler_runs =
        (0..graph.units.len()).map(|at| graph.host_runs(at)).collect::<anyhow::Result<Vec<_>>>()?;
    let make_bindings = |prepared: &captured::Prepared| -> anyhow::Result<serde_json::Value> {
        let bindings = bindings.context("replay producer bindings disappeared")?;
        bindings.validate()?;
        let mut full = bindings.clone();
        full.roots.extend(generated(prepared)?.roots);
        Ok(serde_json::json!(full.encode_bindings()))
    };
    let mut producer_environment: BTreeMap<OsString, OsString> =
        effective.iter().map(|(name, value)| (name.into(), value.clone())).collect();
    producer_environment.insert("CARGO".into(), cargo.as_os_str().to_owned());
    let prepared = captured::run_with_context(
        graph,
        sysroot,
        work,
        crate::process::Native::of(producer_environment),
        captured::Context {
            compiler_environment: Some(&process_environment),
            native,
            runtime,
            source_root: Some(&replay.resolved.graph.root),
            workspace_members: canonical
                .iter()
                .enumerate()
                .filter_map(|(at, unit)| unit.package.member.then_some(at))
                .collect(),
            bindings: bindings.map(|_| {
                &make_bindings as &dyn Fn(&captured::Prepared) -> anyhow::Result<serde_json::Value>
            }),
        },
    )?;
    let units = prepared
        .units
        .iter()
        .enumerate()
        .map(|(at, unit)| {
            let source = at;
            Ok((
                u32::try_from(at)?,
                ProducedUnit {
                    cfg: unit.cfg.clone(),
                    codegen_flags: unit.codegen_flags.clone(),
                    native: prepared.native[&source].clone(),
                    proc_macro: unit.proc_macro.clone(),
                    rustc_env: own_runs[at]
                        .map(|run| prepared.outputs[&run].1.env.iter().cloned().collect())
                        .unwrap_or_default(),
                },
            ))
        })
        .collect::<anyhow::Result<_>>()?;
    let generated = generated(&prepared)?;
    let runtime = canonical
        .iter()
        .enumerate()
        .map(|(at, unit)| {
            let env = runtime_values(
                package_env(unit, &manifests, false)?,
                own_runs[at].map(|run| &prepared.outputs[&run]),
                &cargo,
            );
            let libraries = runtime_runs[at]
                .iter()
                .flat_map(|run| {
                    crate::cargo::loader::artifact_directories(&prepared.outputs[run].1)
                })
                .collect();
            Ok((u32::try_from(at)?, super::runtime::Unit { env, libraries }))
        })
        .collect::<anyhow::Result<_>>()?;
    let mut bindings = bindings.cloned().unwrap_or_default();
    bindings.roots.extend(generated.roots.clone());
    let compiler_libraries = compiler_runs
        .iter()
        .enumerate()
        .map(|(at, runs)| {
            Ok((
                u32::try_from(at)?,
                runs.iter()
                    .flat_map(|run| {
                        crate::cargo::loader::artifact_directories(&prepared.outputs[run].1)
                    })
                    .collect(),
            ))
        })
        .collect::<anyhow::Result<_>>()?;
    Ok(Producers { units, prepared, bindings, generated, runtime, cargo, compiler_libraries })
}

fn runtime_values(
    mut package: BTreeMap<String, OsString>,
    script: Option<&(std::path::PathBuf, blaze_build_script::output::Output)>,
    cargo: &Path,
) -> BTreeMap<String, OsString> {
    package.retain(|name, _| {
        !matches!(name.as_str(), "CARGO_CRATE_NAME" | "CARGO_BIN_NAME" | "CARGO_PRIMARY_PACKAGE")
    });
    if let Some((out, output)) = script {
        package.insert("OUT_DIR".into(), out.as_os_str().to_owned());
        package.extend(output.env.iter().map(|(name, value)| (name.clone(), value.into())));
    }
    package.insert("CARGO".into(), cargo.as_os_str().to_owned());
    package
}

fn generated(prepared: &captured::Prepared) -> anyhow::Result<blaze_spec::native::Closure> {
    let mut roots = Vec::new();
    for (&at, (directory, _)) in &prepared.outputs {
        roots.push((format!("producer/out/{at}"), directory.clone()));
    }
    for (at, unit) in prepared.units.iter().enumerate() {
        if let Some(image) = &unit.proc_macro {
            roots.push((
                format!("producer/image/{at}"),
                image.parent().context("owned macro image directory")?.to_owned(),
            ));
        }
    }
    let paths: Vec<_> = roots.iter().map(|(_, path)| path.clone()).collect();
    let trees = blaze_spec::native_tree::Tree::capture_many(&paths)?;
    Ok(blaze_spec::native::Closure {
        roots: roots
            .into_iter()
            .zip(trees)
            .map(|((name, _), tree)| blaze_spec::native::Root { name, tree })
            .collect(),
    })
}

#[cfg(test)]
mod runtime_tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn runtime_overlays_match_cargo_ambient_and_script_precedence() {
        let package = BTreeMap::from([
            ("CARGO_PRIMARY_PACKAGE".into(), "1".into()),
            ("CARGO_BIN_NAME".into(), "derived-bin".into()),
            ("CARGO_CRATE_NAME".into(), "derived_crate".into()),
            ("CARGO_MANIFEST_DIR".into(), "/original/package".into()),
        ]);
        let values = runtime_values(package.clone(), None, Path::new("/owned/blaze"));
        for key in ["CARGO_PRIMARY_PACKAGE", "CARGO_BIN_NAME", "CARGO_CRATE_NAME"] {
            assert!(!values.contains_key(key));
        }
        let inherited = BTreeMap::from([
            ("CARGO_BIN_NAME".into(), "ambient-bin".into()),
            ("CARGO".into(), "external-cargo".into()),
        ]);
        let mut env = inherited.clone();
        env.extend(values);
        assert_eq!(env["CARGO_BIN_NAME"], "ambient-bin");
        assert_eq!(env["CARGO"], "/owned/blaze");
        let output = (
            PathBuf::from("/generated/out"),
            blaze_build_script::output::Output {
                env: vec![
                    ("CARGO_BIN_NAME".into(), "script-bin".into()),
                    ("CUSTOM".into(), "script-value".into()),
                    ("CARGO".into(), "script-cargo".into()),
                ],
                ..Default::default()
            },
        );
        let values = runtime_values(package, Some(&output), Path::new("/owned/blaze"));
        let mut env = inherited;
        env.extend(values);
        assert_eq!(env["CARGO_BIN_NAME"], "script-bin");
        assert_eq!(env["CUSTOM"], "script-value");
        assert_eq!(env["CARGO"], "/owned/blaze");
        assert_eq!(env["OUT_DIR"], "/generated/out");
        assert_eq!(env["CARGO_MANIFEST_DIR"], "/original/package");
    }
}
