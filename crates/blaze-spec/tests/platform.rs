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
#[derive(Default)]
struct Fake {
    os: Option<String>,
    /// The files the probe reads, by absolute path.
    files: BTreeMap<String, Vec<u8>>,
    /// What a run prints, by the program and its arguments; a run that is absent failed.
    runs: BTreeMap<String, String>,
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
}

fn run_key(program: &str, args: &[&str]) -> String {
    format!("{program}\0{}", args.join("\0"))
}

impl Probe for Fake {
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

/// The digest `native_root` is documented to fold, computed here from the specification and not
/// by the code under test.
fn root(files: &[(&str, Option<&[u8]>)]) -> Digest {
    let mut hasher = Sha256::new();
    for (name, bytes) in files {
        hasher.update((name.len() as u64).to_le_bytes());
        hasher.update(name.as_bytes());
        match bytes {
            Some(bytes) => {
                hasher.update((bytes.len() as u64).to_le_bytes());
                hasher.update(bytes);
            }
            None => hasher.update(u64::MAX.to_le_bytes()),
        }
    }
    hasher.finalize().into()
}

/// The SHA-256 of `bytes`, as a platform's digest.
fn digest(bytes: &[u8]) -> Digest {
    Sha256::digest(bytes).into()
}

fn vars(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs.iter().map(|(name, value)| ((*name).to_owned(), (*value).to_owned())).collect()
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

fn macos_machine<'a>(env: &'a BTreeMap<String, String>, linker: Option<&'a str>) -> Machine<'a> {
    Machine { triple: TRIPLE, cpu: CPU, env, linker }
}

fn macos_env() -> BTreeMap<String, String> {
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
    assert_eq!(
        platform.native_root,
        Some(root(&[
            ("SDKSettings.json", Some(br#"{"Version": "26.0", "DisplayName": "macOS 26.0"}"#)),
            ("usr/lib/libSystem.tbd", Some(b"the names libSystem exports")),
        ]))
    );
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
        &Machine { triple: "x86_64-unknown-linux-gnu", cpu: "x86-64", env: &env, linker: None },
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
    assert_eq!(
        platform.native_root,
        Some(root(&[
            ("/lib/x86_64-linux-gnu/libc.so.6", Some(b"glibc 2.39")),
            ("/lib64/ld-linux-x86-64.so.2", Some(b"the loader")),
        ]))
    );
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
        &Machine { triple: "x86_64-unknown-linux-musl", cpu: "x86-64", env: &env, linker: None },
        &fake,
    )
    .unwrap();
    assert_eq!(platform.os, "linux 6.6");
    assert_eq!(platform.libc, Some(("musl".to_owned(), "1.2.5".to_owned())));
    // No `c++` and no `ar` on the machine: the fields are absent, not guessed.
    assert_eq!(platform.cxx, None);
    assert_eq!(platform.ar, None);
    // musl's loader is its libc, so one file is folded twice and deduplicated to one.
    assert_eq!(platform.native_root, Some(root(&[("/lib/ld-musl-x86_64.so.1", Some(b"musl"))])));
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
    assert_eq!(platform.cc.as_ref().unwrap().2, digest(b"target"));

    let plain = vars(&[("PATH", "/usr/bin"), ("CC", "/opt/plain/cc")]);
    let platform = exec_platform(&macos_machine(&plain, None), &fake).unwrap();
    assert_eq!(platform.cc.as_ref().unwrap().2, digest(b"plain"));
    // `cc` is the linker, and `CC` does not name it.
    assert_eq!(platform.linker.2, digest(CLANG));
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
        &Machine { triple: "x86_64-unknown-linux-gnu", cpu: "x86-64", env: &env, linker: None },
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
    home.insert("HOME".to_owned(), "/Users/someone-else".to_owned());
    home.insert("SHELL".to_owned(), "/bin/zsh".to_owned());
    let four = exec_platform(&macos_machine(&home, None), &fake).unwrap();
    assert_eq!(one, four);
    assert_eq!(digest_of(&one), digest_of(&four));
}

#[test]
#[cfg(any(target_os = "macos", target_os = "linux"))]
#[allow(clippy::print_stderr)]
fn the_system_probe_reads_this_machine() {
    let env: BTreeMap<String, String> = std::env::vars().collect();
    let triple = if cfg!(target_os = "macos") {
        format!("{}-apple-darwin", std::env::consts::ARCH)
    } else {
        let abi = if cfg!(target_env = "musl") { "musl" } else { "gnu" };
        format!("{}-unknown-linux-{abi}", std::env::consts::ARCH)
    };
    let machine = Machine { triple: &triple, cpu: std::env::consts::ARCH, env: &env, linker: None };
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
