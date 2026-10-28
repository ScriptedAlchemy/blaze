//! The link of a program with nothing of rustc's (docs/no-rustc.md, step G): ra-jit's objects
//! and the native libraries the program's crates ask for with `#[link]`, by the system's `cc`,
//! with the arguments rustc gives it for the target. An ELF target links with the toolchain's
//! lld where it has one, as rustc does on x86_64 Linux: GNU ld takes ten times as long to
//! leave out what is not reached.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};
use std::{collections::BTreeMap, ffi::OsString};

use anyhow::{Context, bail};
use hir::Crate;
use hir_def::attrs::{AttrFlags, NativeLib, NativeLibKind};
use ide_db::RootDatabase;
use ide_db::base_db::{self, CrateOrigin, LangCrateOrigin};
use rustc_hash::FxHashSet;

pub(crate) mod split;

/// The request's linker environment, captured once per operation. Clearing the command's
/// environment also honors variables absent in a request; process-global mutation would
/// race detached links and scripts.
#[derive(Clone)]
pub(crate) struct Environment(BTreeMap<OsString, OsString>);

impl std::fmt::Debug for Environment {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Environment").field("names", &self.0.keys().collect::<Vec<_>>()).finish()
    }
}

impl Environment {
    pub(crate) fn of(db: &RootDatabase) -> Self {
        let vars = match base_db::ProcessEnv::try_get(db) {
            Some(env) => crate::process::native()
                .vars()
                .filter(|(k, v)| k.to_str().is_none() || v.to_str().is_none())
                .chain(env.vars(db).iter().map(|(k, v)| (k.into(), v.into())))
                .collect(),
            None => crate::process::native().vars().collect(),
        };
        Self(vars)
    }

    pub(crate) fn get(&self, name: &str) -> Option<&std::ffi::OsStr> {
        self.0.get(std::ffi::OsStr::new(name)).map(OsString::as_os_str)
    }

    pub(crate) fn apply(&self, command: &mut Command) {
        command.env_clear().envs(&self.0);
    }
}

#[cfg(all(test, unix))]
mod environment_tests {
    use super::*;

    #[test]
    fn command_diagnostics_never_render_request_environment_values() {
        let secret = "not-a-real-token-c4-regression";
        let env = Environment(
            [("PRIVATE_TOKEN".into(), secret.into()), ("PATH".into(), "/private/path".into())]
                .into_iter()
                .collect(),
        );
        let mut command = Command::new("cc");
        env.apply(&mut command);
        command.args(["-o", "/output/program", "/input/object.o"]);
        let shown = diagnostic_command(&command);
        assert!(shown.contains("cc") && shown.contains("/input/object.o"));
        assert!(!shown.contains(secret) && !shown.contains("/private/path"));
        assert!(!format!("{env:?}").contains(secret));
        assert!(format!("{env:?}").contains("PRIVATE_TOKEN"));
        assert_eq!(
            command.get_envs().find(|(name, _)| *name == "PRIVATE_TOKEN").unwrap().1,
            Some(std::ffi::OsStr::new(secret)),
            "redaction must not change the child environment"
        );
    }

    #[test]
    fn a_link_command_retains_its_request_and_unsets_worker_variables() {
        let mut db = RootDatabase::default();
        let request =
            |value: &str| [("LIBRARY_PATH".to_owned(), value.to_owned())].into_iter().collect();
        base_db::set_process_env(&mut db, request("a"));
        let first = Environment::of(&db);
        base_db::set_process_env(&mut db, request("b"));
        let second = Environment::of(&db);
        for (env, want) in [(first, "a"), (second, "b")] {
            let mut command = Command::new("/usr/bin/printenv");
            command.env("BLAZE_LINK_LEAK", "worker");
            env.apply(&mut command);
            let output = command.output().unwrap();
            assert!(output.status.success());
            assert_eq!(String::from_utf8(output.stdout).unwrap(), format!("LIBRARY_PATH={want}\n"));
        }
    }

