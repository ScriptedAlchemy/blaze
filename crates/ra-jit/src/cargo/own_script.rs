//! Build scripts compiled and linked by ra-jit (docs/no-rustc.md, step I). When cargo asks
//! rustc for a build script, the wrapper describes the script and the libraries it links, from
//! the arguments cargo gave and the records of those libraries, as a `rust-project.json`, and
//! ra-jit builds it as it builds any program with nothing of rustc's: every crate from source,
//! over the standard library's unit from the store, linked by `cc`. It writes the executable
//! and the dep-info where rustc would have, with the files `include!` and its kin read and
//! the variables `env!` and `option_env!` read, and cargo runs the script as it would rustc's.
//! A script that read a variable cargo sets for rustc alone is left to rustc. A proc macro the
//! script links is expanded with the library compiled of it: rustc's by rust-analyzer's
//! server, ra-jit's (`own_macro.rs`) by the server that reads its list of macros.

use std::{
    collections::BTreeMap,
    ffi::{OsStr, OsString},
    io::Write as _,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

use serde_json::{Value, json};

use super::units::Record;

/// The variable that has rustc compile the build scripts, as the harness's reference builds do.
pub(crate) const BY_RUSTC: &str = "RA_JIT_SCRIPTS_BY_RUSTC";

/// The name of the script's crate in the description: the one ra-jit is asked to build.
const SCRIPT: &str = "build_script_build";

/// The variables cargo sets for rustc that a crate's source reads with `env!`: those of its
/// package and the `OUT_DIR` of its build script.
pub(crate) fn read_by_crates(name: &str) -> bool {
    name.starts_with("CARGO_PKG_")
        || name.starts_with("CARGO_MANIFEST_")
        || matches!(name, "CARGO_CRATE_NAME" | "CARGO_PRIMARY_PACKAGE" | "OUT_DIR" | "CARGO")
}

/// A crate of the description, as rustc was told to compile it.
struct Described {
    name: String,
    root: PathBuf,
    edition: String,
    cfg: Vec<String>,
    codegen_flags: Vec<String>,
    env: BTreeMap<String, String>,
    /// The name each library is linked under and where it is in the description.
    deps: Vec<(String, usize)>,
    /// The library compiled of a proc macro, which a proc-macro server expands with.
    proc_macro: Option<PathBuf>,
    /// Whether the crate is a proc macro the build compiles.
    compiled_macro: bool,
    /// The dep-info of the library rustc compiled of it, and the directory its paths are from.
    inputs: Option<(PathBuf, PathBuf)>,
}

/// What a build makes of the first crate of its description.
pub(crate) struct Made<'a> {
    /// The crate's name in the description, which ra-jit is asked to build.
    pub(crate) name: &'a str,
    /// Whether the crate is a proc macro, linked as a shared library with its list of macros.
    pub(crate) proc_macro: bool,
    pub(crate) exe: PathBuf,
    pub(crate) dep_info: PathBuf,
}

/// The arguments of one run of rustc, as cargo gave them for a crate.
struct Asked<'a> {
    args: &'a [String],
    cwd: &'a Path,
}

impl Asked<'_> {
    fn value(&self, flag: &str) -> Option<&str> {
        let joined = format!("{flag}=");
        let mut args = self.args.iter();
        while let Some(arg) = args.next() {
            if arg == flag {
                return args.next().map(String::as_str);
            }
            if let Some(value) = arg.strip_prefix(&joined) {
                return Some(value);
            }
        }
        None
    }

    fn values(&self, flag: &str) -> Vec<&str> {
        self.args.windows(2).filter(|it| it[0] == flag).map(|it| it[1].as_str()).collect()
    }

    /// `-C name=value`, however cargo spelled it.
    fn codegen(&self, name: &str) -> Option<&str> {
        let joined = self.args.iter().filter_map(|it| it.strip_prefix("-C"));
        let split = self.values("-C").into_iter();
        joined.chain(split).filter(|it| !it.is_empty()).find_map(|it| {
            let (key, value) = it.split_once('=').unwrap_or((it, ""));
            (key == name).then_some(value)
        })
    }

    /// The cfgs rustc compiles the crate with beside the target's: `--cfg`, and
    /// `debug_assertions`, which rustc sets unless it optimizes or is told not to.
    fn cfg(&self) -> Vec<String> {
        let mut cfg: Vec<String> = self.values("--cfg").into_iter().map(str::to_owned).collect();
        let optimized = self.codegen("opt-level").is_some_and(|it| it != "0");
        let asserts = match self.codegen("debug-assertions") {
            Some(it) => matches!(it, "" | "y" | "yes" | "on" | "true"),
            None => !optimized,
        };
        if asserts {
            cfg.push("debug_assertions".to_owned());
        }
        cfg
    }

    fn edition(&self) -> String {
        self.value("--edition").unwrap_or("2015").to_owned()
    }

    /// The libraries it links: the name each is linked under and its file.
    fn externs(&self) -> Vec<(String, PathBuf)> {
        let named = self.values("--extern").into_iter().filter_map(|it| it.split_once('='));
        named.map(|(name, path)| (name.to_owned(), self.cwd.join(path))).collect()
    }
}

