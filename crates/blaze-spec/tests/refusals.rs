//! The catalogue of refusals, walked: every entry has a case that fires it, names the thing,
//! and gives the reason the catalogue gives, next to one that is accepted.

mod common;

use blaze_manifest::profiles::{Lto, PanicStrategy, Profile};
use blaze_metadata::units::{Mode, TargetKind};
use blaze_spec::flags::{Request, Subcommand};
use blaze_spec::profile::{self, Settings};
use blaze_spec::rustflags::Overrides;
use blaze_spec::{Area, CATALOGUE, Reason, Refusal};
use cargo_util_schemas::manifest::TomlTrimPathsValue;

use common::{Fixture, Make, Package, graph, profile_of, spec, spec_on};

type Attempt = Box<dyn Fn() -> Result<(), Refusal>>;

struct Case {
    reason: Reason,
    /// What the refusal names, as a part of its `what`.
    names: &'static str,
    bad: Attempt,
    good: Attempt,
}

fn case(reason: Reason, names: &'static str, bad: Attempt, good: Attempt) -> Case {
    Case { reason, names, bad, good }
}

fn words(args: &[&str]) -> Vec<String> {
    args.iter().map(|it| (*it).to_owned()).collect()
}

/// A command's flags only.
fn flags(command: Subcommand, args: &'static [&'static str]) -> Attempt {
    Box::new(move || Request::parse(command, &words(args)).map(drop))
}

