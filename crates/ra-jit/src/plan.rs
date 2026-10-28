//! What cargo builds: the units of a build of one package, with the features and the profile
//! of each. `cargo metadata` has neither as a build of a single package has them.

use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
};

use anyhow::Context;
use cfg::{CfgAtom, CfgDiff};
use intern::{Symbol, sym};
use rustc_hash::{FxHashMap, FxHashSet};
use serde_derive::{Deserialize, Serialize};

/// What is said of a profile with `lto`. ra-jit compiles every function alone, so there is
/// nothing for it to optimize across functions, and the program does what rustc's does: only
/// how fast and how large it is differ. The libraries it links against are built without.
const LTO_ACCEPTED: &str = "lto: accepted, no cross-function optimization";

/// Which build of a package the daemon stands in for, as the flags of `cargo build` say it.
/// `cargo ra-jit` and `ra-jit` itself both say a build with this.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Selection {
    /// The name of the profile: `dev` unless `--release` or `--profile` says otherwise.
    pub(crate) profile: String,
    pub(crate) features: Vec<String>,
    pub(crate) no_default_features: bool,
    pub(crate) all_features: bool,
    /// The packages asked for with `-p`: none is what `cargo build` builds where it is run.
    pub(crate) packages: Vec<String>,
    /// Whether every member of the workspace is built.
    pub(crate) workspace: bool,
    /// The members `--exclude` leaves out of the workspace.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) excludes: Vec<String>,
    /// The values of `-Z`, which cargo is run with as it was.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) unstable: Vec<String>,
    /// The triple built for, when it is not the host.
    pub(crate) target: Option<String>,
    /// Whether tests, examples and benchmarks are built, and so what they depend on.
    pub(crate) dev_deps: bool,
    /// The `--config` values given, which can change a profile or what is built as much as
    /// the configuration files can.
    #[serde(default)]
    pub(crate) config: Vec<String>,
    /// Where the build was asked for, when not in the package's directory: the manifest given
    /// with `--manifest-path`, or the directory cargo looked for one from. What `--workspace`,
    /// `-p` and no package at all select is decided there.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) asked_in: Option<PathBuf>,
}

impl Default for Selection {
    fn default() -> Selection {
        Selection {
            profile: "dev".to_owned(),
            features: Vec::new(),
            no_default_features: false,
            all_features: false,
            packages: Vec::new(),
            workspace: false,
            excludes: Vec::new(),
            unstable: Vec::new(),
            target: None,
            dev_deps: false,
            config: Vec::new(),
            asked_in: None,
        }
    }
}

impl Selection {
    /// The selection as flags, which `cargo build` and `ra-jit` both read. Whether tests are
    /// built is not among them: cargo and ra-jit name that differently.
    pub(crate) fn flags(&self) -> Vec<String> {
        let mut flags = vec!["--profile".to_owned(), self.profile.clone()];
        if let Some(target) = &self.target {
            flags.extend(["--target".to_owned(), target.clone()]);
        }
        if !self.features.is_empty() {
            flags.extend(["--features".to_owned(), self.features.join(",")]);
        }
        if self.no_default_features {
            flags.push("--no-default-features".to_owned());
        }
        if self.all_features {
            flags.push("--all-features".to_owned());
        }
        for package in &self.packages {
            flags.extend(["--package".to_owned(), package.clone()]);
        }
        if self.workspace {
            flags.push("--workspace".to_owned());
        }
        for package in &self.excludes {
            flags.extend(["--exclude".to_owned(), package.clone()]);
        }
        for unstable in &self.unstable {
            flags.extend(["-Z".to_owned(), unstable.clone()]);
        }
        for config in &self.config {
            flags.extend(["--config".to_owned(), config.clone()]);
        }
        flags
    }
}

/// How a package is built for the target.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Built {
    pub(crate) features: BTreeSet<String>,
    pub(crate) codegen_flags: Vec<String>,
    pub(crate) profile: Profile,
    /// Whether the package is one cargo was asked to build, not one those depend on.
    pub(crate) primary: bool,
    /// Whether the package is built into the program, not only for what runs while building.
    pub(crate) for_target: bool,
}