/// Compiles and links the build script cargo asked rustc for with `args`, in `cwd`, into `exe`
/// with its dep-info beside it, as rustc would have, describing it in `dir`. `rustc` is the
/// compiler cargo named, whose toolchain has the standard library's source. `Err` is why rustc
/// is to compile it.
pub(crate) fn compile(
    rustc: &OsStr,
    args: &[String],
    cwd: &Path,
    source: &Path,
    exe: &Path,
    dir: &Path,
) -> Result<(), String> {
    let made = Made {
        name: SCRIPT,
        proc_macro: false,
        exe: exe.to_owned(),
        dep_info: exe.with_extension("d"),
    };
    compile_as(rustc, args, cwd, source, &made, dir)
}

/// [`compile`] of the crate cargo asked rustc for, made as `made` says.
pub(crate) fn compile_as(
    rustc: &OsStr,
    args: &[String],
    cwd: &Path,
    source: &Path,
    made: &Made<'_>,
    dir: &Path,
) -> Result<(), String> {
    let rustc = crate::answers::toolchain_rustc_of(rustc).ok_or("its toolchain is not known")?;
    let sysroot = rustc.parent().and_then(Path::parent).ok_or("its toolchain is not known")?;
    let env: BTreeMap<String, String> =
        crate::process::vars().filter(|(name, _)| read_by_crates(name)).collect();
    let asked = Asked { args, cwd };
    compile_in(sysroot, &asked, source, env, made, dir, &|name| std::env::var(name).ok())
}

/// [`compile_as`] of the crate rustc compiled as `record` says, over the standard library of
/// `sysroot`, with the variables cargo set for it then.
pub(crate) fn compile_record(
    sysroot: &Path,
    record: &Record,
    made: &Made<'_>,
    dir: &Path,
) -> Result<(), String> {
    let asked = Asked { args: &record.args, cwd: &record.cwd };
    let env = record.crate_env.clone();
    let rustc_had = |name: &str| record.crate_env.get(name).cloned().or(std::env::var(name).ok());
    compile_in(sysroot, &asked, &record.source, env, made, dir, &rustc_had)
}

/// [`compile_as`] of the crate rustc was `asked` for, the variables its source reads by `env!`
/// being `env` and the others what `rustc_had` says.
fn compile_in(
    sysroot: &Path,
    asked: &Asked<'_>,
    source: &Path,
    env: BTreeMap<String, String>,
    made: &Made<'_>,
    dir: &Path,
    rustc_had: &dyn Fn(&str) -> Option<String>,
) -> Result<(), String> {
    let library = sysroot.join("lib/rustlib/src/rust/library");
    if !library.is_dir() {
        return Err("the toolchain has no source of the standard library".to_owned());
    }
    let mut crates = Vec::new();
    let first = Described {
        name: made.name.to_owned(),
        root: source.to_owned(),
        edition: asked.edition(),
        cfg: asked.cfg(),
        codegen_flags: asked.args.to_vec(),
        env,
        deps: Vec::new(),
        proc_macro: None,
        compiled_macro: made.proc_macro,
        inputs: None,
    };
    crates.push(first);
    let mut linked = BTreeMap::new();
    let deps = asked.externs();
    let deps =
        deps.iter().map(|(name, path)| Ok((name.clone(), add(&mut crates, &mut linked, path)?)));
    crates[0].deps = deps.collect::<Result<_, String>>()?;
    let described = description(sysroot, &crates);
    // A proc macro's library is kept in the store, by its description.
    let key = made.proc_macro.then(|| super::own_macro::library_key(&described));
    if !key.is_some_and(|key| super::own_macro::restored(&key, made)) {
        build(&described, dir, made, super::daemon::store(), true, None)?;
        if let Some(key) = key {
            let inputs = crates.iter().filter_map(|it| it.inputs.as_ref());
            super::own_macro::keep(
                &key,
                made,
                &dir.join("rust-project.json"),
                inputs.map(|(info, cwd)| (&**info, &**cwd)),
            );
        }
    }
    let read = std::fs::read_to_string(&made.dep_info).map_err(|it| it.to_string())?;
    read_otherwise(&crate::dep_info::DepInfo::parse(&read), rustc_had)
}

/// The `rust-project.json` of `crates` over the standard library of `sysroot`.
fn description(sysroot: &Path, crates: &[Described]) -> String {
    source_description(sysroot, crates, false)
}

fn source_description(sysroot: &Path, crates: &[Described], native: bool) -> String {
    let path = |path: &Path| {
        if native { crate::compiler_json::path(path) } else { json!(path) }
    };
    json!({
        "sysroot": path(sysroot),
        "sysroot_src": path(&sysroot.join("lib/rustlib/src/rust/library")),
        "crates": crates.iter().enumerate().map(|(at, it)| described(it, at == 0, native)).collect::<Vec<_>>(),
    })
    .to_string()
}

