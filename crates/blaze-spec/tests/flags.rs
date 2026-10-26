//! Cargo's flags read strictly: the table of them, both spellings of a value, the commands that
//! take each flag, what follows `--`, and the combinations cargo rejects.

use blaze_spec::Refusal;
use blaze_spec::flags::{Class, FLAGS, Request, Subcommand, Takes};

const COMMANDS: [Subcommand; 4] =
    [Subcommand::Check, Subcommand::Build, Subcommand::Run, Subcommand::Test];

fn words(args: &[&str]) -> Vec<String> {
    args.iter().map(|it| (*it).to_owned()).collect()
}

fn parse(command: Subcommand, args: &[&str]) -> Result<Request, Refusal> {
    Request::parse(command, &words(args))
}

fn refused(command: Subcommand, args: &[&str]) -> Refusal {
    parse(command, args).expect_err(&format!("{args:?} is refused"))
}

fn id(refusal: &Refusal) -> &'static str {
    refusal.id
}

#[test]
fn only_commands_and_targets_that_need_dev_dependencies_enable_them() {
    for (command, args, expected) in [
        (Subcommand::Build, vec![], false),
        (Subcommand::Build, vec!["--lib"], false),
        (Subcommand::Check, vec!["--bins"], false),
        (Subcommand::Build, vec!["--bin", "app"], false),
        (Subcommand::Build, vec!["--example", "sample"], true),
        (Subcommand::Check, vec!["--tests"], true),
        (Subcommand::Build, vec!["--benches"], true),
        (Subcommand::Check, vec!["--all-targets"], true),
        (Subcommand::Test, vec!["--lib"], true),
        (Subcommand::Test, vec!["--doc"], true),
    ] {
        let request = parse(command, &args).unwrap();
        assert_eq!(request.needs_dev_dependencies(), expected, "{command:?} {args:?}");
    }
}

/// The arguments that read `flag` with its sample value, spelled as `--long value`.
fn typed(flag: &blaze_spec::flags::Flag) -> Vec<String> {
    let mut args = vec![flag.spelled()];
    match flag.takes {
        Takes::Nothing => {}
        Takes::Value => args.push(flag.sample.to_owned()),
        Takes::ValueAfterEquals => args[0] = format!("{}={}", flag.spelled(), flag.sample),
    }
    // `--exclude` is read only with `--workspace`, as cargo has it.
    if flag.long == "exclude" {
        args.push("--workspace".to_owned());
    }
    args
}

#[test]
fn every_flag_of_the_table_is_read_in_its_class() {
    let baseline = |command| Request { command: Some(command), ..Request::default() };
    for flag in FLAGS {
        assert!(!flag.reason().is_empty(), "{} has no reason", flag.spelled());
        for command in COMMANDS {
            let args = typed(flag);
            let read = Request::parse(command, &args);
            if !flag.commands.contains(&command) && !matches!(flag.class, Class::Refused(_)) {
                let error = read.expect_err("cargo rejects a flag the command does not have");
                assert_eq!(id(&error), "UnknownFlag", "{} for {command:?}", flag.spelled());
                assert!(error.what.contains(&flag.spelled()), "{error}");
                continue;
            }
            match flag.class {
                Class::Honoured => {
                    let read =
                        read.unwrap_or_else(|e| panic!("{} for {command:?}: {e}", flag.spelled()));
                    // --offline, --locked and --frozen change whether a spec can be made, but
                    // each is still read into the request.
                    assert_ne!(read, baseline(command), "{} changes nothing", flag.spelled());
                }
                Class::Ignored => {
                    let read =
                        read.unwrap_or_else(|e| panic!("{} for {command:?}: {e}", flag.spelled()));
                    assert_eq!(read, baseline(command), "{} changes the request", flag.spelled());
                }
                Class::Refused(reason) => {
                    let error = read.expect_err("a refused flag");
                    assert_eq!(id(&error), reason.entry().id, "{}", flag.spelled());
                    assert!(error.what.contains(&flag.spelled()), "{error} does not name the flag");
                    assert_eq!(error.why, reason.entry().why);
                }
            }
        }
    }
}

