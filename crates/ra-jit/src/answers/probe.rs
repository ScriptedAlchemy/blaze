//! Build-script compiler probes with owned checking and artifact emission.
//!
//! Unknown flags and artifact consumers are not negative feature results. They
//! remain unsupported operations and must fail the surrounding producer build.
//! Empty library translation units have a complete syntax/attribute check and
//! real LLVM IR emission. Other source goes through the partial metadata checker;
//! apparent success there remains undecided until it owns every acceptance pass.

use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::OsString,
    io::{Read as _, Write as _},
    path::Path,
};

use anyhow::{Context as _, Result};

#[derive(Debug, PartialEq, Eq)]
enum Emission {
    Metadata,
    LlvmIr,
}

struct Probe {
    source: String,
    name: String,
    edition: syntax::Edition,
    cfg: Vec<String>,
    flags: Vec<String>,
    target: Option<String>,
    out: std::path::PathBuf,
    emission: Emission,
}

fn parse(args: &[OsString]) -> Result<Probe> {
    let words = args
        .iter()
        .map(|it| it.to_str().context("non-UTF-8 probe argument"))
        .collect::<Result<Vec<_>>>()?;
    let mut probe = Probe {
        source: String::new(),
        name: "rust_out".into(),
        edition: syntax::Edition::DEFAULT,
        cfg: Vec::new(),
        flags: Vec::new(),
        target: None,
        out: std::env::current_dir()?,
        emission: Emission::Metadata,
    };
    let mut it = words.into_iter();
    let mut seen = BTreeSet::new();
    let mut emission = None;
    while let Some(word) = it.next() {
        let (flag, joined) = word.split_once('=').map_or((word, None), |(a, b)| (a, Some(b)));
        let mut value = || joined.or_else(|| it.next()).context("probe flag has no value");
        if matches!(
            flag,
            "--crate-name"
                | "--edition"
                | "--target"
                | "--out-dir"
                | "--crate-type"
                | "--emit"
                | "--cap-lints"
        ) {
            anyhow::ensure!(seen.insert(flag), "repeated compiler probe option `{flag}`");
        }
        match flag {
            "--crate-name" => probe.name = value()?.into(),
            "--edition" => probe.edition = value()?.parse()?,
            "--cfg" => probe.cfg.push(value()?.into()),
            "--target" => probe.target = Some(value()?.into()),
            "--out-dir" => probe.out = std::path::absolute(value()?)?,
            "--crate-type" => {
                anyhow::ensure!(matches!(value()?, "lib" | "rlib"), "unsupported probe crate type")
            }
            "--emit" => {
                let emit = value()?;
                emission = Some(if emit == "llvm-ir" {
                    Emission::LlvmIr
                } else {
                    anyhow::ensure!(
                        emit.split(',').all(|it| matches!(it, "dep-info" | "metadata"))
                            && emit.split(',').any(|it| it == "metadata"),
                        "unsupported compiler probe emission request"
                    );
                    Emission::Metadata
                });
            }
            "--cap-lints" => anyhow::ensure!(value()? == "allow", "unsupported probe lint policy"),
            "-C" => probe.flags.push(format!("-C{}", value()?)),
            "-O" => probe.flags.push("-Copt-level=2".into()),
            _ if word.starts_with("-C") => probe.flags.push(word.into()),
            _ if !word.starts_with('-') || word == "-" => {
                anyhow::ensure!(probe.source.is_empty(), "multiple probe inputs");
                probe.source = word.into();
            }
            _ => anyhow::bail!("unsupported compiler probe argument `{word}`"),
        }
    }
    anyhow::ensure!(!probe.source.is_empty(), "the source probe requires one input");
    anyhow::ensure!(syntax::empty_crate::valid_crate_name(&probe.name), "invalid probe crate name");
    probe.emission = emission.context("the source probe requires an explicit emission request")?;
    if probe.emission == Emission::LlvmIr {
        anyhow::ensure!(seen.contains("--crate-type"), "LLVM IR probes require a library crate");
        anyhow::ensure!(
            seen.contains("--crate-name") || probe.source == "-",
            "LLVM IR file probes require an explicit crate name"
        );
        // Query answers intentionally ignore many codegen values which do not
        // affect cfg. They are not validation for compilation or output naming.
        anyhow::ensure!(probe.flags.is_empty(), "unsupported LLVM IR probe codegen options");
    }
    Ok(probe)
}