/// What a build is for and who asks for it.
pub(crate) struct Asked<'a> {
    /// The triple built for.
    pub(crate) triple: &'a str,
    /// What `rustc --print cfg` says of the triple.
    pub(crate) cfg: &'a [CfgAtom],
    /// The variables of the process that asks, where they are not those of this one.
    pub(crate) env: &'a FxHashMap<String, Option<String>>,
}

/// The settings of a profile that change what a program does.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub(crate) struct Profile {
    opt_level: String,
    lto: String,
    pub(crate) debug_assertions: bool,
    pub(crate) overflow_checks: bool,
    pub(crate) panic: PanicStrategy,
    debuginfo: DebugLevel,
    split_debuginfo: Option<String>,
    strip: Strip,
}

/// A `debug` setting as cargo passes it on: a level by its number or by its name.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(untagged)]
enum DebugLevel {
    Number(u8),
    Named(String),
}

/// A `strip` setting. One cargo deferred is its own choice for a build without debug info,
/// which has nothing the setting would take out.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
enum Strip {
    Resolved(Stripped),
    Deferred(Stripped),
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
enum Stripped {
    None,
    Named(String),
}

/// What a build tells a debugger of its code, and where that is kept.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Described {
    pub(crate) level: Level,
    pub(crate) kept: Kept,
    /// Whether every function keeps its frame pointer (`-C force-frame-pointers`), for a
    /// profiler or the program's own backtraces to walk the frames by.
    pub(crate) frame_pointers: bool,
}

impl Described {
    pub(crate) const NOTHING: Described =
        Described { level: Level::Nothing, kept: Kept::Linked, frame_pointers: false };

    /// The most either tells, kept the furthest from the program either keeps it.
    fn or(self, other: Described) -> Described {
        Described {
            level: self.level.max(other.level),
            kept: self.kept.max(other.kept),
            frame_pointers: self.frame_pointers || other.frame_pointers,
        }
    }
}

/// How much a build tells a debugger of its code.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Level {
    Nothing,
    /// Which line each instruction is of, and the functions.
    Lines,
    /// The variables and their types as well.
    Full,
}

/// Where what a debugger reads of the code is kept (`split-debuginfo`). The lines are with
/// the program always.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Kept {
    /// In the program, which the linker copies it into.
    Linked,
    /// In the objects the program was linked from, which the program names.
    Objects,
    /// In one package next to the program.
    Package,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum PanicStrategy {
    Unwind,
    Abort,
    ImmediateAbort,
}

impl PanicStrategy {
    pub(crate) fn name(self) -> &'static str {
        match self {
            PanicStrategy::Unwind => "unwind",
            PanicStrategy::Abort | PanicStrategy::ImmediateAbort => "abort",
        }
    }
}

impl Profile {
    /// Host script units use the same profile/flag precedence as ordinary owned units.
    pub(crate) fn for_script(
        profile: &blaze_manifest::profiles::Profile,
        flags: &[String],
    ) -> anyhow::Result<Profile> {
        let profile: Profile = serde_json::from_value(serde_json::to_value(profile)?)?;
        Ok(profile.with(&Flags::parse(flags.iter().map(String::as_str))))
    }

    /// The profile as rustc ends up with it once `flags` follow what cargo passes for it.
    fn with(&self, flags: &Flags) -> Profile {
        // cargo names a check only where it is not what rustc has for the level by itself, so
        // a level among the flags changes the ones cargo left unnamed.
        let unoptimized = self.opt_level == "0";
        let named_assertions =
            (self.debug_assertions != unoptimized).then_some(self.debug_assertions);
        let named_overflow =
            (self.overflow_checks != self.debug_assertions).then_some(self.overflow_checks);
        let unoptimized = flags.opt_level.as_ref().map_or(unoptimized, |it| it == "0");
        let debug_assertions = flags.debug_assertions.or(named_assertions).unwrap_or(unoptimized);
        Profile {
            debug_assertions,
            overflow_checks: flags.overflow_checks.or(named_overflow).unwrap_or(debug_assertions),
            panic: flags.panic.unwrap_or(self.panic),
            debuginfo: flags.debuginfo.clone().unwrap_or_else(|| self.debuginfo.clone()),
            split_debuginfo: flags.split_debuginfo.clone().or_else(|| self.split_debuginfo.clone()),
            strip: flags.strip.clone().map_or_else(
                || self.strip.clone(),
                |named| Strip::Resolved(Stripped::Named(named)),
            ),
            ..self.clone()
        }
    }

