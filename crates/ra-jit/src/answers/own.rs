//! ra-jit's own answers to rustc's questions, from what it knows of a release of rustc and of
//! the targets that release has: no rustc runs. The data of each release is what that rustc
//! printed for every target ra-jit has (`harness/answers.py --write`), and the cfgs that follow
//! from the arguments are added here, as rustc adds them. A question this does not understand
//! whole is answered `None`, and the caller reports a named gap without running rustc.

use std::{
    collections::BTreeMap,
    ffi::OsString,
    path::{Path, PathBuf},
    sync::OnceLock,
};

use serde_derive::Deserialize;

/// What each release of rustc ra-jit has the answers of says.
const RELEASES: [&str; 1] = [include_str!("rustc-1.99.0.json")];

#[derive(Deserialize)]
struct Release {
    /// As the first line of `-V` has it, after `rustc `.
    version: String,
    /// What `-vV` prints, with `{host}` for the host's tuple.
    verbose: String,
    targets: BTreeMap<String, Target>,
}

#[derive(Deserialize)]
struct Target {
    /// The cfgs of the target, but those the arguments decide.
    cfg: Vec<String>,
    /// The cfgs printed besides those when unstable features are allowed.
    nightly_cfg: Vec<String>,
    /// The strategy of `-C panic` when none is given.
    panic: String,
    split_debuginfo: Vec<String>,
    /// The name of the file of each kind of crate named `___`; `None` for a kind the target
    /// does not have.
    file_names: BTreeMap<String, Option<String>>,
    /// By the host rustc runs on, whose build decides some of it.
    target_spec_json: BTreeMap<String, String>,
}

fn releases() -> &'static [Release] {
    static RELEASES_READ: OnceLock<Vec<Release>> = OnceLock::new();
    RELEASES_READ.get_or_init(|| {
        RELEASES
            .iter()
            .map(|it| serde_json::from_str(it).expect("the answers of a release"))
            .collect()
    })
}

/// The toolchain `rustc` is of, as far as ra-jit can tell without running it.
struct Toolchain {
    sysroot: PathBuf,
    host: String,
    release: &'static Release,
}

impl Toolchain {
    /// From rustup's records in the sysroot: the release of rustc and the host it runs on.
    /// rustc finds its sysroot by where its library is, which is beside it.
    fn of(rustc: &Path) -> Option<Toolchain> {
        let sysroot = rustc.parent()?.parent()?.to_owned();
        let lib = sysroot.join("lib");
        let has_driver = std::fs::read_dir(&lib).ok()?.flatten().any(|it| {
            it.file_name().to_str().is_some_and(|name| name.starts_with("librustc_driver-"))
        });
        if !has_driver {
            return None;
        }
        let rustlib = lib.join("rustlib");
        let entries =
            std::fs::read_dir(&rustlib).ok()?.collect::<std::io::Result<Vec<_>>>().ok()?;
        let mut hosts: Vec<_> = entries
            .iter()
            .filter_map(|it| {
                it.file_name().to_str()?.strip_prefix("manifest-rustc-").map(str::to_owned)
            })
            .collect();
        if hosts.is_empty() {
            // Component-composed installations can omit rustup's channel files.
            // rust-lld belongs to the compiler's host component, not target std.
            hosts = entries
                .iter()
                .filter(|it| {
                    it.path().join("bin/rust-lld").is_file()
                        || it.path().join("bin/rust-lld.exe").is_file()
                })
                .filter_map(|it| it.file_name().into_string().ok())
                .collect();
        }
        let [host] = hosts.as_slice() else { return None };
        let host = host.clone();
        let release = match std::fs::read_to_string(rustlib.join("multirust-channel-manifest.toml"))
        {
            Ok(manifest) => {
                let (_, rest) = manifest.split_once("\n[pkg.rustc]\nversion = \"")?;
                let (version, _) = rest.split_once('"')?;
                releases().iter().find(|it| it.version == version)?
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let info =
                    std::fs::read_to_string(sysroot.join("share/doc/rust/html/version_info.html"))
                        .ok()?;
                releases().iter().find(|it| {
                    let Some((version, revision)) = it.version.split_once(" (") else {
                        return false;
                    };
                    let Some((commit, _)) = revision.split_once(' ') else { return false };
                    info.contains(&format!("Rust</a> {version}</span>"))
                        && info.contains(&format!(">{commit}</a>"))
                })?
            }
            Err(_) => return None,
        };
        Some(Toolchain { sysroot, host, release })
    }
}

