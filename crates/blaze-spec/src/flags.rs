//! Cargo's build flags read strictly: every flag is honoured, ignored for a stated reason
//! (it changes only what is printed or how fast the build runs), or refused by name.

use blaze_metadata::units::TargetFilter;

use crate::Refusal;

/// The cargo command a spec is for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Subcommand {
    /// `cargo check`.
    Check,
    /// `cargo build`.
    Build,
    /// `cargo run`.
    Run,
    /// `cargo test`.
    Test,
}

/// What a command asks, as far as the build and the run are concerned: the flags that are
/// honoured, nothing that is only printed. Every field is a function of the arguments alone.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Request {
    /// The command; `Build` until set.
    pub command: Option<Subcommand>,
    /// `--manifest-path`, as typed.
    pub manifest_path: Option<String>,
    /// `-p` and `--package`, as typed (a package id spec).
    pub packages: Vec<String>,
    /// `--workspace` and `--all`.
    pub workspace: bool,
    /// `--exclude`.
    pub excludes: Vec<String>,
    /// `-F` and `--features`, split at commas and spaces.
    pub features: Vec<String>,
    /// `--all-features`.
    pub all_features: bool,
    /// `--no-default-features`.
    pub no_default_features: bool,
    /// `--profile`, or `release` for `--release`; `None` is the command's own.
    pub profile: Option<String>,
    /// `--lib`, `--bin`, `--test` and the other target flags.
    pub targets: TargetFilter,
    /// `--doc`: only the doc tests (`cargo test`).
    pub doc: bool,
    /// `--config`, as typed.
    pub config: Vec<String>,
    /// `--offline`, `--frozen`.
    pub offline: bool,
    /// `--locked`, `--frozen`.
    pub locked: bool,
    /// `--no-run`.
    pub no_run: bool,
    /// `--no-fail-fast`.
    pub no_fail_fast: bool,
    /// The words before `--` that are no flags: `cargo test`'s TESTNAME, `cargo run`'s
    /// program arguments.
    pub positional: Vec<String>,
    /// Every word after `--`, as typed.
    pub trailing: Vec<String>,
}

impl Request {
    /// Reads the arguments after the subcommand. A flag blaze does not honour is a
    /// [`Refusal`] naming it; one that changes nothing about the build or the run is
    /// accepted and left out.
    pub fn parse(_command: Subcommand, _args: &[String]) -> Result<Request, Refusal> {
        Err(Refusal::new("flags", "not read yet"))
    }
}
