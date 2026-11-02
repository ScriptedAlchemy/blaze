//! A spec made back into the flags and environment that do what it says, for running it
//! locally: the same programs built and the same tests run as the command it came from.
//!
//! A [`BuildSpec`] names units, not flags, and a cargo command can say one set of units in
//! several ways (`cargo test` and `cargo test --lib --tests --bins` ask for the same units when
//! a package has nothing else). So [`candidates`] returns every reproduction that could be the
//! command, the plainest first, and the caller keeps the first whose own spec has the digest of
//! the one it was given: a reproduction is proved by computing the spec again, never assumed.
//! What the tree does not hold and the spec does (`--config` values and `CARGO_PROFILE_*`
//! variables of the original command, for one) make no candidate match, and the caller refuses,
//! naming the first field that differs.

use std::collections::BTreeSet;

use blaze_wire::{BuildSpec, Command, UnitSpec};

use crate::Refusal;
use crate::flags::Subcommand;

/// Locate the saved workspace manifest and workspace-relative cwd in a materialized source.
/// No repository-root fallback or package-name inference is used.
pub fn locations(
    spec: &BuildSpec,
    source_root: &std::path::Path,
) -> Result<(std::path::PathBuf, std::path::PathBuf), Refusal> {
    let cwd = spec.cwd.to_native().map_err(|error| Refusal::new("cwd", error.to_string()))?;
    let manifest = source_root.join(
        spec.workspace_manifest
            .to_native()
            .map_err(|error| Refusal::new("workspace_manifest", error.to_string()))?,
    );
    let workspace =
        manifest.parent().ok_or_else(|| Refusal::new("workspace manifest", "has no parent"))?;
    Ok((manifest.clone(), workspace.join(cwd)))
}

/// Configuration values for a worker's selected workspace, preserving serialized tags.
pub fn config_environment(
    spec: &BuildSpec,
    source_root: &std::path::Path,
) -> Result<Vec<(String, std::ffi::OsString)>, Refusal> {
    let (manifest, _) = locations(spec, source_root)?;
    let workspace = manifest.parent().expect("locations checked the parent");
    spec.config_env
        .iter()
        .map(|(name, value)| {
            value
                .materialize(workspace)
                .map(|value| (name.clone(), value))
                .map_err(|error| Refusal::new(name, error.to_string()))
        })
        .collect()
}

/// One way of saying a spec as a cargo command.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Reproduction {
    /// The cargo command.
    pub command: Subcommand,
    /// The words after the command, `--` and what follows included.
    pub args: Vec<String>,
    /// The variables to set: the rustflags, blaze's backend settings and the variables build
    /// scripts may read, as the spec has them.
    pub env: Vec<(String, std::ffi::OsString)>,
    /// The directory to run in, relative to the workspace root.
    pub cwd: blaze_wire::SourcePath,
}

impl Reproduction {
    /// The command and its flags, as they are typed after `cargo`.
    pub fn words(&self) -> Vec<String> {
        std::iter::once(command_name(self.command).to_owned()).chain(self.args.clone()).collect()
    }
}

/// The name cargo knows `command` by.
pub fn command_name(command: Subcommand) -> &'static str {
    match command {
        Subcommand::Check => "check",
        Subcommand::Build => "build",
        Subcommand::Run => "run",
        Subcommand::Test => "test",
    }
}

/// The variables a spec governs. A run from a spec starts with these removed from its
/// environment, so what the requester's environment held and the spec does not cannot reach the
/// build: the rustflags and blaze's backend and MIR settings, and the variables build scripts
/// may read.
pub fn governed(name: &str) -> bool {
    matches!(
        name,
        "RUSTFLAGS"
            | "CARGO_ENCODED_RUSTFLAGS"
            | "RA_JIT_BACKEND"
            | "RA_JIT_OPT"
            | "RA_JIT_EMIT"
            | "RA_JIT_MIR_OPT"
    ) || name.starts_with("CARGO_BUILD_")
        || name.starts_with("CARGO_PROFILE_")
        || [
            "aarch64-apple-darwin",
            "x86_64-apple-darwin",
            "x86_64-unknown-linux-gnu",
            "aarch64-unknown-linux-gnu",
            "x86_64-unknown-linux-musl",
            "aarch64-unknown-linux-musl",
        ]
        .iter()
        .any(|triple| crate::environment::is_script_var(name, triple))
}

