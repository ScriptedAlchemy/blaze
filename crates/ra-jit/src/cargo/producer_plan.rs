//! Host source producers from the application's own resolver and profile planner.

use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
};

use anyhow::{Context as _, Result};
use blaze_manifest::{
    config::{CompileKind, Flags},
    profiles::{Profiles, UnitFor},
};
use blaze_metadata::{ResolvedUnit, TargetPlatform};
use cargo_metadata::Metadata;
use serde_json::json;

use super::own_script::Planned;

pub(crate) const COMMAND: &str = "proc-macro-plan";
pub(crate) const BUILD_COMMAND: &str = "proc-macro-build";

/// Build outputs must be attached by the scheduler before compilation. Resolution alone
/// cannot substitute an empty OUT_DIR, a nested macro image or generated cfg.
pub(crate) struct Producers {
    pub(crate) metadata: Metadata,
    pub(crate) units: Vec<ResolvedUnit>,
    pub(crate) crates: Vec<Planned>,
    pub(crate) sources: Vec<(std::path::PathBuf, Vec<u8>)>,
    scripts: BTreeSet<String>,
    /// Native host script units whose complete key matches a target-domain unit.
    shared_script_units: BTreeSet<String>,
    /// Request/config values travel to compiler children, never into crate descriptions.
    compile_env: crate::process::Native,
}

impl Producers {
    /// Reindex a source closure with the compiled producer at slot zero. A subsequent
    /// scheduler owns script outputs and nested images; do not compile without them.
    fn source_closure(&self, root: usize) -> Result<Vec<Planned>> {
        anyhow::ensure!(
            self.units.get(root).is_some_and(|it| it.host && it.proc_macro),
            "the requested unit is not a host proc-macro producer"
        );
        fn visit(plan: &Producers, at: usize, root: usize, order: &mut Vec<usize>) -> Result<()> {
            if order.contains(&at) {
                return Ok(());
            }
            let unit = &plan.units[at];
            anyhow::ensure!(unit.host, "a producer dependency is not a host unit");
            anyhow::ensure!(
                !unit.build_script || plan.scripts.contains(&unit.package),
                "the producer scheduler must run the build script of {}",
                unit.package
            );
            anyhow::ensure!(
                at == root || !unit.proc_macro,
                "the producer scheduler must build the nested proc macro {}",
                unit.package
            );
            order.push(at);
            for dep in &unit.dependencies {
                if !dep.build {
                    visit(plan, dep.unit, root, order)?;
                }
            }
            Ok(())
        }
        let mut order = Vec::new();
        visit(self, root, root, &mut order)?;
        let slots: BTreeMap<_, _> =
            order.iter().enumerate().map(|(new, old)| (*old, new)).collect();
        let crates = order
            .into_iter()
            .map(|at| {
                let mut krate = self.crates[at].clone();
                krate.deps =
                    krate.deps.into_iter().map(|(name, old)| (name, slots[&old])).collect();
                krate
            })
            .collect::<Vec<_>>();
        // Source graph validation, including cycles, happens before starting any child.
        super::own_script::planned_macro_description(
            Path::new("/unused-source-sysroot"),
            crates.clone(),
        )
        .map_err(anyhow::Error::msg)?;
        Ok(crates)
    }

    /// A CargoWorkspace is only an adapter for the existing owned script runner here.
    /// Its nodes come from the host units, never metadata's folded feature set.
    fn host_metadata(&self, target_dir: &Path) -> Result<Metadata> {
        let mut metadata = serde_json::to_value(&self.metadata)?;
        let units = &self.units;
        let nodes = units.iter().filter(|unit| unit.host).map(|unit| {
            let deps = unit.dependencies.iter().map(|dep| {
                let child = &units[dep.unit];
                anyhow::ensure!(child.host, "a host script dependency is not a host unit");
                Ok(json!({
                    "name": dep.name, "pkg": child.package,
                    "dep_kinds": [{"kind": if dep.build { "build" } else { "normal" }, "target": null}],
                }))
            }).collect::<Result<Vec<_>>>()?;
            Ok(json!({
                "id": unit.package, "features": unit.features, "deps": deps,
                "dependencies": unit.dependencies.iter().map(|dep| &units[dep.unit].package).collect::<Vec<_>>(),
            }))
        }).collect::<Result<Vec<_>>>()?;
        metadata["resolve"] = json!({"root": null, "nodes": nodes});
        metadata["target_directory"] = json!(target_dir);
        serde_json::from_value(metadata).map_err(Into::into)
    }