/// A whole spec of a workspace whose configuration file is `config`.
fn config(config: &'static str, command: Subcommand, args: &'static [&'static str]) -> Attempt {
    Box::new(move || {
        let fixture = Fixture::with_config(config);
        spec(&fixture, &fixture.env(), command, args, None).map(drop)
    })
}

/// A whole spec in an environment with `variables`.
fn env(variables: &'static [(&'static str, &'static str)], command: Subcommand) -> Attempt {
    Box::new(move || {
        let fixture = Fixture::new();
        let mut env = fixture.env();
        env.extend(variables.iter().map(|(k, v)| ((*k).to_owned(), (*v).to_owned())));
        spec(&fixture, &env, command, &[], None).map(drop)
    })
}

/// A spec of a blaze with the llvm backend or without.
fn backend(llvm: bool, variables: &'static [(&'static str, &'static str)]) -> Attempt {
    Box::new(move || {
        let fixture = Fixture::new();
        let mut env = fixture.env();
        env.extend(variables.iter().map(|(k, v)| ((*k).to_owned(), (*v).to_owned())));
        spec_on(&common::host(llvm), &fixture, &env, Subcommand::Build, &[], None).map(drop)
    })
}

fn rustflags(flags: &'static str) -> Attempt {
    env(Box::leak(Box::new([("RUSTFLAGS", flags)])), Subcommand::Build)
}

/// A spec of the root manifest with `extra` after it, in the profile `requested`.
fn manifest(extra: &'static str, requested: &'static str) -> Attempt {
    Box::new(move || {
        let fixture = Fixture::with_manifest(extra);
        let args: &[&str] = if requested == "dev" { &[] } else { &["--profile", requested] };
        spec(&fixture, &fixture.env(), Subcommand::Build, args, None).map(drop)
    })
}

/// The profile of a unit with a setting `change` makes.
fn profile_with(change: fn(&mut Profile)) -> Attempt {
    Box::new(move || {
        let fixture = Fixture::new();
        let mut profile = profile_of(&fixture, &fixture.env(), "dev", &Package::member(&fixture.root(), "a"), true, false);
        change(&mut profile);
        let settings = Settings { backend: 0, backend_opt: 0, backend_emit: 0, mir_opt: 0 };
        profile::spec(&profile, &Overrides::default(), &settings, "unit `a`").map(drop)
    })
}

/// A spec over units `make` builds, with `roots`, for `command` and `args`.
fn units(make: fn() -> Vec<Make>, roots: &'static [usize], command: Subcommand, args: &'static [&'static str]) -> Attempt {
    Box::new(move || {
        let fixture = Fixture::new();
        let units = make();
        let build = move |profile: &Profile| graph(units.clone(), roots, profile);
        spec(&fixture, &fixture.env(), command, args, Some(&build)).map(drop)
    })
}

fn lib() -> Vec<Make> {
    vec![Make::new("a", TargetKind::Lib)]
}

fn cases() -> Vec<Case> {
    use Subcommand::{Build, Check, Run, Test};
    vec![
        // The flags.
        case(Reason::Target, "--target", flags(Build, &["--target", "x86_64-unknown-linux-gnu"]), flags(Build, &["--target-dir", "t"])),
        case(Reason::MessageFormat, "--message-format", flags(Build, &["--message-format=json"]), flags(Build, &["--message-format=short"])),
        case(Reason::UnstableFlag, "-Z", flags(Build, &["-Zunstable-options"]), flags(Build, &["--features", "z"])),
        case(Reason::BuildPlan, "--build-plan", flags(Build, &["--build-plan"]), flags(Build, &[])),
        case(Reason::UnitGraphFlag, "--unit-graph", flags(Check, &["--unit-graph"]), flags(Check, &[])),
        case(Reason::ArtifactDir, "--artifact-dir", flags(Build, &["--artifact-dir", "o"]), flags(Build, &["--target-dir", "o"])),
        case(Reason::KeepGoing, "--keep-going", flags(Build, &["--keep-going"]), flags(Build, &["--no-default-features"])),
        case(Reason::LockfilePath, "--lockfile-path", flags(Build, &["--lockfile-path", "l"]), flags(Build, &["--locked"])),
        case(Reason::Help, "--help", flags(Build, &["--help"]), flags(Build, &["--verbose"])),
        case(Reason::TimingsValue, "--timings", flags(Build, &["--timings=json"]), flags(Build, &["--timings=html"])),
        case(Reason::ChangeDir, "-C", flags(Build, &["-C", "d"]), flags(Build, &["-j", "2"])),
        case(Reason::UnknownFlag, "--frobnicate", flags(Build, &["--frobnicate"]), flags(Build, &["--workspace"])),
        case(Reason::LibtestFlag, "--exact", flags(Test, &["--exact"]), flags(Test, &["--", "--exact"])),
        case(Reason::MissingValue, "--package", flags(Build, &["--package"]), flags(Build, &["--package", "a"])),
        case(Reason::BadValue, "--color", flags(Build, &["--color", "sometimes"]), flags(Build, &["--color", "never"])),
        case(Reason::ProfileConflict, "--release", flags(Build, &["--release", "--profile", "dev"]), flags(Build, &["--release", "--profile", "release"])),
        case(Reason::DocWithTargets, "--doc", flags(Test, &["--doc", "--lib"]), flags(Test, &["--doc"])),
        case(Reason::ConfigUnstable, "unstable.", flags(Build, &["--config", "unstable.x=1"]), flags(Build, &["--config", "build.jobs=1"])),
        case(Reason::ExtraArgument, "x", flags(Build, &["x"]), flags(Test, &["x"])),
        case(Reason::RunArgsAfterFirst, "--release", flags(Run, &["a", "--release"]), flags(Run, &["--release", "a"])),
        case(Reason::ExcludeWithoutWorkspace, "--exclude", flags(Build, &["--exclude", "a"]), flags(Build, &["--workspace", "--exclude", "a"])),
        case(Reason::LibtestDuplicate, "--exact", flags(Test, &["--", "--exact", "--exact"]), flags(Test, &["--", "--exact", "--skip", "a", "--skip", "b"])),
        case(Reason::LibtestIgnored, "--ignored", flags(Test, &["--", "--ignored", "--include-ignored"]), flags(Test, &["--", "--include-ignored"])),
        case(Reason::LibtestThreads, "--test-threads", flags(Test, &["--", "--test-threads=0"]), flags(Test, &["--", "--test-threads=1"])),
        // The configuration.
        case(Reason::ConfigRejected, "build", config("[build]\nincremental = \"yes\"\n", Build, &[]), config("[build]\nincremental = true\n", Build, &[])),
        case(Reason::BuildTarget, "build.target", config("[build]\ntarget = \"x\"\n", Build, &[]), config("[build]\ntarget-dir = \"x\"\n", Build, &[])),
        case(Reason::BuildRustc, "build.rustc", config("[build]\nrustc = \"r\"\n", Build, &[]), config("[build]\njobs = 1\n", Build, &[])),
        case(Reason::BuildRustcWrapper, "build.rustc-wrapper", config("[build]\nrustc-wrapper = \"w\"\n", Build, &[]), config("[build]\njobs = 1\n", Build, &[])),
        case(Reason::BuildRustdoc, "build.rustdoc", config("[build]\nrustdoc = \"r\"\n", Build, &[]), config("[build]\njobs = 1\n", Build, &[])),
        case(Reason::BuildArtifactDir, "build.artifact-dir", config("[build]\nartifact-dir = \"o\"\n", Build, &[]), config("[build]\ntarget-dir = \"o\"\n", Build, &[])),
        case(Reason::BuildWarnings, "build.warnings", config("[build]\nwarnings = \"deny\"\n", Build, &[]), config("[build]\nwarnings = \"warn\"\n", Build, &[])),
        case(Reason::BuildUnstable, "build.sbom", config("[build]\nsbom = true\n", Build, &[]), config("[build]\nsbom = false\n", Build, &[])),
        case(Reason::HostConfig, "host.", config("[host]\nlinker = \"l\"\n", Build, &[]), config("[target.x86_64-unknown-linux-gnu]\nlinker = \"l\"\n", Build, &[])),
        case(Reason::UnstableTable, "unstable.", config("[unstable]\nx = 1\n", Build, &[]), config("[alias]\nb = \"build\"\n", Build, &[])),
        case(Reason::TargetRunner, "runner", config("[target.aarch64-apple-darwin]\nrunner = \"r\"\n", Test, &[]), config("[target.aarch64-apple-darwin]\nrunner = \"r\"\n", Build, &[])),
        case(Reason::TargetLinker, "linker", config("[target.aarch64-apple-darwin]\nlinker = \"l\"\n", Build, &[]), config("[target.aarch64-apple-darwin]\nlinker = \"l\"\n", Check, &[])),
        case(Reason::TargetLinks, "target.aarch64-apple-darwin.foo", config("[target.aarch64-apple-darwin.foo]\nrustc-link-lib = [\"z\"]\n", Build, &[]), config("[target.x86_64-unknown-linux-gnu.foo]\nrustc-link-lib = [\"z\"]\n", Build, &[])),
        case(Reason::FeatureUnification, "feature-unification", config("[resolver]\nfeature-unification = \"workspace\"\n", Build, &[]), config("[resolver]\nfeature-unification = \"selected\"\n", Build, &[])),
        case(Reason::ConfigPatch, "patch.", config("[patch.crates-io]\nfoo = { path = \"p\" }\n", Build, &[]), config("[registries.x]\nindex = \"sparse+https://e.com/\"\n", Build, &[])),
        case(Reason::RustdocFlags, "rustdocflags", config("[build]\nrustdocflags = [\"--cfg\", \"d\"]\n", Test, &[]), config("[build]\nrustdocflags = [\"--cfg\", \"d\"]\n", Test, &["--lib"])),
        case(
            Reason::EnvRelativeOutside,
            "ELSEWHERE",
            Box::new(|| {
                let fixture = Fixture::new();
                let outside = tempfile::tempdir().unwrap();
                let file = outside.path().join(".cargo").join("x.toml");
                std::fs::create_dir_all(file.parent().unwrap()).unwrap();
                std::fs::write(&file, "[env]\nELSEWHERE = { value = \"x\", relative = true }\n").unwrap();
                spec(&fixture, &fixture.env(), Build, &["--config", file.to_str().unwrap()], None).map(drop)
            }),
            Box::new(|| {
                let fixture = Fixture::with_config("[env]\nELSEWHERE = { value = \"x\", relative = true }\n");
                spec(&fixture, &fixture.env(), Build, &[], None).map(drop)
            }),
        ),
        // The environment.
        case(Reason::EnvRustc, "RUSTC", env(&[("RUSTC", "/r")], Build), env(&[("RUSTFLAGS", "")], Build)),
        case(Reason::EnvRustcWrapper, "RUSTC_WRAPPER", env(&[("RUSTC_WRAPPER", "sccache")], Build), env(&[("RUSTC_WRAPPER", "")], Build)),
        case(Reason::EnvRustdoc, "RUSTDOC", env(&[("RUSTDOC", "/r")], Build), env(&[("CARGO_INCREMENTAL", "1")], Build)),
        case(Reason::EnvToolchain, "RUSTUP_TOOLCHAIN", env(&[("RUSTUP_TOOLCHAIN", "nightly")], Build), env(&[("CARGO_TARGET_DIR", "t")], Build)),
        case(Reason::EnvBootstrap, "RUSTC_BOOTSTRAP", env(&[("RUSTC_BOOTSTRAP", "1")], Build), env(&[("RUST_BACKTRACE", "1")], Build)),
        case(Reason::EnvVersionString, "RUSTC_OVERRIDE_VERSION_STRING", env(&[("RUSTC_OVERRIDE_VERSION_STRING", "x")], Build), env(&[("TERM", "x")], Build)),
        case(Reason::EnvUnstable, "CARGO_UNSTABLE_X", env(&[("CARGO_UNSTABLE_X", "1")], Build), env(&[("CARGO_TERM_COLOR", "never")], Build)),
        case(Reason::EnvRaJit, "RA_JIT_TIER", env(&[("RA_JIT_TIER", "off")], Build), env(&[("RA_JIT_HOME", "/h")], Build)),
        case(Reason::EnvRaJitValue, "RA_JIT_OPT=4", backend(true, &[("RA_JIT_OPT", "4")]), backend(true, &[("RA_JIT_OPT", "3")])),
        case(Reason::EnvLlvm, "RA_JIT_BACKEND=llvm", backend(false, &[("RA_JIT_BACKEND", "llvm")]), backend(true, &[("RA_JIT_BACKEND", "llvm")])),
        // The profile of a unit.
        case(Reason::OptLevelSize, "opt-level", manifest("[profile.dev]\nopt-level = \"s\"\n", "dev"), manifest("[profile.dev]\nopt-level = 3\n", "dev")),
        case(Reason::ProfileValue, "opt-level", profile_with(|it| it.opt_level = "9".to_owned()), profile_with(|it| it.opt_level = "2".to_owned())),
        case(Reason::Lto, "profile.release.lto = \"thin\"", manifest("[profile.release]\nlto = \"thin\"\n", "release"), manifest("[profile.release]\nlto = \"off\"\n", "release")),
        case(Reason::CodegenBackend, "codegen-backend", profile_with(|it| it.codegen_backend = Some("x".to_owned())), profile_with(|it| it.codegen_backend = None)),
        case(Reason::ProfileRustflags, "rustflags", profile_with(|it| it.rustflags = vec!["-Cx".to_owned()]), profile_with(|it| it.rustflags = Vec::new())),
        case(Reason::TrimPaths, "trim-paths", profile_with(|it| it.trim_paths = Some(TomlTrimPathsValue::Object.into())), profile_with(|it| it.trim_paths = None)),
        case(Reason::Rpath, "rpath", manifest("[profile.dev]\nrpath = true\n", "dev"), manifest("[profile.dev]\nrpath = false\n", "dev")),
        case(Reason::PanicImmediateAbort, "immediate-abort", profile_with(|it| it.panic = PanicStrategy::ImmediateAbort), profile_with(|it| it.panic = PanicStrategy::Abort)),
        // The flags of rustc.
        case(Reason::FlagLto, "-C lto", rustflags("-C lto=thin"), rustflags("-C lto=off")),
        case(Reason::FlagOptSize, "opt-level=s", rustflags("-C opt-level=s"), rustflags("-C opt-level=2")),
        case(Reason::FlagCpu, "target-cpu", rustflags("-C target-cpu=native"), rustflags("-C debuginfo=1")),
        case(Reason::FlagLink, "link-arg", rustflags("-C link-arg=-x"), rustflags("-C codegen-units=2")),
        case(Reason::FlagRemap, "--remap-path-prefix", rustflags("--remap-path-prefix=/a=/b"), rustflags("--color=never")),
        case(Reason::FlagUnstable, "-Z", rustflags("-Zmacro-backtrace"), rustflags("-Dwarnings")),
        case(Reason::FlagCodegen, "relocation-model", rustflags("-C relocation-model=static"), rustflags("-C strip=none")),
        case(Reason::FlagCargoOwns, "--target", rustflags("--target=x86_64-unknown-linux-gnu"), rustflags("--check-cfg=cfg(x)")),
        case(Reason::FlagUnknown, "--frobnicate", rustflags("--frobnicate"), rustflags("--cap-lints=allow")),
        case(Reason::FlagValue, "opt-level", rustflags("-C opt-level=9"), rustflags("-C opt-level=1")),
        case(Reason::FlagCfg, "--cfg", rustflags("--cfg a=b"), rustflags("--cfg a=\"b\"")),
        case(Reason::FlagInput, "main.rs", rustflags("main.rs"), rustflags("--cfg main")),
        // The unit graph and the directory.
        case(Reason::UnitGraphFailed, "the unit graph", units(lib, &[], Build, &[]), units(lib, &[0], Build, &[])),
        case(
            Reason::UnitOrder,
            "unit `a",
            units(|| vec![Make::new("a", TargetKind::Lib), Make::new("a", TargetKind::Lib)], &[0], Build, &[]),
            units(|| vec![Make::new("a", TargetKind::Lib), Make::new("a", TargetKind::Lib).host()], &[0], Build, &[]),
        ),
        case(
            Reason::UnitEdge,
            "unit `a",
            units(|| vec![Make::new("a", TargetKind::Lib).dep(1, "build_script_build"), Make::new("a", TargetKind::CustomBuild).mode(Mode::RunCustomBuild).host()], &[0], Build, &[]),
            units(
                || vec![
                    Make::new("a", TargetKind::Lib).dep(1, "build_script_build"),
                    Make::new("a", TargetKind::CustomBuild).mode(Mode::RunCustomBuild).host().dep(2, "build_script_build"),
                    Make::new("a", TargetKind::CustomBuild).host(),
                ],
                &[0],
                Build,
                &[],
            ),
        ),
        case(Reason::RunTarget, "cargo run", units(lib, &[0], Run, &[]), units(|| vec![Make::new("p", TargetKind::Bin)], &[0], Run, &[])),
        case(Reason::DocLibrary, "--doc", units(|| vec![Make::new("a", TargetKind::Lib).mode(Mode::Test)], &[0], Test, &["--doc"]), units(lib, &[0], Test, &["--doc"])),
        case(
            Reason::Cwd,
            "",
            Box::new(|| {
                let fixture = Fixture::new();
                let elsewhere = tempfile::tempdir().unwrap();
                let cwd = elsewhere.path().canonicalize().unwrap();
                let mut env = fixture.env();
                env.insert("__CARGO_TEST_ROOT".to_owned(), cwd.display().to_string());
                let host = common::host(false);
                let context = common::context(&fixture, &env, &host, &cwd);
                let profile = profile_of(&fixture, &env, "dev", &Package::member(&fixture.root(), "a"), true, false);
                let manifest = fixture.root().join("Cargo.toml").display().to_string();
                let request = Request::parse(Build, &words(&["--manifest-path", &manifest])).unwrap();
                blaze_spec::assemble::assemble(&request, &context, &common::simple_graph(&profile)).map(drop)
            }),
            Box::new(|| {
                let fixture = Fixture::new();
                spec(&fixture, &fixture.env(), Build, &[], None).map(drop)
            }),
        ),
        // `Lto` is also what the profile of the command says; `lto = true` is spelled by itself.
    ]
}

#[test]
fn the_catalogue_is_well_formed() {
    assert_eq!(CATALOGUE.len(), Reason::ALL.len());
    let mut ids = std::collections::BTreeSet::new();
    for (entry, reason) in CATALOGUE.iter().zip(Reason::ALL) {
        assert!(ids.insert(entry.id), "{} twice", entry.id);
        assert_eq!(entry.id, format!("{reason:?}"));
        assert_eq!(reason.entry(), entry);
        assert!(!entry.what.is_empty() && !entry.why.is_empty(), "{}", entry.id);
        assert!(!entry.why.ends_with('.'), "{}: a reason is a clause", entry.id);
    }
    // Every area has entries.
    for area in [Area::Flag, Area::Config, Area::Env, Area::Profile, Area::Rustflag, Area::Unit] {
        assert!(CATALOGUE.iter().any(|it| it.area == area), "{area:?}");
    }
}

#[test]
fn every_entry_of_the_catalogue_fires_names_the_thing_and_has_a_neighbour_that_is_accepted() {
    let cases = cases();
    let mut covered = std::collections::BTreeSet::new();
    for case in &cases {
        let entry = case.reason.entry();
        assert!(covered.insert(entry.id), "{} has two cases", entry.id);
        let refusal = (case.bad)().expect_err(&format!("{}: the bad input is accepted", entry.id));
        assert_eq!(refusal.id, entry.id, "{}: refused as {} ({refusal})", entry.id, refusal.id);
        assert!(refusal.what.contains(case.names), "{}: `{}` does not name `{}`", entry.id, refusal.what, case.names);
        assert!(refusal.why.starts_with(entry.why), "{}: {}", entry.id, refusal.why);
        assert!(refusal.entry() == Some(entry));
        assert!(!refusal.to_string().is_empty());
        (case.good)().unwrap_or_else(|e| panic!("{}: the neighbour is refused: {e}", entry.id));
    }
    for entry in CATALOGUE {
        assert!(covered.contains(entry.id), "{} has no case", entry.id);
    }
}

#[test]
fn an_lto_true_is_named_as_the_profile_has_it() {
    let error = profile_with(|it| it.lto = Lto::Bool(true))().unwrap_err();
    assert_eq!(error.what, "profile.dev.lto = true");
}