    /// What the profile has a debugger told. Where the profile asks for it to be kept apart
    /// from the program, which ra-jit does not do, that is the reason not to build.
    pub(crate) fn described(&self) -> anyhow::Result<Described> {
        let level = match &self.debuginfo {
            DebugLevel::Number(0) => Level::Nothing,
            DebugLevel::Number(1) => Level::Lines,
            DebugLevel::Number(2) => Level::Full,
            DebugLevel::Named(name) => match name.as_str() {
                "none" => Level::Nothing,
                "line-directives-only" | "line-tables-only" | "limited" => Level::Lines,
                "full" => Level::Full,
                _ => anyhow::bail!("`debug = \"{name}\"`: ra-jit does not know the level"),
            },
            DebugLevel::Number(level) => {
                anyhow::bail!("`debug = {level}`: ra-jit does not know the level")
            }
        };
        // Stripping the symbols is the linker's to do; of what ra-jit writes, what a debugger
        // reads is all either setting takes out.
        let stripped = match &self.strip {
            Strip::Resolved(Stripped::Named(what)) => match what.as_str() {
                "none" => false,
                "debuginfo" | "symbols" => true,
                _ => anyhow::bail!("`strip = \"{what}\"`: ra-jit does not know the setting"),
            },
            Strip::Resolved(Stripped::None) | Strip::Deferred(_) => false,
        };
        if stripped || level == Level::Nothing {
            return Ok(Described::NOTHING);
        }
        let kept = match self.split_debuginfo.as_deref() {
            None | Some("off") => Kept::Linked,
            Some("unpacked") => Kept::Objects,
            Some("packed") => Kept::Package,
            Some(split) => {
                anyhow::bail!("`split-debuginfo = \"{split}\"`: ra-jit does not know the setting")
            }
        };
        Ok(Described { level, kept, frame_pointers: false })
    }

    /// Whether rustc compiles the whole program again when it links it.
    fn links_from_bitcode(&self) -> bool {
        matches!(self.lto.as_str(), "fat" | "thin" | "true")
    }

    /// What the profile changes of the cfg `rustc --print cfg -O` reports.
    pub(crate) fn cfg(&self) -> CfgDiff {
        let flag = |name: Symbol| CfgAtom::Flag(name);
        let panic = |strategy: PanicStrategy| CfgAtom::KeyValue {
            key: sym::panic,
            value: Symbol::intern(strategy.name()),
        };
        let (mut enable, mut disable) = (Vec::new(), Vec::new());
        let mut set =
            |on: bool, atom: CfgAtom| if on { enable.push(atom) } else { disable.push(atom) };
        set(self.debug_assertions, flag(sym::debug_assertions));
        set(self.overflow_checks, flag(sym::overflow_checks));
        for strategy in [PanicStrategy::Unwind, PanicStrategy::Abort] {
            set(strategy.name() == self.panic.name(), panic(strategy));
        }
        CfgDiff::new(enable, disable)
    }
}

/// What the flags given to every rustc of a build say of what a profile has too. They come
/// after the profile's own on the command line and so win.
#[derive(Debug, Default, PartialEq, Eq)]
struct Flags {
    codegen_flags: Vec<String>,
    opt_level: Option<String>,
    debug_assertions: Option<bool>,
    overflow_checks: Option<bool>,
    panic: Option<PanicStrategy>,
    lto: bool,
    debuginfo: Option<DebugLevel>,
    split_debuginfo: Option<String>,
    strip: Option<String>,
    frame_pointers: bool,
    lints_capped: bool,
}