#[test]
fn the_table_has_each_spelling_once() {
    let mut seen = std::collections::BTreeSet::new();
    for flag in FLAGS {
        if !flag.long.is_empty() {
            assert!(seen.insert(format!("--{}", flag.long)), "--{} twice", flag.long);
        }
        if let Some(short) = flag.short {
            assert!(seen.insert(format!("-{short}")), "-{short} twice");
        }
        assert!(!flag.long.is_empty() || flag.short.is_some(), "a flag with no spelling");
        assert!(!flag.commands.is_empty());
    }
}

#[test]
fn the_honoured_flags_the_task_names_are_in_the_table_as_honoured() {
    let honoured: Vec<&str> =
        FLAGS.iter().filter(|it| it.class == Class::Honoured).map(|it| it.long).collect();
    for name in [
        "package",
        "workspace",
        "all",
        "exclude",
        "features",
        "all-features",
        "no-default-features",
        "profile",
        "release",
        "lib",
        "bin",
        "bins",
        "example",
        "examples",
        "test",
        "tests",
        "bench",
        "benches",
        "all-targets",
        "doc",
        "manifest-path",
        "config",
        "offline",
        "locked",
        "frozen",
        "no-run",
        "no-fail-fast",
    ] {
        assert!(honoured.contains(&name), "--{name} is not honoured");
    }
}

#[test]
fn a_value_is_read_in_every_spelling() {
    for command in [Subcommand::Build, Subcommand::Test] {
        for args in [&["-p", "a"][..], &["-pa"], &["--package", "a"], &["--package=a"], &["-p=a"]] {
            assert_eq!(parse(command, args).unwrap().packages, ["a"], "{args:?}");
        }
        assert_eq!(parse(command, &["-p", "a", "-p", "b"]).unwrap().packages, ["a", "b"]);
    }
    let features = |args: &[&str]| parse(Subcommand::Build, args).unwrap().features;
    assert_eq!(features(&["-F", "a,b"]), ["a", "b"]);
    assert_eq!(features(&["--features=a b", "-Fc"]), ["a", "b", "c"]);
    assert_eq!(features(&["--features", "a/x,dep:b"]), ["a/x", "dep:b"]);
    assert_eq!(features(&["-F", ""]), Vec::<String>::new());
}

#[test]
fn ignored_flags_are_read_in_every_spelling_and_change_nothing() {
    let plain = parse(Subcommand::Test, &["-p", "a"]).unwrap();
    for args in [
        &["-p", "a", "-j8"][..],
        &["-p", "a", "-j", "8"],
        &["-p", "a", "--jobs=8"],
        &["-p", "a", "-j", "-1"],
        &["-p", "a", "-vv"],
        &["-p", "a", "-v", "-v"],
        &["-p", "a", "--verbose"],
        &["-p", "a", "-q"],
        &["-p", "a", "-qv"],
        &["-p", "a", "--quiet", "--color", "never"],
        &["-p", "a", "--color=always"],
        &["-p", "a", "--timings"],
        &["-p", "a", "--timings=html"],
        &["-p", "a", "--future-incompat-report"],
        &["-p", "a", "--ignore-rust-version"],
        &["-p", "a", "--target-dir", "t"],
        &["-p", "a", "--target-dir=t"],
        &["-p", "a", "--message-format", "short"],
        &["-p", "a", "--message-format=human"],
        &["-p", "a", "--message-format=human,short"],
    ] {
        assert_eq!(parse(Subcommand::Test, args).unwrap(), plain, "{args:?}");
    }
}

#[test]
fn switches_cluster() {
    let read = parse(Subcommand::Build, &["-rq"]).unwrap();
    assert_eq!(read.profile.as_deref(), Some("release"));
    let read = parse(Subcommand::Build, &["-rj4", "-pa"]).unwrap();
    assert_eq!(
        (read.profile.as_deref(), &*read.packages),
        (Some("release"), &["a".to_owned()][..])
    );
}