pub(super) fn check(sysroot: &Path, args: &[OsString]) -> Result<bool> {
    let mut probe = parse(args)?;
    let host = super::own::host_tuple(&sysroot.join("bin/rustc"))
        .context("no owned compiler probe host")?;
    let target = probe.target.as_deref().unwrap_or(&host);
    let env: BTreeMap<_, _> = crate::process::vars().collect();
    anyhow::ensure!(
        super::target_cfg(sysroot, target, &env).is_some(),
        "unknown compiler probe target"
    );
    let mut cfg_args: Vec<_> = probe.flags.iter().map(OsString::from).collect();
    for cfg in &probe.cfg {
        cfg_args.extend([OsString::from("--cfg"), OsString::from(cfg)]);
    }
    cfg_args.extend([
        OsString::from("--target"),
        OsString::from(target),
        OsString::from("--print=cfg"),
    ]);
    let var = |name: &str| std::env::var_os(name);
    let cfgs = super::own::answer(&sysroot.join("bin/rustc"), &cfg_args, &[], &var)
        .context("unknown compiler probe cfg/codegen options")?;
    probe.cfg = String::from_utf8(cfgs)?.lines().map(str::to_owned).collect();
    let source = if probe.source == "-" { None } else { Some(std::path::absolute(&probe.source)?) };
    let text = if let Some(source) = &source {
        std::fs::read_to_string(source)?
    } else {
        let mut text = String::new();
        std::io::stdin().read_to_string(&mut text)?;
        text
    };
    if probe.emission == Emission::LlvmIr {
        emit_empty_ir(sysroot, &probe, &text, source.as_deref(), target, &var)?;
        return Ok(true);
    }
    std::fs::create_dir_all(&probe.out)?;
    struct Scratch(std::path::PathBuf);
    impl Scratch {
        fn path(&self) -> &Path {
            &self.0
        }
    }
    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    let dir = Scratch(probe.out.join(format!(
        "blaze-probe-{}-{}",
        std::process::id(),
        crate::live::epoch_us()
    )));
    std::fs::create_dir(&dir.0)?;
    let source = if let Some(source) = source {
        source
    } else {
        let path = dir.path().join("stdin.rs");
        std::fs::write(&path, text)?;
        path
    };
    let plan = crate::cargo::own_script::planned_description(
        sysroot,
        vec![crate::cargo::own_script::Planned {
            proc_macro: None,
            name: probe.name.clone(),
            root: source,
            edition: probe.edition.to_string(),
            cfg: probe.cfg,
            codegen_flags: probe.flags,
            // Caller variables are already in the checker process. Crate overrides
            // would persist credentials in a temporary source description.
            env: BTreeMap::new(),
            deps: vec![],
        }],
    );
    let mut plan: serde_json::Value = serde_json::from_str(&plan)?;
    if let Some(target) = &probe.target {
        plan["crates"][0]["target"] = serde_json::json!(target);
    }
    let project = dir.path().join("rust-project.json");
    std::fs::write(&project, serde_json::to_vec(&plan)?)?;
    let mut argv = vec![
        project.display().to_string(),
        "--crate".into(),
        probe.name.clone(),
        "--out".into(),
        dir.path().join("unused.o").display().to_string(),
    ];
    if let Some(target) = probe.target {
        argv.extend(["--target".into(), target]);
    }
    let parsed = crate::Args::parse(argv.into_iter())?;
    let bootstrap = std::env::var("RUSTC_BOOTSTRAP")
        .ok()
        .is_some_and(|it| it == "1" || it.split(',').any(|name| name == probe.name));
    crate::check::source(parsed, bootstrap)
}

