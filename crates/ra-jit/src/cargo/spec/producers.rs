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
    let effective = replay.execution_environment()?;
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
            source: Planned { name: unit.target.name.replace('-', "_"), root: unit.target.src_path.clone(), edition: unit.target.edition.to_string(), cfg: unit_cfg(replay, at), codegen_flags, env: text_env, deps, proc_macro: None },
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
                    cargo: std::env::current_exe()?,
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
    let make_bindings = |prepared: &captured::Prepared| -> anyhow::Result<serde_json::Value> {
        bindings.validate()?;
        let mut full = bindings.clone();
        full.roots.extend(generated(prepared)?.roots);
        Ok(serde_json::json!(full.encode_bindings()))
    };
    let prepared = captured::run_with_context(
        graph,
        sysroot,
        work,
        process_environment,
        captured::Context { native, runtime, bindings: Some(&make_bindings) },
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
                },
            ))
        })
        .collect::<anyhow::Result<_>>()?;
    let generated = generated(&prepared)?;
    let mut bindings = bindings.clone();
    bindings.roots.extend(generated.roots.clone());
    Ok(Producers { units, prepared, bindings, generated })
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