/// The commands that could have made `spec`, the plainest first.
pub fn candidates(spec: &BuildSpec) -> Result<Vec<Reproduction>, Refusal> {
    let (command, run) = match &spec.command {
        Command::Check => (Subcommand::Check, None),
        Command::Build => (Subcommand::Build, None),
        Command::Run(run) => (Subcommand::Run, Some(run)),
        Command::Test(run) => (Subcommand::Test, Some(run)),
    };
    let asked: Vec<&UnitSpec> = spec
        .asked
        .iter()
        .map(|&at| {
            spec.units.get(at as usize).ok_or_else(|| {
                Refusal::new("asked", format!("names unit {at}, which is not there"))
            })
        })
        .collect::<Result<_, _>>()?;
    if asked.is_empty() {
        return Err(Refusal::new("asked", "names no unit"));
    }

    let mut packages: Vec<&str> = asked.iter().map(|unit| unit.package.0.as_str()).collect();
    packages.sort_unstable();
    packages.dedup();

    let mut common = Vec::new();
    for package in &packages {
        common.extend(["-p".to_owned(), (*package).to_owned()]);
    }
    let own_profile = if command == Subcommand::Test { "test" } else { "dev" };
    if spec.profile.name != own_profile {
        common.extend(["--profile".to_owned(), spec.profile.name.clone()]);
    }

    // The target flags that name the asked units one by one, and the plain command that leaves
    // the choice to cargo's defaults. For `cargo test`, `doc` says which of the two could be it.
    let named = named_targets(&asked);
    let targets: Vec<Vec<String>> = match run.map(|it| it.doc) {
        Some(2) => vec![vec!["--doc".to_owned()]],
        Some(1) => vec![Vec::new()],
        Some(0) => vec![named],
        Some(other) => {
            return Err(Refusal::new("doc", format!("is {other}, which is none of 0, 1 and 2")));
        }
        None => vec![Vec::new(), named],
    };

    // The features each package is asked with, one flag each so that a package that lacks one
    // is not asked for it. The plain command leaves them to the defaults.
    let mut features: BTreeSet<String> = BTreeSet::new();
    for unit in &asked {
        for feature in unit.features.iter() {
            features.insert(format!("{}/{feature}", unit.package.0));
        }
    }
    let mut exact = vec!["--no-default-features".to_owned()];
    if !features.is_empty() {
        exact.extend(["--features".to_owned(), features.into_iter().collect::<Vec<_>>().join(",")]);
    }
    let features = [Vec::new(), exact];

    let mut tail = Vec::new();
    if let Some(run) = run {
        if command == Subcommand::Test {
            if run.no_run {
                tail.push("--no-run".to_owned());
            }
            if run.no_fail_fast {
                tail.push("--no-fail-fast".to_owned());
            }
        }
    }

    let mut after = Vec::new();
    if let Some(run) = run {
        after.extend(run.filters.iter().cloned());
        if !run.harness_args.is_empty() {
            after.push("--".to_owned());
            after.extend(run.harness_args.iter().cloned());
        }
    }

    let env =
        environment(spec, run.map(|it| it.env.iter().cloned().collect()).unwrap_or_default())?;
    let mut out = Vec::new();
    for targets in &targets {
        for features in &features {
            let mut args = common.clone();
            args.extend(targets.iter().cloned());
            args.extend(features.iter().cloned());
            args.extend(tail.iter().cloned());
            args.extend(after.iter().cloned());
            let candidate = Reproduction { command, args, env: env.clone(), cwd: spec.cwd.clone() };
            if !out.contains(&candidate) {
                out.push(candidate);
            }
        }
    }
    Ok(out)
}

/// The target flags that name each asked unit's target: `--lib`, `--bin NAME`, `--test NAME`,
/// `--example NAME`, `--bench NAME`. A build script is never asked for.
fn named_targets(asked: &[&UnitSpec]) -> Vec<String> {
    let mut words = BTreeSet::new();
    for unit in asked {
        let (kind, name) = (unit.target.0, unit.target.1.as_str());
        let flag = match kind {
            0 | 6 => Some(("--lib", None)),
            1 => Some(("--bin", Some(name))),
            2 => Some(("--test", Some(name))),
            3 => Some(("--example", Some(name))),
            4 => Some(("--bench", Some(name))),
            _ => None,
        };
        if let Some((flag, name)) = flag {
            words.insert((flag, name.map(str::to_owned)));
        }
    }
    let mut out = Vec::new();
    for (flag, name) in words {
        out.push(flag.to_owned());
        out.extend(name);
    }
    out
}