/// How cfgs follow from `-C` and `--cfg`.
#[derive(Default)]
struct Given<'a> {
    target: Option<&'a str>,
    sysroot: Option<&'a str>,
    opt_level: Option<&'a str>,
    debug_assertions: Option<bool>,
    overflow_checks: Option<bool>,
    panic: Option<&'a str>,
    cfgs: Vec<&'a str>,
    crate_name: Option<&'a str>,
    crate_types: Vec<&'a str>,
    prints: Vec<&'a str>,
    unstable_options: bool,
    from_stdin: bool,
    extra_filename: bool,
    target_features: Vec<&'a str>,
}

/// The flags that take a value and change no answer.
const IGNORED_VALUED: [&str; 12] = [
    "--edition",
    "--error-format",
    "--json",
    "--emit",
    "--out-dir",
    "-L",
    "--extern",
    "--check-cfg",
    "--cap-lints",
    "--color",
    "--diagnostic-width",
    "--remap-path-prefix",
];

/// The lint flags, whose value changes no answer.
const LINTS: [&str; 10] = [
    "-W",
    "-A",
    "-D",
    "-F",
    "--warn",
    "--allow",
    "--deny",
    "--forbid",
    "--force-warn",
    "--cap-lints",
];

/// The codegen options that change no answer.
const IGNORED_CODEGEN: [&str; 16] = [
    "embed-bitcode",
    "debuginfo",
    "metadata",
    "incremental",
    "strip",
    "codegen-units",
    "prefer-dynamic",
    "lto",
    "linker",
    "link-arg",
    "link-args",
    "split-debuginfo",
    "link-dead-code",
    "rpath",
    "force-frame-pointers",
    "force-unwind-tables",
];

fn yes(value: Option<&str>) -> Option<bool> {
    match value {
        None | Some("y" | "yes" | "on" | "true") => Some(true),
        Some("n" | "no" | "off" | "false") => Some(false),
        Some(_) => None,
    }
}

impl<'a> Given<'a> {
    fn of(args: &'a [&'a str]) -> Option<Given<'a>> {
        let flag_sensitive = args.iter().enumerate().any(|(at, word)| {
            word.strip_prefix("--print=")
                .or_else(|| (*word == "--print").then(|| args.get(at + 1).copied()).flatten())
                .is_some_and(|kind| matches!(kind, "cfg" | "check-cfg"))
        });
        let mut given = Given::default();
        let mut words = args.iter().copied();
        while let Some(word) = words.next() {
            let (flag, joined) = match word.split_once('=') {
                Some((flag, value)) if flag.starts_with("--") => (flag, Some(value)),
                _ => match word.as_bytes() {
                    // `-Cname=value`, `-Zname`, `-Wlint`, `-Ldir`.
                    [b'-', b'C' | b'Z' | b'W' | b'A' | b'D' | b'F' | b'L', _, ..] => {
                        (&word[..2], Some(&word[2..]))
                    }
                    _ => (word, None),
                },
            };
            let mut value = || joined.or_else(|| words.next());
            match flag {
                "-" => given.from_stdin = true,
                "--print" => given.prints.push(value()?),
                "--target" => given.target = Some(value()?),
                "--sysroot" => given.sysroot = Some(value()?),
                "--crate-name" => given.crate_name = Some(value()?),
                "--crate-type" => given.crate_types.extend(value()?.split(',')),
                "--cfg" => given.cfgs.push(value()?),
                "-O" => given.opt_level = Some("2"),
                "-g" => {}
                "-C" | "--codegen" => {
                    let option = value()?;
                    let (name, value) = match option.split_once('=') {
                        Some((name, value)) => (name, Some(value)),
                        None => (option, None),
                    };
                    match name.replace('_', "-").as_str() {
                        "opt-level" => given.opt_level = Some(value?),
                        "debug-assertions" => given.debug_assertions = Some(yes(value)?),
                        "overflow-checks" => given.overflow_checks = Some(yes(value)?),
                        "panic" => given.panic = Some(value?),
                        "target-feature" => given.target_features.push(value?),
                        "extra-filename" => given.extra_filename = true,
                        name if IGNORED_CODEGEN.contains(&name) => {}
                        // CPU, relocation and instrumentation do not affect a path or
                        // target-spec print. Do not pretend their cfg effects are known.
                        _ if !flag_sensitive => {}
                        _ => return None,
                    }
                }
                "-Z" => match value()? {
                    "unstable-options" => given.unstable_options = true,
                    _ if !flag_sensitive => {}
                    _ => return None,
                },
                _ if LINTS.contains(&flag) || IGNORED_VALUED.contains(&flag) => {
                    value()?;
                }
                // An input rustc does not read for what it is asked.
                _ if !flag.starts_with('-') => {}
                _ => return None,
            }
        }
        Some(given)
    }

    fn debug_assertions(&self) -> bool {
        self.debug_assertions.unwrap_or(matches!(self.opt_level, None | Some("0")))
    }
}