fn emit_empty_ir(
    sysroot: &Path,
    probe: &Probe,
    text: &str,
    source: Option<&Path>,
    target: &str,
    var: &dyn Fn(&str) -> Option<OsString>,
) -> Result<()> {
    let checked = syntax::empty_crate::check(text, probe.edition)
        .map_err(anyhow::Error::msg)
        .context("owned LLVM IR probe acceptance is not proven")?;
    let target = super::own::llvm_target(sysroot, target, var)
        .context("unknown LLVM IR probe target or deployment version")?;
    anyhow::ensure!(checked.no_std() || target.has_std, "the probe target has no standard library");
    let library = sysroot.join("lib/rustlib/src/rust/library");
    anyhow::ensure!(library.join("core/src/lib.rs").is_file(), "the probe needs core source");
    anyhow::ensure!(
        checked.no_std() || library.join("std/src/lib.rs").is_file(),
        "the probe needs standard library source"
    );
    let output = probe.out.join(format!("{}.ll", probe.name));
    anyhow::ensure!(
        source.is_none_or(|source| !same_file::is_same_file(source, &output).unwrap_or(false)),
        "the probe output would overwrite its source"
    );
    // There are no definitions, references or bodies in the checked translation
    // unit. The complete LLVM module is its source identity and target facts.
    let module = format!(
        "; Empty Rust library compiled by blaze\n\
         source_filename = {}\n\
         target datalayout = {}\n\
         target triple = {}\n",
        llvm_string(&probe.source),
        llvm_string(&target.data_layout),
        llvm_string(&target.triple),
    );
    std::fs::create_dir_all(&probe.out)?;
    let mut file = tempfile::NamedTempFile::new_in(&probe.out)?;
    file.write_all(module.as_bytes())?;
    file.persist(&output).with_context(|| format!("writing probe output {}", output.display()))?;
    Ok(())
}

