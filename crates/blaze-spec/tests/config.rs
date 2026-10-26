//! cargo's configuration: the keys blaze refuses by name, the ones it ignores, `[env]`
//! resolved, and where the command was run.

mod common;

use blaze_spec::assemble::Spec;
use blaze_spec::config::KEYS;
use blaze_spec::flags::{Class, Subcommand};
use blaze_spec::Refusal;

use common::{Fixture, spec};

fn of(fixture: &Fixture, command: Subcommand, args: &[&str]) -> Result<Spec, Refusal> {
    spec(fixture, &fixture.env(), command, args, None)
}

fn refused(config: &str, command: Subcommand, args: &[&str]) -> Refusal {
    of(&Fixture::with_config(config), command, args).expect_err(&format!("{config:?} is refused"))
}

fn accepted(config: &str) -> Spec {
    of(&Fixture::with_config(config), Subcommand::Build, &[]).unwrap_or_else(|e| panic!("{config:?}: {e}"))
}

fn plain() -> Spec {
    accepted("")
}

#[test]
fn the_table_of_keys_has_a_reason_for_each() {
    for key in KEYS {
        assert!(!key.reason().is_empty(), "{} has no reason", key.key);
        if let Class::Refused(reason) = key.class {
            assert_eq!(key.reason(), reason.entry().why);
        }
    }
    let refused: Vec<&str> =
        KEYS.iter().filter(|it| matches!(it.class, Class::Refused(_))).map(|it| it.key).collect();
    for key in [
        "build.target", "build.rustc", "build.rustc-wrapper", "build.rustc-workspace-wrapper",
        "build.rustdoc", "build.target-applies-to-host", "host.*", "unstable.*", "target.<triple>.runner",
    ] {
        assert!(refused.contains(&key), "{key} is not refused");
    }
}

#[test]
fn a_target_is_refused_by_the_key_or_the_variable_that_set_it() {
    for config in ["[build]\ntarget = \"x86_64-unknown-linux-gnu\"\n", "[build]\ntarget = \"aarch64-apple-darwin\"\n", "[build]\ntarget = [\"a\", \"b\"]\n"] {
        let error = refused(config, Subcommand::Build, &[]);
        assert_eq!((error.id, error.what.as_str()), ("BuildTarget", "build.target"), "{config}");
    }
    let fixture = Fixture::new();
    let mut env = fixture.env();
    env.insert("CARGO_BUILD_TARGET".to_owned(), "aarch64-apple-darwin".to_owned());
    let error = spec(&fixture, &env, Subcommand::Build, &[], None).unwrap_err();
    assert_eq!((error.id, error.what.as_str()), ("BuildTarget", "CARGO_BUILD_TARGET"));
    let error = of(&Fixture::new(), Subcommand::Build, &["--config", "build.target=\"x\""]).unwrap_err();
    assert_eq!(error.id, "BuildTarget");
    // No target is the host.
    assert!(of(&Fixture::with_config("[build]\ntarget = []\n"), Subcommand::Build, &[]).is_ok());
}

#[test]
fn a_compiler_a_wrapper_or_an_artifact_directory_is_refused_by_name() {
    for (config, id, what) in [
        ("[build]\nrustc = \"/x/rustc\"\n", "BuildRustc", "build.rustc"),
        ("[build]\nrustc-wrapper = \"sccache\"\n", "BuildRustcWrapper", "build.rustc-wrapper"),
        ("[build]\nrustc-workspace-wrapper = \"w\"\n", "BuildRustcWrapper", "build.rustc-workspace-wrapper"),
        ("[build]\nrustdoc = \"/x/rustdoc\"\n", "BuildRustdoc", "build.rustdoc"),
        ("[build]\nartifact-dir = \"out\"\n", "BuildArtifactDir", "build.artifact-dir"),
        ("[build]\nwarnings = \"deny\"\n", "BuildWarnings", "build.warnings = \"deny\""),
        ("[build]\nwarnings = \"allow\"\n", "BuildWarnings", "build.warnings = \"allow\""),
        ("[build]\nsbom = true\n", "BuildUnstable", "build.sbom = true"),
        ("[build]\ndep-info-basedir = \"/x\"\n", "BuildUnstable", "build.dep-info-basedir"),
        ("[build]\ntarget-applies-to-host = false\n", "HostConfig", "build.target-applies-to-host"),
        ("[host]\nlinker = \"x\"\n", "HostConfig", "host.linker"),
        ("[unstable]\nbuild-std = [\"core\"]\n", "UnstableTable", "unstable.build-std"),
        ("[resolver]\nlockfile-path = \"l\"\n", "LockfilePath", "resolver.lockfile-path"),
    ] {
        let error = refused(config, Subcommand::Build, &[]);
        assert_eq!((error.id, error.what.as_str()), (id, what), "{config}");
        assert!(!error.why.is_empty());
    }
    let fixture = Fixture::new();
    for (variable, what) in [("CARGO_BUILD_RUSTC", "CARGO_BUILD_RUSTC"), ("CARGO_BUILD_RUSTC_WRAPPER", "CARGO_BUILD_RUSTC_WRAPPER")] {
        let mut env = fixture.env();
        env.insert(variable.to_owned(), "x".to_owned());
        let error = spec(&fixture, &env, Subcommand::Build, &[], None).unwrap_err();
        assert_eq!(error.what, what);
    }
}

