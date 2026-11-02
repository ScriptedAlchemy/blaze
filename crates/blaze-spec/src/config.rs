//! What cargo's configuration may set, as far as blaze can honour it, and the `[env]` table
//! resolved for a spec.
//!
//! [`KEYS`] is the one table of the configuration keys that change a build and what blaze does
//! with each: it is honoured (it reaches the spec through another module), ignored (it changes
//! nothing blaze writes), or refused by name. Every key is read through
//! [`blaze_manifest::Config`], which merges the files, `--config` and the `CARGO_*` variables as
//! cargo does, so a refusal names the key and, where a variable set it, the variable.
//!
//! A `[target.<triple>.runner]` is refused for `test` and `run` only, the commands that start a
//! program through it: a spec carries no runner, and an executor that ran a test directly would
//! not do what the command does. `target.<triple>.linker` is conservatively refused for every
//! command: even `check` can link host build scripts and proc macros. `build.rustdocflags` (or `RUSTDOCFLAGS`) for the commands
//! that run doc tests, which blaze runs with no rustdoc flags.

use std::collections::BTreeMap;
use std::path::Path;

use blaze_manifest::config::{
    CargoBuildConfig, CargoResolverConfig, ConfigKey, ConfigRelativePath, ConfigValue, Definition,
    FeatureUnification, WarningHandling,
};
use blaze_manifest::{Config, paths};
use blaze_resolve::Platform;

use crate::Refusal;
use crate::assemble::Host;
use crate::flags::{Class, Subcommand};
use crate::refuse::Reason;

/// One configuration key, how blaze reads it and why.
#[derive(Clone, Copy, Debug)]
pub struct Key {
    /// The key: `build.target`, `target.<triple>.runner`.
    pub key: &'static str,
    /// How blaze reads it.
    pub class: Class,
    /// Why it is in its class.
    pub because: &'static str,
}

const fn key(key: &'static str, class: Class, because: &'static str) -> Key {
    Key { key, class, because }
}

/// The keys of the configuration that change a build or are near ones that do. A key that is
/// not here changes nothing cargo builds (`alias`, `cargo-new`, `credential-alias`, `doc`,
/// `future-incompat-report`, `http`, `install`, `registries`, `registry`, `source`, `term`),
/// and is not read; where packages come from changes no unit, because the lock file pins their
/// content by checksum.
pub const KEYS: &[Key] = &[
    key(
        "build.rustflags",
        Class::Honoured,
        "rustflags, by cargo's precedence, in `rustflags` and the profiles",
    ),
    key("target.<triple>.rustflags", Class::Honoured, "rustflags, by cargo's precedence"),
    key("target.'cfg(..)'.rustflags", Class::Honoured, "rustflags, by cargo's precedence"),
    key(
        "profile.*",
        Class::Honoured,
        "merged over the manifest's profiles by blaze-manifest and carried in each unit's profile",
    ),
    key("env", Class::Honoured, "resolved into `config_env`"),
    key("net.offline", Class::Honoured, "keeps resolution off the network"),
    key(
        "build.target-dir",
        Class::Ignored,
        "where cargo writes; a worker writes where it has room",
    ),
    key("build.build-dir", Class::Ignored, "where cargo writes intermediate files"),
    key("build.jobs", Class::Ignored, "how many units build at once"),
    key("build.incremental", Class::Ignored, "blaze keeps its own state"),
    key("build.pipelining", Class::Ignored, "cargo's scheduling only"),
    key(
        "target.<triple>.ar",
        Class::Ignored,
        "cargo ignores it, and build scripts find `ar` by the `cc` crate's rules",
    ),
    key(
        "resolver.incompatible-rust-versions",
        Class::Ignored,
        "chooses versions when the lock file is made, and a spec has one",
    ),
    key("build.target", Class::Refused(Reason::BuildTarget), ""),
    key("build.rustc", Class::Refused(Reason::BuildRustc), ""),
    key("build.rustc-wrapper", Class::Refused(Reason::BuildRustcWrapper), ""),
    key("build.rustc-workspace-wrapper", Class::Refused(Reason::BuildRustcWrapper), ""),
    key("build.rustdoc", Class::Refused(Reason::BuildRustdoc), ""),
    key("build.artifact-dir", Class::Refused(Reason::BuildArtifactDir), ""),
    key("build.rustdocflags", Class::Refused(Reason::RustdocFlags), ""),
    key("build.warnings", Class::Refused(Reason::BuildWarnings), ""),
    key("build.sbom", Class::Refused(Reason::BuildUnstable), ""),
    key("build.analysis", Class::Refused(Reason::BuildUnstable), ""),
    key("build.dep-info-basedir", Class::Refused(Reason::BuildUnstable), ""),
    key("build.target-applies-to-host", Class::Refused(Reason::HostConfig), ""),
    key("host.*", Class::Refused(Reason::HostConfig), ""),
    key("unstable.*", Class::Refused(Reason::UnstableTable), ""),
    key("target.<triple>.runner", Class::Refused(Reason::TargetRunner), ""),
    key("target.<triple>.linker", Class::Refused(Reason::TargetLinker), ""),
    key("target.<triple>.<links>", Class::Refused(Reason::TargetLinks), ""),
    key("resolver.feature-unification", Class::Refused(Reason::FeatureUnification), ""),
    key("resolver.lockfile-path", Class::Refused(Reason::LockfilePath), ""),
    key("patch.*", Class::Refused(Reason::ConfigPatch), ""),
];

