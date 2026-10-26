//! A unit's profile as the spec's `ProfileSpec`: each setting mapped, the package overrides and
//! build-override carried, the flags over them, and blaze's own settings from the environment.

mod common;

use std::collections::BTreeMap;

use blaze_manifest::profiles::{DebugInfo, Lto, PanicStrategy, Profile};
use blaze_spec::profile::{self, IGNORED, Settings};
use blaze_spec::rustflags::Overrides;
use blaze_spec::Refusal;
use blaze_wire::ProfileSpec;
use cargo_util_schemas::manifest::{TomlDebugInfo, TomlTrimPathsValue};

use common::{Fixture, Package, profile_of};

fn cranelift() -> Settings {
    Settings { backend: 0, backend_opt: 0, backend_emit: 0, mir_opt: 0 }
}

/// The spec of `package`'s unit under the root manifest `extra` in the profile `requested`.
fn spec_with(
    extra: &str,
    env: &[(&str, &str)],
    requested: &str,
    package: &str,
    member: bool,
    for_host: bool,
    flags: &Overrides,
) -> Result<ProfileSpec, Refusal> {
    let fixture = Fixture::with_manifest(extra);
    let mut variables = fixture.env();
    variables.extend(env.iter().map(|(k, v)| ((*k).to_owned(), (*v).to_owned())));
    let package = if member {
        Package::member(&fixture.root(), package)
    } else {
        Package::registry(package)
    };
    let profile = profile_of(&fixture, &variables, requested, &package, member, for_host);
    profile::spec(&profile, flags, &cranelift(), &format!("unit `{}`", package.name))
}

fn spec_of(extra: &str, requested: &str) -> Result<ProfileSpec, Refusal> {
    spec_with(extra, &[], requested, "a", true, false, &Overrides::default())
}

#[test]
fn the_defaults_of_dev_and_release() {
    let dev = spec_of("", "dev").unwrap();
    assert_eq!(
        dev,
        ProfileSpec {
            name: "dev".to_owned(),
            opt_level: 0,
            debug: 2,
            debug_assertions: true,
            overflow_checks: true,
            panic: 0,
            strip: 0,
            // macOS keeps debug info in the objects, as cargo has it for the host triple.
            split_debuginfo: 2,
            backend: 0,
            mir_opt: 0,
            backend_opt: 0,
            backend_emit: 0,
        }
    );
    let release = spec_of("", "release").unwrap();
    assert_eq!(
        (release.name.as_str(), release.opt_level, release.debug, release.debug_assertions, release.overflow_checks, release.split_debuginfo),
        ("release", 3, 0, false, false, 0)
    );
    let test = spec_of("", "test").unwrap();
    assert_eq!((test.name.as_str(), test.opt_level, test.debug_assertions), ("test", 0, true));
    let bench = spec_of("", "bench").unwrap();
    assert_eq!((bench.name.as_str(), bench.opt_level), ("bench", 3));
}

#[test]
fn each_setting_is_mapped() {
    let with = |lines: &str| spec_of(&format!("[profile.dev]\n{lines}\n"), "dev").unwrap();
    for (value, level) in [
        ("0", 0), ("false", 0), ("\"none\"", 0), ("1", 1), ("\"limited\"", 1), ("\"line-tables-only\"", 1),
        ("\"line-directives-only\"", 1), ("2", 2), ("true", 2), ("\"full\"", 2),
    ] {
        assert_eq!(with(&format!("debug = {value}")).debug, level, "debug = {value}");
    }
    for level in 0..=3u8 {
        assert_eq!(with(&format!("opt-level = {level}")).opt_level, level);
    }
    assert!(!with("debug-assertions = false").debug_assertions);
    assert!(!with("overflow-checks = false").overflow_checks);
    assert!(with("debug-assertions = false").overflow_checks);
    assert_eq!(with("panic = \"abort\"").panic, 1);
    assert_eq!(with("panic = \"unwind\"").panic, 0);
    for (value, strip) in [("false", 0), ("\"none\"", 0), ("\"debuginfo\"", 1), ("true", 2), ("\"symbols\"", 2)] {
        assert_eq!(with(&format!("strip = {value}")).strip, strip, "strip = {value}");
    }
    for (value, split) in [("\"off\"", 0), ("\"packed\"", 1), ("\"unpacked\"", 2)] {
        assert_eq!(with(&format!("split-debuginfo = {value}")).split_debuginfo, split, "{value}");
    }
}

