//! Request-scoped native execution platforms, with complete SDK and compiler search trees.
//!
//! Tools follow the native `cc` crate's variable precedence and launcher selection. Their
//! executable bytes, launcher bytes and selected arguments contribute to identity. Files,
//! directory membership, missing paths, native names, modes and symlink referents are bound
//! by complete trees. SDK identities do not contain the SDK's physical root location.
//! Every filesystem read and process query goes through `Probe`; `System` uses the requested
//! cwd and explicit environment. `describe` retains the trees for saved-input admission.
//! Requester-supplied executables are bound by bytes and never run while probing a saved job.

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    process::Command,
};

use blaze_wire::{Digest, ExecPlatform};
use sha2::{Digest as _, Sha256};

use crate::Refusal;

/// The tools of `/usr/bin` that are shims: they run what the active developer directory holds,
/// so their bytes name the shim and not the compiler.
const SHIMS: [&str; 6] = ["cc", "c++", "ar", "ld", "clang", "gcc"];

/// The launchers `cc` looks for in front of a compiler, by file stem (`cc` 1.4.3,
/// `known_wrappers`).
const LAUNCHERS: [&str; 7] =
    ["ccache", "distcc", "sccache", "icecc", "cachepot", "buildcache", "kache"];

/// The variable `cc` reads to add a launcher of its own to [`LAUNCHERS`].
const LAUNCHER_VAR: &str = "CC_KNOWN_WRAPPER_CUSTOM";

/// The SDK's settings, which name its version.
const SDK_SETTINGS: &str = "SDKSettings.json";

/// Where macOS's `xcrun` is.
const XCRUN: &str = "/usr/bin/xcrun";

/// rustc's default `MACOSX_DEPLOYMENT_TARGET` for an apple triple, from the target specs in
/// `crates/ra-jit/src/answers/rustc-1.99.0.json`.
const DEPLOYMENT_TARGETS: [(&str, &str); 2] =
    [("aarch64-apple-darwin", "11.0"), ("x86_64-apple-darwin", "10.12")];

/// Where a Linux libc is, in the order blaze looks: Debian's two multiarch paths, then the
/// merged-usr ones, then musl's libc for the four architectures it is packaged for.
const LIBCS: [&str; 8] = [
    "/lib/x86_64-linux-gnu/libc.so.6",
    "/lib/aarch64-linux-gnu/libc.so.6",
    "/lib64/libc.so.6",
    "/usr/lib/libc.so.6",
    "/lib/libc.musl-x86_64.so.1",
    "/lib/libc.musl-aarch64.so.1",
    "/lib/libc.musl-armhf.so.1",
    "/lib/libc.musl-i386.so.1",
];

/// Where a Linux dynamic loader is, in the order blaze looks.
const LOADERS: [&str; 4] = [
    "/lib64/ld-linux-x86-64.so.2",
    "/lib/ld-linux-aarch64.so.1",
    "/lib/ld-musl-x86_64.so.1",
    "/lib/ld-musl-aarch64.so.1",
];

/// What the platform is read from: this machine's programs and files, or canned answers.
pub trait Probe {
    /// Complete native tree, including missing paths and symlink referents.
    fn native_tree(&self, path: &Path) -> std::io::Result<crate::native_tree::Tree>;

    /// Capture one observation of overlapping roots without repeated file reads.
    fn native_trees(&self, paths: &[PathBuf]) -> std::io::Result<Vec<crate::native_tree::Tree>> {
        paths.iter().map(|path| self.native_tree(path)).collect()
    }

    /// Raw process output for native compiler search-path queries.
    fn run_output_in(
        &self,
        cwd: &Path,
        env: &BTreeMap<String, std::ffi::OsString>,
        program: &Path,
        args: &[std::ffi::OsString],
    ) -> Option<(Vec<u8>, Vec<u8>)> {
        self.run_path_in(cwd, env, program, args).map(|stdout| (stdout, Vec::new()))
    }

    /// Lossless native lookup. Existing text-only test probes may override this for raw bytes.
    fn which_native(&self, name: &std::ffi::OsStr, path: &std::ffi::OsStr) -> Option<PathBuf>;

    /// Lossless native invocation arguments.
    fn run_native(&self, program: &Path, args: &[std::ffi::OsString]) -> Option<String>;
    /// Successful stdout bytes for pathname queries, separate from version text.
    fn run_path(&self, program: &Path, args: &[std::ffi::OsString]) -> Option<Vec<u8>>;
    /// `name` found in the directories of `path`, a PATH value: the first directory that holds
    /// it, and `None` when none does.
    fn which(&self, name: &str, path: &str) -> Option<std::path::PathBuf>;

    /// What `program` with `args` wrote to standard output and then standard error, when it
    /// succeeded; `None` when it did not run or did not succeed.
    fn run(&self, program: &std::path::Path, args: &[&str]) -> Option<String>;

    /// The bytes of the file `path`, following a symlink.
    fn read(&self, path: &std::path::Path) -> Option<Vec<u8>>;

    /// The operating system and its version: `"macos 26.6.2"`, `"linux 6.8"`.
    fn os(&self) -> Option<String>;

    /// Native lookup in a request's directory. Canned probes may keep their existing lookup;
    /// filesystem probes must resolve relative and empty PATH entries against `cwd`.
    fn which_native_in(
        &self,
        _cwd: &Path,
        name: &std::ffi::OsStr,
        path: &std::ffi::OsStr,
    ) -> Option<PathBuf> {
        self.which_native(name, path)
    }

    /// Text lookup in a request's directory, with the same defaults as native lookup.
    fn which_in(&self, _cwd: &Path, name: &str, path: &str) -> Option<PathBuf> {
        self.which(name, path)
    }

    /// Invoke the program in the request's directory and exact environment.
    /// Canned probes may return a fixed answer; filesystem probes must not inherit ambient variables.
    fn run_in(
        &self,
        _cwd: &Path,
        _env: &BTreeMap<String, std::ffi::OsString>,
        program: &Path,
        args: &[&str],
    ) -> Option<String> {
        self.run(program, args)
    }

    /// Native invocation in a request's directory.
    fn run_native_in(
        &self,
        _cwd: &Path,
        _env: &BTreeMap<String, std::ffi::OsString>,
        program: &Path,
        args: &[std::ffi::OsString],
    ) -> Option<String> {
        self.run_native(program, args)
    }

    /// Pathname-query invocation in a request's directory.
    fn run_path_in(
        &self,
        _cwd: &Path,
        _env: &BTreeMap<String, std::ffi::OsString>,
        program: &Path,
        args: &[std::ffi::OsString],
    ) -> Option<Vec<u8>> {
        self.run_path(program, args)
    }

    /// Operating-system queries in a request's directory.
    fn os_in(&self, _cwd: &Path, _env: &BTreeMap<String, std::ffi::OsString>) -> Option<String> {
        self.os()
    }
}

/// Bind path resolution and child processes to one request, leaving its environment values
/// unchanged for serialization. In particular, do not rebuild PATH with an absolute prefix:
/// the request's directory may itself contain a PATH separator.
struct InDirectory<'a> {
    probe: &'a dyn Probe,
    cwd: &'a Path,
    env: &'a BTreeMap<String, std::ffi::OsString>,
    /// The requester's tree by its canonical name, when the machine has one.
    requester: Option<PathBuf>,
}

impl InDirectory<'_> {
    /// Whether `program` is a file of the requester's tree. Such a file is named by its bytes
    /// and never run: the platform is read before the inputs are compiled or admitted, and a
    /// program the requester shipped is job code, not the machine's toolchain.
    fn supplied(&self, program: &Path) -> bool {
        let Some(root) = &self.requester else {
            return false;
        };
        let path = blaze_manifest::paths::normalize_path(&self.cwd.join(program));
        path.starts_with(root) || path.canonicalize().is_ok_and(|path| path.starts_with(root))
    }
}

impl Probe for InDirectory<'_> {
    fn native_trees(&self, paths: &[PathBuf]) -> std::io::Result<Vec<crate::native_tree::Tree>> {
        self.probe.native_trees(&paths.iter().map(|path| self.cwd.join(path)).collect::<Vec<_>>())
    }
    fn native_tree(&self, path: &Path) -> std::io::Result<crate::native_tree::Tree> {
        self.probe.native_tree(&self.cwd.join(path))
    }
    fn run_output_in(
        &self,
        _cwd: &Path,
        env: &BTreeMap<String, std::ffi::OsString>,
        program: &Path,
        args: &[std::ffi::OsString],
    ) -> Option<(Vec<u8>, Vec<u8>)> {
        if self.supplied(program) {
            return None;
        }
        self.probe.run_output_in(self.cwd, env, &self.cwd.join(program), args)
    }
    fn which_native(&self, name: &std::ffi::OsStr, path: &std::ffi::OsStr) -> Option<PathBuf> {
        self.probe.which_native_in(self.cwd, name, path).map(|path| self.cwd.join(path))
    }
    fn which(&self, name: &str, path: &str) -> Option<PathBuf> {
        self.probe.which_in(self.cwd, name, path).map(|path| self.cwd.join(path))
    }
    fn run(&self, program: &Path, args: &[&str]) -> Option<String> {
        if self.supplied(program) {
            return None;
        }
        if args == ["--version"] {
            self.probe.run_in(
                self.cwd,
                &query_environment(self.env).ok()?,
                &self.cwd.join(program),
                args,
            )
        } else {
            self.probe.run_in(self.cwd, self.env, &self.cwd.join(program), args)
        }
    }
    fn run_native(&self, program: &Path, args: &[std::ffi::OsString]) -> Option<String> {
        if self.supplied(program) {
            return None;
        }
        self.probe.run_native_in(self.cwd, self.env, &self.cwd.join(program), args)
    }
    fn run_path(&self, program: &Path, args: &[std::ffi::OsString]) -> Option<Vec<u8>> {
        if self.supplied(program) {
            return None;
        }
        self.probe.run_path_in(self.cwd, self.env, &self.cwd.join(program), args)
    }
    fn read(&self, path: &Path) -> Option<Vec<u8>> {
        self.probe.read(&self.cwd.join(path))
    }
    fn os(&self) -> Option<String> {
        self.probe.os_in(self.cwd, self.env)
    }
}

