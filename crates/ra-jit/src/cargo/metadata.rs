//! The workspace as `cargo metadata --no-deps --format-version 1` prints it, read by
//! blaze-manifest as cargo 1.99.0 reads it, with no cargo run (docs/resolve.md).

use std::path::Path;

use anyhow::Context as _;
use blaze_manifest::diagnostic::ManifestDiagnostic;
use blaze_manifest::metadata::{export_no_deps, root_manifest};
use blaze_manifest::{Config, Context, Workspace};
use cargo_metadata::Metadata;

/// The workspace found from the current directory, or from `manifest_path`, with cargo's
/// `--config` arguments `config`. A workspace cargo refuses ends the command as cargo ends:
/// its message, status 101.
pub(crate) fn no_deps(manifest_path: Option<&Path>, config: &[String]) -> Metadata {
    let cwd = std::env::current_dir().unwrap_or_default();
    match Config::from_process(config).and_then(|config| read(config, manifest_path)) {
        Ok(metadata) => metadata,
        Err(err) => {
            refused(&err, &cwd);
            std::process::exit(101)
        }
    }
}

fn read(config: Config, manifest_path: Option<&Path>) -> anyhow::Result<Metadata> {
    let ctx = Context::new(config);
    let root = root_manifest(manifest_path, &ctx)?;
    let ws = Workspace::new(&root, &ctx)?;
    let json = serde_json::to_value(export_no_deps(&ws))?;
    serde_json::from_value(json).context("blaze's own reading of the workspace is not cargo's form")
}

/// `err` as cargo prints it.
fn refused(err: &anyhow::Error, cwd: &Path) {
    if let Some(diagnostic) = err.downcast_ref::<ManifestDiagnostic>() {
        eprintln!("{}", diagnostic.render(cwd));
        return;
    }
    let mut chain = err.chain();
    super::say::error(&chain.next().map(ToString::to_string).unwrap_or_default());
    for cause in chain {
        eprintln!("\nCaused by:");
        for line in cause.to_string().lines() {
            if line.is_empty() { eprintln!() } else { eprintln!("  {line}") }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use cargo_metadata::Metadata;

    fn copy_dir(from: &Path, to: &Path) {
        std::fs::create_dir_all(to).unwrap();
        for entry in std::fs::read_dir(from).unwrap() {
            let entry = entry.unwrap();
            if entry.file_type().unwrap().is_dir() {
                copy_dir(&entry.path(), &to.join(entry.file_name()));
            } else {
                std::fs::copy(entry.path(), to.join(entry.file_name())).unwrap();
            }
        }
    }

    /// Each workspace of blaze-manifest's fixtures that cargo reads is, as `cargo_metadata`
    /// gives it to the rest of blaze, what cargo 1.99.0 printed for it.
    #[test]
    fn fixtures_are_what_cargo_metadata_gives() {
        let dir =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../blaze-manifest/tests/fixtures");
        let scratch = std::env::temp_dir().join(format!("ra-jit-metadata-{}", std::process::id()));
        let home = scratch.join("cargo-home");
        std::fs::create_dir_all(&home).unwrap();
        let home = home.canonicalize().unwrap();
        let mut read = 0;
        for entry in std::fs::read_dir(&dir).unwrap() {
            let fixture = entry.unwrap().path();
            let expected = std::fs::read_to_string(fixture.join("expected.json")).unwrap();
            if expected.contains("\"error\"") {
                continue;
            }
            let root = scratch.join(fixture.file_name().unwrap());
            copy_dir(&fixture, &root);
            let root = root.canonicalize().unwrap();
            let cwd = match std::fs::read_to_string(fixture.join("fixture.json")) {
                Ok(text) => {
                    let spec: serde_json::Value = serde_json::from_str(&text).unwrap();
                    root.join(spec["cwd"].as_str().unwrap_or(""))
                }
                Err(_) => root.clone(),
            };
            let expected = expected
                .replace("$ROOT", &root.display().to_string())
                .replace("$CARGO_HOME", &home.display().to_string());
            let expected: Metadata = serde_json::from_str(&expected).unwrap();
            let env = [("CARGO_HOME".to_owned(), home.display().to_string())];
            let config = blaze_manifest::Config::load(&cwd, env, &[]).unwrap();
            let got = super::read(config, None).unwrap();
            assert_eq!(got, expected, "{}", fixture.display());
            read += 1;
        }
        let _ = std::fs::remove_dir_all(&scratch);
        assert!(read >= 3, "{read} fixtures read");
    }
}
