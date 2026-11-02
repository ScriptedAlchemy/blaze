use std::fmt;

/// What blaze cannot honour of a build, named, before it is scheduled.
///
/// `what` is the flag, profile setting, variable or tool exactly as the requester wrote or has
/// it (`--target`, `profile.release.lto = "thin"`, `RUSTC_WRAPPER`); `why` says what blaze does
/// not do. A refused spec is not a failed build: the requester builds it locally.
///
/// `id` names the [`CATALOGUE`] entry the refusal is of, for a harness that counts them; it is
/// empty for a refusal made elsewhere (the platform's).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Refusal {
    /// The thing refused, as the requester wrote it.
    pub what: String,
    /// Why blaze does not honour it.
    pub why: String,
    /// The catalogue entry this is of, or empty.
    pub id: &'static str,
}

impl Refusal {
    /// A refusal of `what` for `why`, of no catalogue entry.
    pub fn new(what: impl Into<String>, why: impl Into<String>) -> Refusal {
        Refusal { what: what.into(), why: why.into(), id: "" }
    }

    /// A refusal of `what` for the reason the catalogue gives.
    pub fn of(reason: Reason, what: impl Into<String>) -> Refusal {
        let entry = reason.entry();
        Refusal { what: what.into(), why: entry.why.to_owned(), id: entry.id }
    }

    /// A refusal of `what` for the reason the catalogue gives, with what is particular to this
    /// one (the unit, the value cargo said) after it.
    pub fn with(reason: Reason, what: impl Into<String>, detail: impl fmt::Display) -> Refusal {
        let entry = reason.entry();
        Refusal { what: what.into(), why: format!("{}: {detail}", entry.why), id: entry.id }
    }

    /// The catalogue entry this refusal is of, when it is of one.
    pub fn entry(&self) -> Option<&'static Entry> {
        CATALOGUE.iter().find(|it| it.id == self.id)
    }
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.what, self.why)
    }
}

impl std::error::Error for Refusal {}

/// Where a refusal comes from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Area {
    /// A flag of the command, or a libtest option after `--`.
    Flag,
    /// A setting of cargo's configuration.
    Config,
    /// A variable of the requester's environment.
    Env,
    /// A setting of the profile of a unit.
    Profile,
    /// A flag of rustc in `RUSTFLAGS` or the configuration.
    Rustflag,
    /// The unit graph, or where the command was run.
    Unit,
}

/// One thing blaze refuses: the pattern of `what`, and `why`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Entry {
    /// The name of the reason, as the [`Reason`] variant spells it.
    pub id: &'static str,
    /// Where it comes from.
    pub area: Area,
    /// What is refused, as a pattern: `--target <triple>`, `profile.<name>.lto = <value>`.
    pub what: &'static str,
    /// What blaze does not do.
    pub why: &'static str,
}

macro_rules! catalogue {
    ($($id:ident $area:ident $what:literal $why:literal;)*) => {
        /// Why a [`Refusal`] was made: one variant for each entry of the [`CATALOGUE`].
        #[derive(Clone, Copy, Debug, PartialEq, Eq)]
        pub enum Reason {
            $(
                #[doc = $why]
                $id,
            )*
        }

        impl Reason {
            /// Every reason, in the order of the catalogue.
            pub const ALL: &'static [Reason] = &[$(Reason::$id),*];

            /// The catalogue's entry of the reason.
            pub fn entry(self) -> &'static Entry {
                &CATALOGUE[self as usize]
            }
        }

        /// Everything blaze refuses, with the pattern of what is refused and why: the one table
        /// the refusals are made from, which the tests walk and the harness prints.
        pub const CATALOGUE: &[Entry] = &[
            $(Entry { id: stringify!($id), area: Area::$area, what: $what, why: $why }),*
        ];
    };
}

