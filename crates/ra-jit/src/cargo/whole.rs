//! The cold build by blaze (docs/no-rustc.md, step K, #91):
//! `cargo blaze build` and `run` from nothing build each program of the build whole, every
//! crate from source over the shipped standard library and linked by blaze (step G), and put
//! it where cargo would. No `cargo build` runs, and what blaze cannot compile that the program
//! reaches fails the build, with what it is.

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    process::Command,
    time::Instant,
};

use anyhow::Context as _;
use cargo_metadata::{Metadata, Package};

use crate::plan::Selection;

use super::{
    FAILED,
    cli::{Invocation, Subcommand, worker_env},
    client::{self, Start},
    daemon::{Checkout, Reply, Request, WorkerConfig},
    plan::Plan as Linked,
    run, say,
    state::{as_built, configured_env, place, repository},
};

/// How many of the functions blaze could not compile a failed build names.
const SHOWN: usize = 8;

/// Whether the build `asked` for, from nothing, is blaze's whole build: `build` or `run`, of
/// the programs a build picks by default or one named with `--bin` or `--example`, for one
/// target at most.
pub(super) fn asked_for(asked: &Invocation) -> bool {
    let flags = &asked.flags;
    let named = flags.bin.is_some() || flags.example.is_some();
    matches!(asked.subcommand, Subcommand::Build | Subcommand::Run)
        && (!flags.selects_targets || named)
        && !flags.cargos_plan
        && flags.targets.len() <= 1
}

/// A program of the build, as cargo plans it.
struct Program {
    /// The name of the executable.
    name: String,
    /// The root file of its crate.
    root: PathBuf,
    package: Package,
    /// Where cargo puts it.
    executable: PathBuf,
}

/// Builds the programs of `asked` whole and runs the one `run` asks for. `None` when the build
/// has no program to link, which is then built as without the setting.
pub(super) fn build(asked: &Invocation) -> anyhow::Result<Option<i32>> {
    let started = Instant::now();
    let cwd = std::env::current_dir()?;
    let metadata = super::state::metadata(asked);
    let programs = planned(asked, &cwd, &metadata)?;
    if programs.is_empty() {
        return Ok(None);
    }
    let root = metadata.workspace_root.as_std_path();
    let project = programs.first().map(|it| super::state::package_dir(&it.package));
    let project = project.as_deref().unwrap_or(root);
    let config = config_of(asked, &cwd, root, project);
    // One directory per build of the checkout, as the daemon names it: the features, the
    // profile, the target and the variables that change what is compiled are all in that
    // name, so the program one build links is never the one another places.
    let target = metadata.target_directory.as_std_path();
    lay_out(target, programs.iter().map(|it| &*it.executable))?;
    let dir = prepare_dir(&config, target)?;
    for program in &programs {
        let package = &program.package;
        let at = super::state::package_dir(package);
        say::status(
            asked.flags.quiet,
            "Building",
            &format!("{} v{} ({})", package.name, package.version, at.display()),
        );
    }
    let runtime = match whole(asked, &cwd, &metadata, &config, &programs, &dir) {
        Ok(runtime) => runtime,
        Err(why) => {
            say::error(&why);
            return Ok(Some(FAILED));
        }
    };
    let took = started.elapsed().as_secs_f64();
    let profile = asked.profile();
    let finished = format!("`{profile}` profile [ra-jit] target(s) in {took:.2}s");
    say::status(asked.flags.quiet, "Finished", &finished);
    if asked.subcommand != Subcommand::Run {
        return Ok(Some(0));
    }
    ran(asked, &cwd, &programs, &runtime).map(Some)
}

