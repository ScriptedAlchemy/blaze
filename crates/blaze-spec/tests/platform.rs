//! The `ExecPlatform` of canned machines and of this one: every field of the macOS, glibc and
//! musl platforms, the tools by their bytes, and what does and does not change the digest.

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

use blaze_spec::platform::{Machine, Probe, System, exec_platform};
use blaze_wire::{Digest, ExecPlatform, digest_of};
use sha2::{Digest as _, Sha256};

/// A machine of canned files and canned programs, so that a platform of any machine can be
/// asserted without having that machine.
#[derive(Clone, Default)]
struct Fake {
    os: Option<String>,
    /// The files the probe reads, by absolute path.
    files: BTreeMap<String, Vec<u8>>,
    /// What a run prints, by the program and its arguments; a run that is absent failed.
    runs: BTreeMap<String, String>,
    native_files: BTreeMap<PathBuf, Vec<u8>>,
    deny_native_trees: bool,
    outputs: BTreeMap<(PathBuf, Vec<std::ffi::OsString>), (Vec<u8>, Vec<u8>)>,
    queries: std::cell::RefCell<
        Vec<(BTreeMap<String, std::ffi::OsString>, PathBuf, Vec<std::ffi::OsString>)>,
    >,
}

impl Fake {
    fn file(mut self, path: &str, bytes: &[u8]) -> Fake {
        self.files.insert(path.to_owned(), bytes.to_vec());
        self
    }

    fn run(mut self, program: &str, args: &[&str], out: &str) -> Fake {
        self.runs.insert(run_key(program, args), out.to_owned());
        self
    }
    fn output(mut self, program: &str, args: &[&str], stdout: &[u8], stderr: &[u8]) -> Fake {
        self.outputs.insert(
            (PathBuf::from(program), args.iter().map(std::ffi::OsString::from).collect()),
            (stdout.to_vec(), stderr.to_vec()),
        );
        self
    }
}

fn run_key(program: &str, args: &[&str]) -> String {
    format!("{program}\0{}", args.join("\0"))
}

impl Probe for Fake {
    fn run_output_in(
        &self,
        _cwd: &Path,
        env: &BTreeMap<String, std::ffi::OsString>,
        program: &Path,
        args: &[std::ffi::OsString],
    ) -> Option<(Vec<u8>, Vec<u8>)> {
        self.queries.borrow_mut().push((env.clone(), program.to_owned(), args.to_vec()));
        self.outputs
            .get(&(program.to_owned(), args.to_vec()))
            .cloned()
            .or_else(|| self.run_path(program, args).map(|out| (out, Vec::new())))
    }

    fn native_tree(&self, path: &Path) -> std::io::Result<blaze_spec::native_tree::Tree> {
        if self.deny_native_trees {
            return Err(std::io::ErrorKind::PermissionDenied.into());
        }
        blaze_spec::native_tree::Tree::from_files(
            path,
            self.files
                .iter()
                .map(|(path, bytes)| (PathBuf::from(path), bytes.clone()))
                .chain(self.native_files.iter().map(|(path, bytes)| (path.clone(), bytes.clone()))),
        )
    }

    fn which_native(&self, n: &std::ffi::OsStr, p: &std::ffi::OsStr) -> Option<PathBuf> {
        self.which(n.to_str()?, p.to_str()?)
    }
    fn run_native(&self, p: &Path, a: &[std::ffi::OsString]) -> Option<String> {
        self.run(p, &a.iter().map(|v| v.to_str()).collect::<Option<Vec<_>>>()?)
    }
    fn run_path(&self, p: &Path, a: &[std::ffi::OsString]) -> Option<Vec<u8>> {
        self.run_native(p, a).map(String::into_bytes)
    }
    fn which(&self, name: &str, path: &str) -> Option<PathBuf> {
        if Path::new(name).components().count() > 1 {
            return self.files.contains_key(name).then(|| PathBuf::from(name));
        }
        std::env::split_paths(path)
            .map(|dir| dir.join(name))
            .find(|it| self.files.contains_key(it.to_string_lossy().as_ref()))
    }

    fn run(&self, program: &Path, args: &[&str]) -> Option<String> {
        self.runs.get(&run_key(program.to_str()?, args)).cloned()
    }

    fn read(&self, path: &Path) -> Option<Vec<u8>> {
        self.files.get(path.to_str()?).cloned()
    }

    fn os(&self) -> Option<String> {
        self.os.clone()
    }
}

/// The SHA-256 of `bytes`, as a platform's digest.
fn digest(bytes: &[u8]) -> Digest {
    Sha256::digest(bytes).into()
}

fn vars(pairs: &[(&str, &str)]) -> BTreeMap<String, std::ffi::OsString> {
    pairs.iter().map(|(name, value)| ((*name).to_owned(), (*value).into())).collect()
}

const TRIPLE: &str = "aarch64-apple-darwin";
const CPU: &str = "apple-m1";

/// The shim `/usr/bin` holds, which is one stub for every tool there.
const SHIM: &[u8] = b"the shim of /usr/bin";
const CLANG: &[u8] = b"the developer directory's clang";
const AR: &[u8] = b"the developer directory's ar";
const SDK: &str = "/SDKs/MacOSX26.0.sdk";
const XCODE_CC: &str = "/Xcode/usr/bin/cc";
const XCODE_CXX: &str = "/Xcode/usr/bin/c++";
const XCODE_AR: &str = "/Xcode/usr/bin/ar";
const SETTINGS: &str = "/SDKs/MacOSX26.0.sdk/SDKSettings.json";
const LIBSYSTEM: &str = "/SDKs/MacOSX26.0.sdk/usr/lib/libSystem.tbd";

/// The macOS machine the tests read a platform from: `/usr/bin`'s shims, the developer
/// directory's tools behind them, and one SDK.
fn macos() -> Fake {
    let mut fake = Fake { os: Some("macos 26.6.2".to_owned()), ..Fake::default() };
    for name in ["cc", "c++", "ar", "ld"] {
        fake = fake.file(&format!("/usr/bin/{name}"), SHIM);
    }
    fake = fake
        .file(XCODE_CC, CLANG)
        .file(XCODE_CXX, CLANG)
        .file(XCODE_AR, AR)
        .file(SETTINGS, br#"{"Version": "26.0", "DisplayName": "macOS 26.0"}"#)
        .file(LIBSYSTEM, b"the names libSystem exports")
        .run("/usr/bin/env", &xcrun(["--find", "cc"]), &format!("{XCODE_CC}\n"))
        .run("/usr/bin/env", &xcrun(["--find", "c++"]), &format!("{XCODE_CXX}\n"))
        .run("/usr/bin/env", &xcrun(["--find", "ar"]), &format!("{XCODE_AR}\n"))
        .run("/usr/bin/env", &xcrun(["--show-sdk-path"]), &format!("{SDK}\n"))
        .run("/usr/bin/env", &xcrun(["--show-sdk-version"]), "26.0\n")
        .run(
            XCODE_CC,
            &["--version"],
            "Apple clang version 21.0.0 (clang-2100.3.34.2)\nTarget: arm64\n",
        )
        .run(XCODE_CXX, &["--version"], "Apple clang version 21.0.0 (clang-2100.3.34.2)\n");
    // `ar --version` fails on macOS, so the archiver's version is empty: no run is canned for
    // it, and a run that is not canned failed.
    fake
}

/// The words of an `xcrun` call as the probe runs it, with `DEVELOPER_DIR` unset.
fn xcrun<const N: usize>(args: [&str; N]) -> Vec<&str> {
    ["-u", "DEVELOPER_DIR", "/usr/bin/xcrun"].into_iter().chain(args).collect()
}

fn macos_machine<'a>(
    env: &'a BTreeMap<String, std::ffi::OsString>,
    linker: Option<&'a str>,
) -> Machine<'a> {
    Machine { cwd: Path::new("/"), triple: TRIPLE, cpu: CPU, env, linker }
}

