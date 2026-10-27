//! The spec assembled from a unit graph: the canonical order, what a unit is, the run, the
//! determinism of the bytes, what each honoured input changes and what an ignored one does not,
//! and the keys.

mod common;

use std::collections::BTreeMap;

use blaze_manifest::profiles::Profile;
use blaze_metadata::units::{Mode, TargetKind, UnitGraph};
use blaze_spec::Refusal;
use blaze_spec::assemble::{Spec, canonical_key};
use blaze_spec::flags::Subcommand;
use blaze_wire::{Command, Namespace, UnitSpec, digest_of, encode};
use sha2::{Digest as _, Sha256};

use common::{Fixture, Make, Package, graph, profile_of, simple_graph, spec, spec_on};

const REGISTRY: &str = "registry+https://github.com/rust-lang/crates.io-index";

/// A library `lib_a` of the workspace, a dependency `serde`, and a binary `app` that uses both.
fn units() -> Vec<Make> {
    vec![
        Make::new("app", TargetKind::Bin).dep(1, "lib_a").dep(2, "serde"),
        Make::new("lib_a", TargetKind::Lib).dep(2, "serde").features(&["x"]),
        Make::new("serde", TargetKind::Lib).registry().features(&["std", "derive"]),
    ]
}

/// `units` in another order, whose dependencies and roots still name the same units: the unit
/// at position `at` of the new order is `order[at]` of the old.
fn permuted(units: &[Make], roots: &[usize], order: &[usize]) -> (Vec<Make>, Vec<usize>) {
    let new_index = |old: usize| order.iter().position(|it| *it == old).unwrap();
    let moved = order
        .iter()
        .map(|&old| {
            let mut unit = units[old].clone();
            unit.deps =
                unit.deps.iter().map(|(index, name)| (new_index(*index), name.clone())).collect();
            unit
        })
        .collect();
    (moved, roots.iter().map(|it| new_index(*it)).collect())
}

fn graph_of(units: Vec<Make>, roots: Vec<usize>) -> impl Fn(&Profile) -> UnitGraph {
    move |profile| graph(units.clone(), &roots, profile)
}

fn index_of(spec: &Spec, name: &str) -> u32 {
    let found: Vec<usize> =
        (0..spec.build.units.len()).filter(|&at| spec.build.units[at].package.0 == name).collect();
    assert_eq!(found.len(), 1, "{name}");
    found[0] as u32
}

fn build(
    fixture: &Fixture,
    command: Subcommand,
    args: &[&str],
    units: Vec<Make>,
    roots: Vec<usize>,
) -> Result<Spec, Refusal> {
    spec(fixture, &fixture.env(), command, args, Some(&graph_of(units, roots)))
}

fn app(command: Subcommand, args: &[&str]) -> Spec {
    // This component helper supplies an already-selected graph. Doctest requests
    // select its library rather than its binary; real selection is tested in build_spec.rs.
    let roots = if args.contains(&"--doc") { vec![1] } else { vec![0] };
    build(&Fixture::new(), command, args, units(), roots)
        .unwrap_or_else(|e| panic!("{command:?} {args:?}: {e}"))
}

#[test]
fn dependencies_and_asked_units_are_indexes_in_the_canonical_order() {
    let spec = app(Subcommand::Build, &[]);
    let units = &spec.build.units;
    assert_eq!(units.len(), 3);
    let (app, lib_a, serde) =
        (index_of(&spec, "app"), index_of(&spec, "lib_a"), index_of(&spec, "serde"));
    let deps =
        |at: u32| -> Vec<(u32, String)> { units[at as usize].deps.iter().cloned().collect() };
    let mut want = vec![(lib_a, "lib_a".to_owned()), (serde, "serde".to_owned())];
    want.sort_by_key(|it| encode(it));
    assert_eq!(deps(app), want);
    assert_eq!(deps(lib_a), [(serde, "serde".to_owned())]);
    assert!(deps(serde).is_empty());
    assert_eq!(spec.build.asked.as_slice(), [app]);
    // The units are in the order of their keys, which are all different.
    let keys: Vec<Vec<u8>> = units.iter().map(canonical_key).collect();
    assert!(keys.windows(2).all(|pair| pair[0] < pair[1]), "not strictly ascending");
}

