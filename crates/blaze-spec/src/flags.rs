//! Cargo's build flags read strictly: every flag is honoured, ignored for a stated reason
//! (it changes only what is printed or how fast the build runs), or refused by name.
//!
//! [`FLAGS`] is the one table of them, with a reason for each, which a test walks: each entry
//! is read in the class it names. A flag that is not in the table is refused, and so is one
//! cargo itself rejects for the command (`--exact` before `--`, `--workspace` for `cargo run`).
//! [`Request::parse`] is given the words after the subcommand; cargo also takes its flags
//! before it, which a caller moves behind it as `cargo ra-jit`'s own reader does.
//!
//! A word that is not a flag is `cargo test`'s one TESTNAME, or an argument of the program for
//! `cargo run`. Cargo gives `run` every word after the first such argument, flags and `--`
//! included, to the program; blaze reads flags there, so it refuses one rather than guess
//! which reading a caller meant.

use blaze_metadata::units::TargetFilter;

use crate::Refusal;
use crate::refuse::Reason;

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

impl Subcommand {
    /// The command's name, as cargo spells it.
    pub fn name(self) -> &'static str {
        match self {
            Subcommand::Check => "check",
            Subcommand::Build => "build",
            Subcommand::Run => "run",
            Subcommand::Test => "test",
        }
    }
}

/// How a flag is read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Class {
    /// It changes the spec.
    Honoured,
    /// It changes only what is printed or how fast the build runs, and is left out.
    Ignored,
    /// It is refused, for the reason the catalogue gives.
    Refused(Reason),
}

/// What follows a flag.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Takes {
    /// Nothing: it is a switch.
    Nothing,
    /// A value, in the next word or after `=` (`-p name`, `-pname`, `--package=name`).
    Value,
    /// A value only after `=`: `--timings` or `--timings=html`.
    ValueAfterEquals,
}

/// One flag of cargo's build commands.
#[derive(Clone, Copy, Debug)]
pub struct Flag {
    /// The long spelling without its dashes, or empty for a flag with only a short one.
    pub long: &'static str,
    /// The short spelling.
    pub short: Option<char>,
    /// What follows it.
    pub takes: Takes,
    /// How blaze reads it.
    pub class: Class,
    /// The commands that take it; cargo rejects it for the others.
    pub commands: &'static [Subcommand],
    /// A value the flag is read with in the table's test, empty for a switch.
    pub sample: &'static str,
    /// Checks the value of an ignored flag, and says why it is refused when it is.
    pub check: Option<fn(&str) -> Option<Reason>>,
    /// Why the flag is in its class, in a line. A refused flag's is the catalogue's.
    because: &'static str,
}

impl Flag {
    /// Why the flag is in its class.
    pub fn reason(&self) -> &'static str {
        match self.class {
            Class::Refused(reason) => reason.entry().why,
            _ => self.because,
        }
    }

    /// The flag as it is typed: `--package`, or `-Z` for a flag with no long spelling.
    pub fn spelled(&self) -> String {
        match (self.long.is_empty(), self.short) {
            (false, _) => format!("--{}", self.long),
            (true, Some(short)) => format!("-{short}"),
            (true, None) => String::new(),
        }
    }
}

const ALL: &[Subcommand] =
    &[Subcommand::Check, Subcommand::Build, Subcommand::Run, Subcommand::Test];
const NOT_RUN: &[Subcommand] = &[Subcommand::Check, Subcommand::Build, Subcommand::Test];
const TEST: &[Subcommand] = &[Subcommand::Test];

const fn flag(
    long: &'static str,
    short: Option<char>,
    takes: Takes,
    class: Class,
    commands: &'static [Subcommand],
    sample: &'static str,
    because: &'static str,
) -> Flag {
    Flag { long, short, takes, class, commands, sample, check: None, because }
}

const fn checked(flag: Flag, check: fn(&str) -> Option<Reason>) -> Flag {
    Flag { check: Some(check), ..flag }
}