/// Builds the first crate of `described`, a [`description`], as `made` says, describing them
/// in `dir`, with `store` as the store: what the build said, or why it did not build. With
/// `shipped` it fetches the standard library as builds do (`shipped.rs`). `asked` is the
/// variables to run the build with in place of this process's, and where to write the
/// dep-info of every crate the program links.
fn build(
    described: &str,
    dir: &Path,
    made: &Made<'_>,
    store: Option<PathBuf>,
    shipped: bool,
    asked: Option<(&[(OsString, OsString)], &Path)>,
) -> Result<String, String> {
    std::fs::create_dir_all(dir).map_err(|it| it.to_string())?;
    let project = dir.join("rust-project.json");
    let private: Value = serde_json::from_str(described).map_err(|it| it.to_string())?;
    let private = crate::compiler_json::write(&project, &private).map_err(|it| it.to_string())?;
    let executable = std::env::current_exe().map_err(|it| it.to_string())?;
    let mut ra_jit = build_command(&executable, &project, dir, made, store, shipped, asked);
    ra_jit.env("RA_JIT_PROJECT_STDIN", "1").stdin(Stdio::piped()).stderr(Stdio::piped());
    let mut child = ra_jit.spawn().map_err(|it| format!("ra-jit did not run: {it}"))?;
    let sent = child.stdin.take().ok_or("project pipe")?.write_all(&private);
    if let Err(error) = sent {
        let _ = child.kill();
        let _ = child.wait();
        return Err(format!("sending owned project: {error}"));
    }
    let built = child.wait_with_output().map_err(|it| format!("waiting for ra-jit: {it}"))?;
    let said = String::from_utf8_lossy(&built.stderr).into_owned();
    std::fs::write(dir.join("ra-jit.log"), &said).map_err(|it| it.to_string())?;
    if !built.status.success() {
        if let Some(at) = said.find("error:") {
            return Err(format!("ra-jit did not build it ({}):\n{}", built.status, &said[at..]));
        }
        let last = said.lines().rev().find(|it| !it.trim().is_empty()).unwrap_or("");
        return Err(format!("ra-jit did not build it ({}): {last}", built.status));
    }
    complete(&said)?;
    Ok(said)
}

fn complete(said: &str) -> Result<(), String> {
    let mut links = 0;
    for line in said.lines().filter(|line| line.starts_with("linked ")) {
        let Some((_, count)) = line.rsplit_once(", ") else { continue };
        let Some(count) = count.strip_suffix(" functions in it ra-jit could not compile") else {
            continue;
        };
        if count.parse::<u64>().map_err(|_| "invalid producer link report")? != 0 {
            return Err(format!("it calls functions blaze could not compile ({line})"));
        }
        links += 1;
    }
    if links == 0 {
        return Err("producer compiler did not report a complete native link".into());
    }
    Ok(())
}

fn build_command(
    executable: &Path,
    project: &Path,
    dir: &Path,
    made: &Made<'_>,
    store: Option<PathBuf>,
    shipped: bool,
    asked: Option<(&[(OsString, OsString)], &Path)>,
) -> Command {
    let mut ra_jit = Command::new(executable);
    ra_jit.arg(&project).args(["--crate", made.name]).arg("--out").arg(dir.join("built.o"));
    ra_jit.arg("--exe").arg(&made.exe).arg("--dep-info").arg(&made.dep_info);
    if made.proc_macro {
        ra_jit.args(["--crate", "proc_macro", "--shared"]);
    }
    // What cargo set for the script's rustc is not the build's: the build ra-jit runs is its
    // own, which asks cargo nothing through a wrapper and is no doctest.
    let not_the_builds = |text: &str| {
        let cargos = text.starts_with("CARGO_") && text != "CARGO_HOME";
        cargos || text.starts_with("RUSTC_") || text.starts_with("RA_JIT_") || text == "CARGO"
    };
    if let Some((env, inputs)) = asked {
        ra_jit.env_clear();
        ra_jit.envs(
            env.iter()
                .filter(|(name, _)| !not_the_builds(&name.to_string_lossy()))
                .map(|(k, v)| (k, v)),
        );
        ra_jit.arg("--inputs").arg(inputs);
    }
    for (name, _) in std::env::vars_os() {
        if not_the_builds(&name.to_string_lossy()) {
            ra_jit.env_remove(&name);
        }
    }
    for kept in ["RA_JIT_MEMORY_MAX", "RA_JIT_OWN_BASE_MEMORY", "RA_JIT_SPARE", "RA_JIT_RUN_ID"] {
        if let Some(value) = std::env::var_os(kept).filter(|_| asked.is_some()) {
            ra_jit.env(kept, value);
        }
    }
    for kept in ["RA_JIT_HOME", "RA_JIT_BACKEND", "RA_JIT_STD_FROM", super::own_macro::SERVER] {
        if let Some(value) = std::env::var_os(kept) {
            ra_jit.env(kept, value);
        }
    }
    if let Some(store) = store {
        ra_jit.env("RA_JIT_CACHE", store);
    }
    if !shipped {
        ra_jit.env("RA_JIT_STD_FROM", "off");
    }
    // An owned source build cannot reach the remaining rustc query fallback,
    // including while compiling a producer's build script.
    if asked.is_some() {
        ra_jit.env("RA_JIT_ANSWERS", "own");
        ra_jit.env("BLAZE_ANSWERS", "own");
        ra_jit.env("RA_JIT_OWN_RESOLVE", "1");
        ra_jit.env("BLAZE_OWN_RESOLVE", "1");
    }
    ra_jit.current_dir(dir).stdin(Stdio::null()).stdout(Stdio::null());
    ra_jit
}

