use std::{
    collections::BTreeMap,
    ffi::OsString,
    path::{Path, PathBuf},
};

use anyhow::{Context as _, ensure};
use blaze_wire::{ConfigEnvValue, Digest};
use serde_json::Value;
use sha2::{Digest as _, Sha256};

pub(super) const FIELD: &str = "blaze_owned_proc_macro_server";

pub(super) fn server(project: &Value) -> anyhow::Result<Option<PathBuf>> {
    let Some(value) = project.get(FIELD) else {
        return Ok(None);
    };
    let bytes: Vec<u8> = serde_json::from_value(value.clone())
        .map_err(|_| anyhow::anyhow!("invalid owned macro runtime transport"))?;
    let (version, path, expected): (u32, ConfigEnvValue, Digest) =
        blaze_wire::decode(&bytes, &blaze_wire::Limits::DOCUMENTED)?;
    ensure!(version == 1, "unknown owned macro runtime version");
    ensure!(
        !matches!(path, ConfigEnvValue::WorkspaceRelative(_)),
        "owned macro runtime path is relative"
    );
    let path = PathBuf::from(path.materialize(Path::new("/"))?);
    ensure!(path.is_absolute(), "owned macro runtime path is relative");
    ensure!(path.is_file(), "owned macro runtime is not an executable file");
    let actual: Digest = Sha256::digest(std::fs::read(&path)?).into();
    ensure!(actual == expected, "owned macro runtime changed after admission");
    Ok(Some(path))
}

/// Runtime policy is supplied explicitly by the parent service and never enters a crate env.
pub(super) fn pin(
    service: &BTreeMap<String, OsString>,
    work: &Path,
) -> anyhow::Result<Option<Value>> {
    let Some(path) = service.get("RA_JIT_PROC_MACRO_SERVER") else {
        return Ok(None);
    };
    if path.is_empty() || path == "0" || path == "off" {
        return Ok(None);
    }
    let path = PathBuf::from(path).canonicalize().context("explicit owned macro runtime")?;
    ensure!(path.is_file(), "explicit owned macro runtime is not a file");
    let before: Digest = Sha256::digest(std::fs::read(&path)?).into();
    std::fs::create_dir_all(work)?;
    let copied = work.join(format!("owned-macro-server{}", std::env::consts::EXE_SUFFIX));
    let mut destination = std::fs::OpenOptions::new().write(true).create_new(true).open(&copied)?;
    let mut source = std::fs::File::open(&path)?;
    std::io::copy(&mut source, &mut destination)?;
    destination.sync_all()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        destination.set_permissions(std::fs::Permissions::from_mode(0o500))?;
    }
    let copied_digest: Digest = Sha256::digest(std::fs::read(&copied)?).into();
    let after: Digest = Sha256::digest(std::fs::read(&path)?).into();
    ensure!(
        before == copied_digest && before == after,
        "explicit owned macro runtime changed while pinning"
    );
    Ok(Some(serde_json::json!(blaze_wire::encode(&(
        1_u32,
        ConfigEnvValue::from_native(copied.as_os_str()),
        copied_digest
    )))))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_runtime_is_pinned_and_revalidated_without_crate_environment() {
        let work = tempfile::tempdir().unwrap();
        let source = work.path().join("explicit-server");
        std::fs::write(&source, b"owned server image").unwrap();
        let value = pin(
            &BTreeMap::from([("RA_JIT_PROC_MACRO_SERVER".into(), source.clone().into_os_string())]),
            &work.path().join("runtime"),
        )
        .unwrap()
        .unwrap();
        let project = serde_json::json!({ FIELD: value });
        let pinned = server(&project).unwrap().unwrap();
        assert_ne!(pinned, source);
        std::fs::write(&source, b"changed source").unwrap();
        assert_eq!(server(&project).unwrap(), Some(pinned.clone()));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&pinned, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        std::fs::write(&pinned, b"changed pinned image").unwrap();
        assert!(server(&project).unwrap_err().to_string().contains("changed after admission"));
        assert!(server(&serde_json::json!({})).unwrap().is_none());
    }

    #[test]
    fn disabled_or_absent_runtime_never_discovers_a_toolchain_server() {
        let work = tempfile::tempdir().unwrap();
        for value in [None, Some("0"), Some("off"), Some("")] {
            let service = value
                .map(|value| ("RA_JIT_PROC_MACRO_SERVER".into(), value.into()))
                .into_iter()
                .collect();
            assert!(pin(&service, work.path()).unwrap().is_none());
        }
    }
}