    /// Libraries compiled with a script use host units. A target-only package remains
    /// as the script root, but only its build edges enter that compilation.
    pub(crate) fn script_metadata(&self, target_dir: &Path) -> Result<Metadata> {
        let mut metadata = serde_json::to_value(&self.metadata)?;
        let mut selected = BTreeMap::new();
        for unit in &self.units {
            if unit.host || !selected.contains_key(&unit.package) {
                selected.insert(unit.package.clone(), unit);
            }
        }
        let nodes = selected.values().map(|unit| {
            let deps = unit.dependencies.iter().filter(|dep| unit.host || dep.build)
                .map(|dep| {
                    let child = &self.units[dep.unit];
                    anyhow::ensure!(child.host, "a script compile dependency is not a host unit");
                    Ok(json!({
                        "name": dep.name, "pkg": child.package,
                        "dep_kinds": [{"kind": if dep.build { "build" } else { "normal" }, "target": null}],
                    }))
                }).collect::<Result<Vec<_>>>()?;
            Ok(json!({"id": unit.package, "features": unit.features, "deps": deps,
                "dependencies": unit.dependencies.iter().filter(|dep| unit.host || dep.build)
                    .map(|dep| &self.units[dep.unit].package).collect::<Vec<_>>()}))
        }).collect::<Result<Vec<_>>>()?;
        metadata["resolve"] = json!({"root": null, "nodes": nodes});
        metadata["target_directory"] = json!(target_dir);
        serde_json::from_value(metadata).map_err(Into::into)
    }

    fn run_scripts(
        &mut self,
        selection: &crate::plan::Selection,
        sysroot: &Path,
        host: &str,
        dir: &Path,
        build: u64,
    ) -> Result<(crate::scripts::World, BTreeMap<String, std::path::PathBuf>)> {
        let metadata = self.host_metadata(&dir.join("scripts"))?;
        let manifest = project_model::ManifestPath::try_from(paths::AbsPathBuf::assert_utf8(
            metadata.workspace_root.join("Cargo.toml").into_std_path_buf(),
        ))
        .map_err(|path| anyhow::anyhow!("{path} is not a manifest path"))?;
        let cargo = project_model::CargoWorkspace::new(
            metadata,
            manifest,
            ide_db::base_db::Env::default(),
            false,
        );
        let packages = self
            .units
            .iter()
            .filter(|unit| unit.host)
            .map(|unit| {
                let shared = self.shared_script_units.contains(&unit.package);
                (unit.package.clone(), shared)
            })
            .collect();
        let mut env: BTreeMap<String, String> =
            crate::process::unicode(self.compile_env.vars()).collect();
        let probe = dir.join("producer-rustc");
        crate::answers::producer_launcher(&probe)?;
        env.insert("RUSTC".into(), probe.display().to_string());
        env.insert("RUSTDOC".into(), probe.display().to_string());
        env.remove("RUSTC_WRAPPER");
        env.remove("RUSTC_WORKSPACE_WRAPPER");
        let failure = dir.join("unsupported-probe");
        match std::fs::remove_file(&failure) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        env.insert("BLAZE_PRODUCER_PROBE_FAILURE".into(), failure.display().to_string());
        let root = self
            .units
            .iter()
            .position(|unit| unit.proc_macro)
            .context("the plan has no producer")?;
        // The first flag belongs to the library profile. The runner calculates the
        // build-script profile independently; only configured flags follow it.
        let flags = self.crates[root].codegen_flags.iter().skip(1).cloned().collect();
        let cfgs = crate::answers::target_cfg(sysroot, host, &env)
            .context("blaze has no owned cfg for the producer host")?;
        let asked = crate::scripts::Asked {
            env: &env,
            build,
            profile: &selection.profile,
            config: &selection.config,
            host,
            target: host,
            explicit_target: selection.target.is_some(),
            packages: Some(&packages),
            host_cargo: None,
        };
        let mut raw: BTreeMap<_, _> = self.compile_env.vars().collect();
        raw.extend(env.iter().map(|(name, value)| (name.into(), value.into())));
        raw.remove(std::ffi::OsStr::new("RUSTC_WRAPPER"));
        raw.remove(std::ffi::OsStr::new("RUSTC_WORKSPACE_WRAPPER"));
        let (outputs, images, world) =
            crate::scripts::run_producers(&cargo, &asked, sysroot.to_owned(), flags, cfgs, &raw)?;
        anyhow::ensure!(
            !failure.exists(),
            "blaze could not compile a producer's compiler probe: {}",
            std::fs::read_to_string(&failure).unwrap_or_default(),
        );
        for (unit, krate) in self.units.iter().zip(&mut self.crates) {
            if unit.host
                && let Some((out_dir, output)) = outputs.get(&unit.package)
            {
                krate.cfg.extend(output.cfgs.clone());
                krate.env.extend(output.env.clone());
                krate.env.insert("OUT_DIR".into(), out_dir.display().to_string());
                self.scripts.insert(unit.package.clone());
            }
        }
        Ok((world, images))
    }
}