/// Builds a script that does nothing over the standard library of `sysroot` in `dir`, into
/// the store `store`: what the store then has is what any build script's build takes from
/// it before its own crates, the standard library's unit and the frozen maps of its crates.
pub(crate) fn build_nothing(sysroot: &Path, dir: &Path, store: &Path) -> Result<String, String> {
    let source = dir.join("build.rs");
    std::fs::create_dir_all(dir).map_err(|it| it.to_string())?;
    std::fs::write(&source, "fn main() {}\n").map_err(|it| it.to_string())?;
    let script = Described {
        name: SCRIPT.to_owned(),
        root: source,
        edition: "2021".to_owned(),
        cfg: vec!["debug_assertions".to_owned()],
        codegen_flags: Vec::new(),
        env: BTreeMap::new(),
        deps: Vec::new(),
        proc_macro: None,
        compiled_macro: false,
        inputs: None,
    };
    let exe = dir.join("build-script-build");
    let made = Made { name: SCRIPT, proc_macro: false, dep_info: exe.with_extension("d"), exe };
    let described = description(sysroot, &[script]);
    build(&described, dir, &made, Some(store.to_owned()), false, None)
}

/// A crate of a build script's description that blaze's runner of build scripts makes from
/// blaze's own reading of the workspace (`crate::scripts`), not from what cargo told rustc.
#[derive(Clone, serde_derive::Deserialize, serde_derive::Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Planned {
    pub(crate) name: String,
    pub(crate) root: PathBuf,
    pub(crate) edition: String,
    pub(crate) cfg: Vec<String>,
    pub(crate) codegen_flags: Vec<String>,
    pub(crate) env: BTreeMap<String, String>,
    /// The name each library is linked under and where it is among the crates.
    pub(crate) deps: Vec<(String, usize)>,
    /// An owned, content-admitted image of a dependency producer. The root being
    /// compiled has no image; dependency producers expand rather than link as Rust.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) proc_macro: Option<PathBuf>,
}

/// The `rust-project.json` of the script that is the first of `crates`, over the standard
/// library of `sysroot`.
pub(crate) fn planned_description(sysroot: &Path, crates: Vec<Planned>) -> String {
    planned_source_description(sysroot, crates, false)
}

#[derive(Clone, Copy)]
pub(crate) enum SourceMode<'a> {
    Legacy,
    Captured {
        profile: Option<&'a Value>,
        units: Option<&'a BTreeMap<u32, blaze_wire::native::Unit>>,
        runtime: Option<&'a Value>,
        bindings: Option<&'a Value>,
    },
}

pub(crate) fn with_mode(described: String, mode: SourceMode<'_>) -> Result<String, String> {
    let (profile, native, runtime, bindings) = match mode {
        SourceMode::Legacy => return Ok(described),
        SourceMode::Captured { profile, units, runtime, bindings } => {
            (profile, units, runtime, bindings)
        }
    };
    let mut project: Value = serde_json::from_str(&described).map_err(|it| it.to_string())?;
    project["compiler_mode"] = Value::Bool(true);
    if let Some(runtime) = runtime {
        project["blaze_owned_proc_macro_server"] = runtime.clone();
    }
    if let Some(bindings) = bindings {
        project[blaze_wire::native::BINDINGS_FIELD] = bindings.clone();
    }
    if let Some(profile) = profile {
        project["blaze_spec_profile"] = profile.clone();
    }
    for (at, source) in
        project["crates"].as_array_mut().ok_or("captured source crates")?.iter_mut().enumerate()
    {
        let kind = if at == 0 && source["is_proc_macro"] != true { "bin" } else { "lib" };
        if let Some(native) = native {
            let id = u32::try_from(at).map_err(|_| "too many captured compiler units")?;
            let unit = native.get(&id).ok_or("missing captured compiler context")?;
            source["blaze_native_unit"] = json!(id);
            source["env"] = json!(
                crate::process::unicode(
                    unit.env.iter().map(|(key, value)| (key.clone(), value.clone()))
                )
                .collect::<BTreeMap<_, _>>()
            );
            source["blaze_invalid_env"] = json!(
                unit.env
                    .iter()
                    .filter_map(|(key, value)| {
                        value.to_str().is_none().then(|| key.to_str()).flatten()
                    })
                    .collect::<Vec<_>>()
            );
        }
        source["build"] = json!({"label": source["display_name"], "build_file": source["root_module"], "target_kind": kind});
    }
    if let Some(native) = native {
        project[blaze_wire::native::UNIT_FIELD] = json!(blaze_wire::native::encode_units(native));
    }
    crate::dep_info::ensure_replay_key(&mut project).map_err(|error| error.to_string())?;
    serde_json::to_string(&project).map_err(|it| it.to_string())
}