/// The variables to set: the rustflags, the backend and MIR settings of the asked units'
/// profile, the variables build scripts may read, and the environment a test program is given.
fn environment(
    spec: &BuildSpec,
    test_env: Vec<(String, String)>,
) -> Result<Vec<(String, std::ffi::OsString)>, Refusal> {
    let mut env = Vec::new();
    if !spec.rustflags.is_empty() {
        env.push((
            "CARGO_ENCODED_RUSTFLAGS".to_owned(),
            spec.rustflags.iter().map(|s| s.0.as_str()).collect::<Vec<_>>().join("\u{1f}"),
        ));
    }
    let profile = &spec.profile;
    let llvm = profile.backend == 1;
    env.push(("RA_JIT_BACKEND".to_owned(), if llvm { "llvm" } else { "cranelift" }.to_owned()));
    if llvm {
        env.push(("RA_JIT_OPT".to_owned(), profile.backend_opt.to_string()));
        let emit = if profile.backend_emit == 1 { "fast" } else { "optimized" };
        env.push(("RA_JIT_EMIT".to_owned(), emit.to_owned()));
    }
    env.push(("RA_JIT_MIR_OPT".to_owned(), profile.mir_opt.to_string()));
    let mut native: Vec<_> =
        env.into_iter().map(|(k, v)| (k, std::ffi::OsString::from(v))).collect();
    for (k, v) in spec.script_env.iter() {
        if matches!(v, blaze_wire::ConfigEnvValue::WorkspaceRelative(_)) {
            return Err(Refusal::new(
                k,
                "script_env contains configuration-relative origin; expected ambient value",
            ));
        }
        native.push((
            k.clone(),
            v.materialize(std::path::Path::new(".")).map_err(|e| Refusal::new(k, e.to_string()))?,
        ));
    }
    native.extend(test_env.into_iter().map(|(k, v)| (k, v.into())));
    Ok(native)
}

/// What differs first between the spec a run was given and the one the tree and environment make
/// of the command it came back to, named as a requester would look for it. `None` when they are
/// equal.
pub fn first_difference(given: &BuildSpec, made: &BuildSpec) -> Option<String> {
    if given == made {
        return None;
    }
    if given.source != made.source {
        return Some("source: the tree is not the commit the spec names".to_owned());
    }
    if given.toolchain != made.toolchain {
        return Some("toolchain: this blaze is not the one that made the spec".to_owned());
    }
    if given.workspace_manifest != made.workspace_manifest {
        return Some("workspace_manifest: the selected workspace is not the spec's".to_owned());
    }
    if given.lock != made.lock {
        return Some("lock: Cargo.lock is not the one the spec names".to_owned());
    }
    if given.platform != made.platform {
        return Some("platform: this machine's native tools are not the spec's".to_owned());
    }
    for (name, same) in [
        ("rustflags", given.rustflags == made.rustflags),
        ("config_env", given.config_env == made.config_env),
        ("script_env", given.script_env == made.script_env),
        ("cwd", given.cwd == made.cwd),
        ("command", given.command == made.command),
        ("profile", given.profile == made.profile),
    ] {
        if !same {
            return Some(format!("{name}: it is not what the spec has"));
        }
    }
    let unit = |spec: &BuildSpec, at: usize| {
        spec.units.get(at).map(|it| format!("{} {} ({})", it.package.0, it.package.1, it.target.1))
    };
    if given.units.len() != made.units.len() {
        return Some(format!(
            "units: the spec has {}, the tree makes {}",
            given.units.len(),
            made.units.len()
        ));
    }
    for (at, (a, b)) in given.units.iter().zip(&made.units).enumerate() {
        if a != b {
            let named = unit(given, at).unwrap_or_default();
            return Some(format!("units[{at}] {named}: it is not what the spec has"));
        }
    }
    Some("asked: the units the command names are not the spec's".to_owned())
}