/// The real probe: it runs this machine's programs and reads its files.
pub struct System;

impl Probe for System {
    fn native_trees(&self, paths: &[PathBuf]) -> std::io::Result<Vec<crate::native_tree::Tree>> {
        crate::native_tree::Tree::capture_many(paths)
    }
    fn native_tree(&self, path: &Path) -> std::io::Result<crate::native_tree::Tree> {
        crate::native_tree::Tree::capture(path)
    }
    fn run_output_in(
        &self,
        cwd: &Path,
        env: &BTreeMap<String, std::ffi::OsString>,
        program: &Path,
        args: &[std::ffi::OsString],
    ) -> Option<(Vec<u8>, Vec<u8>)> {
        let output = Command::new(program)
            .current_dir(cwd)
            .env_clear()
            .envs(env)
            .args(args)
            .output()
            .ok()?;
        output.status.success().then_some((output.stdout, output.stderr))
    }
    fn run_path(&self, program: &Path, args: &[std::ffi::OsString]) -> Option<Vec<u8>> {
        let output = Command::new(program).args(args).output().ok()?;
        output.status.success().then_some(output.stdout)
    }
    fn which_native(&self, name: &std::ffi::OsStr, path: &std::ffi::OsStr) -> Option<PathBuf> {
        std::env::split_paths(path).map(|dir| dir.join(name)).find(|it| it.is_file())
    }

    fn run_native(&self, program: &Path, args: &[std::ffi::OsString]) -> Option<String> {
        text_output(Command::new(program).args(args))
    }
    fn which(&self, name: &str, path: &str) -> Option<PathBuf> {
        std::env::split_paths(path).map(|dir| dir.join(name)).find(|it| it.is_file())
    }

    fn run(&self, program: &Path, args: &[&str]) -> Option<String> {
        text_output(Command::new(program).args(args))
    }

    fn read(&self, path: &Path) -> Option<Vec<u8>> {
        std::fs::read(path).ok()
    }

    fn which_native_in(
        &self,
        cwd: &Path,
        name: &std::ffi::OsStr,
        path: &std::ffi::OsStr,
    ) -> Option<PathBuf> {
        std::env::split_paths(path).map(|dir| cwd.join(dir).join(name)).find(|it| it.is_file())
    }

    fn which_in(&self, cwd: &Path, name: &str, path: &str) -> Option<PathBuf> {
        self.which_native_in(cwd, name.as_ref(), path.as_ref())
    }

    fn run_in(
        &self,
        cwd: &Path,
        env: &BTreeMap<String, std::ffi::OsString>,
        program: &Path,
        args: &[&str],
    ) -> Option<String> {
        text_output(Command::new(program).current_dir(cwd).env_clear().envs(env).args(args))
    }

    fn run_native_in(
        &self,
        cwd: &Path,
        env: &BTreeMap<String, std::ffi::OsString>,
        program: &Path,
        args: &[std::ffi::OsString],
    ) -> Option<String> {
        text_output(Command::new(program).current_dir(cwd).env_clear().envs(env).args(args))
    }

    fn run_path_in(
        &self,
        cwd: &Path,
        env: &BTreeMap<String, std::ffi::OsString>,
        program: &Path,
        args: &[std::ffi::OsString],
    ) -> Option<Vec<u8>> {
        let output = Command::new(program)
            .current_dir(cwd)
            .env_clear()
            .envs(env)
            .args(args)
            .output()
            .ok()?;
        output.status.success().then_some(output.stdout)
    }

    fn os(&self) -> Option<String> {
        self.os_in(Path::new("."), &BTreeMap::new())
    }

    fn os_in(&self, cwd: &Path, env: &BTreeMap<String, std::ffi::OsString>) -> Option<String> {
        // The OS this binary was built for, which blaze's triple says it is not cross-building
        // away from.
        match std::env::consts::OS {
            "macos" => {
                let version =
                    self.run_in(cwd, env, Path::new("/usr/bin/sw_vers"), &["-productVersion"])?;
                let version = version.trim();
                (!version.is_empty()).then(|| format!("macos {version}"))
            }
            "linux" => {
                // Kernel facts do not require a distribution's layout of coreutils.
                // Keep uname for environments that do not mount procfs.
                let release =
                    std::fs::read_to_string("/proc/sys/kernel/osrelease").ok().or_else(|| {
                        ["/usr/bin/uname", "/bin/uname"]
                            .iter()
                            .find_map(|it| self.run_in(cwd, env, Path::new(it), &["-r"]))
                    })?;
                let release = release.trim();
                (!release.is_empty()).then(|| format!("linux {}", major_minor(release)))
            }
            _ => None,
        }
    }
}

fn text_output(command: &mut Command) -> Option<String> {
    let output = command.output().ok()?;
    if !output.status.success() {
        return None;
    }
    let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
    text += &String::from_utf8_lossy(&output.stderr);
    Some(text)
}

/// The machine a platform is computed for: what cargo's configuration and the environment say.
pub struct Machine<'a> {
    /// The requester's working directory, absolute, for relative native inputs and probes.
    pub cwd: &'a Path,
    /// The target triple of the blaze binary: blaze does not cross-build.
    pub triple: &'a str,
    /// The target CPU and features codegen assumes.
    pub cpu: &'a str,
    /// The environment build scripts and the link see.
    pub env: &'a std::collections::BTreeMap<String, std::ffi::OsString>,
    /// `target.<triple>.linker` from cargo's configuration, if it is set.
    pub linker: Option<&'a str>,
}

/// The `ExecPlatform` of `machine`, read through `probe`, or a [`Refusal`] naming the field
/// blaze cannot name.
pub fn exec_platform(machine: &Machine<'_>, probe: &dyn Probe) -> Result<ExecPlatform, Refusal> {
    exec_platform_of(machine, probe, None)
}

/// Bound native identity without executing tools supplied in the requester's tree.
pub fn exec_platform_of(
    machine: &Machine<'_>,
    probe: &dyn Probe,
    requester: Option<&Path>,
) -> Result<ExecPlatform, Refusal> {
    Ok(describe_of(machine, probe, requester)?.platform)
}

/// Native platform identity together with the complete trees that justify it.
pub struct Description {
    /// The wire execution platform.
    pub platform: ExecPlatform,
    /// Complete native input bindings, available to the saved executor.
    pub native: crate::native::Closure,
}

/// Discover the platform and retain its native closure in one observation.
pub fn describe(machine: &Machine<'_>, probe: &dyn Probe) -> Result<Description, Refusal> {
    describe_of(machine, probe, None)
}

/// Retain the complete native closure while keeping requester tools inert during discovery.
pub fn describe_of(
    machine: &Machine<'_>,
    probe: &dyn Probe,
    requester: Option<&Path>,
) -> Result<Description, Refusal> {
    discover(machine, probe, true, requester)
}

/// Native tool facts for live planning, without claiming a bound input closure.
pub fn facts(machine: &Machine<'_>, probe: &dyn Probe) -> Result<ExecPlatform, Refusal> {
    Ok(discover(machine, probe, false, None)?.platform)
}