#[test]
fn the_canonical_order_does_not_depend_on_the_order_of_the_graph() {
    let fixture = Fixture::new();
    let reference = build(&fixture, Subcommand::Build, &[], units(), vec![0]).unwrap();
    for order in [[0, 1, 2], [2, 1, 0], [1, 2, 0], [2, 0, 1], [1, 0, 2], [0, 2, 1]] {
        let (moved, roots) = permuted(&units(), &[0], &order);
        let spec = build(&fixture, Subcommand::Build, &[], moved, roots).unwrap();
        assert_eq!(encode(&spec.build), encode(&reference.build), "{order:?}");
        assert_eq!(spec.digest, reference.digest);
    }
}

#[test]
fn two_units_alike_in_everything_but_their_dependencies_are_refused_naming_both() {
    let alike = vec![
        Make::new("a", TargetKind::Lib),
        Make::new("a", TargetKind::Lib),
        Make::new("app", TargetKind::Bin).dep(0, "a"),
    ];
    let error = build(&Fixture::new(), Subcommand::Build, &[], alike, vec![2]).unwrap_err();
    assert_eq!(error.id, "UnitOrder");
    assert_eq!(error.what, "unit `a 0.1.0` (a) and unit `a 0.1.0` (a)");
    // A difference in any one field makes the order total.
    let features =
        vec![Make::new("a", TargetKind::Lib), Make::new("a", TargetKind::Lib).features(&["x"])];
    assert!(build(&Fixture::new(), Subcommand::Build, &[], features, vec![0]).is_ok());
    let host = vec![Make::new("a", TargetKind::Lib), Make::new("a", TargetKind::Lib).host()];
    assert!(build(&Fixture::new(), Subcommand::Build, &[], host, vec![0]).is_ok());
    let mode =
        vec![Make::new("a", TargetKind::Lib), Make::new("a", TargetKind::Lib).mode(Mode::Test)];
    assert!(build(&Fixture::new(), Subcommand::Build, &[], mode, vec![0]).is_ok());
}

#[test]
fn a_run_of_a_build_script_is_the_unit_that_compiles_it() {
    let fixture = Fixture::new();
    let units = vec![
        Make::new("a", TargetKind::Lib).dep(1, "build_script_build"),
        Make::new("a", TargetKind::CustomBuild)
            .mode(Mode::RunCustomBuild)
            .host()
            .dep(2, "build_script_build"),
        Make::new("a", TargetKind::CustomBuild).host(),
    ];
    let spec = build(&fixture, Subcommand::Build, &[], units, vec![0]).unwrap();
    assert_eq!(spec.build.units.len(), 2, "the run is no unit");
    let script = spec.build.units.iter().position(|it| it.target.0 == 5).unwrap() as u32;
    let lib = spec.build.units.iter().position(|it| it.target.0 == 0).unwrap();
    assert_eq!(spec.build.units[lib].deps.as_slice(), [(script, "build_script_build".to_owned())]);
    assert!(spec.build.units[script as usize].host);
    assert_eq!(spec.build.asked.as_slice(), [lib as u32]);
    // A run with no compile of the script has nothing to stand for it.
    let broken = vec![
        Make::new("a", TargetKind::Lib).dep(1, "build_script_build"),
        Make::new("a", TargetKind::CustomBuild).mode(Mode::RunCustomBuild).host(),
    ];
    let error = build(&fixture, Subcommand::Build, &[], broken, vec![0]).unwrap_err();
    assert_eq!(error.id, "UnitEdge");
}