/// Every flag of `cargo check`, `build`, `run` and `test` that blaze knows, in its class.
pub const FLAGS: &[Flag] = &[
    // Honoured: each changes the spec.
    flag(
        "package",
        Some('p'),
        Takes::Value,
        Class::Honoured,
        ALL,
        "a",
        "selects the members built",
    ),
    flag("workspace", None, Takes::Nothing, Class::Honoured, NOT_RUN, "", "selects every member"),
    flag(
        "all",
        None,
        Takes::Nothing,
        Class::Honoured,
        NOT_RUN,
        "",
        "cargo's deprecated spelling of --workspace",
    ),
    flag(
        "exclude",
        None,
        Takes::Value,
        Class::Honoured,
        NOT_RUN,
        "a",
        "leaves members out of --workspace",
    ),
    flag(
        "features",
        Some('F'),
        Takes::Value,
        Class::Honoured,
        ALL,
        "a",
        "turns features on, which changes the units' features and cfgs",
    ),
    flag(
        "all-features",
        None,
        Takes::Nothing,
        Class::Honoured,
        ALL,
        "",
        "turns every feature of the members on",
    ),
    flag(
        "no-default-features",
        None,
        Takes::Nothing,
        Class::Honoured,
        ALL,
        "",
        "turns the default features off",
    ),
    flag(
        "release",
        Some('r'),
        Takes::Nothing,
        Class::Honoured,
        ALL,
        "",
        "builds with the release profile",
    ),
    flag(
        "profile",
        None,
        Takes::Value,
        Class::Honoured,
        ALL,
        "release",
        "builds with the named profile",
    ),
    flag(
        "lib",
        None,
        Takes::Nothing,
        Class::Honoured,
        NOT_RUN,
        "",
        "selects the members' libraries",
    ),
    flag("bin", None, Takes::Value, Class::Honoured, ALL, "a", "selects the named binary"),
    flag("bins", None, Takes::Nothing, Class::Honoured, NOT_RUN, "", "selects every binary"),
    flag("example", None, Takes::Value, Class::Honoured, ALL, "a", "selects the named example"),
    flag("examples", None, Takes::Nothing, Class::Honoured, NOT_RUN, "", "selects every example"),
    flag(
        "test",
        None,
        Takes::Value,
        Class::Honoured,
        NOT_RUN,
        "a",
        "selects the named integration test",
    ),
    flag("tests", None, Takes::Nothing, Class::Honoured, NOT_RUN, "", "selects every test target"),
    flag("bench", None, Takes::Value, Class::Honoured, NOT_RUN, "a", "selects the named benchmark"),
    flag("benches", None, Takes::Nothing, Class::Honoured, NOT_RUN, "", "selects every benchmark"),
    flag(
        "all-targets",
        None,
        Takes::Nothing,
        Class::Honoured,
        NOT_RUN,
        "",
        "selects every target of every kind",
    ),
    flag(
        "doc",
        None,
        Takes::Nothing,
        Class::Honoured,
        TEST,
        "",
        "runs only the doc tests, of the libraries",
    ),
    flag(
        "manifest-path",
        None,
        Takes::Value,
        Class::Honoured,
        ALL,
        "a/Cargo.toml",
        "names the manifest the workspace is found from",
    ),
    flag(
        "config",
        None,
        Takes::Value,
        Class::Honoured,
        ALL,
        "build.jobs=1",
        "adds configuration, which can change a profile or what is built",
    ),
    flag(
        "offline",
        None,
        Takes::Nothing,
        Class::Honoured,
        ALL,
        "",
        "keeps resolution off the network; changes whether a spec can be made, not what it is",
    ),
    flag(
        "locked",
        None,
        Takes::Nothing,
        Class::Honoured,
        ALL,
        "",
        "fails on a lock file the manifests no longer agree with, as blaze's resolution always does",
    ),
    flag("frozen", None, Takes::Nothing, Class::Honoured, ALL, "", "is --offline and --locked"),
    flag(
        "no-run",
        None,
        Takes::Nothing,
        Class::Honoured,
        TEST,
        "",
        "builds the tests and runs none",
    ),
    flag(
        "no-fail-fast",
        None,
        Takes::Nothing,
        Class::Honoured,
        TEST,
        "",
        "runs the remaining tests after one fails",
    ),
    // Ignored: each changes only what is printed or how fast the build is.
    checked(
        flag(
            "color",
            None,
            Takes::Value,
            Class::Ignored,
            ALL,
            "never",
            "changes only whether messages are in colour",
        ),
        color,
    ),
    checked(
        flag(
            "message-format",
            None,
            Takes::Value,
            Class::Ignored,
            ALL,
            "short",
            "changes only how human messages are written; a JSON stream is refused",
        ),
        message_format,
    ),
    flag(
        "verbose",
        Some('v'),
        Takes::Nothing,
        Class::Ignored,
        ALL,
        "",
        "changes only what is printed",
    ),
    flag(
        "quiet",
        Some('q'),
        Takes::Nothing,
        Class::Ignored,
        ALL,
        "",
        "changes only what is printed",
    ),
    flag(
        "jobs",
        Some('j'),
        Takes::Value,
        Class::Ignored,
        ALL,
        "1",
        "changes only how many units build at once",
    ),
    checked(
        flag(
            "timings",
            None,
            Takes::ValueAfterEquals,
            Class::Ignored,
            ALL,
            "html",
            "writes a report beside the build and changes nothing in it",
        ),
        timings,
    ),
    flag(
        "future-incompat-report",
        None,
        Takes::Nothing,
        Class::Ignored,
        ALL,
        "",
        "changes only what is printed at the end",
    ),
    flag(
        "ignore-rust-version",
        None,
        Takes::Nothing,
        Class::Ignored,
        ALL,
        "",
        "skips a rust-version check that fails a build and changes no unit",
    ),
    flag(
        "target-dir",
        None,
        Takes::Value,
        Class::Ignored,
        ALL,
        "a",
        "changes where cargo writes, and a worker writes where it has room",
    ),
    // Refused: blaze cannot honour each.
    flag("target", None, Takes::Value, Class::Refused(Reason::Target), ALL, "a", ""),
    flag("", Some('Z'), Takes::Value, Class::Refused(Reason::UnstableFlag), ALL, "a", ""),
    flag("", Some('C'), Takes::Value, Class::Refused(Reason::ChangeDir), ALL, "a", ""),
    flag("build-plan", None, Takes::Nothing, Class::Refused(Reason::BuildPlan), ALL, "", ""),
    flag("unit-graph", None, Takes::Nothing, Class::Refused(Reason::UnitGraphFlag), ALL, "", ""),
    flag("artifact-dir", None, Takes::Value, Class::Refused(Reason::ArtifactDir), ALL, "a", ""),
    flag("out-dir", None, Takes::Value, Class::Refused(Reason::ArtifactDir), ALL, "a", ""),
    flag("keep-going", None, Takes::Nothing, Class::Refused(Reason::KeepGoing), ALL, "", ""),
    flag("lockfile-path", None, Takes::Value, Class::Refused(Reason::LockfilePath), ALL, "a", ""),
    flag("help", Some('h'), Takes::Nothing, Class::Refused(Reason::Help), ALL, "", ""),
];