fn discover(
    machine: &Machine<'_>,
    probe: &dyn Probe,
    bind_native: bool,
    requester: Option<&Path>,
) -> Result<Description, Refusal> {
    let requester = requester.map(|root| {
        let root = blaze_manifest::paths::normalize_path(root);
        root.canonicalize().unwrap_or(root)
    });
    let scoped = InDirectory { probe, cwd: machine.cwd, env: machine.env, requester };
    let probe: &dyn Probe = &scoped;
    let os = probe
        .os()
        .ok_or_else(|| Refusal::new("os", "the machine's operating system cannot be named"))?;
    let family = Family::of_os(&os).ok_or_else(|| {
        Refusal::new(os.clone(), "blaze names the native side of macOS and Linux")
    })?;
    let env = machine.env;
    let linker_value = machine.linker.map(str::trim).filter(|it| !it.is_empty()).unwrap_or("cc");
    let linker = resolve(probe, env, std::ffi::OsStr::new(linker_value)).ok_or_else(|| {
        Refusal::new(linker_value, "the linker cannot be resolved: a build cannot link without it")
    })?;
    let cc = tool(probe, env, machine.triple, "CC", "cc");
    let cxx = tool(probe, env, machine.triple, "CXX", "c++");
    let ar = tool(probe, env, machine.triple, "AR", "ar");
    let mut roots = Vec::new();
    let (sdk, libc) = match family {
        Family::Macos => {
            let (sdk, dir) = macos_sdk(probe, env, machine.triple)?;
            roots.push(("sdk".to_owned(), dir));
            (Some(sdk), None)
        }
        Family::Linux => {
            let libc = linux_libc(probe, env)?;
            for path in LIBCS.iter().chain(LOADERS.iter()) {
                roots.push((format!("system{path}"), PathBuf::from(path)));
            }
            for path in ["/usr/include", "/usr/local/include"] {
                roots.push((format!("system{path}"), PathBuf::from(path)));
            }
            (None, Some(libc))
        }
    };
    let mut native = crate::native::Closure::default();
    if bind_native {
        let queries = NativeQueries {
            probe,
            env,
            triple: machine.triple,
            cc: cc.as_ref(),
            cxx: cxx.as_ref(),
            linker: &linker,
        };
        for (role, compiler, language) in
            [("cc", cc.as_ref(), "c"), ("cxx", cxx.as_ref(), "c++"), ("linker", Some(&linker), "c")]
        {
            if let Some(compiler) = compiler {
                compiler_roots(probe, env, role, compiler, language, &mut roots)?;
                let words = compiler
                    .args
                    .iter()
                    .map(|word| word.as_encoded_bytes().to_vec())
                    .collect::<Vec<_>>();
                if !words.is_empty() {
                    native_flag_roots(role, &words, Some(&queries), Some(compiler), &mut roots)?;
                }
            }
        }
        native_environment_roots(env, machine.triple, Some(&queries), &mut roots)?;
        let paths: Vec<_> = roots.iter().map(|(_, path)| machine.cwd.join(path)).collect();
        let trees = probe.native_trees(&paths).map_err(|error| {
            Refusal::new("native roots", format!("cannot bind the complete native trees: {error}"))
        })?;
        if trees.len() != paths.len() {
            return Err(Refusal::new(
                "native roots",
                "the probe did not return every requested tree",
            ));
        }
        for ((name, _), tree) in roots.into_iter().zip(trees) {
            native.roots.push(crate::native::Root { name, tree });
        }
    }
    let platform = ExecPlatform {
        triple: machine.triple.to_owned(),
        os,
        sdk,
        libc,
        linker: linker.tuple(),
        cpu: machine.cpu.to_owned(),
        cc: cc.map(Tool::tuple),
        cxx: cxx.map(Tool::tuple),
        ar: ar.map(Tool::tuple),
        native_root: bind_native.then(|| native.digest()),
    };
    Ok(Description { platform, native })
}

struct NativeQueries<'a> {
    probe: &'a dyn Probe,
    env: &'a BTreeMap<String, std::ffi::OsString>,
    triple: &'a str,
    cc: Option<&'a Tool>,
    cxx: Option<&'a Tool>,
    linker: &'a Tool,
}

fn native_environment_roots(
    env: &BTreeMap<String, std::ffi::OsString>,
    triple: &str,
    queries: Option<&NativeQueries<'_>>,
    roots: &mut Vec<(String, PathBuf)>,
) -> Result<(), Refusal> {
    for (name, value) in env {
        if name == "LIBRARY_PATH"
            || matches!(
                name.as_str(),
                "CPATH" | "C_INCLUDE_PATH" | "CPLUS_INCLUDE_PATH" | "OBJC_INCLUDE_PATH"
            )
        {
            let role = if name == "LIBRARY_PATH" { "linker/libraries" } else { "native/headers" };
            for (at, path) in std::env::split_paths(value).enumerate() {
                roots.push((format!("{role}/env/{name}/{at}"), path));
            }
        } else if (name.starts_with("NIX_LDFLAGS") || name.starts_with("NIX_CFLAGS_"))
            && crate::environment::is_script_var(name, triple)
        {
            let compiler = queries.and_then(|queries| {
                if name.starts_with("NIX_LDFLAGS") { Some(queries.linker) } else { queries.cc }
            });
            native_flag_roots(name, &flag_words(value, false), queries, compiler, roots)?;
        }
    }
    let shell = env.get("CC_SHELL_ESCAPED_FLAGS").is_some_and(|value| {
        !value.is_empty() && !["0", "false", "no"].iter().any(|off| value == *off)
    });
    let underscored = triple.replace(['-', '.'], "_");
    // cc 1.4.3 appends flags from least to most specific. Keep host and target
    // invocations separate: an argument can span variables within one invocation.
    for base in ["CFLAGS", "CXXFLAGS", "CPPFLAGS", "LDFLAGS"] {
        let mut previous = None;
        for kind in ["HOST", "TARGET"] {
            let mut words = Vec::new();
            for name in [
                base.to_owned(),
                format!("{kind}_{base}"),
                format!("{base}_{underscored}"),
                format!("{base}_{triple}"),
            ] {
                if crate::environment::is_script_var(&name, triple)
                    && let Some(value) = env.get(&name)
                {
                    words.extend(flag_words(value, shell));
                }
            }
            if !words.is_empty() && previous.as_ref() != Some(&words) {
                let compiler = queries.and_then(|queries| match base {
                    "CXXFLAGS" => queries.cxx,
                    "LDFLAGS" => Some(queries.linker),
                    _ => queries.cc,
                });
                native_flag_roots(&format!("{kind}_{base}"), &words, queries, compiler, roots)?;
            }
            previous = Some(words);
        }
    }
    Ok(())
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SearchKind {
    Header,
    Framework,
    Library,
    Sysroot,
    HeaderSysroot,
    Prefix,
    PrefixBefore,
    PrefixAfter,
    SysrootFramework,
    Forced,
}

struct SearchArg {
    flag: &'static [u8],
    value: Vec<u8>,
    kind: SearchKind,
    linker: bool,
    at: usize,
}

fn search_args(words: &[Vec<u8>]) -> Vec<SearchArg> {
    let mut args = Vec::new();
    let mut words = words.iter();
    while let Some(word) = words.next() {
        if let Some(linker) = word.strip_prefix(b"-Wl,") {
            args.extend(linker.split(|byte| *byte == b',').map(|word| (word.to_vec(), true)));
        } else if matches!(word.as_slice(), b"-Xlinker" | b"-Xpreprocessor" | b"-Xclang") {
            if let Some(arg) = words.next() {
                args.push((arg.clone(), word == b"-Xlinker"));
            }
        } else {
            args.push((word.clone(), false));
        }
    }
    let mut paths = Vec::new();
    let mut at = 0;
    while at < args.len() {
        let (word, linker) = &args[at];
        at += 1;
        let flags: &[(&[u8], SearchKind)] = &[
            (b"--sysroot=", SearchKind::Sysroot),
            (b"--sysroot", SearchKind::Sysroot),
            (b"-isysroot", SearchKind::HeaderSysroot),
            (b"-iframeworkwithsysroot", SearchKind::SysrootFramework),
            (b"-iframework", SearchKind::Framework),
            (b"-include-pch", SearchKind::Forced),
            (b"-include-pth", SearchKind::Forced),
            (b"-include", SearchKind::Forced),
            (b"-imacros", SearchKind::Forced),
            (b"-iwithprefixbefore", SearchKind::PrefixBefore),
            (b"-iwithprefix", SearchKind::PrefixAfter),
            (b"-iprefix", SearchKind::Prefix),
            (b"-isystem-after", SearchKind::Header),
            (b"-isystem", SearchKind::Header),
            (b"-idirafter", SearchKind::Header),
            (b"-iquote", SearchKind::Header),
            (b"-L", SearchKind::Library),
            (b"-I", SearchKind::Header),
            (b"-F", SearchKind::Framework),
        ];
        let Some(&(flag, kind)) = flags.iter().find(|(flag, _)| {
            word.starts_with(flag) && (*flag != b"--sysroot" || word.len() == flag.len())
        }) else {
            continue;
        };
        let rest = &word[flag.len()..];
        let value = if rest.is_empty() && !flag.ends_with(b"=") {
            let Some((path, _)) = args.get(at) else { break };
            at += 1;
            path.clone()
        } else {
            rest.to_vec()
        };
        paths.push(SearchArg { flag, value, kind, linker: *linker, at });
    }
    paths
}

fn native_flag_roots(
    name: &str,
    words: &[Vec<u8>],
    queries: Option<&NativeQueries<'_>>,
    compiler: Option<&Tool>,
    roots: &mut Vec<(String, PathBuf)>,
) -> Result<(), Refusal> {
    let mut effective = compiler
        .map(|tool| {
            tool.args.iter().map(|word| word.as_encoded_bytes().to_vec()).collect::<Vec<_>>()
        })
        .unwrap_or_default();
    if effective != words {
        effective.extend_from_slice(words);
    }
    if let Some(queries) = queries {
        for (key, value) in queries.env {
            if (key.starts_with("NIX_CFLAGS_") || key.starts_with("NIX_LDFLAGS"))
                && crate::environment::is_script_var(key, queries.triple)
            {
                effective.extend(flag_words(value, false));
            }
        }
    }
    let paths = search_args(&effective);
    let mut interpreted_headers = false;
    let mut interpreted_libraries = Vec::new();
    for arg in &paths {
        let interpreted = arg.value.starts_with(b"=")
            || arg.value.starts_with(b"$SYSROOT")
            || (arg.flag == b"-I" && arg.value == b"-");
        let role = match arg.kind {
            SearchKind::Header | SearchKind::Framework if interpreted && !arg.linker => {
                interpreted_headers = true;
                continue;
            }
            SearchKind::Prefix
            | SearchKind::PrefixBefore
            | SearchKind::PrefixAfter
            | SearchKind::SysrootFramework => {
                interpreted_headers = true;
                continue;
            }
            SearchKind::Library if interpreted => {
                interpreted_libraries.push(arg);
                continue;
            }
            SearchKind::Header => "native/headers",
            SearchKind::Framework => "linker/frameworks",
            SearchKind::Library => "linker/libraries",
            SearchKind::Sysroot => "native/sysroot",
            SearchKind::HeaderSysroot => "native/header-sysroot",
            SearchKind::Forced => "native/forced-include",
        };
        roots.push((format!("{role}/flags/{name}/{}", arg.at), native_path(&arg.value)?));
    }
    if interpreted_headers || !interpreted_libraries.is_empty() {
        let queries =
            queries.ok_or_else(|| Refusal::new(name, "native search query is unavailable"))?;
        let compiler =
            compiler.ok_or_else(|| Refusal::new(name, "native compiler is not resolved"))?;
        check_compound_controls(
            name,
            &paths,
            queries.env,
            interpreted_headers,
            !interpreted_libraries.is_empty(),
        )?;
        if interpreted_headers {
            interpreted_header_roots(name, words, queries, compiler, roots)?;
        }
        if !interpreted_libraries.is_empty() {
            interpreted_library_roots(
                name,
                words,
                &interpreted_libraries,
                queries,
                compiler,
                roots,
            )?;
        }
    }
    Ok(())
}

fn flag_family(name: &str) -> Option<&'static str> {
    ["CFLAGS", "CXXFLAGS", "CPPFLAGS", "LDFLAGS"].into_iter().find(|base| {
        name == *base
            || name == format!("HOST_{base}")
            || name == format!("TARGET_{base}")
            || name.starts_with(&format!("{base}_"))
    })
}