/// The source graph of an owned proc-macro producer. No rustc artifact or unit record
/// is needed to identify its root.
pub(crate) fn planned_macro_description(
    sysroot: &Path,
    crates: Vec<Planned>,
) -> Result<String, String> {
    if crates.is_empty() {
        return Err("a proc-macro producer needs a root crate".to_owned());
    }
    for krate in &crates {
        if !krate.root.is_absolute() {
            return Err(format!("the source root of `{}` is not absolute", krate.name));
        }
        if krate.deps.iter().any(|(_, at)| *at >= crates.len()) {
            return Err(format!("a dependency of `{}` is outside the source graph", krate.name));
        }
    }
    fn visit(at: usize, crates: &[Planned], state: &mut [u8]) -> Result<(), String> {
        match state[at] {
            1 => return Err(format!("a cycle in the source graph reaches `{}`", crates[at].name)),
            2 => return Ok(()),
            _ => {}
        }
        state[at] = 1;
        for (_, dep) in &crates[at].deps {
            visit(*dep, crates, state)?;
        }
        state[at] = 2;
        Ok(())
    }
    let mut state = vec![0; crates.len()];
    for at in 0..crates.len() {
        visit(at, &crates, &mut state)?;
    }
    Ok(planned_source_description(sysroot, crates, true))
}

/// Compiles an owned host source graph into a proc-macro image and its complete dep-info.
/// The caller supplies resolved host cfgs, profiles and script outputs, and owns `dir`
/// exclusively until the build ends. This does not read wrapper records, restore supplied
/// libraries or route a cold workspace load. Publication and scheduling belong to the caller.
pub(crate) fn link_macro_planned(
    sysroot: &Path,
    crates: Vec<Planned>,
    dir: &Path,
    env: &[(OsString, OsString)],
    mode: SourceMode<'_>,
) -> Result<PathBuf, String> {
    let name = crates.first().ok_or("a proc-macro producer needs a root crate")?.name.clone();
    let described = with_mode(planned_macro_description(sysroot, crates)?, mode)?;
    let exe =
        dir.join(format!("{}{name}{}", std::env::consts::DLL_PREFIX, std::env::consts::DLL_SUFFIX));
    let made = Made {
        name: &name,
        proc_macro: true,
        dep_info: dir.join(format!("{name}.d")),
        exe: exe.clone(),
    };
    let inputs = dir.join("producer.inputs");
    build(&described, dir, &made, super::daemon::store(), false, Some((env, &inputs)))?;
    if !exe.is_file() || !made.dep_info.is_file() || !inputs.is_file() {
        return Err("the producer build did not write its library and complete dep-info".to_owned());
    }
    Ok(exe)
}

fn planned_source_description(sysroot: &Path, crates: Vec<Planned>, macro_root: bool) -> String {
    let crates: Vec<Described> = crates
        .into_iter()
        .enumerate()
        .map(|(at, it)| Described {
            name: it.name,
            root: it.root,
            edition: it.edition,
            cfg: it.cfg,
            codegen_flags: it.codegen_flags,
            env: it.env,
            deps: it.deps,
            proc_macro: it.proc_macro,
            compiled_macro: macro_root && at == 0,
            inputs: None,
        })
        .collect();
    source_description(sysroot, &crates, true)
}

/// Compiles and links the build script `described` (a [`planned_description`]) into `exe`
/// in `dir`, with its dep-info beside it and the dep-info of every crate it links at
/// `inputs`, the build run with the variables `env` (those of the process that asked for
/// the build) and nothing of the daemon's but what the store and the backend are. What the
/// build said, or why it did not build.
pub(crate) fn link_planned(
    described: &str,
    dir: &Path,
    exe: &Path,
    inputs: &Path,
    env: &[(OsString, OsString)],
) -> Result<String, String> {
    let project: Value = serde_json::from_str(described).map_err(|it| it.to_string())?;
    let name = project["crates"][0]["display_name"].as_str().ok_or("script source root name")?;
    let made =
        Made { name, proc_macro: false, exe: exe.to_owned(), dep_info: exe.with_extension("d") };
    build(described, dir, &made, super::daemon::store(), true, Some((env, inputs)))
}

/// Why rustc is to compile the script when it read a variable as another than rustc would,
/// `rustc_has` saying what rustc would: one cargo sets for rustc alone, which ra-jit's build
/// is not given (`RUSTC_*` and the like).
fn read_otherwise(
    info: &crate::dep_info::DepInfo,
    rustc_has: impl Fn(&str) -> Option<String>,
) -> Result<(), String> {
    match info.env.iter().find(|(name, value)| rustc_has(name) != *value) {
        Some((name, _)) => Err(format!("it reads `{name}`, which ra-jit's build is not given")),
        None => Ok(()),
    }
}