#[test]
fn what_is_near_a_refused_key_and_harmless_is_accepted() {
    let plain = plain();
    for config in [
        "[build]\nwarnings = \"warn\"\n",
        "[build]\nsbom = false\n",
        "[build]\njobs = 3\n",
        "[build]\ntarget-dir = \"elsewhere\"\n",
        "[build]\nincremental = true\n",
        "[build]\npipelining = false\n",
        "[net]\nretry = 5\n",
        "[http]\ntimeout = 3\n",
        "[term]\ncolor = \"never\"\nverbose = true\n",
        "[alias]\nb = \"build\"\n",
        "[registries.x]\nindex = \"sparse+https://example.com/\"\n",
        "[future-incompat-report]\nfrequency = \"never\"\n",
        "[resolver]\nincompatible-rust-versions = \"fallback\"\nfeature-unification = \"selected\"\n",
        "[target.x86_64-unknown-linux-gnu]\nrunner = \"qemu\"\nlinker = \"gcc\"\n[target.x86_64-unknown-linux-gnu.links-x]\nrustc-link-lib = [\"z\"]\n",
        "[target.'cfg(target_os = \"linux\")']\nrunner = \"x\"\nlinker = \"y\"\n",
        "[target.aarch64-apple-darwin]\nar = \"x\"\n",
    ] {
        assert_eq!(accepted(config), plain, "{config}");
    }
}

#[test]
fn a_runner_is_refused_for_the_commands_that_start_programs_through_it() {
    let host = "[target.aarch64-apple-darwin]\nrunner = [\"echo\", \"x\"]\n";
    let cfg = "[target.'cfg(target_os = \"macos\")']\nrunner = \"echo\"\n";
    for (config, what) in [(host, "target.aarch64-apple-darwin.runner"), (cfg, "target.cfg(target_os = \"macos\").runner")] {
        for command in [Subcommand::Test, Subcommand::Run] {
            let error = refused(config, command, &[]);
            assert_eq!((error.id, error.what.as_str()), ("TargetRunner", what), "{command:?}");
        }
        for command in [Subcommand::Build, Subcommand::Check] {
            assert!(of(&Fixture::with_config(config), command, &[]).is_ok(), "{command:?}");
        }
    }
    let fixture = Fixture::new();
    let mut env = fixture.env();
    env.insert("CARGO_TARGET_AARCH64_APPLE_DARWIN_RUNNER".to_owned(), "echo".to_owned());
    let error = spec(&fixture, &env, Subcommand::Test, &[], None).unwrap_err();
    assert_eq!(error.what, "CARGO_TARGET_AARCH64_APPLE_DARWIN_RUNNER");
}

#[test]
fn a_linker_is_refused_for_the_commands_that_link() {
    let config = "[target.aarch64-apple-darwin]\nlinker = \"clang\"\n";
    for command in [Subcommand::Build, Subcommand::Test, Subcommand::Run] {
        let error = refused(config, command, &[]);
        assert_eq!((error.id, error.what.as_str()), ("TargetLinker", "target.aarch64-apple-darwin.linker"));
    }
    assert!(of(&Fixture::with_config(config), Subcommand::Check, &[]).is_ok());
    let error = refused("[target.aarch64-apple-darwin.foo]\nrustc-link-lib = [\"z\"]\n", Subcommand::Check, &[]);
    assert_eq!((error.id, error.what.as_str()), ("TargetLinks", "target.aarch64-apple-darwin.foo"));
}