/// The built-in cfgs of `target`, without running a compiler. Unlike a general query,
/// this asks for neither version output nor a target specification, so an SDK or version
/// override does not invalidate the answer. Build dependencies need the host's cfgs even
/// when their workspace was loaded for another target.
pub(crate) fn target_cfg(
    sysroot: &Path,
    target: &str,
    env: &BTreeMap<String, String>,
) -> Option<Vec<String>> {
    let nightly = match env.get("RUSTC_BOOTSTRAP").map(String::as_str) {
        None => false,
        Some("1") => true,
        Some(_) => return None,
    };
    let toolchain = Toolchain::of(&sysroot.join("bin/rustc"))?;
    let target = toolchain.release.targets.get(target)?;
    let given = Given { opt_level: Some("2"), ..Given::default() };
    cfgs(target, &given, nightly)
}

pub(crate) fn host_tuple(rustc: &Path) -> Option<String> {
    Some(Toolchain::of(rustc)?.host)
}

/// The target facts needed to write an LLVM module. An empty module still needs
/// the correct data layout and deployment-version triple, even though it has no
/// machine instructions or referenced symbols.
pub(super) struct LlvmTarget {
    pub triple: String,
    pub data_layout: String,
    pub has_std: bool,
}

pub(super) fn llvm_target(
    sysroot: &Path,
    tuple: &str,
    var: &dyn Fn(&str) -> Option<OsString>,
) -> Option<LlvmTarget> {
    let toolchain = Toolchain::of(&sysroot.join("bin/rustc"))?;
    let target = toolchain.release.targets.get(tuple)?;
    let spec: serde_json::Value =
        serde_json::from_str(target.target_spec_json.get(&toolchain.host)?).ok()?;
    let mut triple = spec["llvm-target"].as_str()?.to_owned();
    if tuple.ends_with("-apple-darwin") {
        // The recorded target spec deliberately has no deployment version. The
        // release's macOS minimum is 10.12 on x86_64 and 11.0 on arm64, and rustc
        // clamps a lower requested deployment version to that target minimum.
        let minimum = match spec["arch"].as_str()? {
            "x86_64" => [10, 12, 0],
            "aarch64" => [11, 0, 0],
            _ => return None,
        };
        let version = match var("MACOSX_DEPLOYMENT_TARGET") {
            None => minimum,
            Some(value) => macos_version(value.to_str()?)?.max(minimum),
        };
        triple += &format!("{}.{}.{}", version[0], version[1], version[2]);
    }
    Some(LlvmTarget {
        triple,
        data_layout: spec["data-layout"].as_str()?.to_owned(),
        has_std: spec["metadata"]["std"].as_bool()?,
    })
}

fn macos_version(value: &str) -> Option<[u16; 3]> {
    let mut version = [0; 3];
    for (index, part) in value.split('.').enumerate() {
        *version.get_mut(index)? = part.parse().ok()?;
    }
    Some(version)
}

/// The release of `rustc` as the first line of its `-V` has it, `1.99.0`, from the
/// toolchain's records, not from executing it.
pub(super) fn release(rustc: &Path) -> Option<String> {
    Some(Toolchain::of(rustc)?.release.version.split_whitespace().next()?.to_owned())
}