/// Adds the library rustc wrote at `path`, and what it links, to `crates` from their records:
/// where it is in them. `linked` has where each file already is.
fn add(
    crates: &mut Vec<Described>,
    linked: &mut BTreeMap<PathBuf, usize>,
    path: &Path,
) -> Result<usize, String> {
    // A library is named by its metadata to a crate rustc pipelines and by itself to the others.
    let path = &match path.extension().is_some_and(|it| it == "rmeta") {
        true => path.with_extension("rlib"),
        false => path.to_owned(),
    };
    if let Some(&at) = linked.get(path) {
        return Ok(at);
    }
    let name = path.file_name().and_then(|it| it.to_str()).unwrap_or_default();
    let library = name
        .strip_prefix("lib")
        .and_then(|it| it.strip_suffix(".rlib").or(it.strip_suffix(".rmeta")));
    let (prefix, suffix) = (std::env::consts::DLL_PREFIX, std::env::consts::DLL_SUFFIX);
    let proc_macro = name.strip_prefix(prefix).and_then(|it| it.strip_suffix(suffix));
    let Some(unit) = library.or(proc_macro) else {
        return Err(format!("it links {name}, which is not a library ra-jit knows"));
    };
    let dir = path.parent().map(Record::dir_for).ok_or("a library without a directory")?;
    let record = Record::described(&dir, unit).ok_or_else(|| {
        format!("`{unit}`, which it links, was compiled before ra-jit described it")
    })?;
    let asked = Asked { args: &record.args, cwd: &record.cwd };
    let at = crates.len();
    linked.insert(path.to_owned(), at);
    let extra = record.codegen("extra-filename").unwrap_or_default();
    let info = path.with_file_name(format!("{}{extra}.d", record.crate_name));
    crates.push(Described {
        name: record.crate_name.clone(),
        root: record.source.clone(),
        edition: asked.edition(),
        cfg: asked.cfg(),
        codegen_flags: asked.args.to_vec(),
        env: record.crate_env.clone(),
        deps: Vec::new(),
        proc_macro: library.is_none().then(|| path.to_owned()),
        compiled_macro: false,
        inputs: Some((info, record.cwd.clone())),
    });
    // A proc macro's libraries are described too, though the library of it is what
    // expands: what the store keeps of the script is keyed by them.
    let mut deps = Vec::new();
    for (name, path) in asked.externs() {
        deps.push((name, add(crates, linked, &path)?));
    }
    crates[at].deps = deps;
    Ok(at)
}