pub(crate) fn of(
    manifest: &Path,
    selection: &crate::plan::Selection,
    host: &str,
    target: &str,
    env: BTreeMap<String, String>,
) -> Result<Producers> {
    anyhow::ensure!(
        selection.target.as_deref().unwrap_or(host) == target,
        "the target facts do not match the application's selection"
    );
    let sysroot = env
        .get("RUSTUP_TOOLCHAIN")
        .map(std::path::PathBuf::from)
        .filter(|it| it.is_absolute())
        .context("the producer plan needs an absolute source toolchain")?;
    let cfg = |triple| {
        crate::answers::target_cfg(&sysroot, triple, &env)
            .with_context(|| format!("blaze has no owned cfg for `{triple}`"))?
            .into_iter()
            .map(|it| it.parse::<cargo_platform::Cfg>().map_err(anyhow::Error::from))
            .collect::<Result<Vec<_>>>()
    };
    let host_cfg = cfg(host)?;
    let target_cfg = cfg(target)?;
    let cwd = match selection.asked_in.as_deref() {
        Some(it) if it.is_dir() => it,
        Some(it) => it.parent().context("the selected manifest has no directory")?,
        None => manifest.parent().context("the application manifest has no directory")?,
    };
    let asked = blaze_metadata::Asked {
        cwd,
        env: env.clone().into_iter().collect(),
        manifest_path: Some(manifest),
        config: &selection.config,
        features: &selection.features,
        all_features: selection.all_features,
        no_default_features: selection.no_default_features,
        build: Some(blaze_metadata::Build {
            packages: &selection.packages,
            workspace: selection.workspace,
            excludes: &selection.excludes,
            dev_units: selection.dev_deps,
        }),
        platform: selection
            .target
            .as_ref()
            .map(|_| TargetPlatform { triple: target, cfg: &target_cfg }),
        offline: selection.offline,
        locked: selection.locked,
        store: None,
    };
    let (metadata, units, mut sources) =
        blaze_metadata::build_units(&asked, &TargetPlatform { triple: host, cfg: &host_cfg })?;
    let metadata: Metadata = serde_json::from_str(&metadata)?;
    let mut config = blaze_manifest::Config::load(cwd, env.clone(), &[])?;
    config.start("build", &selection.config)?;
    let requested = match &selection.target {
        Some(target) => CompileKind::Target(target.clone()),
        None => CompileKind::Host,
    };
    let host_flags = config.extra_args(
        std::slice::from_ref(&requested),
        host,
        Some(&host_cfg),
        &CompileKind::Host,
        Flags::Rust,
    )?;
    let target_flags = config.extra_args(
        std::slice::from_ref(&requested),
        host,
        Some(&target_cfg),
        &requested,
        Flags::Rust,
    )?;
    let ctx = blaze_manifest::Context::new(config);
    let root = metadata.workspace_root.join("Cargo.toml");
    let root = blaze_manifest::metadata::root_manifest(Some(root.as_std_path()), &ctx)?;
    let ws = blaze_manifest::Workspace::new(&root, &ctx)?;
    let profiles = Profiles::new(
        ws.profiles(),
        ws.unstable_features(),
        &ctx.config,
        &selection.profile,
        host,
    )?;
    let config_env: BTreeMap<String, String> = ctx
        .config
        .env_config()?
        .into_iter()
        .map(|(key, value)| {
            let value =
                value.into_string().map_err(|_| anyhow::anyhow!("non-UTF-8 env `{key}`"))?;
            Ok((key, value))
        })
        .collect::<Result<_>>()?;
    let manifest_path = project_model::ManifestPath::try_from(paths::AbsPathBuf::assert_utf8(
        metadata.workspace_root.join("Cargo.toml").into_std_path_buf(),
    ))
    .map_err(|path| anyhow::anyhow!("{path} is not a manifest path"))?;
    let cargo = project_model::CargoWorkspace::new(
        metadata.clone(),
        manifest_path,
        ide_db::base_db::Env::default(),
        false,
    );
    let mut shared_script_units = BTreeSet::new();
    for unit in units.iter().filter(|it| it.host && it.build_script) {
        let Some(other) = units.iter().find(|it| !it.host && it.package == unit.package) else {
            continue;
        };
        let package = cargo
            .packages()
            .find(|&it| cargo[it].id.repr == unit.package)
            .with_context(|| format!("no package for script unit {}", unit.package))?;
        let data = &cargo[package];
        let named = crate::scripts::named(data);
        let host_profile = profiles.get_profile(
            &named,
            data.is_member,
            data.is_local,
            UnitFor::new_host(false),
            &CompileKind::Host,
        );
        let target_profile = profiles.get_profile(
            &named,
            data.is_member,
            data.is_local,
            UnitFor::new_normal(),
            &requested,
        );
        if script_units_share(&unit.features, &other.features, &host_profile, &target_profile)
            && host_flags == target_flags
        {
            shared_script_units.insert(unit.package.clone());
        }
    }
    let mut crates = Vec::new();
    for unit in &units {
        let package = cargo
            .packages()
            .find(|&it| cargo[it].id.repr == unit.package)
            .with_context(|| format!("no package for unit {}", unit.package))?;
        let data = &cargo[package];
        let target = data
            .targets
            .iter()
            .copied()
            .find(|&it| matches!(cargo[it].kind, ide_db::base_db::TargetKind::Lib { .. }));
        // A non-library selected root identifies the producers its program reaches.
        let target =
            target.or_else(|| data.targets.first().copied()).context("unit has no target")?;
        let target_data = &cargo[target];
        let flags = if unit.host { &host_flags } else { &target_flags };
        let kind = if unit.host { CompileKind::Host } else { requested.clone() };
        let profile = profiles.get_profile(
            &crate::scripts::named(data),
            data.is_member,
            data.is_local,
            if unit.host { UnitFor::new_host(true) } else { UnitFor::new_normal() },
            &kind,
        );
        let effective = crate::plan::Profile::for_script(&profile, flags)?;
        let mut cfg: Vec<_> = unit.features.iter().map(|it| format!("feature={it:?}")).collect();
        if effective.debug_assertions {
            cfg.push("debug_assertions".into());
        }
        if effective.overflow_checks {
            cfg.push("overflow_checks".into());
        }
        cfg.push(format!("panic={:?}", effective.panic.name()));
        let mut it = flags.iter();
        while let Some(flag) = it.next() {
            if flag == "--cfg" {
                cfg.push(it.next().context("--cfg has no value")?.clone());
            } else if let Some(value) = flag.strip_prefix("--cfg=") {
                cfg.push(value.to_owned());
            }
        }
        let mut crate_env = BTreeMap::new();
        let mut set = |name: &str, value: String| {
            crate_env.insert(name.into(), value);
        };
        set("CARGO_MANIFEST_DIR", data.manifest.parent().to_string());
        set("CARGO_MANIFEST_PATH", data.manifest.to_string());
        set("CARGO_PKG_NAME", data.name.clone());
        set("CARGO_PKG_VERSION", data.version.to_string());
        set("CARGO_PKG_VERSION_MAJOR", data.version.major.to_string());
        set("CARGO_PKG_VERSION_MINOR", data.version.minor.to_string());
        set("CARGO_PKG_VERSION_PATCH", data.version.patch.to_string());
        set("CARGO_PKG_VERSION_PRE", data.version.pre.to_string());
        set("CARGO_PKG_AUTHORS", data.authors.join(":"));
        set("CARGO_PKG_DESCRIPTION", data.description.clone().unwrap_or_default());
        set("CARGO_PKG_HOMEPAGE", data.homepage.clone().unwrap_or_default());
        set("CARGO_PKG_REPOSITORY", data.repository.clone().unwrap_or_default());
        set("CARGO_PKG_LICENSE", data.license.clone().unwrap_or_default());
        set(
            "CARGO_PKG_LICENSE_FILE",
            data.license_file.as_ref().map(ToString::to_string).unwrap_or_default(),
        );
        set("CARGO_PKG_README", data.readme.as_ref().map(ToString::to_string).unwrap_or_default());
        set(
            "CARGO_PKG_RUST_VERSION",
            data.rust_version.as_ref().map(ToString::to_string).unwrap_or_default(),
        );
        set("CARGO_CRATE_NAME", target_data.name.replace('-', "_"));
        crates.push(Planned {
            workspace_member: data.is_member,
            proc_macro: None,
            name: target_data.name.replace('-', "_"),
            root: AsRef::<Path>::as_ref(&target_data.root).to_owned(),
            edition: data.edition.to_string(),
            cfg,
            codegen_flags: std::iter::once(format!("-Copt-level={}", profile.opt_level))
                .chain(flags.iter().cloned())
                .collect(),
            env: crate_env,
            deps: unit
                .dependencies
                .iter()
                .filter(|it| !it.build)
                .map(|it| (it.name.clone(), it.unit))
                .collect(),
        });
    }
    sources.extend(ctx.sources());
    Ok(Producers {
        metadata,
        units,
        crates,
        sources,
        scripts: BTreeSet::new(),
        shared_script_units,
        compile_env: crate::process::native()
            .over(env.iter().map(|(k, v)| (k.into(), v.into())))
            .over(config_env.into_iter().map(|(k, v)| (k.into(), v.into()))),
    })
}