impl Flags {
    fn parse<'a>(mut words: impl Iterator<Item = &'a str>) -> Flags {
        let all: Vec<&str> = words.by_ref().collect();
        let mut words = all.iter().copied();
        let mut flags = Flags::default();
        flags.codegen_flags = all.iter().map(|it| (*it).to_owned()).collect();
        while let Some(word) = words.next() {
            if ["--cap-lints", "--force-warn"].iter().any(|it| {
                word.strip_prefix(it).is_some_and(|rest| rest.is_empty() || rest.starts_with('='))
            }) {
                flags.lints_capped = true;
                continue;
            }
            let option = match word {
                "-O" => "opt-level=3",
                "-g" => "debuginfo=2",
                "-C" => words.next().unwrap_or_default(),
                _ => match word.strip_prefix("-C") {
                    Some(option) => option,
                    None => continue,
                },
            };
            let (name, value) = option.split_once('=').unwrap_or((option, ""));
            let on = match value {
                "y" | "yes" | "on" | "true" | "" => Some(true),
                "n" | "no" | "off" | "false" => Some(false),
                _ => None,
            };
            match name.replace('_', "-").as_str() {
                "opt-level" => flags.opt_level = Some(value.to_owned()),
                "debug-assertions" => flags.debug_assertions = on,
                "overflow-checks" => flags.overflow_checks = on,
                "panic" if value == "abort" => flags.panic = Some(PanicStrategy::Abort),
                "panic" if value == "unwind" => flags.panic = Some(PanicStrategy::Unwind),
                "lto" => flags.lto = on != Some(false),
                "linker-plugin-lto" => flags.lto = on != Some(false),
                "debuginfo" => {
                    flags.debuginfo = Some(match value.parse() {
                        Ok(level) => DebugLevel::Number(level),
                        Err(_) => DebugLevel::Named(value.to_owned()),
                    });
                }
                "split-debuginfo" => flags.split_debuginfo = Some(value.to_owned()),
                "strip" => flags.strip = Some(value.to_owned()),
                // `always` and `non-leaf` keep them too, `non-leaf` in more functions than asked.
                "force-frame-pointers" => {
                    flags.frame_pointers = on.unwrap_or(matches!(value, "always" | "non-leaf"));
                }
                _ => {}
            }
        }
        flags
    }
}

/// The packages of a build by their id, each as it is built for the target.
pub(crate) struct Plan {
    packages: FxHashMap<String, Built>,
    host_cargo: Option<project_model::CargoWorkspace>,
    lto: bool,
    frame_pointers: bool,
    lints_capped: bool,
    /// Profiles and workspace configuration read by the owned planner.
    sources: Vec<(PathBuf, Vec<u8>)>,
}

/// What a plan says of a program as a whole.
#[derive(Clone, Copy)]
pub(crate) struct Whole {
    /// What the build has a debugger told of the code.
    pub(crate) debug: Described,
    /// Whether rustc links it from bitcode, with `lto`.
    pub(crate) lto: bool,
    /// Whether `--cap-lints` or `--force-warn` is among the flags, so that a lint the source
    /// denies may not fail rustc's build.
    pub(crate) lints_capped: bool,
}