#[test]
fn another_way_of_unifying_features_and_a_config_patch_are_refused() {
    for unification in ["package", "workspace"] {
        let error = refused(&format!("[resolver]\nfeature-unification = \"{unification}\"\n"), Subcommand::Build, &[]);
        assert_eq!(error.id, "FeatureUnification");
        assert_eq!(error.what, format!("resolver.feature-unification = \"{unification}\""));
    }
    let error = refused("[patch.crates-io]\nfoo = { path = \"x\" }\n", Subcommand::Build, &[]);
    assert_eq!((error.id, error.what.as_str()), ("ConfigPatch", "patch.crates-io"));
}

#[test]
fn rustdoc_flags_matter_only_where_doc_tests_run() {
    let config = "[build]\nrustdocflags = [\"--cfg\", \"docs\"]\n";
    let error = refused(config, Subcommand::Test, &[]);
    assert_eq!((error.id, error.what.as_str()), ("RustdocFlags", "build.rustdocflags"));
    assert_eq!(refused(config, Subcommand::Test, &["--doc"]).id, "RustdocFlags");
    let fixture = Fixture::with_config(config);
    for (command, args) in [
        (Subcommand::Test, &["--lib"][..]),
        (Subcommand::Test, &["--tests"]),
        (Subcommand::Build, &[]),
        (Subcommand::Check, &[]),
    ] {
        assert!(of(&fixture, command, args).is_ok(), "{command:?} {args:?}");
    }
    let plain = Fixture::new();
    let mut env = plain.env();
    env.insert("RUSTDOCFLAGS".to_owned(), "--cfg x".to_owned());
    let error = spec(&plain, &env, Subcommand::Test, &[], None).unwrap_err();
    assert_eq!(error.what, "RUSTDOCFLAGS");
}

#[test]
fn a_config_that_cargo_rejects_is_refused_with_its_message() {
    let error = refused("[build]\nincremental = \"yes\"\n", Subcommand::Build, &[]);
    assert_eq!(error.id, "ConfigRejected");
    assert!(!error.why.is_empty());
    let error = refused("this is = not [valid\n", Subcommand::Build, &[]);
    assert_eq!(error.id, "ConfigRejected");
    let error = refused("[env]\nCARGO_HOME = \"x\"\n", Subcommand::Build, &[]);
    assert_eq!((error.id, error.what.as_str()), ("ConfigRejected", "env.CARGO_HOME"));
}

fn env_of(spec: &Spec) -> Vec<(String, String)> {
    spec.build.config_env.iter().cloned().collect()
}

fn pair(name: &str, value: &str) -> (String, String) {
    (name.to_owned(), value.to_owned())
}

#[test]
fn the_env_table_is_resolved_against_the_requesters_environment() {
    let config = "[env]\nPLAIN = \"one\"\nSET = \"from-config\"\nFORCED = { value = \"forced\", force = true }\nSET_FORCED = { value = \"forced\", force = true }\nNOT_FORCED = { value = \"v\", force = false }\n";
    let fixture = Fixture::with_config(config);
    let mut env = fixture.env();
    env.insert("SET".to_owned(), "from-environment".to_owned());
    env.insert("SET_FORCED".to_owned(), "from-environment".to_owned());
    let spec = spec(&fixture, &env, Subcommand::Build, &[], None).unwrap();
    let mut want = vec![
        pair("PLAIN", "one"),
        // A variable the environment has wins unless the entry is forced.
        pair("SET", "from-environment"),
        pair("FORCED", "forced"),
        pair("SET_FORCED", "forced"),
        pair("NOT_FORCED", "v"),
    ];
    want.sort_by_key(|it| blaze_wire::encode(it));
    assert_eq!(env_of(&spec), want);
    assert!(spec.rebased_env.is_empty());
    // With no variable of its own the same file gives the entry's.
    let spec = of(&fixture, Subcommand::Build, &[]).unwrap();
    assert!(env_of(&spec).contains(&pair("SET", "from-config")));
    assert!(plain().build.config_env.is_empty());
}

