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
        rustflags(&config, &env, target).unwrap()
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