catalogue! {
    // The flags of the command.
    Target Flag "--target <triple>"
        "blaze builds for the machine it runs on and never cross-compiles: a `--target`, the host's own included, makes cargo build for a target with a layout and host rules blaze does not model";
    MessageFormat Flag "--message-format json"
        "blaze reads cargo's human and short messages only; a JSON message stream is cargo's to print";
    UnstableFlag Flag "-Z <flag>"
        "an unstable cargo flag changes what cargo does in ways blaze does not read";
    BuildPlan Flag "--build-plan"
        "cargo's build plan lists the rustc runs of a build, and blaze makes none";
    UnitGraphFlag Flag "--unit-graph"
        "the unit graph of a command is for blaze to make for itself, not to print";
    ArtifactDir Flag "--artifact-dir <dir>"
        "blaze copies no artifact out of its store into a directory";
    KeepGoing Flag "--keep-going"
        "blaze has no mode that builds on past a failed unit";
    LockfilePath Flag "--lockfile-path <path>"
        "blaze reads the lock file beside the workspace root only";
    Help Flag "--help"
        "it prints text and builds nothing";
    TimingsValue Flag "--timings=<format>"
        "blaze writes no timing report, and cargo takes any format but `html` only with an unstable flag";
    ChangeDir Flag "-C <dir>"
        "cargo's `-C` is unstable: run blaze in that directory instead";
    UnknownFlag Flag "<flag>"
        "cargo rejects it for this command, or blaze does not read it";
    LibtestFlag Flag "--exact"
        "cargo rejects a libtest flag before `--`: libtest's flags go after it";
    MissingValue Flag "<flag>"
        "the flag takes a value and none was given";
    BadValue Flag "<flag> <value>"
        "cargo rejects that value for the flag";
    ProfileConflict Flag "--release --profile <name>"
        "cargo rejects `--release` together with a `--profile` of another name";
    DocWithTargets Flag "--doc --<target flag>"
        "cargo rejects `--doc` together with flags that select targets";
    ConfigUnstable Flag "--config unstable.<key>=<value>"
        "an unstable cargo setting changes what cargo does in ways blaze does not read";
    ExtraArgument Flag "<word>"
        "cargo rejects a word it has no place for: `cargo build` and `cargo check` take none, and `cargo test` takes one test name before `--`";
    RunArgsAfterFirst Flag "cargo run <arg> --<flag>"
        "cargo run hands every word after its first program argument to the program, flags and `--` included, and blaze reads flags there: write the flags before it";
    ExcludeWithoutWorkspace Flag "--exclude <spec>"
        "cargo rejects `--exclude` without `--workspace`";
    LibtestDuplicate Flag "--exact --exact"
        "libtest rejects an option given more than once, `--skip` apart";
    LibtestIgnored Flag "--ignored --include-ignored"
        "libtest rejects the two together";
    LibtestThreads Flag "--test-threads <n>"
        "libtest takes a number above zero";

    // The configuration.
    ConfigRejected Config "<configuration or manifest>"
        "cargo rejects the configuration or the manifests it is run with";
    BuildTarget Config "build.target"
        "blaze builds for the machine it runs on and never cross-compiles: a `build.target`, the host's own included, makes cargo build for a target with a layout and host rules blaze does not model";
    BuildRustc Config "build.rustc"
        "blaze compiles with its own front end and code generator and never runs a rustc a configuration names";
    BuildRustcWrapper Config "build.rustc-wrapper"
        "a wrapper runs in front of rustc, and blaze runs no rustc";
    BuildRustdoc Config "build.rustdoc"
        "blaze runs the doc tests itself and never runs a rustdoc a configuration names";
    BuildArtifactDir Config "build.artifact-dir"
        "blaze copies no artifact out of its store into a directory";
    BuildWarnings Config "build.warnings"
        "it sets the lint level of every unit, which blaze does not apply from configuration";
    BuildUnstable Config "build.sbom"
        "cargo reads it only with an unstable flag, and blaze writes no such output";
    HostConfig Config "host.<key>"
        "the `[host]` table and `target-applies-to-host` are unstable cross-compilation settings";
    UnstableTable Config "unstable.<key>"
        "the `[unstable]` table turns on unstable cargo features, which blaze does not read";
    TargetRunner Config "target.<triple>.runner"
        "a runner starts every test and program through another program, which a spec does not carry and an executor does not run";
    TargetLinker Config "target.<triple>.linker"
        "blaze links with the system's `cc` and passes no configured linker";
    TargetLinks Config "target.<triple>.<links>"
        "an override of a build script's output is not applied by blaze";
    FeatureUnification Config "resolver.feature-unification"
        "blaze unifies features as `selected` only";
    ConfigPatch Config "patch.<registry>.<crate>"
        "blaze reads `[patch]` from the root manifest only";
    RustdocFlags Config "build.rustdocflags"
        "blaze runs doc tests with no rustdoc flags, and these would change them";
    EnvRelativeOutside Config "env.<NAME> = { relative = true }"
        "a `relative` path is made relative to the workspace root, and this one is of a configuration file outside it";

    // The requester's environment.
    EnvRustc Env "RUSTC"
        "blaze compiles with its own front end and code generator and never runs the rustc a variable names";
    EnvRustcWrapper Env "RUSTC_WRAPPER"
        "a wrapper runs in front of rustc, and blaze runs no rustc";
    EnvRustdoc Env "RUSTDOC"
        "blaze runs the doc tests itself and never runs a rustdoc a variable names";
    EnvToolchain Env "RUSTUP_TOOLCHAIN"
        "the toolchain a spec builds with is the spec's own `toolchain`, not one the environment picks";
    EnvBootstrap Env "RUSTC_BOOTSTRAP"
        "it turns on nightly-only features, whose behaviour in blaze a spec does not describe";
    EnvVersionString Env "RUSTC_OVERRIDE_VERSION_STRING"
        "it changes the version rustc reports, which build scripts read, and a spec does not carry it";
    EnvUnstable Env "CARGO_UNSTABLE_<flag>"
        "an unstable cargo setting changes what cargo does in ways blaze does not read";
    EnvRaJit Env "RA_JIT_<name>"
        "it is a blaze setting that a spec neither carries nor knows to change nothing a build writes";
    EnvRaJitValue Env "RA_JIT_BACKEND, RA_JIT_OPT, RA_JIT_EMIT, RA_JIT_MIR_OPT"
        "blaze reads no other value of it";
    EnvLlvm Env "RA_JIT_BACKEND=llvm"
        "this blaze was built without the llvm backend";

    // The profile of a unit.
    OptLevelSize Profile "profile.<name>.opt-level = \"s\" | \"z\""
        "blaze's code generators have no level that optimizes for size";
    ProfileValue Profile "profile.<name>.<setting> = <value>"
        "blaze does not know that value of the setting";
    Lto Profile "profile.<name>.lto = <value>"
        "link-time optimization compiles the whole program again as it links it, which blaze's per-function compile and link does not do";
    CodegenBackend Profile "profile.<name>.codegen-backend = <name>"
        "blaze's code generators are its own: a profile cannot pick another";
    ProfileRustflags Profile "profile.<name>.rustflags = [..]"
        "blaze reads rustflags from the environment and the configuration only";
    TrimPaths Profile "profile.<name>.trim-paths = <value>"
        "blaze writes the paths of the source it was given into panic locations, `file!()` and debug info, and trims none";
    Rpath Profile "profile.<name>.rpath = true"
        "blaze's link line has no run-path";
    PanicImmediateAbort Profile "profile.<name>.panic = \"immediate-abort\""
        "blaze's code generators have no immediate-abort panic strategy";

    // The flags of rustc in `RUSTFLAGS` and the configuration.
    FlagLto Rustflag "-C lto, -C linker-plugin-lto"
        "link-time optimization compiles the whole program again as it links it, which blaze's per-function compile and link does not do";
    FlagOptSize Rustflag "-C opt-level=s | z"
        "blaze's code generators have no level that optimizes for size";
    FlagCpu Rustflag "-C target-cpu, -C target-feature"
        "blaze's code generators assume the target CPU the platform names and read no flag that changes it";
    FlagLink Rustflag "-C link-arg, -C linker, -L, -l, ..."
        "blaze links with the system's `cc` and a fixed line, and reads no flag that changes what is linked";
    FlagRemap Rustflag "--remap-path-prefix"
        "blaze writes the paths of the source it was given into panic locations, `file!()` and debug info, and remaps none";
    FlagUnstable Rustflag "-Z <flag>"
        "an unstable rustc flag changes what rustc does in ways blaze does not read";
    FlagCodegen Rustflag "-C <option>"
        "blaze's code generators read no such option, so rustc's build with it and blaze's would differ";
    FlagCargoOwns Rustflag "--target, --crate-type, --emit, ..."
        "cargo gives every rustc its own, so a flag of this kind in the rustflags is not part of what a spec describes";
    FlagUnknown Rustflag "<flag>"
        "blaze does not know what it changes";
    FlagValue Rustflag "<flag>=<value>"
        "rustc rejects that value for the flag, or blaze does not know it";
    FlagCfg Rustflag "--cfg <spec>"
        "rustc rejects a cfg that is not a name or a name and a quoted value";
    FlagInput Rustflag "<word>"
        "a word that is no flag is an input file of rustc, which no spec has";

    // The unit graph and where the command ran.
    UnitGraphFailed Unit "the unit graph"
        "blaze could not make the unit graph of the build";
    UnitOrder Unit "two units alike"
        "two units are alike in every field but their dependencies, so the canonical order of the units is not total";
    UnitEdge Unit "a dependency on a build script's run"
        "a run of a build script has no unit that compiles the script to stand for it";
    RunTarget Unit "cargo run"
        "cargo run needs exactly one binary or example to run";
    DocLibrary Unit "--doc"
        "the doc tests are of libraries, and the unit graph has none of the members selected";
    Cwd Unit "the working directory"
        "a spec names the directory it was run in relative to the workspace root, and this one is outside it";
}