#[test]
fn what_blaze_ignores_changes_no_spec() {
    let plain = spec_of("", "dev").unwrap();
    for lines in ["codegen-units = 1", "codegen-units = 256", "incremental = false", "incremental = true"] {
        assert_eq!(spec_of(&format!("[profile.dev]\n{lines}\n"), "dev").unwrap(), plain, "{lines}");
    }
    let names: Vec<&str> = IGNORED.iter().map(|it| it.0).collect();
    assert!(names.contains(&"codegen-units") && names.contains(&"incremental"));
    assert!(IGNORED.iter().all(|it| !it.1.is_empty()));
    // The profile of a rpath that is off is the profile with none.
    assert_eq!(spec_of("[profile.dev]\nrpath = false\n", "dev").unwrap(), plain);
}

#[test]
fn a_package_override_and_the_build_override_are_carried_per_unit() {
    let manifest = "[profile.dev.package.a]\nopt-level = 1\n\n[profile.dev.package.\"*\"]\nopt-level = 2\ndebug = 0\n\n[profile.dev.build-override]\nopt-level = 3\n";
    let none = Overrides::default();
    let of = |package: &str, member: bool, host: bool| {
        spec_with(manifest, &[], "dev", package, member, host, &none).unwrap()
    };
    assert_eq!(of("a", true, false).opt_level, 1);
    // `b` is a member: `*` is for the packages that are not.
    assert_eq!(of("b", true, false).opt_level, 0);
    assert_eq!(of("dep", false, false).opt_level, 2);
    assert_eq!(of("dep", false, false).debug, 0);
    // The build override is for what runs on the host, below the packages' own overrides:
    // `*` is for a dependency built for the host too.
    assert_eq!(of("b", true, true).opt_level, 3);
    assert_eq!(of("dep", false, true).opt_level, 2);
    // A host unit does not unwind or abort as the profile says.
    let aborting = "[profile.dev]\npanic = \"abort\"\n";
    let host = spec_with(aborting, &[], "dev", "dep", false, true, &none).unwrap();
    let target = spec_with(aborting, &[], "dev", "dep", false, false, &none).unwrap();
    assert_eq!((host.panic, target.panic), (0, 1));
    // The override that is a size level is refused with the unit it is of.
    let error = spec_with("[profile.dev.package.a]\nopt-level = \"s\"\n", &[], "dev", "a", true, false, &none).unwrap_err();
    assert_eq!(error.what, "profile.dev.opt-level = \"s\"");
    assert!(error.why.contains("unit `a`"), "{error}");
    assert_eq!(error.id, "OptLevelSize");
}

#[test]
fn the_profile_of_the_environment_is_merged_by_the_configuration() {
    let env = [("CARGO_PROFILE_DEV_OPT_LEVEL", "2"), ("CARGO_PROFILE_DEV_DEBUG", "line-tables-only")];
    let spec = spec_with("", &env, "dev", "a", true, false, &Overrides::default()).unwrap();
    assert_eq!((spec.opt_level, spec.debug), (2, 1));
    let error = spec_with("", &[("CARGO_PROFILE_RELEASE_LTO", "thin")], "release", "a", true, false, &Overrides::default()).unwrap_err();
    assert_eq!(error.what, "profile.release.lto = \"thin\"");
}

#[test]
fn link_time_optimization_is_refused_as_the_profile_has_it() {
    let lto = |value: &str, requested: &str| {
        spec_of(&format!("[profile.release]\nlto = {value}\n\n[profile.dist]\ninherits = \"release\"\n"), requested)
    };
    for (value, what) in [
        ("\"thin\"", "profile.release.lto = \"thin\""),
        ("\"fat\"", "profile.release.lto = \"fat\""),
        ("true", "profile.release.lto = true"),
    ] {
        let error = lto(value, "release").unwrap_err();
        assert_eq!((error.id, error.what.as_str()), ("Lto", what), "lto = {value}");
        assert!(error.why.contains("unit `a`"), "{error}");
    }
    // An inherited setting is named by the profile asked for.
    let error = lto("\"thin\"", "dist").unwrap_err();
    assert_eq!(error.what, "profile.dist.lto = \"thin\"");
    for value in ["false", "\"off\""] {
        assert!(lto(value, "release").is_ok(), "lto = {value}");
    }
    assert!(spec_of("", "release").is_ok());
}