#[test]
fn a_unit_is_what_the_graph_says_it_is() {
    let fixture = Fixture::new();
    let units = vec![
        Make::new("lib", TargetKind::Lib).features(&["y", "x"]),
        Make::new("bin", TargetKind::Bin),
        Make::new("itest", TargetKind::Test).mode(Mode::Test),
        Make::new("custom", TargetKind::Test).no_harness(),
        Make::new("custom_test_mode", TargetKind::Test).mode(Mode::Test).no_harness(),
        Make::new("example", TargetKind::Example),
        Make::new("bench", TargetKind::Bench).mode(Mode::Check),
        Make::new("script", TargetKind::CustomBuild).host(),
        Make::new("macro", TargetKind::ProcMacro).host(),
        Make::new("dep", TargetKind::Lib).registry(),
    ];
    let spec = build(&fixture, Subcommand::Test, &[], units, vec![0, 1, 2]).unwrap();
    let unit = |name: &str| -> &UnitSpec { &spec.build.units[index_of(&spec, name) as usize] };
    for (name, kind) in [
        ("lib", 0),
        ("bin", 1),
        ("itest", 2),
        ("custom", 2),
        ("example", 3),
        ("bench", 4),
        ("script", 5),
        ("macro", 6),
        ("dep", 0),
    ] {
        assert_eq!(unit(name).target, (kind, name.to_owned()), "{name}");
    }
    let has = |name: &str, cfg: &str| unit(name).cfg.iter().any(|it| it == cfg);
    // Features, as a set, and as cfgs.
    assert_eq!(unit("lib").features.as_slice(), ["x", "y"]);
    assert!(
        has("lib", "feature=\"x\"") && has("lib", "feature=\"y\"") && !has("lib", "feature=\"z\"")
    );
    // A custom harness gets cfg(test) without asking rustc to generate libtest.
    for (name, test, test_cfg) in [
        ("lib", false, false),
        ("itest", true, true),
        ("custom", false, false),
        ("custom_test_mode", false, true),
        ("bench", true, true),
        ("bin", false, false),
    ] {
        assert_eq!((unit(name).test, has(name, "test")), (test, test_cfg), "{name}");
    }
    assert!(unit("script").host && unit("macro").host && !unit("lib").host);
    assert!(has("macro", "proc_macro") && !has("lib", "proc_macro"));
    // The profile of a test is `test`'s: debug assertions and unwinding.
    for name in ["lib", "itest", "dep"] {
        assert!(
            has(name, "debug_assertions")
                && has(name, "overflow_checks")
                && has(name, "panic=\"unwind\""),
            "{name}"
        );
    }
    assert_eq!(unit("lib").edition, 2021);
    assert_eq!(unit("dep").package.0, "dep");
    assert_eq!(unit("dep").package.1, "0.1.0");
    assert_eq!(unit("dep").package.2, <[u8; 32]>::from(Sha256::digest(REGISTRY.as_bytes())));
    assert_eq!(unit("lib").package.2, <[u8; 32]>::from(Sha256::digest(b"path+lib")));
    assert_eq!(spec.build.profile.name, "test");
}

#[test]
fn normal_and_test_checks_of_a_library_or_binary_remain_distinct() {
    let fixture = Fixture::new();
    for kind in [TargetKind::Lib, TargetKind::Bin, TargetKind::Example] {
        let checked = |profile: &Profile| {
            let mut graph = graph(
                vec![
                    Make::new("a", kind).mode(Mode::Check),
                    Make::new("a", kind).mode(Mode::Check),
                ],
                &[0, 1],
                profile,
            );
            graph.units[1].test = true;
            graph
        };
        let spec =
            spec(&fixture, &fixture.env(), Subcommand::Check, &["--all-targets"], Some(&checked))
                .unwrap();
        assert_eq!(spec.build.units.len(), 2, "{kind:?}");
        assert_eq!(spec.build.asked.len(), 2, "{kind:?}");
        for unit in &spec.build.units {
            assert_eq!(unit.test, unit.cfg.iter().any(|cfg| cfg == "test"));
        }
        assert_eq!(spec.build.units.iter().filter(|unit| unit.test).count(), 1);
    }
}

#[test]
fn custom_harness_checks_enable_test_cfg_without_libtest() {
    let fixture = Fixture::new();
    let units = vec![Make::new("a", TargetKind::Test).mode(Mode::Check).no_harness()];
    let spec = build(&fixture, Subcommand::Check, &["--tests"], units, vec![0]).unwrap();
    let unit = &spec.build.units[0];
    assert!(!unit.test);
    assert!(unit.cfg.iter().any(|cfg| cfg == "test"));
}

#[test]
fn the_cfgs_follow_the_profile_and_the_rustflags() {
    let fixture = Fixture::new();
    let cfgs = |args: &[&str], flags: &str, units: Vec<Make>| -> Vec<String> {
        let mut env = fixture.env();
        if !flags.is_empty() {
            env.insert("RUSTFLAGS".to_owned(), flags.to_owned());
        }
        let spec =
            spec(&fixture, &env, Subcommand::Build, args, Some(&graph_of(units, vec![0]))).unwrap();
        spec.build.units[index_of(&spec, "a") as usize].cfg.iter().cloned().collect()
    };
    let one = || vec![Make::new("a", TargetKind::Lib)];
    let dev = cfgs(&[], "", one());
    for cfg in ["debug_assertions", "overflow_checks", "panic=\"unwind\""] {
        assert!(dev.contains(&cfg.to_owned()), "{cfg} of {dev:?}");
    }
    let release = cfgs(&["--release"], "", one());
    assert_eq!(release, ["panic=\"unwind\""]);
    // A flag that sets a cfg is in every unit's; one that moves a check moves the cfg.
    let flagged = cfgs(
        &["--release"],
        "--cfg foo --cfg bar=\"xy\" -C debug-assertions=yes -C panic=abort",
        one(),
    );
    let mut want = vec![
        "foo".to_owned(),
        "bar=\"xy\"".to_owned(),
        "debug_assertions".to_owned(),
        "overflow_checks".to_owned(),
        "panic=\"abort\"".to_owned(),
    ];
    want.sort_by_key(|it| encode(it));
    assert_eq!(flagged, want);
    // Host units and dependencies get them too: no target is asked for.
    let both = vec![Make::new("a", TargetKind::Lib), Make::new("b", TargetKind::Lib).host()];
    let spec = spec_with_flags(&fixture, "--cfg everywhere", both);
    assert!(spec.build.units.iter().all(|it| it.cfg.iter().any(|c| c == "everywhere")));
}

