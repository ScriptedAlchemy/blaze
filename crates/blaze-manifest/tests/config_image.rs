use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use blaze_manifest::Config;
use blaze_manifest::config::{ConfigImage, ConfigRelativePath, Discovery};

struct Tree {
    root: PathBuf,
}

impl Tree {
    fn new() -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let root = std::env::temp_dir().join(format!(
            "blaze-config-image-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(root.join("home")).unwrap();
        Self { root: root.canonicalize().unwrap() }
    }

    fn write(&self, path: &str, bytes: impl AsRef<[u8]>) {
        let path = self.root.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, bytes).unwrap();
    }

    fn env(&self) -> BTreeMap<String, String> {
        BTreeMap::from([("__CARGO_TEST_ROOT".to_owned(), self.root.to_str().unwrap().to_owned())])
    }

    fn live(&self, cwd: &Path, cli: &[String]) -> anyhow::Result<Config> {
        Config::load_with_home(cwd, self.env(), cli, self.root.join("home"))
    }

    fn image(&self, layers: &[&str], files: &[(&str, &[u8])]) -> ConfigImage {
        ConfigImage {
            layers: layers.iter().map(|path| self.root.join(path)).collect(),
            files: files
                .iter()
                .map(|(path, bytes)| (self.root.join(path), bytes.to_vec()))
                .collect(),
        }
    }

    fn replay(&self, cwd: &Path, cli: &[String], image: ConfigImage) -> anyhow::Result<Config> {
        Config::from_image(cwd, self.root.join("home"), self.env(), cli, image)
    }
}