fn check_compound_controls(
    name: &str,
    current: &[SearchArg],
    env: &BTreeMap<String, std::ffi::OsString>,
    headers: bool,
    libraries: bool,
) -> Result<(), Refusal> {
    for (other, value) in env {
        if (other.starts_with("NIX_CFLAGS_") || other.starts_with("NIX_LDFLAGS"))
            && search_args(&flag_words(value, false))
                .iter()
                .any(|arg| arg.linker && arg.kind == SearchKind::Sysroot)
        {
            return Err(Refusal::new(
                name,
                format!(
                    "{other} forwards a sysroot outside the validated native wrapper query interface"
                ),
            ));
        }
    }
    let Some(family) = flag_family(name) else { return Ok(()) };
    let shell = env.get("CC_SHELL_ESCAPED_FLAGS").is_some_and(|value| {
        !value.is_empty() && !["0", "false", "no"].iter().any(|off| value == *off)
    });
    for (other, value) in env {
        let Some(other_family) = flag_family(other) else { continue };
        if other_family == family {
            continue;
        }
        for control in search_args(&flag_words(value, shell)) {
            let relevant = (control.kind == SearchKind::Sysroot && (headers || libraries))
                || (headers
                    && matches!(control.kind, SearchKind::HeaderSysroot | SearchKind::Prefix));
            if relevant
                && (control.kind == SearchKind::Prefix
                    || current
                        .iter()
                        .rev()
                        .find(|known| known.kind == control.kind)
                        .is_none_or(|known| known.value != control.value))
            {
                return Err(Refusal::new(
                    name,
                    format!(
                        "native search controls also come from {other}; the combined native invocation argv is not captured"
                    ),
                ));
            }
        }
    }
    Ok(())
}

fn safe_search_args(words: &[Vec<u8>]) -> Result<Vec<std::ffi::OsString>, Refusal> {
    let mut args = Vec::new();
    for path in search_args(words) {
        if path.linker || matches!(path.kind, SearchKind::Forced | SearchKind::Library) {
            continue;
        }
        if path.flag == b"-I" && path.value == b"-" {
            args.push("-I-".into());
        } else {
            args.push(
                native_path(path.flag.strip_suffix(b"=").unwrap_or(path.flag))?.into_os_string(),
            );
            args.push(native_path(&path.value)?.into_os_string());
        }
    }
    let mut words = words.iter();
    while let Some(word) = words.next() {
        if matches!(
            word.as_slice(),
            b"-m32"
                | b"-m64"
                | b"-mx32"
                | b"-nostdinc"
                | b"-nostdinc++"
                | b"-nobuiltininc"
                | b"-nostdlibinc"
                | b"-nostdsysteminc"
        ) || [
            b"--target=".as_slice(),
            b"-march=",
            b"-mcpu=",
            b"-mtune=",
            b"-mabi=",
            b"-stdlib=",
            b"-fuse-ld=",
            b"--ld-path=",
            b"--gcc-toolchain=",
            b"-resource-dir=",
        ]
        .iter()
        .any(|prefix| word.starts_with(prefix))
        {
            args.push(native_path(word)?.into_os_string());
        } else if matches!(
            word.as_slice(),
            b"--target" | b"-target" | b"-arch" | b"-B" | b"-gcc-toolchain" | b"-resource-dir"
        ) {
            if let Some(value) = words.next() {
                args.push(native_path(word)?.into_os_string());
                args.push(native_path(value)?.into_os_string());
            }
        } else if word.starts_with(b"-B") && word.len() > 2 {
            args.push(native_path(word)?.into_os_string());
        }
    }
    Ok(args)
}

fn query_environment(
    env: &BTreeMap<String, std::ffi::OsString>,
) -> Result<BTreeMap<String, std::ffi::OsString>, Refusal> {
    let mut clean = env.clone();
    clean.insert("LC_ALL".into(), "C".into());
    let shell = env.get("CC_SHELL_ESCAPED_FLAGS").is_some_and(|value| {
        !value.is_empty() && !["0", "false", "no"].iter().any(|off| value == *off)
    });
    for (name, value) in env {
        let nix = name.starts_with("NIX_CFLAGS_") || name.starts_with("NIX_LDFLAGS");
        let ordinary = ["CFLAGS", "CXXFLAGS", "CPPFLAGS", "LDFLAGS"].iter().any(|base| {
            name == base
                || name == &format!("HOST_{base}")
                || name == &format!("TARGET_{base}")
                || name.starts_with(&format!("{base}_"))
        });
        if nix || ordinary {
            let quoted = ordinary && shell;
            let safe = safe_search_args(&flag_words(value, quoted))?;
            let mut bytes = Vec::new();
            for word in &safe {
                if !bytes.is_empty() {
                    bytes.push(b' ');
                }
                if quoted {
                    bytes.push(b'\'');
                    for byte in word.as_encoded_bytes() {
                        if *byte == b'\'' {
                            bytes.extend_from_slice(b"'\\''");
                        } else {
                            bytes.push(*byte);
                        }
                    }
                    bytes.push(b'\'');
                } else if word.is_empty() {
                    // The only empty operand representable in whitespace mode is
                    // a joined long option such as --sysroot=.
                    bytes.pop();
                    bytes.push(b'=');
                } else {
                    bytes.extend_from_slice(word.as_encoded_bytes());
                }
            }
            clean.insert(name.clone(), native_path(&bytes)?.into_os_string());
        }
    }
    Ok(clean)
}

fn native_query(
    probe: &dyn Probe,
    env: &BTreeMap<String, std::ffi::OsString>,
    compiler: &Tool,
    words: &[Vec<u8>],
    tail: &[std::ffi::OsString],
) -> Option<(Vec<u8>, Vec<u8>)> {
    let tool_words: Vec<_> =
        compiler.args.iter().map(|word| word.as_encoded_bytes().to_vec()).collect();
    let mut args = safe_search_args(&tool_words).ok()?;
    if tool_words != words {
        args.extend(safe_search_args(words).ok()?);
    }
    args.extend_from_slice(tail);
    probe.run_output_in(Path::new("."), &query_environment(env).ok()?, &compiler.path, &args)
}

fn header_search_roots(bytes: &[u8]) -> Result<Option<Vec<PathBuf>>, Refusal> {
    let mut roots = Vec::new();
    let mut listing = false;
    let mut complete = false;
    for line in bytes.split(|byte| *byte == b'\n') {
        if matches!(
            line,
            b"#include <...> search starts here:" | b"#include \"...\" search starts here:"
        ) {
            listing = true;
            continue;
        }
        if line == b"End of search list." {
            complete = true;
            listing = false;
            continue;
        }
        let missing = line.strip_prefix(b"ignoring nonexistent directory \"").and_then(|line| {
            line.strip_suffix(b"\"").or_else(|| line.strip_suffix(b"\" (framework directory)"))
        });
        if let Some(path) = missing {
            roots.push(native_path(path)?);
        } else if listing {
            let path = line.strip_prefix(b" ").unwrap_or(line);
            let path = path.strip_suffix(b" (framework directory)").unwrap_or(path);
            if !path.is_empty() {
                roots.push(native_path(path)?);
            }
        }
    }
    Ok(complete.then_some(roots))
}

