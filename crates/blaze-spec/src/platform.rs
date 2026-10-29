//! The `ExecPlatform` of this machine: the native side of a build, with the C and C++
//! compilers, the archiver and the linker by the digest of their executables and the SDK or
//! sysroot by what is in it (review B M5).
//!
//! [`exec_platform`] is a pure function of a [`Machine`] and a [`Probe`]: every variable it
//! reads is in `Machine::env`, and every program it runs and file it reads goes through the
//! probe in `Machine::cwd`. It reads no variable of the process's own environment, so two calls
//! with one machine and probe give one platform, and a variable the build does not see cannot
//! change it.
//! [`System`] is the probe that runs this machine's programs and reads its files.
//!
//! # What each field is
//!
//! `triple` and `cpu` are the machine's, copied. `os` is what the probe names: `macos` with the
//! full product version, `linux` with the kernel's major and minor. A machine whose operating
//! system cannot be named is refused. Which of `sdk` and `libc` is read follows `os`, the
//! machine's own operating system, and not the triple: blaze does not cross-build, so a caller
//! gives the machine's own triple, and one that named another OS would make the platform
//! contradict itself.
//!
//! ## `cc`, `cxx`, `ar`
//!
//! The program is the one the `cc` crate (1.4.3, which the workspace locks) would run for a
//! build script of this triple with the host equal to the target, because blaze never
//! cross-builds. The value is the first set of `CC_<triple>`,
//! `CC_<triple with `-` and `.` replaced by `_>`, `HOST_CC` and `CC`; `CXX` and
//! `AR` the same, with the defaults `c++` and `ar`. Read from `cc` 1.4.3 (`lib.rs:3166-3260`,
//! `:3470-3530`, `:4078-4085`, `:4207-4222`): `target_envs` writes the triple's two names and
//! then `HOST_CC`, because `get_is_cross_compile` is false when the host is the target.
//! `TARGET_CC` is unused for native builds. An empty selected value is not skipped:
//! cc's `env_tool` discards it and uses the default tool, not a lower-priority variable.
//! `cc`'s default is `cc`
//! and not `gcc`: `gcc` is its name only on Solaris and illumos, which blaze does not build
//! for (`lib.rs:3195-3207`).
//!
//! A value may carry arguments (`clang --target=x`). The words are split as a shell splits
//! them, on whitespace and quotes, so that a value a makefile wrote names its program and its
//! arguments; `cc` itself splits on whitespace alone (`lib.rs:3520`), so quotes are what a
//! shell would do and not what `cc` does. A value that is a path with a separator and exists is
//! used whole, so that a path with a space in it survives (`cc` checks the whole value first,
//! `lib.rs:3479-3484`). A launcher in front of the compiler (`ccache`, `sccache`, `distcc`,
//! `icecc`, `cachepot`, `buildcache`, `kache`, and `CC_KNOWN_WRAPPER_CUSTOM`'s name,
//! `lib.rs:3506-3517`) is skipped to the compiler after it, and the launcher's bytes fold into
//! the digest, so the same compiler behind a different launcher is a different platform. A
//! launcher that cannot be resolved is not there to run the compiler, so the tool is absent.
//!
//! The program is resolved in the directories of `Machine::env`'s `PATH`, with relative paths
//! and empty PATH entries rooted at `Machine::cwd`. Probes run in that directory without
//! changing the serving process's cwd. Child processes receive exactly `Machine::env`,
//! with no serving-process variables inherited. No program by that name means the field is `None`: a
//! build with no C dependency needs no compiler, and two machines that differ only in that one
//! has a compiler do not share a platform, which is the safe direction.
//!
//! The tuple is the program's file name as resolved, the first line of its `--version` output
//! trimmed (empty when it cannot be run), and the SHA-256 of its bytes. The bytes are those of
//! the file read through the probe, which follows a symlink; the name is that of the path the
//! program was resolved to, not of the symlink's target, so `CC=cc` and `CC=clang` name
//! differently while digesting the same bytes. A launcher's bytes are appended to the
//! compiler's before the digest is taken.
//!
//! On macOS the tools in `/usr/bin` (`cc`, `c++`, `ar`, `ld`, `clang`, `gcc`) are shims that run
//! whatever the active developer directory holds — all six are one 118,640-byte stub here, read
//! on macOS 26.6.2 — so their bytes do not name the compiler. A shim is resolved with
//! `xcrun --find <name>` and that executable is digested; when `xcrun` is missing or names
//! nothing, the shim's own bytes are digested and the version line is what names the tool.
//! `DEVELOPER_DIR` is passed through `/usr/bin/env` for canned probes as well as the real
//! request-scoped process environment. The serving process's directory cannot reach `xcrun`.
//!
//! ## `linker`
//!
//! The program that links: `Machine::linker` (`target.<triple>.linker` from cargo's
//! configuration) when it is set, else `cc`, which is what rustc runs for this triple's
//! `darwin-cc` and `gnu-cc` link flavours and what blaze's own link runs
//! (`crates/ra-jit/src/link.rs:119`), resolved and named as the tools above. `cc` does not read
//! `CC`: a machine whose compiler is named only in `CC` still links with `cc`. A linker that
//! cannot be resolved is refused, naming it as it was written: the build cannot link.
//!
//! ## `sdk`
//!
//! macOS only; `None` on Linux. The SDK directory is `SDKROOT` from the environment when it is
//! set, else what `xcrun --show-sdk-path` names. The name is the directory's file name without
//! `.sdk`, lowercased and cut at its first digit, so `MacOSX15.4.sdk` and `MacOSX.sdk` are both
//! `macosx`. The version is what `xcrun --show-sdk-version` prints, or, with `SDKROOT` set, the
//! `Version` key of `<sdk>/SDKSettings.json` (read with `serde_json`, already a dependency of
//! this crate). The deployment target is `MACOSX_DEPLOYMENT_TARGET` from the environment when
//! it is set, else rustc's default for the triple: 11.0 for `aarch64-apple-darwin` and 10.12 for
//! `x86_64-apple-darwin`, as `crates/ra-jit/src/answers/rustc-1.99.0.json` states them in each
//! target spec's `metadata.description` ("ARM64 Apple macOS (11.0+, Big Sur+)", "x86_64 Apple
//! macOS (10.12+, Sierra+)"), which `rustc +1.99.0 --print deployment-target` answers the same
//! way on this machine. An SDK that cannot be found or named is refused.
//!
//! ## `libc`
//!
//! Linux only; `None` on macOS. `glibc` with the second word of `getconf GNU_LIBC_VERSION`
//! ("glibc 2.39" is "2.39"), or `musl` with the version its loader prints for `--version` (the
//! word after `Version`) when a `/lib/ld-musl-*` or a `/lib/libc.musl-*` is at one of the paths
//! blaze knows and no glibc answers. A Linux machine that answers neither is refused.
//!
//! ## `native_root`
//!
//! A proxy for what the native side holds, so that two machines whose SDK or sysroot differs
//! do not share a platform: the SHA-256 over, in this order, each file's relative path as 8
//! little-endian bytes, the path, its length as 8 little-endian bytes and its bytes.
//!
//! On macOS the files are `SDKSettings.json` and `usr/lib/libSystem.tbd` of the SDK directory.
//! That covers the SDK's settings and the names libSystem exports, and it does not cover the
//! SDK's headers, its other `.tbd` stubs, libc++, or the compiler: two SDKs that differ only in
//! something this does not read get the same digest, a limitation of the proxy.
//!
//! On Linux they are the libc shared object and the dynamic loader, from a fixed list, because
//! a probe has no way to list a directory and `/proc/self/maps` is not a file a probe reads for
//! this: `/lib/x86_64-linux-gnu/libc.so.6`, `/lib/aarch64-linux-gnu/libc.so.6`,
//! `/lib64/libc.so.6`, `/usr/lib/libc.so.6`, then `/lib/libc.musl-*.so.1`; and
//! `/lib64/ld-linux-x86-64.so.2`, `/lib/ld-linux-aarch64.so.1`, `/lib/ld-musl-*.so.1`. The path
//! as listed is what the digest folds, so two machines whose libc is at different listed paths
//! differ even when the bytes are equal, which is the safe direction. A file that cannot be
//! read folds as a length of `u64::MAX` and no bytes, so its absence is part of the digest;
//! `None` when none of the files could be read.
//!
//! # Unverified
//!
//! - The Linux half: this machine is macOS, so the libc and loader paths, what `getconf
//!   GNU_LIBC_VERSION` prints, what a musl loader prints for `--version`, and what `uname -r`
//!   gives are unverified here.
//! - That `cc`'s launcher handling and this one agree for every launcher: the list is `cc`'s,
//!   read from its source, and a launcher `cc` learns from `CC_KNOWN_WRAPPER_CUSTOM` is read
//!   here too.
//! - That no launcher in front of a compiler names it: the compiler's bytes are digested, and a
//!   launcher that is itself the compiler (a script that runs it) is not seen through.

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