/// The options of libtest, which cargo rejects before `--` and the harness takes after it.
const LIBTEST: &[&str] = &[
    "exact",
    "ignored",
    "include-ignored",
    "nocapture",
    "show-output",
    "test-threads",
    "skip",
    "list",
    "format",
    "logfile",
    "report-time",
    "ensure-time",
    "shuffle",
    "shuffle-seed",
    "force-run-in-process",
];

fn color(value: &str) -> Option<Reason> {
    (!matches!(value, "auto" | "always" | "never")).then_some(Reason::BadValue)
}

fn message_format(value: &str) -> Option<Reason> {
    value.split(',').find_map(|format| match format {
        "human" | "short" => None,
        json if json.starts_with("json") => Some(Reason::MessageFormat),
        _ => Some(Reason::BadValue),
    })
}

fn timings(value: &str) -> Option<Reason> {
    (value != "html").then_some(Reason::TimingsValue)
}

fn by_long(name: &str) -> Option<&'static Flag> {
    FLAGS.iter().find(|it| !it.long.is_empty() && it.long == name)
}

fn by_short(short: char) -> Option<&'static Flag> {
    FLAGS.iter().find(|it| it.short == Some(short))
}

/// What a command asks, as far as the build and the run are concerned: the flags that are
/// honoured, nothing that is only printed. Every field is a function of the arguments alone.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Request {
    /// The command; `Build` until set.
    pub command: Option<Subcommand>,
    /// `--manifest-path`, as typed.
    pub manifest_path: Option<std::path::PathBuf>,
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
    /// What `cargo test` passes to libtest that changes what runs, read out of `trailing`.
    pub libtest: Libtest,
}

/// The options of libtest that say which tests run and how, read out of what follows `--`.
/// The words themselves stay in [`Request::trailing`] for a harness that is not libtest.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Libtest {
    /// `--exact`.
    pub exact: bool,
    /// Each `--skip`, in order.
    pub skip: Vec<String>,
    /// 0 for neither, 1 for `--ignored`, 2 for `--include-ignored`.
    pub ignored: u8,
    /// `--test-threads`.
    pub test_threads: Option<u32>,
}