/// Cargo's release version from the installed channel manifest, not from executing it.
pub(super) fn cargo_version(rustc: &Path) -> Option<Vec<u8>> {
    let toolchain = Toolchain::of(rustc)?;
    let manifest = std::fs::read_to_string(
        toolchain.sysroot.join("lib/rustlib/multirust-channel-manifest.toml"),
    )
    .ok()?;
    let (_, rest) = manifest.split_once("\n[pkg.cargo]\nversion = \"")?;
    let (version, _) = rest.split_once('"')?;
    // Cargo's package version (0.100.0) differs from its release version (1.99.0).
    // The channel's rustc release supplies that prefix; Cargo supplies its own commit/date.
    let release = toolchain.release.version.split_whitespace().next()?;
    let (_, commit) = version.split_once(' ')?;
    Some(format!("cargo {release} {commit}\n").into_bytes())
}

fn cfgs(target: &Target, given: &Given<'_>, nightly: bool) -> Option<Vec<String>> {
    let mut cfgs = target.cfg.clone();
    if !given.target_features.is_empty() {
        let arch = cfgs.iter().find_map(|cfg| cfg.strip_prefix("target_arch=\""))?;
        let arch = arch.trim_end_matches('"').to_owned();
        let initial = cfgs
            .iter()
            .filter_map(|cfg| cfg.strip_prefix("target_feature=\""))
            .map(|name| name.trim_end_matches('"').to_owned())
            .collect::<Vec<_>>();
        let flags = given
            .target_features
            .iter()
            .map(|flags| format!("-Ctarget-feature={flags}"))
            .collect::<Vec<_>>();
        let features = hir_ty::mir::codegen_options::TargetMachine::from_flags(&flags)
            .effective_features(&arch, initial);
        cfgs.retain(|cfg| !cfg.starts_with("target_feature="));
        cfgs.extend(features.into_iter().map(|feature| format!("target_feature={feature:?}")));
    }
    if nightly {
        cfgs.extend(target.nightly_cfg.iter().cloned());
    }
    let debug_assertions = given.debug_assertions();
    if debug_assertions {
        cfgs.push("debug_assertions".to_owned());
    }
    if nightly && given.overflow_checks.unwrap_or(debug_assertions) {
        cfgs.push("overflow_checks".to_owned());
    }
    if nightly && debug_assertions {
        cfgs.push("ub_checks".to_owned());
    }
    let panic = given.panic.unwrap_or(&target.panic);
    if panic != target.panic && panic != "abort" {
        return None;
    }
    cfgs.push(format!("panic=\"{panic}\""));
    if given.crate_types.contains(&"proc-macro") {
        cfgs.push("proc_macro".to_owned());
    }
    for cfg in &given.cfgs {
        if !is_plain_cfg(cfg) {
            return None;
        }
        cfgs.push((*cfg).to_owned());
    }
    cfgs.sort();
    cfgs.dedup();
    Some(cfgs)
}

/// Overrides not yet modelled by the owned query answers. SDKROOT and a valid
/// MACOSX_DEPLOYMENT_TARGET do not alter release, cfg, filename or spec prints.
/// Deployment-sensitive code generation and `--print deployment-target` stay
/// separately modelled operations, never compiler-query fallbacks.
const UNSET: [&str; 6] = [
    "RUSTC_OVERRIDE_VERSION_STRING",
    "RUST_TARGET_PATH",
    "IPHONEOS_DEPLOYMENT_TARGET",
    "TVOS_DEPLOYMENT_TARGET",
    "WATCHOS_DEPLOYMENT_TARGET",
    "XROS_DEPLOYMENT_TARGET",
];

/// rustc validates a Darwin deployment version when constructing the target, but
/// version-only requests do not construct a target. Never echo the variable's value.
pub(super) fn deployment_error(
    rustc: &Path,
    args: &[OsString],
    var: &dyn Fn(&str) -> Option<OsString>,
) -> Option<String> {
    let args = args.iter().map(|it| it.to_str()).collect::<Option<Vec<_>>>()?;
    let given = Given::of(&args)?;
    if given.prints.is_empty() {
        return None;
    }
    let toolchain = Toolchain::of(rustc)?;
    if !given.target.unwrap_or(&toolchain.host).ends_with("-apple-darwin") {
        return None;
    }
    let value = var("MACOSX_DEPLOYMENT_TARGET")?;
    let value = value.to_str()?;
    for part in value.splitn(3, '.') {
        if let Err(error) = part.parse::<u16>() {
            return Some(format!(
                "error: failed to parse deployment target specified in MACOSX_DEPLOYMENT_TARGET: {error}\n\n\
                 error: aborting due to 1 previous error\n\n"
            ));
        }
    }
    None
}