impl Key {
    /// Why the key is in its class.
    pub fn reason(&self) -> &'static str {
        match self.class {
            Class::Refused(reason) => reason.entry().why,
            _ => self.because,
        }
    }
}

/// What the configuration says that goes into a spec.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Resolved {
    /// `[env]` against the requester's environment, by name.
    pub config_env: Vec<(String, blaze_wire::ConfigEnvValue)>,
    /// Whether the configuration (`net.offline`) is offline.
    pub offline: bool,
}

/// Apply raw ambient overrides only when Cargo's `[env]` entry is not forced.
pub fn native_overrides(
    config: &Config,
    resolved: &mut Resolved,
    native: &BTreeMap<String, std::ffi::OsString>,
) -> Result<(), Refusal> {
    for (name, value) in &mut resolved.config_env {
        let Some(raw) = native.get(name).filter(|v| v.to_str().is_none()) else { continue };
        // A quoted environment name may contain dots; it is one literal key component.
        let mut key = ConfigKey::parse("env");
        key.push(name);
        let entry = config.get_cv(&key).map_err(|e| rejected(name, e))?;
        let forced = matches!(entry,Some(ConfigValue::Table(ref fields,_))
            if matches!(fields.get("force"),Some(ConfigValue::Boolean(true,_))));
        if !forced {
            *value = blaze_wire::ConfigEnvValue::from_native(raw);
        }
    }
    Ok(())
}

fn rejected(what: &str, error: impl std::fmt::Display) -> Refusal {
    Refusal::with(Reason::ConfigRejected, what, format!("{error:#}"))
}

/// The name a refusal gives a setting: the variable that set it when one did, else its key.
fn named(definition: &Definition, key: &str) -> String {
    match definition {
        Definition::Environment(variable) => variable.clone(),
        _ => key.to_owned(),
    }
}

fn path_setting(
    refusals: &mut Vec<Refusal>,
    path: &Option<ConfigRelativePath>,
    key: &str,
    reason: Reason,
) {
    if let Some(path) = path {
        refusals.push(Refusal::of(reason, named(&path.value().definition, key)));
    }
}