fn macos_env() -> BTreeMap<String, std::ffi::OsString> {
    vars(&[("PATH", "/usr/bin:/opt/homebrew/bin")])
}

#[test]
fn macos_platform() {
    let fake = macos();
    let platform = exec_platform(&macos_machine(&macos_env(), None), &fake).unwrap();
    assert_eq!(platform.triple, TRIPLE);
    assert_eq!(platform.cpu, CPU);
    assert_eq!(platform.os, "macos 26.6.2");
    assert_eq!(platform.sdk, Some(("macosx".to_owned(), "26.0".to_owned(), "11.0".to_owned())));
    assert_eq!(platform.libc, None);
    // The linker is `cc`, resolved through the shim to the developer directory's clang.
    assert_eq!(
        platform.linker,
        (
            "cc".to_owned(),
            "Apple clang version 21.0.0 (clang-2100.3.34.2)".to_owned(),
            digest(CLANG)
        )
    );
    assert_eq!(
        platform.cc,
        Some((
            "cc".to_owned(),
            "Apple clang version 21.0.0 (clang-2100.3.34.2)".to_owned(),
            digest(CLANG)
        ))
    );
    assert_eq!(
        platform.cxx,
        Some((
            "c++".to_owned(),
            "Apple clang version 21.0.0 (clang-2100.3.34.2)".to_owned(),
            digest(CLANG)
        ))
    );
    assert_eq!(platform.ar, Some(("ar".to_owned(), String::new(), digest(AR))));
    assert!(platform.native_root.is_some());
}

#[test]
fn sdk_platform_binds_headers_other_libraries_and_new_members() {
    let original = macos()
        .file(&format!("{SDK}/usr/include/nested/config.h"), b"header")
        .file(&format!("{SDK}/usr/lib/libc++.tbd"), b"other library")
        .file(
            &format!("{SDK}/System/Library/Frameworks/Metal.framework/Modules/module.modulemap"),
            b"framework module",
        );
    let env = macos_env();
    let before = exec_platform(&macos_machine(&env, None), &original).unwrap();
    for (path, bytes) in [
        ("usr/include/nested/config.h", b"changed header".as_slice()),
        ("usr/lib/libc++.tbd", b"changed library"),
        ("System/Library/Frameworks/Metal.framework/Modules/module.modulemap", b"changed module"),
        ("usr/include/new.h", b"new member"),
    ] {
        let changed = original.clone().file(&format!("{SDK}/{path}"), bytes);
        let after = exec_platform(&macos_machine(&env, None), &changed).unwrap();
        assert_eq!(before.cc, after.cc);
        assert_eq!(before.sdk, after.sdk);
        assert_ne!(before.native_root, after.native_root, "unbound SDK member {path}");
        assert_ne!(digest_of(&before), digest_of(&after));
    }
    let mut deleted = original.clone();
    deleted.files.remove(&format!("{SDK}/usr/lib/libc++.tbd"));
    let after = exec_platform(&macos_machine(&env, None), &deleted).unwrap();
    assert_ne!(before.native_root, after.native_root);
}

#[test]
fn the_same_complete_sdk_at_a_different_location_has_the_same_platform() {
    let first = macos()
        .file(&format!("{SDK}/usr/include/config.h"), b"header")
        .file(&format!("{SDK}/usr/lib/libc++.tbd"), b"other library");
    let moved_sdk = "/relocated/SDKs/MacOSX26.0.sdk";
    let mut moved = first.clone();
    moved.files = first
        .files
        .iter()
        .map(|(path, bytes)| {
            let path = path
                .strip_prefix(SDK)
                .map_or_else(|| path.clone(), |suffix| format!("{moved_sdk}{suffix}"));
            (path, bytes.clone())
        })
        .collect();
    let first_env = vars(&[("PATH", "/usr/bin"), ("SDKROOT", SDK)]);
    let moved_env = vars(&[("PATH", "/usr/bin"), ("SDKROOT", moved_sdk)]);
    let before = exec_platform(&macos_machine(&first_env, None), &first).unwrap();
    let after = exec_platform(&macos_machine(&moved_env, None), &moved).unwrap();
    assert_eq!(before, after);
    assert_eq!(digest_of(&before), digest_of(&after));
}

#[test]
fn linux_glibc_platform() {
    let fake = Fake { os: Some("linux 6.8".to_owned()), ..Fake::default() }
        .file("/usr/bin/cc", b"gcc")
        .file("/usr/bin/c++", b"g++")
        .file("/usr/bin/ar", b"binutils ar")
        .file("/usr/bin/getconf", b"getconf")
        .file("/lib/x86_64-linux-gnu/libc.so.6", b"glibc 2.39")
        .file("/lib64/ld-linux-x86-64.so.2", b"the loader")
        .run("/usr/bin/cc", &["--version"], "cc (Ubuntu 13.3.0-6ubuntu2~24.04) 13.3.0\n")
        .run("/usr/bin/c++", &["--version"], "c++ (Ubuntu 13.3.0-6ubuntu2~24.04) 13.3.0\n")
        .run("/usr/bin/ar", &["--version"], "GNU ar (GNU Binutils for Ubuntu) 2.42\n")
        .run("/usr/bin/getconf", &["GNU_LIBC_VERSION"], "glibc 2.39\n");
    let env = vars(&[("PATH", "/usr/local/bin:/usr/bin:/bin")]);
    let platform = exec_platform(
        &Machine {
            cwd: Path::new("/"),
            triple: "x86_64-unknown-linux-gnu",
            cpu: "x86-64",
            env: &env,
            linker: None,
        },
        &fake,
    )
    .unwrap();
    assert_eq!(platform.triple, "x86_64-unknown-linux-gnu");
    assert_eq!(platform.cpu, "x86-64");
    assert_eq!(platform.os, "linux 6.8");
    assert_eq!(platform.sdk, None);
    assert_eq!(platform.libc, Some(("glibc".to_owned(), "2.39".to_owned())));
    assert_eq!(
        platform.linker,
        ("cc".to_owned(), "cc (Ubuntu 13.3.0-6ubuntu2~24.04) 13.3.0".to_owned(), digest(b"gcc"))
    );
    assert_eq!(
        platform.cc,
        Some((
            "cc".to_owned(),
            "cc (Ubuntu 13.3.0-6ubuntu2~24.04) 13.3.0".to_owned(),
            digest(b"gcc")
        ))
    );
    assert_eq!(
        platform.cxx,
        Some((
            "c++".to_owned(),
            "c++ (Ubuntu 13.3.0-6ubuntu2~24.04) 13.3.0".to_owned(),
            digest(b"g++")
        ))
    );
    assert_eq!(
        platform.ar,
        Some((
            "ar".to_owned(),
            "GNU ar (GNU Binutils for Ubuntu) 2.42".to_owned(),
            digest(b"binutils ar")
        ))
    );
    assert!(platform.native_root.is_some());
}

