//! Native filesystem names at the UTF-8 project-model boundary. Aliases refer to
//! the authoritative files; no source is copied or rewritten for the frontend.

use std::{
    collections::BTreeMap,
    ffi::OsString,
    path::{Path, PathBuf},
    sync::OnceLock,
};

use anyhow::{Context as _, ensure};
use serde_json::{Value, json};

/// The loader and profile/native-context installation consume the same private
/// descriptor. A producer sends it once; subsequent readers never drain stdin.
pub(crate) fn read(file: &Path) -> anyhow::Result<Value> {
    static PRIVATE: OnceLock<Result<Value, String>> = OnceLock::new();
    if std::env::var_os("RA_JIT_PROJECT_STDIN").is_some() {
        cached(&PRIVATE, std::io::stdin().lock())
    } else {
        Ok(serde_json::from_slice(&std::fs::read(file)?)?)
    }
}

fn cached(
    cache: &OnceLock<Result<Value, String>>,
    reader: impl std::io::Read,
) -> anyhow::Result<Value> {
    cache
        .get_or_init(|| {
            serde_json::from_reader(reader)
                .map_err(|_| "invalid owned project transport".to_owned())
        })
        .clone()
        .map_err(anyhow::Error::msg)
}

pub(crate) fn path(path: &Path) -> Value {
    match path.to_str() {
        Some(path) => json!(path),
        None => json!({"blaze_native_path": path.as_os_str().as_encoded_bytes()}),
    }
}

fn decoded(value: &Value) -> anyhow::Result<Option<PathBuf>> {
    let Some(bytes) = value.get("blaze_native_path") else { return Ok(None) };
    let bytes: Vec<u8> = serde_json::from_value(bytes.clone())?;
    ensure!(!bytes.contains(&0), "NUL in compiler source path");
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStringExt as _;
        Ok(Some(PathBuf::from(OsString::from_vec(bytes))))
    }
    #[cfg(not(unix))]
    {
        anyhow::bail!("native compiler path belongs to another platform")
    }
}