fn script_units_share(
    host_features: &[String],
    target_features: &[String],
    host: &blaze_manifest::profiles::Profile,
    target: &blaze_manifest::profiles::Profile,
) -> bool {
    // Cargo's profile equality ignores deferred vs resolved debug-info wrappers
    // and compares their canonical values, plus the other codegen settings.
    let host_features = host_features.iter().collect::<BTreeSet<_>>();
    let target_features = target_features.iter().collect::<BTreeSet<_>>();
    host_features == target_features && host == target
}

/// A diagnostic rendering for the independent harness, not an alternate Cargo unit graph.
pub(crate) fn command(
    mut args: impl Iterator<Item = std::ffi::OsString>,
    build: bool,
) -> Result<()> {
    let manifest = std::path::absolute(args.next().context("the application manifest is first")?)?;
    let selection = std::path::PathBuf::from(args.next().context("then the build selection")?);
    let host = args
        .next()
        .context("then the host triple")?
        .into_string()
        .map_err(|_| anyhow::anyhow!("host is not UTF-8"))?;
    let target = args
        .next()
        .context("then the target triple")?
        .into_string()
        .map_err(|_| anyhow::anyhow!("target is not UTF-8"))?;
    let dir = if build {
        Some(std::path::absolute(args.next().context("then an exclusive build directory")?)?)
    } else {
        None
    };
    anyhow::ensure!(args.next().is_none(), "unexpected producer plan argument");
    let selection = serde_json::from_slice(&std::fs::read(selection)?)?;
    let mut planned = of(&manifest, &selection, &host, &target, crate::process::vars().collect())?;
    if let Some(dir) = dir {
        std::fs::create_dir_all(&dir)?;
        let sysroot = std::env::var_os("RUSTUP_TOOLCHAIN")
            .map(std::path::PathBuf::from)
            .context("the source toolchain is not named")?;
        // Validate graph edges and cycles before any child; nested producer
        // readiness is established by the recursive script/image scheduler.
        super::own_script::planned_macro_description(&sysroot, planned.crates.clone())
            .map_err(anyhow::Error::msg)?;
        let build = crate::scripts::anonymous();
        struct Release(u64);
        impl Drop for Release {
            fn drop(&mut self) {
                crate::scripts::release(self.0);
            }
        }
        let _release = Release(build);
        let _operation = crate::scripts::operation(build);
        let (world, images) = planned.run_scripts(&selection, &sysroot, &host, &dir, build)?;
        let _pin = crate::scripts::pin(build, &world)?;
        let libraries = images
            .into_iter()
            .map(|(package, library)| json!({"package": package, "library": library}))
            .collect::<Vec<_>>();
        println!("{}", json!(libraries));
        return Ok(());
    }
    println!(
        "{}",
        json!({
            "metadata": planned.metadata, "units": planned.units, "crates": planned.crates,
            "sources": planned.sources.iter().map(|(path, _)| path).collect::<Vec<_>>(),
        })
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plan() -> Producers {
        let unit = |name: &str, proc_macro: bool, deps: Vec<blaze_metadata::UnitDependency>| {
            ResolvedUnit {
                package: name.into(),
                host: true,
                features: vec!["host".into()],
                proc_macro,
                build_script: false,
                dependencies: deps,
            }
        };
        let edge =
            || blaze_metadata::UnitDependency { name: "renamed".into(), unit: 0, build: false };
        let crates = ["dep", "derive"]
            .into_iter()
            .map(|name| Planned {
                workspace_member: false,
                proc_macro: None,
                name: name.into(),
                root: format!("/source/{name}/lib.rs").into(),
                edition: "2021".into(),
                cfg: vec!["feature=\"host\"".into()],
                codegen_flags: vec!["-Copt-level=1".into()],
                env: BTreeMap::new(),
                deps: if name == "derive" { vec![("renamed".into(), 0)] } else { vec![] },
            })
            .collect();
        Producers {
            metadata: serde_json::from_value(json!({
                "packages": [], "workspace_members": [], "workspace_root": "/source",
                "target_directory": "/source/target", "version": 1,
            }))
            .unwrap(),
            units: vec![unit("dep", false, vec![]), unit("derive", true, vec![edge()])],
            crates,
            sources: vec![],
            scripts: BTreeSet::new(),
            shared_script_units: BTreeSet::new(),
            compile_env: crate::process::Native::default(),
        }
    }

    #[test]
    fn script_sharing_requires_features_and_canonical_profile_equality() {
        use blaze_manifest::profiles::{DebugInfo, Profile};
        let mut host = Profile::default();
        let mut target = host.clone();
        let features = vec!["common".to_owned()];
        assert!(script_units_share(&features, &features, &host, &target));
        assert!(!script_units_share(&features, &["other".into()], &host, &target));
        target.opt_level = "1".into();
        assert!(!script_units_share(&features, &features, &host, &target));
        target = host.clone();
        host.debuginfo = DebugInfo::Deferred(target.debuginfo.into_inner());
        assert!(script_units_share(&features, &features, &host, &target));
        target.debug_assertions = !host.debug_assertions;
        assert!(!script_units_share(&features, &features, &host, &target));
    }

    #[test]
    fn host_adapter_does_not_fold_target_features_or_build_edges() {
        let mut plan = plan();
        plan.units[0].features = vec!["host".into()];
        let mut target = plan.units[0].clone();
        target.host = false;
        target.features = vec!["target".into()];
        plan.units.push(target);
        plan.units[1].dependencies.push(blaze_metadata::UnitDependency {
            name: "build_renamed".into(),
            unit: 0,
            build: true,
        });
        let metadata = plan.host_metadata(Path::new("/out/scripts")).unwrap();
        let nodes = metadata.resolve.unwrap().nodes;
        assert_eq!(nodes.len(), 2);
        assert_eq!(nodes[0].features, ["host"]);
        assert_eq!(nodes[1].deps.len(), 2);
        let build = nodes[1].deps.iter().find(|dep| dep.name == "build_renamed").unwrap();
        assert_eq!(build.dep_kinds[0].kind, cargo_metadata::DependencyKind::Build);
    }

    #[test]
    fn source_closure_reindexes_root_and_never_omits_unbuilt_producers() {
        let mut plan = plan();
        let closure = plan.source_closure(1).unwrap();
        assert_eq!(closure[0].name, "derive");
        assert_eq!(closure[0].deps, [("renamed".into(), 1)]);
        assert_eq!(closure[1].cfg, ["feature=\"host\""]);
        assert!(plan.source_closure(0).is_err());
        plan.units[0].build_script = true;
        assert!(plan.source_closure(1).err().unwrap().to_string().contains("build script"));
        plan.scripts.insert("dep".into());
        assert!(plan.source_closure(1).is_ok());
        plan.scripts.clear();
        plan.units[0].build_script = false;
        plan.units[0].proc_macro = true;
        assert!(plan.source_closure(1).err().unwrap().to_string().contains("nested"));
        plan.units[0].proc_macro = false;
        plan.units[0].host = false;
        assert!(plan.source_closure(1).is_err());
    }

    #[test]
    fn discovered_sysroot_tool_paths_need_no_rustup_process() {
        let root = paths::AbsPathBuf::assert_utf8(std::env::temp_dir().join("no-rustup-tool-path"));
        let sysroot = project_model::Sysroot::new(Some(root.clone()), None);
        let env = ide_db::base_db::Env::default();
        assert_eq!(
            sysroot.tool_path(toolchain::Tool::Cargo, &root, &env),
            paths::Utf8PathBuf::from(
                root.join("bin").join(format!("cargo{}", std::env::consts::EXE_SUFFIX))
            )
        );
        let env = [("CARGO".to_owned(), "/explicit/cargo".to_owned())].into_iter().collect();
        assert_eq!(
            sysroot.tool_path(toolchain::Tool::Cargo, &root, &env),
            paths::Utf8PathBuf::from("/explicit/cargo")
        );
    }
}
