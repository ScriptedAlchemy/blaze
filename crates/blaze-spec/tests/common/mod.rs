//! A small workspace written to a temporary directory, a fake machine to read the platform
//! from, and hand-built unit graphs: what the tests of a spec are made of. Nothing here needs
//! the network or a lock file.

#![allow(dead_code, unreachable_pub)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use blaze_manifest::profiles::{Profile, Profiles, UnitFor};
use blaze_metadata::units::{Mode, TargetKind, Unit, UnitDep, UnitGraph, UnitPackage, UnitTarget};
use blaze_spec::Refusal;
use blaze_spec::assemble::{Context, Host, Spec, assemble};
use blaze_spec::flags::{Request, Subcommand};
use blaze_spec::platform::Probe;
use blaze_wire::{SourceRef, StoreIdentity};

/// A workspace of one member `a`, in a temporary directory that `.cargo/config.toml` and the
/// like can be written into.
pub struct Fixture {
    pub dir: tempfile::TempDir,
}

pub const ROOT_MANIFEST: &str = "[workspace]\nmembers = [\"a\", \"b\"]\nresolver = \"2\"\n";

pub const MEMBER: &str = "[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[features]\ndefault = []\nx = []\ny = []\n";

impl Fixture {
    /// The workspace with `root` as its root manifest.
    pub fn with_root(root: &str) -> Fixture {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let fixture = Fixture { dir };
        fixture.write("Cargo.toml", root);
        for name in ["a", "b"] {
            fixture.write(&format!("{name}/Cargo.toml"), &MEMBER.replace("{name}", name));
            fixture.write(&format!("{name}/src/lib.rs"), "");
        }
        fixture.write("Cargo.lock", "# a lock file\nversion = 4\n");
        fixture
    }

    pub fn new() -> Fixture {
        Fixture::with_root(ROOT_MANIFEST)
    }

    /// A root manifest of `ROOT_MANIFEST` with `extra` after it.
    pub fn with_manifest(extra: &str) -> Fixture {
        Fixture::with_root(&format!("{ROOT_MANIFEST}\n{extra}"))
    }

    pub fn with_config(config: &str) -> Fixture {
        let fixture = Fixture::new();
        fixture.write(".cargo/config.toml", config);
        fixture
    }

    pub fn write(&self, relative: &str, content: &str) {
        let path = self.dir.path().join(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
    }

    /// The workspace root, as the test names it.
    pub fn root(&self) -> PathBuf {
        self.dir.path().canonicalize().unwrap()
    }

    /// The environment of a requester in this workspace: nothing a build reads, a cargo home of
    /// its own, and no configuration file above the workspace.
    pub fn env(&self) -> BTreeMap<String, String> {
        let root = self.root();
        let mut env = BTreeMap::new();
        env.insert("CARGO_HOME".to_owned(), root.join("home").display().to_string());
        env.insert("__CARGO_TEST_ROOT".to_owned(), root.display().to_string());
        env.insert("PATH".to_owned(), "/usr/bin".to_owned());
        env
    }
}

/// Reads the platform from what a Mac has, without asking one.
pub struct FakeMachine;

impl Probe for FakeMachine {
    fn which(&self, name: &str, _path: &str) -> Option<PathBuf> {
        Some(PathBuf::from("/usr/bin").join(name))
    }

    fn run(&self, program: &Path, args: &[&str]) -> Option<String> {
        if program == Path::new("/usr/bin/env") {
            return match args.last().copied() {
                Some("--show-sdk-path") => Some("/sdk/MacOSX.sdk".to_owned()),
                Some("--show-sdk-version") => Some("26.0".to_owned()),
                _ => None,
            };
        }
        Some(format!("{} 1.0.0\n", program.display()))
    }

    fn read(&self, path: &Path) -> Option<Vec<u8>> {
        Some(path.display().to_string().into_bytes())
    }