fn spec_with_flags(fixture: &Fixture, flags: &str, units: Vec<Make>) -> Spec {
    let mut env = fixture.env();
    env.insert("RUSTFLAGS".to_owned(), flags.to_owned());
    spec(fixture, &env, Subcommand::Build, &[], Some(&graph_of(units, vec![0]))).unwrap()
}

#[test]
fn the_profile_of_the_command_is_the_profile_without_an_override_and_each_unit_has_its_own() {
    let fixture = Fixture::with_manifest(
        "[profile.dev.package.lib_a]\nopt-level = 1\n\n[profile.dev.build-override]\nopt-level = 3\n",
    );
    let env = fixture.env();
    let root = fixture.root();
    let profile = |name: &str, member: bool, host: bool| {
        profile_of(&fixture, &env, "dev", &Package::member(&root, name), member, host)
    };
    let units = vec![
        Make::new("app", TargetKind::Bin).dep(1, "lib_a").profile(profile("app", true, false)),
        Make::new("lib_a", TargetKind::Lib).profile(profile("lib_a", true, false)),
        Make::new("script", TargetKind::CustomBuild).host().profile(profile("script", true, true)),
    ];
    let spec = build(&fixture, Subcommand::Build, &[], units, vec![0]).unwrap();
    let of = |name: &str| spec.build.units[index_of(&spec, name) as usize].profile.clone();
    assert_eq!((of("app").opt_level, of("lib_a").opt_level, of("script").opt_level), (0, 1, 3));
    assert_eq!(spec.build.profile.opt_level, 0);
    assert_eq!(spec.build.profile.name, "dev");
    // The package override is in the keys of what is asked, and so is its absence.
    let plain = build(
        &Fixture::new(),
        Subcommand::Build,
        &[],
        vec![Make::new("lib_a", TargetKind::Lib)],
        vec![0],
    )
    .unwrap();
    let overridden = build(
        &fixture,
        Subcommand::Build,
        &[],
        vec![Make::new("lib_a", TargetKind::Lib).profile(profile("lib_a", true, false))],
        vec![0],
    )
    .unwrap();
    assert_ne!(plain.digest, overridden.digest);
    assert_ne!(plain.config, overridden.config);
    assert_ne!(plain.front_end, overridden.front_end);
}

#[test]
fn blazes_backend_and_mir_level_are_carried_on_every_unit_and_none_is_refused() {
    let fixture = Fixture::new();
    let mut env = fixture.env();
    env.insert("RA_JIT_BACKEND".to_owned(), "llvm".to_owned());
    env.insert("RA_JIT_OPT".to_owned(), "3".to_owned());
    env.insert("RA_JIT_EMIT".to_owned(), "fast".to_owned());
    env.insert("RA_JIT_MIR_OPT".to_owned(), "2".to_owned());
    let spec = spec_on(
        &common::host(true),
        &fixture,
        &env,
        Subcommand::Build,
        &[],
        Some(&graph_of(units(), vec![0])),
    )
    .unwrap();
    let carried = |profile: &blaze_wire::ProfileSpec| {
        (profile.backend, profile.backend_opt, profile.backend_emit, profile.mir_opt)
    };
    assert_eq!(carried(&spec.build.profile), (1, 3, 1, 2));
    assert!(spec.build.units.iter().all(|it| carried(&it.profile) == (1, 3, 1, 2)));
    let plain = app(Subcommand::Build, &[]);
    assert_eq!(carried(&plain.build.profile), (0, 0, 0, 0));
    assert_ne!(spec.digest, plain.digest);
    assert_ne!(spec.front_end, plain.front_end);
    assert_ne!(spec.config, plain.config);
    // Without the llvm backend the same environment is refused, and a malformed value is.
    let error =
        spec_on(&common::host(false), &fixture, &env, Subcommand::Build, &[], None).unwrap_err();
    assert_eq!(error.id, "EnvLlvm");
}