/// LLVM string escapes are hexadecimal bytes, not Rust/JSON character escapes.
fn llvm_string(value: &str) -> String {
    use std::fmt::Write as _;
    let mut quoted = String::from("\"");
    for byte in value.bytes() {
        if byte.is_ascii_graphic() && !matches!(byte, b'"' | b'\\') || byte == b' ' {
            quoted.push(char::from(byte));
        } else {
            write!(quoted, "\\{byte:02X}").unwrap();
        }
    }
    quoted.push('"');
    quoted
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ir_probe(out: &Path) -> Probe {
        let args = ["--crate-name=empty", "--crate-type=lib", "--emit=llvm-ir", "-"];
        let mut probe = parse(&args.map(OsString::from)).unwrap();
        probe.out = out.to_owned();
        probe
    }

    fn source_toolchain(path: &Path) {
        crate::answers::tests::synthetic_toolchain(path);
        for name in ["core", "std"] {
            let root = path.join("lib/rustlib/src/rust/library").join(name).join("src");
            std::fs::create_dir_all(&root).unwrap();
            std::fs::write(root.join("lib.rs"), "").unwrap();
        }
    }

    #[test]
    fn metadata_probe_parser_preserves_cfg_target_and_rejects_artifact_inputs() {
        let args = [
            "--cfg=enabled",
            "--edition=2021",
            "--crate-name=sample",
            "--crate-type=lib",
            "--cap-lints=allow",
            "--emit=dep-info,metadata",
            "--out-dir",
            "/tmp/probes",
            "probe.rs",
            "--target",
            "aarch64-apple-darwin",
            "-Copt-level=1",
        ];
        let args: Vec<_> = args.into_iter().map(OsString::from).collect();
        let parsed = parse(&args).unwrap();
        assert_eq!(parsed.cfg, ["enabled"]);
        assert_eq!(parsed.flags, ["-Copt-level=1"]);
        assert_eq!(parsed.target.as_deref(), Some("aarch64-apple-darwin"));
        assert_eq!(parsed.source, "probe.rs");
        assert_eq!(parsed.edition, syntax::Edition::Edition2021);
        assert_eq!(parsed.emission, Emission::Metadata);
        for extra in ["--extern=other=/rustc.rmeta", "--emit=link", "--unknown"] {
            let mut args = args.clone();
            args.push(extra.into());
            assert!(parse(&args).is_err(), "{extra}");
        }
    }

    #[test]
    fn llvm_ir_requests_validate_names_editions_flags_and_every_output() {
        let base = ["--crate-type=lib", "--emit=llvm-ir", "-"].map(OsString::from);
        for name in ["sample", "_", "0abc", "type", "é"] {
            for edition in ["2015", "2018", "2021", "2024"] {
                let mut args = base.to_vec();
                args.extend([
                    format!("--crate-name={name}").into(),
                    format!("--edition={edition}").into(),
                ]);
                assert!(parse(&args).is_ok(), "{name}, {edition}");
            }
        }
        for extra in [
            "--crate-name=",
            "--crate-name=../escape",
            "--crate-name=bad-name",
            "--edition=2027",
            "--emit=llvm-ir",
            "--emit=llvm-ir,metadata",
            "--emit=link",
            "--emit=llvm-ir=other.ll",
            "--crate-type=bin",
            "--extern=other=other.rmeta",
            "-Cextra-filename=-suffix",
            "-Cdebuginfo=not-a-number",
            "-Cdebuginfo=0",
            "-Copt-level=3",
            "-O",
            "-o=other.ll",
            "--unknown",
        ] {
            let mut args = base.to_vec();
            args.push(extra.into());
            assert!(parse(&args).is_err(), "{extra}");
        }
        for args in [
            vec!["--emit=llvm-ir", "-"],
            vec!["--crate-type=lib", "--emit=llvm-ir,metadata", "-"],
            vec!["--crate-type=lib", "--emit=llvm-ir=other.ll", "-"],
            vec!["--crate-type=lib", "--emit=llvm-ir", "file.rs"],
        ] {
            assert!(parse(&args.into_iter().map(OsString::from).collect::<Vec<_>>()).is_err());
        }
    }

    #[test]
    fn empty_ir_emission_writes_a_targeted_module_and_requires_language_sources() {
        let dir = tempfile::tempdir().unwrap();
        let sysroot = dir.path().join("toolchain");
        source_toolchain(&sysroot);
        let probe = ir_probe(&dir.path().join("out"));
        let output = probe.out.join("empty.ll");
        let emit = |text| {
            emit_empty_ir(&sysroot, &probe, text, None, "x86_64-unknown-linux-gnu", &|_| None)
        };
        emit("//! empty library\n").unwrap();
        let module = std::fs::read_to_string(&output).unwrap();
        assert!(module.contains("source_filename = \"-\"\n"), "{module}");
        assert!(module.contains("target datalayout = \"e-m:e-"), "{module}");
        assert!(module.contains("target triple = \"x86_64-unknown-linux-gnu\"\n"), "{module}");
        assert!(!module.contains("define "), "{module}");
        std::fs::remove_file(&output).unwrap();
        for text in ["pub fn f() {}", "#![no_std(bad)]", "#![cfg(any())]"] {
            assert!(emit(text).is_err(), "{text}");
            assert!(!output.exists(), "{text}");
        }
        let library = sysroot.join("lib/rustlib/src/rust/library");
        std::fs::remove_file(library.join("std/src/lib.rs")).unwrap();
        assert!(emit("").is_err());
        assert!(!output.exists());
        emit("#![no_std]").unwrap();
        std::fs::remove_file(&output).unwrap();
        std::fs::remove_file(library.join("core/src/lib.rs")).unwrap();
        assert!(emit("#![no_std]").is_err());
        assert!(!output.exists());
    }

    #[test]
    fn empty_ir_does_not_overwrite_its_input_or_ignore_unknown_targets() {
        let dir = tempfile::tempdir().unwrap();
        let sysroot = dir.path().join("toolchain");
        source_toolchain(&sysroot);
        let probe = ir_probe(dir.path());
        let source = probe.out.join("empty.ll");
        std::fs::write(&source, "// original source").unwrap();
        assert!(
            emit_empty_ir(&sysroot, &probe, "", Some(&source), "x86_64-unknown-linux-gnu", &|_| {
                None
            })
            .is_err()
        );
        assert_eq!(std::fs::read_to_string(&source).unwrap(), "// original source");
        std::fs::remove_file(&source).unwrap();
        assert!(emit_empty_ir(&sysroot, &probe, "", None, "unknown-target", &|_| None).is_err());
        assert!(!source.exists());
    }

    #[test]
    fn llvm_strings_escape_bytes_including_non_ascii_paths() {
        assert_eq!(llvm_string("a\\b\"c\né"), "\"a\\5Cb\\22c\\0A\\C3\\A9\"");
    }
}
