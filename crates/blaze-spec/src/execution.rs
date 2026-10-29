//! Source-only descriptions for the owned compiler. This module never starts a
//! process: it checks the supported execution shapes and makes `rust-project.json`
//! from the exact native units that reproduced the BuildSpec.

use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
};

use anyhow::{Context as _, bail, ensure};
use blaze_metadata::units::{Mode, TargetKind, Unit};
use blaze_wire::{BuildSpec, Command, ProfileSpec};
use serde_json::{Value, json};

use crate::{assemble::canonical_units, replay::Prepared};

/// The artifact the owned compiler produces for a root unit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Output {
    /// A linked executable.
    Executable,
    /// A native object of a standalone Rust library.
    Object,
    /// A successful frontend check, without code generation.
    Check,
    /// A documented library checked before its owned examples run.
    Doctest,
}

/// A compiler description preserves native path bytes until its local filesystem
/// renderer supplies the frontend's names. Ordinary UTF-8 paths keep their spelling.
pub fn compiler_path(path: &Path) -> Value {
    match path.to_str() {
        Some(path) => json!(path),
        None => json!({"blaze_native_path": path.as_os_str().as_encoded_bytes()}),
    }
}

/// One root compilation, its complete source graph, and whether the command runs it.
pub struct Program {
    /// Canonical index of the root unit in the BuildSpec.
    pub unit: u32,
    /// The root crate name accepted by the owned compiler.
    pub crate_name: String,
    /// Whether Blaze must inject its libtest harness into the root crate.
    pub harness: bool,
    /// Whether this program is run after all artifacts have passed admission.
    pub run: bool,
    /// Whether this root is linked, compiled to an object, or checked.
    pub output: Output,
    /// Where the linked executable is written.
    pub executable: PathBuf,
    /// A complete source description. No Cargo project is passed to the compiler.
    pub project: Value,
}

/// The source and native settings after the owned producer scheduler has completed.
/// Indices are canonical BuildSpec unit identities, including distinct instances that
/// happen to share one source file.
#[derive(Clone, Debug)]
pub struct ProducedUnit {
    /// Complete configured target/profile/features and producer cfg.
    pub cfg: Vec<String>,
    /// Frontend text settings; native linker settings remain lossless below.
    pub codegen_flags: Vec<String>,
    /// Full per-instance environment and native linker settings.
    pub native: blaze_wire::native::Unit,
    /// An immutable, leased macro image, when this unit is an expansion leaf.
    pub proc_macro: Option<PathBuf>,
}

/// Complete target/profile/features cfg before a producer adds its output.
pub fn unit_cfg(prepared: &Prepared, at: usize) -> Vec<String> {
    prepared
        .host
        .cfg
        .iter()
        .map(ToString::to_string)
        .filter(|cfg| {
            let key = cfg.split('=').next().unwrap_or(cfg).trim();
            !matches!(key, "panic" | "debug_assertions" | "overflow_checks")
        })
        .chain(prepared.resolved.spec.build.units[at].cfg.iter().map(|cfg| cfg.0.clone()))
        .collect()
}

fn reachable(build: &BuildSpec, native: &[&Unit], root: u32) -> BTreeSet<u32> {
    let mut seen = BTreeSet::new();
    let mut next = vec![root];
    while let Some(at) = next.pop() {
        if native[at as usize].target.kind == TargetKind::CustomBuild
            || (at != root
                && !matches!(
                    native[at as usize].target.kind,
                    TargetKind::Lib | TargetKind::ProcMacro
                ))
        {
            continue;
        }
        if seen.insert(at) && native[at as usize].target.kind != TargetKind::ProcMacro {
            next.extend(build.units[at as usize].deps.iter().map(|(at, _)| *at));
        }
    }
    seen
}