fn interpreted_header_roots(
    name: &str,
    words: &[Vec<u8>],
    queries: &NativeQueries<'_>,
    compiler: &Tool,
    roots: &mut Vec<(String, PathBuf)>,
) -> Result<(), Refusal> {
    for path in search_args(words) {
        if path.value.contains(&b'\n') || path.value.contains(&b'\r') {
            return Err(Refusal::new(
                name,
                "native compiler search listing cannot name a newline-bearing path",
            ));
        }
    }
    let language =
        if queries.cxx.is_some_and(|tool| std::ptr::eq(tool, compiler)) { "c++" } else { "c" };
    let (out, err) = native_query(
        queries.probe,
        queries.env,
        compiler,
        words,
        &["-E".into(), "-x".into(), language.into(), "-".into(), "-v".into()],
    )
    .ok_or_else(|| {
        Refusal::new(
            name,
            format!("{} has no successful native header search query", compiler.name),
        )
    })?;
    let paths = header_search_roots(&err)?.or(header_search_roots(&out)?).ok_or_else(|| {
        Refusal::new(
            name,
            format!("{} did not report a complete native header search listing", compiler.name),
        )
    })?;
    roots.extend(
        paths
            .into_iter()
            .enumerate()
            .map(|(at, path)| (format!("native/headers/query/{name}/{at}"), path)),
    );
    Ok(())
}

struct DriverLink {
    program: PathBuf,
    sysroot: Option<PathBuf>,
    args: Vec<Vec<u8>>,
}

fn driver_linker(
    name: &str,
    words: &[Vec<u8>],
    queries: &NativeQueries<'_>,
    compiler: &Tool,
) -> Result<DriverLink, Refusal> {
    if !compiler.version.to_ascii_lowercase().contains("clang")
        && native_query(queries.probe, queries.env, compiler, words, &["-print-sysroot".into()])
            .is_none()
    {
        return Err(Refusal::new(
            name,
            "native compiler has no safe linker driver query interface",
        ));
    }
    let mut tail = Vec::new();
    let mut effective =
        compiler.args.iter().map(|word| word.as_encoded_bytes().to_vec()).collect::<Vec<_>>();
    effective.extend_from_slice(words);
    for arg in search_args(&effective) {
        if arg.linker && arg.kind == SearchKind::Sysroot {
            let mut option = std::ffi::OsString::from("--sysroot=");
            option.push(native_path(&arg.value)?.as_os_str());
            tail.extend([std::ffi::OsString::from("-Xlinker"), option]);
        }
    }
    tail.extend(["-###".into(), "-x".into(), "c".into(), "/dev/null".into()]);
    let (out, err) =
        native_query(queries.probe, queries.env, compiler, words, &tail).ok_or_else(|| {
            Refusal::new(name, "native compiler driver has no safe linker command query")
        })?;
    let mut found = None;
    for line in err.split(|byte| *byte == b'\n').chain(out.split(|byte| *byte == b'\n')) {
        if line
            .windows(2)
            .any(|pair| pair[0] == b'\\' && matches!(pair[1], b'0'..=b'7' | b'x' | b'n' | b'r'))
        {
            continue;
        }
        let args = flag_words(native_path(line)?.as_os_str(), true);
        let Some(program) = args.first() else { continue };
        let program = native_path(program)?;
        if !program.is_absolute()
            || (!args.iter().any(|arg| arg == b"-o")
                && program.file_name() != Some(std::ffi::OsStr::new("collect2")))
            || args.iter().any(|arg| arg.starts_with(b"-cc1"))
            || program.file_name() == Some(std::ffi::OsStr::new("as"))
        {
            continue;
        }
        let paths = search_args(&args[1..]);
        let sysroot = paths
            .iter()
            .rev()
            .find(|arg| arg.kind == SearchKind::Sysroot)
            .map(|arg| native_path(&arg.value))
            .transpose()?;
        found = Some(DriverLink { program, sysroot, args: args[1..].to_vec() });
    }
    found.ok_or_else(|| {
        Refusal::new(name, "native compiler driver has no lossless linker command interface")
    })
}

fn interpreted_library_roots(
    name: &str,
    words: &[Vec<u8>],
    libraries: &[&SearchArg],
    queries: &NativeQueries<'_>,
    compiler: &Tool,
    roots: &mut Vec<(String, PathBuf)>,
) -> Result<(), Refusal> {
    let tool_words: Vec<_> =
        compiler.args.iter().map(|word| word.as_encoded_bytes().to_vec()).collect();
    let mut combined = tool_words;
    combined.extend_from_slice(words);
    let direct = compiler.version.contains("GNU ld")
        || (compiler.version.contains("LLD") && compiler.version.contains("GNU linkers"));
    let driver = if direct { None } else { Some(driver_linker(name, words, queries, compiler)?) };
    let mut linker_name = std::ffi::OsString::from("ld");
    for word in combined.iter().chain(driver.iter().flat_map(|driver| driver.args.iter())) {
        if let Some(value) = word.strip_prefix(b"--ld-path=") {
            linker_name = native_path(value)?.into_os_string();
        } else if let Some(value) = word.strip_prefix(b"-fuse-ld=") {
            linker_name = if value.contains(&b'/') {
                native_path(value)?.into_os_string()
            } else {
                let mut name = std::ffi::OsString::from("ld.");
                name.push(native_path(value)?.as_os_str());
                name
            };
        }
    }
    let selected = if let Some(driver) = &driver
        && driver.program.file_name() != Some(std::ffi::OsStr::new("collect2"))
    {
        driver.program.clone()
    } else if direct {
        compiler.path.clone()
    } else if Path::new(&linker_name).components().count() > 1 {
        PathBuf::from(linker_name)
    } else {
        let mut query = std::ffi::OsString::from("-print-prog-name=");
        query.push(linker_name);
        let (out, _) = native_query(queries.probe, queries.env, compiler, words, &[query])
            .ok_or_else(|| {
                Refusal::new(name, format!("{} cannot name its native linker", compiler.name))
            })?;
        let selected =
            crate::git_path::decode_line(out).map_err(|error| Refusal::new(name, error))?;
        if selected.components().count() > 1 {
            selected
        } else {
            queries
                .probe
                .which_native(
                    selected.as_os_str(),
                    queries.env.get("PATH").map(|value| value.as_os_str()).unwrap_or_default(),
                )
                .ok_or_else(|| Refusal::new(name, "selected native linker cannot be resolved"))?
        }
    };
    let env = query_environment(queries.env)?;
    let (version, _) = queries
        .probe
        .run_output_in(Path::new("."), &env, &selected, &["--version".into()])
        .ok_or_else(|| Refusal::new(name, "selected native linker has no version interface"))?;
    let gnu = version.windows(b"GNU ld".len()).any(|part| part == b"GNU ld");
    let lld = version.windows(b"LLD".len()).any(|part| part == b"LLD")
        && version.windows(b"GNU linkers".len()).any(|part| part == b"GNU linkers");
    if !gnu && !lld {
        return Err(Refusal::new(
            name,
            "selected native linker has no owned sysroot search syntax",
        ));
    }
    let paths = search_args(&combined);
    let explicit = paths
        .iter()
        .rev()
        .find(|path| path.kind == SearchKind::Sysroot && path.linker)
        .or_else(|| paths.iter().rev().find(|path| path.kind == SearchKind::Sysroot));
    let driver_root = driver.as_ref().and_then(|driver| driver.sysroot.clone());
    let root = if direct {
        explicit.map(|path| native_path(&path.value)).transpose()?
    } else {
        driver_root
    };
    let sysroot = if gnu {
        let mut args = Vec::new();
        if let Some(root) = &root {
            let mut option = std::ffi::OsString::from("--sysroot=");
            option.push(root);
            args.push(option);
        }
        args.push("--print-sysroot".into());
        let (out, _) =
            queries.probe.run_output_in(Path::new("."), &env, &selected, &args).ok_or_else(
                || Refusal::new(name, "native GNU linker did not report its effective sysroot"),
            )?;
        crate::git_path::decode_line(out).map_err(|error| Refusal::new(name, error))?
    } else {
        root.unwrap_or_default()
    };
    if let Some(driver) = driver {
        if !roots.iter().any(|(_, root)| driver.program.starts_with(root)) {
            roots.push((format!("native/linker/driver/{name}"), driver.program));
        }
    }
    if !roots.iter().any(|(_, root)| selected.starts_with(root)) {
        roots.push((format!("native/linker/query/{name}"), selected));
    }
    for arg in libraries {
        let path = if lld {
            // lld/ELF/DriverUtils.cpp::findFile uses path::append only for '=';
            // '$SYSROOT' is an ordinary directory spelling, even with --sysroot.
            if let Some(rest) = arg.value.strip_prefix(b"=") {
                let relative = if sysroot.as_os_str().is_empty() {
                    rest
                } else {
                    let start = rest.iter().position(|byte| *byte != b'/').unwrap_or(rest.len());
                    &rest[start..]
                };
                sysroot.join(native_path(relative)?)
            } else {
                native_path(&arg.value)?
            }
        } else {
            let rest = arg
                .value
                .strip_prefix(b"=")
                .or_else(|| arg.value.strip_prefix(b"$SYSROOT"))
                .unwrap();
            let mut path = sysroot.as_os_str().to_owned();
            path.push(native_path(rest)?.as_os_str());
            PathBuf::from(path)
        };
        roots.push((format!("linker/libraries/query/{name}/{}", arg.at), path));
    }
    Ok(())
}