/// The TAPI stub that names what libSystem exports: the SDK's own libc.
const LIBSYSTEM: &str = "usr/lib/libSystem.tbd";

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
                let release = std::fs::read_to_string("/proc/sys/kernel/osrelease")
                    .ok()
                    .or_else(|| {
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
    let scoped = InDirectory { probe, cwd: machine.cwd, env: machine.env };
    let probe: &dyn Probe = &scoped;
    let os = probe
        .os()
        .ok_or_else(|| Refusal::new("os", "the machine's operating system cannot be named"))?;
    // The machine's own OS, not its triple, says which native side it has: blaze does not
    // cross-build, so a caller gives the machine's own triple, and one that named another OS
    // would make the platform contradict itself.
    let family = Family::of_os(&os).ok_or_else(|| {
        Refusal::new(os.clone(), "blaze names the native side of macOS and Linux")
    })?;
    let env = machine.env;

    // The files `native_root` folds, and where they are read: the SDK's on macOS, the libc's
    // and loader's on Linux.
    let mut files: Vec<(String, PathBuf)> = Vec::new();
    let (sdk, libc) = match family {
        Family::Macos => {
            let (sdk, dir) = macos_sdk(probe, env, machine.triple)?;
            for name in [SDK_SETTINGS, LIBSYSTEM] {
                files.push((name.to_owned(), dir.join(name)));
            }
            (Some(sdk), None)
        }
        Family::Linux => {
            let libc = linux_libc(probe, env)?;
            for path in [first_of(probe, &LIBCS), first_of(probe, &LOADERS)].into_iter().flatten() {
                if !files.iter().any(|(name, _)| name.as_str() == path) {
                    files.push((path.to_owned(), PathBuf::from(path)));
                }
            }
            (None, Some(libc))
        }
    };

    let linker_value = machine.linker.map(str::trim).filter(|it| !it.is_empty()).unwrap_or("cc");
    let linker = resolve(probe, env, std::ffi::OsStr::new(linker_value)).ok_or_else(|| {
        Refusal::new(linker_value, "the linker cannot be resolved: a build cannot link without it")
    })?;

    let cc = tool(probe, env, machine.triple, "CC", "cc");
    let cxx = tool(probe, env, machine.triple, "CXX", "c++");
    let ar = tool(probe, env, machine.triple, "AR", "ar");

    Ok(ExecPlatform {
        triple: machine.triple.to_owned(),
        os,
        sdk,
        libc,
        linker: linker.tuple(),
        cpu: machine.cpu.to_owned(),
        cc: cc.map(Tool::tuple),
        cxx: cxx.map(Tool::tuple),
        ar: ar.map(Tool::tuple),
        native_root: native_root(probe, &files),
    })
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
    named(probe, env, Path::new(program), launcher)
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
    Some(Tool { name, version: version(probe, &path), digest })
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

/// The digest of the native side's files, folded in the order given.
fn native_root(probe: &dyn Probe, files: &[(String, PathBuf)]) -> Option<Digest> {
    let mut hasher = Sha256::new();
    let mut any = false;
    for (name, path) in files {
        let bytes = probe.read(path);
        hasher.update((name.len() as u64).to_le_bytes());
        hasher.update(name.as_bytes());
        match &bytes {
            Some(bytes) => {
                any = true;
                hasher.update((bytes.len() as u64).to_le_bytes());
                hasher.update(bytes);
            }
            // A file that is not there is part of the digest as a length no file has.
            None => hasher.update(u64::MAX.to_le_bytes()),
        }
    }
    any.then(|| hasher.finalize().into())
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
