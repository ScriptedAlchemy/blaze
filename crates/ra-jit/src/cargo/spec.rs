//! Build specs from blaze's own resolution. No cargo or rustc is invoked here.

use std::{
    collections::BTreeMap,
    ffi::OsString,
    path::{Path, PathBuf},
    process::Command,
};

use anyhow::{Context as _, bail};
use blaze_spec::{
    Refusal,
    assemble::{Context, Host, Spec},
    flags::{Request, Subcommand},
};
use blaze_wire::{BuildSpec, GitRef, Namespace, SourceRef, StoreIdentity};
use sha2::{Digest as _, Sha256};

pub(super) fn command(words: &[String]) -> anyhow::Result<i32> {
    if let [run, file] = words
        && run == "--run"
    {
        return replay(Path::new(file));
    }
    let mut graph = false;
    let mut out = None;
    let mut at = 0;
    while let Some(word) = words.get(at) {
        match word.as_str() {
            "--unit-graph" => graph = true,
            "--out" => {
                at += 1;
                out = Some(PathBuf::from(words.get(at).context("--out needs a file")?));
            }
            _ => break,
        }
        at += 1;
    }
    let subcommand = match words.get(at).map(String::as_str) {
        Some("build") => Subcommand::Build,
        Some("check") => Subcommand::Check,
        Some("run") => Subcommand::Run,
        Some("test") => Subcommand::Test,
        _ => bail!(
            "usage: cargo spec [--unit-graph] [--out FILE] build|check|run|test [flags]; cargo spec --run FILE"
        ),
    };
    let request = match Request::parse(subcommand, &words[at + 1..]) {
        Ok(request) => request,
        Err(why) => return refused(&why),
    };
    let cwd = std::env::current_dir()?;
    let env = std::env::vars().collect();
    let (host, identity) = host()?;
    let source_root = PathBuf::from(git(&cwd, &["rev-parse", "--show-toplevel"])?);
    let ctx = context(&cwd, &env, &host, identity, &source_root)?;
    let made = match blaze_spec::assemble::build_spec(&request, &ctx) {
        Ok(made) => made,
        Err(why) => return refused(&why),
    };
    if graph {
        println!("{}", unit_graph(&request, &ctx)?);
        return Ok(0);
    }
    if let Some(out) = out {
        std::fs::write(out, blaze_wire::encode(&made.build))?;
    }
    println!("{}", rendering(&made)?);
    Ok(0)
}

fn refused(why: &Refusal) -> anyhow::Result<i32> {
    println!("{}", serde_json::json!({"refused": {"what": why.what, "why": why.why}}));
    Ok(2)
}

fn rendering(made: &Spec) -> anyhow::Result<serde_json::Value> {
    let asked: Vec<_> = made
        .build
        .asked
        .iter()
        .map(|&at| {
            let unit = &made.build.units[at as usize];
            format!("{}@{}:{}:{}", unit.package.0, unit.package.1, unit.target.0, unit.target.1)
        })
        .collect();
    let command = match made.build.command {
        blaze_wire::Command::Build => "build",
        blaze_wire::Command::Check => "check",
        blaze_wire::Command::Run(_) => "run",
        blaze_wire::Command::Test(_) => "test",
    };
    Ok(serde_json::json!({
        "refused": null,
        "digests": {
            "spec": hex(&made.digest), "platform": hex(&made.platform_digest),
            "front_end": hex(&made.front_end.0), "config": hex(&made.config.0),
            "job_content": hex(&made.job_content(Namespace::Main).0),
        },
        "summary": {"command": command, "asked": asked, "units": made.build.units.len()},
        "spec": serde_json::from_str::<serde_json::Value>(&blaze_wire::to_json(&made.build))?["BuildSpec"],
        "platform": serde_json::from_str::<serde_json::Value>(&blaze_wire::to_json(&made.platform))?["ExecPlatform"],
    }))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn git(cwd: &Path, args: &[&str]) -> anyhow::Result<String> {
    let output = Command::new("git").current_dir(cwd).args(args).output()?;
    if !output.status.success() {
        bail!("git {}: {}", args.join(" "), String::from_utf8_lossy(&output.stderr));
    }
    Ok(String::from_utf8(output.stdout)?.trim().to_owned())
}

fn oid(text: &str) -> anyhow::Result<[u8; 20]> {
    if text.len() != 40 {
        bail!("expected a SHA-1 git object id");
    }
    let mut out = [0; 20];
    for (byte, pair) in out.iter_mut().zip(text.as_bytes().chunks_exact(2)) {
        *byte = u8::from_str_radix(std::str::from_utf8(pair)?, 16)?;
    }
    Ok(out)
}

fn context<'a>(
    cwd: &'a Path,
    env: &'a BTreeMap<String, String>,
    host: &'a Host,
    identity: StoreIdentity,
    source_root: &'a Path,
) -> anyhow::Result<Context<'a>> {
    // A checkout without origin (including a local fixture) still has a stable repository
    // identity: its initial commit, not the absolute path it happens to live at.
    let repo = git(cwd, &["config", "--get", "remote.origin.url"])
        .or_else(|_| git(cwd, &["rev-list", "--max-parents=0", "HEAD"]))?;
    let digest = Sha256::digest(repo.as_bytes());
    Ok(Context {
        source_root: Some(source_root),
        cwd,
        env,
        host,
        source: SourceRef::Git(GitRef {
            repo_id: u64::from_le_bytes(digest[..8].try_into().unwrap()),
            commit: oid(&git(cwd, &["rev-parse", "HEAD"])?)?,
            tree: oid(&git(cwd, &["rev-parse", "HEAD^{tree}"])?)?,
        }),
        toolchain: identity,
        offline: false,
        store: None,
        probe: &blaze_spec::platform::System,
    })
}