/// Has the daemon build `programs` whole into `dir`, and puts each where cargo would. A
/// worker of the daemon loads the workspace and links each program before it answers, and
/// builds them again on what it has loaded when asked next. `Err` says why not.
fn whole(
    asked: &Invocation,
    cwd: &Path,
    metadata: &Metadata,
    config: &WorkerConfig,
    programs: &[Program],
    dir: &Path,
) -> Result<Vec<super::runtime_context::Context>, String> {
    let checkout = checkout(asked, cwd, metadata, config, programs, dir);
    let compiler = std::env::current_exe().map_err(|it| format!("finding owned compiler: {it}"))?;
    let cargo = super::runtime_context::cargo_frontend(&compiler, &dir.join("cargo-frontend"))
        .map_err(|it| format!("{it:#}"))?;
    let mut request = super::asking(&checkout);
    request.env.remove("CARGO");
    let env = request
        .native_env
        .take()
        .unwrap_or_default()
        .over([("CARGO".into(), cargo.into_os_string())]);
    request.env =
        env.unicode().iter().map(|(name, value)| (name.to_owned(), value.to_owned())).collect();
    request.native_env = Some(env);
    let reply =
        client::ask(Request::Build(request), Start::IfMissing).map_err(|it| format!("{it:#}"))?;
    let (built, runtime) = match reply {
        Some(Reply::Synced { programs, runtime, .. }) => (programs, runtime),
        Some(Reply::Cold { reason }) => return Err(not_loaded(cwd, &checkout, &reason)),
        None => return Err(client::why_none()),
        Some(other) => return Err(format!("the daemon answered {other:?}")),
    };
    if built.len() != programs.len() {
        return Err("the whole-build reply has no matching program results".into());
    }
    for (program, built) in programs.iter().zip(&built) {
        if let Err(why) = built {
            return Err(format!("could not build `{}`: {}", program.name, why.clone().why()));
        }
    }
    if runtime.len() != programs.len() || runtime.iter().any(Option::is_none) {
        return Err("the whole-build reply has no matching runtime contexts".into());
    }
    for (program, plan) in programs.iter().zip(&checkout.programs) {
        let _unwritten = crate::splice::unwritten(&plan.binary);
        let stand_ins = crate::link::stand_ins(&plan.binary).map_err(|it| format!("{it:#}"))?;
        if !stand_ins.is_empty() {
            let mut reached: Vec<String> = stand_ins
                .iter()
                .take(SHOWN)
                .map(|it| it.trim().trim_start_matches(hir_ty_codegen::STAND_IN_SAYS))
                .map(without_hashes)
                .collect();
            if stand_ins.len() > SHOWN {
                reached.push(format!("and {} more", stand_ins.len() - SHOWN));
            }
            return Err(format!(
                "`{}` can reach functions blaze could not compile, so it is not built:\n  {}",
                program.name,
                reached.join("\n  ")
            ));
        }
        if let Some(parent) = program.executable.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|it| format!("creating {}: {it}", parent.display()))?;
        }
        place(&plan.binary, &program.executable)
            .map_err(|it| format!("installing {}: {it}", program.executable.display()))?;
    }
    runtime
        .into_iter()
        .map(|context| context.ok_or_else(|| "whole build omitted its runtime context".into()))
        .collect()
}

/// The worker configuration of a whole build asked in `cwd`, of the workspace at `root` whose
/// program is in `project`. The invocation directory selects the members and resolves a
/// relative manifest path; the workspace root names the repository and project separately.
fn config_of(asked: &Invocation, cwd: &Path, root: &Path, project: &Path) -> WorkerConfig {
    let asked_in =
        asked.flags.manifest_path.as_ref().map_or_else(|| cwd.to_owned(), |it| cwd.join(it));
    WorkerConfig {
        repository: repository(root),
        project: project.strip_prefix(root).unwrap_or(Path::new("")).to_owned(),
        cfg_test: false,
        selection: Selection { asked_in: Some(asked_in), ..asked.selection() },
        env: worker_env(),
        whole: true,
    }
}