#[test]
fn the_profile_is_release_or_named() {
    let profile = |args: &[&str]| parse(Subcommand::Build, args).map(|it| it.profile);
    assert_eq!(profile(&[]).unwrap(), None);
    assert_eq!(profile(&["--release"]).unwrap().as_deref(), Some("release"));
    assert_eq!(profile(&["-r"]).unwrap().as_deref(), Some("release"));
    assert_eq!(profile(&["--profile", "bench"]).unwrap().as_deref(), Some("bench"));
    assert_eq!(profile(&["--profile=custom-1"]).unwrap().as_deref(), Some("custom-1"));
    // `--release` is `--profile=release`.
    assert_eq!(profile(&["-r", "--profile", "release"]).unwrap().as_deref(), Some("release"));
    let conflict = profile(&["--release", "--profile", "dev"]).unwrap_err();
    assert_eq!(id(&conflict), "ProfileConflict");
    assert_eq!(conflict.what, "--release --profile dev");
    assert_eq!(id(&profile(&["--profile", "doc"]).unwrap_err()), "BadValue");
    assert_eq!(id(&profile(&["--profile", "a b"]).unwrap_err()), "BadValue");
}

#[test]
fn targets_are_chosen_as_cargo_chooses_them() {
    let targets = |args: &[&str]| parse(Subcommand::Test, args).unwrap().targets;
    let none = targets(&[]);
    assert!(!none.explicit && !none.lib && none.bins.is_none() && !none.all_targets);
    let t = targets(&["--lib"]);
    assert!(t.explicit && t.lib && t.tests.is_none());
    let t = targets(&["--bin", "x", "--bin", "y"]);
    assert_eq!(t.bins, Some(vec!["x".to_owned(), "y".to_owned()]));
    assert!(t.explicit && !t.lib);
    // Every binary wins over the names given.
    assert_eq!(targets(&["--bins", "--bin", "x"]).bins, Some(Vec::new()));
    assert_eq!(targets(&["--tests"]).tests, Some(Vec::new()));
    assert_eq!(targets(&["--test", "t"]).tests, Some(vec!["t".to_owned()]));
    assert_eq!(targets(&["--examples"]).examples, Some(Vec::new()));
    assert_eq!(targets(&["--example=e"]).examples, Some(vec!["e".to_owned()]));
    assert_eq!(targets(&["--benches"]).benches, Some(Vec::new()));
    assert_eq!(targets(&["--bench", "b"]).benches, Some(vec!["b".to_owned()]));
    assert!(targets(&["--all-targets"]).all_targets);
    // --doc selects no target the way the other flags do.
    let doc = parse(Subcommand::Test, &["--doc"]).unwrap();
    assert!(doc.doc && !doc.targets.explicit);
}

#[test]
fn what_is_cargos_and_what_is_the_programs() {
    let test = parse(
        Subcommand::Test,
        &["--no-fail-fast", "-p", "a", "filter", "--no-run", "--", "--nocapture", "x"],
    )
    .unwrap();
    assert_eq!(test.positional, ["filter"]);
    assert_eq!(test.trailing, ["--nocapture", "x"]);
    assert!(test.no_fail_fast && test.no_run);
    // Words after `--` are never flags, whatever they look like.
    let test = parse(Subcommand::Test, &["--", "--release", "-p", "--workspace"]).unwrap();
    assert!(test.packages.is_empty() && test.profile.is_none() && !test.workspace);
    assert_eq!(test.trailing, ["--release", "-p", "--workspace"]);

    let run = parse(Subcommand::Run, &["--bin", "a", "one", "two"]).unwrap();
    assert_eq!(run.positional, ["one", "two"]);
    let run = parse(Subcommand::Run, &["--bin", "a", "--", "--one", "-x"]).unwrap();
    assert_eq!(
        (run.positional.len(), &*run.trailing),
        (0, &["--one".to_owned(), "-x".to_owned()][..])
    );
    let run = parse(Subcommand::Run, &["-r", "--", "x"]).unwrap();
    assert_eq!(run.trailing, ["x"]);
}

#[test]
fn cargo_run_passes_every_word_after_its_first_argument_to_the_program() {
    for args in
        [&["a", "--release"][..], &["a", "-r"], &["a", "--"], &["a", "b", "--", "c"], &["a", "-j8"]]
    {
        let error = refused(Subcommand::Run, args);
        assert_eq!(id(&error), "RunArgsAfterFirst", "{args:?}");
    }
    // The same words are fine for `test`, which has no such rule.
    assert!(parse(Subcommand::Test, &["a", "--release"]).is_ok());
}