/// cc's default ASCII splitting, or its optional POSIX shlex mode, without loss of native bytes.
fn flag_words(value: &std::ffi::OsStr, shell: bool) -> Vec<Vec<u8>> {
    let bytes = value.as_encoded_bytes();
    if !shell {
        return bytes
            .split(u8::is_ascii_whitespace)
            .filter(|word| !word.is_empty())
            .map(<[u8]>::to_vec)
            .collect();
    }
    let mut words = Vec::new();
    let mut at = 0;
    while at < bytes.len() {
        match bytes[at] {
            b' ' | b'\t' | b'\n' => {
                at += 1;
                continue;
            }
            b'#' => {
                while at < bytes.len() && bytes[at] != b'\n' {
                    at += 1;
                }
                continue;
            }
            _ => {}
        }
        let mut word = Vec::new();
        let mut quote = None;
        while at < bytes.len() {
            let byte = bytes[at];
            at += 1;
            if quote == Some(b'\'') {
                if byte == b'\'' {
                    quote = None;
                } else {
                    word.push(byte);
                }
            } else if byte == b'\\' {
                let Some(&next) = bytes.get(at) else { return words };
                at += 1;
                if quote == Some(b'"') && !matches!(next, b'$' | b'`' | b'"' | b'\\' | b'\n') {
                    word.push(b'\\');
                }
                if next != b'\n' {
                    word.push(next);
                }
            } else if quote == Some(b'"') {
                if byte == b'"' {
                    quote = None;
                } else {
                    word.push(byte);
                }
            } else if matches!(byte, b'\'' | b'"') {
                quote = Some(byte);
            } else if matches!(byte, b' ' | b'\t' | b'\n') {
                break;
            } else {
                word.push(byte);
            }
        }
        // cc consumes the shlex iterator without checking had_error; an incomplete
        // final token is dropped, while preceding complete arguments are retained.
        if quote.is_some() {
            return words;
        }
        words.push(word);
    }
    words
}

fn compiler_roots(
    probe: &dyn Probe,
    env: &BTreeMap<String, std::ffi::OsString>,
    role: &str,
    compiler: &Tool,
    language: &str,
    roots: &mut Vec<(String, PathBuf)>,
) -> Result<(), Refusal> {
    let query = |words: &[&str]| {
        native_query(
            probe,
            env,
            compiler,
            &[],
            &words.iter().map(std::ffi::OsString::from).collect::<Vec<_>>(),
        )
    };
    for (name, flag) in [
        ("sysroot", "-print-sysroot"),
        ("include", "-print-file-name=include"),
        ("resources", "-print-resource-dir"),
        ("libc", "-print-file-name=libc.so"),
        ("libc-runtime", "-print-file-name=libc.so.6"),
        ("loader", "-print-file-name=ld-linux-x86-64.so.2"),
        ("loader-arm", "-print-file-name=ld-linux-aarch64.so.1"),
    ] {
        if let Some((stdout, _)) = query(&[flag])
            && !stdout.iter().all(u8::is_ascii_whitespace)
            && let Ok(path) = crate::git_path::decode_line(stdout)
            && path.is_absolute()
            && path != Path::new("/")
        {
            roots.push((format!("{role}/{name}"), path));
        }
    }
    if let Some((stdout, _)) = query(&["-print-search-dirs"]) {
        for line in stdout.split(|b| *b == b'\n') {
            let Some(paths) =
                line.strip_prefix(b"libraries: =").or_else(|| line.strip_prefix(b"libraries: "))
            else {
                continue;
            };
            for (at, bytes) in paths.split(|b| *b == b':').enumerate() {
                if !bytes.is_empty() {
                    roots.push((format!("{role}/libraries/{at}"), native_path(bytes)?));
                }
            }
        }
    }
    if let Some((stdout, stderr)) = query(&["-E", "-x", language, "-", "-v"])
        && let Some(paths) = header_search_roots(&stderr)?.or(header_search_roots(&stdout)?)
    {
        roots.extend(
            paths.into_iter().enumerate().map(|(at, path)| (format!("{role}/headers/{at}"), path)),
        );
    }
    Ok(())
}

fn native_path(bytes: &[u8]) -> Result<PathBuf, Refusal> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStringExt;
        if bytes.contains(&0) {
            return Err(Refusal::new("native search path", "path contains a NUL"));
        }
        Ok(PathBuf::from(std::ffi::OsString::from_vec(bytes.to_vec())))
    }
    #[cfg(not(unix))]
    {
        String::from_utf8(bytes.to_vec()).map(PathBuf::from).map_err(|_| {
            Refusal::new("native search path", "path is not representable on this host")
        })
    }
}

/// Which native side a triple has: the two blaze builds for.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Family {
    Macos,
    Linux,
}

impl Family {
    /// The family a probe's `os` names: its first word is the OS.
    fn of_os(os: &str) -> Option<Family> {
        match os.split_whitespace().next()? {
            "macos" => Some(Family::Macos),
            "linux" => Some(Family::Linux),
            _ => None,
        }
    }
}

/// A native tool as `ExecPlatform` names it.
struct Tool {
    path: PathBuf,
    args: Vec<std::ffi::OsString>,
    name: String,
    version: String,
    digest: Digest,
}

impl Tool {
    /// The tool as a platform's tuple: name, version, digest.
    fn tuple(self) -> (String, String, Digest) {
        (self.name, self.version, self.digest)
    }
}

/// The tool a variable names, or `default` when it names none.
fn tool(
    probe: &dyn Probe,
    env: &BTreeMap<String, std::ffi::OsString>,
    triple: &str,
    var: &str,
    default: &str,
) -> Option<Tool> {
    // cc selects the first present variable, then env_tool treats an empty/whitespace
    // value as absent. The caller uses its default, not another environment variable.
    let selected = tool_value(env, triple, var).map(trim_native).filter(|value| !value.is_empty());
    resolve(probe, env, selected.as_deref().unwrap_or(std::ffi::OsStr::new(default)))
}

/// The value of the most specific of a tool's variables, or `None` when none is set: `CC_<triple>`,
/// `CC_<triple with - and . replaced by _>`, `HOST_CC`, `CC`. Empty values are selected too.
fn tool_value<'a>(
    env: &'a BTreeMap<String, std::ffi::OsString>,
    triple: &str,
    var: &str,
) -> Option<&'a std::ffi::OsStr> {
    let underscored = triple.replace(['-', '.'], "_");
    [
        format!("{var}_{triple}"),
        format!("{var}_{underscored}"),
        format!("HOST_{var}"),
        var.to_owned(),
    ]
    .iter()
    .find_map(|name| env.get(name.as_str()).map(|value| value.as_os_str()))
}

/// The tool `value` names: its program, the launcher in front of it when it has one, and the
/// bytes of both.
fn resolve(
    probe: &dyn Probe,
    env: &BTreeMap<String, std::ffi::OsString>,
    value: &std::ffi::OsStr,
) -> Option<Tool> {
    let trimmed = trim_native(value);
    let value = trimmed.as_os_str();
    if value.is_empty() {
        return None;
    }
    // A value that is a path with a separator and is there is the program itself, spaces and
    // all: `cc` checks the whole value before it splits it (`lib.rs:3479-3484`).
    if Path::new(value).components().count() > 1
        && let Some(tool) = named(probe, env, Path::new(value), None)
    {
        return Some(tool);
    }
    let split = words_native(value);
    let (launcher, program) = match split.as_slice() {
        [first, second, ..] if is_launcher(env, first) => {
            (Some(first.as_os_str()), second.as_os_str())
        }
        [first, ..] => (None, first.as_os_str()),
        [] => return None,
    };
    let mut tool = named(probe, env, Path::new(program), launcher)?;
    let skip = if launcher.is_some() { 2 } else { 1 };
    tool.args = split.into_iter().skip(skip).collect();
    if !tool.args.is_empty() {
        let mut hash = Sha256::new();
        hash.update(tool.digest);
        for arg in &tool.args {
            let bytes = arg.as_encoded_bytes();
            hash.update((bytes.len() as u64).to_le_bytes());
            hash.update(bytes);
        }
        tool.digest = hash.finalize().into();
    }
    Some(tool)
}

/// The tool of the program `program`, behind the launcher `launcher` when there is one.
fn named(
    probe: &dyn Probe,
    env: &BTreeMap<String, std::ffi::OsString>,
    program: &Path,
    launcher: Option<&std::ffi::OsStr>,
) -> Option<Tool> {
    let (path, bytes) = executable(probe, env, program)?;
    let file_name = path.file_name()?;
    // Tool names are diagnostic text; preserve existing UTF-8 spelling. Raw-byte names
    // get reversible escaped display text, while executable bytes remain their identity.
    let name = match file_name.to_str() {
        Some(name) => name.to_owned(),
        None => blaze_wire::SourcePath::from_native(Path::new(file_name)).ok()?.as_str().to_owned(),
    };
    let mut hasher = Sha256::new();
    hasher.update(&bytes);
    if let Some(launcher) = launcher {
        // The launcher's bytes fold in: the same compiler behind another launcher is another
        // platform.
        let (_, launcher_bytes) = executable(probe, env, Path::new(launcher))?;
        hasher.update(&launcher_bytes);
    }
    let digest = hasher.finalize().into();
    Some(Tool { name, version: version(probe, &path), digest, path, args: Vec::new() })
}

/// Whether `word` is a launcher `cc` skips to the compiler after it.
fn is_launcher(env: &BTreeMap<String, std::ffi::OsString>, word: &std::ffi::OsStr) -> bool {
    let Some(stem) = Path::new(word).file_stem() else {
        return false;
    };
    stem.to_str().is_some_and(|stem| LAUNCHERS.contains(&stem))
        || env.get(LAUNCHER_VAR).is_some_and(|it| it == stem)
}

