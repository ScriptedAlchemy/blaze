//! The workspace as `cargo metadata --format-version 1` prints it, read by blaze-manifest as
//! cargo 1.99.0 reads it and resolved by blaze-resolve, with no cargo run (docs/resolve.md).

use std::path::Path;

use anyhow::Context as _;
use blaze_manifest::diagnostic::ManifestDiagnostic;
use blaze_manifest::metadata::{export_no_deps, root_manifest};
use blaze_manifest::{Config, Context, Workspace};
use cargo_metadata::Metadata;

/// The workspace found from the current directory, or from `manifest_path`, with cargo's
/// `--config` arguments `config`. A workspace cargo refuses ends the command as cargo ends:
/// its message, status 101.
pub(crate) fn no_deps(manifest_path: Option<&Path>, config: &[String]) -> Metadata {
    let cwd = std::env::current_dir().unwrap_or_default();
    match no_deps_in(&cwd, manifest_path, config, crate::process::vars().collect()) {
        Ok(metadata) => metadata,
        Err(err) => {
            refused(&err, &cwd);
            std::process::exit(101)
        }
    }
}

/// Read from the invoking Cargo's directory, not the directory it runs rustc in.
/// A caller that only collects provenance must not end a compile on a reader error.
pub(super) fn no_deps_in(
    cwd: &Path,
    manifest_path: Option<&Path>,
    config: &[String],
    env: Vec<(String, String)>,
) -> anyhow::Result<Metadata> {
    let mut process = Config::load(cwd, env, &[])?;
    process.start("metadata", config)?;
    let manifest = manifest_path.map(|path| cwd.join(path));
    read(process, manifest.as_deref())
}

fn read(config: Config, manifest_path: Option<&Path>) -> anyhow::Result<Metadata> {
    let ctx = Context::new(config);
    let root = root_manifest(manifest_path, &ctx)?;
    let ws = Workspace::new(&root, &ctx)?;
    let json = serde_json::to_value(export_no_deps(&ws))?;
    serde_json::from_value(json).context("blaze's own reading of the workspace is not cargo's form")
}

/// How the daemon's loads read a workspace for the build `selection` names: blaze's own
/// reading and resolution, the graph that of the build, its features not unified over the
/// other members. `RA_JIT_OWN_RESOLVE=0` leaves both to cargo.
pub(crate) fn own(selection: &crate::plan::Selection) -> Option<project_model::OwnMetadata> {
    if std::env::var_os("RA_JIT_OWN_RESOLVE").is_some_and(|it| it == "0") {
        return None;
    }
    let selection = selection.clone();
    let offline = selection.offline;
    let root = |manifest: &project_model::ManifestPath| {
        let manifest: &Path = manifest.as_ref();
        let cwd = manifest.parent().unwrap_or(Path::new("/"));
        let root = blaze_metadata::workspace_root(cwd, crate::process::vars().collect(), manifest)?;
        let root = paths::AbsPathBuf::assert_utf8(root);
        Ok(project_model::ManifestPath::try_from(root).expect("a manifest is a file"))
    };
    let metadata = move |manifest: &project_model::ManifestPath,
                         triple: Option<&str>,
                         cfg: &[cfg::CfgAtom],
                         env: &rustc_hash::FxHashMap<String, Option<String>>| {
        // The build's members are those cargo would pick where it was asked, as `-p`,
        // `--workspace` and the default members choose them from there.
        let loaded: &Path = manifest.as_ref();
        let (cwd, manifest) = match &selection.asked_in {
            Some(asked_in) if asked_in.is_dir() => (asked_in.clone(), None),
            Some(asked_in) => {
                (asked_in.parent().unwrap_or(Path::new("/")).to_owned(), Some(asked_in.as_path()))
            }
            None => (loaded.parent().unwrap_or(Path::new("/")).to_owned(), Some(loaded)),
        };
        let mut vars: std::collections::BTreeMap<String, String> = crate::process::vars().collect();
        for (key, value) in env {
            match value {
                Some(value) => vars.insert(key.clone(), value.clone()),
                None => vars.remove(key),
            };
        }
        let cfg: Vec<cargo_platform::Cfg> = cfg.iter().filter_map(platform_cfg).collect();
        let triple = triple.context("no target triple")?;
        let asked = blaze_metadata::Asked {
            cargo_home: None,
            cwd: &cwd,
            env: vars.into_iter().collect(),
            manifest_path: manifest,
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
            platform: Some(blaze_metadata::TargetPlatform { triple, cfg: &cfg }),
            offline: selection.offline,
            locked: selection.locked,
            store: None,
            rust_release: Some(&crate::answers::rust_release),
        };
        let (host_triple, host_cfg) = host_platform()?;
        let host = blaze_metadata::TargetPlatform { triple: &host_triple, cfg: &host_cfg };
        let (json, sources) = blaze_metadata::metadata_with_sources_for_host(&asked, &host)?;
        let metadata =
            serde_json::from_str(&json).context("blaze's own metadata is not cargo's form")?;
        Ok((metadata, sources))
    };
    Some(project_model::OwnMetadata {
        root: std::sync::Arc::new(root),
        metadata: std::sync::Arc::new(metadata),
        sysroot: std::sync::Arc::new(move |manifest, config, cfg| {
            sysroot_metadata(manifest, config, cfg, offline)
        }),
    })
}