#[test]
fn the_same_inputs_make_the_same_bytes_wherever_they_are() {
    let first = app(Subcommand::Test, &["--no-fail-fast", "x", "--", "--exact"]);
    let again = app(Subcommand::Test, &["--no-fail-fast", "x", "--", "--exact"]);
    assert_eq!(first, again);
    assert_eq!(encode(&first.build), encode(&again.build));
    assert_eq!(first.digest, digest_of(&first.build));
    assert_eq!(first.platform_digest, digest_of(&first.platform));
    assert_eq!(first.build.platform, first.platform_digest);
    // Another checkout of the same workspace is the same spec: no path of the machine is in it.
    let elsewhere = build(
        &Fixture::new(),
        Subcommand::Test,
        &["--no-fail-fast", "x", "--", "--exact"],
        units(),
        vec![0],
    )
    .unwrap();
    assert_eq!(elsewhere, first);
    assert_eq!(first.job_content(Namespace::Main), again.job_content(Namespace::Main));
}

/// What a spec is told apart by.
fn mark(spec: &Spec) -> ([u8; 32], blaze_wire::FrontEndKey, blaze_wire::ConfigKey) {
    (spec.digest, spec.front_end, spec.config)
}

#[test]
fn each_honoured_input_changes_the_spec_and_each_ignored_one_does_not() {
    let test = |args: &[&str]| app(Subcommand::Test, args);
    let base = test(&[]);
    let honoured: &[(&str, Spec)] = &[
        ("check", app(Subcommand::Check, &[])),
        ("build", app(Subcommand::Build, &[])),
        ("--release", test(&["--release"])),
        ("--profile bench", test(&["--profile", "bench"])),
        ("--no-run", test(&["--no-run"])),
        ("--no-fail-fast", test(&["--no-fail-fast"])),
        ("TESTNAME", test(&["name"])),
        ("--lib", test(&["--lib"])),
        ("--doc", test(&["--doc"])),
        ("-- --nocapture", test(&["--", "--nocapture"])),
        ("-- --exact", test(&["--", "--exact"])),
        ("-- --skip a", test(&["--", "--skip", "a"])),
        ("-- --skip b", test(&["--", "--skip", "b"])),
        ("-- --ignored", test(&["--", "--ignored"])),
        ("-- --include-ignored", test(&["--", "--include-ignored"])),
        ("-- --test-threads 2", test(&["--", "--test-threads", "2"])),
        ("-- --test-threads 3", test(&["--", "--test-threads", "3"])),
    ];
    let mut seen = vec![mark(&base)];
    for (label, spec) in honoured {
        let marked = mark(spec);
        assert!(
            !seen.iter().any(|it| it.0 == marked.0),
            "{label} is the spec of an input already seen"
        );
        seen.push(marked);
    }
    let ignored: &[&[&str]] = &[
        &["-j4"],
        &["-v"],
        &["-vv"],
        &["-q"],
        &["--color", "never"],
        &["--message-format", "short"],
        &["--timings"],
        &["--future-incompat-report"],
        &["--ignore-rust-version"],
        &["--target-dir", "t"],
        &["--offline"],
        &["--locked"],
        &["--frozen"],
    ];
    for args in ignored {
        assert_eq!(test(args), base, "{args:?}");
    }
    // `run` of the binary is its own spec, with the program's arguments in it.
    let run = app(Subcommand::Run, &[]);
    assert!(!seen.iter().any(|it| it.0 == run.digest));
    assert_ne!(app(Subcommand::Run, &["x"]).digest, run.digest);
    assert_ne!(app(Subcommand::Run, &["--", "x"]).digest, run.digest);
    assert_eq!(
        app(Subcommand::Run, &["x"]).build.command,
        app(Subcommand::Run, &["--", "x"]).build.command
    );
}

/// The spec of a build in an environment with `variables` over a requester's own.
fn with_env(variables: &[(&str, &str)], config: &str) -> Result<Spec, Refusal> {
    let fixture = if config.is_empty() { Fixture::new() } else { Fixture::with_config(config) };
    let mut env = fixture.env();
    env.extend(variables.iter().map(|(k, v)| ((*k).to_owned(), (*v).to_owned())));
    spec(&fixture, &env, Subcommand::Build, &[], Some(&graph_of(units(), vec![0])))
}

