//! The unit graph of a build, made by blaze with no cargo: what `cargo <command> --unit-graph`
//! prints (cargo 1.99.0's `unit_dependencies` and `unit_generator`, ported), unit by unit: the
//! package, the target, the mode, the features that unit is built with (host and target apart,
//! as the resolver unifies them), its profile once `[profile.*.package.*]` and `build-override`
//! apply, and the units it depends on.
//!
//! `harness/dist/buildspec.py` compares [`UnitGraph::to_cargo_json`] with cargo's `--unit-graph`
//! over its fixtures; `docs/resolve.md` says what else is compared.

use std::path::PathBuf;

use anyhow::Result;

use crate::Asked;

/// What cargo is asked to compile: `cargo check`, `build` (and `run`), `test`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Command {
    /// `cargo check`: libraries are checked, build scripts and proc macros built.
    Check,
    /// `cargo build` and `cargo run`.
    Build,
    /// `cargo test`: unit tests, integration tests and the rest of the targets' tests, with the
    /// dev-dependencies.
    Test,
}

/// The targets of the members a build compiles, as `--lib`, `--bin`, `--bins`, `--example`,
/// `--examples`, `--test`, `--tests`, `--bench`, `--benches` and `--all-targets` choose them.
/// `Default` is what the command compiles with none of them (cargo's `CompileFilter::Default`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TargetFilter {
    /// Whether any of the flags was given. When none was, the command's defaults apply.
    pub explicit: bool,
    /// `--lib`.
    pub lib: bool,
    /// `--bins`, or the names of `--bin`: `None` is not asked for, `Some(vec![])` is every one.
    pub bins: Option<Vec<String>>,
    /// `--examples`, or the names of `--example`.
    pub examples: Option<Vec<String>>,
    /// `--tests`, or the names of `--test`.
    pub tests: Option<Vec<String>>,
    /// `--benches`, or the names of `--bench`.
    pub benches: Option<Vec<String>>,
    /// `--all-targets`.
    pub all_targets: bool,
}

/// What [`unit_graph`] is asked: the workspace as [`Asked`] has it (`build` is required: it
/// selects the members, and `platform` the triple and cfg of the host and the target), the
/// command, which of the members' targets it compiles, and the profile it is asked for.
pub struct UnitsAsked<'a> {
    /// The workspace, its configuration, the members and the features asked for.
    pub asked: &'a Asked<'a>,
    /// The command whose units are wanted.
    pub command: Command,
    /// Which targets of the members.
    pub targets: &'a TargetFilter,
    /// `dev`, `release`, `test`, `bench` or a custom profile's name; `None` for the command's
    /// own (`dev`, and `test` for a test).
    pub profile: Option<&'a str>,
}

/// The kind of a target. `CustomBuild` is a build script, `ProcMacro` a `proc-macro` library.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum TargetKind {
    /// A library (`lib`, `rlib`, `dylib`, `cdylib`, `staticlib`).
    Lib,
    /// A binary.
    Bin,
    /// An integration test.
    Test,
    /// An example.
    Example,
    /// A benchmark.
    Bench,
    /// A build script.
    CustomBuild,
    /// A procedural macro library.
    ProcMacro,
}

/// cargo's `CompileMode` for a unit.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Mode {
    /// Compiled to be linked or loaded.
    Build,
    /// Checked: metadata only.
    Check,
    /// Compiled with `--test`: the libtest harness is in it.
    Test,
    /// A build script, run.
    RunCustomBuild,
}

/// The package of a unit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UnitPackage {
    /// The name.
    pub name: String,
    /// The version.
    pub version: String,
    /// Where it comes from as `Cargo.lock` spells it (`registry+https://...`, `git+https://...`),
    /// or `path+` and the package's directory relative to the workspace root, so that the same
    /// source on another machine is the same.
    pub source: String,
    /// The directory of its manifest.
    pub manifest_dir: PathBuf,
    /// Whether it is a member of the workspace.
    pub member: bool,
}

/// The target of a unit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UnitTarget {
    /// Its kind.
    pub kind: TargetKind,
    /// `lib`, `rlib`, `bin`, `proc-macro` and the like, as cargo's unit graph spells them.
    pub crate_types: Vec<String>,
    /// Its name.
    pub name: String,
    /// The root file of the crate.
    pub src_path: PathBuf,
    /// The edition, `2021`.
    pub edition: u16,
    /// Whether the libtest harness is used (`harness = false` is a custom harness).
    pub harness: bool,
}

/// An edge: a unit this one is compiled against.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UnitDep {
    /// The index in [`UnitGraph::units`].
    pub index: usize,
    /// The name the crate is used under.
    pub extern_crate_name: String,
    /// `public = true` (unstable): always false on stable.
    pub public: bool,
    /// Whether it is left out of the prelude (`noprelude`).
    pub noprelude: bool,
}

/// One compilation.
#[derive(Clone, Debug, PartialEq)]
pub struct Unit {
    /// Its package.
    pub package: UnitPackage,
    /// Its target.
    pub target: UnitTarget,
    /// Its mode.
    pub mode: Mode,
    /// Its profile: the package's override, or the build override if it is built for the host.
    pub profile: blaze_manifest::profiles::Profile,
    /// The features it is built with, sorted.
    pub features: Vec<String>,
    /// Whether it is built for the host: a build script, a proc macro, and what they depend on.
    pub for_host: bool,
    /// What it depends on.
    pub dependencies: Vec<UnitDep>,
}

/// The units of a build and the ones the command asked for.
#[derive(Clone, Debug, PartialEq)]
pub struct UnitGraph {
    /// Every unit.
    pub units: Vec<Unit>,
    /// The indexes of the units the command named: the targets of the members it compiles.
    pub roots: Vec<usize>,
    /// The workspace's root directory.
    pub root: PathBuf,
    /// The bytes of `Cargo.lock`.
    pub lock: Vec<u8>,
}

impl UnitGraph {
    /// The graph as `cargo build --unit-graph` prints it (`{version, units, roots}`; `platform`
    /// null when nothing is built for another target), so that a harness can compare the two.
    pub fn to_cargo_json(&self) -> serde_json::Value {
        todo!("unit C2: the unit graph")
    }
}

/// The unit graph of `asked`, made with no cargo.
pub fn unit_graph(_asked: &UnitsAsked<'_>) -> Result<UnitGraph> {
    anyhow::bail!("the unit graph is not made yet")
}
