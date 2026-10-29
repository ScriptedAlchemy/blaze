//! Native filesystem names at the UTF-8 project-model boundary. A reversible
//! logical namespace preserves native ancestors without copying source files.

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

pub(crate) fn install_native(project: &Value) -> anyhow::Result<()> {
    let projection = projection(project)?;
    crate::dep_info::install_replay_key(project)?;
    let units = match project.get(blaze_wire::native::UNIT_FIELD) {
        Some(value) => {
            let bytes: Vec<u8> = serde_json::from_value(value.clone())
                .map_err(|_| anyhow::anyhow!("invalid owned native transport"))?;
            blaze_wire::native::decode_units(&bytes)?
        }
        None => BTreeMap::new(),
    };
    crate::native_inputs::set_native_units(units)?;
    paths::native::install(projection);
    Ok(())
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

const PROJECTION_FIELD: &str = "blaze_native_projection";

/// The workspace used for source names by an owned compiler description.
pub(crate) fn source_root(project: &Value) -> anyhow::Result<Option<paths::AbsPathBuf>> {
    let Some(root) = project.get("blaze_compiler_source_root") else { return Ok(None) };
    source_unit(project)?;
    let root = root["path"].as_str().context("invalid owned source naming root")?;
    let root = paths::AbsPathBuf::try_from(root)
        .map_err(|_| anyhow::anyhow!("owned source naming root is not absolute"))?;
    Ok(Some(root))
}

pub(crate) fn source_unit(project: &Value) -> anyhow::Result<Option<u32>> {
    let Some(root) = project.get("blaze_compiler_source_root") else { return Ok(None) };
    ensure!(
        project["compiler_mode"] == json!(true) && root["version"] == json!(1),
        "unknown owned compiler source naming format"
    );
    let unit = serde_json::from_value(root["unit"].clone())
        .map_err(|_| anyhow::anyhow!("invalid owned compiler root unit"))?;
    Ok(Some(unit))
}

fn projection(project: &Value) -> anyhow::Result<Option<paths::native::Projection>> {
    let Some(prefix) = project.get(PROJECTION_FIELD) else { return Ok(None) };
    let prefix = prefix.as_str().context("invalid native path namespace")?;
    let prefix = paths::AbsPathBuf::try_from(prefix)
        .map_err(|_| anyhow::anyhow!("native path namespace is not absolute"))?;
    Ok(Some(paths::native::Projection::new(prefix)?))
}

/// Resolve a traced logical filename using the exact descriptor that compiled it.
/// The caller still authenticates the authoritative file against its source snapshot.
pub(crate) fn physical_path(project: &Value, path: &Path) -> anyhow::Result<PathBuf> {
    match projection(project)?.map(|projection| projection.decode(path)).transpose()?.flatten() {
        Some(path) => Ok(path),
        None => Ok(path.to_owned()),
    }
}

/// Serialize a local compiler description. Native paths keep every ancestor in
/// a reversible logical namespace; the VFS resolves actual filesystem reads.
pub(crate) fn write(file: &Path, project: &Value) -> anyhow::Result<Vec<u8>> {
    let mut project = project.clone();
    let directory =
        file.parent().context("compiler description has no directory")?.join("native-paths");
    let mut projection = None;
    fn visit(
        value: &mut Value,
        directory: &Path,
        projection: &mut Option<paths::native::Projection>,
    ) -> anyhow::Result<()> {
        if let Some(native) = decoded(value)? {
            let projection = match projection {
                Some(projection) => projection,
                vacant @ None => {
                    let directory = if directory.is_absolute() {
                        directory.to_owned()
                    } else {
                        std::env::current_dir()?.join(directory)
                    };
                    let prefix = paths::AbsPathBuf::try_from(
                        directory.to_str().context("compiler namespace directory is not UTF-8")?,
                    )
                    .map_err(|_| anyhow::anyhow!("compiler namespace directory is not absolute"))?
                    .normalize();
                    vacant.insert(paths::native::Projection::new(prefix)?)
                }
            };
            *value = json!(projection.encode(&native)?.as_str());
            return Ok(());
        }
        match value {
            Value::Array(values) => {
                for value in values {
                    visit(value, directory, projection)?;
                }
            }
            Value::Object(values) => {
                for value in values.values_mut() {
                    visit(value, directory, projection)?;
                }
            }
            _ => {}
        }
        Ok(())
    }
    visit(&mut project, &directory, &mut projection)?;
    if let Some(projection) = projection {
        project[PROJECTION_FIELD] = json!(projection.prefix().as_str());
    }
    let private = serde_json::to_vec(&project)?;
    blaze_wire::native::redact_transport(&mut project)?;
    if let Some(object) = project.as_object_mut() {
        object.remove("blaze_owned_proc_macro_server");
        object.remove(crate::dep_info::TRACE_KEY_FIELD);
    }
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
    #[cfg(target_os = "linux")]
    fn native_filename_and_nested_directories_keep_module_and_include_topology() {
        use std::os::unix::ffi::OsStringExt as _;
        let work = tempfile::tempdir().unwrap();
        let source = work.path().join(OsString::from_vec(b"source-\xff".to_vec()));
        let nested = source.join("a").join(OsString::from_vec(b"nested-\xfe".to_vec()));
        let root = nested.join("src").join(OsString::from_vec(b"main-\xfd.rs".to_vec()));
        std::fs::create_dir_all(root.parent().unwrap()).unwrap();
        std::fs::create_dir_all(source.join("peer")).unwrap();
        std::fs::write(
            &root,
            b"mod child; const DATA: &str = include_str!(\"../../../peer/data\");",
        )
        .unwrap();
        std::fs::write(root.parent().unwrap().join("child.rs"), b"pub fn answer() -> u8 { 42 }")
            .unwrap();
        std::fs::write(source.join("peer/data"), b"native bytes").unwrap();
        let project = json!({"compiler_mode": true, "crates": [{
            "display_name": "raw_source", "root_module": path(&root), "edition": "2021",
            "deps": [], "cfg": [], "env": {}, "is_workspace_member": true,
            "build": {"label": "raw_source", "build_file": path(&root), "target_kind": "lib"},
        }]});
        let file = work.path().join("rust-project.json");
        let private: Value = serde_json::from_slice(&write(&file, &project).unwrap()).unwrap();
        let logical =
            paths::AbsPathBuf::try_from(private["crates"][0]["root_module"].as_str().unwrap())
                .unwrap();
        let projection = projection(&private).unwrap().unwrap();
        assert_eq!(physical_path(&private, logical.as_ref()).unwrap(), root);
        let child = projection.join(logical.parent().unwrap(), "child.rs").unwrap();
        assert_eq!(
            std::fs::read(physical_path(&private, child.as_ref()).unwrap()).unwrap(),
            b"pub fn answer() -> u8 { 42 }"
        );
        let sibling = projection.join(logical.parent().unwrap(), "../../../peer/data").unwrap();
        assert_eq!(
            std::fs::read(physical_path(&private, sibling.as_ref()).unwrap()).unwrap(),
            b"native bytes"
        );
        assert!(!work.path().join("native-paths").exists());
        assert!(private.get("blaze_native_aliases").is_none());
        assert_eq!(decoded(&path(&source)).unwrap(), Some(source));

        let prefix = projection.prefix().to_path_buf();
        paths::native::install(Some(projection));
        struct ResetProjection;
        impl Drop for ResetProjection {
            fn drop(&mut self) {
                paths::native::install(None);
            }
        }
        let _reset = ResetProjection;
        let base = paths::AbsPathBuf::assert_utf8(work.path().to_owned());
        let data: project_model::ProjectJsonData = serde_json::from_value(private).unwrap();
        let json = project_model::ProjectJson::new(None, &base, data);
        let workspace = project_model::ProjectWorkspace {
            kind: project_model::ProjectWorkspaceKind::Json(json),
            sysroot: project_model::Sysroot::empty(),
            rustc_cfg: Vec::new(),
            toolchain: None,
            target: Err("unused target layout in path-loading test".into()),
            cfg_overrides: Default::default(),
            extra_includes: Vec::new(),
            set_test: false,
        };
        let config = load_cargo::LoadCargoConfig {
            load_out_dirs_from_check: false,
            with_proc_macro_server: load_cargo::ProcMacroServerChoice::None,
            prefill_caches: false,
            num_worker_threads: 1,
            proc_macro_processes: 1,
        };
        let (db, _vfs, _server) =
            load_cargo::load_workspace(workspace, &Default::default(), &config).unwrap();
        let krate = hir::Crate::all(&db)[0];
        assert_eq!(
            krate.modules(&db).len(),
            2,
            "mod child resolves from the raw filename's actual parent"
        );
        use ide_db::base_db::SourceDatabase as _;
        let include = db
            .included_file(
                krate.into(),
                ide_db::base_db::AnchoredPath {
                    anchor: krate.root_file(&db),
                    path: "../../../peer/data",
                },
            )
            .unwrap();
        assert_eq!(include.bytes(&db).as_deref(), Some(&b"native bytes"[..]));

        let malformed = PathBuf::from(prefix).join("~zz.rs");
        std::fs::create_dir_all(malformed.parent().unwrap()).unwrap();
        std::fs::write(&malformed, b"physical shadow must not be read").unwrap();
        assert!(std::fs::read(db.on_disk(&malformed)).is_err());
        let malformed = paths::AbsPathBuf::assert_utf8(malformed);
        assert!(paths::native::absolutize(&malformed, "child.rs").is_none());
        assert!(vfs::VfsPath::from(malformed).join("child.rs").is_none());
    }

    #[test]
    #[cfg(unix)]
    fn relative_writer_keeps_ordinary_utf8_descriptors_ordinary() {
        let work = tempfile::tempdir().unwrap();
        let cwd = std::env::current_dir().unwrap();
        let common = cwd.ancestors().find(|path| work.path().starts_with(path)).unwrap();
        let mut directory = PathBuf::new();
        for _ in cwd.strip_prefix(common).unwrap().components() {
            directory.push("..");
        }
        directory.push(work.path().strip_prefix(common).unwrap());
        let file = directory.join("rust-project.json");
        assert!(file.is_relative());
        let project = json!({"crates": [{"root_module": "source.rs"}]});
        let sent: Value = serde_json::from_slice(&write(&file, &project).unwrap()).unwrap();
        assert_eq!(sent["crates"][0]["root_module"], "source.rs");
        assert!(sent.get(PROJECTION_FIELD).is_none());
        assert!(directory.join("native-paths").symlink_metadata().is_err());

        use std::os::unix::ffi::OsStringExt as _;
        let native = work.path().join(OsString::from_vec(b"source-\xff.rs".to_vec()));
        let project = json!({"crates": [{"root_module": path(&native)}]});
        let sent: Value = serde_json::from_slice(&write(&file, &project).unwrap()).unwrap();
        let logical = Path::new(sent["crates"][0]["root_module"].as_str().unwrap());
        assert!(logical.is_absolute());
        assert_eq!(physical_path(&sent, logical).unwrap(), native);
        assert!(!native.exists());
    }

    #[test]
    fn source_naming_root_requires_the_owned_versioned_absolute_contract() {
        let work = tempfile::tempdir().unwrap();
        let mut project = json!({
            "compiler_mode": true,
            "blaze_compiler_source_root": {"version": 1, "unit": 7, "path": work.path()},
        });
        assert_eq!(Path::new(source_root(&project).unwrap().unwrap().as_str()), work.path());
        assert_eq!(source_unit(&project).unwrap(), Some(7));
        project["blaze_compiler_source_root"]["unit"] = json!(u64::from(u32::MAX) + 1);
        assert!(source_root(&project).is_err());
        project["blaze_compiler_source_root"]["unit"] = json!(7);
        project["blaze_compiler_source_root"]["version"] = json!(2);
        assert!(source_root(&project).is_err());
        project["blaze_compiler_source_root"]["version"] = json!(1);
        project["blaze_compiler_source_root"]["path"] = json!("relative");
        assert!(source_root(&project).is_err());
        project["blaze_compiler_source_root"]["path"] = json!(work.path());
        project["compiler_mode"] = json!(false);
        assert!(source_root(&project).is_err());
        assert!(source_root(&json!({})).unwrap().is_none());
    }

    #[test]
    #[cfg(unix)]
    fn source_naming_root_round_trips_native_bytes_without_creating_native_files() {
        use std::os::unix::ffi::OsStringExt as _;
        let work = tempfile::tempdir().unwrap();
        let root = work.path().join(OsString::from_vec(b"workspace-\xff".to_vec()));
        let project = json!({
            "compiler_mode": true,
            "blaze_compiler_source_root": {"version": 1, "unit": 7, "path": path(&root)},
        });
        let sent: Value = serde_json::from_slice(
            &write(&work.path().join("rust-project.json"), &project).unwrap(),
        )
        .unwrap();
        let logical = source_root(&sent).unwrap().unwrap();
        assert_eq!(physical_path(&sent, Path::new(logical.as_str())).unwrap(), root);
        assert!(!root.exists());
    }

    #[test]
    fn malformed_native_transport_never_echoes_private_values() {
        let token = "not-a-real-native-transport-token";
        let malformed = json!({blaze_wire::native::UNIT_FIELD: [token]});
        let error = install_native(&malformed).unwrap_err().to_string();
        assert!(!error.contains(token));
        assert_eq!(error, "invalid owned native transport");
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
        let mut private = json!({"crates": [{"env": {"TOKEN": "private"}}], "blaze_native_units": [1, 2], "blaze_native_bindings": [3, 4], "blaze_owned_proc_macro_server": [5, 6]});
        private[crate::dep_info::TRACE_KEY_FIELD] = json!(vec![7_u8; 32]);
        let sent: Value = serde_json::from_slice(&write(&file, &private).unwrap()).unwrap();
        let public = read(&file).unwrap();
        assert_eq!(sent["crates"][0]["env"], private["crates"][0]["env"]);
        assert_eq!(sent["blaze_native_units"], private["blaze_native_units"]);
        assert!(public.get("blaze_native_units").is_none());
        assert!(public.get("blaze_owned_proc_macro_server").is_none());
        assert!(public.get(crate::dep_info::TRACE_KEY_FIELD).is_none());
        assert_eq!(
            sent[crate::dep_info::TRACE_KEY_FIELD],
            private[crate::dep_info::TRACE_KEY_FIELD]
        );
        assert_eq!(sent["blaze_owned_proc_macro_server"], private["blaze_owned_proc_macro_server"]);
        assert_eq!(public["crates"][0]["env"], json!({}));
        assert_eq!(public["crates"][0]["env_names"], json!(["TOKEN"]));
        assert_eq!(public["blaze_native_bindings"], json!([3, 4]));
    }
}