#[test]
fn compiler_sysroot_libraries_are_bound_outside_distribution_paths() {
    let mut fake = Fake { os: Some("linux 6.8".to_owned()), ..Fake::default() }
        .file("/usr/bin/cc", b"gcc")
        .file("/usr/bin/getconf", b"getconf")
        .file("/custom/sysroot/lib/libc.so.6", b"sysroot libc")
        .file("/custom/sysroot/lib/ld-linux-x86-64.so.2", b"sysroot loader")
        .run("/usr/bin/getconf", &["GNU_LIBC_VERSION"], "glibc 2.39\n")
        .run("/usr/bin/cc", &["-print-file-name=libc.so.6"], "/custom/sysroot/lib/libc.so.6\n")
        .run(
            "/usr/bin/cc",
            &["-print-file-name=ld-linux-x86-64.so.2"],
            "/custom/sysroot/lib/ld-linux-x86-64.so.2\n",
        );
    let env = vars(&[("PATH", "/usr/bin")]);
    let cwd = Path::new("/");
    let machine =
        Machine { cwd, env: &env, triple: "x86_64-unknown-linux-gnu", cpu: "x86-64", linker: None };
    let before = exec_platform(&machine, &fake).unwrap();
    assert!(before.native_root.is_some());
    fake.files.insert("/custom/sysroot/lib/libc.so.6".into(), b"changed libc".to_vec());
    let after = exec_platform(&machine, &fake).unwrap();
    assert_eq!(before.cc, after.cc);
    assert_ne!(before.native_root, after.native_root);
    assert_ne!(digest_of(&before), digest_of(&after));
}

#[test]
fn compiler_search_trees_bind_headers_libraries_and_absent_candidates() {
    let original = Fake { os: Some("linux 6.8".to_owned()), ..Fake::default() }
        .file("/usr/bin/cc", b"gcc")
        .file("/usr/bin/getconf", b"getconf")
        .file("/lib/x86_64-linux-gnu/libc.so.6", b"glibc")
        .file("/custom/include/nested/config.h", b"header")
        .file("/custom/lib/libdependency.a", b"library")
        .run("/usr/bin/getconf", &["GNU_LIBC_VERSION"], "glibc 2.39\n")
        .run("/usr/bin/cc", &["-print-search-dirs"], "libraries: =/custom/lib:/custom/absent\n")
        .run(
            "/usr/bin/cc",
            &["-E", "-x", "c", "-", "-v"],
            "#include <...> search starts here:\n /custom/include\nEnd of search list.\n",
        );
    let env = vars(&[("PATH", "/usr/bin")]);
    let machine = Machine {
        cwd: Path::new("/"),
        triple: "x86_64-unknown-linux-gnu",
        cpu: "x86-64",
        env: &env,
        linker: None,
    };
    let before = exec_platform(&machine, &original).unwrap();
    for path in [
        "/custom/include/nested/config.h",
        "/custom/include/new.h",
        "/custom/lib/libdependency.a",
        "/custom/lib/libnew.a",
        "/custom/absent/libnew.a",
    ] {
        let changed = original.clone().file(path, b"changed member");
        let after = exec_platform(&machine, &changed).unwrap();
        assert_eq!(before.cc, after.cc);
        assert_eq!(before.libc, after.libc);
        assert_ne!(before.native_root, after.native_root, "unbound compiler search member {path}");
        assert_ne!(digest_of(&before), digest_of(&after));
    }
}

fn native_flags_probe() -> Fake {
    Fake { os: Some("linux 6.8".to_owned()), ..Fake::default() }
        .file("/usr/bin/cc", b"gcc")
        .file("/usr/bin/getconf", b"getconf")
        .file("/lib/x86_64-linux-gnu/libc.so.6", b"glibc")
        .run("/usr/bin/getconf", &["GNU_LIBC_VERSION"], "glibc 2.39\n")
}

fn native_flags_machine(env: &BTreeMap<String, std::ffi::OsString>) -> Machine<'_> {
    Machine {
        cwd: Path::new("/workspace"),
        env,
        triple: "x86_64-unknown-linux-gnu",
        cpu: "x86-64",
        linker: None,
    }
}

#[test]
fn live_facts_do_not_claim_or_require_portable_tree_bindings() {
    let mut probe = native_flags_probe();
    probe.deny_native_trees = true;
    let env = vars(&[("PATH", "/usr/bin")]);
    let machine = native_flags_machine(&env);
    let facts = blaze_spec::platform::facts(&machine, &probe).unwrap();
    assert_eq!(facts.native_root, None);
    assert_eq!(facts.triple, machine.triple);
    assert!(facts.cc.is_some());
    assert!(blaze_spec::platform::describe(&machine, &probe).is_err());
}

#[test]
fn captured_native_flag_trees_bind_external_bytes_and_membership() {
    for base in ["CFLAGS", "CXXFLAGS", "CPPFLAGS", "LDFLAGS"] {
        for name in [
            base.to_owned(),
            format!("HOST_{base}"),
            format!("TARGET_{base}"),
            format!("{base}_x86_64_unknown_linux_gnu"),
            format!("{base}_x86_64-unknown-linux-gnu"),
        ] {
            if !blaze_spec::environment::is_script_var(&name, "x86_64-unknown-linux-gnu") {
                continue;
            }
            for (flag, file) in [
                ("-I/external/include", "/external/include/config.h"),
                ("-L /external/lib", "/external/lib/libdependency.a"),
                ("-F/external/frameworks", "/external/frameworks/Dependency.framework/Dependency"),
            ] {
                let original = native_flags_probe().file(file, b"original");
                let mut env = vars(&[("PATH", "/usr/bin")]);
                env.insert(name.clone(), flag.into());
                let machine = native_flags_machine(&env);
                let before = blaze_spec::platform::describe(&machine, &original).unwrap();
                assert!(before.native.entry(Path::new(file)).is_some(), "{name}: {flag}");
                for changed in [
                    original.clone().file(file, b"changed"),
                    original.clone().file(&format!("{file}.new"), b"new member"),
                ] {
                    let after = exec_platform(&machine, &changed).unwrap();
                    assert_eq!(before.platform.cc, after.cc);
                    assert_ne!(before.platform.native_root, after.native_root, "{name}: {flag}");
                    assert_ne!(digest_of(&before.platform), digest_of(&after), "{name}: {flag}");
                }
            }
        }
    }
}