#[test]
fn the_environment_changes_the_spec_where_a_build_reads_it_and_not_where_it_does_not() {
    let base = with_env(&[], "").unwrap();
    for (label, variables, config) in [
        ("RUSTFLAGS cfg", &[("RUSTFLAGS", "--cfg a")][..], ""),
        ("another cfg", &[("RUSTFLAGS", "--cfg b")], ""),
        ("a cfg with a value", &[("RUSTFLAGS", "--cfg a=\"b\"")], ""),
        ("another value", &[("RUSTFLAGS", "--cfg a=\"c\"")], ""),
        ("encoded", &[("CARGO_ENCODED_RUSTFLAGS", "--cfg\u{1f}a")], ""),
        ("opt-level", &[("RUSTFLAGS", "-C opt-level=1")], ""),
        ("frame pointers", &[("RUSTFLAGS", "-C force-frame-pointers=yes")], ""),
        ("cap-lints", &[("RUSTFLAGS", "--cap-lints=warn")], ""),
        ("build.rustflags", &[], "[build]\nrustflags = [\"--cfg\", \"x\"]\n"),
        ("config env", &[], "[env]\nX = \"1\"\n"),
        ("another config env", &[], "[env]\nX = \"2\"\n"),
        ("C compiler", &[("CC", "clang")], ""),
        ("another C compiler", &[("CC", "gcc")], ""),
        ("deployment target", &[("MACOSX_DEPLOYMENT_TARGET", "11.0")], ""),
        ("pkg-config", &[("PKG_CONFIG_PATH", "/x")], ""),
        ("MIR level", &[("RA_JIT_MIR_OPT", "1")], ""),
    ] {
        let spec = with_env(variables, config).unwrap_or_else(|e| panic!("{label}: {e}"));
        assert_ne!(spec.digest, base.digest, "{label}");
    }
    let mut digests = std::collections::BTreeSet::new();
    for flags in ["--cfg a", "--cfg b", "--cfg a --cfg b", "--cfg a=\"b\"", "--cfg a=\"c\""] {
        assert!(digests.insert(with_env(&[("RUSTFLAGS", flags)], "").unwrap().digest), "{flags}");
    }
    for (label, variables, config) in [
        ("CARGO_INCREMENTAL", &[("CARGO_INCREMENTAL", "0")][..], ""),
        ("CARGO_TARGET_DIR", &[("CARGO_TARGET_DIR", "/t")], ""),
        ("backtrace", &[("RUST_BACKTRACE", "1")], ""),
        (
            "blaze's home",
            &[("RA_JIT_HOME", "/h"), ("RA_JIT_CACHE", "/c"), ("RA_JIT_STATS", "1")],
            "",
        ),
        ("lints", &[("RUSTFLAGS", "-D warnings -A unused --check-cfg cfg(x)")], ""),
        ("codegen units", &[("RUSTFLAGS", "-C codegen-units=1 -C incremental=/x")], ""),
        ("a variable the scripts do not read", &[("OPENSSL_DIR", "/o")], ""),
        ("jobs", &[], "[build]\njobs = 2\n"),
        ("term", &[], "[term]\ncolor = \"never\"\n"),
        ("alias", &[], "[alias]\nb = \"build\"\n"),
    ] {
        assert_eq!(with_env(variables, config).unwrap(), base, "{label}");
    }
}

#[test]
fn the_script_environment_is_what_a_build_script_may_read_and_the_config_env_is_apart() {
    let fixture = Fixture::with_config("[env]\nFROM_CONFIG = \"1\"\n");
    let mut env = fixture.env();
    env.insert("CC".to_owned(), "clang".to_owned());
    env.insert("CFLAGS_aarch64_apple_darwin".to_owned(), "-g".to_owned());
    env.insert("HOME".to_owned(), "/home/x".to_owned());
    let spec = spec(&fixture, &env, Subcommand::Build, &[], None).unwrap();
    let script: Vec<(String, String)> = spec.build.script_env.iter().cloned().collect();
    let mut want = vec![
        ("CC".to_owned(), "clang".to_owned()),
        ("CFLAGS_aarch64_apple_darwin".to_owned(), "-g".to_owned()),
    ];
    want.sort_by_key(|it| encode(it));
    assert_eq!(script, want);
    assert_eq!(spec.build.config_env.as_slice(), [("FROM_CONFIG".to_owned(), "1".to_owned())]);
}