    fn os(&self) -> Option<String> {
        Some("macos 26.0".to_owned())
    }
}

pub fn host(llvm: bool) -> Host {
    let cfg = ["unix", "target_os=\"macos\"", "target_arch=\"aarch64\"", "debug_assertions"];
    Host {
        triple: "aarch64-apple-darwin".to_owned(),
        cfg: cfg.iter().map(|it| it.parse().unwrap()).collect(),
        cpu: "apple-m1".to_owned(),
        llvm,
    }
}

pub fn context<'a>(
    fixture: &Fixture,
    env: &'a BTreeMap<String, String>,
    host: &'a Host,
    cwd: &'a Path,
) -> Context<'a> {
    let _ = fixture;
    Context {
        cwd,
        env,
        host,
        source: SourceRef::Root(blaze_wire::RootRef { root: [7; 32] }),
        toolchain: StoreIdentity([9; 32]),
        offline: true,
        store: None,
        probe: &FakeMachine,
    }
}

/// A package as a profile override is matched against it.
pub struct Package {
    pub name: String,
    pub version: semver::Version,
    pub source: blaze_manifest::source::SourceId,
}

impl blaze_manifest::profiles::ProfilePackage for Package {
    fn name(&self) -> &str {
        &self.name
    }

    fn version(&self) -> &semver::Version {
        &self.version
    }

    fn source_id(&self) -> &blaze_manifest::source::SourceId {
        &self.source
    }
}

impl std::fmt::Display for Package {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} v{}", self.name, self.version)
    }
}

impl Package {
    pub fn member(root: &Path, name: &str) -> Package {
        Package {
            name: name.to_owned(),
            version: semver::Version::new(0, 1, 0),
            source: blaze_manifest::source::SourceId::for_path(&root.join(name)).unwrap(),
        }
    }

    pub fn registry(name: &str) -> Package {
        let index = blaze_manifest::source::CRATES_IO_INDEX.parse().unwrap();
        Package {
            name: name.to_owned(),
            version: semver::Version::new(1, 0, 0),
            source: blaze_manifest::source::SourceId::for_registry(&index),
        }
    }
}

/// The profile cargo makes of `package`'s unit in `fixture` for the profile `requested`.
pub fn profile_of(
    fixture: &Fixture,
    env: &BTreeMap<String, String>,
    requested: &str,
    package: &Package,
    member: bool,
    host_unit: bool,
) -> Profile {
    try_profile_of(fixture, env, requested, package, member, host_unit).unwrap()
}

/// [`profile_of`], or what went wrong where the configuration or the manifests are cargo's to
/// reject.
pub fn try_profile_of(
    fixture: &Fixture,
    env: &BTreeMap<String, String>,
    requested: &str,
    package: &Package,
    member: bool,
    host_unit: bool,
) -> anyhow::Result<Profile> {
    let root = fixture.root();
    let config = blaze_manifest::Config::load(&root, env.clone(), &[])?;
    let manifests = blaze_manifest::Context::new(config);
    let manifest = root.join("Cargo.toml");
    let workspace = blaze_manifest::Workspace::new(&manifest, &manifests)?;
    let profiles = Profiles::new(
        workspace.profiles(),
        workspace.unstable_features(),
        &manifests.config,
        requested,
        "aarch64-apple-darwin",
    )?;
    let unit_for = if host_unit { UnitFor::new_host(false) } else { UnitFor::new_normal() };
    Ok(profiles.get_profile(
        package,
        member,
        package.source.url.scheme() == "file",
        unit_for,
        &blaze_manifest::config::CompileKind::Host,
    ))
}

/// A unit of the graph, built by hand.
#[derive(Clone)]
pub struct Make {
    pub name: String,
    pub kind: TargetKind,
    pub mode: Mode,
    pub features: Vec<String>,
    pub for_host: bool,
    pub member: bool,
    pub harness: bool,
    pub deps: Vec<(usize, String)>,
    pub profile: Option<Profile>,
    pub source: String,
}

impl Make {
    pub fn new(name: &str, kind: TargetKind) -> Make {
        Make {
            name: name.to_owned(),
            kind,
            mode: Mode::Build,
            features: Vec::new(),
            for_host: false,
            member: true,
            harness: true,
            deps: Vec::new(),
            profile: None,
            source: format!("path+{name}"),
        }
    }

    pub fn mode(mut self, mode: Mode) -> Make {
        self.mode = mode;
        self
    }