/// Refuses what `config` sets that blaze cannot honour, for `command` run on `host`, and
/// resolves what a spec carries of it. `doc_tests` is whether the command runs doc tests, and
/// `root` the workspace root.
pub fn check(
    config: &Config,
    host: &Host,
    command: Subcommand,
    doc_tests: bool,
    root: &Path,
    env: &BTreeMap<String, String>,
) -> Result<Resolved, Refusal> {
    let mut refusals = Vec::new();

    let build: &CargoBuildConfig = config.build_config().map_err(|it| rejected("[build]", it))?;
    if let Some(target) = &build.target
        && !target.values(config.cwd()).is_empty()
    {
        refusals.push(Refusal::of(Reason::BuildTarget, target_name(config)));
    }
    path_setting(&mut refusals, &build.rustc, "build.rustc", Reason::BuildRustc);
    path_setting(
        &mut refusals,
        &build.rustc_wrapper,
        "build.rustc-wrapper",
        Reason::BuildRustcWrapper,
    );
    path_setting(
        &mut refusals,
        &build.rustc_workspace_wrapper,
        "build.rustc-workspace-wrapper",
        Reason::BuildRustcWrapper,
    );
    path_setting(&mut refusals, &build.rustdoc, "build.rustdoc", Reason::BuildRustdoc);
    path_setting(
        &mut refusals,
        &build.artifact_dir,
        "build.artifact-dir",
        Reason::BuildArtifactDir,
    );
    path_setting(
        &mut refusals,
        &build.dep_info_basedir,
        "build.dep-info-basedir",
        Reason::BuildUnstable,
    );
    if let Some(warnings @ (WarningHandling::Allow | WarningHandling::Deny)) = build.warnings {
        let what = format!("build.warnings = {:?}", format!("{warnings:?}").to_lowercase());
        refusals.push(Refusal::of(Reason::BuildWarnings, what));
    }
    if build.sbom == Some(true) {
        refusals.push(Refusal::of(Reason::BuildUnstable, "build.sbom = true"));
    }
    if build.analysis.as_ref().is_some_and(|it| it.enabled) {
        refusals.push(Refusal::of(Reason::BuildUnstable, "build.analysis.enabled = true"));
    }

    let top = |name: &str| config.get_cv(&ConfigKey::parse(name)).map_err(|it| rejected(name, it));
    if let Some(value) = top("build.target-applies-to-host")? {
        let what = named(value.definition(), "build.target-applies-to-host");
        refusals.push(Refusal::of(Reason::HostConfig, what));
    }
    for (table, reason) in [
        ("host", Reason::HostConfig),
        ("unstable", Reason::UnstableTable),
        ("patch", Reason::ConfigPatch),
    ] {
        match top(table)? {
            Some(ConfigValue::Table(entries, _)) => {
                let mut keys: Vec<&String> = entries.keys().collect();
                keys.sort();
                refusals.extend(
                    keys.first().map(|first| Refusal::of(reason, format!("{table}.{first}"))),
                );
            }
            Some(other) => refusals.push(Refusal::of(reason, named(other.definition(), table))),
            None => {}
        }
    }

    let resolver: Option<CargoResolverConfig> =
        config.get("resolver").map_err(|it| rejected("[resolver]", it))?;
    if let Some(resolver) = resolver {
        if let Some(unification) = resolver.feature_unification
            && !matches!(unification, FeatureUnification::Selected)
        {
            let what = format!(
                "resolver.feature-unification = {:?}",
                format!("{unification:?}").to_lowercase()
            );
            refusals.push(Refusal::of(Reason::FeatureUnification, what));
        }
        path_setting(
            &mut refusals,
            &resolver.lockfile_path,
            "resolver.lockfile-path",
            Reason::LockfilePath,
        );
    }

    target_tables(config, host, command, &mut refusals)?;

    // The first refusal in the order read above, so that one configuration is refused for one
    // reason however it is written.
    if let Some(first) = refusals.into_iter().next() {
        return Err(first);
    }

    if doc_tests {
        let words = crate::rustflags::doc_words(config, host)?;
        if !words.is_empty() {
            let from = ["CARGO_ENCODED_RUSTDOCFLAGS", "RUSTDOCFLAGS"]
                .into_iter()
                .find(|it| env.contains_key(*it));
            let what = from.unwrap_or("build.rustdocflags");
            return Err(Refusal::of(Reason::RustdocFlags, what));
        }
    }

    let offline = config.net_config().map_err(|it| rejected("[net]", it))?.offline.unwrap_or(false);
    let config_env = resolve_env(config, root, env)?;
    Ok(Resolved { config_env, offline })
}

/// The name a refusal of `build.target` gives: its variable when one set it.
fn target_name(config: &Config) -> String {
    match config.env("CARGO_BUILD_TARGET") {
        Some(_) => "CARGO_BUILD_TARGET".to_owned(),
        None => "build.target".to_owned(),
    }
}

