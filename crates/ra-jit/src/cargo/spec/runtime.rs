use std::{
    collections::BTreeMap,
    ffi::{OsStr, OsString},
    path::{Path, PathBuf},
};

use anyhow::{Context as _, ensure};
use blaze_wire::{ConfigEnvValue, Digest};
use serde_json::Value;
use sha2::{Digest as _, Sha256};

pub(super) const FIELD: &str = "blaze_owned_proc_macro_server";

#[derive(Clone, Default)]
pub(super) struct Unit {
    pub(super) env: BTreeMap<String, OsString>,
    pub(super) libraries: Vec<PathBuf>,
}

pub(super) fn environment(
    unit: &Unit,
    inherited: &BTreeMap<String, OsString>,
    profile: &Path,
    sysroot_lib: &Path,
    triple: &str,
) -> anyhow::Result<BTreeMap<String, OsString>> {
    let mut env = inherited.clone();
    env.extend(unit.env.clone());
    let path = crate::cargo::loader::path(&unit.libraries, profile, sysroot_lib, triple, |name| {
        env.get(name).map(OsString::as_os_str)
    })?;
    env.insert(crate::cargo::loader::variable(triple).into(), path);
    Ok(env)
}

pub(super) fn install_environment(project: &mut Value, environment: &BTreeMap<String, OsString>) {
    let units = BTreeMap::from([(
        0,
        blaze_wire::native::Unit {
            env: environment.iter().map(|(name, value)| (name.into(), value.clone())).collect(),
            flags: Vec::new(),
        },
    )]);
    project[blaze_wire::native::RUNTIME_FIELD] =
        serde_json::json!(blaze_wire::native::encode_units(&units));
}

pub(crate) fn described_environment(
    project: &Value,
) -> anyhow::Result<Option<BTreeMap<OsString, OsString>>> {
    let Some(value) = project.get(blaze_wire::native::RUNTIME_FIELD) else { return Ok(None) };
    let bytes: Vec<u8> = serde_json::from_value(value.clone())
        .map_err(|_| anyhow::anyhow!("invalid private runtime environment"))?;
    let mut units = blaze_wire::native::decode_units(&bytes)?;
    ensure!(
        units.len() == 1 && units.get(&0).is_some_and(|unit| unit.flags.is_empty()),
        "invalid private runtime environment shape"
    );
    Ok(Some(units.remove(&0).unwrap().env))
}

pub(super) use crate::cargo::runtime_context::cargo_frontend;

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
    fn loader_paths_follow_cargo_order_and_macos_defaults() {
        let unit = Unit {
            env: BTreeMap::new(),
            libraries: vec!["/generated/lib".into(), "/generated/lib".into()],
        };
        let base =
            BTreeMap::from([("LD_LIBRARY_PATH".into(), OsString::from("/prior:/profile/deps"))]);
        let env = environment(
            &unit,
            &base,
            Path::new("/profile"),
            Path::new("/sysroot/lib"),
            "x86_64-unknown-linux-gnu",
        )
        .unwrap();
        assert_eq!(
            std::env::split_paths(&env["LD_LIBRARY_PATH"]).collect::<Vec<_>>(),
            ["/generated/lib", "/profile/deps", "/profile", "/sysroot/lib", "/prior"]
                .map(PathBuf::from)
        );
        let base = BTreeMap::from([("HOME".into(), OsString::from("/home/user"))]);
        let env = environment(
            &unit,
            &base,
            Path::new("/profile"),
            Path::new("/sysroot/lib"),
            "aarch64-apple-darwin",
        )
        .unwrap();
        assert_eq!(
            std::env::split_paths(&env["DYLD_FALLBACK_LIBRARY_PATH"]).collect::<Vec<_>>(),
            [
                "/generated/lib",
                "/profile/deps",
                "/profile",
                "/sysroot/lib",
                "/home/user/lib",
                "/usr/local/lib",
                "/usr/lib"
            ]
            .map(PathBuf::from)
        );
    }

    #[cfg(unix)]
    #[test]
    fn runtime_native_paths_values_and_private_transport_round_trip() {
        use std::os::unix::ffi::OsStringExt as _;
        let raw = PathBuf::from(OsString::from_vec(b"native=/generated-\xff/lib".to_vec()));
        let unit = Unit {
            env: BTreeMap::from([(
                "RUNTIME_SECRET".into(),
                OsString::from_vec(b"value-\xfe".to_vec()),
            )]),
            libraries: vec![crate::cargo::loader::search_directory(&raw).to_owned()],
        };
        let env = environment(
            &unit,
            &BTreeMap::new(),
            Path::new("/profile"),
            Path::new("/sysroot/lib"),
            "x86_64-unknown-linux-gnu",
        )
        .unwrap();
        assert_eq!(
            std::env::split_paths(&env["LD_LIBRARY_PATH"]).next().unwrap(),
            PathBuf::from(OsString::from_vec(b"/generated-\xff/lib".to_vec()))
        );
        let mut project = serde_json::json!({"crates": []});
        install_environment(&mut project, &env);
        let decoded = described_environment(&project).unwrap().unwrap();
        assert_eq!(
            decoded[OsStr::new("RUNTIME_SECRET")],
            OsString::from_vec(b"value-\xfe".to_vec())
        );
        blaze_wire::native::redact_transport(&mut project).unwrap();
        assert!(project.get(blaze_wire::native::RUNTIME_FIELD).is_none());
        assert!(described_environment(&project).unwrap().is_none());
    }

    #[test]
    fn malformed_runtime_transport_never_inherits_the_compiler_environment() {
        let project = serde_json::json!({blaze_wire::native::RUNTIME_FIELD: [0, 1, 2]});
        assert!(described_environment(&project).is_err());
        let units = BTreeMap::from([(1, blaze_wire::native::Unit::default())]);
        let project = serde_json::json!({blaze_wire::native::RUNTIME_FIELD: blaze_wire::native::encode_units(&units)});
        assert!(described_environment(&project).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn cargo_frontend_uses_sibling_or_exact_owned_launcher() {
        use std::os::unix::fs::PermissionsExt as _;
        let work = tempfile::tempdir().unwrap();
        let compiler = work.path().join("engine's binary");
        let shell = std::env::var_os("PATH")
            .and_then(|path| {
                std::env::split_paths(&path)
                    .map(|directory| directory.join("sh"))
                    .find(|path| path.is_file())
            })
            .unwrap()
            .canonicalize()
            .unwrap();
        std::fs::write(&compiler, format!("#!{}\nprintf '%s\\n' \"$@\"\n", shell.display()))
            .unwrap();
        std::fs::set_permissions(&compiler, std::fs::Permissions::from_mode(0o500)).unwrap();
        let launcher = cargo_frontend(&compiler, &work.path().join("private")).unwrap();
        let result = std::process::Command::new(&launcher).arg("--version").output().unwrap();
        assert!(result.status.success());
        assert_eq!(result.stdout, b"cargo\n--version\n");
        let sibling = compiler.with_file_name("blaze");
        std::fs::write(&sibling, b"installed frontend").unwrap();
        assert_eq!(cargo_frontend(&compiler, &work.path().join("unused")).unwrap(), sibling);
        assert!(!work.path().join("unused").exists());
    }

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