/// Where `word` runs and the bytes of what runs: a shim of `/usr/bin` runs the developer
/// directory's tool, and that tool's bytes are what name the compiler.
fn executable(
    probe: &dyn Probe,
    env: &BTreeMap<String, std::ffi::OsString>,
    word: &Path,
) -> Option<(PathBuf, Vec<u8>)> {
    let (path, bytes) = if word.components().count() > 1 {
        (word.to_owned(), probe.read(word)?)
    } else {
        let path = probe.which_native(word.as_os_str(), env.get("PATH")?)?;
        let bytes = probe.read(&path)?;
        (path, bytes)
    };
    if is_shim(&path)
        && let Some(name) = path.file_name().and_then(|it| it.to_str())
        && let Some(real) = xcrun_find(probe, env, name)
        && let Some(bytes) = probe.read(&real)
    {
        return Some((real, bytes));
    }
    Some((path, bytes))
}

/// Whether `path` is one of the shims of `/usr/bin`.
fn is_shim(path: &Path) -> bool {
    path.parent() == Some(Path::new("/usr/bin"))
        && path.file_name().and_then(|it| it.to_str()).is_some_and(|it| SHIMS.contains(&it))
}

/// The first line of what `path` prints for `--version`, trimmed, or an empty string when it
/// cannot be run, or must not be: a tool of the requester's tree is named by its digest alone.
fn version(probe: &dyn Probe, path: &Path) -> String {
    probe
        .run(path, &["--version"])
        .and_then(|out| out.lines().next().map(|it| it.trim().to_owned()))
        .unwrap_or_default()
}

/// What `xcrun --find <name>` names: the tool of the active developer directory.
fn xcrun_find(
    probe: &dyn Probe,
    env: &BTreeMap<String, std::ffi::OsString>,
    name: &str,
) -> Option<PathBuf> {
    let path = xcrun_path(probe, env, &["--find", name])?;
    path.is_absolute().then_some(path)
}

fn xcrun_path(
    probe: &dyn Probe,
    env: &BTreeMap<String, std::ffi::OsString>,
    args: &[&str],
) -> Option<PathBuf> {
    let mut words = Vec::new();
    match env.get("DEVELOPER_DIR") {
        Some(dir) => {
            let mut v = std::ffi::OsString::from("DEVELOPER_DIR=");
            v.push(dir);
            words.push(v);
        }
        None => words.extend(["-u".into(), "DEVELOPER_DIR".into()]),
    }
    words.push(XCRUN.into());
    words.extend(args.iter().map(std::ffi::OsString::from));
    crate::git_path::decode_line(probe.run_path(Path::new("/usr/bin/env"), &words)?).ok()
}

/// What macOS's `xcrun` with `args` prints. It is named by its absolute path, so the daemon's
/// `PATH` cannot change which one runs. `Probe::run` takes no environment, so `DEVELOPER_DIR`
/// is passed through `/usr/bin/env`, set or unset as the machine has it.
fn xcrun(
    probe: &dyn Probe,
    env: &BTreeMap<String, std::ffi::OsString>,
    args: &[&str],
) -> Option<String> {
    let mut words: Vec<std::ffi::OsString> = Vec::new();
    match env.get("DEVELOPER_DIR").filter(|it| !it.is_empty()) {
        Some(dir) => {
            let mut value = std::ffi::OsString::from("DEVELOPER_DIR=");
            value.push(dir);
            words.push(value);
        }
        None => words.extend(["-u".into(), "DEVELOPER_DIR".into()]),
    }
    words.push(XCRUN.into());
    words.extend(args.iter().map(std::ffi::OsString::from));
    probe.run_native(Path::new("/usr/bin/env"), &words)
}

/// macOS's `sdk` field and the SDK directory its files are read from.
fn macos_sdk(
    probe: &dyn Probe,
    env: &BTreeMap<String, std::ffi::OsString>,
    triple: &str,
) -> Result<((String, String, String), PathBuf), Refusal> {
    let root = env.get("SDKROOT").filter(|it| !it.is_empty());
    let dir = match root {
        Some(root) => PathBuf::from(root),
        None => xcrun_path(probe, env, &["--show-sdk-path"])
            .filter(|it| it.is_absolute())
            .ok_or_else(|| {
                Refusal::new(
                    "sdk",
                    "xcrun --show-sdk-path names no SDK: the machine has no developer directory",
                )
            })?,
    };
    let name = sdk_name(&dir).ok_or_else(|| {
        Refusal::new(dir.display().to_string(), "the SDK directory's name does not name an SDK")
    })?;
    let version = match root {
        Some(_) => {
            let file = dir.join(SDK_SETTINGS);
            let settings = probe.read(&file).ok_or_else(|| {
                Refusal::new(file.display().to_string(), "the SDK's settings cannot be read")
            })?;
            let settings: serde_json::Value = serde_json::from_slice(&settings).map_err(|_| {
                Refusal::new(file.display().to_string(), "the SDK's settings are not JSON")
            })?;
            settings.get("Version").and_then(|it| it.as_str()).map(str::to_owned).ok_or_else(
                || Refusal::new(file.display().to_string(), "the SDK's settings name no Version"),
            )?
        }
        None => xcrun(probe, env, &["--show-sdk-version"])
            .map(|it| it.trim().to_owned())
            .filter(|it| !it.is_empty())
            .ok_or_else(|| Refusal::new("sdk", "xcrun --show-sdk-version names no version"))?,
    };
    let target = match env
        .get("MACOSX_DEPLOYMENT_TARGET")
        .map(|it| {
            it.to_str().ok_or_else(|| {
                Refusal::new("MACOSX_DEPLOYMENT_TARGET", "expected textual deployment version")
            })
        })
        .transpose()?
        .map(str::trim)
        .filter(|it| !it.is_empty())
    {
        Some(target) => target.to_owned(),
        None => DEPLOYMENT_TARGETS
            .iter()
            .find(|(it, _)| *it == triple)
            .map(|(_, target)| (*target).to_owned())
            .ok_or_else(|| {
                Refusal::new(
                    "MACOSX_DEPLOYMENT_TARGET",
                    format!("rustc's default deployment target for {triple} is not known"),
                )
            })?,
    };
    Ok(((name, version, target), dir))
}

/// The name of an SDK: its directory's file name without `.sdk`, lowercased, up to its first
/// digit.
fn sdk_name(dir: &Path) -> Option<String> {
    let native_name = dir.file_name()?;
    let escaped;
    let name = match native_name.to_str() {
        Some(name) => name,
        None => {
            escaped = blaze_wire::SourcePath::from_native(Path::new(native_name)).ok()?;
            escaped.as_str()
        }
    };
    let name = name.strip_suffix(".sdk").unwrap_or(name);
    let stem: String = name.chars().take_while(|it| !it.is_ascii_digit()).collect();
    let stem = stem.trim_end_matches('.').to_ascii_lowercase();
    (!stem.is_empty()).then_some(stem)
}

/// Linux's `libc` field: `glibc` with what `getconf` names, or `musl` with what its loader names.
fn linux_libc(
    probe: &dyn Probe,
    env: &BTreeMap<String, std::ffi::OsString>,
) -> Result<(String, String), Refusal> {
    let getconf = probe
        .which_native(
            std::ffi::OsStr::new("getconf"),
            env.get("PATH").map(|it| it.as_os_str()).unwrap_or_default(),
        )
        .unwrap_or_else(|| PathBuf::from("/usr/bin/getconf"));
    if let Some(out) = probe.run(&getconf, &["GNU_LIBC_VERSION"])
        && let Some(version) = glibc_version(&out)
    {
        return Ok(("glibc".to_owned(), version));
    }
    // musl's loader is its libc, and a distribution may package either name.
    if let Some(loader) = first_of(probe, &LIBCS[4..]).or_else(|| first_of(probe, &LOADERS[2..])) {
        let version = probe
            .run(Path::new(loader), &["--version"])
            .and_then(|out| {
                out.lines().find_map(|line| {
                    line.trim().strip_prefix("Version ").map(|it| it.trim().to_owned())
                })
            })
            .unwrap_or_default();
        return Ok(("musl".to_owned(), version));
    }
    Err(Refusal::new(
        "libc",
        "no glibc answers getconf GNU_LIBC_VERSION and no musl loader is where blaze looks",
    ))
}

/// The version `getconf GNU_LIBC_VERSION` names: its second word ("glibc 2.39" is "2.39").
fn glibc_version(out: &str) -> Option<String> {
    let mut words = out.split_whitespace();
    words.next()?;
    words.next().map(str::to_owned)
}

/// The major and minor numbers of a release: "6.8.0-45-generic" is "6.8".
fn major_minor(release: &str) -> String {
    release.split('.').take(2).collect::<Vec<_>>().join(".")
}