/// The daemon's checkout of a whole build of `programs`, each linked into `dir`.
fn checkout(
    asked: &Invocation,
    cwd: &Path,
    metadata: &Metadata,
    config: &WorkerConfig,
    programs: &[Program],
    dir: &Path,
) -> Checkout {
    let root = metadata.workspace_root.as_std_path();
    let config = config.clone();
    let plans = programs.iter().map(|program| Linked {
        crates: vec![program.root.clone()],
        object: dir.join(format!("{}.o", program.name)),
        patch: dir.join(format!("{}.patch.o", program.name)),
        binary: dir.join(&program.name),
        rustc_binary: None,
        link_cmd: String::new(),
        symbol_files: Vec::new(),
        unit_test: false,
        linked_as_built: Vec::new(),
        copies: Vec::new(),
        package: None,
        stamp: 0,
        whole: true,
    });
    let members = metadata.workspace_packages();
    let manifests = members.iter().map(|it| it.manifest_path.as_std_path().to_owned());
    let lock = root.join("Cargo.lock");
    // A worker that loaded other manifests loads them again; one with the same is asked to
    // build again what was edited since.
    let generation = manifests.clone().chain([lock]).filter_map(|it| written_at(&it)).max();
    let sources = members.iter().flat_map(|it| sources_in(&super::state::package_dir(it)));
    Checkout {
        key: format!("{}-whole", asked.key(cwd)),
        generation: generation.unwrap_or(0),
        built_at: 0,
        config,
        root: root.to_owned(),
        programs: plans.collect(),
        sources: manifests.chain(sources).collect(),
        env: BTreeMap::new(),
        native_env: None,
        scripts: None,
    }
}

/// When `file` was last written, as a stamp.
fn written_at(file: &Path) -> Option<u128> {
    let written = std::fs::metadata(file).and_then(|it| it.modified()).ok()?;
    Some(written.duration_since(std::time::UNIX_EPOCH).ok()?.as_nanos())
}

/// The Rust files of the package in `dir`, but not those of a target directory or of a
/// package within it.
fn sources_in(dir: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut left = vec![dir.to_owned()];
    while let Some(dir) = left.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name();
            let Ok(kind) = entry.file_type() else { continue };
            if kind.is_dir() {
                let hidden = name.to_string_lossy().starts_with('.');
                if !hidden && name != "target" && !path.join("Cargo.toml").exists() {
                    left.push(path);
                }
            } else if path.extension().is_some_and(|it| it == "rs") {
                found.push(path);
            }
        }
    }
    found
}

/// Why the daemon did not load the workspace, from what its worker said: the functions blaze
/// could not compile, or its last error.
fn not_loaded(cwd: &Path, checkout: &Checkout, reason: &str) -> String {
    let named = checkout.programs.iter().map(|it| it.binary.file_name().unwrap_or_default());
    let named: Vec<String> = named.map(|it| format!("`{}`", it.to_string_lossy())).collect();
    let named = named.join(", ");
    let Some(log) = worker_log(&checkout.root.join(&checkout.config.project)) else {
        return format!("could not build {named}: {reason}");
    };
    let said = std::fs::read_to_string(&log).unwrap_or_default();
    let shown = log.strip_prefix(cwd).unwrap_or(&log).display().to_string();
    // A function of the program's own crates blaze cannot compile is left out of the object,
    // and the link then fails on what it would have defined.
    let unsupported: Vec<String> = said
        .lines()
        .filter_map(|it| it.trim().strip_prefix("left to rustc: "))
        .map(without_hashes)
        .collect();
    if !unsupported.is_empty() {
        return format!(
            "{named} has functions blaze could not compile, so it is not built:\n  {}\n(what blaze said is in {shown})",
            unsupported.join("\n  ")
        );
    }
    let last = said.lines().rev().find(|it| it.starts_with("Error"));
    let last = last.map_or(reason, |it| it.strip_prefix("Error: ").unwrap_or(it));
    format!("could not build {named}: {last} (what blaze said is in {shown})")
}

/// The log of the daemon's worker for the package in `project`.
fn worker_log(project: &Path) -> Option<PathBuf> {
    let Ok(Some(Reply::Status(status))) = client::ask(Request::Status, Start::Never) else {
        return None;
    };
    status.workers.into_iter().find(|it| it.project == project).map(|it| it.log)
}