/// The cfgs rustc sets itself, which it refuses as `--cfg`, by name or by what it starts with.
const BUILTIN_CFGS: [&str; 13] = [
    "debug_assertions",
    "overflow_checks",
    "ub_checks",
    "contract_checks",
    "fmt_debug",
    "relocation_model",
    "panic",
    "unix",
    "windows",
    "test",
    "emscripten_wasm_eh",
    "sanitize",
    "sanitizer_",
];

/// Whether `cfg` is `name` or `name="value"` as rustc prints it back: a plain name of no
/// builtin cfg, a value with nothing to escape.
fn is_plain_cfg(cfg: &str) -> bool {
    let (name, value) = match cfg.split_once('=') {
        Some((name, value)) => (name, Some(value)),
        None => (cfg, None),
    };
    let plain_name = name.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_')
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_');
    let builtin = name.starts_with("target_")
        || BUILTIN_CFGS.iter().any(|it| name == *it || (it.ends_with('_') && name.starts_with(it)));
    let plain_value = value.is_none_or(|value| {
        let inner = value.strip_prefix('"').and_then(|it| it.strip_suffix('"'));
        inner.is_some_and(|it| {
            it.chars().all(|c| c.is_ascii_graphic() && c != '"' && c != '\\' || c == ' ')
        })
    });
    plain_name && !builtin && plain_value
}