impl Plan {
    /// The plan made by blaze with no cargo: the packages of `cargo`,
    /// the workspace blaze read and resolved itself for this build (`cargo/metadata.rs`), each
    /// with the features that reading has and the profile blaze-manifest gives it as cargo's
    /// `Profiles::get_profile` does (for the target when the packages the build selects reach
    /// it without a build script or proc macro between, for the host otherwise), and the
    /// flags for rustc of the process that asks and of cargo's configuration.
    pub(crate) fn own(
        cargo: &project_model::CargoWorkspace,
        manifest: &Path,
        selection: &Selection,
        asked: &Asked<'_>,
    ) -> anyhow::Result<Plan> {
        use blaze_manifest::profiles::{Profiles, UnitFor};
        let root: &Path = cargo.workspace_root().as_ref();
        let mut env: std::collections::BTreeMap<String, String> = crate::process::vars().collect();
        for (name, value) in asked.env {
            match value {
                Some(value) => env.insert(name.clone(), value.clone()),
                None => env.remove(name),
            };
        }
        let (cwd, loaded_manifest) = match selection.asked_in.as_deref() {
            Some(path) if path.is_dir() => (path, None),
            Some(path) => (path.parent().unwrap_or(root), Some(path)),
            None => (manifest.parent().unwrap_or(root), Some(manifest)),
        };
        let mut config = blaze_manifest::Config::load(cwd, env.clone(), &[])?;
        config.start("build", &selection.config)?;
        let ctx = blaze_manifest::Context::new(config);
        let root_manifest =
            blaze_manifest::metadata::root_manifest(Some(&root.join("Cargo.toml")), &ctx)?;
        let ws = blaze_manifest::Workspace::new(&root_manifest, &ctx)?;
        let native = selection.target.is_none();
        let compiler = crate::answers::toolchain_rustc_of(std::ffi::OsStr::new("rustc"))
            .context("owned planner has no toolchain; Cargo was not executed")?;
        let host = crate::answers::host_tuple(&compiler)
            .context("owned planner has no host facts; Cargo was not executed")?;
        let sysroot = compiler
            .parent()
            .and_then(Path::parent)
            .context("owned planner has no sysroot; Cargo was not executed")?;
        env.insert("RUSTUP_TOOLCHAIN".into(), sysroot.display().to_string());
        let mut resolved_selection = selection.clone();
        if resolved_selection.asked_in.is_none() {
            resolved_selection.asked_in = Some(cwd.to_owned());
        }
        let resolved = crate::cargo::producer_plan::of(
            loaded_manifest.unwrap_or(manifest),
            &resolved_selection,
            &host,
            asked.triple,
            env,
        )
        .context("owned build units failed; Cargo was not executed")?;
        let profiles = Profiles::new(
            ws.profiles(),
            ws.unstable_features(),
            &ctx.config,
            &selection.profile,
            &host,
        )?;
        let flags = crate::scripts::rustflags(&ctx.config, asked.triple, asked.cfg)?;
        let flags = Flags::parse(flags.iter().map(String::as_str));
        anyhow::ensure!(
            !flags.lto,
            "`-C lto` is among the flags: rustc compiles the whole program again as it links \
             it and leaves no library on the link line for ra-jit's functions to be linked against"
        );
        // The packages the build selects: those `-p` names, every member for `--workspace`,
        // else the one asked in, or every member of a virtual workspace.
        let asked_in = match &selection.asked_in {
            Some(it) if it.is_dir() => it.join("Cargo.toml"),
            Some(it) => it.clone(),
            None => manifest.to_owned(),
        };
        let members: Vec<project_model::Package> =
            cargo.packages().filter(|&it| cargo[it].is_member).collect();
        let named = |it: &project_model::Package| {
            selection
                .packages
                .iter()
                .any(|name| name.split('@').next() == Some(cargo[*it].name.as_str()))
        };
        let excluded = |it: &project_model::Package| {
            selection.excludes.iter().any(|name| *name == cargo[*it].name)
        };
        let manifest_of = |it: &project_model::Package| -> PathBuf {
            AsRef::<Path>::as_ref(&cargo[*it].manifest).to_owned()
        };
        let roots: Vec<project_model::Package> = if !selection.packages.is_empty() {
            members.iter().copied().filter(named).collect()
        } else if selection.workspace {
            members.iter().copied().filter(|it| !excluded(it)).collect()
        } else {
            let at: Vec<_> =
                members.iter().copied().filter(|it| manifest_of(it) == asked_in).collect();
            if at.is_empty() {
                let defaults = ws.default_members().map(|(path, _)| path).collect::<Vec<_>>();
                members
                    .iter()
                    .copied()
                    .filter(|it| defaults.contains(&manifest_of(it).as_path()))
                    .collect()
            } else {
                at
            }
        };
        let kind = match &selection.target {
            Some(triple) => blaze_manifest::config::CompileKind::Target(triple.clone()),
            None => blaze_manifest::config::CompileKind::Host,
        };
        let mut packages = FxHashMap::default();
        let mut lto = false;
        let mut units = resolved.units.iter().collect::<Vec<_>>();
        // The adapter keeps the target unit when a package exists in both
        // feature domains, but resolution itself never folds those domains.
        units.sort_by_key(|unit| !unit.host);
        for unit in units {
            let package = cargo
                .packages()
                .find(|&it| cargo[it].id.repr == unit.package)
                .with_context(|| {
                    format!(
                        "owned unit {} absent from workspace; Cargo was not executed",
                        unit.package
                    )
                })?;
            let data = &cargo[package];
            let for_target = !unit.host;
            let unit_for = match for_target {
                true => UnitFor::new_normal(),
                false => UnitFor::new_host(false),
            };
            let kind = match for_target && !native {
                true => kind.clone(),
                false => blaze_manifest::config::CompileKind::Host,
            };
            let named = crate::scripts::named(data);
            let profile =
                profiles.get_profile(&named, data.is_member, data.is_local, unit_for, &kind);
            let profile: Profile = serde_json::from_value(serde_json::to_value(&profile)?)
                .context("blaze-manifest's profile is not the unit graph's form")?;
            lto |= for_target && profile.links_from_bitcode();
            let applies = for_target || selection.target.is_none();
            let profile = if applies { profile.with(&flags) } else { profile };
            let built = Built {
                features: unit.features.iter().cloned().collect(),
                codegen_flags: std::iter::once(format!("-Copt-level={}", profile.opt_level))
                    .chain(
                        flags
                            .codegen_flags
                            .iter()
                            .filter(|_| for_target || selection.target.is_none())
                            .cloned(),
                    )
                    .collect(),
                profile,
                primary: roots.contains(&package),
                for_target,
            };
            packages.insert(package_named(&data.id.repr), built);
        }
        if lto {
            eprintln!("{LTO_ACCEPTED}");
        }
        let host_metadata = resolved.script_metadata(cargo.target_directory().as_ref())?;
        let host_manifest = project_model::ManifestPath::try_from(paths::AbsPathBuf::assert_utf8(
            host_metadata.workspace_root.join("Cargo.toml").into_std_path_buf(),
        ))
        .map_err(|_| anyhow::anyhow!("host script manifest is not absolute"))?;
        let host_cargo = project_model::CargoWorkspace::new(
            host_metadata,
            host_manifest,
            ide_db::base_db::Env::default(),
            false,
        );
        Ok(Plan {
            packages,
            host_cargo: Some(host_cargo),
            lto,
            frame_pointers: flags.frame_pointers,
            lints_capped: flags.lints_capped,
            sources: ctx.sources().into_iter().chain(resolved.sources).collect(),
        })
    }