/// `[target.<triple>]` of the host and the `[target.'cfg(..)']` tables that match it.
fn target_tables(
    config: &Config,
    host: &Host,
    command: Subcommand,
    refusals: &mut Vec<Refusal>,
) -> Result<(), Refusal> {
    let runs = matches!(command, Subcommand::Test | Subcommand::Run);
    let links = true;
    let table = config.target_cfg_triple(&host.triple).map_err(|it| rejected("[target]", it))?;
    let at = |key: &str, definition: &Definition| named(definition, key);
    if runs && let Some(runner) = &table.runner {
        let what = at(&format!("target.{}.runner", host.triple), &runner.definition);
        refusals.push(Refusal::of(Reason::TargetRunner, what));
    }
    if links && let Some(linker) = &table.linker {
        let what = at(&format!("target.{}.linker", host.triple), &linker.definition);
        refusals.push(Refusal::of(Reason::TargetLinker, what));
    }
    let raw = config
        .get_cv(&ConfigKey::parse(&format!("target.{}", host.triple)))
        .map_err(|it| rejected("[target]", it))?;
    if let Some(ConfigValue::Table(entries, _)) = raw {
        let mut keys: Vec<&String> = entries.keys().collect();
        keys.sort();
        let known = ["runner", "rustflags", "rustdocflags", "linker", "ar"];
        for other in keys.into_iter().filter(|it| !known.contains(&it.as_str())) {
            let what = format!("target.{}.{other}", host.triple);
            refusals.push(Refusal::of(Reason::TargetLinks, what));
        }
    }
    for (key, table) in config.target_cfgs().map_err(|it| rejected("[target]", it))? {
        let matches = key.parse::<Platform>().is_ok_and(|it| it.matches(&host.triple, &host.cfg));
        if !matches {
            continue;
        }
        if runs && let Some(runner) = &table.runner {
            let what = at(&format!("target.{key}.runner"), &runner.definition);
            refusals.push(Refusal::of(Reason::TargetRunner, what));
        }
        if links && let Some(linker) = &table.linker {
            let what = at(&format!("target.{key}.linker"), &linker.definition);
            refusals.push(Refusal::of(Reason::TargetLinker, what));
        }
        for other in table.other.keys() {
            refusals.push(Refusal::of(Reason::TargetLinks, format!("target.{key}.{other}")));
        }
    }
    Ok(())
}

/// `[env]` against the requester's environment, as cargo's `env_config` and the environment
/// the build runs in make it: a variable the environment has wins unless the entry is
/// `force`d, and a `relative` path is relative to the directory above the `.cargo` its file
/// is in, which a spec names from the workspace root so that it means the same on a worker.
/// The distinction is an explicit wire tag, not an out-of-band list of names.
fn resolve_env(
    config: &Config,
    root: &Path,
    env: &BTreeMap<String, String>,
) -> Result<Vec<(String, blaze_wire::ConfigEnvValue)>, Refusal> {
    use blaze_wire::{ConfigEnvValue, SourcePath};
    let table = config.get_cv(&ConfigKey::parse("env")).map_err(|it| rejected("[env]", it))?;
    let Some(table) = table else { return Ok(Vec::new()) };
    let ConfigValue::Table(entries, _) = table else {
        return Err(rejected("[env]", "expected a table"));
    };
    let mut names: Vec<&String> = entries.keys().collect();
    names.sort();
    let mut resolved = Vec::new();
    for name in names {
        if matches!(name.as_str(), "CARGO_HOME" | "RUSTUP_HOME" | "RUSTUP_TOOLCHAIN") {
            let what = format!("env.{name}");
            let why = format!(
                "setting the `{name}` environment variable is not supported in the `[env]` configuration table"
            );
            return Err(rejected(&what, why));
        }
        let what = format!("env.{name}");
        let (value, definition, force, relative) = match &entries[name] {
            ConfigValue::String(value, definition) => {
                (value.clone(), definition.clone(), false, false)
            }
            ConfigValue::Table(fields, _) => {
                let Some(ConfigValue::String(value, definition)) = fields.get("value") else {
                    return Err(rejected(&what, "expected a string `value`"));
                };
                let flag = |field: &str| match fields.get(field) {
                    None => Ok(false),
                    Some(ConfigValue::Boolean(on, _)) => Ok(*on),
                    Some(_) => Err(rejected(&what, format!("expected a boolean `{field}`"))),
                };
                (value.clone(), definition.clone(), flag("force")?, flag("relative")?)
            }
            _ => return Err(rejected(&what, "expected a string or a table")),
        };
        if !force && let Some(set) = env.get(name) {
            resolved.push((name.clone(), ConfigEnvValue::Literal(set.clone().into())));
            continue;
        }
        if !relative {
            resolved.push((name.clone(), ConfigEnvValue::Literal(value.into())));
            continue;
        }
        let from = paths::normalize_path(&definition.root(config.cwd()).join(&value));
        let Ok(inside) = from.strip_prefix(root) else {
            return Err(Refusal::with(
                Reason::EnvRelativeOutside,
                format!("env.{name} = {{ value = {value:?}, relative = true }}"),
                format!("it is of {definition}, and the workspace root is {}", root.display()),
            ));
        };
        let path = SourcePath::from_native(inside).map_err(|error| rejected(&what, error))?;
        resolved.push((name.clone(), ConfigEnvValue::WorkspaceRelative(path)));
    }
    Ok(resolved)
}