/// `name` with the `[0123456789abcdef]` after each crate's name, and the `ra-jit.<hash>.` of
/// a symbol, taken out.
fn without_hashes(name: &str) -> String {
    let is_hash = |it: &str| it.len() == 16 && it.bytes().all(|it| it.is_ascii_hexdigit());
    let mut name = name.to_owned();
    while let Some(at) = name.find("ra-jit.")
        && let Some(hash) = name.get(at + 7..at + 23)
        && is_hash(hash)
        && name[at + 23..].starts_with('.')
    {
        name.replace_range(at..at + 24, "");
    }
    let mut out = String::with_capacity(name.len());
    let mut rest = name.as_str();
    while let Some(at) = rest.find('[') {
        let hash = rest[at + 1..].split_once(']').map(|(it, _)| it);
        out.push_str(&rest[..at]);
        match hash {
            Some(hash) if is_hash(hash) => {
                rest = &rest[at + hash.len() + 2..];
            }
            _ => {
                out.push('[');
                rest = &rest[at + 1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// Runs the program `asked` names, as `cargo run` does.
fn ran(
    asked: &Invocation,
    cwd: &Path,
    programs: &[Program],
    runtime: &[super::runtime_context::Context],
) -> anyhow::Result<i32> {
    let named = asked.flags.bin.as_ref().or(asked.flags.example.as_ref());
    let mut chosen: Vec<&Program> =
        programs.iter().filter(|it| named.is_none_or(|name| it.name == *name)).collect();
    if chosen.len() > 1 {
        let default = |it: &&Program| it.package.default_run.as_deref() == Some(it.name.as_str());
        chosen.retain(default);
    }
    let [program] = chosen[..] else {
        let names: Vec<&str> = programs.iter().map(|it| it.name.as_str()).collect();
        say::error(&format!(
            "`cargo run` could not determine which binary to run. Use the `--bin` option to specify a binary, \
             or the `default-run` manifest key.\navailable binaries: {}",
            names.join(", ")
        ));
        return Ok(FAILED);
    };
    let shown =
        program.executable.strip_prefix(cwd).unwrap_or(&program.executable).display().to_string();
    let args = asked.program_args().map(|it| format!(" {it}")).collect::<String>();
    say::status(asked.flags.quiet, "Running", &format!("`{shown}{args}`"));
    let at = programs.iter().position(|candidate| std::ptr::eq(candidate, program)).unwrap();
    let mut inherited: BTreeMap<_, _> = std::env::vars_os().collect();
    for it in configured_env(cwd) {
        if it.force || !inherited.contains_key(std::ffi::OsStr::new(&it.name)) {
            inherited.insert(it.name.into(), it.value.into());
        }
    }
    let profile = profile_of(&program.executable).context("program has no profile directory")?;
    let environment = runtime[at].environment(profile, &crate::process::Native::of(inherited))?;
    let mut command = Command::new(&program.executable);
    command.env_clear().envs(environment.vars()).args(asked.program_args());
    as_built(&mut command)?;
    let status = command.status().with_context(|| format!("could not execute {shown}"))?;
    Ok(run::exit_code(status))
}

/// The directory of cargo's profile that `executable` is in: `debug` of `debug/app` and of
/// `debug/examples/app`.
fn profile_of(executable: &Path) -> Option<&Path> {
    let directory = executable.parent()?;
    let under = directory.file_name().is_some_and(|name| name == "examples" || name == "deps");
    if under { directory.parent() } else { Some(directory) }
}

/// What cargo's `CACHEDIR.TAG` says, which marks the target directory as a build's to
/// backups, to cargo's and blaze's walks of a package's files and to blaze's own.
const CACHEDIR_TAG: &str = "Signature: 8a477f597d28d172789f06886806bc55\n\
    # This file is a cache directory tag created by cargo.\n\
    # For information about cache directory tags see https://bford.info/cachedir/\n";

/// Lays out `target` and the directory of each profile `executables` are placed in as cargo's
/// build does (`Layout::prepare`): a target directory blaze makes is tagged, and a
/// profile's has `deps`, `build`, `examples`, `incremental` and `.fingerprint`. A program is
/// placed where cargo has it, so what reads cargo's layout finds it in a profile it knows.
fn lay_out<'a>(
    target: &Path,
    executables: impl IntoIterator<Item = &'a Path>,
) -> anyhow::Result<()> {
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    match std::fs::create_dir(target) {
        Ok(()) => std::fs::write(target.join("CACHEDIR.TAG"), CACHEDIR_TAG)
            .with_context(|| format!("tagging {}", target.display()))?,
        Err(it) if it.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(it) => return Err(it).with_context(|| format!("creating {}", target.display())),
    }
    for executable in executables {
        let Some(profile) = profile_of(executable) else { continue };
        for dir in ["deps", "build", "examples", "incremental", ".fingerprint"] {
            let dir = profile.join(dir);
            std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        }
    }
    Ok(())
}

/// The directory a whole build of `config`, under `target`, links its programs in: one per
/// build of the checkout, named as the daemon names the worker of it. Two builds of one
/// program with different features, profile, target or variables put it where cargo does,
/// which is one path, so they must not share the file that is linked before it is placed
/// there.
fn dir_of(config: &WorkerConfig, target: &Path) -> PathBuf {
    target.join("ra-jit").join("whole").join(config.id())
}

/// Creates only this build's directory. Another configuration may still be reading or
/// placing its linked output after a worker reply, regardless of its directory's age.
fn prepare_dir(config: &WorkerConfig, target: &Path) -> anyhow::Result<PathBuf> {
    let dir = dir_of(config, target);
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    Ok(dir)
}

/// The programs `asked` builds, each executable root of the build, as blaze plans it with its
/// own resolution, or as cargo's unit graph has them where blaze cannot.
fn planned(asked: &Invocation, cwd: &Path, metadata: &Metadata) -> anyhow::Result<Vec<Program>> {
    let manifest = asked.flags.manifest_path.as_ref().map(|it| cwd.join(it));
    let dir = manifest.as_deref().and_then(Path::parent).unwrap_or(cwd);
    let selection = asked.selection();
    let named = blaze_metadata::Targets {
        bin: asked.flags.bin.as_deref(),
        example: asked.flags.example.as_deref(),
    };
    let own = super::metadata::programs(&selection, dir, manifest.as_deref(), named);
    let planned: Vec<crate::plan::Program> = match own {
        Some(Ok(planned)) => planned
            .into_iter()
            .map(|it| crate::plan::Program {
                package: it.package,
                name: it.name,
                root: it.root,
                example: it.example,
                platform: selection.target.clone(),
            })
            .collect(),
        Some(Err(why)) => {
            anyhow::bail!("blaze could not plan the build: {why:#}; Cargo was not executed")
        }
        None => anyhow::bail!("the owned build planner is disabled; Cargo was not executed"),
    };
    // cargo's directory of a profile: `dev`'s is `debug`.
    let profile = match selection.profile.as_str() {
        "dev" => "debug",
        other => other,
    };
    let target = metadata.target_directory.as_std_path();
    let mut programs = Vec::new();
    for program in planned {
        let Some(package) = metadata.packages.iter().find(|it| it.id.repr == program.package)
        else {
            anyhow::bail!(
                "the plan of the build names {}, which the workspace does not",
                program.package
            );
        };
        let mut dir = target.to_owned();
        if let Some(triple) = &program.platform {
            dir.push(triple);
        }
        dir.push(profile);
        if program.example {
            dir.push("examples");
        }
        let name = format!("{}{}", program.name, std::env::consts::EXE_SUFFIX);
        programs.push(Program {
            executable: dir.join(&name),
            name: program.name,
            root: program.root,
            package: package.clone(),
        });
    }
    Ok(programs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cargo::cli::Asked;

    /// A whole build of one program, as `cargo ra-jit build` of `args` is it, with the
    /// configuration the daemon would load for it.
    fn config(args: &str) -> WorkerConfig {
        config_in(args, Path::new("/ws"), Path::new("/ws"), Path::new("/ws"))
    }

    fn config_in(args: &str, cwd: &Path, root: &Path, project: &Path) -> WorkerConfig {
        config_with_args(args.split_whitespace().map(str::to_owned).collect(), cwd, root, project)
    }

    fn config_with_args(
        args: Vec<String>,
        cwd: &Path,
        root: &Path,
        project: &Path,
    ) -> WorkerConfig {
        let words = std::iter::once("build".to_owned()).chain(args);
        let asked = match Asked::parse(words.collect()) {
            Asked::Build(invocation) => *invocation,
            other => panic!("{other:?}"),
        };
        config_of(&asked, cwd, root, project)
    }

    /// The file a whole build of `args` links its program into, before it places it where
    /// cargo has it. Two builds whose program differs in what it compiles must not share it:
    /// the one of the second would be placed from a file the first linked.
    #[test]
    fn a_build_links_into_a_directory_of_its_own() {
        let target = Path::new("/ws/target");
        let linked = |args: &str| dir_of(&config(args), target).join("app");
        assert_ne!(linked("--features x"), linked(""));
        assert_ne!(linked("--release"), linked(""));
        assert_ne!(linked("--target aarch64-unknown-linux-gnu"), linked(""));
        assert_ne!(linked("--no-default-features"), linked(""));
        assert_ne!(linked("--all-features"), linked(""));
        assert_ne!(linked("--config profile.dev.opt-level=2"), linked(""));
        // A build of the same flags again is the same directory, and places the same file,
        // whatever the spelling of the flags that asked for it.
        assert_eq!(linked("--features x"), linked("--features x"));
        assert_eq!(linked("--features x"), linked("-F x"));
        assert_eq!(linked("--release"), linked("-r"));
        assert_eq!(linked(""), linked(""));
        // Under the target directory, beside the daemon's other files.
        assert!(linked("--features x").starts_with(target.join("ra-jit/whole")));
    }

    #[test]
    fn worker_resolution_keeps_the_invocation_directory() {
        let ws = tempfile::tempdir().unwrap();
        let root = ws.path();
        for (file, source) in [
            (
                "Cargo.toml",
                "[workspace]\nmembers = ['default', 'member', 'parts']\n\
                 default-members = ['default']\nresolver = '2'\n",
            ),
            (
                "Cargo.lock",
                "# This file is automatically @generated by Cargo.\n\
                 # It is not intended for manual editing.\n\
                 version = 4\n\n\
                 [[package]]\nname = \"kdefault\"\nversion = \"0.1.0\"\n\
                 dependencies = [\n \"kparts\",\n]\n\n\
                 [[package]]\nname = \"kmember\"\nversion = \"0.1.0\"\n\
                 dependencies = [\n \"kparts\",\n]\n\n\
                 [[package]]\nname = \"kparts\"\nversion = \"0.1.0\"\n",
            ),
            (
                "default/Cargo.toml",
                "[package]\nname = 'kdefault'\nversion = '0.1.0'\nedition = '2021'\n\
                 [dependencies]\nkparts = { path = '../parts', features = ['default-member'] }\n",
            ),
            ("default/src/main.rs", "fn main() {}"),
            (
                "member/Cargo.toml",
                "[package]\nname = 'kmember'\nversion = '0.1.0'\nedition = '2021'\n\
                 [dependencies]\nkparts = { path = '../parts', features = ['selected-member'] }\n",
            ),
            ("member/src/main.rs", "fn main() {}"),
            (
                "parts/Cargo.toml",
                "[package]\nname = 'kparts'\nversion = '0.1.0'\nedition = '2021'\n\
                 [features]\ndefault-member = []\nselected-member = []\n",
            ),
            ("parts/src/lib.rs", ""),
        ] {
            let file = root.join(file);
            std::fs::create_dir_all(file.parent().unwrap()).unwrap();
            std::fs::write(file, source).unwrap();
        }
        let member = root.join("member");
        let loaded = project_model::ManifestPath::try_from(paths::AbsPathBuf::assert_utf8(
            member.join("Cargo.toml"),
        ))
        .unwrap();
        for (cwd, manifest, expected) in [
            (root.to_owned(), None, "default-member"),
            (member.clone(), None, "selected-member"),
            (member.join("src"), None, "selected-member"),
            (member.join("src"), Some(PathBuf::from("../Cargo.toml")), "selected-member"),
            (root.to_owned(), Some(member.join("Cargo.toml")), "selected-member"),
        ] {
            let args = manifest.as_ref().map_or_else(Vec::new, |it| {
                vec!["--manifest-path".to_owned(), it.to_str().unwrap().to_owned()]
            });
            let config = config_with_args(args, &cwd, root, &member);
            let asked_in = manifest.as_ref().map_or_else(|| cwd.clone(), |it| cwd.join(it));
            assert_eq!(config.selection.asked_in.as_deref(), Some(asked_in.as_path()));
            assert_eq!(config.repository, repository(root));
            assert_eq!(config.project, Path::new("member"));
            // Exercise the worker's actual resolver callback, not just the identity hash.
            // Loading the member must still resolve root defaults when asked from the root.
            let own = super::super::metadata::own(&config.selection).unwrap();
            let (metadata, _) =
                (own.metadata)(&loaded, Some("x86_64-unknown-linux-gnu"), &[], &Default::default())
                    .unwrap();
            let parts = metadata.packages.iter().find(|it| it.name == "kparts").unwrap();
            let node = metadata
                .resolve
                .as_ref()
                .unwrap()
                .nodes
                .iter()
                .find(|it| it.id == parts.id)
                .unwrap();
            assert_eq!(node.features, [expected], "asked in {} with {manifest:?}", cwd.display());
        }
    }

    /// `build` at the root of a virtual workspace builds what cargo's unit graph has as its
    /// roots, the libraries `a` and `b` and the program `c`, and puts `c` at `debug/c` in a
    /// profile directory laid out as cargo's: what reads cargo's layout (a harness, a tool
    /// that looks in `deps`) finds the program there, as after `cargo build`.
    #[test]
    fn a_workspace_build_places_its_program_in_cargos_layout() {
        let ws = tempfile::tempdir().unwrap();
        let root = ws.path();
        let package = |name: &str, deps: &str| {
            format!("[package]\nname = '{name}'\nversion = '0.1.0'\nedition = '2021'\n{deps}")
        };
        let lock = "# This file is automatically @generated by Cargo.\n\
                    # It is not intended for manual editing.\n\
                    version = 4\n\n\
                    [[package]]\nname = \"a\"\nversion = \"0.1.0\"\ndependencies = [\n \"b\",\n]\n\n\
                    [[package]]\nname = \"b\"\nversion = \"0.1.0\"\n\n\
                    [[package]]\nname = \"c\"\nversion = \"0.1.0\"\ndependencies = [\n \"a\",\n]\n";
        for (file, source) in [
            (
                "Cargo.toml",
                "[workspace]\nresolver = '2'\nmembers = ['crates/a', 'crates/b', 'crates/c']\n"
                    .to_owned(),
            ),
            ("Cargo.lock", lock.to_owned()),
            ("crates/a/Cargo.toml", package("a", "[dependencies]\nb = { path = '../b' }\n")),
            ("crates/a/src/lib.rs", "pub fn a() -> u32 { b::b() + 1 }".to_owned()),
            ("crates/b/Cargo.toml", package("b", "")),
            ("crates/b/src/lib.rs", "pub fn b() -> u32 { 1 }".to_owned()),
            ("crates/c/Cargo.toml", package("c", "[dependencies]\na = { path = '../a' }\n")),
            ("crates/c/src/main.rs", "fn main() { println!(\"{}\", a::a()); }".to_owned()),
        ] {
            let file = root.join(file);
            std::fs::create_dir_all(file.parent().unwrap()).unwrap();
            std::fs::write(file, source).unwrap();
        }
        let asked = match Asked::parse(vec!["build".to_owned()]) {
            Asked::Build(invocation) => *invocation,
            other => panic!("{other:?}"),
        };
        let planned_in = |cwd: &Path| {
            let metadata = super::super::metadata::no_deps_in(cwd, None, &[], Vec::new()).unwrap();
            let programs = planned(&asked, cwd, &metadata).unwrap();
            (metadata, programs)
        };
        // In a member that is a library, the build is of that member: no program.
        let (_, programs) = planned_in(&root.join("crates/a"));
        assert!(
            programs.is_empty(),
            "{:?}",
            programs.iter().map(|it| &it.name).collect::<Vec<_>>()
        );

        let (metadata, programs) = planned_in(root);
        let target = metadata.target_directory.as_std_path();
        let debug = target.join("debug");
        let placed: Vec<_> = programs.iter().map(|it| (&*it.name, &*it.executable)).collect();
        assert_eq!(placed, [("c", &*debug.join(format!("c{}", std::env::consts::EXE_SUFFIX)))]);
        assert!(!target.exists());
        lay_out(target, programs.iter().map(|it| &*it.executable)).unwrap();
        for dir in ["deps", "build", "examples", "incremental", ".fingerprint"] {
            assert!(debug.join(dir).is_dir(), "no {dir}");
        }
        assert_eq!(std::fs::read_to_string(target.join("CACHEDIR.TAG")).unwrap(), CACHEDIR_TAG);
        // A target directory that was there before is cargo's or the user's to tag.
        let made = tempfile::tempdir().unwrap();
        let example = made.path().join("x86_64-unknown-linux-gnu/release/examples/demo");
        lay_out(made.path(), [&*example]).unwrap();
        assert!(!made.path().join("CACHEDIR.TAG").exists());
        assert!(made.path().join("x86_64-unknown-linux-gnu/release/deps").is_dir());
        assert!(!example.parent().unwrap().join("deps").exists());
    }

    #[cfg(unix)]
    #[test]
    fn another_configuration_preserves_aged_active_output() {
        use std::{
            fs::{File, FileTimes},
            sync::mpsc,
            time::{Duration, SystemTime},
        };

        let target = tempfile::tempdir().unwrap();
        let active = prepare_dir(&config(""), target.path()).unwrap();
        let binary = active.join("app");
        std::fs::write(&binary, b"configuration A").unwrap();
        let aged = SystemTime::now() - Duration::from_secs(8 * 24 * 60 * 60);
        File::open(&active).unwrap().set_times(FileTimes::new().set_modified(aged)).unwrap();
        assert!(std::fs::metadata(&active).unwrap().modified().unwrap() <= aged);
        // A no-op reuse neither removes the output nor refreshes the directory's age.
        assert_eq!(prepare_dir(&config(""), target.path()).unwrap(), active);
        assert_eq!(std::fs::read(&binary).unwrap(), b"configuration A");
        assert!(std::fs::metadata(&active).unwrap().modified().unwrap() <= aged);

        let placed = target.path().join("installed");
        let (ready, replied) = mpsc::channel();
        let (resume, paused) = mpsc::channel();
        let client = std::thread::spawn(move || {
            // A's worker has replied; its client has not yet read or placed the binary.
            ready.send(()).unwrap();
            paused.recv().unwrap();
            assert_eq!(std::fs::read(&binary).unwrap(), b"configuration A");
            place(&binary, &placed).unwrap();
            assert_eq!(std::fs::read(&placed).unwrap(), b"configuration A");
        });
        replied.recv().unwrap();
        let other = prepare_dir(&config("--features x"), target.path()).unwrap();
        assert_ne!(active, other);
        std::fs::write(other.join("app"), b"configuration B").unwrap();
        // B uses the production preparation path while A is paused, with no age refresh.
        assert!(std::fs::metadata(&active).unwrap().modified().unwrap() <= aged);
        resume.send(()).unwrap();
        client.join().unwrap();
    }
}