    pub(crate) fn sources(&self) -> &[(PathBuf, Vec<u8>)] {
        &self.sources
    }

    pub(crate) fn host_cargo(&self) -> Option<&project_model::CargoWorkspace> {
        self.host_cargo.as_ref()
    }

    /// What the build has a debugger told of the packages cargo was asked to build: the
    /// most any of them asks for, which is one answer for all the code ra-jit writes.
    pub(crate) fn described(&self) -> anyhow::Result<Described> {
        let mut described = Described::NOTHING;
        for built in self.packages.values().filter(|it| it.primary) {
            described = described.or(built.profile.described()?);
        }
        Ok(described)
    }

    /// What the plan says of the programs cargo was asked to build.
    pub(crate) fn whole(&self) -> anyhow::Result<Whole> {
        let debug = Described { frame_pointers: self.frame_pointers, ..self.described()? };
        Ok(Whole { debug, lto: self.lto, lints_capped: self.lints_capped })
    }

    /// How the package cargo calls `id` is built, when the build has it.
    pub(crate) fn package(&self, id: &str) -> Option<&Built> {
        self.packages.get(&package_named(id))
    }

    /// The profile of the packages cargo was asked to build, for the target.
    pub(crate) fn target_profile(&self) -> Option<&Profile> {
        self.packages.values().find(|it| it.primary && it.for_target).map(|it| &it.profile)
    }
}