impl Drop for Tree {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

#[test]
fn captured_image_matches_live_layers_cli_includes_and_relative_env() {
    let tree = Tree::new();
    let files = [
        ("home/config.toml", "[build]\njobs=1\nrustflags=['home']\n[env]\nHOME_ONLY='home'\n"),
        (".cargo/config.toml", "[build]\njobs=2\nrustflags=['root']\n[env]\nROOT_ONLY='root'\n"),
        (
            "work/.cargo/config.toml",
            "[build]\njobs=3\nrustflags=['work']\n[env]\nWORK_ONLY='work'\n",
        ),
        (
            "work/member/.cargo/config.toml",
            "include=['parts/base.toml', {path='optional.toml',optional=true}]\r\n\
             [build]\r\njobs=4\r\nrustflags=['member']\r\ntarget-dir='out'\r\n\
             [env]\r\nMEMBER_REL={value='data',relative=true,force=true}\r\n\
             FORCED={value='configured',force=true}\r\nKEPT='configured'\r\n",
        ),
        (
            "work/member/.cargo/parts/base.toml",
            "include=['nested/deep.toml']\n[build]\nrustflags=['included']\n\
             [env]\nINCLUDE_REL={value='inc-data',relative=true}\n",
        ),
        (
            "work/member/.cargo/parts/nested/deep.toml",
            "[env]\nDEEP_REL={value='deep-data',relative=true}\n",
        ),
        (
            "work/member/opts/cli.toml",
            "include=['sub/defaults.toml']\n[build]\njobs=5\nrustflags=['cli-file']\n\
             [env]\nCLI_FILE_REL={value='cli-data',relative=true}\n",
        ),
        (
            "work/member/opts/sub/defaults.toml",
            "[build]\nrustflags=['cli-include']\n\
             [env]\nCLI_INCLUDE_REL={value='cli-inc-data',relative=true}\n",
        ),
        (
            "work/member/inline.toml",
            "[build]\nrustflags=['inline-include']\n\
             [env]\nINLINE_INCLUDE_REL={value='inline-inc-data',relative=true}\n",
        ),
    ];
    for (path, bytes) in files {
        tree.write(path, bytes);
    }
    let cwd = tree.root.join("work/member");
    let cli = [
        "opts/cli.toml",
        "include=['inline.toml']",
        "build.jobs=6",
        "build.rustflags=['inline']",
        "env.INLINE_REL.value='inline-data'",
        "env.INLINE_REL.relative=true",
    ]
    .map(str::to_owned);
    let mut env = tree.env();
    env.extend([
        ("CARGO_BUILD_JOBS".to_owned(), "7".to_owned()),
        ("FORCED".to_owned(), "ambient".to_owned()),
        ("KEPT".to_owned(), "ambient".to_owned()),
        ("MEMBER_REL".to_owned(), "ambient".to_owned()),
    ]);
    let live = Config::load_with_home(&cwd, env.clone(), &cli, tree.root.join("home")).unwrap();
    let layers = [
        "work/member/.cargo/config.toml",
        "work/.cargo/config.toml",
        ".cargo/config.toml",
        "home/config.toml",
    ];
    let image = ConfigImage {
        layers: layers.map(|path| tree.root.join(path)).to_vec(),
        files: live.sources().into_iter().collect(),
    };
    for (path, _) in files {
        tree.write(path, "[poisoned");
    }
    tree.write("work/member/.cargo/optional.toml", "[poisoned");
    let replay = Config::from_image(&cwd, tree.root.join("home"), env, &cli, image).unwrap();
    for config in [&live, &replay] {
        assert_eq!(config.get::<u32>("build.jobs").unwrap(), 6);
        assert_eq!(
            config.get::<Vec<String>>("build.rustflags").unwrap(),
            [
                "home",
                "root",
                "work",
                "included",
                "member",
                "cli-include",
                "cli-file",
                "inline-include",
                "inline",
            ]
        );
        assert_eq!(config.target_dir().unwrap(), Some(cwd.join("out")));
        let values = config.env_config().unwrap();
        assert_eq!(values["HOME_ONLY"], OsString::from("home"));
        assert_eq!(values["ROOT_ONLY"], OsString::from("root"));
        assert_eq!(values["WORK_ONLY"], OsString::from("work"));
        assert_eq!(values["FORCED"], OsString::from("configured"));
        assert!(!values.contains_key("KEPT"));
        for (name, path) in [
            ("MEMBER_REL", cwd.join("data")),
            ("INCLUDE_REL", cwd.join(".cargo/inc-data")),
            ("DEEP_REL", cwd.join(".cargo/parts/deep-data")),
            ("CLI_FILE_REL", cwd.join("cli-data")),
            ("CLI_INCLUDE_REL", cwd.join("opts/cli-inc-data")),
            ("INLINE_INCLUDE_REL", tree.root.join("work/inline-inc-data")),
            ("INLINE_REL", cwd.join("inline-data")),
        ] {
            assert_eq!(values[name], path.into_os_string(), "{name}");
        }
        let cli_origin = config.get_string("env.CLI_INCLUDE_REL.value").unwrap().unwrap();
        assert_eq!(
            cli_origin.definition.to_string(),
            cwd.join("opts/sub/defaults.toml").display().to_string()
        );
    }
    assert_eq!(replay.env_config().unwrap(), live.env_config().unwrap());
    assert_eq!(replay.sources(), live.sources());
    let member_bytes = replay
        .sources()
        .into_iter()
        .find(|(path, _)| path == &cwd.join(".cargo/config.toml"))
        .unwrap()
        .1;
    assert_eq!(member_bytes, files[3].1.as_bytes());
    let unmerged = replay.load_values_unmerged().unwrap();
    let origins: Vec<_> = unmerged.iter().map(|value| value.definition().to_string()).collect();
    assert_eq!(
        origins,
        [
            "work/member/.cargo/parts/nested/deep.toml",
            "work/member/.cargo/parts/base.toml",
            "work/member/.cargo/config.toml",
            "work/.cargo/config.toml",
            ".cargo/config.toml",
            "home/config.toml",
        ]
        .map(|path| tree.root.join(path).display().to_string())
    );
}

#[test]
fn layer_order_is_explicit_and_unreferenced_files_do_not_participate() {
    let tree = Tree::new();
    let image = tree.image(
        &["z/top.toml", "a/base.toml"],
        &[
            ("a/base.toml", b"[build]\njobs=1\nrustflags=['base']\n"),
            ("z/top.toml", b"[build]\njobs=2\nrustflags=['top']\n"),
            (".cargo/config.toml", b"[unreferenced-and-malformed"),
            ("home/config.toml", b"[unreferenced-and-malformed"),
        ],
    );
    let config = tree.replay(&tree.root, &[], image).unwrap();
    assert!(config.sources().is_empty());
    assert_eq!(config.get::<u32>("build.jobs").unwrap(), 2);
    assert_eq!(config.get::<Vec<String>>("build.rustflags").unwrap(), ["base", "top"]);
    assert_eq!(config.sources().len(), 2);
    let unmerged = config.load_values_unmerged().unwrap();
    let origins: Vec<_> = unmerged.iter().map(|value| value.definition().to_string()).collect();
    assert_eq!(
        origins,
        [tree.root.join("z/top.toml"), tree.root.join("a/base.toml")]
            .map(|path| path.display().to_string())
    );
}

#[test]
fn captured_environment_overrides_files_and_yields_to_cli() {
    let tree = Tree::new();
    let bytes = b"[build]\njobs=1\n";
    tree.write(".cargo/config.toml", bytes);
    let image = tree.image(&[".cargo/config.toml"], &[(".cargo/config.toml", bytes)]);
    let mut env = tree.env();
    env.insert("CARGO_BUILD_JOBS".to_owned(), "2".to_owned());
    for cli in [Vec::new(), vec!["build.jobs=3".to_owned()]] {
        let live =
            Config::load_with_home(&tree.root, env.clone(), &cli, tree.root.join("home")).unwrap();
        let replay = Config::from_image(
            &tree.root,
            tree.root.join("home"),
            env.clone(),
            &cli,
            image.clone(),
        )
        .unwrap();
        for config in [&live, &replay] {
            assert_eq!(
                config.get::<u32>("build.jobs").unwrap(),
                if cli.is_empty() { 2 } else { 3 }
            );
        }
    }
}

#[test]
fn empty_image_ignores_poisoned_cwd_parent_and_home_config() {
    let tree = Tree::new();
    for path in [".cargo/config.toml", "member/.cargo/config.toml", "home/config.toml"] {
        tree.write(path, "[poisoned");
    }
    let cwd = tree.root.join("member");
    assert!(tree.live(&cwd, &[]).unwrap().ensure_loaded().is_err());
    let mut config = Config::from_image(
        &cwd,
        tree.root.join("home"),
        BTreeMap::new(),
        &[],
        ConfigImage::default(),
    )
    .unwrap();
    assert_eq!(config.cwd(), cwd);
    assert_eq!(config.home(), tree.root.join("home"));
    config.ensure_loaded().unwrap();
    assert!(config.get_string("build.target-dir").unwrap().is_none());
    assert!(config.env_config().unwrap().is_empty());
    assert!(config.load_values_unmerged().unwrap().is_empty());
    config.start("build", &["build.jobs=9".to_owned()]).unwrap();
    assert_eq!(config.get::<u32>("build.jobs").unwrap(), 9);
    assert!(config.sources().is_empty());
}

#[test]
fn include_presence_comes_only_from_the_image() {
    let tree = Tree::new();
    tree.write(".cargo/optional.toml", "[poisoned");
    let mut image = tree.image(
        &[".cargo/config.toml"],
        &[
            (
                ".cargo/config.toml",
                b"include=[{path='optional.toml',optional=true},{path='supplied.toml',optional=true}]\n",
            ),
            (".cargo/supplied.toml", b"[build]\njobs=3\n"),
        ],
    );
    assert_eq!(
        tree.replay(&tree.root, &[], image.clone()).unwrap().get::<u32>("build.jobs").unwrap(),
        3
    );
    image
        .files
        .insert(tree.root.join(".cargo/config.toml"), b"include=['required.toml']\n".to_vec());
    tree.write(".cargo/required.toml", "[build]\njobs=99\n");
    let error = tree.replay(&tree.root, &[], image).unwrap().ensure_loaded().unwrap_err();
    let message = format!("{error:#}");
    assert!(message.contains("required.toml"));
    assert!(message.contains("absent from the supplied image"));
}

#[test]
fn cli_files_are_available_only_when_supplied_and_can_be_added_on_start() {
    let tree = Tree::new();
    tree.write("extra.toml", "[build]\njobs=99\n");
    let cli = ["extra.toml".to_owned()];
    assert!(tree.replay(&tree.root, &cli, ConfigImage::default()).is_err());
    let image = tree.image(&[], &[("extra.toml", b"[build]\njobs=4\n")]);
    let mut config = tree.replay(&tree.root, &[], image).unwrap();
    config.start("build", &cli).unwrap();
    assert_eq!(config.get::<u32>("build.jobs").unwrap(), 4);
    assert_eq!(config.sources(), [(tree.root.join("extra.toml"), b"[build]\njobs=4\n".to_vec())]);
    let error = tree
        .replay(&tree.root, &["registry.token='secret'".to_owned()], ConfigImage::default())
        .unwrap_err();
    assert!(error.to_string().contains("cannot be set through --config"));
}

#[test]
fn images_keep_live_parse_include_and_merge_errors() {
    for contents in [
        "[broken",
        "number=1.5",
        "include=1",
        "include=[{path='bad.toml',optional='yes'}]",
        "include=['bad.txt']",
        "include=['config.toml']",
    ] {
        let tree = Tree::new();
        tree.write(".cargo/config.toml", contents);
        let image =
            tree.image(&[".cargo/config.toml"], &[(".cargo/config.toml", contents.as_bytes())]);
        let live = tree.live(&tree.root, &[]).unwrap();
        let replay = tree.replay(&tree.root, &[], image.clone()).unwrap();
        assert_eq!(
            format!("{:#}", replay.ensure_loaded().unwrap_err()),
            format!("{:#}", live.ensure_loaded().unwrap_err()),
            "{contents}"
        );
        let cli = ["build.jobs=3".to_owned()];
        assert!(tree.live(&tree.root, &cli).is_err());
        assert!(tree.replay(&tree.root, &cli, image).is_err());
    }
    let tree = Tree::new();
    let files: [(&str, &[u8]); 2] =
        [(".cargo/config.toml", b"[build]\njobs=[]\n"), ("home/config.toml", b"[build]\njobs=3\n")];
    for (path, bytes) in files {
        tree.write(path, bytes);
    }
    let image = tree.image(&[".cargo/config.toml", "home/config.toml"], &files);
    assert_eq!(
        format!("{:#}", tree.replay(&tree.root, &[], image).unwrap().ensure_loaded().unwrap_err()),
        format!("{:#}", tree.live(&tree.root, &[]).unwrap().ensure_loaded().unwrap_err())
    );
}

#[test]
fn missing_layers_missing_includes_and_invalid_utf8_are_errors() {
    let tree = Tree::new();
    tree.write(".cargo/config.toml", "[build]\njobs=99\n");
    let missing = tree.image(&[".cargo/config.toml"], &[]);
    let error = tree.replay(&tree.root, &[], missing).unwrap().ensure_loaded().unwrap_err();
    assert!(format!("{error:#}").contains("absent from the supplied image"));
    for bytes in [b"include=['missing.toml']\n".as_slice(), b"\xff"] {
        tree.write(".cargo/config.toml", bytes);
        let image = tree.image(&[".cargo/config.toml"], &[(".cargo/config.toml", bytes)]);
        for config in
            [tree.live(&tree.root, &[]).unwrap(), tree.replay(&tree.root, &[], image).unwrap()]
        {
            let error = config.ensure_loaded().unwrap_err();
            let message = format!("{error:#}");
            assert!(message.contains("failed to read configuration file"));
            assert!(message.contains(if bytes == b"\xff" {
                "config.toml"
            } else {
                "missing.toml"
            }));
        }
    }
}

#[test]
fn relative_image_paths_rebase_origins_without_materializing_files() {
    let tree = Tree::new();
    let cwd = tree.root.join("unmaterialized/work");
    let image = ConfigImage {
        layers: vec![PathBuf::from(".cargo/config.toml")],
        files: BTreeMap::from([
            (
                PathBuf::from(".cargo/config.toml"),
                b"include=['parts/../included.toml']\n[build]\ntarget-dir='out'\n".to_vec(),
            ),
            (
                PathBuf::from(".cargo/included.toml"),
                b"[env]\nREL={value='data',relative=true}\n".to_vec(),
            ),
            (
                PathBuf::from("opts/cli.toml"),
                b"[env]\nCLI={value='cli-data',relative=true}\n".to_vec(),
            ),
        ]),
    };
    let config = tree.replay(&cwd, &["opts/./cli.toml".to_owned()], image).unwrap();
    assert!(!cwd.exists());
    assert_eq!(config.target_dir().unwrap(), Some(cwd.join("out")));
    assert_eq!(config.env_config().unwrap()["REL"], cwd.join("data").into_os_string());
    assert_eq!(config.env_config().unwrap()["CLI"], cwd.join("cli-data").into_os_string());
    let value: ConfigRelativePath = config.get("build.target-dir").unwrap();
    assert_eq!(
        value.value().definition.to_string(),
        cwd.join(".cargo/config.toml").display().to_string()
    );
    assert_eq!(value.resolve_path(&cwd), cwd.join("out"));
}

#[test]
fn conflicting_normalized_image_paths_are_rejected() {
    let tree = Tree::new();
    let image = ConfigImage {
        layers: Vec::new(),
        files: BTreeMap::from([
            (PathBuf::from("config.toml"), b"[build]\njobs=1\n".to_vec()),
            (tree.root.join("config.toml"), b"[build]\njobs=2\n".to_vec()),
        ]),
    };
    let error = tree.replay(&tree.root, &[], image).unwrap_err();
    assert!(error.to_string().contains("conflicting configuration image files"));
}

#[cfg(unix)]
#[test]
fn image_file_paths_retain_native_bytes() {
    use std::os::unix::ffi::OsStringExt;

    let tree = Tree::new();
    let path = tree.root.join(OsString::from_vec(b"config-\xff/settings.toml".to_vec()));
    let bytes = b"[build]\njobs=2\n".to_vec();
    let image = ConfigImage {
        layers: vec![path.clone()],
        files: BTreeMap::from([(path.clone(), bytes.clone())]),
    };
    let config = tree.replay(&tree.root, &[], image).unwrap();
    assert_eq!(config.get::<u32>("build.jobs").unwrap(), 2);
    assert_eq!(config.sources(), [(path, bytes)]);
}

#[test]
fn a_live_walk_reports_the_layers_that_replay_the_same_precedence() {
    let tree = Tree::new();
    // The Cargo home is an ancestor's `.cargo` too, and lies between two others: live discovery
    // visits it there and skips it at the end.
    tree.write(".cargo/config.toml", "[env]\nWHO='root'\nROOT_ONLY='root'\n");
    tree.write("work/.cargo/config.toml", "[env]\nWHO='home'\nHOME_ONLY='home'\n");
    let cwd = tree.root.join("work/member");
    std::fs::create_dir_all(&cwd).unwrap();
    let home = tree.root.join("work/.cargo");
    let live = Config::load_in(&cwd, tree.env(), &[], Some(home.clone()), Discovery::Live).unwrap();
    assert_eq!(live.env_config().unwrap()["WHO"], OsString::from("home"));
    let layers = live.layers();
    assert_eq!(
        layers,
        [tree.root.join("work/.cargo/config.toml"), tree.root.join(".cargo/config.toml")]
    );

    let image = ConfigImage { layers, files: live.sources().into_iter().collect() };
    // A tree that no longer has the files, or has others, cannot change what replays.
    tree.write(".cargo/config.toml", "[poisoned");
    tree.write("work/member/.cargo/config.toml", "[env]\nWHO='worker'\n");
    let replay =
        Config::load_in(&cwd, tree.env(), &[], Some(home), Discovery::Image(&image)).unwrap();
    assert_eq!(replay.env_config().unwrap(), live.env_config().unwrap());
    assert_eq!(replay.layers(), live.layers());
}