/// Serialize the compiler's local description, preserving physical source names
/// through directory aliases where its UTF-8 path interner cannot hold them.
pub(crate) fn write(file: &Path, project: &Value) -> anyhow::Result<Vec<u8>> {
    let mut project = project.clone();
    let directory = file.parent().context("compiler description has no directory")?;
    let directory = directory.join("native-paths");
    let mut aliases = BTreeMap::new();
    fn visit(
        value: &mut Value,
        directory: &Path,
        aliases: &mut BTreeMap<PathBuf, PathBuf>,
    ) -> anyhow::Result<()> {
        if let Some(native) = decoded(value)? {
            ensure!(native.is_absolute(), "compiler source path is not absolute");
            let root = native
                .ancestors()
                .find(|path| path.file_name().is_some_and(|name| name.to_str().is_none()))
                .context("native source path has no native component")?;
            let at = aliases.len();
            let alias = match aliases.get(root) {
                Some(alias) => alias.clone(),
                None => {
                    std::fs::create_dir_all(directory)?;
                    let alias = directory.join(at.to_string());
                    #[cfg(unix)]
                    match std::fs::read_link(&alias) {
                        Ok(previous) => ensure!(previous == root, "compiler source alias changed"),
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                            std::os::unix::fs::symlink(root, &alias)?
                        }
                        Err(error) => return Err(error.into()),
                    }
                    aliases.insert(root.to_owned(), alias.clone());
                    alias
                }
            };
            let actual = alias.join(native.strip_prefix(root)?);
            *value = json!(actual.to_str().context("compiler alias is not UTF-8")?);
            return Ok(());
        }
        match value {
            Value::Array(values) => {
                for value in values {
                    visit(value, directory, aliases)?;
                }
            }
            Value::Object(values) => {
                for value in values.values_mut() {
                    visit(value, directory, aliases)?;
                }
            }
            _ => {}
        }
        Ok(())
    }
    visit(&mut project, &directory, &mut aliases)?;
    project["blaze_native_aliases"] = json!(
        aliases
            .iter()
            .map(|(native, alias)| {
                (
                    alias.to_str().expect("validated compiler alias"),
                    native.as_os_str().as_encoded_bytes(),
                )
            })
            .collect::<Vec<_>>()
    );
    let private = serde_json::to_vec(&project)?;
    blaze_wire::native::redact_transport(&mut project)?;
    if let Some(crates) = project["crates"].as_array_mut() {
        for krate in crates {
            if let Some(krate) = krate.as_object_mut() {
                let names: Vec<_> = krate
                    .get("env")
                    .and_then(Value::as_object)
                    .into_iter()
                    .flat_map(|env| env.keys())
                    .cloned()
                    .collect();
                krate.insert("env_names".into(), json!(names));
                krate.insert("env".into(), json!({}));
                krate.remove("blaze_native_env");
                krate.remove("blaze_native_flags");
            }
        }
    }
    std::fs::write(file, serde_json::to_vec(&project)?)?;
    Ok(private)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[cfg(unix)]
    fn raw_directory_aliases_keep_native_bytes_and_relative_source_topology() {
        use std::os::unix::ffi::OsStringExt as _;
        let work = tempfile::tempdir().unwrap();
        let source = work.path().join(OsString::from_vec(b"source-\xff".to_vec()));
        std::fs::create_dir_all(source.join("a/src")).unwrap();
        std::fs::create_dir_all(source.join("b")).unwrap();
        std::fs::write(source.join("a/src/lib.rs"), b"pub fn answer() {}\n").unwrap();
        std::fs::write(source.join("b/data"), b"native bytes").unwrap();
        let project = json!({"crates": [{"root_module": path(&source.join("a/src/lib.rs")), "build": {"build_file": path(&source.join("a/Cargo.toml"))}}]});
        let file = work.path().join("rust-project.json");
        write(&file, &project).unwrap();
        let rendered: Value = serde_json::from_slice(&std::fs::read(&file).unwrap()).unwrap();
        let root = Path::new(rendered["crates"][0]["root_module"].as_str().unwrap());
        assert_eq!(root.canonicalize().unwrap(), source.join("a/src/lib.rs"));
        assert_eq!(
            std::fs::read(root.parent().unwrap().join("../../b/data")).unwrap(),
            b"native bytes"
        );
        assert_eq!(rendered["blaze_native_aliases"].as_array().unwrap().len(), 1);
        assert_eq!(decoded(&path(&source)).unwrap(), Some(source));
    }
    #[test]
    fn private_transport_is_cached_without_a_second_read() {
        let cache = OnceLock::new();
        let private = json!({"crates": [], "blaze_native_units": [1, 2], "private": true});
        let first = cached(&cache, serde_json::to_vec(&private).unwrap().as_slice()).unwrap();
        struct NoRead;
        impl std::io::Read for NoRead {
            fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
                panic!("private descriptor drained twice")
            }
        }
        assert_eq!(cached(&cache, NoRead).unwrap(), first);
        assert_eq!(first, private);
    }

    #[test]
    fn public_file_redacts_native_and_text_environment_but_stdin_retains_them() {
        let work = tempfile::tempdir().unwrap();
        let file = work.path().join("rust-project.json");
        let private = json!({"crates": [{"env": {"TOKEN": "private"}}], "blaze_native_units": [1, 2], "blaze_native_bindings": [3, 4]});
        let sent: Value = serde_json::from_slice(&write(&file, &private).unwrap()).unwrap();
        let public = read(&file).unwrap();
        assert_eq!(sent["crates"][0]["env"], private["crates"][0]["env"]);
        assert_eq!(sent["blaze_native_units"], private["blaze_native_units"]);
        assert!(public.get("blaze_native_units").is_none());
        assert_eq!(public["crates"][0]["env"], json!({}));
        assert_eq!(public["crates"][0]["env_names"], json!(["TOKEN"]));
        assert_eq!(public["blaze_native_bindings"], json!([3, 4]));
    }
}