#[test]
fn what_blaze_has_no_value_for_is_refused_by_name() {
    let fixture = Fixture::new();
    let env = fixture.env();
    let root = fixture.root();
    let base = profile_of(&fixture, &env, "dev", &Package::member(&root, "a"), true, false);
    let refuse = |change: &dyn Fn(&mut Profile)| {
        let mut profile = base.clone();
        change(&mut profile);
        profile::spec(&profile, &Overrides::default(), &cranelift(), "unit `a`").unwrap_err()
    };
    let error = refuse(&|it| it.codegen_backend = Some("cranelift".to_owned()));
    assert_eq!((error.id, error.what.as_str()), ("CodegenBackend", "profile.dev.codegen-backend = \"cranelift\""));
    let error = refuse(&|it| it.rustflags = vec!["-Ccodegen-units=1".to_owned()]);
    assert_eq!((error.id, error.what.as_str()), ("ProfileRustflags", "profile.dev.rustflags = [\"-Ccodegen-units=1\"]"));
    let error = refuse(&|it| it.trim_paths = Some(TomlTrimPathsValue::Object.into()));
    assert_eq!(error.id, "TrimPaths");
    assert!(error.what.starts_with("profile.dev.trim-paths = "), "{error}");
    let error = refuse(&|it| it.rpath = true);
    assert_eq!((error.id, error.what.as_str()), ("Rpath", "profile.dev.rpath = true"));
    let error = refuse(&|it| it.panic = PanicStrategy::ImmediateAbort);
    assert_eq!((error.id, error.what.as_str()), ("PanicImmediateAbort", "profile.dev.panic = \"immediate-abort\""));
    let error = refuse(&|it| it.opt_level = "9".to_owned());
    assert_eq!((error.id, error.what.as_str()), ("ProfileValue", "profile.dev.opt-level = \"9\""));
    let error = refuse(&|it| it.split_debuginfo = Some("sideways".to_owned()));
    assert_eq!((error.id, error.what.as_str()), ("ProfileValue", "profile.dev.split-debuginfo = \"sideways\""));
    let error = refuse(&|it| it.lto = Lto::Named("thin".to_owned()));
    assert_eq!(error.id, "Lto");
    // The neighbours: a debug level the profile has deferred is none, and a resolved one is its own.
    let mut deferred = base.clone();
    deferred.debuginfo = DebugInfo::Deferred(TomlDebugInfo::Full);
    assert_eq!(profile::spec(&deferred, &Overrides::default(), &cranelift(), "x").unwrap().debug, 0);
}

#[test]
fn the_flags_win_over_the_profile_as_rustcs_last_flag_does() {
    let over = |extra: &str, requested: &str, flags: Overrides| {
        spec_with(extra, &[], requested, "a", true, false, &flags).unwrap()
    };
    let none = || Overrides::default();
    // A level among the flags moves the checks cargo left unnamed.
    let release = over("", "release", Overrides { opt_level: Some("0".to_owned()), ..none() });
    assert_eq!((release.opt_level, release.debug_assertions, release.overflow_checks), (0, true, true));
    let dev = over("", "dev", Overrides { opt_level: Some("3".to_owned()), ..none() });
    assert_eq!((dev.opt_level, dev.debug_assertions, dev.overflow_checks), (3, false, false));
    // A check cargo named is the profile's own, whatever the level the flags give.
    let named = over("[profile.dev]\ndebug-assertions = false\n", "dev", Overrides { opt_level: Some("3".to_owned()), ..none() });
    assert!(!named.debug_assertions);
    // The checks the flags give are last.
    let given = over("", "release", Overrides { debug_assertions: Some(true), overflow_checks: Some(false), ..none() });
    assert_eq!((given.debug_assertions, given.overflow_checks), (true, false));
    // cargo names no check of `dev`, so with debug assertions off the overflow checks follow.
    let given = over("", "dev", Overrides { debug_assertions: Some(false), ..none() });
    assert_eq!((given.debug_assertions, given.overflow_checks), (false, false));
    let abort = over("", "dev", Overrides { panic: Some("abort".to_owned()), ..none() });
    assert_eq!(abort.panic, 1);
    let unwind = over("[profile.dev]\npanic = \"abort\"\n", "dev", Overrides { panic: Some("unwind".to_owned()), ..none() });
    assert_eq!(unwind.panic, 0);
    let debug = over("", "release", Overrides { debug: Some(1), ..none() });
    assert_eq!(debug.debug, 1);
    let stripped = over("", "dev", Overrides { strip: Some("symbols".to_owned()), ..none() });
    assert_eq!(stripped.strip, 2);
    let split = over("", "dev", Overrides { split_debuginfo: Some("packed".to_owned()), ..none() });
    assert_eq!(split.split_debuginfo, 1);
    // Flags that name a size level are refused, as the profile's are.
    let error = spec_with("", &[], "dev", "a", true, false, &Overrides { opt_level: Some("s".to_owned()), ..none() }).unwrap_err();
    assert_eq!(error.id, "OptLevelSize");
}