/// Resolve the actual installed std workspace with its own lockfile/features. It is
/// not the user's package selection or configuration, and is never a stitched subset.
fn sysroot_metadata(
    manifest: &project_model::ManifestPath,
    config: &project_model::CargoMetadataConfig,
    cfg: &[cfg::CfgAtom],
    offline: bool,
) -> anyhow::Result<Metadata> {
    let manifest: &Path = manifest.as_ref();
    let cwd = manifest.parent().context("std manifest has no directory")?;
    let mut vars: std::collections::BTreeMap<String, String> = crate::process::vars().collect();
    for (name, value) in &config.extra_env {
        match value {
            Some(value) => vars.insert(name.clone(), value.clone()),
            None => vars.remove(name),
        };
    }
    let (features, all, no_default) = match &config.features {
        project_model::CargoFeatures::All => (Vec::new(), true, false),
        project_model::CargoFeatures::Selected { features, no_default_features } => {
            (features.clone(), false, *no_default_features)
        }
    };
    let cfg = cfg.iter().filter_map(platform_cfg).collect::<Vec<_>>();
    let target = config.targets.first().context("std has no target")?;
    let asked = blaze_metadata::Asked {
        cargo_home: None,
        cwd,
        env: vars.into_iter().collect(),
        manifest_path: Some(manifest),
        config: &[],
        features: &features,
        all_features: all,
        no_default_features: no_default,
        build: None,
        platform: Some(blaze_metadata::TargetPlatform { triple: target, cfg: &cfg }),
        offline,
        locked: true,
        store: None,
        rust_release: Some(&crate::answers::rust_release),
    };
    let json = blaze_metadata::metadata(&asked)?;
    serde_json::from_str(&json).context("blaze's std metadata is not cargo's form")
}

/// The programs a build of `selection` links of `targets`, asked for in `cwd` (of the package
/// of `manifest`, when there is one), planned with blaze's own resolution for the triple
/// `target` or the host's: what cargo's unit graph has. `None` when `RA_JIT_OWN_RESOLVE=0`
/// leaves the plan to cargo.
pub(crate) fn programs(
    selection: &crate::plan::Selection,
    cwd: &Path,
    manifest: Option<&Path>,
    targets: blaze_metadata::Targets<'_>,
) -> Option<anyhow::Result<Vec<blaze_metadata::Program>>> {
    if std::env::var_os("RA_JIT_OWN_RESOLVE").is_some_and(|it| it == "0") {
        return None;
    }
    let planned = || {
        let (host_triple, host_cfg) = host_platform()?;
        let triple = selection.target.as_deref().unwrap_or(&host_triple);
        let cfg = if triple == host_triple { host_cfg.clone() } else { platform_cfgs(triple)? };
        let asked = blaze_metadata::Asked {
            cargo_home: None,
            cwd,
            env: crate::process::vars().collect(),
            manifest_path: manifest,
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
            platform: Some(blaze_metadata::TargetPlatform { triple, cfg: &cfg }),
            offline: selection.offline,
            locked: selection.locked,
            store: None,
            rust_release: Some(&crate::answers::rust_release),
        };
        let host = blaze_metadata::TargetPlatform { triple: &host_triple, cfg: &host_cfg };
        Ok(blaze_metadata::programs_for_host(&asked, targets, &host)?.1)
    };
    Some(planned())
}

fn compiler_answer(args: &[&str]) -> anyhow::Result<String> {
    let rustc = super::state::rustc_path();
    // `rustc_says` uses the owned answers; it never runs a compiler.
    super::state::rustc_says(&rustc, args)
        .filter(|it| !it.is_empty())
        .with_context(|| format!("blaze has no compiler answer for {}", args.join(" ")))
}

fn platform_cfgs(triple: &str) -> anyhow::Result<Vec<cargo_platform::Cfg>> {
    let printed = compiler_answer(&["--print", "cfg", "--target", triple])?;
    printed
        .lines()
        .filter(|it| !it.is_empty())
        .map(|it| it.parse().with_context(|| format!("the cfg {it}")))
        .collect()
}

fn host_platform() -> anyhow::Result<(String, Vec<cargo_platform::Cfg>)> {
    let told = compiler_answer(&["-vV"])?;
    let host = told.lines().find_map(|it| it.strip_prefix("host: "));
    let host = host.context("blaze has no compiler host")?.to_owned();
    let cfg = platform_cfgs(&host)?;
    Ok((host, cfg))
}

