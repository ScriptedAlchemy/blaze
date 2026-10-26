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
        packages: Some(&packages),
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
    let mut runner = Runner::new(&cargo, &asked, sysroot, &target_cfg).unwrap();
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
            Ok(std::rc::Rc::new(Done {
                out_dir: unit.out_dir.clone(),
                output,
                fingerprint: 0,
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