fn described(krate: &Described, member: bool, native: bool) -> Value {
    let path = |path: &Path| {
        if native { crate::compiler_json::path(path) } else { json!(path) }
    };
    json!({
        "display_name": krate.name,
        "root_module": path(&krate.root),
        "edition": krate.edition,
        "deps": krate.deps.iter().map(|(name, at)| json!({ "crate": at, "name": name })).collect::<Vec<_>>(),
        "cfg": krate.cfg,
        "codegen_flags": krate.codegen_flags,
        "env": krate.env,
        "is_workspace_member": member,
        "is_proc_macro": krate.proc_macro.is_some() || krate.compiled_macro,
        "proc_macro_dylib_path": krate.proc_macro.as_deref().map(path),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn captured_private_contexts_keep_instances_and_non_unicode_presence() {
        use std::os::unix::ffi::OsStringExt as _;
        let source = json!({"root_module": "/source/shared.rs", "display_name": "a", "is_proc_macro": false, "cfg": [], "env": {}});
        let project = json!({"crates": [source.clone(), source]});
        let native = BTreeMap::from([
            (
                0,
                blaze_wire::native::Unit {
                    env: BTreeMap::from([
                        ("TOKEN".into(), "private-a".into()),
                        ("RAW".into(), OsString::from_vec(vec![255])),
                    ]),
                    flags: vec!["-Copt-level=1".into()],
                },
            ),
            (
                1,
                blaze_wire::native::Unit {
                    env: BTreeMap::from([("TOKEN".into(), "private-b".into())]),
                    flags: vec!["-Copt-level=2".into()],
                },
            ),
        ]);
        let described = with_mode(
            project.to_string(),
            SourceMode::Captured {
                profile: None,
                units: Some(&native),
                runtime: None,
                bindings: None,
            },
        )
        .unwrap();
        let private: Value = serde_json::from_str(&described).unwrap();
        assert_eq!(private["crates"][0]["blaze_native_unit"], 0);
        assert_eq!(private["crates"][1]["blaze_native_unit"], 1);
        assert_eq!(private["crates"][0]["env"]["TOKEN"], "private-a");
        assert_eq!(private["crates"][1]["env"]["TOKEN"], "private-b");
        assert_eq!(private["crates"][0]["blaze_invalid_env"], json!(["RAW"]));
        let bytes: Vec<u8> =
            serde_json::from_value(private[blaze_wire::native::UNIT_FIELD].clone()).unwrap();
        assert_eq!(blaze_wire::native::decode_units(&bytes).unwrap(), native);
        let directory = tempfile::tempdir().unwrap();
        let file = directory.path().join("rust-project.json");
        let sent: Value =
            serde_json::from_slice(&crate::compiler_json::write(&file, &private).unwrap()).unwrap();
        assert_eq!(sent[blaze_wire::native::UNIT_FIELD], private[blaze_wire::native::UNIT_FIELD]);
        let public = std::fs::read_to_string(&file).unwrap();
        assert!(!public.contains("private-a") && !public.contains("private-b"));
        assert!(!public.contains(blaze_wire::native::UNIT_FIELD));
    }

    #[test]
    fn every_link_must_report_zero_uncompiled_functions() {
        assert!(complete("").is_err());
        for counts in [vec![0], vec![4], vec![10], vec![0, 10], vec![0, 0]] {
            let said = counts
                .iter()
                .map(|n| {
                    format!("linked image in 1s, {n} functions in it ra-jit could not compile\n")
                })
                .collect::<String>();
            assert_eq!(complete(&said).is_ok(), counts.iter().all(|n| *n == 0));
        }
    }

    #[test]
    fn captured_descriptions_preserve_authoritative_cfg_and_target_kinds() {
        let source = |name: &str, deps| Planned {
            name: name.into(),
            root: std::env::temp_dir().join(format!("{name}.rs")),
            edition: "2021".into(),
            cfg: vec!["captured_feature".into()],
            codegen_flags: vec!["-Copt-level=3".into()],
            env: BTreeMap::new(),
            deps,
            proc_macro: None,
        };
        let crates = vec![source("root", vec![("dep".into(), 1)]), source("dep", vec![])];
        let plain = planned_description(Path::new("/sysroot"), crates.clone());
        assert_eq!(with_mode(plain.clone(), SourceMode::Legacy).unwrap(), plain);
        let profile = json!({"version": 1, "profile": [1, 2], "rustflags": []});
        let captured: Value = serde_json::from_str(
            &with_mode(
                plain,
                SourceMode::Captured {
                    profile: Some(&profile),
                    units: None,
                    runtime: None,
                    bindings: None,
                },
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(captured["compiler_mode"], true);
        assert_eq!(captured["blaze_spec_profile"], profile);
        assert_eq!(captured["crates"][0]["build"]["target_kind"], "bin");
        assert_eq!(captured["crates"][1]["build"]["target_kind"], "lib");
        assert_eq!(captured["crates"][0]["cfg"], json!(["captured_feature"]));
        let macro_graph = planned_macro_description(Path::new("/sysroot"), crates).unwrap();
        let captured: Value = serde_json::from_str(
            &with_mode(
                macro_graph,
                SourceMode::Captured { profile: None, units: None, runtime: None, bindings: None },
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(captured["crates"][0]["build"]["target_kind"], "lib");
        assert_eq!(captured["crates"][0]["is_proc_macro"], true);
    }

    #[test]
    fn planned_macro_root_is_source_only_and_preserves_host_graph() {
        let crates = || {
            vec![
                Planned {
                    name: "derive".into(),
                    root: "/sources/derive/src/lib.rs".into(),
                    edition: "2021".into(),
                    cfg: vec!["feature=\"host-feature\"".into()],
                    codegen_flags: vec!["-Copt-level=1".into()],
                    env: [("PRODUCER".into(), "host".into())].into_iter().collect(),
                    deps: vec![("renamed_dependency".into(), 1)],
                    proc_macro: None,
                },
                Planned {
                    name: "dependency".into(),
                    root: "/sources/dependency/src/lib.rs".into(),
                    edition: "2021".into(),
                    cfg: vec![],
                    codegen_flags: vec![],
                    env: BTreeMap::new(),
                    deps: vec![],
                    proc_macro: None,
                },
            ]
        };
        let sysroot = Path::new("/owned/sysroot");
        let plain: Value = serde_json::from_str(&planned_description(sysroot, crates())).unwrap();
        let mut producer: Value =
            serde_json::from_str(&planned_macro_description(sysroot, crates()).unwrap()).unwrap();
        assert_eq!(producer["crates"][0]["is_proc_macro"], true);
        assert_eq!(producer["crates"][1]["is_proc_macro"], false);
        assert!(
            producer["crates"]
                .as_array()
                .unwrap()
                .iter()
                .all(|krate| krate["proc_macro_dylib_path"].is_null())
        );
        producer["crates"][0]["is_proc_macro"] = json!(false);
        assert_eq!(plain, producer, "only the root's producer kind may change");
        assert!(planned_macro_description(sysroot, vec![]).is_err());
    }

    fn source_crate(name: &str, deps: Vec<(String, usize)>) -> Planned {
        Planned {
            name: name.into(),
            root: format!("/sources/{name}/src/lib.rs").into(),
            edition: "2021".into(),
            cfg: vec![],
            codegen_flags: vec![],
            env: BTreeMap::new(),
            deps,
            proc_macro: None,
        }
    }

    #[test]
    fn planned_macro_rejects_artifacts_and_invalid_edges_before_building() {
        let sysroot = Path::new("/owned/sysroot");
        let invalid = vec![source_crate("derive", vec![("missing".into(), 1)])];
        assert!(planned_macro_description(sysroot, invalid).unwrap_err().contains("outside"));
        let cycle = vec![
            source_crate("derive", vec![("dep".into(), 1)]),
            source_crate("dep", vec![("root".into(), 0)]),
        ];
        assert!(planned_macro_description(sysroot, cycle).unwrap_err().contains("cycle"));
        let mut relative = source_crate("derive", vec![]);
        relative.root = "src/lib.rs".into();
        assert!(planned_macro_description(sysroot, vec![relative]).is_err());
        let mut supplied = json!({
            "name": "derive", "root": "/sources/derive/src/lib.rs", "edition": "2021",
            "cfg": [], "codegen_flags": [], "env": {}, "deps": [],
            "proc_macro_dylib_path": "/cargo/libderive.dylib",
        });
        assert!(serde_json::from_value::<Planned>(supplied.clone()).is_err());
        supplied.as_object_mut().unwrap().remove("proc_macro_dylib_path");
        assert!(serde_json::from_value::<Planned>(supplied).is_ok());
    }

    #[test]
    fn planned_macro_build_uses_owned_compiler_shared_link_and_host_environment() {
        let dir = Path::new("/producer");
        let made = Made {
            name: "derive",
            proc_macro: true,
            exe: dir.join("libderive.dylib"),
            dep_info: dir.join("derive.d"),
        };
        let env = vec![
            ("PATH".into(), "/tools".into()),
            ("CARGO_BUILD_TARGET".into(), "wasm32-unknown-unknown".into()),
            ("RUSTC_WRAPPER".into(), "/forbidden".into()),
        ];
        let command = build_command(
            Path::new("/blaze/ra-jit"),
            &dir.join("rust-project.json"),
            dir,
            &made,
            None,
            false,
            Some((&env, &dir.join("producer.inputs"))),
        );
        assert_eq!(command.get_program(), "/blaze/ra-jit");
        let args = command.get_args().map(|it| it.to_str().unwrap()).collect::<Vec<_>>();
        assert!(args.windows(3).any(|it| it == ["--crate", "proc_macro", "--shared"]));
        assert!(args.windows(2).any(|it| it == ["--inputs", "/producer/producer.inputs"]));
        assert!(args.windows(2).any(|it| it == ["--exe", "/producer/libderive.dylib"]));
        let env = command.get_envs().collect::<BTreeMap<_, _>>();
        assert_ne!(
            env.get(OsStr::new("CARGO_BUILD_TARGET")).copied().flatten(),
            Some(OsStr::new("wasm32-unknown-unknown"))
        );
        assert_ne!(
            env.get(OsStr::new("RUSTC_WRAPPER")).copied().flatten(),
            Some(OsStr::new("/forbidden"))
        );
        assert_eq!(env.get(OsStr::new("RA_JIT_STD_FROM")), Some(&Some(OsStr::new("off"))));
        assert_eq!(env.get(OsStr::new("RA_JIT_ANSWERS")), Some(&Some(OsStr::new("own"))));
        assert_eq!(env.get(OsStr::new("BLAZE_ANSWERS")), Some(&Some(OsStr::new("own"))));
        assert_eq!(env.get(OsStr::new("RA_JIT_OWN_RESOLVE")), Some(&Some(OsStr::new("1"))));
        assert_eq!(env.get(OsStr::new("BLAZE_OWN_RESOLVE")), Some(&Some(OsStr::new("1"))));
    }

    #[test]
    fn a_variable_read_as_rustc_would_read_it() {
        let info = crate::dep_info::DepInfo {
            files: Vec::new(),
            env: vec![("A".to_owned(), Some("1".to_owned())), ("B".to_owned(), None)],
        };
        let rustc_has = |a: Option<&'static str>| {
            move |name: &str| if name == "A" { a.map(str::to_owned) } else { None }
        };
        assert_eq!(read_otherwise(&info, rustc_has(Some("1"))), Ok(()));
        assert!(read_otherwise(&info, rustc_has(Some("2"))).is_err());
        assert!(read_otherwise(&info, rustc_has(None)).is_err());
    }

    #[test]
    fn debug_assertions_unless_optimized() {
        let cwd = Path::new("/");
        let args = |it: &[&str]| it.iter().map(|it| (*it).to_owned()).collect::<Vec<_>>();
        let plain = args(&["--cfg", "feature=\"std\""]);
        assert_eq!(Asked { args: &plain, cwd }.cfg(), ["feature=\"std\"", "debug_assertions"]);
        let optimized = args(&["-C", "opt-level=3"]);
        assert!(Asked { args: &optimized, cwd }.cfg().is_empty());
        let told = args(&["-C", "opt-level=3", "-Cdebug-assertions=on"]);
        assert_eq!(Asked { args: &told, cwd }.cfg(), ["debug_assertions"]);
    }
}