/// The ways of the flags and values read so far, which only the end of the words settles.
#[derive(Default)]
struct Seen {
    release: bool,
    profile: Option<String>,
    lib: bool,
    all_targets: bool,
    selecting: Option<String>,
    bins: Names,
    examples: Names,
    tests: Names,
    benches: Names,
}

/// The names given to a target flag, or all of them.
#[derive(Default)]
struct Names {
    all: bool,
    names: Vec<String>,
}

impl Names {
    fn rule(self) -> Option<Vec<String>> {
        if self.all {
            Some(Vec::new())
        } else if self.names.is_empty() {
            None
        } else {
            Some(self.names)
        }
    }
}

impl Request {
    /// Whether the command selects targets whose resolution includes dev-dependencies.
    /// Explicitly selecting a library or binary alone does not enable them.
    pub fn needs_dev_dependencies(&self) -> bool {
        self.command == Some(Subcommand::Test)
            || self.targets.tests.is_some()
            || self.targets.examples.is_some()
            || self.targets.benches.is_some()
            || self.targets.all_targets
    }

    /// Reads the arguments after the subcommand. A flag blaze does not honour is a
    /// [`Refusal`] naming it; one that changes nothing about the build or the run is
    /// accepted and left out.
    pub fn parse(command: Subcommand, args: &[String]) -> Result<Request, Refusal> {
        let mut request = Request { command: Some(command), ..Request::default() };
        let mut seen = Seen::default();
        let mut words = args.iter();
        let mut after_dashes = false;
        while let Some(word) = words.next() {
            if after_dashes {
                request.trailing.push(word.clone());
                continue;
            }
            if command == Subcommand::Run
                && !request.positional.is_empty()
                && word.starts_with('-')
                && word.len() > 1
            {
                return Err(Refusal::of(Reason::RunArgsAfterFirst, word));
            }
            if word == "--" {
                match command {
                    Subcommand::Check | Subcommand::Build => {
                        return Err(Refusal::of(Reason::ExtraArgument, word));
                    }
                    _ => {}
                }
                after_dashes = true;
            } else if let Some(long) = word.strip_prefix("--") {
                let (name, attached) = match long.split_once('=') {
                    Some((name, value)) => (name, Some(value)),
                    None => (long, None),
                };
                let Some(flag) = by_long(name) else {
                    return Err(unknown(command, word, name));
                };
                let value = take(flag, word, attached, &mut words)?;
                request.read(command, flag, value.as_deref(), &mut seen)?;
            } else if word.starts_with('-') && word.len() > 1 {
                // A cluster of switches (`-rv`), the last of which may take the rest of the
                // word or the next one as its value (`-pname`, `-j8`, `-p name`).
                for (at, short) in word[1..].char_indices() {
                    let Some(flag) = by_short(short) else {
                        return Err(unknown(command, word, &short.to_string()));
                    };
                    if flag.takes == Takes::Nothing {
                        request.read(command, flag, None, &mut seen)?;
                        continue;
                    }
                    let rest = &word[1 + at + short.len_utf8()..];
                    let attached =
                        (!rest.is_empty()).then(|| rest.strip_prefix('=').unwrap_or(rest));
                    let value = take(flag, word, attached, &mut words)?;
                    request.read(command, flag, value.as_deref(), &mut seen)?;
                    break;
                }
            } else {
                match command {
                    Subcommand::Check | Subcommand::Build => {
                        return Err(Refusal::of(Reason::ExtraArgument, word));
                    }
                    Subcommand::Test if !request.positional.is_empty() => {
                        return Err(Refusal::of(Reason::ExtraArgument, word));
                    }
                    _ => request.positional.push(word.clone()),
                }
            }
        }
        request.finish(command, seen)?;
        Ok(request)
    }