fn settings(env: &[(&str, &str)], llvm: bool) -> Result<Settings, Refusal> {
    let env: BTreeMap<String, String> =
        env.iter().map(|(k, v)| ((*k).to_owned(), (*v).to_owned())).collect();
    Settings::of(&env, llvm)
}

fn shape(settings: Settings) -> (u8, u8, u8, u8) {
    (settings.backend, settings.backend_opt, settings.backend_emit, settings.mir_opt)
}

#[test]
fn blazes_backend_and_mir_level_are_read_as_blaze_reads_them() {
    assert_eq!(shape(settings(&[], false).unwrap()), (0, 0, 0, 0));
    assert_eq!(shape(settings(&[], true).unwrap()), (0, 0, 0, 0));
    // Under LLVM the passes default to 2 and the selection to optimized.
    assert_eq!(shape(settings(&[("RA_JIT_BACKEND", "llvm")], true).unwrap()), (1, 2, 0, 0));
    // With the passes off the selection is fast, whatever is asked for.
    assert_eq!(shape(settings(&[("RA_JIT_BACKEND", "llvm"), ("RA_JIT_OPT", "0")], true).unwrap()), (1, 0, 1, 0));
    assert_eq!(shape(settings(&[("RA_JIT_BACKEND", "llvm"), ("RA_JIT_OPT", "0"), ("RA_JIT_EMIT", "optimized")], true).unwrap()), (1, 0, 1, 0));
    assert_eq!(shape(settings(&[("RA_JIT_BACKEND", "llvm"), ("RA_JIT_OPT", "1")], true).unwrap()), (1, 1, 0, 0));
    assert_eq!(shape(settings(&[("RA_JIT_BACKEND", "llvm"), ("RA_JIT_OPT", "3"), ("RA_JIT_EMIT", "fast")], true).unwrap()), (1, 3, 1, 0));
    // Under Cranelift neither changes it.
    assert_eq!(shape(settings(&[("RA_JIT_OPT", "3"), ("RA_JIT_EMIT", "fast")], false).unwrap()), (0, 0, 0, 0));
    assert_eq!(shape(settings(&[("RA_JIT_BACKEND", "cranelift"), ("RA_JIT_OPT", "1")], true).unwrap()), (0, 0, 0, 0));
    for (asked, level) in [("0", 0), ("1", 1), ("2", 2)] {
        assert_eq!(settings(&[("RA_JIT_MIR_OPT", asked)], false).unwrap().mir_opt, level);
    }
}

#[test]
fn a_value_blaze_would_refuse_or_fall_back_from_is_refused_naming_it() {
    for (env, what) in [
        (&[("RA_JIT_OPT", "4")][..], "RA_JIT_OPT=4"),
        (&[("RA_JIT_OPT", "")], "RA_JIT_OPT="),
        (&[("RA_JIT_OPT", "02")], "RA_JIT_OPT=02"),
        (&[("RA_JIT_EMIT", "slow")], "RA_JIT_EMIT=slow"),
        (&[("RA_JIT_BACKEND", "gcc")], "RA_JIT_BACKEND=gcc"),
        (&[("RA_JIT_BACKEND", "LLVM")], "RA_JIT_BACKEND=LLVM"),
        (&[("RA_JIT_MIR_OPT", "3")], "RA_JIT_MIR_OPT=3"),
        (&[("RA_JIT_MIR_OPT", "")], "RA_JIT_MIR_OPT="),
        (&[("RA_JIT_MIR_OPT", "-1")], "RA_JIT_MIR_OPT=-1"),
        (&[("RA_JIT_MIR_OPT", "one")], "RA_JIT_MIR_OPT=one"),
    ] {
        let error = settings(env, true).unwrap_err();
        assert_eq!((error.id, error.what.as_str()), ("EnvRaJitValue", what), "{env:?}");
    }
    // Under Cranelift a value of the LLVM settings is still read, as the backend reads it first.
    assert!(settings(&[("RA_JIT_OPT", "9")], false).is_err());
    let error = settings(&[("RA_JIT_BACKEND", "llvm")], false).unwrap_err();
    assert_eq!((error.id, error.what.as_str()), ("EnvLlvm", "RA_JIT_BACKEND=llvm"));
    assert!(error.why.contains("llvm"), "{error}");
}