#[test]
fn a_relative_path_is_made_relative_to_the_workspace_root() {
    let config = "[env]\nVENDOR = { value = \"vendor/x\", relative = true }\nHERE = { value = \".\", relative = true }\nABSOLUTE = { value = \"/abs\", relative = false }\n";
    let fixture = Fixture::with_config(config);
    let spec = of(&fixture, Subcommand::Build, &[]).unwrap();
    let env = env_of(&spec);
    assert!(env.contains(&pair("VENDOR", "vendor/x")), "{env:?}");
    assert!(env.contains(&pair("HERE", ".")));
    assert!(env.contains(&pair("ABSOLUTE", "/abs")));
    let mut rebased = spec.rebased_env.clone();
    rebased.sort();
    assert_eq!(rebased, ["HERE", "VENDOR"]);
    // The workspace in another place is the same spec: no path of the machine is in it.
    let other = Fixture::with_config(config);
    assert_eq!(of(&other, Subcommand::Build, &[]).unwrap().build.config_env, spec.build.config_env);
    // A file below the root, as a `cwd` below it finds it, is relative to where the file is.
    let below = Fixture::with_config("");
    below.write("a/.cargo/config.toml", "[env]\nHERE = { value = \"data\", relative = true }\n");
    let root = below.root();
    let env = below.env();
    let host = common::host(false);
    let cwd = root.join("a");
    let context = common::context(&below, &env, &host, &cwd);
    let package = common::Package::member(&root, "a");
    let profile = common::profile_of(&below, &env, "dev", &package, true, false);
    let graph = common::simple_graph(&profile);
    let request = blaze_spec::flags::Request::parse(Subcommand::Build, &[]).unwrap();
    let spec = blaze_spec::assemble::assemble(&request, &context, &graph).unwrap();
    assert!(env_of(&spec).contains(&pair("HERE", "a/data")), "{:?}", env_of(&spec));
    assert_eq!(spec.build.cwd, "a");
}

#[test]
fn a_relative_path_of_a_file_outside_the_workspace_is_refused() {
    let fixture = Fixture::new();
    let outside = tempfile::tempdir().unwrap();
    let file = outside.path().join(".cargo").join("extra.toml");
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    std::fs::write(&file, "[env]\nELSEWHERE = { value = \"x\", relative = true }\n").unwrap();
    let error = of(&fixture, Subcommand::Build, &["--config", file.to_str().unwrap()]).unwrap_err();
    assert_eq!(error.id, "EnvRelativeOutside");
    assert!(error.what.starts_with("env.ELSEWHERE"), "{error}");
    // A plain value of the same file is fine.
    std::fs::write(&file, "[env]\nELSEWHERE = \"x\"\n").unwrap();
    assert!(of(&fixture, Subcommand::Build, &["--config", file.to_str().unwrap()]).is_ok());
}

#[test]
fn the_directory_is_named_from_the_workspace_root() {
    let fixture = Fixture::new();
    assert_eq!(of(&fixture, Subcommand::Build, &[]).unwrap().build.cwd, ".");
    let env = fixture.env();
    let host = common::host(false);
    let root = fixture.root();
    let below = root.join("a").join("src");
    std::fs::create_dir_all(&below).unwrap();
    let context = common::context(&fixture, &env, &host, &below);
    let profile = common::profile_of(&fixture, &env, "dev", &common::Package::member(&root, "a"), true, false);
    let request = blaze_spec::flags::Request::parse(Subcommand::Build, &[]).unwrap();
    let spec = blaze_spec::assemble::assemble(&request, &context, &common::simple_graph(&profile)).unwrap();
    assert_eq!(spec.build.cwd, "a/src");
    // Outside the workspace of the manifest it names.
    let elsewhere = tempfile::tempdir().unwrap();
    let cwd = elsewhere.path().canonicalize().unwrap();
    let mut env = env;
    env.insert("__CARGO_TEST_ROOT".to_owned(), cwd.display().to_string());
    let context = common::context(&fixture, &env, &host, &cwd);
    let manifest = root.join("Cargo.toml").display().to_string();
    let request = blaze_spec::flags::Request::parse(Subcommand::Build, &["--manifest-path".to_owned(), manifest]).unwrap();
    let error = blaze_spec::assemble::assemble(&request, &context, &common::simple_graph(&profile)).unwrap_err();
    assert_eq!(error.id, "Cwd");
}
