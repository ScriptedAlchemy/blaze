//! The real manifest/resolver/spec boundary, not the synthetic assemble helper.

mod common;

use blaze_spec::{
    assemble::{Spec, build_spec},
    flags::{Request, Subcommand},
};
use common::{Fixture, context, host};

fn resolve(
    fixture: &Fixture,
    command: Subcommand,
    words: &[&str],
) -> Result<Spec, blaze_spec::Refusal> {
    let root = fixture.root();
    let env = fixture.env();
    let host = host(false);
    let request =
        Request::parse(command, &words.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>())?;
    build_spec(&request, &context(fixture, &env, &host, &root))
}

fn locked(fixture: &Fixture, names: &[&str]) {
    let mut lock = "version = 4\n".to_owned();
    for name in names {
        lock.push_str(&format!("\n[[package]]\nname = \"{name}\"\nversion = \"0.1.0\"\n"));
        if *name == "a"
            && std::fs::read_to_string(fixture.root().join("a/Cargo.toml"))
                .is_ok_and(|manifest| manifest.contains("[dependencies]"))
        {
            lock.push_str("dependencies = [\"b\"]\n");
        }
    }
    fixture.write("Cargo.lock", &lock);
}

#[test]
fn doctest_only_selects_the_requested_library_not_its_member_dependency() {
    let fixture = Fixture::new();
    fixture.write("a/Cargo.toml", "[package]\nname=\"a\"\nversion=\"0.1.0\"\nedition=\"2021\"\n[dependencies]\nb={path=\"../b\"}\n");
    locked(&fixture, &["a", "b"]);
    let made = resolve(&fixture, Subcommand::Test, &["--doc", "-p", "a"]).unwrap();
    let selected: Vec<_> =
        made.build.asked.iter().map(|&i| &made.build.units[i as usize]).collect();
    assert_eq!(selected.len(), 1);
    assert_eq!(selected[0].package.0, "a");
    assert!(!selected[0].test && !selected[0].host);
    assert!(made.build.units.iter().any(|u| u.package.0 == "b" && !u.test));
    assert!(made.build.units.iter().all(|u| !u.test));
}

#[test]
fn standalone_and_proc_macro_doctest_libraries_are_selected() {
    for proc_macro in [false, true] {
        let fixture =
            Fixture::with_root("[package]\nname=\"solo\"\nversion=\"0.1.0\"\nedition=\"2021\"\n");
        fixture.write("src/lib.rs", "");
        if proc_macro {
            fixture.write("Cargo.toml", "[package]\nname=\"solo\"\nversion=\"0.1.0\"\nedition=\"2021\"\n[lib]\nproc-macro=true\n");
        }
        locked(&fixture, &["solo"]);
        let made = resolve(&fixture, Subcommand::Test, &["--doc"]).unwrap();
        assert_eq!(made.build.asked.len(), 1);
        let unit = &made.build.units[made.build.asked.as_slice()[0] as usize];
        assert_eq!(unit.package.0, "solo");
        assert_eq!(unit.target.0, if proc_macro { 6 } else { 0 });
        assert!(!unit.test);
        assert_eq!(unit.host, proc_macro);
    }
}

#[test]
fn ordinary_doctest_only_builds_are_target_reachable_without_promoting_macros() {
    let fixture = Fixture::new();
    fixture.write(
        "b/Cargo.toml",
        "[package]\nname=\"b\"\nversion=\"0.1.0\"\nedition=\"2021\"\n[lib]\nproc-macro=true\n",
    );
    locked(&fixture, &["a", "b"]);
    let made = resolve(&fixture, Subcommand::Test, &["--workspace"]).unwrap();
    let library = made.build.units.iter().find(|u| u.package.0 == "a" && !u.test).unwrap();
    assert!(!library.host);
    let macro_library = made.build.units.iter().find(|u| u.package.0 == "b" && !u.test).unwrap();
    assert!(macro_library.host);
}

#[test]
fn check_refuses_linker_overrides_with_real_host_compilations() {
    for proc_macro in [false, true] {
        let fixture = Fixture::new();
        if proc_macro {
            fixture.write("a/Cargo.toml", "[package]\nname=\"a\"\nversion=\"0.1.0\"\nedition=\"2021\"\n[lib]\nproc-macro=true\n");
        } else {
            fixture.write("a/build.rs", "fn main() {}");
        }
        locked(&fixture, &["a", "b"]);
        let made = resolve(&fixture, Subcommand::Check, &["-p", "a"]).unwrap();
        assert!(made.build.units.iter().any(|unit| unit.host));
        for key in
            ["target.aarch64-apple-darwin.linker", "target.'cfg(target_os = \"macos\")'.linker"]
        {
            fixture.write(
                ".cargo/config.toml",
                &format!(
                    "[{key_without_linker}]\nlinker=\"unused-linker\"\n",
                    key_without_linker = key.strip_suffix(".linker").unwrap()
                ),
            );
            let error = resolve(&fixture, Subcommand::Check, &["-p", "a"]).unwrap_err();
            assert_eq!(error.id, "TargetLinker");
        }
    }
}

#[test]
fn actual_native_compiler_bytes_change_spec_and_job_with_unused_target_cc_fixed() {
    use blaze_spec::platform::Probe;
    use std::path::{Path, PathBuf};
    struct Compiler(&'static [u8]);
    impl Probe for Compiler {
        fn which(&self, name: &str, path: &str) -> Option<PathBuf> {
            common::FakeMachine.which(name, path)
        }
        fn run(&self, program: &Path, args: &[&str]) -> Option<String> {
            common::FakeMachine.run(program, args)
        }
        fn read(&self, path: &Path) -> Option<Vec<u8>> {
            if path == Path::new("/opt/actual/cc") {
                Some(self.0.to_vec())
            } else {
                common::FakeMachine.read(path)
            }
        }
        fn os(&self) -> Option<String> {
            common::FakeMachine.os()
        }
    }
    let fixture = Fixture::new();
    fixture.write("a/build.rs", "fn main() {}");
    locked(&fixture, &["a", "b"]);
    let root = fixture.root();
    let host = host(false);
    let mut env = fixture.env();
    env.insert("CC".to_owned(), "/opt/actual/cc".to_owned());
    env.insert("TARGET_CC".to_owned(), "/opt/unused/cc".to_owned());
    let request = Request::parse(Subcommand::Build, &["-p".to_owned(), "a".to_owned()]).unwrap();
    let make = |probe: &dyn Probe| {
        let mut ctx = context(&fixture, &env, &host, &root);
        ctx.probe = probe;
        build_spec(&request, &ctx).unwrap()
    };
    let a = make(&Compiler(b"v1"));
    let b = make(&Compiler(b"v2"));
    assert_ne!(a.platform_digest, b.platform_digest);
    assert_ne!(a.digest, b.digest);
    assert_ne!(
        a.job_content(blaze_wire::Namespace::Main),
        b.job_content(blaze_wire::Namespace::Main)
    );
    assert_eq!(a.build.script_env, b.build.script_env);
}