    #[test]
    fn detached_link_snapshots_keep_native_bytes_after_the_operation_turns() {
        use std::os::unix::ffi::OsStringExt as _;
        let mut db = RootDatabase::default();
        base_db::set_process_env(
            &mut db,
            [("SOURCE".to_owned(), "same".to_owned())].into_iter().collect(),
        );
        let capture = |byte| {
            let _native = crate::process::enter_native(crate::process::Native::of([(
                "BYTE_VALUE".into(),
                OsString::from_vec(vec![0xff, byte]),
            )]));
            Environment::of(&db)
        };
        let first = capture(b'a');
        let second = capture(b'b');
        std::thread::scope(|scope| {
            for (env, byte) in [(first, b'a'), (second, b'b')] {
                scope.spawn(move || {
                    let mut command = Command::new("/usr/bin/printenv");
                    env.apply(&mut command);
                    command.env("SOURCE", "same");
                    let output = command.output().unwrap();
                    let want = [
                        b'B', b'Y', b'T', b'E', b'_', b'V', b'A', b'L', b'U', b'E', b'=', 0xff,
                        byte, b'\n',
                    ];
                    assert!(output.status.success());
                    assert!(output.stdout.windows(want.len()).any(|it| it == want));
                });
            }
        });
    }
}

/// Links `objects` into the executable `exe`, with the native libraries of the crates `program`
/// links. A shared library instead when `exports` names what it exports: the rest of it is
/// its own, and the link leaves out what that does not reach, as rustc's of a `cdylib` does.
/// How long `cc` took.
pub(crate) fn executable(
    db: &RootDatabase,
    program: &[Crate],
    objects: &[&Path],
    sysroot: Option<&Path>,
    exe: &Path,
    exports: Option<&[String]>,
) -> anyhow::Result<Duration> {
    let mut line = Line::of(db, program, sysroot, exports.is_some())?;
    let Some(exports) = exports else { return line.link(objects, exe) };
    let mut listed = exe.as_os_str().to_owned();
    listed.push(".exports");
    let listed = PathBuf::from(listed);
    let (text, arg) = if line.apple {
        let text: String = exports.iter().map(|it| format!("_{it}\n")).collect();
        (text, format!("-Wl,-exported_symbols_list,{}", listed.display()))
    } else {
        let text: String = exports.iter().map(|it| format!("  {it};\n")).collect();
        (
            format!("{{\n global:\n{text} local: *;\n}};\n"),
            format!("-Wl,--version-script={}", listed.display()),
        )
    };
    std::fs::write(&listed, text).with_context(|| format!("writing {}", listed.display()))?;
    line.after.push(arg);
    let took = line.link(objects, exe);
    _ = std::fs::remove_file(&listed);
    took
}

/// The command that links `objects` into the executable `exe` as [`executable`] does, for a
/// shell: the daemon's line, which its edits are spliced after. The first of `objects` is the
/// patch of the object after it: a definition of the patch is the one that stays. On Apple
/// that is the object's weak definitions giving way; on ELF the daemon makes the object's
/// definitions of what the patch defines weak before the line runs
/// ([`crate::weaken::replaced`]), and the line asks the linker for nothing of duplicates, so
/// that two definitions of any other name are its error.
pub(crate) fn command(
    db: &RootDatabase,
    program: &[Crate],
    objects: &[&Path],
    sysroot: Option<&Path>,
    exe: &Path,
) -> anyhow::Result<String> {
    let line = Line::of(db, program, sysroot, false)?;
    command_of(&line, objects, exe, CommandPurpose::WholeLink)
}

/// The native command and its request environment, before compiling the Rust objects. A
/// native identity includes every user link argument, but not the Rust outputs or the
/// daemon's mutable output-layout options: its headerpad file may grow during a link.
pub(crate) fn native_command(
    db: &RootDatabase,
    program: &[Crate],
    sysroot: Option<&Path>,
    exe: &Path,
) -> anyhow::Result<(String, Environment)> {
    let line = Line::of(db, program, sysroot, false)?;
    let command = command_of(&line, &[], exe, CommandPurpose::NativeInputs)?;
    Ok((command, line.env))
}

enum CommandPurpose {
    WholeLink,
    NativeInputs,
}