/// What `rustc` (the toolchain's executable) prints for `args` with `stdin`, with the
/// variables `var` reads; `None` for what this does not answer.
pub(super) fn answer(
    rustc: &Path,
    args: &[OsString],
    stdin: &[u8],
    var: &dyn Fn(&str) -> Option<OsString>,
) -> Option<Vec<u8>> {
    if UNSET.iter().any(|it| var(it).is_some()) {
        return None;
    }
    if deployment_error(rustc, args, var).is_some() {
        return None;
    }
    let nightly = match var("RUSTC_BOOTSTRAP") {
        None => false,
        Some(it) if it == "1" => true,
        Some(_) => return None,
    };
    let args = args.iter().map(|it| it.to_str()).collect::<Option<Vec<_>>>()?;
    // A driver of rustc's (clippy's) may answer otherwise.
    if rustc.file_stem()? != "rustc" {
        return None;
    }
    let toolchain = Toolchain::of(rustc)?;
    let release = toolchain.release;
    if !args.is_empty()
        && args.iter().all(|it| matches!(*it, "-V" | "-v" | "-vV" | "--version" | "--verbose"))
    {
        let verbose = args.iter().any(|it| matches!(*it, "-v" | "-vV" | "--verbose"));
        let text = if verbose {
            release.verbose.replace("{host}", &toolchain.host)
        } else {
            format!("rustc {}\n", release.version)
        };
        return Some(text.into_bytes());
    }
    let given = Given::of(&args)?;
    if given.prints.is_empty() || (given.from_stdin && !stdin.trim_ascii().is_empty()) {
        return None;
    }
    let tuple = given.target.unwrap_or(&toolchain.host);
    let target = release.targets.get(tuple)?;
    let sysroot = given.sysroot.map(PathBuf::from).unwrap_or_else(|| toolchain.sysroot.clone());
    let mut out = String::new();
    for print in &given.prints {
        match *print {
            "sysroot" => out += &format!("{}\n", sysroot.to_str()?),
            "target-libdir" => {
                let dir = sysroot.join("lib/rustlib").join(tuple).join("lib");
                out += &format!("{}\n", dir.to_str()?);
            }
            "host-tuple" => out += &format!("{}\n", toolchain.host),
            "split-debuginfo" => {
                target.split_debuginfo.iter().for_each(|it| out += &format!("{it}\n"))
            }
            "crate-name" => out += &format!("{}\n", given.crate_name.unwrap_or("rust_out")),
            "file-names" => {
                if given.extra_filename || !given.from_stdin {
                    return None;
                }
                let name = given.crate_name.unwrap_or("rust_out");
                let mut kinds: Vec<&str> = Vec::new();
                for kind in
                    if given.crate_types.is_empty() { &["bin"][..] } else { &given.crate_types }
                {
                    if !kinds.contains(kind) {
                        kinds.push(kind);
                    }
                }
                // `lib` is an `rlib`, which rustc may not name twice.
                if kinds.contains(&"lib") && kinds.contains(&"rlib") {
                    return None;
                }
                for kind in kinds {
                    let file = target.file_names.get(kind)?.as_deref()?;
                    out += &format!("{}\n", file.replace("___", name));
                }
            }
            "cfg" => {
                let cfgs = cfgs(target, &given, nightly)?;
                cfgs.iter().for_each(|it| out += &format!("{it}\n"));
            }
            "target-spec-json" if given.unstable_options && nightly => {
                out += target.target_spec_json.get(&toolchain.host)?;
            }
            _ => return None,
        }
    }
    Some(out.into_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn llvm_targets_use_owned_layouts_and_macos_deployment_versions() {
        let dir = tempfile::tempdir().unwrap();
        crate::answers::tests::synthetic_toolchain(dir.path());
        for (tuple, base, default) in [
            ("x86_64-apple-darwin", "x86_64-apple-macosx", "10.12.0"),
            ("aarch64-apple-darwin", "arm64-apple-macosx", "11.0.0"),
        ] {
            let target = llvm_target(dir.path(), tuple, &|_| None).unwrap();
            assert_eq!(target.triple, format!("{base}{default}"));
            assert!(target.has_std);
            assert!(target.data_layout.starts_with("e-m:o-"));
            for (value, expected) in [("10.7", default), ("14", "14.0.0"), ("14.2.1", "14.2.1")] {
                let var = |name: &str| (name == "MACOSX_DEPLOYMENT_TARGET").then(|| value.into());
                let target = llvm_target(dir.path(), tuple, &var).unwrap();
                assert_eq!(target.triple, format!("{base}{expected}"));
            }
            for value in ["", "bad", "14.2.3.4", "65536", "14."] {
                let var = |name: &str| (name == "MACOSX_DEPLOYMENT_TARGET").then(|| value.into());
                assert!(llvm_target(dir.path(), tuple, &var).is_none(), "{value}");
            }
        }
        let target = llvm_target(dir.path(), "x86_64-unknown-linux-gnu", &|_| None).unwrap();
        assert_eq!(target.triple, "x86_64-unknown-linux-gnu");
        assert!(target.data_layout.starts_with("e-m:e-"));
        assert!(llvm_target(dir.path(), "missing-target", &|_| None).is_none());
    }

    #[test]
    fn cfgs_given() {
        assert!(is_plain_cfg("foo"));
        assert!(is_plain_cfg("feature=\"serde derive\""));
        assert!(!is_plain_cfg("feature=serde"));
        assert!(!is_plain_cfg("feature=\"a\\\"b\""));
        assert!(!is_plain_cfg("debug_assertions"));
        assert!(!is_plain_cfg("target_os=\"linux\""));
        assert!(!is_plain_cfg("sanitizer_cfi_generalize_pointers"));
        assert!(!is_plain_cfg("9lives"));
    }

    #[test]
    fn arguments_read() {
        let args =
            ["-Copt-level=3", "-C", "panic=abort", "--print=cfg", "--cfg", "foo", "-Wwarnings"];
        let given = Given::of(&args).unwrap();
        assert_eq!(given.opt_level, Some("3"));
        assert_eq!(given.panic, Some("abort"));
        assert_eq!(given.prints, ["cfg"]);
        assert_eq!(given.cfgs, ["foo"]);
        assert!(!given.debug_assertions());
        assert!(Given::of(&["-O"]).is_some_and(|it| !it.debug_assertions()));
        assert!(Given::of(&["-C", "opt-level=0"]).is_some_and(|it| it.debug_assertions()));
        // What may change the target's features is rustc's to answer.
        assert!(Given::of(&["-C", "target-cpu=native", "--print", "cfg"]).is_none());
        assert!(Given::of(&["-Ctarget-feature=+avx2", "--print", "cfg"]).is_some());
        assert!(Given::of(&["-Z", "ub-checks", "--print", "cfg"]).is_none());
    }

    #[test]
    fn independent_prints_do_not_require_a_cpu_or_codegen_model() {
        for kind in ["sysroot", "target-libdir", "target-spec-json", "file-names"] {
            let args = ["-Ctarget-cpu=native", "-Crelocation-model=pic", "--print", kind];
            assert!(Given::of(&args).is_some(), "{kind}");
        }
        assert!(
            Given::of(&["-Ctarget-cpu=native", "--print", "target-libdir", "--print", "cfg"])
                .is_none(),
            "a combined cfg answer must not silently ignore CPU semantics"
        );
    }
}