/// The one name of a package for the two ways cargo spells its id: `source#name@version`,
/// and `name version (source)` as the unit graph of an older cargo does, with the
/// commit of a git source after it.
fn package_named(id: &str) -> String {
    let old = id.strip_suffix(')').and_then(|it| it.split_once(" ("));
    if let Some((name_version, source)) = old
        && let Some((name, version)) = name_version.split_once(' ')
    {
        let source = source.split_once('#').map_or(source, |it| it.0);
        return format!("{source}#{name}@{version}");
    }
    let Some((source, named)) = id.split_once('#') else { return id.to_owned() };
    if named.contains('@') {
        return id.to_owned();
    }
    // A package whose directory or repository is called what it is has only its version.
    let path = source.split_once('?').map_or(source, |it| it.0);
    let name = path.trim_end_matches('/').rsplit('/').next().unwrap_or(path);
    let name = name.strip_suffix(".git").unwrap_or(name);
    format!("{source}#{name}@{named}")
}

#[derive(Deserialize)]
struct UnitGraph {
    units: Vec<Unit>,
    roots: Vec<usize>,
}

#[derive(Deserialize)]
struct Unit {
    pkg_id: String,
    target: UnitTarget,
    profile: Profile,
    features: Vec<String>,
    dependencies: Vec<UnitDependency>,
    /// The target it is built for, when the build names one.
    #[serde(default)]
    platform: Option<String>,
    /// `build`, `test`, `run-custom-build` and the like.
    #[serde(default)]
    mode: String,
}

#[derive(Deserialize)]
struct UnitTarget {
    kind: Vec<String>,
    #[serde(default)]
    crate_types: Vec<String>,
    #[serde(default)]
    name: String,
    #[serde(default)]
    src_path: PathBuf,
}

/// A program of a build: a unit cargo links an executable of.
pub(crate) struct Program {
    /// The package, as cargo names it in `cargo metadata`.
    pub(crate) package: String,
    /// The executable's name.
    pub(crate) name: String,
    /// The root file of its crate.
    pub(crate) root: PathBuf,
    pub(crate) example: bool,
    /// The target it is built for, when the build names one.
    pub(crate) platform: Option<String>,
}

#[derive(Deserialize)]
struct UnitDependency {
    index: usize,
}

impl Unit {
    /// Whether the unit runs while building: a build script or a proc macro. What it depends
    /// on is built for the host, with features and a profile of its own.
    fn runs_on_host(&self) -> bool {
        self.target.kind.iter().any(|it| it == "custom-build" || it == "proc-macro")
    }

    fn is_library(&self) -> bool {
        self.target.kind.iter().any(|it| it.ends_with("lib"))
    }
}

impl UnitGraph {
    /// The units that end up in the program: those the roots reach without going through one
    /// that runs on the host.
    fn for_target(&self) -> FxHashSet<usize> {
        let mut reached = FxHashSet::default();
        let mut queue: Vec<usize> =
            self.roots.iter().copied().filter(|&it| it < self.units.len()).collect();
        while let Some(index) = queue.pop() {
            let unit = &self.units[index];
            if unit.runs_on_host() || !reached.insert(index) {
                continue;
            }
            queue.extend(
                unit.dependencies.iter().map(|it| it.index).filter(|&it| it < self.units.len()),
            );
        }
        reached
    }