#[test]
fn shell_escaped_native_flags_keep_search_order_and_relative_paths() {
    let fake = native_flags_probe();
    let env = vars(&[
        ("PATH", "/usr/bin"),
        ("CC_SHELL_ESCAPED_FLAGS", "1"),
        (
            "CFLAGS",
            r#"-Ijoined -I 'include space' -isystem"/system space" -idirafter /after -iquotequoted -F'/framework space' --sysroot='/root space' -isysroot "/second root" -I"/literal\q" -I/continued\
/path # -I/not-selected
-I/final"#,
        ),
        ("HOST_CFLAGS", "-I/host"),
        ("TARGET_CFLAGS", "-I/target"),
        ("CFLAGS_x86_64_unknown_linux_gnu", "-I/underscored"),
        ("CFLAGS_x86_64-unknown-linux-gnu", "-I/literal"),
    ]);
    assert!(blaze_spec::environment::is_script_var(
        "CC_SHELL_ESCAPED_FLAGS",
        "x86_64-unknown-linux-gnu"
    ));
    let described = blaze_spec::platform::describe(&native_flags_machine(&env), &fake).unwrap();
    let roots: Vec<_> = described
        .native
        .roots
        .iter()
        .filter(|root| root.name.contains("/flags/"))
        .map(|root| root.tree.root.clone())
        .collect();
    let common = [
        "/workspace/joined",
        "/workspace/include space",
        "/system space",
        "/after",
        "/workspace/quoted",
        "/framework space",
        "/root space",
        "/second root",
        "/literal\\q",
        "/continued/path",
        "/final",
    ];
    let expected: Vec<_> = common
        .iter()
        .copied()
        .chain(["/host", "/underscored", "/literal"])
        .chain(common)
        .chain(["/target", "/underscored", "/literal"])
        .map(PathBuf::from)
        .collect();
    assert_eq!(roots, expected);
}

#[test]
fn forwarded_linker_and_preprocessor_search_paths_are_bound() {
    let fake = native_flags_probe();
    let env = vars(&[
        ("PATH", "/usr/bin"),
        ("LDFLAGS", "-Wl,-L,/first,-L/second -Xlinker -L -Xlinker /third -Wl,--sysroot=/link-root"),
        (
            "CPPFLAGS",
            "-Xpreprocessor -isystem -Xpreprocessor /headers -Xclang -iquote -Xclang /quoted",
        ),
    ]);
    let described = blaze_spec::platform::describe(&native_flags_machine(&env), &fake).unwrap();
    let roots: Vec<_> = described
        .native
        .roots
        .iter()
        .filter(|root| root.name.contains("/flags/"))
        .map(|root| root.tree.root.clone())
        .collect();
    assert_eq!(
        roots,
        ["/headers", "/quoted", "/first", "/second", "/third", "/link-root"].map(PathBuf::from)
    );
}

#[test]
fn forced_include_files_and_explicit_sysroot_trees_are_bound() {
    let original = native_flags_probe()
        .file("/external/forced.h", b"forced header")
        .file("/external/macros.h", b"forced macros")
        .file("/external/precompiled.pch", b"precompiled header")
        .file("/selected/sysroot/lib/libdependency.a", b"library")
        .file("/selected/headers/include/config.h", b"header");
    let env = vars(&[
        ("PATH", "/usr/bin"),
        (
            "CFLAGS",
            "-include /external/forced.h -imacros/external/macros.h -include-pch /external/precompiled.pch --sysroot=/selected/sysroot -isysroot /selected/headers",
        ),
    ]);
    let machine = native_flags_machine(&env);
    let before = blaze_spec::platform::describe(&machine, &original).unwrap();
    for path in [
        "/external/forced.h",
        "/external/macros.h",
        "/external/precompiled.pch",
        "/selected/sysroot/lib/libdependency.a",
        "/selected/headers/include/config.h",
    ] {
        assert!(before.native.entry(Path::new(path)).is_some());
        let changed = original.clone().file(path, b"changed");
        let after = exec_platform(&machine, &changed).unwrap();
        assert_ne!(before.platform.native_root, after.native_root, "{path}");
        assert_ne!(digest_of(&before.platform), digest_of(&after), "{path}");
    }
}

#[cfg(unix)]
#[test]
fn native_flag_trees_preserve_raw_path_bytes_without_filesystem_assumptions() {
    use std::os::unix::ffi::OsStringExt;
    let include =
        PathBuf::from(std::ffi::OsString::from_vec(b"/external/raw-\xff space/config.h".to_vec()));
    let library = PathBuf::from(std::ffi::OsString::from_vec(
        b"/external/rawlib-\xfe/libdependency.a".to_vec(),
    ));
    let forced =
        PathBuf::from(std::ffi::OsString::from_vec(b"/external/forced-\xfd file.h".to_vec()));
    let rooted = PathBuf::from(std::ffi::OsString::from_vec(
        b"/external/sysroot-\xfc space/include/config.h".to_vec(),
    ));
    let mut fake = native_flags_probe();
    fake.native_files.insert(include.clone(), b"header".to_vec());
    fake.native_files.insert(library.clone(), b"library".to_vec());
    fake.native_files.insert(forced.clone(), b"forced".to_vec());
    fake.native_files.insert(rooted.clone(), b"rooted".to_vec());
    let mut env = vars(&[("PATH", "/usr/bin"), ("CC_SHELL_ESCAPED_FLAGS", "true")]);
    env.insert(
        "CFLAGS".into(),
        std::ffi::OsString::from_vec(
            b"-I'/external/raw-\xff space' -L/external/rawlib-\xfe -include '/external/forced-\xfd file.h' --sysroot '/external/sysroot-\xfc space'".to_vec(),
        ),
    );
    let machine = native_flags_machine(&env);
    let before = blaze_spec::platform::describe(&machine, &fake).unwrap();
    for path in [include, library, forced, rooted] {
        assert!(before.native.entry(&path).is_some());
        let mut changed = fake.clone();
        changed.native_files.insert(path, b"changed".to_vec());
        let after = exec_platform(&machine, &changed).unwrap();
        assert_ne!(before.platform.native_root, after.native_root);
        assert_ne!(digest_of(&before.platform), digest_of(&after));
    }
}