    /// Applies one flag with its value. The flag is of a command that takes it, and an
    /// ignored one is checked and left out.
    fn read(
        &mut self,
        command: Subcommand,
        flag: &Flag,
        value: Option<&str>,
        seen: &mut Seen,
    ) -> Result<(), Refusal> {
        let spelled = flag.spelled();
        if let Class::Refused(reason) = flag.class {
            return Err(Refusal::of(reason, refused_as_typed(&spelled, value)));
        }
        if !flag.commands.contains(&command) {
            let named = format!("{spelled} for `cargo {}`", command.name());
            return Err(Refusal::of(Reason::UnknownFlag, named));
        }
        if flag.class == Class::Ignored {
            if let (Some(check), Some(value)) = (flag.check, value)
                && let Some(reason) = check(value)
            {
                return Err(Refusal::of(reason, format!("{spelled}={value}")));
            }
            return Ok(());
        }
        let value = value.unwrap_or_default();
        let select = |seen: &mut Seen| {
            seen.selecting.get_or_insert_with(|| spelled.clone());
        };
        match flag.long {
            "package" => self.packages.push(value.to_owned()),
            "workspace" | "all" => self.workspace = true,
            "exclude" => self.excludes.push(value.to_owned()),
            "features" => {
                let listed = value.split([',', ' ']).filter(|it| !it.is_empty());
                self.features.extend(listed.map(str::to_owned));
            }
            "all-features" => self.all_features = true,
            "no-default-features" => self.no_default_features = true,
            "release" => seen.release = true,
            "profile" => seen.profile = Some(value.to_owned()),
            "lib" => {
                seen.lib = true;
                select(seen);
            }
            "bin" => {
                seen.bins.names.push(value.to_owned());
                select(seen);
            }
            "bins" => {
                seen.bins.all = true;
                select(seen);
            }
            "example" => {
                seen.examples.names.push(value.to_owned());
                select(seen);
            }
            "examples" => {
                seen.examples.all = true;
                select(seen);
            }
            "test" => {
                seen.tests.names.push(value.to_owned());
                select(seen);
            }
            "tests" => {
                seen.tests.all = true;
                select(seen);
            }
            "bench" => {
                seen.benches.names.push(value.to_owned());
                select(seen);
            }
            "benches" => {
                seen.benches.all = true;
                select(seen);
            }
            "all-targets" => {
                seen.all_targets = true;
                select(seen);
            }
            "doc" => self.doc = true,
            "manifest-path" => self.manifest_path = Some(value.into()),
            "config" => {
                let key = value.split_once('=').map_or(value, |it| it.0).trim();
                if key == "unstable" || key.starts_with("unstable.") {
                    return Err(Refusal::of(Reason::ConfigUnstable, format!("--config {value}")));
                }
                self.config.push(value.to_owned());
            }
            "offline" => self.offline = true,
            "locked" => self.locked = true,
            "frozen" => {
                self.offline = true;
                self.locked = true;
            }
            "no-run" => self.no_run = true,
            "no-fail-fast" => self.no_fail_fast = true,
            other => unreachable!("the honoured flag `{other}` has no arm"),
        }
        Ok(())
    }

    /// What the words settle only as a whole: the profile, the targets, and the combinations
    /// cargo rejects.
    fn finish(&mut self, command: Subcommand, seen: Seen) -> Result<(), Refusal> {
        self.profile = match (seen.release, seen.profile) {
            (true, None) => Some("release".to_owned()),
            (true, Some(named)) if named == "release" => Some(named),
            (true, Some(named)) => {
                return Err(Refusal::of(
                    Reason::ProfileConflict,
                    format!("--release --profile {named}"),
                ));
            }
            (false, Some(named)) if named == "doc" => {
                let detail = "profile `doc` is reserved and not allowed to be explicitly specified";
                return Err(Refusal::with(Reason::BadValue, "--profile doc", detail));
            }
            (false, Some(named)) => {
                if let Err(error) = cargo_util_schemas::manifest::ProfileName::new(&named) {
                    return Err(Refusal::with(
                        Reason::BadValue,
                        format!("--profile {named}"),
                        error,
                    ));
                }
                Some(named)
            }
            (false, None) => None,
        };
        if !self.excludes.is_empty() && !self.workspace {
            return Err(Refusal::of(Reason::ExcludeWithoutWorkspace, "--exclude"));
        }
        if self.doc
            && let Some(selecting) = seen.selecting
        {
            return Err(Refusal::of(Reason::DocWithTargets, format!("--doc {selecting}")));
        }
        self.targets = TargetFilter {
            all_test_targets: false,
            doc_only: self.doc,
            explicit: seen.lib
                || seen.all_targets
                || seen.bins.all
                || !seen.bins.names.is_empty()
                || seen.examples.all
                || !seen.examples.names.is_empty()
                || seen.tests.all
                || !seen.tests.names.is_empty()
                || seen.benches.all
                || !seen.benches.names.is_empty(),
            lib: seen.lib,
            bins: seen.bins.rule(),
            examples: seen.examples.rule(),
            tests: seen.tests.rule(),
            benches: seen.benches.rule(),
            all_targets: seen.all_targets,
        };
        if command == Subcommand::Test {
            self.targets.all_test_targets =
                !self.doc && !self.targets.explicit && !self.positional.is_empty();
            self.libtest = Libtest::read(&self.trailing)?;
        }
        Ok(())
    }
}

