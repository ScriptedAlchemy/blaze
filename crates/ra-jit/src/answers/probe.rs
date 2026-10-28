//! Metadata-only build-script probes through the partial owned source checker.
//!
//! Unknown flags and artifact consumers are not negative feature results. They
//! remain unsupported operations and must fail the surrounding producer build.
//! Apparent success is also undecided until the checker owns every acceptance
//! pass. It must never be reported as successful compilation.

use std::{collections::BTreeMap, ffi::OsString, io::Read as _, path::Path};

use anyhow::{Context as _, Result};

struct Probe {
    source: String,
    name: String,
    edition: String,
    cfg: Vec<String>,
    flags: Vec<String>,
    target: Option<String>,
    out: std::path::PathBuf,
}

fn parse(args: &[OsString]) -> Result<Probe> {
    let words = args
        .iter()
        .map(|it| it.to_str().context("non-UTF-8 probe argument"))
        .collect::<Result<Vec<_>>>()?;
    let mut probe = Probe {
        source: String::new(),
        name: "rust_out".into(),
        edition: "2015".into(),
        cfg: Vec::new(),
        flags: Vec::new(),
        target: None,
        out: std::env::current_dir()?,
    };
    let mut it = words.into_iter();
    let mut metadata = false;
    while let Some(word) = it.next() {
        let (flag, joined) = word.split_once('=').map_or((word, None), |(a, b)| (a, Some(b)));
        let mut value = || joined.or_else(|| it.next()).context("probe flag has no value");
        match flag {
            "--crate-name" => probe.name = value()?.into(),
            "--edition" => probe.edition = value()?.into(),
            "--cfg" => probe.cfg.push(value()?.into()),
            "--target" => probe.target = Some(value()?.into()),
            "--out-dir" => probe.out = std::path::absolute(value()?)?,
            "--crate-type" => {
                anyhow::ensure!(matches!(value()?, "lib" | "rlib"), "unsupported probe crate type")
            }
            "--emit" => {
                let emit = value()?;
                anyhow::ensure!(
                    emit.split(',').all(|it| matches!(it, "dep-info" | "metadata")),
                    "the source probe requires metadata-only emission"
                );
                metadata = emit.split(',').any(|it| it == "metadata");
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
    anyhow::ensure!(
        metadata && !probe.source.is_empty(),
        "the source probe requires metadata and one input"
    );
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
    let source = if probe.source == "-" {
        let mut text = String::new();
        std::io::stdin().read_to_string(&mut text)?;
        let path = dir.path().join("stdin.rs");
        std::fs::write(&path, text)?;
        path
    } else {
        std::path::absolute(&probe.source)?
    };
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
    let plan = crate::cargo::own_script::planned_description(
        sysroot,
        vec![crate::cargo::own_script::Planned {
            name: probe.name.clone(),
            root: source,
            edition: probe.edition,
            cfg: probe.cfg,
            codegen_flags: probe.flags,
            env,
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

#[cfg(test)]
mod tests {
    use super::*;

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
        for extra in ["--extern=other=/rustc.rmeta", "--emit=link", "--unknown"] {
            let mut args = args.clone();
            args.push(extra.into());
            assert!(parse(&args).is_err(), "{extra}");
        }
    }
}