fn roots(build: &BuildSpec, native: &[&Unit]) -> Vec<u32> {
    let mut roots: BTreeSet<_> = build.asked.iter().copied().collect();
    if matches!(build.command, Command::Test(_)) {
        let mut seen = BTreeSet::new();
        let mut pending: Vec<_> = roots.iter().copied().collect();
        while let Some(at) = pending.pop() {
            if !seen.insert(at) {
                continue;
            }
            let unit = native[at as usize];
            if unit.target.kind == TargetKind::Bin && !unit.test {
                roots.insert(at);
            }
            pending.extend(build.units[at as usize].deps.iter().map(|(at, _)| *at));
        }
    }
    if let Command::Test(run) = &build.command
        && run.doc != 0
        && !run.no_run
    {
        let selected: BTreeSet<_> =
            build.asked.iter().map(|&at| &native[at as usize].package.id).collect();
        roots.extend(
            native
                .iter()
                .enumerate()
                .filter(|(_, unit)| {
                    matches!(unit.target.kind, TargetKind::Lib | TargetKind::ProcMacro)
                        && unit.mode == Mode::Build
                        && unit.target.doctest
                        && selected.contains(&unit.package.id)
                })
                .map(|(at, _)| at as u32),
        );
    }
    let mut roots: Vec<_> = roots.into_iter().collect();
    roots.sort_by_key(|&at| (native[at as usize].test, at));
    roots
}

fn is_program(unit: &Unit) -> bool {
    unit.test
        || matches!(unit.target.kind, TargetKind::Bin | TargetKind::Example | TargetKind::Test)
}

/// The resolved code-generation flags for one canonical unit.
pub fn flags(profile: &ProfileSpec, build: &BuildSpec) -> Vec<String> {
    // The effective profile already includes rustflags' overrides. Write it last so
    // no spelling of an original flag can undo that resolved per-unit decision.
    build
        .rustflags
        .iter()
        .map(|it| it.0.clone())
        .chain([
            format!("-Copt-level={}", profile.opt_level),
            format!("-Cdebug-assertions={}", profile.debug_assertions),
            format!("-Coverflow-checks={}", profile.overflow_checks),
            format!("-Cpanic={}", if profile.panic == 1 { "abort" } else { "unwind" }),
            format!("-Cdebuginfo={}", profile.debug),
            format!("-Cstrip={}", ["none", "debuginfo", "symbols"][profile.strip as usize]),
            format!(
                "-Csplit-debuginfo={}",
                ["off", "packed", "unpacked"][profile.split_debuginfo as usize]
            ),
        ])
        .collect()
}

/// Cargo's package and target variables, retaining native manifest directory bytes.
pub fn package_env(
    unit: &Unit,
    manifests: &blaze_manifest::Context,
    primary: bool,
) -> anyhow::Result<BTreeMap<String, std::ffi::OsString>> {
    use blaze_manifest::manifest::{EitherManifest, read_manifest};
    let manifest = unit.package.manifest_dir.join("Cargo.toml");
    let source = blaze_manifest::source::SourceId::for_manifest_path(&manifest)?;
    let EitherManifest::Real(package) = read_manifest(&manifest, source, manifests)? else {
        bail!("{} is not a package", manifest.display());
    };
    let version = &package.package_id.version;
    let meta = &package.metadata;
    let mut env: BTreeMap<String, std::ffi::OsString> = [
        ("CARGO_MANIFEST_DIR".to_owned(), unit.package.manifest_dir.as_os_str().to_owned()),
        ("CARGO_MANIFEST_PATH".to_owned(), manifest.as_os_str().to_owned()),
    ]
    .into_iter()
    .collect();
    let mut set = |name: &str, value: String| {
        env.insert(name.to_owned(), value.into());
    };
    set("CARGO_PKG_VERSION", version.to_string());
    set("CARGO_PKG_VERSION_MAJOR", version.major.to_string());
    set("CARGO_PKG_VERSION_MINOR", version.minor.to_string());
    set("CARGO_PKG_VERSION_PATCH", version.patch.to_string());
    set("CARGO_PKG_VERSION_PRE", version.pre.to_string());
    set("CARGO_PKG_AUTHORS", meta.authors.join(":"));
    set("CARGO_PKG_NAME", unit.package.name.clone());
    set("CARGO_PKG_DESCRIPTION", meta.description.clone().unwrap_or_default());
    set("CARGO_PKG_HOMEPAGE", meta.homepage.clone().unwrap_or_default());
    set("CARGO_PKG_REPOSITORY", meta.repository.clone().unwrap_or_default());
    set("CARGO_PKG_LICENSE", meta.license.clone().unwrap_or_default());
    set("CARGO_PKG_LICENSE_FILE", meta.license_file.clone().unwrap_or_default());
    set("CARGO_PKG_README", meta.readme.clone().unwrap_or_default());
    set(
        "CARGO_PKG_RUST_VERSION",
        meta.rust_version.as_ref().map(ToString::to_string).unwrap_or_default(),
    );
    set("CARGO_CRATE_NAME", unit.target.name.replace('-', "_"));
    if primary {
        set("CARGO_PRIMARY_PACKAGE", "1".to_owned());
    }
    if matches!(unit.target.kind, TargetKind::Bin | TargetKind::Example) {
        set("CARGO_BIN_NAME", unit.target.name.clone());
    }
    Ok(env)
}