/// The value of `flag`: attached to it, or the next word.
fn take<'a>(
    flag: &Flag,
    word: &str,
    attached: Option<&str>,
    words: &mut impl Iterator<Item = &'a String>,
) -> Result<Option<String>, Refusal> {
    match (flag.takes, attached) {
        (Takes::Nothing, None) => Ok(None),
        (Takes::Nothing, Some(value)) => {
            let given = format!("{word} (a value for a switch: `{value}`)");
            Err(Refusal::of(Reason::BadValue, given))
        }
        (Takes::ValueAfterEquals, value) => Ok(value.map(str::to_owned)),
        (Takes::Value, Some(value)) => Ok(Some(value.to_owned())),
        (Takes::Value, None) => {
            // A word that is a flag is not a value (clap's rule), but a negative number is.
            let next = words.next().filter(|it| {
                !it.starts_with('-') || it.len() == 1 || it[1..].parse::<i64>().is_ok()
            });
            match next {
                Some(value) => Ok(Some(value.clone())),
                None => Err(Refusal::of(Reason::MissingValue, word)),
            }
        }
    }
}

/// A refused flag as the requester typed it, with its value when it has one.
fn refused_as_typed(spelled: &str, value: Option<&str>) -> String {
    match value {
        Some(value) => format!("{spelled} {value}"),
        None => spelled.to_owned(),
    }
}

/// The refusal of a flag that is not in the table.
fn unknown(command: Subcommand, word: &str, name: &str) -> Refusal {
    if LIBTEST.contains(&name) {
        return Refusal::of(Reason::LibtestFlag, word);
    }
    Refusal::of(Reason::UnknownFlag, format!("{word} for `cargo {}`", command.name()))
}

impl Libtest {
    /// Reads libtest's `--exact`, `--skip`, `--ignored`, `--include-ignored` and
    /// `--test-threads` out of the words after cargo's `--`, in both spellings of a value.
    /// libtest stops reading options at a `--` of its own, and rejects an option given twice.
    pub fn read(words: &[String]) -> Result<Libtest, Refusal> {
        let mut out = Libtest::default();
        let (mut exact, mut ignored, mut include, mut threads) = (false, false, false, false);
        let mut words = words.iter();
        while let Some(word) = words.next() {
            if word == "--" {
                break;
            }
            let Some(option) = word.strip_prefix("--") else { continue };
            let (name, attached) = match option.split_once('=') {
                Some((name, value)) => (name, Some(value.to_owned())),
                None => (option, None),
            };
            let once = |seen: &mut bool| {
                if std::mem::replace(seen, true) {
                    return Err(Refusal::of(Reason::LibtestDuplicate, word.clone()));
                }
                Ok(())
            };
            match name {
                "exact" => {
                    once(&mut exact)?;
                    out.exact = true;
                }
                "ignored" => {
                    once(&mut ignored)?;
                    out.ignored = 1;
                }
                "include-ignored" => {
                    once(&mut include)?;
                    out.ignored = 2;
                }
                "skip" => match attached.or_else(|| words.next().cloned()) {
                    Some(skip) => out.skip.push(skip),
                    None => return Err(Refusal::of(Reason::MissingValue, word.clone())),
                },
                "test-threads" => {
                    once(&mut threads)?;
                    let Some(value) = attached.or_else(|| words.next().cloned()) else {
                        return Err(Refusal::of(Reason::MissingValue, word.clone()));
                    };
                    match value.parse::<u32>() {
                        Ok(n) if n > 0 => out.test_threads = Some(n),
                        _ => {
                            return Err(Refusal::of(
                                Reason::LibtestThreads,
                                format!("--test-threads {value}"),
                            ));
                        }
                    }
                }
                _ => {}
            }
        }
        if ignored && include {
            return Err(Refusal::of(Reason::LibtestIgnored, "--ignored --include-ignored"));
        }
        Ok(out)
    }
}
