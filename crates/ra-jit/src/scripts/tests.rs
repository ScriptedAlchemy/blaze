use std::time::Duration;

use super::*;

fn dir(test: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("ra-jit-scripts-{test}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn include_and_exclude_are_read_from_the_manifest_or_the_workspace() {
    let root = dir("include");
    std::fs::write(
        root.join("Cargo.toml"),
        "[workspace]\nmembers = [\"a\", \"b\"]\n[workspace.package]\nexclude = [\"assets/\"]\n",
    )
    .unwrap();
    std::fs::create_dir_all(root.join("a")).unwrap();
    std::fs::write(
        root.join("a/Cargo.toml"),
        "[package]\nname = \"a\"\ninclude = [\"src/**\", \"build.rs\"]\n",
    )
    .unwrap();
    std::fs::create_dir_all(root.join("b")).unwrap();
    std::fs::write(
        root.join("b/Cargo.toml"),
        "[package]\nname = \"b\"\nexclude.workspace = true\n",
    )
    .unwrap();
    assert_eq!(
        include_exclude(&root.join("a")),
        (vec!["src/**".into(), "build.rs".into()], vec![])
    );
    assert_eq!(include_exclude(&root.join("b")), (vec![], vec!["assets/".to_owned()]));
    assert_eq!(include_exclude(&root.join("none")), (vec![], vec![]));
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn package_file_rules_can_be_inherited_by_the_root_or_from_an_explicit_workspace() {
    let root = dir("include-root");
    std::fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = \"root\"\ninclude.workspace = true\nexclude.workspace = true\n\
         [workspace]\n[workspace.package]\ninclude = [\"src/**\", \"build.rs\"]\nexclude = [\"assets/\"]\n",
    )
    .unwrap();
    assert_eq!(
        include_exclude(&root),
        (vec!["src/**".into(), "build.rs".into()], vec!["assets/".into()])
    );
    let member = root.join("member");
    let workspace = root.join("elsewhere");
    std::fs::create_dir_all(&member).unwrap();
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::write(
        member.join("Cargo.toml"),
        "[package]\nname = \"member\"\nworkspace = \"../elsewhere\"\ninclude.workspace = true\n",
    )
    .unwrap();
    std::fs::write(
        workspace.join("Cargo.toml"),
        "[workspace]\n[workspace.package]\ninclude = [\"input/**\"]\n",
    )
    .unwrap();
    // The explicit workspace wins over the one in an ancestor directory.
    assert_eq!(include_exclude(&member), (vec!["input/**".into()], vec![]));
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn another_compiler_build_or_in_place_toolchain_replacement_invalidates_scripts() {
    let root = dir("compiler");
    std::fs::create_dir_all(root.join("lib")).unwrap();
    std::fs::write(root.join("lib/librustc_driver-before.so"), "").unwrap();
    let before = compiler_identity(&root, "sources-before");
    assert_eq!(before, compiler_identity(&root, "sources-before"));
    assert_ne!(before, compiler_identity(&root, "sources-after"));
    std::fs::remove_file(root.join("lib/librustc_driver-before.so")).unwrap();
    std::fs::write(root.join("lib/librustc_driver-after.so"), "").unwrap();
    assert_ne!(before, compiler_identity(&root, "sources-before"));
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn the_flags_for_rustc_are_cargos_first_of_the_variables_and_the_configuration() {
    let root = dir("flags");
    std::fs::create_dir_all(root.join(".cargo")).unwrap();
    std::fs::write(
        root.join(".cargo/config.toml"),
        "[build]\nrustflags = [\"--cfg\", \"build\"]\n[target.x86_64-unknown-linux-gnu]\nrustflags = \"--cfg target\"\n",
    )
    .unwrap();
    let flags = |env: &[(&str, &str)], target: &str| {
        let mut env: BTreeMap<String, String> =
            env.iter().map(|(k, v)| ((*k).to_owned(), (*v).to_owned())).collect();
        // A home of its own: no configuration of the machine's is read.
        env.insert("CARGO_HOME".to_owned(), root.join("home").display().to_string());
        env.insert("HOME".to_owned(), root.join("home").display().to_string());
        let config = Config::load(&root, env.clone(), &[]).unwrap();
        rustflags(&config, target, &[]).unwrap()
    };
    assert_eq!(flags(&[], "aarch64-unknown-linux-gnu"), ["--cfg", "build"]);
    assert_eq!(flags(&[], "x86_64-unknown-linux-gnu"), ["--cfg", "target"]);
    assert_eq!(
        flags(&[("RUSTFLAGS", "--cfg a  -Cb")], "x86_64-unknown-linux-gnu"),
        ["--cfg", "a", "-Cb"]
    );
    assert_eq!(
        flags(&[("RUSTFLAGS", "--cfg a"), ("CARGO_ENCODED_RUSTFLAGS", "--cfg\u{1f}b c")], "x"),
        ["--cfg", "b c"]
    );
    assert!(flags(&[("CARGO_ENCODED_RUSTFLAGS", "")], "x").is_empty());
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn matching_target_cfg_flags_are_combined_with_the_triple_and_override_build_flags() {
    let root = dir("cfg-flags");
    std::fs::create_dir_all(root.join(".cargo")).unwrap();
    std::fs::write(
        root.join(".cargo/config.toml"),
        "[build]\nrustflags = [\"--cfg\", \"build\"]\n\
         [target.x86_64-unknown-linux-gnu]\nrustflags = [\"--cfg\", \"triple\"]\n\
         [target.'cfg(unix)']\nrustflags = [\"--cfg\", \"unix_only\"]\n\
         [target.'cfg(windows)']\nrustflags = [\"--cfg\", \"windows_only\"]\n",
    )
    .unwrap();
    let unix = [cfg::CfgAtom::Flag(intern::Symbol::intern("unix"))];
    let flags = |target: &str, override_flags: Option<&str>| {
        let mut env = BTreeMap::from([
            ("CARGO_HOME".to_owned(), root.join("home").display().to_string()),
            ("HOME".to_owned(), root.join("home").display().to_string()),
        ]);
        if let Some(flags) = override_flags {
            env.insert("RUSTFLAGS".to_owned(), flags.to_owned());
        }
        let config = Config::load(&root, env, &[]).unwrap();
        rustflags(&config, target, &unix).unwrap()
    };
    assert_eq!(flags("x86_64-unknown-linux-gnu", None), ["--cfg", "triple", "--cfg", "unix_only"]);
    assert_eq!(flags("aarch64-unknown-linux-gnu", None), ["--cfg", "unix_only"]);
    assert_eq!(
        flags("x86_64-unknown-linux-gnu", Some("--cfg override_flags")),
        ["--cfg", "override_flags"]
    );
    assert!(flags("x86_64-unknown-linux-gnu", Some("")).is_empty());
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_build_dependencys_script_uses_the_host_even_when_the_package_is_also_for_the_target() {
    use std::ffi::OsStr;

    let root = dir("host-script");
    std::fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = \"native\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\
         [lib]\npath = \"lib.rs\"\n[workspace]\n\
         [profile.dev]\nopt-level = 1\n[profile.dev.build-override]\nopt-level = 0\n",
    )
    .unwrap();
    std::fs::write(root.join("lib.rs"), "").unwrap();
    std::fs::write(root.join("build.rs"), "fn main() {}\n").unwrap();
    let env = BTreeMap::from([
        ("CARGO_HOME".to_owned(), root.join("home").display().to_string()),
        ("RUSTFLAGS".to_owned(), "--cfg target_flag".to_owned()),
        ("SDKROOT".to_owned(), root.join("unused-sdk").display().to_string()),
    ]);
    // Read a real package through the manifest reader, without invoking cargo or rustc.
    let config = Config::load(&root, env.clone(), &[]).unwrap();
    let cx = blaze_manifest::Context::new(config);
    let manifest = root.join("Cargo.toml");
    let ws = blaze_manifest::Workspace::new(&manifest, &cx).unwrap();
    let metadata = serde_json::to_value(blaze_manifest::metadata::export_no_deps(&ws)).unwrap();
    let metadata = serde_json::from_value(metadata).unwrap();
    let manifest =
        project_model::ManifestPath::try_from(paths::AbsPathBuf::assert_utf8(manifest)).unwrap();
    let cargo = CargoWorkspace::new(metadata, manifest, ide_db::base_db::Env::default(), false);
    let package = cargo.packages().next().unwrap();
    let packages = FxHashMap::from_iter([(cargo[package].id.repr.clone(), true)]);
    let asked = Asked {
        env: &env,
        build: 1,
        profile: "dev",
        config: &[],
        host: "x86_64-unknown-linux-gnu",
        target: "aarch64-unknown-linux-gnu",
        explicit_target: true,
        packages: Some(&packages),
        host_cargo: None,
    };
    // The installed toolchain's identity selects checked-in cfg answers. No executable is
    // present in this fixture, so obtaining host cfgs cannot rely on a compiler process.
    let sysroot = root.join("toolchain");
    let rustlib = sysroot.join("lib/rustlib");
    std::fs::create_dir_all(&rustlib).unwrap();
    std::fs::write(sysroot.join("lib/librustc_driver-test.so"), "").unwrap();
    std::fs::write(rustlib.join(format!("manifest-rustc-{}", asked.host)), "").unwrap();
    let answers: serde_json::Value =
        serde_json::from_str(include_str!("../answers/rustc-1.99.0.json")).unwrap();
    std::fs::write(
        rustlib.join("multirust-channel-manifest.toml"),
        format!("\n[pkg.rustc]\nversion = {}\n", answers["version"]),
    )
    .unwrap();
    let target_cfg = [
        cfg::CfgAtom::Flag(intern::Symbol::intern("unix")),
        cfg::CfgAtom::KeyValue {
            key: intern::Symbol::intern("target_arch"),
            value: intern::Symbol::intern("aarch64"),
        },
    ];
    let leases = Leases::default();
    #[cfg(unix)]
    let _native = {
        use std::os::unix::ffi::OsStringExt as _;
        crate::process::enter_native(crate::process::Native::of([
            ("BYTE_VALUE".into(), OsString::from_vec(vec![0xff, 0xfe])),
            ("CLIENT_ONLY".into(), "request".into()),
        ]))
    };
    let mut runner = Runner::new(&leases, &cargo, &asked, sysroot, &target_cfg).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt as _;
        let child = runner.child_env();
        assert_eq!(child[std::ffi::OsStr::new("BYTE_VALUE")].as_bytes(), &[0xff, 0xfe]);
        assert_eq!(child[std::ffi::OsStr::new("CLIENT_ONLY")], "request");
        assert_eq!(child[std::ffi::OsStr::new("SDKROOT")], root.join("unused-sdk").as_os_str());
    }
    let profile = runner.profile(package, UnitFor::new_normal(), &runner.kind(false));
    let target = runner.unit(package, false, &profile, root.join("script"), Vec::new()).unwrap();
    let profile = runner.profile(package, UnitFor::new_host(false), &runner.kind(true));
    let host = runner.unit(package, true, &profile, root.join("script"), Vec::new()).unwrap();
    assert_ne!(host.out_dir, target.out_dir, "host and target runs have separate records");
    assert_eq!(target.profile.opt_level, "1");
    assert_eq!(host.profile.opt_level, "0");
    let host_env = blaze_build_script::env(&host, &[]).unwrap();
    let target_env = blaze_build_script::env(&target, &[]).unwrap();
    assert_eq!(host_env.get("TARGET"), Some(OsStr::new(asked.host)));
    assert_eq!(target_env.get("TARGET"), Some(OsStr::new(asked.target)));
    assert_eq!(host_env.get("CARGO_CFG_TARGET_ARCH"), Some(OsStr::new("x86_64")));
    assert_eq!(target_env.get("CARGO_CFG_TARGET_ARCH"), Some(OsStr::new("aarch64")));
    assert_eq!(host_env.get("CARGO_ENCODED_RUSTFLAGS"), Some(OsStr::new("")));
    assert_eq!(target_env.get("CARGO_ENCODED_RUSTFLAGS"), Some(OsStr::new("--cfg\x1ftarget_flag")));
    // A package used on both sides must not reuse the target's cfg/env/OUT_DIR when it is
    // linked into another script. Seed the two completed runs so this tests that routing
    // without compiling or executing a script.
    for (for_host, unit, marker) in [(false, &target, "target"), (true, &host, "host")] {
        let output = blaze_build_script::Output {
            cfgs: vec![format!("{marker}_script")],
            env: vec![("SCRIPT_CONTEXT".to_owned(), marker.to_owned())],
            ..blaze_build_script::Output::default()
        };
        runner.done.insert(
            (package, for_host),
            Ok(Arc::new(Done {
                out_dir: unit.out_dir.clone(),
                output,
                fingerprint: 0,
                generation: 0,
                ran: false,
                made: None,
                links: None,
            })),
        );
    }
    let mut crates = Vec::new();
    let index = runner.add(package, &mut crates, &mut FxHashMap::default()).unwrap();
    assert!(crates[index].cfg.contains(&"host_script".to_owned()));
    assert!(!crates[index].cfg.contains(&"target_script".to_owned()));
    assert_eq!(crates[index].env["SCRIPT_CONTEXT"], "host");
    assert_eq!(crates[index].env["OUT_DIR"], host.out_dir.display().to_string());
    let _ = std::fs::remove_dir_all(&root);
}

/// A package whose build script is a shell script that counts its runs and writes what it is
/// told to generate (`GEN`), so that what a build reads of a run can be compared with what the
/// build asked for. Its requests are the judgment of `Runner` as `run` makes it, over the real
/// run, record and lease files; only compiling the script is left out, as the other tests do.
#[cfg(unix)]
struct Fixture {
    root: PathBuf,
    cargo: CargoWorkspace,
    package: Package,
    sysroot: PathBuf,
    cfg: Vec<cfg::CfgAtom>,
}

#[cfg(unix)]
const HOST: &str = "x86_64-unknown-linux-gnu";

#[cfg(unix)]
const SCRIPT: &str = r#"#!/bin/sh
echo "$GEN" >> "$COUNTER"
if [ "$FAIL" = 1 ]; then echo "script failed for $GEN" >&2; exit 3; fi
printf '%s' "$GEN" > "$OUT_DIR/gen.txt"
echo "cargo:rerun-if-env-changed=GEN"
if [ "$ALWAYS" = 1 ]; then echo "cargo:rerun-if-changed=missing.txt"; fi
echo "cargo:rustc-cfg=gen_$GEN"
echo "cargo:rustc-env=GEN_VALUE=$GEN"
echo "cargo:rustc-link-search=native=$OUT_DIR"
echo "cargo:rustc-link-lib=static=gen_$GEN"
"#;

#[cfg(unix)]
impl Fixture {
    /// The package in `base/pkg`, made if it is not there; the counter is beside it, where
    /// nothing watches it.
    fn at(base: &Path) -> Fixture {
        use std::os::unix::fs::PermissionsExt as _;

        let root = base.join("pkg");
        let write = |path: PathBuf, text: &str| {
            if !path.exists() {
                std::fs::create_dir_all(path.parent().unwrap()).unwrap();
                std::fs::write(path, text).unwrap();
            }
        };
        write(
            root.join("Cargo.toml"),
            "[package]\nname = \"gen\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\
             [lib]\npath = \"lib.rs\"\n[workspace]\n",
        );
        write(root.join("lib.rs"), "");
        write(root.join("build.rs"), "fn main() {}\n");
        write(root.join("script.sh"), SCRIPT);
        std::fs::set_permissions(root.join("script.sh"), std::fs::Permissions::from_mode(0o755))
            .unwrap();
        let env =
            BTreeMap::from([("CARGO_HOME".to_owned(), base.join("home").display().to_string())]);
        let config = Config::load(&root, env, &[]).unwrap();
        let cx = blaze_manifest::Context::new(config);
        let manifest = root.join("Cargo.toml");
        let ws = blaze_manifest::Workspace::new(&manifest, &cx).unwrap();
        let metadata = serde_json::to_value(blaze_manifest::metadata::export_no_deps(&ws)).unwrap();
        let metadata = serde_json::from_value(metadata).unwrap();
        let manifest =
            project_model::ManifestPath::try_from(paths::AbsPathBuf::assert_utf8(manifest))
                .unwrap();
        let cargo = CargoWorkspace::new(metadata, manifest, ide_db::base_db::Env::default(), false);
        let package = cargo.packages().next().unwrap();
        // The installed toolchain's identity selects checked-in cfg answers; no compiler runs.
        let sysroot = base.join("toolchain");
        let rustlib = sysroot.join("lib/rustlib");
        std::fs::create_dir_all(&rustlib).unwrap();
        write(sysroot.join("lib/librustc_driver-test.so"), "");
        write(rustlib.join(format!("manifest-rustc-{HOST}")), "");
        let answers: serde_json::Value =
            serde_json::from_str(include_str!("../answers/rustc-1.99.0.json")).unwrap();
        write(
            rustlib.join("multirust-channel-manifest.toml"),
            &format!("\n[pkg.rustc]\nversion = {}\n", answers["version"]),
        );
        let cfg = vec![cfg::CfgAtom::Flag(intern::Symbol::intern("unix"))];
        Fixture { root, cargo, package, sysroot, cfg }
    }

    fn counter(&self) -> PathBuf {
        self.root.parent().unwrap().join("counter.log")
    }

    /// How many times the script has run, and for which value of `GEN`.
    fn runs(&self) -> Vec<String> {
        let counted = std::fs::read_to_string(self.counter()).unwrap_or_default();
        counted.lines().map(str::to_owned).collect()
    }

    fn generated(&self, done: &Done) -> String {
        std::fs::read_to_string(done.out_dir.join("gen.txt")).unwrap_or_default()
    }

    /// One request of `build` to a worker with `leases`, as `run` makes it for the package's
    /// script: what the script printed, as cargo reads it.
    fn request(
        &self,
        leases: &Leases,
        build: u64,
        gen_: &str,
        extra: &[(&str, &str)],
    ) -> Result<Arc<Done>, Attempt> {
        let mut env = BTreeMap::from([
            (
                "CARGO_HOME".to_owned(),
                self.root.parent().unwrap().join("home").display().to_string(),
            ),
            ("COUNTER".to_owned(), self.counter().display().to_string()),
            ("GEN".to_owned(), gen_.to_owned()),
        ]);
        env.extend(extra.iter().map(|(k, v)| ((*k).to_owned(), (*v).to_owned())));
        let id = self.cargo[self.package].id.repr.clone();
        let packages = FxHashMap::from_iter([(id, true)]);
        let asked = Asked {
            env: &env,
            build,
            profile: "dev",
            config: &[],
            host: HOST,
            target: HOST,
            explicit_target: false,
            packages: Some(&packages),
            host_cargo: None,
        };
        let mut runner =
            Runner::new(leases, &self.cargo, &asked, self.sysroot.clone(), &self.cfg).unwrap();
        let unit_for = UnitFor::new_normal();
        let profile = runner.profile(self.package, unit_for, &runner.kind(false));
        let script = self.root.join("script.sh");
        let made = std::fs::metadata(&script).and_then(|it| it.modified()).unwrap();
        let said = (|| {
            // As `Runner::make` starts: what the build judged already, else the admission.
            if let Some(judged) = runner.begin(self.package, false, &profile)? {
                return Ok(judged);
            }
            let unit =
                runner.unit(self.package, false, &profile, script.clone(), Vec::new()).unwrap();
            runner.judge(self.package, unit, (1, made), &[])
        })();
        // As `attempt` ends: the admission is let go unless the build waits.
        match said {
            Ok(done) => {
                leases.release_admission(build);
                Ok(done)
            }
            Err(why) => {
                let attempt = Attempt::of(runner.busy.take(), why);
                if !matches!(attempt, Attempt::Busy(_)) {
                    leases.release_admission(build);
                }
                Err(attempt)
            }
        }
    }
}

/// The messages of an attempt that did not end: the build that holds the output, or the failure.
#[cfg(unix)]
fn why(attempt: Result<Arc<Done>, Attempt>) -> Result<String, String> {
    match attempt {
        Ok(_) => Ok("ran".to_owned()),
        Err(Attempt::Busy(Busy::Process { build, .. })) => Err(format!("busy {build}")),
        Err(Attempt::Busy(Busy::Elsewhere { path })) => Err(format!("waits {}", path.display())),
        Err(Attempt::Failed(error)) => Err(format!("{error:#}")),
    }
}

#[cfg(unix)]
#[test]
fn a_script_runs_once_in_each_build_whatever_the_other_builds_did_between_its_requests() {
    let base = dir("aba");
    let pkg = Fixture::at(&base);
    let leases = Leases::default();
    // An always-stale script, which cargo runs once in each build: A, then B, then A again.
    let always = [("ALWAYS", "1")];
    let a1 = pkg.request(&leases, 1, "a", &always).unwrap();
    assert_eq!(pkg.generated(&a1), "a");
    // The token a consumer of this output holds is the record the run left.
    assert_eq!(generation_of(a1.out_dir.parent().unwrap()), Some(a1.generation));
    assert_eq!(pkg.runs(), ["a"]);
    // B asks while A reads the output: it does not write over it and is put off.
    assert_eq!(why(pkg.request(&leases, 2, "b", &always)), Err("busy 1".to_owned()));
    assert_eq!(pkg.runs(), ["a"], "B did not run the script over A's output");
    assert_eq!(pkg.generated(&a1), "a");
    // A's next request is the same build: judged already, not run a second time.
    let a2 = pkg.request(&leases, 1, "a", &always).unwrap();
    assert_eq!(pkg.runs(), ["a"], "an always-stale script runs once in a build");
    assert_eq!(a2.output.cfgs, a1.output.cfgs);
    assert_eq!(a2.output.library_links, a1.output.library_links);
    assert_eq!(pkg.generated(&a2), "a");
    // A is over; B runs, once, and its later request does not.
    leases.retire(1);
    let b1 = pkg.request(&leases, 2, "b", &always).unwrap();
    let b2 = pkg.request(&leases, 2, "b", &always).unwrap();
    assert_eq!(pkg.runs(), ["a", "b"]);
    assert_eq!((pkg.generated(&b1), pkg.generated(&b2)), ("b".to_owned(), "b".to_owned()));
    assert_eq!(b2.output.cfgs, ["gen_b"]);
    assert_eq!(a1.output.cfgs, ["gen_a"]);
    leases.retire(2);
    assert!(leases.builds().is_empty());
    let _ = std::fs::remove_dir_all(&base);
}

#[cfg(unix)]
#[test]
fn a_script_that_failed_fails_each_request_of_its_build_alike_and_is_not_run_again() {
    let base = dir("aba-failed");
    let pkg = Fixture::at(&base);
    let leases = Leases::default();
    let (fails, works) = ([("ALWAYS", "1"), ("FAIL", "1")], [("ALWAYS", "1")]);
    let a1 = why(pkg.request(&leases, 1, "a", &fails)).unwrap_err();
    assert!(a1.contains("script failed for a") && a1.contains("exit"), "{a1}");
    assert_eq!(pkg.runs(), ["a"]);
    // B's request in between succeeds: a failure holds nothing, so B runs the script.
    let b1 = pkg.request(&leases, 2, "b", &works).unwrap();
    assert_eq!(pkg.generated(&b1), "b");
    // A's next requests fail with the same message, and run nothing.
    for _ in 0..2 {
        assert_eq!(why(pkg.request(&leases, 1, "a", &fails)), Err(a1.clone()));
    }
    assert_eq!(
        why(pkg.request(&leases, 1, "a", &works)),
        Err(a1.clone()),
        "the build's failure stands"
    );
    assert_eq!(pkg.runs(), ["a", "b"]);
    // And B's output is not A's failure.
    assert!(pkg.request(&leases, 2, "b", &works).is_ok());
    assert_eq!(pkg.runs(), ["a", "b"]);
    leases.retire(1);
    leases.retire(2);
    let _ = std::fs::remove_dir_all(&base);
}

#[cfg(unix)]
#[test]
fn a_judgment_that_stands_is_not_taken_for_the_output_another_worker_wrote_after_it() {
    let base = dir("elsewhere");
    let pkg = Fixture::at(&base);
    // Two workers over one target directory: separate leases, one set of files.
    let (first, second) = (Leases::default(), Leases::default());
    let a = pkg.request(&first, 1, "a", &[]).unwrap();
    assert_eq!((pkg.generated(&a), pkg.runs()), ("a".to_owned(), vec!["a".to_owned()]));
    // The other worker's build has another value of a variable the script watches, so it must
    // run the script, and cannot while the first worker's build reads the output.
    assert!(matches!(
        pkg.request(&second, 9, "b", &[]),
        Err(Attempt::Busy(Busy::Elsewhere { .. }))
    ));
    assert_eq!((pkg.generated(&a), pkg.runs()), ("a".to_owned(), vec!["a".to_owned()]));
    // The first build's next request is the same judgment, over the same output.
    let again = pkg.request(&first, 1, "a", &[]).unwrap();
    assert_eq!((pkg.generated(&again), pkg.runs().len()), ("a".to_owned(), 1));
    // The first build is over; the second runs, and the first's judgment is gone with the
    // build: a request that names it again is judged by the record, which is the second's.
    first.retire(1);
    let b = pkg.request(&second, 9, "b", &[]).unwrap();
    assert_eq!((pkg.generated(&b), pkg.runs().len()), ("b".to_owned(), 2));
    assert!(matches!(pkg.request(&first, 1, "a", &[]), Err(Attempt::Busy(Busy::Elsewhere { .. }))));
    assert_eq!(pkg.generated(&b), "b");
    second.retire(9);
    let a_again = pkg.request(&first, 1, "a", &[]).unwrap();
    assert_eq!((pkg.generated(&a_again), pkg.runs().len()), ("a".to_owned(), 3));
    assert_eq!(a_again.output.cfgs, ["gen_a"]);
    first.retire(1);
    let _ = std::fs::remove_dir_all(&base);
}

#[cfg(unix)]
#[test]
fn builds_that_find_the_script_fresh_read_its_output_together() {
    let base = dir("fresh-together");
    let pkg = Fixture::at(&base);
    let (first, second) = (Leases::default(), Leases::default());
    // An earlier build ran the script, and is over.
    let earlier = Leases::default();
    pkg.request(&earlier, 9, "a", &[]).unwrap();
    earlier.retire(9);
    pkg.request(&first, 1, "a", &[]).unwrap();
    // The same value: nothing is to run, so the second worker's build does not wait for the
    // first's.
    let b = pkg.request(&second, 2, "a", &[]).unwrap();
    assert_eq!((pkg.generated(&b), pkg.runs().len()), ("a".to_owned(), 1));
    // A build that must run it waits for both.
    let third = Leases::default();
    assert!(matches!(pkg.request(&third, 3, "c", &[]), Err(Attempt::Busy(Busy::Elsewhere { .. }))));
    first.retire(1);
    assert!(matches!(pkg.request(&third, 3, "c", &[]), Err(Attempt::Busy(Busy::Elsewhere { .. }))));
    second.retire(2);
    let c = pkg.request(&third, 3, "c", &[]).unwrap();
    assert_eq!((pkg.generated(&c), pkg.runs().len()), ("c".to_owned(), 2));
    let _ = std::fs::remove_dir_all(&base);
}

/// The other worker of [`two_workers_each_compile_and_link_the_output_of_their_own_build`]: a
/// process of its own, which asks for another value of `GEN` while the first worker's build
/// reads the output, and says what it found by its exit status.
#[cfg(unix)]
#[test]
fn the_second_worker_of_the_barrier_test() {
    let Some(base) = std::env::var_os("RA_JIT_LEASE_TEST_BASE").map(PathBuf::from) else { return };
    let pkg = Fixture::at(&base);
    let leases = Leases::default();
    std::fs::write(base.join("b-asks"), "").unwrap();
    // As `run` asks: put off for a worker's build, and asked again once it is over.
    let done = retrying(7, false, || pkg.request(&leases, 7, "b", &[("ALWAYS", "1")])).unwrap();
    std::fs::write(base.join("b-ran"), pkg.generated(&done)).unwrap();
    assert_eq!(done.output.cfgs, ["gen_b"]);
    assert_eq!(done.output.library_links, ["static=gen_b"]);
    assert_eq!(pkg.generated(&done), "b");
    leases.retire(7);
}

#[cfg(unix)]
#[test]
fn two_workers_each_compile_and_link_the_output_of_their_own_build() {
    let base = dir("barrier");
    let pkg = Fixture::at(&base);
    let leases = Leases::default();
    let always = [("ALWAYS", "1")];
    // Worker A: its script is done. Its compile and link have not begun.
    let a = pkg.request(&leases, 1, "a", &always).unwrap();
    // Worker B is a process of its own and wants another value of the same script's output.
    let mut b = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "scripts::tests::the_second_worker_of_the_barrier_test", "--nocapture"])
        .env("RA_JIT_LEASE_TEST_BASE", &base)
        .env("RA_JIT_SCRIPT_LEASE_WAIT_SECS", "60")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let began = std::time::Instant::now();
    while !base.join("b-asks").exists() {
        assert!(began.elapsed() < Duration::from_secs(60), "worker B did not start");
        std::thread::sleep(Duration::from_millis(20));
    }
    // B has asked and is held at the barrier: the output is A's, whatever B is generating.
    std::thread::sleep(Duration::from_millis(500));
    assert!(b.try_wait().unwrap().is_none(), "worker B ran the script over A's output");
    assert_eq!(pkg.runs(), ["a"]);
    // A resumes: its compile reads the source `include!` names, its link reads the libraries
    // and the paths it printed, all of them in the one output directory A's script wrote.
    assert_eq!(std::fs::read_to_string(a.out_dir.join("gen.txt")).unwrap(), "a");
    assert_eq!(a.output.cfgs, ["gen_a"]);
    assert_eq!(a.output.env, [("GEN_VALUE".to_owned(), "a".to_owned())]);
    assert_eq!(a.output.library_links, ["static=gen_a"]);
    // As cargo's `linked_paths` have it: the kind, then the path.
    let paths: Vec<_> =
        a.output.library_paths.iter().map(|it| it.path().display().to_string()).collect();
    assert_eq!(paths, [format!("native={}", a.out_dir.display())]);
    // A's build is over: the daemon retires it, and B proceeds.
    leases.retire(1);
    let status = wait_within(&mut b, Duration::from_secs(60));
    assert!(status.success(), "worker B found another output than its own: {status}");
    assert_eq!(pkg.runs(), ["a", "b"]);
    assert_eq!(std::fs::read_to_string(base.join("b-ran")).unwrap(), "b");
    let _ = std::fs::remove_dir_all(&base);
}

#[cfg(unix)]
fn wait_within(child: &mut std::process::Child, within: Duration) -> std::process::ExitStatus {
    let began = std::time::Instant::now();
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            return status;
        }
        if began.elapsed() > within {
            let _ = child.kill();
            panic!("the worker did not end in {within:?}");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[cfg(unix)]
#[test]
fn two_runs_of_one_fingerprint_are_two_generations_and_a_world_of_one_is_not_the_other() {
    let base = dir("generation");
    let pkg = Fixture::at(&base);
    let (first, second) = (Leases::default(), Leases::default());
    let always = [("ALWAYS", "1")];
    // A script that runs every build, over one fingerprint: the same inputs, the same watched
    // paths, two executions that may leave other output.
    let a = pkg.request(&first, 1, "a", &always).unwrap();
    let a_world = vec![(a.out_dir.parent().unwrap().to_owned(), a.generation)];
    first.retire(1);
    let b = pkg.request(&second, 2, "a", &always).unwrap();
    assert_eq!(a.fingerprint, b.fingerprint, "what the runs were made from is the same");
    assert_ne!(a.generation, b.generation, "and they are two runs");
    assert_eq!(generation_of(a.out_dir.parent().unwrap()), Some(b.generation));
    // A database loaded from the first run is not read with the second's output.
    let reader = anonymous();
    let stale = second.pin(reader, &a_world).unwrap_err();
    assert_eq!(stale.run_dir, a.out_dir.parent().unwrap());
    second.retire(2);
    let _ = std::fs::remove_dir_all(&base);
}

#[cfg(unix)]
#[test]
fn a_script_a_build_ran_is_run_once_in_it_whatever_another_build_asks_between_its_requests() {
    let base = dir("once");
    let pkg = Fixture::at(&base);
    let leases = Leases::default();
    let always = [("ALWAYS", "1")];
    pkg.request(&leases, 1, "a", &always).unwrap();
    assert_eq!(pkg.runs(), ["a"]);
    // The build is refused something and would yield: it ran a script, so it keeps what it holds
    // and the admission, and nothing waits for it.
    assert!(!leases.yield_world(1));
    // Another build wants to run the script: it is put off, and does not hold the admission
    // while it waits, so the first build's later requests do not wait for it.
    assert_eq!(why(pkg.request(&leases, 2, "b", &always)), Err("busy 1".to_owned()));
    let again = pkg.request(&leases, 1, "a", &always).unwrap();
    assert_eq!(pkg.runs(), ["a"], "an always-stale script runs once in a build");
    assert_eq!(pkg.generated(&again), "a");
    assert_eq!(why(pkg.request(&leases, 2, "b", &always)), Err("busy 1".to_owned()));
    assert_eq!(pkg.generated(&again), "a", "and nobody wrote its output");
    let last = pkg.request(&leases, 1, "a", &always).unwrap();
    assert_eq!((pkg.generated(&last), pkg.runs()), ("a".to_owned(), vec!["a".to_owned()]));
    // The build is over, and the other runs it, once.
    leases.retire(1);
    let b = pkg.request(&leases, 2, "b", &always).unwrap();
    assert_eq!(
        (pkg.generated(&b), pkg.runs()),
        ("b".to_owned(), vec!["a".to_owned(), "b".to_owned()])
    );
    leases.retire(2);
    let _ = std::fs::remove_dir_all(&base);
}

/// The sequence of the review, with the real runner: the first build's first request ran the
/// script and was answered, the second build waits to run it, and the first build has another
/// request. The script counts its runs: a second run of it in the first build is the defect.
#[cfg(unix)]
#[test]
fn a_build_that_ran_a_script_never_runs_it_again_in_a_later_request_with_another_build_waiting() {
    let base = dir("once-later");
    let pkg = Fixture::at(&base);
    let leases = Leases::default();
    let always = [("ALWAYS", "1")];
    let first = pkg.request(&leases, 1, "a", &always).unwrap();
    // The attempt ended and the second build comes: the first answered, and has more to judge.
    let mut waiting = 0;
    for _ in 0..3 {
        match pkg.request(&leases, 2, "b", &always) {
            Err(Attempt::Busy(Busy::Process { build: 1, .. })) => waiting += 1,
            other => panic!("the second build was not put off: {}", why(other).unwrap_err()),
        }
        // The first build's next request: the same judgment, the same output.
        let next = pkg.request(&leases, 1, "a", &always).unwrap();
        assert_eq!(next.generation, first.generation);
        assert_eq!(pkg.generated(&next), "a");
    }
    assert_eq!((waiting, pkg.runs()), (3, vec!["a".to_owned()]));
    leases.retire(1);
    assert!(pkg.request(&leases, 2, "b", &always).is_ok());
    assert_eq!(pkg.runs(), ["a", "b"]);
    leases.retire(2);
    let _ = std::fs::remove_dir_all(&base);
}

/// A build that only found the script fresh has not run it: when another build ran it meanwhile,
/// the first one's judging of it again is its first run, never a second.
#[cfg(unix)]
#[test]
fn a_build_that_found_a_script_fresh_runs_it_once_if_another_build_ran_it_while_it_waited() {
    let base = dir("once-fresh");
    let pkg = Fixture::at(&base);
    let leases = Leases::default();
    // An earlier build ran it for `a0`, and is over.
    pkg.request(&leases, 9, "a0", &[]).unwrap();
    leases.retire(9);
    // The build finds it fresh, holds the output, and is refused something: it yields.
    pkg.request(&leases, 1, "a0", &[]).unwrap();
    assert_eq!(pkg.runs(), ["a0"]);
    assert!(leases.yield_world(1));
    // Another build wants another value, and runs.
    pkg.request(&leases, 2, "b", &[]).unwrap();
    leases.retire(2);
    assert_eq!(pkg.runs(), ["a0", "b"]);
    // The first build's script is to run: the output is not the one it found, and it ran nothing.
    let a = pkg.request(&leases, 1, "a0", &[]).unwrap();
    assert_eq!(pkg.generated(&a), "a0");
    assert_eq!(pkg.runs(), ["a0", "b", "a0"]);
    // And now it ran, and keeps it.
    assert!(!leases.yield_world(1));
    leases.retire(1);
    let _ = std::fs::remove_dir_all(&base);
}

#[cfg(unix)]
#[test]
fn a_failure_is_the_builds_for_good_whether_it_yielded_or_not() {
    let base = dir("failed-yield");
    let pkg = Fixture::at(&base);
    let leases = Leases::default();
    let fails = [("ALWAYS", "1"), ("FAIL", "1")];
    let first = why(pkg.request(&leases, 1, "a", &fails)).unwrap_err();
    assert_eq!(pkg.runs(), ["a"]);
    // The build holds nothing of a failure; it yields what it has, and another build runs.
    leases.yield_world(1);
    assert!(pkg.request(&leases, 2, "b", &[("ALWAYS", "1")]).is_ok());
    assert_eq!(why(pkg.request(&leases, 1, "a", &fails)), Err(first));
    assert_eq!(pkg.runs(), ["a", "b"], "the failed script is not run again for the build");
    leases.retire(2);
    leases.retire(1);
    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn the_flags_for_rustc_a_crate_is_compiled_with_are_applied_where_they_change_the_crate() {
    let flags = |it: &[&str]| it.iter().map(|it| (*it).to_owned()).collect::<Vec<_>>();
    let applied =
        apply_flags(&flags(&["--cfg", "hostflag", "--cfg=other", "--cfg", "feature=\"x\""]));
    assert_eq!(applied.cfgs, ["hostflag", "other", "feature=\"x\""]);
    assert!(applied.ignored.is_empty() && applied.debug_assertions.is_none());
    // `-C debug-assertions`, joined or not, with its values.
    for (given, want) in [
        (&["-C", "debug-assertions=no"][..], false),
        (&["-Cdebug-assertions=off"], false),
        (&["-C", "debug-assertions=yes"], true),
        (&["-Cdebug-assertions"], true),
    ] {
        assert_eq!(apply_flags(&flags(given)).debug_assertions, Some(want), "{given:?}");
    }
    // What changes no crate is named and not applied, and it does not take the next flag with it.
    let applied = apply_flags(&flags(&[
        "-C",
        "opt-level=3",
        "--check-cfg",
        "cfg(x)",
        "-Wunused",
        "--cfg",
        "kept",
    ]));
    assert_eq!(applied.cfgs, ["kept"]);
    assert_eq!(applied.ignored, ["-Wunused"]);
    // A flag with no value says so.
    assert_eq!(apply_flags(&flags(&["--cfg"])).ignored, ["--cfg has no value"]);
}

#[test]
fn host_overflow_and_panic_flags_change_the_effective_script_profile() {
    let profile = blaze_manifest::profiles::Profile::default();
    let flags = |words: &[&str]| words.iter().map(|it| (*it).to_owned()).collect::<Vec<_>>();
    let asked = flags(&["-C", "overflow_checks=yes", "-Cpanic=abort"]);
    let effective = crate::plan::Profile::for_script(&profile, &asked).unwrap();
    assert!(effective.overflow_checks);
    assert_eq!(effective.panic.name(), "abort");
    let applied = apply_flags(&asked);
    assert_eq!(applied.overflow_checks, Some(true));
    assert_eq!(applied.panic.as_deref(), Some("abort"));
    assert!(applied.ignored.is_empty());
    let asked = flags(&["-Coverflow-checks=off", "-C", "panic=unwind"]);
    let effective = crate::plan::Profile::for_script(&profile, &asked).unwrap();
    assert!(!effective.overflow_checks);
    assert_eq!(effective.panic.name(), "unwind");
}

#[test]
fn producer_compiler_cfg_preserves_host_facts_and_effective_profile() {
    let base = [
        "unix",
        "proc_macro",
        "target_arch=\"x86_64\"",
        "target_pointer_width=\"64\"",
        "debug_assertions",
        "overflow_checks",
        "panic=\"unwind\"",
    ]
    .map(str::to_owned);
    let flags = [
        "-Cdebug-assertions=off",
        "-Coverflow-checks=off",
        "-Cpanic=abort",
        "--cfg",
        "custom_host",
    ]
    .map(str::to_owned);
    let profile =
        crate::plan::Profile::for_script(&blaze_manifest::profiles::Profile::default(), &flags)
            .unwrap();
    let cfg =
        source_cfgs(&base, &["feature=\"selected\"".into()], &profile, &apply_flags(&flags).cfgs);
    for fact in [
        "unix",
        "proc_macro",
        "target_arch=\"x86_64\"",
        "target_pointer_width=\"64\"",
        "feature=\"selected\"",
        "custom_host",
        "panic=\"abort\"",
    ] {
        assert!(cfg.iter().any(|it| it == fact), "missing {fact}: {cfg:?}");
    }
    for absent in ["debug_assertions", "overflow_checks", "panic=\"unwind\""] {
        assert!(!cfg.iter().any(|it| it == absent), "stale profile cfg {absent}: {cfg:?}");
    }
    let flags =
        ["-Cdebug-assertions=on", "-Coverflow-checks=on", "-Cpanic=unwind"].map(str::to_owned);
    let profile =
        crate::plan::Profile::for_script(&blaze_manifest::profiles::Profile::default(), &flags)
            .unwrap();
    let cfg = source_cfgs(&["unix".into(), "panic=\"abort\"".into()], &[], &profile, &[]);
    for fact in ["unix", "debug_assertions", "overflow_checks", "panic=\"unwind\""] {
        assert!(cfg.iter().any(|it| it == fact));
    }
    assert!(!cfg.iter().any(|it| it == "panic=\"abort\""));
}