fn command_of(
    line: &Line,
    objects: &[&Path],
    exe: &Path,
    purpose: CommandPurpose,
) -> anyhow::Result<String> {
    let mut args = vec!["cc".to_owned()];
    args.extend(line.before.iter().cloned());
    for &object in objects {
        let path = object.display().to_string();
        // An object the build has yet to write is none.
        if object.exists() && split::is_archive(&leading_bytes(object)?) {
            args.extend([
                "-Wl,--whole-archive".to_owned(),
                path,
                "-Wl,--no-whole-archive".to_owned(),
            ]);
        } else {
            args.push(path);
        }
    }
    args.extend(line.after.iter().cloned());
    if let CommandPurpose::WholeLink = purpose {
        let splices = crate::cargo::plan::whole_link_args(line.apple, exe)
            .with_context(|| format!("writing what the link of {} asks for", exe.display()))?;
        args.extend(splices);
    }
    args.extend(["-o".to_owned(), exe.display().to_string()]);
    shlex::try_join(args.iter().map(String::as_str)).context("a link argument has a NUL in it")
}

/// The arguments `cc` links the program of some crates with, around its objects.
pub(crate) struct Line {
    before: Vec<String>,
    after: Vec<String>,
    apple: bool,
    env: Environment,
}

impl Line {
    pub(crate) fn of(
        db: &RootDatabase,
        program: &[Crate],
        sysroot: Option<&Path>,
        shared: bool,
    ) -> anyhow::Result<Line> {
        let Some(&first) = program.first() else { bail!("nothing to link") };
        let target = Target { shared, ..Target::of(db, first.into()) };
        let mut before = Vec::new();
        if !target.apple
            && let Some(gcc_ld) = sysroot.and_then(lld_dir)
        {
            before.extend([format!("-B{}", gcc_ld.display()), "-fuse-ld=lld".to_owned()]);
        }
        before.extend(target.leading());
        let native = native(db, program, &target);
        let mut after = native.search;
        for lib in &native.libs {
            after.extend(target.lib(lib));
        }
        after.extend(native.args);
        after.extend(target.trailing());
        Ok(Line { before, after, apple: target.apple, env: Environment::of(db) })
    }

    /// Links `objects` into the executable `exe`. How long `cc` took.
    pub(crate) fn link(&self, objects: &[&Path], exe: &Path) -> anyhow::Result<Duration> {
        let mut cmd = Command::new("cc");
        self.env.apply(&mut cmd);
        cmd.args(&self.before);
        for &object in objects {
            if split::is_archive(&leading_bytes(object)?) {
                cmd.arg("-Wl,--whole-archive").arg(object).arg("-Wl,--no-whole-archive");
            } else {
                cmd.arg(object);
            }
        }
        cmd.args(&self.after).arg("-o").arg(exe);
        let shown = diagnostic_command(&cmd);
        eprintln!("link line: {shown}");
        let start = Instant::now();
        let ran = cmd.output().with_context(|| format!("running {shown}"))?;
        if !ran.status.success() {
            bail!(
                "the link of {} failed ({}):\n{}\n{}",
                exe.display(),
                ran.status,
                shown,
                String::from_utf8_lossy(&ran.stderr)
            );
        }
        Ok(start.elapsed())
    }
}

/// Command's Debug includes all explicitly supplied environment values. A link
/// diagnostic needs its argv, not credentials from the calling build request.
fn diagnostic_command(command: &Command) -> String {
    let mut argv = Command::new(command.get_program());
    argv.args(command.get_args());
    format!("{argv:?}")
}

