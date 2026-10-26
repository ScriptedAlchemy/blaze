//! Reproduction candidates retain run controls and never copy diagnostic-only options.

mod common;

use blaze_spec::{flags::Subcommand, run};
use blaze_wire::Command;
use common::{Fixture, spec};

#[test]
fn run_controls_survive_reproduction() {
    let fixture = Fixture::new();
    let made = spec(
        &fixture,
        &fixture.env(),
        Subcommand::Test,
        &["--no-run", "--no-fail-fast", "needle", "--", "--include-ignored", "--exact"],
        None,
    )
    .unwrap();
    for candidate in run::candidates(&made.build).unwrap() {
        assert!(candidate.args.contains(&"--no-run".to_owned()));
        assert!(candidate.args.contains(&"--no-fail-fast".to_owned()));
        assert!(candidate.args.ends_with(&[
            "needle".to_owned(),
            "--".to_owned(),
            "--include-ignored".to_owned(),
            "--exact".to_owned(),
        ]));
        assert!(!candidate.args.contains(&"-v".to_owned()));
        assert_eq!(candidate.cwd, ".");
    }
    assert_eq!(run::first_difference(&made.build, &made.build), None);
    let mut changed = made.build.clone();
    let Command::Test(control) = &mut changed.command else { panic!("test") };
    control.no_run = false;
    assert!(run::first_difference(&made.build, &changed).unwrap().starts_with("command:"));
}

#[test]
fn ambient_build_variables_are_governed() {
    for key in [
        "RUSTFLAGS",
        "CARGO_PROFILE_DEV_OPT_LEVEL",
        "CC",
        "TARGET_CC",
        "CC_aarch64_apple_darwin",
        "MACOSX_DEPLOYMENT_TARGET",
        "SDKROOT",
    ] {
        assert!(run::governed(key), "{key}");
    }
    for key in ["RA_JIT_HOME", "RA_JIT_MEMORY_MAX", "RA_JIT_OWN_BASE_MEMORY", "HOME"] {
        assert!(!run::governed(key), "{key}");
    }
}