/// The first of `list` that is there.
fn first_of(probe: &dyn Probe, list: &[&'static str]) -> Option<&'static str> {
    list.iter().copied().find(|it| probe.read(Path::new(it)).is_some())
}

fn trim_native(value: &std::ffi::OsStr) -> std::ffi::OsString {
    if let Some(text) = value.to_str() {
        return text.trim().into();
    }
    #[cfg(unix)]
    {
        use std::os::unix::ffi::{OsStrExt, OsStringExt};
        let bytes = value.as_bytes();
        let start = bytes.iter().position(|b| !b.is_ascii_whitespace()).unwrap_or(bytes.len());
        let end = bytes.iter().rposition(|b| !b.is_ascii_whitespace()).map_or(start, |at| at + 1);
        std::ffi::OsString::from_vec(bytes[start..end].to_vec())
    }
    #[cfg(not(unix))]
    {
        value.to_owned()
    }
}

/// Preserve raw Unix bytes while applying the same ASCII quote/escape syntax as text tools.
fn words_native(value: &std::ffi::OsStr) -> Vec<std::ffi::OsString> {
    if let Some(text) = value.to_str() {
        return words(text).into_iter().map(Into::into).collect();
    }
    #[cfg(unix)]
    {
        use std::os::unix::ffi::{OsStrExt, OsStringExt};
        let mut out = Vec::new();
        let mut word = Vec::new();
        let mut quote = None;
        let mut escaped = false;
        let mut started = false;
        for &b in value.as_bytes() {
            if escaped {
                word.push(b);
                escaped = false;
                started = true;
                continue;
            }
            if b == b'\\' && quote != Some(b'\'') {
                escaped = true;
                started = true;
                continue;
            }
            if let Some(q) = quote {
                if b == q {
                    quote = None;
                } else {
                    word.push(b);
                }
            } else if matches!(b, b'\'' | b'"') {
                quote = Some(b);
                started = true;
            } else if b.is_ascii_whitespace() {
                if started {
                    out.push(std::ffi::OsString::from_vec(std::mem::take(&mut word)));
                    started = false;
                }
            } else {
                word.push(b);
                started = true;
            }
        }
        if started {
            out.push(std::ffi::OsString::from_vec(word));
        }
        out
    }
    #[cfg(not(unix))]
    {
        vec![value.to_owned()]
    }
}

#[cfg(test)]
mod native_word_tests {
    #[test]
    fn shell_flag_control_uses_cc_boolean_values() {
        for (control, expected) in [
            (None, "'/quoted"),
            (Some(""), "'/quoted"),
            (Some("0"), "'/quoted"),
            (Some("no"), "'/quoted"),
            (Some("false"), "'/quoted"),
            (Some("1"), "/quoted path"),
            (Some("true"), "/quoted path"),
            (Some("FALSE"), "/quoted path"),
        ] {
            let mut env = std::collections::BTreeMap::from([(
                "CFLAGS".to_owned(),
                "-I'/quoted path'".into(),
            )]);
            if let Some(control) = control {
                env.insert("CC_SHELL_ESCAPED_FLAGS".into(), control.into());
            }
            let mut roots = Vec::new();
            super::native_environment_roots(&env, "x86_64-unknown-linux-gnu", None, &mut roots)
                .unwrap();
            assert_eq!(roots.len(), 1);
            assert_eq!(roots[0].1, std::path::Path::new(expected), "{control:?}");
        }
    }

    #[test]
    fn flag_operands_span_variables_within_each_build_kind() {
        let env = std::collections::BTreeMap::from([
            ("CFLAGS".to_owned(), "-I".into()),
            ("HOST_CFLAGS".to_owned(), "/host/include".into()),
            ("TARGET_CFLAGS".to_owned(), "/target/include".into()),
        ]);
        let mut roots = Vec::new();
        super::native_environment_roots(&env, "x86_64-unknown-linux-gnu", None, &mut roots)
            .unwrap();
        assert_eq!(
            roots.iter().map(|(_, path)| path.as_path()).collect::<Vec<_>>(),
            [std::path::Path::new("/host/include"), std::path::Path::new("/target/include"),]
        );
        let mut unused = env;
        unused.insert("TARGET_CFLAGS".into(), "".into());
        let mut roots = Vec::new();
        super::native_environment_roots(&unused, "x86_64-unknown-linux-gnu", None, &mut roots)
            .unwrap();
        assert_eq!(roots.len(), 1);
        assert_eq!(roots[0].1, std::path::Path::new("/host/include"));
    }

    #[test]
    fn flag_words_match_cc_whitespace_and_posix_modes() {
        let value = std::ffi::OsStr::new("-I'a b' -I\"c\\q\" # ignored\n-Ilast\\\n/path");
        assert_eq!(
            super::flag_words(value, false),
            [
                b"-I'a".to_vec(),
                b"b'".to_vec(),
                b"-I\"c\\q\"".to_vec(),
                b"#".to_vec(),
                b"ignored".to_vec(),
                b"-Ilast\\".to_vec(),
                b"/path".to_vec(),
            ]
        );
        assert_eq!(
            super::flag_words(value, true),
            [b"-Ia b".to_vec(), b"-Ic\\q".to_vec(), b"-Ilast/path".to_vec(),]
        );
        for value in ["first 'unfinished", "first \"unfinished", "first unfinished\\"] {
            assert_eq!(super::flag_words(std::ffi::OsStr::new(value), true), [b"first".to_vec()]);
        }
        assert_eq!(
            super::flag_words(std::ffi::OsStr::new("'' a#b \"a\\$b\\`c\\\\d\\\"e\""), true),
            [Vec::new(), b"a#b".to_vec(), b"a$b`c\\d\"e".to_vec(),]
        );
        assert_eq!(
            super::flag_words(std::ffi::OsStr::new("a\r\x0bb\x0cc"), true),
            [b"a\r\x0bb\x0cc".to_vec()]
        );
        assert_eq!(
            super::flag_words(std::ffi::OsStr::new("a\r\x0bb\x0cc"), false),
            [b"a".to_vec(), b"\x0bb".to_vec(), b"c".to_vec()]
        );
    }

    #[test]
    fn text_and_native_supported_syntax_agree() {
        for text in ["cc -O2", "'cc name' -g", "sccache cc", "cc \\", "cc\t\"a b\""] {
            let a = super::words_native(std::ffi::OsStr::new(text));
            let b: Vec<std::ffi::OsString> =
                super::words(text).into_iter().map(Into::into).collect();
            assert_eq!(a, b);
        }
    }
    #[cfg(unix)]
    #[test]
    fn raw_parser_quote_escape_and_trailing_backslash_match_ascii_control() {
        use std::os::unix::ffi::{OsStrExt, OsStringExt};
        for prefix in ["cc 'a b' ", "cc \"a b\" ", "cc a\\ b ", "cc\t", "cc "] {
            for tail in ["n", "n\\"] {
                let control = format!("{prefix}{tail}");
                let mut raw = control.as_bytes().to_vec();
                let at = raw.iter().rposition(|b| *b == b'n').unwrap();
                raw[at] = 0xff;
                let parsed = super::words_native(&std::ffi::OsString::from_vec(raw));
                let mut expected: Vec<Vec<u8>> =
                    super::words(&control).into_iter().map(String::into_bytes).collect();
                for word in &mut expected {
                    for b in word {
                        if *b == b'n' {
                            *b = 0xff;
                        }
                    }
                }
                assert_eq!(
                    parsed.iter().map(|v| v.as_bytes().to_vec()).collect::<Vec<_>>(),
                    expected
                );
            }
        }
    }
}

/// The words of `value`, split as a shell splits them on whitespace and quotes, so that a value
/// a makefile wrote names its program and its arguments.
fn words(value: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut word = String::new();
    let mut started = false;
    let mut quote: Option<char> = None;
    let mut chars = value.chars();
    while let Some(c) = chars.next() {
        match quote {
            Some(q) => {
                if c == q {
                    quote = None;
                } else if c == '\\' && q == '"' {
                    if let Some(next) = chars.next() {
                        word.push(next);
                    }
                } else {
                    word.push(c);
                }
            }
            None => match c {
                '\'' | '"' => {
                    quote = Some(c);
                    started = true;
                }
                '\\' => {
                    if let Some(next) = chars.next() {
                        word.push(next);
                        started = true;
                    }
                }
                c if c.is_whitespace() => {
                    if started {
                        out.push(std::mem::take(&mut word));
                        started = false;
                    }
                }
                c => {
                    word.push(c);
                    started = true;
                }
            },
        }
    }
    if started {
        out.push(word);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn words_split_as_a_shell_does() {
        assert_eq!(words("cc"), ["cc"]);
        assert_eq!(words("clang --target=x"), ["clang", "--target=x"]);
        assert_eq!(words("  clang   -O2  "), ["clang", "-O2"]);
        assert_eq!(words(r#""/opt/my cc/cc" -m32"#), ["/opt/my cc/cc", "-m32"]);
        assert_eq!(words("'/opt/my cc/cc'"), ["/opt/my cc/cc"]);
        assert_eq!(words(r#""a\"b" c"#), [r#"a"b"#, "c"]);
        assert!(words("   ").is_empty());
    }

    #[test]
    fn sdk_names_drop_the_version() {
        assert_eq!(sdk_name(Path::new("/SDKs/MacOSX15.4.sdk")).as_deref(), Some("macosx"));
        assert_eq!(sdk_name(Path::new("/SDKs/MacOSX.sdk")).as_deref(), Some("macosx"));
        assert_eq!(
            sdk_name(Path::new("/Library/Developer/CommandLineTools/SDKs/MacOSX26.0.sdk"))
                .as_deref(),
            Some("macosx")
        );
        assert_eq!(sdk_name(Path::new("/")), None);
    }

    #[test]
    fn libc_versions_are_second_words() {
        assert_eq!(glibc_version("glibc 2.39\n").as_deref(), Some("2.39"));
        assert_eq!(glibc_version("glibc"), None);
        assert_eq!(glibc_version(""), None);
    }

    #[test]
    fn kernel_releases_are_major_and_minor() {
        assert_eq!(major_minor("6.8.0-45-generic"), "6.8");
        assert_eq!(major_minor("6.8"), "6.8");
        assert_eq!(major_minor("6"), "6");
    }
}