    pub fn features(mut self, features: &[&str]) -> Make {
        self.features = features.iter().map(|it| (*it).to_owned()).collect();
        self
    }

    pub fn host(mut self) -> Make {
        self.for_host = true;
        self
    }

    pub fn dep(mut self, index: usize, name: &str) -> Make {
        self.deps.push((index, name.to_owned()));
        self
    }

    pub fn profile(mut self, profile: Profile) -> Make {
        self.profile = Some(profile);
        self
    }

    pub fn registry(mut self) -> Make {
        self.member = false;
        self.source = "registry+https://github.com/rust-lang/crates.io-index".to_owned();
        self
    }

    pub fn no_harness(mut self) -> Make {
        self.harness = false;
        self
    }
}

/// The graph of `units`, whose profiles `profile` makes where a unit has none, and its roots.
pub fn graph(units: Vec<Make>, roots: &[usize], profile: &Profile) -> UnitGraph {
    let units = units
        .into_iter()
        .map(|make| Unit {
            package: UnitPackage {
                id: format!("{}#{}@0.1.0", make.source, make.name),
                name: make.name.clone(),
                version: "0.1.0".to_owned(),
                source: make.source.clone(),
                manifest_dir: PathBuf::from(&make.name),
                member: make.member,
            },
            target: UnitTarget {
                kind: make.kind,
                crate_types: vec!["lib".to_owned()],
                name: make.name.clone(),
                src_path: PathBuf::from(format!("{}/src/lib.rs", make.name)),
                edition: 2021,
                harness: make.harness,
                required_features: None,
                doc: true,
                doctest: make.kind == TargetKind::Lib,
                tested: true,
            },
            mode: make.mode,
            test: make.mode == Mode::Test
                || (make.mode == Mode::Check
                    && matches!(make.kind, TargetKind::Test | TargetKind::Bench)),
            profile: make.profile.unwrap_or_else(|| profile.clone()),
            features: make.features,
            for_host: make.for_host,
            dependencies: make
                .deps
                .into_iter()
                .map(|(index, name)| UnitDep {
                    index,
                    extern_crate_name: name,
                    public: false,
                    noprelude: false,
                    nounused: false,
                })
                .collect(),
        })
        .collect();
    UnitGraph {
        units,
        roots: roots.to_vec(),
        root: PathBuf::new(),
        lock: b"lock".to_vec(),
        warnings: Vec::new(),
    }
}

/// The graph of a member `a` as a library, built for the target.
pub fn simple_graph(profile: &Profile) -> UnitGraph {
    graph(vec![Make::new("a", TargetKind::Lib)], &[0], profile)
}

/// The spec of `args` for `command` in `fixture`, over `graph` where the test gives one.
pub fn spec(
    fixture: &Fixture,
    env: &BTreeMap<String, String>,
    command: Subcommand,
    args: &[&str],
    graph: Option<&dyn Fn(&Profile) -> UnitGraph>,
) -> Result<Spec, Refusal> {
    spec_on(&host(false), fixture, env, command, args, graph)
}

/// [`spec`] for a blaze with `host`'s facts.
pub fn spec_on(
    host: &Host,
    fixture: &Fixture,
    env: &BTreeMap<String, String>,
    command: Subcommand,
    args: &[&str],
    graph: Option<&dyn Fn(&Profile) -> UnitGraph>,
) -> Result<Spec, Refusal> {
    let args: Vec<String> = args.iter().map(|it| (*it).to_owned()).collect();
    let request = Request::parse(command, &args)?;
    let root = fixture.root();
    let context = context(fixture, env, host, &root);
    let default = if command == Subcommand::Test { "test" } else { "dev" };
    let requested = request.profile.clone().unwrap_or_else(|| default.to_owned());
    // A configuration cargo rejects has no profile to make, and the spec refuses it first.
    let profile =
        try_profile_of(fixture, env, &requested, &Package::member(&root, "a"), true, false)
            .unwrap_or_default();
    let units = match graph {
        Some(graph) => graph(&profile),
        None => simple_graph(&profile),
    };
    assemble(&request, &context, &units)
}