#[test]
fn the_run_is_read_out_of_what_cargo_test_is_given() {
    let spec = app(
        Subcommand::Test,
        &[
            "--no-run",
            "--no-fail-fast",
            "filter",
            "--",
            "--exact",
            "--skip",
            "a",
            "--skip=b",
            "--ignored",
            "--test-threads=2",
            "--nocapture",
            "other",
        ],
    );
    let Command::Test(run) = &spec.build.command else { panic!("a test") };
    assert_eq!(run.filters, ["filter"]);
    assert!(run.exact && run.no_run && run.no_fail_fast);
    assert_eq!(
        (&*run.skip, run.ignored, run.test_threads),
        (&["a".to_owned(), "b".to_owned()][..], 1, Some(2))
    );
    // Every word after `--`, as typed, so that a custom harness gets exactly these.
    assert_eq!(
        run.harness_args,
        [
            "--exact",
            "--skip",
            "a",
            "--skip=b",
            "--ignored",
            "--test-threads=2",
            "--nocapture",
            "other"
        ]
    );
    assert!(run.env.is_empty());
    assert_eq!(run.doc, 1);

    let included = app(Subcommand::Test, &["--", "--include-ignored"]);
    let Command::Test(run) = &included.build.command else { panic!("a test") };
    assert_eq!((run.ignored, run.exact, run.test_threads, run.skip.len()), (2, false, None, 0));
    let doc = |args: &[&str]| match app(Subcommand::Test, args).build.command {
        Command::Test(run) => run.doc,
        other => panic!("{other:?}"),
    };
    assert_eq!(
        (doc(&[]), doc(&["--doc"]), doc(&["--lib"]), doc(&["--tests"]), doc(&["--no-run"])),
        (1, 2, 0, 0, 1)
    );

    // `run` has the program's arguments, positional first, and reads none of them.
    let Command::Run(run) = app(Subcommand::Run, &["--bin", "app", "one", "two"]).build.command
    else {
        panic!("a run")
    };
    assert_eq!(run.harness_args, ["one", "two"]);
    let Command::Run(run) = app(Subcommand::Run, &["--", "--exact", "x"]).build.command else {
        panic!("a run")
    };
    assert_eq!(
        (&*run.harness_args, run.exact, run.doc, run.no_run),
        (&["--exact".to_owned(), "x".to_owned()][..], false, 0, false)
    );
    assert!(matches!(app(Subcommand::Check, &[]).build.command, Command::Check));
    assert!(matches!(app(Subcommand::Build, &[]).build.command, Command::Build));
}

#[test]
fn cargo_run_needs_one_program() {
    let fixture = Fixture::new();
    let two = vec![Make::new("one", TargetKind::Bin), Make::new("two", TargetKind::Bin)];
    let error = build(&fixture, Subcommand::Run, &[], two.clone(), vec![0, 1]).unwrap_err();
    assert_eq!(error.id, "RunTarget");
    assert!(build(&fixture, Subcommand::Build, &[], two, vec![0, 1]).is_ok());
    let none = vec![Make::new("lib", TargetKind::Lib)];
    assert_eq!(build(&fixture, Subcommand::Run, &[], none, vec![0]).unwrap_err().id, "RunTarget");
    // The program among a library and an example is the one asked.
    let mixed =
        vec![Make::new("lib", TargetKind::Lib), Make::new("prog", TargetKind::Bin).dep(0, "lib")];
    let spec = build(&fixture, Subcommand::Run, &[], mixed, vec![0, 1]).unwrap();
    assert_eq!(spec.build.asked.as_slice(), [index_of(&spec, "prog")]);
}

#[test]
fn the_doc_tests_are_of_the_libraries_of_the_members() {
    let fixture = Fixture::new();
    let units = vec![
        Make::new("a", TargetKind::Lib).mode(Mode::Test),
        Make::new("a", TargetKind::Lib),
        Make::new("b", TargetKind::Lib),
        Make::new("dep", TargetKind::Lib).registry(),
        Make::new("macro", TargetKind::ProcMacro).host(),
    ];
    let spec = build(&fixture, Subcommand::Test, &["--doc"], units.clone(), vec![1, 2]).unwrap();
    let asked: Vec<&str> = spec
        .build
        .asked
        .iter()
        .map(|&it| spec.build.units[it as usize].package.0.as_str())
        .collect();
    assert_eq!(asked, ["a", "b"]);
    assert!(spec.build.asked.iter().all(|&it| !spec.build.units[it as usize].test));
    let only_unit_tests = vec![Make::new("a", TargetKind::Lib).mode(Mode::Test)];
    let error =
        build(&fixture, Subcommand::Test, &["--doc"], only_unit_tests, vec![0]).unwrap_err();
    assert_eq!(error.id, "DocLibrary");
    // Without `--doc` the roots of the graph are what is asked.
    let spec = build(&fixture, Subcommand::Test, &[], units, vec![0]).unwrap();
    assert_eq!(spec.build.asked.len(), 1);
}

