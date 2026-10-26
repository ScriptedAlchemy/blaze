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