#[test]
fn cargo_rejects_what_it_has_no_place_for() {
    for command in [Subcommand::Build, Subcommand::Check] {
        assert_eq!(id(&refused(command, &["word"])), "ExtraArgument");
        assert_eq!(id(&refused(command, &["--"])), "ExtraArgument");
        assert_eq!(id(&refused(command, &["--", "x"])), "ExtraArgument");
    }
    let second = refused(Subcommand::Test, &["one", "two"]);
    assert_eq!((id(&second), second.what.as_str()), ("ExtraArgument", "two"));
    // A flag of another command, and one that does not exist.
    for (command, flag) in [
        (Subcommand::Run, "--workspace"),
        (Subcommand::Run, "--lib"),
        (Subcommand::Run, "--no-run"),
        (Subcommand::Build, "--no-run"),
        (Subcommand::Check, "--doc"),
        (Subcommand::Build, "--no-fail-fast"),
        (Subcommand::Build, "--frobnicate"),
        (Subcommand::Build, "-x"),
    ] {
        let error = refused(command, &[flag]);
        assert_eq!(id(&error), "UnknownFlag", "{flag} for {command:?}");
        assert!(error.what.contains(flag) && error.what.contains(command.name()), "{error}");
    }
}

#[test]
fn a_libtest_flag_before_the_dashes_is_cargos_to_reject() {
    for flag in [
        "--exact",
        "--ignored",
        "--include-ignored",
        "--nocapture",
        "--show-output",
        "--test-threads=1",
        "--skip",
        "--list",
    ] {
        let error = refused(Subcommand::Test, &[flag]);
        assert_eq!(id(&error), "LibtestFlag", "{flag}");
        assert_eq!(error.what, flag);
    }
    // After `--` the same flags are libtest's.
    assert!(parse(Subcommand::Test, &["--", "--exact", "--nocapture"]).is_ok());
}

#[test]
fn a_flag_without_its_value_is_refused() {
    for flag in
        ["-p", "--package", "--features", "-F", "--profile", "--bin", "--manifest-path", "--config"]
    {
        let error = refused(Subcommand::Build, &[flag]);
        assert_eq!(id(&error), "MissingValue", "{flag}");
        // The next word being a flag is no value.
        let error = refused(Subcommand::Build, &[flag, "--workspace"]);
        assert_eq!(id(&error), "MissingValue", "{flag} --workspace");
    }
    let error = refused(Subcommand::Build, &["--workspace=yes"]);
    assert_eq!(id(&error), "BadValue");
}

#[test]
fn values_cargo_rejects_are_refused() {
    assert_eq!(id(&refused(Subcommand::Build, &["--color", "sometimes"])), "BadValue");
    assert_eq!(id(&refused(Subcommand::Build, &["--message-format", "nonsense"])), "BadValue");
    for format in ["json", "json-diagnostic-short", "json-render-diagnostics", "human,json"] {
        let error = refused(Subcommand::Build, &["--message-format", format]);
        assert_eq!(id(&error), "MessageFormat", "{format}");
    }
    assert_eq!(id(&refused(Subcommand::Build, &["--timings=json"])), "TimingsValue");
    assert_eq!(id(&refused(Subcommand::Build, &["--exclude", "a"])), "ExcludeWithoutWorkspace");
    assert!(parse(Subcommand::Build, &["--workspace", "--exclude", "a"]).is_ok());
    assert!(parse(Subcommand::Build, &["--all", "--exclude=a"]).is_ok());
    for target in ["--lib", "--bins", "--test=t", "--all-targets"] {
        let error = refused(Subcommand::Test, &["--doc", target]);
        assert_eq!(id(&error), "DocWithTargets", "{target}");
    }
}

#[test]
fn config_arguments_are_kept_and_an_unstable_key_is_refused() {
    let read =
        parse(Subcommand::Build, &["--config", "build.jobs=1", "--config=profile.dev.opt-level=1"])
            .unwrap();
    assert_eq!(read.config, ["build.jobs=1", "profile.dev.opt-level=1"]);
    for config in ["unstable.build-std=[\"core\"]", "unstable.x = 1", "unstable"] {
        let error = refused(Subcommand::Build, &["--config", config]);
        assert_eq!(id(&error), "ConfigUnstable", "{config}");
    }
    // A key that only starts like it.
    assert!(parse(Subcommand::Build, &["--config", "unstableish.x=1"]).is_ok());
    // A path of a file is no key.
    assert!(parse(Subcommand::Build, &["--config", "extra.toml"]).is_ok());
}