#[test]
fn interpreted_header_paths_bind_actual_query_roots_and_absence() {
    for (flags, query, selected) in [
        ("-I=relative", vec!["-I", "=relative"], "=relative"),
        (
            "--sysroot=/sdk -I=relative",
            vec!["--sysroot", "/sdk", "-I", "=relative"],
            "/sdkrelative",
        ),
        (
            "--sysroot=/sdk -I=relative",
            vec!["--sysroot", "/sdk", "-I", "=relative"],
            "/sdk/relative",
        ),
        (
            "--sysroot=/sdk -I$SYSROOT/relative",
            vec!["--sysroot", "/sdk", "-I", "$SYSROOT/relative"],
            "$SYSROOT/relative",
        ),
        (
            "-iprefix /prefix -iwithprefix after -iwithprefixbefore before",
            vec!["-iprefix", "/prefix", "-iwithprefix", "after", "-iwithprefixbefore", "before"],
            "/prefixafter",
        ),
        (
            "-isysroot /sdk -iframeworkwithsysroot /Frameworks",
            vec!["-isysroot", "/sdk", "-iframeworkwithsysroot", "/Frameworks"],
            "/sdk/Frameworks",
        ),
    ] {
        let mut args = query;
        args.extend(["-E", "-x", "c", "-", "-v"]);
        let missing = format!("{selected}-not-yet-present");
        let listing = format!(
            "ignoring nonexistent directory \"{missing}\"\n#include \"...\" search starts here:\n#include <...> search starts here:\n {selected}\nEnd of search list.\n"
        );
        let file = Path::new("/workspace").join(selected).join("config.h");
        let original = native_flags_probe().file(file.to_str().unwrap(), b"original").output(
            "/usr/bin/cc",
            &args,
            b"",
            listing.as_bytes(),
        );
        let mut env = vars(&[("PATH", "/usr/bin")]);
        env.insert("CFLAGS".into(), flags.into());
        let machine = native_flags_machine(&env);
        let before = blaze_spec::platform::describe(&machine, &original).unwrap();
        assert!(before.native.entry(&file).is_some(), "{flags}: {selected}");
        let selected_roots: Vec<_> = before
            .native
            .roots
            .iter()
            .filter(|root| root.name.starts_with("native/headers/query/"))
            .map(|root| root.tree.root.clone())
            .collect();
        assert_eq!(
            selected_roots,
            [Path::new("/workspace").join(&missing), Path::new("/workspace").join(selected)]
        );
        for (file, bytes) in [
            (file, b"changed".as_slice()),
            (Path::new("/workspace").join(&missing).join("new.h"), b"appeared".as_slice()),
        ] {
            let changed = original.clone().file(file.to_str().unwrap(), bytes);
            let after = exec_platform(&machine, &changed).unwrap();
            assert_ne!(before.platform.native_root, after.native_root, "{flags}");
            assert_ne!(digest_of(&before.platform), digest_of(&after), "{flags}");
        }
    }
}

#[test]
fn native_library_prefixes_follow_the_selected_linker_interface() {
    for (version, default_root, flags, query, expected) in [
        ("GNU ld (GNU Binutils) 2.42\n", "", "-L=relative", "ld", "/workspace/relative"),
        (
            "GNU ld (GNU Binutils) 2.42\n",
            "/default",
            "-L$SYSROOT/relative",
            "ld",
            "/default/relative",
        ),
        ("GNU ld (GNU Binutils) 2.42\n", "", "--sysroot=/sdk -L=relative", "ld", "/sdkrelative"),
        (
            "LLD 20.1.8 (compatible with GNU linkers)\n",
            "",
            "-fuse-ld=lld --sysroot=/sdk -L=relative",
            "ld.lld",
            "/sdk/relative",
        ),
        (
            "LLD 20.1.8 (compatible with GNU linkers)\n",
            "",
            "-fuse-ld=lld --sysroot=/sdk -L$SYSROOT/relative",
            "ld.lld",
            "/workspace/$SYSROOT/relative",
        ),
    ] {
        let mut args = Vec::new();
        if flags.contains("--sysroot=") {
            args.extend(["--sysroot", "/sdk"]);
        }
        if flags.contains("-fuse-ld=lld") {
            args.push("-fuse-ld=lld");
        }
        let mut preflight = args.clone();
        preflight.push("-print-sysroot");
        let mut driver_args = args.clone();
        driver_args.extend(["-###", "-x", "c", "/dev/null"]);
        let query_arg = format!("-print-prog-name={query}");
        args.push(&query_arg);
        let root_arg = if flags.contains("--sysroot=") { " \"--sysroot=/sdk\"" } else { "" };
        let fuse_arg = if flags.contains("-fuse-ld=lld") { " \"-fuse-ld=lld\"" } else { "" };
        let driver = format!(" \"/usr/lib/collect2\"{root_arg}{fuse_arg}\n");
        let effective_root = if flags.contains("--sysroot=") { "/sdk" } else { default_root };
        let mut root_query = Vec::new();
        if flags.contains("--sysroot=") {
            root_query.push("--sysroot=/sdk");
        }
        root_query.push("--print-sysroot");
        let tool = format!("/usr/bin/{query}");
        let file = format!("{expected}/libdependency.a");
        let original = native_flags_probe()
            .file("/usr/lib/collect2", b"GNU driver trampoline")
            .output("/usr/bin/cc", &preflight, b"", b"")
            .output("/usr/bin/cc", &driver_args, b"", driver.as_bytes())
            .file(&tool, b"linker bytes")
            .file(&file, b"library")
            .output("/usr/bin/cc", &args, format!("{tool}\n").as_bytes(), b"")
            .output("/usr/bin/cc", &["-print-sysroot"], b"", b"")
            .output(&tool, &["--version"], version.as_bytes(), b"")
            .output(&tool, &root_query, format!("{effective_root}\n").as_bytes(), b"");
        let mut env = vars(&[("PATH", "/usr/bin")]);
        env.insert("LDFLAGS".into(), flags.into());
        let machine = native_flags_machine(&env);
        let before = blaze_spec::platform::describe(&machine, &original).unwrap();
        assert!(before.native.entry(Path::new(&file)).is_some(), "{flags}");
        let paths: Vec<_> = before
            .native
            .roots
            .iter()
            .filter(|root| root.name.starts_with("linker/libraries/query/"))
            .map(|root| root.tree.root.clone())
            .collect();
        assert_eq!(paths, [PathBuf::from(expected)], "{flags}");
        let after = exec_platform(&machine, &original.clone().file(&file, b"changed")).unwrap();
        assert_ne!(before.platform.native_root, after.native_root);
        let mut missing = original.clone();
        missing.files.remove(&file);
        let absent = exec_platform(&machine, &missing).unwrap();
        assert_ne!(before.platform.native_root, absent.native_root, "{flags}: absent candidate");
    }
}

#[test]
fn native_library_paths_use_driver_defaults_before_linker_defaults() {
    for (clang, driver_default, expected) in [
        (false, Some("/compiler/default"), "/compiler/defaultrelative"),
        (true, Some("/compiler/default"), "/compiler/defaultrelative"),
        (true, None, "/linker/defaultrelative"),
    ] {
        let file = format!("{expected}/libdependency.a");
        let mut fake = native_flags_probe()
            .file("/usr/bin/ld", b"linker")
            .file(&file, b"library")
            .output("/usr/bin/ld", &["--version"], b"GNU ld (GNU Binutils) 2.42\n", b"");
        let mut root_query = Vec::new();
        let root_arg = if let Some(root) = driver_default {
            root_query.push(format!("--sysroot={root}"));
            format!(" \"--sysroot={root}\"")
        } else {
            String::new()
        };
        root_query.push("--print-sysroot".into());
        let borrowed = root_query.iter().map(String::as_str).collect::<Vec<_>>();
        fake = fake.output(
            "/usr/bin/ld",
            &borrowed,
            format!("{}\n", driver_default.unwrap_or("/linker/default")).as_bytes(),
            b"",
        );
        let dry_run = format!(" \"/usr/bin/ld\"{root_arg} \"-o\" \"a.out\"\n");
        fake =
            fake.output("/usr/bin/cc", &["-###", "-x", "c", "/dev/null"], b"", dry_run.as_bytes());
        if clang {
            fake = fake.run("/usr/bin/cc", &["--version"], "clang version 20.1.8\n");
        } else {
            fake = fake.output("/usr/bin/cc", &["-print-sysroot"], b"/compiler/default\n", b"");
        }
        let env = vars(&[("PATH", "/usr/bin"), ("LDFLAGS", "-L=relative")]);
        let machine = native_flags_machine(&env);
        let before = blaze_spec::platform::describe(&machine, &fake).unwrap();
        assert!(before.native.entry(Path::new(&file)).is_some());
        let changed = fake.clone().file(&file, b"changed");
        assert_ne!(
            before.platform.native_root,
            exec_platform(&machine, &changed).unwrap().native_root
        );
    }
}