    fn plan(self, flags: &Flags, explicit_target: bool) -> anyhow::Result<Plan> {
        let for_target = self.for_target();
        // The flag is not cargo's to know: the libraries are then built for a link without it
        // and rustc finds no bitcode in them.
        anyhow::ensure!(
            !flags.lto,
            "`-C lto` is among the flags: rustc compiles the whole program again as it links \
             it and leaves no library on the link line for ra-jit's functions to be linked against"
        );
        let lto = for_target.iter().any(|&it| self.units[it].profile.links_from_bitcode());
        if lto {
            eprintln!("{LTO_ACCEPTED}");
        }
        // A package that is only built for the host, a proc macro, is as that build has it.
        let rank = |index: usize, unit: &Unit| (for_target.contains(&index), unit.is_library());
        let mut best: FxHashMap<&str, (usize, &Unit)> = FxHashMap::default();
        for (index, unit) in self.units.iter().enumerate() {
            let chosen = best.entry(unit.pkg_id.as_str()).or_insert((index, unit));
            if rank(index, unit) > rank(chosen.0, chosen.1) {
                *chosen = (index, unit);
            }
        }
        let roots: FxHashSet<&str> =
            self.roots.iter().filter_map(|&it| Some(self.units.get(it)?.pkg_id.as_str())).collect();
        let packages = best
            .into_iter()
            .map(|(id, (index, unit))| {
                let features = unit.features.iter().cloned().collect();
                let for_target = for_target.contains(&index);
                // Cargo's unstable host.rustflags/target-applies-to-host overrides are
                // not modeled here yet; this is the ordinary explicit-target rule.
                let applies = for_target || !explicit_target;
                let profile = if applies { unit.profile.with(flags) } else { unit.profile.clone() };
                let primary = roots.contains(id);
                (
                    package_named(id),
                    Built {
                        features,
                        profile,
                        primary,
                        for_target,
                        codegen_flags: std::iter::once(format!(
                            "-Copt-level={}",
                            unit.profile.opt_level
                        ))
                        .chain(flags.codegen_flags.iter().filter(|_| applies).cloned())
                        .collect(),
                    },
                )
            })
            .collect();
        Ok(Plan {
            packages,
            host_cargo: None,
            lto,
            frame_pointers: flags.frame_pointers,
            lints_capped: flags.lints_capped,
            sources: Vec::new(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{Flags, UnitGraph};

    #[test]
    fn explicit_target_flags_are_not_given_to_host_only_proc_macro_units() {
        let profile = serde_json::json!({
            "opt_level": "0", "lto": "false", "debug_assertions": true,
            "overflow_checks": true, "panic": "unwind", "debuginfo": 0,
            "split_debuginfo": null, "strip": {"resolved": "None"}
        });
        let graph = serde_json::json!({
            "roots": [0],
            "units": [
                {"pkg_id":"path+file:///w/app#app@0.1.0",
                 "target":{"kind":["bin"],"crate_types":["bin"]},
                 "profile":profile, "features":[], "dependencies":[{"index":1}]},
                {"pkg_id":"path+file:///w/mac#mac@0.1.0",
                 "target":{"kind":["proc-macro"],"crate_types":["proc-macro"]},
                 "profile":profile, "features":[], "dependencies":[]}
            ]
        });
        let flags = Flags::parse(
            ["-Ctarget-cpu=haswell", "-Ctarget-feature=+avx2", "-Cpanic=abort"].into_iter(),
        );
        for explicit in [false, true] {
            let graph: UnitGraph = serde_json::from_value(graph.clone()).unwrap();
            let plan = graph.plan(&flags, explicit).unwrap();
            let hosted = plan.package("path+file:///w/mac#mac@0.1.0").unwrap();
            let targeted = plan.package("path+file:///w/app#app@0.1.0").unwrap();
            assert!(targeted.codegen_flags.iter().any(|flag| flag == "-Ctarget-cpu=haswell"));
            assert_eq!(
                hosted.codegen_flags.iter().any(|flag| flag == "-Ctarget-cpu=haswell"),
                !explicit
            );
            assert_eq!(hosted.profile.panic.name(), if explicit { "unwind" } else { "abort" });
        }
    }

    #[test]
    fn force_frame_pointers_is_read_in_each_spelling() {
        let kept = |flags: &str| Flags::parse(flags.split_whitespace()).frame_pointers;
        assert!(kept("-Cforce-frame-pointers=yes"));
        assert!(kept("--cap-lints allow -C force-frame-pointers"));
        assert!(kept("-C force_frame_pointers=always"));
        assert!(kept("-Cforce-frame-pointers=non-leaf"));
        assert!(!kept("-Cforce-frame-pointers=yes -Cforce-frame-pointers=no"));
        assert!(!kept("-C link-dead-code"));
    }

    #[test]
    fn the_flags_that_cap_lints_are_read() {
        let capped = |flags: &str| Flags::parse(flags.split_whitespace()).lints_capped;
        assert!(capped("--cap-lints warn"));
        assert!(capped("-C opt-level=1 --cap-lints=allow"));
        assert!(capped("--force-warn non_snake_case"));
        assert!(!capped("-D warnings -A dead_code"));
        assert!(!capped("--cap-lintsx"));
    }
}