/// Plan every root of a validated replay before starting a compiler. Libraries
/// produce native objects and check units use the frontend without code generation.
/// Doctests, build scripts and proc macros need their owned execution contracts.
pub fn programs(
    prepared: &Prepared,
    sysroot: &Path,
    artifacts: &Path,
) -> anyhow::Result<Vec<Program>> {
    programs_with(prepared, sysroot, artifacts, None)
}

/// Check execution shapes before a producer or compiler is started.
pub fn preflight(prepared: &Prepared, allow_producers: bool) -> anyhow::Result<()> {
    let build = &prepared.resolved.spec.build;
    let native = canonical_units(build, &prepared.resolved.graph)?;
    for (at, unit) in native.iter().enumerate() {
        let label = format!("{}:{}", unit.package.name, unit.target.name);
        if !allow_producers {
            match unit.target.kind {
                TargetKind::CustomBuild => {
                    bail!("owned BuildSpec replay has no build-script producer for {label}")
                }
                TargetKind::ProcMacro => {
                    bail!("owned BuildSpec replay has no proc-macro producer for {label}")
                }
                _ => {}
            }
        }
        if unit.target.kind == TargetKind::CustomBuild {
            continue;
        }
        ensure!(
            allow_producers || !unit.for_host,
            "owned BuildSpec replay cannot collapse host unit {label} into target units"
        );
        ensure!(
            unit.target
                .crate_types
                .iter()
                .all(|kind| matches!(kind.as_str(), "lib" | "rlib" | "bin" | "proc-macro")),
            "owned BuildSpec replay cannot emit {:?} for {label}",
            unit.target.crate_types
        );
        ensure!(
            unit.dependencies.iter().all(|dep| !dep.noprelude && !dep.public && !dep.nounused),
            "owned BuildSpec replay cannot represent special extern options of {label}"
        );
        ensure!(
            build.units[at].profile.strip != 2,
            "owned BuildSpec replay does not yet strip native symbols for {label}"
        );
    }
    for root in roots(build, &native) {
        let root_profile = &build.units[root as usize].profile;
        for at in reachable(build, &native, root) {
            let unit = native[at as usize];
            let profile = &build.units[at as usize].profile;
            ensure!(
                unit.target.kind == TargetKind::ProcMacro
                    || (profile.debug, profile.strip, profile.split_debuginfo)
                        == (root_profile.debug, root_profile.strip, root_profile.split_debuginfo),
                "owned BuildSpec replay cannot mix per-unit debug/strip/split settings in {}:{}",
                unit.package.name,
                unit.target.name
            );
        }
    }
    Ok(())
}