#[cfg(unix)]
#[test]
fn explicit_linker_paths_preserve_native_bytes_without_a_program_name_query() {
    use std::os::unix::ffi::OsStringExt;
    let tool = PathBuf::from(std::ffi::OsString::from_vec(b"/external/ld-\xfe".to_vec()));
    let mut fake = native_flags_probe()
        .run("/usr/bin/cc", &["--version"], "clang version 20.1.8\n")
        .file("/sdklib/libdependency.a", b"library");
    fake.native_files.insert(tool.clone(), b"native linker".to_vec());
    fake.outputs.insert(
        (tool.clone(), vec!["--version".into()]),
        (b"GNU ld (GNU Binutils) 2.42\n".to_vec(), Vec::new()),
    );
    let driver_args = vec![
        "--sysroot".into(),
        "/sdk".into(),
        std::ffi::OsString::from_vec(b"--ld-path=/external/ld-\xfe".to_vec()),
        "-###".into(),
        "-x".into(),
        "c".into(),
        "/dev/null".into(),
    ];
    fake.outputs.insert(
        (PathBuf::from("/usr/bin/cc"), driver_args),
        (Vec::new(), b" \"/external/ld-\xfe\" \"--sysroot=/sdk\" \"-o\" \"a.out\"\n".to_vec()),
    );
    fake.outputs.insert(
        (tool.clone(), vec!["--sysroot=/sdk".into(), "--print-sysroot".into()]),
        (b"/sdk\n".to_vec(), Vec::new()),
    );
    let mut env = vars(&[("PATH", "/usr/bin")]);
    env.insert(
        "LDFLAGS".into(),
        std::ffi::OsString::from_vec(b"--ld-path=/external/ld-\xfe --sysroot=/sdk -L=lib".to_vec()),
    );
    let described = blaze_spec::platform::describe(&native_flags_machine(&env), &fake).unwrap();
    assert!(described.native.entry(Path::new("/sdklib/libdependency.a")).is_some());
    assert!(described.native.entry(&tool).is_some());
    assert!(!fake.queries.borrow().iter().any(|(_, _, args)| {
        args.iter().any(|arg| arg.as_encoded_bytes().starts_with(b"-print-prog-name="))
    }));
}

#[test]
fn portable_unknown_search_interface_is_explicit_while_live_facts_remain_accepted() {
    let fake = native_flags_probe();
    let env = vars(&[("PATH", "/usr/bin"), ("CFLAGS", "--sysroot=/sdk -I=relative")]);
    let machine = native_flags_machine(&env);
    let error = blaze_spec::platform::describe(&machine, &fake).err().unwrap();
    assert!(error.to_string().contains("native header search query"));
    assert_eq!(blaze_spec::platform::facts(&machine, &fake).unwrap().native_root, None);
}

#[test]
fn compound_environment_flag_controls_require_the_actual_native_invocation() {
    let fake = native_flags_probe();
    let env =
        vars(&[("PATH", "/usr/bin"), ("CFLAGS", "--sysroot=/sdk"), ("LDFLAGS", "-L=relative")]);
    let machine = native_flags_machine(&env);
    let error = blaze_spec::platform::describe(&machine, &fake).err().unwrap();
    assert!(error.to_string().contains("combined native invocation argv is not captured"));
    assert_eq!(blaze_spec::platform::facts(&machine, &fake).unwrap().native_root, None);
}

#[test]
fn native_queries_filter_output_plugin_and_dependency_switches() {
    let flags = "--sysroot=/sdk -I=relative -o /output -MF /dependencies -fplugin=/plugin -O3 -include /must-not-be-read.h";
    let listing = b"#include <...> search starts here:\n /sdkrelative\nEnd of search list.\n";
    let fake = native_flags_probe().output(
        "/usr/bin/cc",
        &["--sysroot", "/sdk", "-I", "=relative", "-E", "-x", "c", "-", "-v"],
        b"",
        listing,
    );
    let env = vars(&[
        ("PATH", "/usr/bin"),
        ("CC", "cc -o /tool-output -fplugin=/tool-plugin"),
        ("CFLAGS", flags),
        ("NIX_CFLAGS_COMPILE", "-isystem /nix/include -fplugin=/nix/plugin -o /nix/output"),
    ]);
    blaze_spec::platform::describe(&native_flags_machine(&env), &fake).unwrap();
    for (env, _, args) in fake.queries.borrow().iter() {
        assert_eq!(env.get("LC_ALL").unwrap(), "C");
        assert_eq!(env.get("NIX_CFLAGS_COMPILE").unwrap(), "-isystem /nix/include");
        for word in args {
            assert!(
                ![
                    "-o",
                    "-MF",
                    "-fplugin=/plugin",
                    "-fplugin=/tool-plugin",
                    "/tool-output",
                    "-O3",
                    "-include",
                    "/must-not-be-read.h"
                ]
                .contains(&word.to_str().unwrap_or("")),
                "{args:?}"
            );
        }
    }
}

#[test]
fn linux_musl_platform() {
    let fake = Fake { os: Some("linux 6.6".to_owned()), ..Fake::default() }
        .file("/usr/bin/cc", b"musl-gcc")
        .file("/usr/bin/getconf", b"getconf")
        .file("/lib/ld-musl-x86_64.so.1", b"musl")
        .run("/usr/bin/cc", &["--version"], "musl-gcc 1.2.5\n")
        // No glibc on this machine: getconf does not answer.
        .run("/lib/ld-musl-x86_64.so.1", &["--version"], "musl libc (x86_64)\nVersion 1.2.5\n");
    let env = vars(&[("PATH", "/usr/bin:/bin")]);
    let platform = exec_platform(
        &Machine {
            cwd: Path::new("/"),
            triple: "x86_64-unknown-linux-musl",
            cpu: "x86-64",
            env: &env,
            linker: None,
        },
        &fake,
    )
    .unwrap();
    assert_eq!(platform.os, "linux 6.6");
    assert_eq!(platform.libc, Some(("musl".to_owned(), "1.2.5".to_owned())));
    // No `c++` and no `ar` on the machine: the fields are absent, not guessed.
    assert_eq!(platform.cxx, None);
    assert_eq!(platform.ar, None);
    // The complete native closure includes missing candidates and the musl loader.
    assert!(platform.native_root.is_some());
}