#[test]
fn the_flags_blaze_refuses_are_named_with_their_value() {
    let cases: &[(&[&str], &str, &str)] = &[
        (&["--target", "x86_64-unknown-linux-gnu"], "Target", "--target x86_64-unknown-linux-gnu"),
        (&["--target=aarch64-apple-darwin"], "Target", "--target aarch64-apple-darwin"),
        (&["-Zunstable-options"], "UnstableFlag", "-Z unstable-options"),
        (&["-Z", "build-std"], "UnstableFlag", "-Z build-std"),
        (&["--build-plan"], "BuildPlan", "--build-plan"),
        (&["--unit-graph"], "UnitGraphFlag", "--unit-graph"),
        (&["--artifact-dir", "out"], "ArtifactDir", "--artifact-dir out"),
        (&["--out-dir=out"], "ArtifactDir", "--out-dir out"),
        (&["--keep-going"], "KeepGoing", "--keep-going"),
        (&["--lockfile-path", "l"], "LockfilePath", "--lockfile-path l"),
        (&["-C", "dir"], "ChangeDir", "-C dir"),
        (&["--help"], "Help", "--help"),
        (&["-h"], "Help", "--help"),
    ];
    for command in COMMANDS {
        for (args, reason, what) in cases {
            let error = refused(command, args);
            assert_eq!((id(&error), error.what.as_str()), (*reason, *what), "{args:?}");
            assert!(!error.why.is_empty());
        }
    }
}

#[test]
fn what_is_read_is_the_same_whichever_the_order() {
    let a = parse(Subcommand::Test, &["-p", "a", "--features", "x", "--release", "t", "--", "q"])
        .unwrap();
    let b = parse(Subcommand::Test, &["--release", "t", "--features=x", "-pa", "--", "q"]).unwrap();
    assert_eq!(a, b);
}

#[test]
fn libtest_options_are_read_out_of_what_follows_the_dashes() {
    let read = |args: &[&str]| {
        let mut all = vec!["--"];
        all.extend_from_slice(args);
        parse(Subcommand::Test, &all).map(|it| it.libtest)
    };
    let none = read(&[]).unwrap();
    assert_eq!((none.exact, none.skip.len(), none.ignored, none.test_threads), (false, 0, 0, None));
    assert!(read(&["--exact"]).unwrap().exact);
    assert_eq!(read(&["--ignored"]).unwrap().ignored, 1);
    assert_eq!(read(&["--include-ignored"]).unwrap().ignored, 2);
    assert_eq!(read(&["--skip", "a", "--skip=b", "--nocapture"]).unwrap().skip, ["a", "b"]);
    assert_eq!(read(&["--test-threads", "4"]).unwrap().test_threads, Some(4));
    assert_eq!(read(&["--test-threads=1"]).unwrap().test_threads, Some(1));
    // libtest stops reading options at a `--` of its own.
    let after = read(&["--exact", "--", "--ignored", "--skip", "x"]).unwrap();
    assert!(after.exact && after.ignored == 0 && after.skip.is_empty());
    // A name is no option: `--exactly` is libtest's to refuse and is carried.
    assert!(!read(&["--exactly"]).unwrap().exact);
    for (args, reason) in [
        (&["--exact", "--exact"][..], "LibtestDuplicate"),
        (&["--ignored", "--ignored"], "LibtestDuplicate"),
        (&["--test-threads=1", "--test-threads", "2"], "LibtestDuplicate"),
        (&["--ignored", "--include-ignored"], "LibtestIgnored"),
        (&["--test-threads", "0"], "LibtestThreads"),
        (&["--test-threads=many"], "LibtestThreads"),
        (&["--skip"], "MissingValue"),
        (&["--test-threads"], "MissingValue"),
    ] {
        assert_eq!(id(&read(args).unwrap_err()), reason, "{args:?}");
    }
    // `run` passes them to a program of its own, and reads none.
    let run = parse(Subcommand::Run, &["--", "--exact"]).unwrap();
    assert!(!run.libtest.exact);
}