/// The first bytes of the file at `path`, which tell an archive from an object.
fn leading_bytes(path: &Path) -> anyhow::Result<[u8; 8]> {
    use std::io::Read;
    let mut bytes = [0; 8];
    let mut file =
        std::fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
    match file.read_exact(&mut bytes) {
        Ok(()) => Ok(bytes),
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => Ok([0; 8]),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

/// What the stand-ins left in the executable say: the functions ra-jit could not compile that
/// the program can reach, as the link leaves out every one it can't.
pub(crate) fn stand_ins(exe: &Path) -> anyhow::Result<Vec<String>> {
    let bytes = std::fs::read(exe).with_context(|| format!("reading {}", exe.display()))?;
    let said =
        memchr::memmem::find_iter(&bytes, hir_ty_codegen::STAND_IN_SAYS.as_bytes()).map(|at| {
            let rest = &bytes[at..];
            let end = memchr::memchr(b'\n', rest).unwrap_or(rest.len());
            String::from_utf8_lossy(&rest[..end]).into_owned()
        });
    Ok(said.collect())
}

/// The toolchain's directory with lld as `ld.lld`, for `cc -B`: the `gcc-ld` of the host's
/// `lib/rustlib/<host>/bin`.
pub(crate) fn lld_dir(sysroot: &Path) -> Option<PathBuf> {
    std::fs::read_dir(sysroot.join("lib/rustlib"))
        .ok()?
        .filter_map(|it| Some(it.ok()?.path().join("bin/gcc-ld")))
        .find(|it| it.join("ld.lld").is_file())
}

/// What the link line has of the crates `program` links beyond their objects.
#[derive(Default)]
struct Native {
    /// Where the libraries are looked for: `-L` and `-F`.
    search: Vec<String>,
    libs: Vec<NativeLib>,
    /// What the build scripts of the program's packages give the linker as it is.
    args: Vec<String>,
}

/// The native libraries of the crates `program` links, as rustc orders them: a crate's before
/// those of the crates it depends on, each crate's in the order written and then those its
/// build script asked for, and each library last where it is asked for more than once.
fn native(db: &RootDatabase, program: &[Crate], target: &Target) -> Native {
    let mut seen = FxHashSet::default();
    let mut order = Vec::new();
    let unlinked = crate::unlinked_panic_runtime(db, program);
    let programs: FxHashSet<base_db::Crate> = program.iter().map(|&it| it.into()).collect();
    for &krate in program {
        depth_first(db, krate.into(), unlinked, &programs, &mut seen, &mut order);
    }
    let mut native = Native::default();
    for &krate in order.iter().rev() {
        let attributes = hir_def::nameres::crate_def_map(db, krate)
            .modules(db)
            .flat_map(|(_, module)| module.scope(db).extern_blocks())
            .flat_map(|block| AttrFlags::native_libs(db, block).iter().cloned());
        native.libs.extend(attributes.collect::<Vec<_>>());
        let asked = asked_by_script(db, krate, target);
        native.libs.extend(asked.libs);
        native.search.extend(asked.search);
        // As cargo has it: a script's arguments are for its own package's targets.
        if programs.contains(&krate) {
            native.args.extend(asked.args);
        }
    }
    let mut later = FxHashSet::default();
    native.libs.reverse();
    native.libs.retain(|lib| later.insert((lib.name.clone(), lib.kind)));
    native.libs.reverse();
    let mut searched = FxHashSet::default();
    native.search.retain(|it| searched.insert(it.clone()));
    native
}

/// What the build script of `krate`'s package printed for the link, as cargo passes it to
/// rustc: `rustc-link-lib`, `rustc-link-search`, `rustc-flags` and `rustc-link-arg`.
fn asked_by_script(db: &RootDatabase, krate: base_db::Crate, target: &Target) -> Native {
    let mut asked = Native::default();
    let Some(out_dir) = krate.env(db).get("OUT_DIR") else { return asked };
    let printed = crate::cargo::script::printed(Path::new(&out_dir));
    let Some(Ok(printed)) = printed.map(std::fs::read_to_string) else { return asked };
    for line in printed.lines() {
        let Some(line) = line.strip_prefix("cargo:") else { continue };
        let line = line.strip_prefix(':').unwrap_or(line);
        let Some((key, value)) = line.split_once('=') else { continue };
        match key {
            "rustc-link-lib" => asked.libs.extend(lib_of(value)),
            "rustc-link-search" => asked.search.extend(target.search(value)),
            "rustc-link-arg" | "rustc-link-arg-bins" => asked.args.push(value.to_owned()),
            "rustc-flags" => {
                let mut flags = value.split_whitespace();
                while let Some(flag) = flags.next() {
                    let (kind, rest) = flag.split_at(flag.len().min(2));
                    let rest = if rest.is_empty() { flags.next().unwrap_or("") } else { rest };
                    match kind {
                        "-l" => asked.libs.extend(lib_of(rest)),
                        "-L" => asked.search.extend(target.search(rest)),
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }
    asked
}

/// The library `-l` names: `[KIND[:MODIFIERS]=]NAME[:RENAME]`.
fn lib_of(spec: &str) -> Option<NativeLib> {
    let (kind, name) = match spec.split_once('=') {
        Some((kind, name)) => (Some(kind), name),
        None => (None, spec),
    };
    let (kind, modifiers) = match kind.map(|it| it.split_once(':').unwrap_or((it, ""))) {
        Some((kind, modifiers)) => (Some(kind), modifiers),
        None => (None, ""),
    };
    let kind = match kind {
        None => NativeLibKind::Unspecified,
        Some("static") => NativeLibKind::Static,
        Some("dylib") => NativeLibKind::Dylib,
        Some("framework") => NativeLibKind::Framework,
        Some(_) => return None,
    };
    let name = name.split_once(':').map_or(name, |(_, renamed)| renamed);
    let on = |modifier: &str| modifiers.split(',').any(|it| it.strip_prefix('+') == Some(modifier));
    (!name.is_empty()).then(|| NativeLib {
        name: intern::Symbol::intern(name),
        kind,
        verbatim: on("verbatim"),
        whole_archive: on("whole-archive"),
    })
}

/// Puts `krate` in `order` after every crate it links: what the panic runtime named
/// `unlinked` links is not linked, nor what a proc macro or `test` links unless it is one of
/// the `programs`. A proc macro's library has the macro crate for its program and links libc
/// through it: with libc not named, glibc gives each of its symbols the oldest version, a
/// `realpath` that takes no null buffer.
fn depth_first(
    db: &RootDatabase,
    krate: base_db::Crate,
    unlinked: &str,
    programs: &FxHashSet<base_db::Crate>,
    seen: &mut FxHashSet<base_db::Crate>,
    order: &mut Vec<base_db::Crate>,
) {
    let data = krate.data(db);
    let left_out = (!programs.contains(&krate)
        && (data.target_kind.is_proc_macro()
            || matches!(
                data.origin,
                CrateOrigin::Lang(LangCrateOrigin::ProcMacro | LangCrateOrigin::Test)
            )))
        || Crate::from(krate)
            .display_name(db)
            .is_some_and(|name| name.crate_name().as_str() == unlinked);
    if left_out || !seen.insert(krate) {
        return;
    }
    for dep in &data.dependencies {
        depth_first(db, dep.crate_id, unlinked, programs, seen, order);
    }
    order.push(krate);
}

/// What the link line is made of for the target, from the cfgs of the program's crate.
struct Target {
    apple: bool,
    arch: String,
    /// A shared library is linked, not an executable.
    shared: bool,
}

impl Target {
    fn of(db: &RootDatabase, krate: base_db::Crate) -> Target {
        let cfg = krate.cfg_options(db);
        let value = |key: &str| {
            cfg.get_cfg_values(key).next().map(|it| it.as_str().to_owned()).unwrap_or_default()
        };
        let (apple, arch) = (value("target_vendor") == "apple", value("target_arch"));
        Target { apple, arch, shared: false }
    }

    fn leading(&self) -> Vec<String> {
        if self.apple {
            let arch = if self.arch == "aarch64" { "arm64" } else { &self.arch };
            ["-arch", arch].map(str::to_owned).to_vec()
        } else {
            let width = match &*self.arch {
                "x86_64" => Some("-m64"),
                "x86" => Some("-m32"),
                _ => None,
            };
            width.into_iter().chain(["-Wl,--as-needed"]).map(str::to_owned).collect()
        }
    }

    fn lib(&self, lib: &NativeLib) -> Vec<String> {
        let name = lib.name.as_str();
        if self.apple {
            return match lib.kind {
                NativeLibKind::Framework => vec!["-framework".to_owned(), name.to_owned()],
                _ if lib.verbatim => vec![name.to_owned()],
                NativeLibKind::Static if lib.whole_archive => {
                    vec![format!("-Wl,-force_load,lib{name}.a")]
                }
                _ => vec![format!("-l{name}")],
            };
        }
        let l = if lib.verbatim { format!("-l:{name}") } else { format!("-l{name}") };
        match lib.kind {
            NativeLibKind::Static if lib.whole_archive => [
                "-Wl,-Bstatic",
                "-Wl,--whole-archive",
                &l,
                "-Wl,--no-whole-archive",
                "-Wl,-Bdynamic",
            ]
            .map(str::to_owned)
            .to_vec(),
            NativeLibKind::Static => {
                ["-Wl,-Bstatic", &l, "-Wl,-Bdynamic"].map(str::to_owned).to_vec()
            }
            _ => vec![l],
        }
    }

    /// Where a script's `rustc-link-search=[KIND=]PATH` has the linker look: rustc gives it
    /// the native and framework paths, not those it finds crates in.
    fn search(&self, spec: &str) -> Option<String> {
        let (kind, path) = match spec.split_once('=') {
            Some((kind @ ("dependency" | "crate" | "native" | "framework" | "all"), path)) => {
                (kind, path)
            }
            _ => ("all", spec),
        };
        match kind {
            "native" | "all" => Some(format!("-L{path}")),
            "framework" => Some(if self.apple { format!("-F{path}") } else { format!("-L{path}") }),
            _ => None,
        }
    }

    /// The rest of rustc's arguments: what is not reached is left out, and nothing is linked
    /// that the crates did not ask for.
    fn trailing(&self) -> Vec<String> {
        let args: &[&str] = match (self.apple, self.shared) {
            (true, false) => &["-Wl,-dead_strip", "-nodefaultlibs"],
            (true, true) => &["-dynamiclib", "-Wl,-dead_strip", "-nodefaultlibs"],
            (false, false) => &[
                "-Wl,--eh-frame-hdr",
                "-Wl,-z,noexecstack",
                "-Wl,--gc-sections",
                "-pie",
                "-Wl,-z,relro,-z,now",
                "-nodefaultlibs",
            ],
            // A call in the library goes to its own definition, which the code generator
            // took for one in the same object: nothing loaded before it may stand in.
            (false, true) => &[
                "-Wl,--eh-frame-hdr",
                "-Wl,-z,noexecstack",
                "-Wl,--gc-sections",
                "-shared",
                "-Wl,-Bsymbolic",
                "-Wl,-z,relro,-z,now",
                "-nodefaultlibs",
            ],
        };
        args.iter().map(|it| (*it).to_owned()).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_scripts_libraries_are_read_as_rustc_reads_them() {
        let lib = |spec| {
            lib_of(spec)
                .map(|it| (it.name.as_str().to_owned(), it.kind, it.verbatim, it.whole_archive))
        };
        assert_eq!(
            lib("sqlite3"),
            Some(("sqlite3".to_owned(), NativeLibKind::Unspecified, false, false))
        );
        assert_eq!(
            lib("static=sqlite3"),
            Some(("sqlite3".to_owned(), NativeLibKind::Static, false, false))
        );
        assert_eq!(
            lib("static:+whole-archive,-bundle=foo:bar"),
            Some(("bar".to_owned(), NativeLibKind::Static, false, true))
        );
        assert_eq!(
            lib("dylib:+verbatim=libz.so.1"),
            Some(("libz.so.1".to_owned(), NativeLibKind::Dylib, true, false))
        );
        assert_eq!(lib("bogus=x"), None);
        let linux = Target { apple: false, arch: "x86_64".to_owned(), shared: false };
        assert_eq!(linux.search("native=/a/out").as_deref(), Some("-L/a/out"));
        assert_eq!(linux.search("/b").as_deref(), Some("-L/b"));
        assert_eq!(linux.search("dependency=/c"), None);
    }
}