#[test]
fn sdkroot_names_the_sdk_and_its_version() {
    let mut fake = macos();
    // With `SDKROOT` set the version comes from the SDK's own settings, not from xcrun.
    fake.runs.remove(&run_key("/usr/bin/env", &xcrun(["--show-sdk-version"])));
    let env = vars(&[("PATH", "/usr/bin:/opt/homebrew/bin"), ("SDKROOT", SDK)]);
    let platform = exec_platform(&macos_machine(&env, None), &fake).unwrap();
    assert_eq!(platform.sdk, Some(("macosx".to_owned(), "26.0".to_owned(), "11.0".to_owned())));
}

#[test]
fn two_compilers_two_platforms_one_executable_one_platform() {
    let mut first = macos();
    let second_cc = "/opt/other/cc";
    first = first.file(second_cc, b"another compiler").run(
        second_cc,
        &["--version"],
        "clang version 20.0.0\n",
    );
    let env_cc = vars(&[("PATH", "/usr/bin:/opt/homebrew/bin"), ("CC", "cc")]);
    let env_other = vars(&[("PATH", "/usr/bin:/opt/homebrew/bin"), ("CC", second_cc)]);
    let one = exec_platform(&macos_machine(&env_cc, None), &first).unwrap();
    let two = exec_platform(&macos_machine(&env_other, None), &first).unwrap();
    assert_ne!(one.cc, two.cc);
    assert_ne!(digest_of(&one), digest_of(&two));
    // The linker is `cc` in both, so only the compiler's field differs.
    assert_eq!(one.linker, two.linker);

    // One executable at two paths is one compiler: the digest is of its bytes, and the name of
    // both paths is the same.
    let third_cc = "/opt/third/cc";
    let mut both = first.file(third_cc, b"another compiler");
    both = both.run(third_cc, &["--version"], "clang version 20.0.0\n");
    let env_third = vars(&[("PATH", "/usr/bin:/opt/homebrew/bin"), ("CC", third_cc)]);
    let three = exec_platform(&macos_machine(&env_third, None), &both).unwrap();
    assert_eq!(two.cc, three.cc);
    assert_eq!(digest_of(&two), digest_of(&three));
}

#[test]
fn tool_variables_precedence() {
    let mut fake = macos();
    for (path, bytes) in [
        ("/opt/triple-dash/cc", b"triple dash".as_slice()),
        ("/opt/triple-under/cc", b"triple under"),
        ("/opt/host/cc", b"host"),
        ("/opt/target/cc", b"target"),
        ("/opt/plain/cc", b"plain"),
    ] {
        fake = fake.file(path, bytes).run(path, &["--version"], "a compiler\n");
    }
    let mut all = vars(&[("PATH", "/usr/bin")]);
    all.extend(vars(&[
        ("CC", "/opt/plain/cc"),
        ("TARGET_CC", "/opt/target/cc"),
        ("HOST_CC", "/opt/host/cc"),
        ("CC_aarch64_apple_darwin", "/opt/triple-under/cc"),
        ("CC_aarch64-apple-darwin", "/opt/triple-dash/cc"),
    ]));
    let platform = exec_platform(&macos_machine(&all, None), &fake).unwrap();
    // `cc` tries `CC_<triple>` before `CC_<triple with _>`, so the dashed name wins.
    assert_eq!(platform.cc.as_ref().unwrap().2, digest(b"triple dash"));

    let without_dash = all
        .iter()
        .filter(|(name, _)| name.as_str() != "CC_aarch64-apple-darwin")
        .map(|(name, value)| (name.clone(), value.clone()))
        .collect::<BTreeMap<_, _>>();
    let platform = exec_platform(&macos_machine(&without_dash, None), &fake).unwrap();
    assert_eq!(platform.cc.as_ref().unwrap().2, digest(b"triple under"));

    let without_triples = vars(&[
        ("PATH", "/usr/bin"),
        ("CC", "/opt/plain/cc"),
        ("TARGET_CC", "/opt/target/cc"),
        ("HOST_CC", "/opt/host/cc"),
    ]);
    let platform = exec_platform(&macos_machine(&without_triples, None), &fake).unwrap();
    assert_eq!(platform.cc.as_ref().unwrap().2, digest(b"host"));

    let without_host =
        vars(&[("PATH", "/usr/bin"), ("CC", "/opt/plain/cc"), ("TARGET_CC", "/opt/target/cc")]);
    let platform = exec_platform(&macos_machine(&without_host, None), &fake).unwrap();
    assert_eq!(platform.cc.as_ref().unwrap().2, digest(b"plain"));

    let plain = vars(&[("PATH", "/usr/bin"), ("CC", "/opt/plain/cc")]);
    let platform = exec_platform(&macos_machine(&plain, None), &fake).unwrap();
    assert_eq!(platform.cc.as_ref().unwrap().2, digest(b"plain"));
    // `cc` is the linker, and `CC` does not name it.
    assert_eq!(platform.linker.2, digest(CLANG));
}

#[test]
fn unused_target_cc_cannot_hide_changes_to_the_native_compiler() {
    let env =
        vars(&[("PATH", "/usr/bin"), ("CC", "/opt/actual/cc"), ("TARGET_CC", "/opt/unused/cc")]);
    let first = macos().file("/opt/actual/cc", b"actual-v1").file("/opt/unused/cc", b"unused");
    let second = macos().file("/opt/actual/cc", b"actual-v2").file("/opt/unused/cc", b"unused");
    let a = exec_platform(&macos_machine(&env, None), &first).unwrap();
    let b = exec_platform(&macos_machine(&env, None), &second).unwrap();
    assert_eq!(a.cc.as_ref().unwrap().2, digest(b"actual-v1"));
    assert_eq!(b.cc.as_ref().unwrap().2, digest(b"actual-v2"));
    assert_ne!(digest_of(&a), digest_of(&b));
    for var in ["CC", "CXX", "AR"] {
        let mut empty = env.clone();
        empty.insert(format!("{var}_{TRIPLE}"), std::ffi::OsString::new());
        let platform = exec_platform(&macos_machine(&empty, None), &first).unwrap();
        let tool = match var {
            "CC" => platform.cc,
            "CXX" => platform.cxx,
            _ => platform.ar,
        };
        let baseline = exec_platform(&macos_machine(&macos_env(), None), &first).unwrap();
        let default_tool = match var {
            "CC" => baseline.cc,
            "CXX" => baseline.cxx,
            _ => baseline.ar,
        };
        assert_eq!(
            tool, default_tool,
            "empty selected {var} uses default, not lower-priority variables"
        );
    }
}