/// The shipped answer table reads toolchain records, never executes its rustc.
fn host() -> anyhow::Result<(Host, StoreIdentity)> {
    let home = std::env::var_os("RUSTUP_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|it| PathBuf::from(it).join(".rustup")))
        .context("cannot find the toolchain records")?;
    let mut dirs: Vec<_> = std::fs::read_dir(home.join("toolchains"))?
        .filter_map(Result::ok)
        .filter(|entry| {
            entry.file_name().to_string_lossy().starts_with(super::toolchain::OURS.trim())
        })
        .map(|entry| entry.path())
        .collect();
    dirs.sort();
    let rustc =
        dirs.first().context("blaze's release is not installed in rustup")?.join("bin/rustc");
    let answer = |args: &[&str]| -> anyhow::Result<String> {
        let words: Vec<OsString> = args.iter().map(OsString::from).collect();
        let bytes = crate::answers::own_answer(&rustc, &words)
            .context("blaze has no ported answer for its toolchain records")?;
        Ok(String::from_utf8(bytes)?)
    };
    let verbose = answer(&["-vV"])?;
    let triple =
        verbose.lines().find_map(|line| line.strip_prefix("host: ")).context("no host")?.to_owned();
    let commit = verbose
        .lines()
        .find_map(|line| line.strip_prefix("commit-hash: "))
        .context("no toolchain commit")?;
    let cfg = answer(&["--print", "cfg", "--target", &triple])?
        .lines()
        .map(str::parse)
        .collect::<Result<Vec<_>, _>>()?;
    let build = match option_env!("RA_JIT_SOURCES") {
        Some(sources) => [&b"sources "[..], sources.as_bytes()].concat(),
        None => Sha256::digest(std::fs::read(std::env::current_exe()?)?).to_vec(),
    };
    let cpu = hir_ty_codegen::Target::select(&triple).map_err(anyhow::Error::msg)?.cpu();
    Ok((
        Host { triple, cfg, cpu: cpu.to_owned(), llvm: cfg!(feature = "llvm") },
        StoreIdentity::from_store(ra_jit_store::FORMAT, &build, commit),
    ))
}

fn unit_graph(request: &Request, ctx: &Context<'_>) -> anyhow::Result<serde_json::Value> {
    let asked = blaze_metadata::Asked {
        cwd: ctx.cwd,
        env: ctx.env.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
        manifest_path: request.manifest_path.as_deref(),
        config: &request.config,
        features: &request.features,
        all_features: request.all_features,
        no_default_features: request.no_default_features,
        build: Some(blaze_metadata::Build {
            packages: &request.packages,
            workspace: request.workspace,
            excludes: &request.excludes,
            dev_units: request.needs_dev_dependencies(),
        }),
        platform: Some(blaze_metadata::TargetPlatform {
            triple: &ctx.host.triple,
            cfg: &ctx.host.cfg,
        }),
        offline: request.offline,
        store: None,
    };
    let targets = if request.doc {
        blaze_metadata::units::TargetFilter { doc_only: true, ..Default::default() }
    } else {
        request.targets.clone()
    };
    let units = blaze_metadata::units::UnitsAsked {
        asked: &asked,
        targets: &targets,
        profile: request.profile.as_deref(),
        command: match request.command {
            Some(Subcommand::Check) => blaze_metadata::units::Command::Check,
            Some(Subcommand::Test) => blaze_metadata::units::Command::Test,
            _ => blaze_metadata::units::Command::Build,
        },
    };
    Ok(blaze_metadata::units::unit_graph(&units)?.to_cargo_json())
}

fn replay(file: &Path) -> anyhow::Result<i32> {
    let given: BuildSpec =
        blaze_wire::decode(&std::fs::read(file)?, &blaze_wire::Limits::default())?;
    let root = PathBuf::from(git(&std::env::current_dir()?, &["rev-parse", "--show-toplevel"])?);
    let (manifest, cwd) = blaze_spec::run::locations(&given, &root)?;
    let (host, identity) = host()?;
    let mut first = None;
    for candidate in blaze_spec::run::candidates(&given)? {
        let mut env: BTreeMap<String, String> =
            std::env::vars().filter(|(key, _)| !blaze_spec::run::governed(key)).collect();
        env.extend(candidate.env);
        let mut request = Request::parse(candidate.command, &candidate.args)?;
        request.manifest_path = Some(manifest.clone());
        let ctx = context(&cwd, &env, &host, identity, &root)?;
        match blaze_spec::assemble::build_spec(&request, &ctx) {
            Ok(made) if made.build == given => {
                // The existing dispatcher can hand cold libraries, tests and doctests to
                // cargo. Never add a spec execution entry to that fallback path.
                bail!(
                    "spec execution is not connected to an owned unit executor; no cargo or rustc was run"
                );
            }
            Ok(made) => {
                first.get_or_insert_with(|| {
                    blaze_spec::run::first_difference(&given, &made.build).unwrap()
                });
            }
            Err(why) => {
                first.get_or_insert(why.to_string());
            }
        }
    }
    eprintln!(
        "blaze: the tree does not match the spec: {}",
        first.unwrap_or_else(|| "no reproduction".to_owned())
    );
    Ok(2)
}