/// A `cfg` as rust-analyzer has it, as cargo's platforms read it.
fn platform_cfg(atom: &cfg::CfgAtom) -> Option<cargo_platform::Cfg> {
    use std::str::FromStr;
    match atom {
        cfg::CfgAtom::Flag(name) => cargo_platform::Cfg::from_str(name.as_str()).ok(),
        cfg::CfgAtom::KeyValue { key, value } => {
            cargo_platform::Cfg::from_str(&format!("{key}={:?}", value.as_str())).ok()
        }
    }
}

/// `err` as cargo prints it.
fn refused(err: &anyhow::Error, cwd: &Path) {
    if let Some(diagnostic) = err.downcast_ref::<ManifestDiagnostic>() {
        eprintln!("{}", diagnostic.render(cwd));
        return;
    }
    let mut chain = err.chain();
    super::say::error(&chain.next().map(ToString::to_string).unwrap_or_default());
    for cause in chain {
        eprintln!("\nCaused by:");
        for line in cause.to_string().lines() {
            if line.is_empty() { eprintln!() } else { eprintln!("  {line}") }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use cargo_metadata::Metadata;

    fn copy_dir(from: &Path, to: &Path) {
        std::fs::create_dir_all(to).unwrap();
        for entry in std::fs::read_dir(from).unwrap() {
            let entry = entry.unwrap();
            if entry.file_type().unwrap().is_dir() {
                copy_dir(&entry.path(), &to.join(entry.file_name()));
            } else {
                std::fs::copy(entry.path(), to.join(entry.file_name())).unwrap();
            }
        }
    }

    #[test]
    fn provenance_reader_uses_cargo_cwd_and_returns_errors() {
        let root =
            std::env::temp_dir().join(format!("ra-jit-provenance-cwd-{}", std::process::id()));
        let workspace = root.join("macros");
        std::fs::create_dir_all(workspace.join("src")).unwrap();
        std::fs::write(
            workspace.join("Cargo.toml"),
            "[package]\nname='producer'\nversion='0.1.0'\n[workspace]\n",
        )
        .unwrap();
        std::fs::write(workspace.join("src/lib.rs"), "").unwrap();
        let env = vec![("CARGO_HOME".into(), root.join("home").display().to_string())];
        let read = super::no_deps_in(&root, Some(Path::new("macros/Cargo.toml")), &[], env.clone());
        assert_eq!(read.unwrap().packages[0].name.as_str(), "producer");
        assert!(
            super::no_deps_in(&workspace, Some(Path::new("macros/Cargo.toml")), &[], env.clone())
                .is_err()
        );
        std::fs::write(workspace.join("Cargo.toml"), "invalid = [").unwrap();
        assert!(super::no_deps_in(&root, Some(Path::new("macros/Cargo.toml")), &[], env).is_err());
        std::fs::remove_dir_all(root).unwrap();
    }

    /// Each workspace of blaze-manifest's fixtures that cargo reads is, as `cargo_metadata`
    /// gives it to the rest of blaze, what cargo 1.99.0 printed for it.
    #[test]
    fn fixtures_are_what_cargo_metadata_gives() {
        let dir =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../blaze-manifest/tests/fixtures");
        let scratch = std::env::temp_dir().join(format!("ra-jit-metadata-{}", std::process::id()));
        let home = scratch.join("cargo-home");
        std::fs::create_dir_all(&home).unwrap();
        let home = home.canonicalize().unwrap();
        let mut read = 0;
        for entry in std::fs::read_dir(&dir).unwrap() {
            let fixture = entry.unwrap().path();
            let expected = std::fs::read_to_string(fixture.join("expected.json")).unwrap();
            if expected.contains("\"error\"") {
                continue;
            }
            let root = scratch.join(fixture.file_name().unwrap());
            copy_dir(&fixture, &root);
            let root = root.canonicalize().unwrap();
            let cwd = match std::fs::read_to_string(fixture.join("fixture.json")) {
                Ok(text) => {
                    let spec: serde_json::Value = serde_json::from_str(&text).unwrap();
                    root.join(spec["cwd"].as_str().unwrap_or(""))
                }
                Err(_) => root.clone(),
            };
            let expected = expected
                .replace("$ROOT", &root.display().to_string())
                .replace("$CARGO_HOME", &home.display().to_string());
            let expected: Metadata = serde_json::from_str(&expected).unwrap();
            let env = [("CARGO_HOME".to_owned(), home.display().to_string())];
            let config = blaze_manifest::Config::load(&cwd, env, &[]).unwrap();
            let got = super::read(config, None).unwrap();
            assert_eq!(got, expected, "{}", fixture.display());
            read += 1;
        }
        let _ = std::fs::remove_dir_all(&scratch);
        assert!(read >= 3, "{read} fixtures read");
    }
}