/// Plan roots using the owned scheduler's completed source/producer settings. The
/// caller retains the producer generation leases through compilation and admission.
pub fn programs_with(
    prepared: &Prepared,
    sysroot: &Path,
    artifacts: &Path,
    produced: Option<&BTreeMap<u32, ProducedUnit>>,
) -> anyhow::Result<Vec<Program>> {
    let build = &prepared.resolved.spec.build;
    let native = canonical_units(build, &prepared.resolved.graph)?;
    preflight(prepared, produced.is_some())?;
    if let Some(produced) = produced {
        for (at, unit) in native.iter().enumerate() {
            if unit.target.kind == TargetKind::CustomBuild {
                continue;
            }
            let configured = produced
                .get(&(at as u32))
                .with_context(|| format!("missing configured canonical unit {at}"))?;
            ensure!(
                unit.target.kind != TargetKind::ProcMacro || configured.proc_macro.is_some(),
                "missing owned macro image for {}:{}",
                unit.package.name,
                unit.target.name
            );
        }
    }
    // Cargo builds auxiliary binaries before tests that use CARGO_BIN_EXE_*.
    let roots = roots(build, &native);
    let executable = |at| {
        artifacts
            .join(format!("unit-{at}"))
            .join(format!("program{}", std::env::consts::EXE_SUFFIX))
    };
    let execution_env = prepared.execution_environment()?;
    let manifests = prepared.manifests()?;
    let mut result = Vec::new();
    for &root in &roots {
        let root_unit = &build.units[root as usize];
        let mut order = vec![root];
        order.extend(reachable(build, &native, root).into_iter().filter(|&at| at != root));
        let indexes: BTreeMap<_, _> =
            order.iter().enumerate().map(|(at, &original)| (original, at)).collect();
        let mut crates = Vec::new();
        let mut native_units = BTreeMap::new();
        for &at in &order {
            let spec = &build.units[at as usize];
            let unit = native[at as usize];
            let configured = produced.and_then(|units| units.get(&at));
            let mut crate_env = execution_env.clone();
            let primary = build
                .asked
                .iter()
                .any(|&asked| native[asked as usize].package.id == unit.package.id);
            crate_env.extend(package_env(unit, &manifests, primary)?);
            if let Some(configured) = configured {
                crate_env = configured
                    .native
                    .env
                    .iter()
                    .filter_map(|(name, value)| Some((name.to_str()?.to_owned(), value.clone())))
                    .collect();
            }
            if unit.test {
                for &binary in &roots {
                    let other = native[binary as usize];
                    if !other.test
                        && other.target.kind == TargetKind::Bin
                        && other.package.id == unit.package.id
                    {
                        crate_env.insert(
                            format!("CARGO_BIN_EXE_{}", other.target.name),
                            executable(binary).into_os_string(),
                        );
                    }
                }
            }
            let kind = if spec.test {
                "test"
            } else if matches!(unit.target.kind, TargetKind::Lib | TargetKind::ProcMacro) {
                "lib"
            } else {
                "bin"
            };
            let cfg =
                configured.map_or_else(|| unit_cfg(prepared, at as usize), |unit| unit.cfg.clone());
            let codegen_flags = configured
                .map_or_else(|| flags(&spec.profile, build), |unit| unit.codegen_flags.clone());
            let invalid_env: Vec<_> = crate_env
                .iter()
                .filter(|(_, value)| value.to_str().is_none())
                .map(|(name, _)| name.clone())
                .collect();
            let text_env: BTreeMap<_, _> = crate_env
                .iter()
                .filter_map(|(name, value)| Some((name, value.to_str()?)))
                .collect();
            native_units.insert(
                at,
                configured.map_or_else(
                    || blaze_wire::native::Unit {
                        env: crate_env
                            .iter()
                            .map(|(name, value)| (name.into(), value.clone()))
                            .collect(),
                        flags: flags(&spec.profile, build).into_iter().map(Into::into).collect(),
                    },
                    |unit| {
                        let mut native = unit.native.clone();
                        native.env.extend(
                            crate_env.iter().map(|(name, value)| (name.into(), value.clone())),
                        );
                        native
                    },
                ),
            );
            let dependencies: Vec<_> = if unit.target.kind == TargetKind::ProcMacro {
                Vec::new()
            } else {
                spec.deps
                    .iter()
                    .filter_map(|(dep, name)| {
                        indexes.get(dep).map(|index| json!({"crate": index, "name": name}))
                    })
                    .collect()
            };
            crates.push(json!({
                "display_name": unit.target.name.replace('-', "_"),
                "root_module": compiler_path(&unit.target.src_path),
                "edition": spec.edition.to_string(),
                "version": unit.package.version,
                "deps": dependencies,
                "cfg": cfg,
                "codegen_flags": codegen_flags,
                "env": text_env,
                "blaze_invalid_env": invalid_env,
                "blaze_native_unit": at,
                "is_workspace_member": at == root,
                "is_proc_macro": unit.target.kind == TargetKind::ProcMacro,
                "proc_macro_dylib_path": configured.and_then(|unit| unit.proc_macro.as_deref()).map(compiler_path),
                "build": {"label": unit.target.name, "build_file": compiler_path(&unit.package.manifest_dir.join("Cargo.toml")), "target_kind": kind},
            }));
        }
        let unit = native[root as usize];
        let output = if unit.mode == Mode::Check {
            Output::Check
        } else if is_program(unit) || unit.target.kind == TargetKind::Bench {
            Output::Executable
        } else {
            Output::Object
        };
        let project = json!({
            "compiler_mode": true,
            "blaze_native_units": blaze_wire::native::encode_units(&native_units),
            "blaze_spec_target": prepared.host.triple,
            "sysroot": compiler_path(sysroot),
            "sysroot_src": compiler_path(&sysroot.join("lib/rustlib/src/rust/library")),
            "crates": crates,
            "blaze_spec_profile": {"version": 1, "profile": blaze_wire::encode(&root_unit.profile), "rustflags": build.rustflags.iter().map(|flag| flag.0.clone()).collect::<Vec<_>>()},
        });
        let run = match &build.command {
            Command::Run(_) => true,
            Command::Test(run) => unit.test && !run.no_run,
            _ => false,
        };
        result.push(Program {
            unit: root,
            crate_name: unit.target.name.replace('-', "_"),
            harness: root_unit.test,
            run,
            output,
            executable: match output {
                Output::Executable => executable(root),
                Output::Object => artifacts.join(format!("unit-{root}/library.o")),
                Output::Check => artifacts.join(format!("unit-{root}/checked.json")),
                Output::Doctest => unreachable!(),
            },
            project,
        });
    }
    if let Command::Test(run) = &build.command
        && run.doc != 0
        && !run.no_run
    {
        let selected: BTreeSet<_> =
            roots.iter().map(|&at| &native[at as usize].package.id).collect();
        let libraries: Vec<_> = native
            .iter()
            .enumerate()
            .filter(|(_, unit)| {
                matches!(unit.target.kind, TargetKind::Lib | TargetKind::ProcMacro)
                    && unit.mode == Mode::Build
                    && unit.target.doctest
                    && selected.contains(&unit.package.id)
            })
            .collect();
        for (at, unit) in libraries {
            // The library's source-only description is already needed by a root
            // executable or by --doc itself. Reuse its exact crate graph/profile.
            let project = result
                .iter()
                .find_map(|program| {
                    let mut project = program.project.clone();
                    let crates = project["crates"].as_array_mut()?;
                    let root = crates.iter().position(|krate| {
                        krate["blaze_native_unit"] == json!(at)
                            && krate["build"]["target_kind"] == "lib"
                    })?;
                    for krate in crates.iter_mut() {
                        krate["is_workspace_member"] = false.into();
                    }
                    crates[root]["is_workspace_member"] = true.into();
                    project["blaze_spec_doctest"] = json!(blaze_wire::encode(run));
                    project["blaze_spec_profile"]["profile"] =
                        json!(blaze_wire::encode(&build.units[at].profile));
                    Some(project)
                })
                .context("doctest library has no source description")?;
            result.push(Program {
                unit: at as u32,
                crate_name: unit.target.name.replace('-', "_"),
                harness: false,
                run: true,
                output: Output::Doctest,
                executable: artifacts.join(format!("doctest-{at}/checked.json")),
                project,
            });
        }
    }
    Ok(result)
}