#[test]
fn a_launcher_changes_the_digest() {
    let fake = macos().file("/opt/ccache/ccache", b"the launcher").run(
        "/opt/ccache/ccache",
        &["--version"],
        "ccache version 4.10\n",
    );
    let plain = vars(&[("PATH", "/usr/bin:/opt/ccache"), ("CC", "cc")]);
    let wrapped = vars(&[("PATH", "/usr/bin:/opt/ccache"), ("CC", "ccache cc")]);
    let one = exec_platform(&macos_machine(&plain, None), &fake).unwrap();
    let two = exec_platform(&macos_machine(&wrapped, None), &fake).unwrap();
    // The compiler is still `cc`, and the launcher's bytes are part of its digest.
    assert_eq!(one.cc.as_ref().unwrap().0, "cc");
    assert_eq!(two.cc.as_ref().unwrap().0, "cc");
    assert_ne!(one.cc, two.cc);
    assert_ne!(digest_of(&one), digest_of(&two));
    // The linker is `cc` in both: a launcher in `CC` does not wrap the link.
    assert_eq!(one.linker, two.linker);

    // A launcher that is not there cannot run the compiler, so the tool is absent.
    let absent = vars(&[("PATH", "/usr/bin"), ("CC", "sccache cc")]);
    let platform = exec_platform(&macos_machine(&absent, None), &fake).unwrap();
    assert_eq!(platform.cc, None);
}

#[test]
fn absent_tools_are_absent_and_an_absent_linker_is_refused() {
    let fake = macos();
    let no_tools = vars(&[("PATH", "/opt/empty")]);
    let refused = exec_platform(&macos_machine(&no_tools, None), &fake).unwrap_err();
    assert_eq!(refused.what, "cc");
    assert!(refused.why.contains("link"));

    let named = exec_platform(&macos_machine(&no_tools, Some("/opt/ld64")), &fake).unwrap_err();
    assert_eq!(named.what, "/opt/ld64");

    // With a linker and no C compiler, the platform is still a platform.
    let fake = fake.file("/opt/ld64", b"ld64").run("/opt/ld64", &["--version"], "ld64-951.9\n");
    let platform = exec_platform(&macos_machine(&no_tools, Some("/opt/ld64")), &fake).unwrap();
    assert_eq!(platform.cc, None);
    assert_eq!(platform.cxx, None);
    assert_eq!(platform.ar, None);
    assert_eq!(platform.linker.0, "ld64");
    assert_eq!(platform.linker.1, "ld64-951.9");
}

#[test]
fn a_linux_machine_without_a_libc_is_refused() {
    let fake = Fake { os: Some("linux 6.8".to_owned()), ..Fake::default() }
        .file("/usr/bin/cc", b"gcc")
        .run("/usr/bin/cc", &["--version"], "cc 13\n");
    let env = vars(&[("PATH", "/usr/bin")]);
    let refused = exec_platform(
        &Machine {
            cwd: Path::new("/"),
            triple: "x86_64-unknown-linux-gnu",
            cpu: "x86-64",
            env: &env,
            linker: None,
        },
        &fake,
    )
    .unwrap_err();
    assert_eq!(refused.what, "libc");
}

#[test]
fn os_is_refused_when_it_cannot_be_named() {
    let fake = Fake::default();
    let refused = exec_platform(&macos_machine(&macos_env(), None), &fake).unwrap_err();
    assert_eq!(refused.what, "os");
}

#[test]
fn a_platform_is_a_pure_function_of_the_machine() {
    let fake = macos();
    let env = vars(&[("PATH", "/usr/bin:/opt/homebrew/bin"), ("MACOSX_DEPLOYMENT_TARGET", "14.0")]);
    let one = exec_platform(&macos_machine(&env, None), &fake).unwrap();
    let two = exec_platform(&macos_machine(&env, None), &fake).unwrap();
    assert_eq!(one, two);
    assert_eq!(digest_of(&one), digest_of(&two));

    // The deployment target names what the SDK is used for, so it changes the platform.
    let other =
        vars(&[("PATH", "/usr/bin:/opt/homebrew/bin"), ("MACOSX_DEPLOYMENT_TARGET", "15.0")]);
    let three = exec_platform(&macos_machine(&other, None), &fake).unwrap();
    assert_ne!(one.sdk, three.sdk);
    assert_ne!(digest_of(&one), digest_of(&three));

    // A variable no field is read from does not.
    let mut home = env.clone();
    home.insert("HOME".to_owned(), "/Users/someone-else".into());
    home.insert("SHELL".to_owned(), "/bin/zsh".into());
    let four = exec_platform(&macos_machine(&home, None), &fake).unwrap();
    assert_eq!(one, four);
    assert_eq!(digest_of(&one), digest_of(&four));
}

#[test]
#[cfg(any(target_os = "macos", target_os = "linux"))]
#[allow(clippy::print_stderr)]
fn the_system_probe_reads_this_machine() {
    let env: BTreeMap<String, std::ffi::OsString> = std::env::vars_os()
        .filter_map(|(name, value)| Some((name.into_string().ok()?, value)))
        .collect();
    let triple = if cfg!(target_os = "macos") {
        format!("{}-apple-darwin", std::env::consts::ARCH)
    } else {
        let abi = if cfg!(target_env = "musl") { "musl" } else { "gnu" };
        format!("{}-unknown-linux-{abi}", std::env::consts::ARCH)
    };
    let cwd = std::env::current_dir().unwrap();
    let machine = Machine {
        cwd: &cwd,
        triple: &triple,
        cpu: std::env::consts::ARCH,
        env: &env,
        linker: None,
    };
    let platform: ExecPlatform = exec_platform(&machine, &System).unwrap();
    eprintln!("{platform:#?}");
    assert_eq!(platform.triple, triple);
    assert_eq!(platform.os.split(' ').next(), Some(std::env::consts::OS));
    if cfg!(target_os = "macos") {
        assert!(platform.sdk.is_some(), "this machine has an SDK");
        assert_eq!(platform.libc, None);
    } else {
        assert_eq!(platform.sdk, None);
        assert!(platform.libc.is_some(), "this machine has a libc");
    }
    assert!(platform.cc.is_some(), "this machine has a C compiler");
    assert!(platform.cxx.is_some(), "this machine has a C++ compiler");
    assert!(platform.ar.is_some(), "this machine has an archiver");
    assert!(platform.native_root.is_some(), "this machine's native libraries are readable");
    // `/usr/bin`'s shim is resolved to the developer directory's clang, whose bytes name the
    // compiler; the shim's own bytes are one stub for every tool there and name nothing.
    let (_, version, _) = platform.cc.as_ref().unwrap();
    assert!(!version.is_empty());
    if cfg!(target_os = "macos") {
        // Probe the shim directly: CC may select a different executable or launcher.
        let shim_env = vars(&[("PATH", "/usr/bin:/bin"), ("CC", "/usr/bin/cc")]);
        let shim_machine = Machine { env: &shim_env, ..machine };
        let resolved = exec_platform(&shim_machine, &System).unwrap();
        let shim = std::fs::read("/usr/bin/cc").expect("this machine has /usr/bin/cc");
        assert_ne!(resolved.cc.as_ref().unwrap().2, digest(&shim));
    }
    // Two calls read one platform: the digests are stable.
    let again = exec_platform(&machine, &System).unwrap();
    assert_eq!(platform, again);
    assert_eq!(digest_of(&platform), digest_of(&again));
}