#[test]
fn the_keys_are_the_parts_of_the_spec_they_are_of() {
    let fixture = Fixture::new();
    let make = |lock: &[u8], tweak: &dyn Fn(&mut UnitGraph)| -> Spec {
        let mut units = graph_of(units(), vec![0]);
        let _ = &mut units;
        let env = fixture.env();
        let host = common::host(false);
        let root = fixture.root();
        let context = common::context(&fixture, &env, &host, &root);
        let profile = profile_of(&fixture, &env, "dev", &Package::member(&root, "a"), true, false);
        let mut graph = graph(units_for_keys(), &[0], &profile);
        graph.lock = lock.to_vec();
        tweak(&mut graph);
        let request = blaze_spec::flags::Request::parse(Subcommand::Build, &[]).unwrap();
        blaze_spec::assemble::assemble(&request, &context, &graph).unwrap()
    };
    let base = make(b"lock", &|_| {});
    // A lock file is in the spec and the front end's key, not in what retention groups by.
    let other_lock = make(b"another lock", &|_| {});
    assert_ne!(other_lock.digest, base.digest);
    assert_ne!(other_lock.front_end, base.front_end);
    assert_eq!(other_lock.config, base.config);
    // Which units are asked is in the config key, and a unit nobody asks for is not.
    let asked_lib = make(b"lock", &|graph| graph.roots = vec![1]);
    assert_ne!(asked_lib.config, base.config);
    assert_eq!(asked_lib.front_end, base.front_end);
    // The source and the namespace change what a job is, not what a front end or a retention is.
    let job = |spec: &Spec, ns: Namespace| spec.job_content(ns);
    assert_ne!(job(&base, Namespace::Main), job(&base, Namespace::Std));
    assert_ne!(
        job(&base, Namespace::Pr(blaze_wire::PrNamespace { n: 1 })),
        job(&base, Namespace::Pr(blaze_wire::PrNamespace { n: 2 }))
    );
    assert_ne!(job(&base, Namespace::Main), job(&other_lock, Namespace::Main));
}

/// A library and a binary of the workspace.
fn units_for_keys() -> Vec<Make> {
    vec![Make::new("app", TargetKind::Bin).dep(1, "lib_a"), Make::new("lib_a", TargetKind::Lib)]
}

#[test]
fn the_front_end_does_not_depend_on_the_command_but_the_config_does() {
    let build_spec = app(Subcommand::Build, &[]);
    let check = app(Subcommand::Check, &[]);
    assert_eq!(build_spec.front_end, check.front_end);
    assert_ne!(build_spec.config, check.config);
    assert_ne!(build_spec.digest, check.digest);
    // A cfg of the rustflags is in the front end's key; a flag that sets none is only in the
    // units' profiles it changes.
    let cfg = with_env(&[("RUSTFLAGS", "--cfg a")], "").unwrap();
    let base = with_env(&[], "").unwrap();
    assert_ne!(cfg.front_end, base.front_end);
    assert_ne!(cfg.config, base.config);
    let lint = with_env(&[("RUSTFLAGS", "--cap-lints=warn")], "").unwrap();
    assert_eq!(lint.front_end, base.front_end);
    assert_ne!(lint.config, base.config);
}

#[test]
fn a_spec_of_one_unit_has_its_platform_and_its_asked_unit() {
    let fixture = Fixture::new();
    let spec =
        build(&fixture, Subcommand::Check, &[], vec![Make::new("a", TargetKind::Lib)], vec![0])
            .unwrap();
    assert_eq!(spec.build.units.len(), 1);
    assert_eq!(spec.build.asked.as_slice(), [0]);
    assert_eq!(spec.build.cwd, ".");
    assert_eq!(spec.platform.triple, "aarch64-apple-darwin");
    assert_eq!(spec.build.lock, <[u8; 32]>::from(Sha256::digest(b"lock")));
    let _ = (BTreeMap::<u8, u8>::new(), simple_graph);
}
