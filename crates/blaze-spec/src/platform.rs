//! Request-scoped native execution platforms, with complete SDK and compiler search trees.
//!
//! Tools follow the native `cc` crate's variable precedence and launcher selection. Their
//! executable bytes, launcher bytes and selected arguments contribute to identity. Files,
//! directory membership, missing paths, native names, modes and symlink referents are bound
//! by complete trees. SDK identities do not contain the SDK's physical root location.
//! Every filesystem read and process query goes through `Probe`; `System` uses the exact
//! request cwd and environment. `describe` retains the trees for saved-input admission.

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
        _env: &BTreeMap<String, std::ffi::OsString>,
        program: &Path,
        args: &[std::ffi::OsString],
    ) -> Option<(Vec<u8>, Vec<u8>)> {
        self.probe.run_output_in(self.cwd, self.env, &self.cwd.join(program), args)
    }
    fn which_native(&self, name: &std::ffi::OsStr, path: &std::ffi::OsStr) -> Option<PathBuf> {
        self.probe.which_native_in(self.cwd, name, path).map(|path| self.cwd.join(path))
    }
    fn which(&self, name: &str, path: &str) -> Option<PathBuf> {
        self.probe.which_in(self.cwd, name, path).map(|path| self.cwd.join(path))
    }
    fn run(&self, program: &Path, args: &[&str]) -> Option<String> {
        self.probe.run_in(self.cwd, self.env, &self.cwd.join(program), args)
    }
    fn run_native(&self, program: &Path, args: &[std::ffi::OsString]) -> Option<String> {
        self.probe.run_native_in(self.cwd, self.env, &self.cwd.join(program), args)
    }
    fn run_path(&self, program: &Path, args: &[std::ffi::OsString]) -> Option<Vec<u8>> {
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
    Ok(describe(machine, probe)?.platform)
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
    let scoped = InDirectory { probe, cwd: machine.cwd, env: machine.env };
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
    for (role, compiler, language) in
        [("cc", cc.as_ref(), "c"), ("cxx", cxx.as_ref(), "c++"), ("linker", Some(&linker), "c")]
    {
        if let Some(compiler) = compiler {
            compiler_roots(probe, env, role, compiler, language, &mut roots)?;
        }
    }
    native_environment_roots(env, machine.triple, &mut roots)?;
    let mut native = crate::native::Closure::default();
    let paths: Vec<_> = roots.iter().map(|(_, path)| machine.cwd.join(path)).collect();
    let trees = probe.native_trees(&paths).map_err(|error| {
        Refusal::new("native roots", format!("cannot bind the complete native trees: {error}"))
    })?;
    if trees.len() != paths.len() {
        return Err(Refusal::new("native roots", "the probe did not return every requested tree"));
    }
    for ((name, _), tree) in roots.into_iter().zip(trees) {
        native.roots.push(crate::native::Root { name, tree });
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
        native_root: Some(native.digest()),
    };
    Ok(Description { platform, native })
}

fn native_environment_roots(
    env: &BTreeMap<String, std::ffi::OsString>,
    triple: &str,
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
            let words: Vec<_> = value
                .as_encoded_bytes()
                .split(u8::is_ascii_whitespace)
                .filter(|word| !word.is_empty())
                .collect();
            let mut at = 0;
            while at < words.len() {
                let word = words[at];
                at += 1;
                let (role, rest) = if let Some(path) = word.strip_prefix(b"-L") {
                    ("linker/libraries", path)
                } else if let Some(path) = word.strip_prefix(b"-I") {
                    ("native/headers", path)
                } else if let Some(path) = word.strip_prefix(b"-F") {
                    ("linker/frameworks", path)
                } else if matches!(
                    word,
                    b"-isystem" | b"-idirafter" | b"-iframework" | b"-isysroot"
                ) {
                    ("native/headers", &b""[..])
                } else {
                    continue;
                };
                let path = if rest.is_empty() {
                    let path = words.get(at).ok_or_else(|| {
                        Refusal::new(name, "native wrapper search flag needs a path")
                    })?;
                    at += 1;
                    *path
                } else {
                    rest
                };
                roots.push((format!("{role}/flags/{name}/{at}"), native_path(path)?));
            }
        }
    }
    Ok(())
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
        let mut args = compiler.args.clone();
        args.extend(words.iter().map(std::ffi::OsString::from));
        probe.run_output_in(Path::new("."), env, &compiler.path, &args)
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
    if let Some((stdout, stderr)) = query(&["-E", "-x", language, "-", "-v"]) {
        for bytes in [stdout, stderr] {
            let mut listing = false;
            for line in bytes.split(|b| *b == b'\n') {
                if line == b"#include <...> search starts here:" {
                    listing = true;
                    continue;
                }
                if line == b"End of search list." {
                    listing = false;
                }
                if listing {
                    let line = line.strip_prefix(b" ").unwrap_or(line);
                    let line = line.strip_suffix(b" (framework directory)").unwrap_or(line);
                    if !line.is_empty() {
                        let at = roots.len();
                        roots.push((format!("{role}/headers/{at}"), native_path(line)?));
                    }
                }
            }
        }
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
/// cannot be run.
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
