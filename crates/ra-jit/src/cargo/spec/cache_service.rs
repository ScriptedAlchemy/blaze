//! Compiler-process cache policy, separate from the source request and saved BuildSpec.
//!
//! A direct command may explicitly select the existing shared cache service. A saved
//! replay never acquires a worker's ambient remote, credentials or trust configuration.

use std::{collections::BTreeMap, ffi::OsString, path::Path};

use anyhow::Context as _;

/// Whether `caller` selects a store and a remote of its own, which `configure` hands to the
/// compiler processes. They pull before compiling and publish before they exit, so the
/// command that started them returns with its packs on the remote.
pub(in crate::cargo) fn selected(caller: &BTreeMap<String, OsString>) -> bool {
    caller.get("RA_JIT_CACHE").is_some_and(|store| !ra_jit_store::is_off(store))
        && caller.get("RA_JIT_REMOTE").is_some_and(|remote| !remote.is_empty())
}

pub(super) fn configure(
    compiler: &mut BTreeMap<String, OsString>,
    caller: &BTreeMap<String, OsString>,
    cwd: &Path,
) -> anyhow::Result<()> {
    let Some(store) = caller.get("RA_JIT_CACHE") else { return Ok(()) };
    if ra_jit_store::is_off(store) {
        // Shared::from_env expects a real store path; a disabled store must not
        // accidentally open a directory named `off` just because a remote is set.
        return Ok(());
    }
    compiler.insert("RA_JIT_CACHE".into(), cwd.join(store).into_os_string());
    for name in ["RA_JIT_CACHE_MAX", "RA_JIT_CACHE_SEAL_AT", "RA_JIT_CACHE_CHECK"] {
        copy(compiler, caller, name);
    }
    let Some(remote) = caller.get("RA_JIT_REMOTE").filter(|value| !value.is_empty()) else {
        return Ok(());
    };
    // Keep the transport's own validation. A directory is relative to the requester,
    // not to a compiler/producer child that changes its working directory.
    let text = remote.to_str().context("RA_JIT_REMOTE must be text")?;
    let s3 = text.starts_with("s3://");
    let network = s3
        || text.starts_with("blaze+")
        || text.starts_with("http://")
        || text.starts_with("https://");
    compiler.insert(
        "RA_JIT_REMOTE".into(),
        if network { remote.clone() } else { cwd.join(remote).into_os_string() },
    );
    for name in [
        "RA_JIT_PROJECT",
        "RA_JIT_REMOTE_READONLY",
        "RA_JIT_REMOTE_LAZY",
        "RA_JIT_REMOTE_PUSH_EVERY",
        "RA_JIT_REMOTE_JOBS",
        "RA_JIT_REMOTE_TIMEOUT",
        "RA_JIT_REMOTE_MAX",
        "RA_JIT_REMOTE_TRUST",
        "RA_JIT_REMOTE_UNSIGNED",
    ] {
        copy(compiler, caller, name);
    }
    copy_path(compiler, caller, cwd, "RA_JIT_REMOTE_SIGN");
    if s3 {
        // Preserve the existing AWS SDK provider/configuration chain for this selected
        // transport. Do not inherit HOME: explicit config/token files, environment
        // credentials and role providers work; HOME-based profile/SSO discovery still
        // belongs to the compiler's isolated home.
        for (name, value) in caller.iter().filter(|(name, _)| name.starts_with("AWS_")) {
            compiler.insert(name.clone(), value.clone());
        }
        for name in [
            "AWS_CONFIG_FILE",
            "AWS_SHARED_CREDENTIALS_FILE",
            "AWS_WEB_IDENTITY_TOKEN_FILE",
            "AWS_CONTAINER_AUTHORIZATION_TOKEN_FILE",
            "AWS_CA_BUNDLE",
        ] {
            copy_path(compiler, caller, cwd, name);
        }
    }
    if network {
        copy_path(compiler, caller, cwd, "SSL_CERT_FILE");
        if let Some(value) = caller.get("SSL_CERT_DIR") {
            compiler.insert(
                "SSL_CERT_DIR".into(),
                std::env::join_paths(
                    std::env::split_paths(value).map(|path| {
                        if path.as_os_str().is_empty() { path } else { cwd.join(path) }
                    }),
                )
                .context("SSL_CERT_DIR contains an invalid certificate directory")?,
            );
        }
    }
    Ok(())
}

fn copy(
    compiler: &mut BTreeMap<String, OsString>,
    caller: &BTreeMap<String, OsString>,
    name: &str,
) {
    if let Some(value) = caller.get(name) {
        compiler.insert(name.into(), value.clone());
    }
}

fn copy_path(
    compiler: &mut BTreeMap<String, OsString>,
    caller: &BTreeMap<String, OsString>,
    cwd: &Path,
    name: &str,
) {
    if let Some(value) = caller.get(name) {
        compiler.insert(
            name.into(),
            if value.is_empty() { value.clone() } else { cwd.join(value).into_os_string() },
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    fn environment(pairs: &[(&str, &str)]) -> BTreeMap<String, OsString> {
        pairs.iter().map(|(key, value)| ((*key).into(), (*value).into())).collect()
    }

    fn isolated() -> BTreeMap<String, OsString> {
        environment(&[
            ("HOME", "private-home"),
            ("RA_JIT_CACHE", "off"),
            ("RA_JIT_BACKEND", "cranelift"),
        ])
    }

    #[test]
    fn direct_cache_policy_is_explicit_and_paths_follow_requester() {
        let directory = tempfile::tempdir().unwrap();
        let cwd = directory.path();
        let caller = environment(&[
            ("RA_JIT_CACHE", "cache"),
            ("RA_JIT_CACHE_MAX", "64M"),
            ("RA_JIT_CACHE_CHECK", "1"),
            ("RA_JIT_REMOTE", "shared"),
            ("RA_JIT_PROJECT", "project-a"),
            ("RA_JIT_REMOTE_SIGN", "keys/sign"),
            ("RA_JIT_REMOTE_TRUST", "public-key"),
            ("RA_JIT_REMOTE_READONLY", "1"),
            ("RA_JIT_REMOTE_LAZY", "1"),
            ("RA_JIT_REMOTE_TIMEOUT", "12"),
            ("RA_JIT_REMOTE_MAX", "4M"),
            ("HOME", "requester-home"),
            ("RA_JIT_BACKEND", "invalid"),
            ("AWS_SECRET_ACCESS_KEY", "unused-transport"),
            ("UNRELATED_SECRET", "do-not-inherit"),
        ]);
        let unchanged = caller.clone();
        let mut compiler = isolated();
        configure(&mut compiler, &caller, cwd).unwrap();
        for (name, path) in [
            ("RA_JIT_CACHE", "cache"),
            ("RA_JIT_REMOTE", "shared"),
            ("RA_JIT_REMOTE_SIGN", "keys/sign"),
        ] {
            assert_eq!(compiler[name], cwd.join(path));
        }
        for name in [
            "RA_JIT_PROJECT",
            "RA_JIT_CACHE_MAX",
            "RA_JIT_CACHE_CHECK",
            "RA_JIT_REMOTE_TRUST",
            "RA_JIT_REMOTE_READONLY",
            "RA_JIT_REMOTE_LAZY",
            "RA_JIT_REMOTE_TIMEOUT",
            "RA_JIT_REMOTE_MAX",
        ] {
            assert_eq!(compiler[name], caller[name]);
        }
        assert_eq!(compiler["HOME"], "private-home");
        assert_eq!(compiler["RA_JIT_BACKEND"], "cranelift");
        assert!(!compiler.contains_key("AWS_SECRET_ACCESS_KEY"));
        assert!(!compiler.contains_key("UNRELATED_SECRET"));
        assert_eq!(caller, unchanged);
    }

    #[test]
    fn disabled_or_default_stores_do_not_inherit_remote_service() {
        let directory = tempfile::tempdir().unwrap();
        let mut caller = environment(&[
            ("RA_JIT_CACHE", "active"),
            ("RA_JIT_REMOTE", "s3://bucket"),
            ("RA_JIT_REMOTE_UNSIGNED", "1"),
            ("AWS_ACCESS_KEY_ID", "unused"),
        ]);
        let mut compiler = isolated();
        for value in [None, Some("off"), Some("0"), Some("")] {
            match value {
                None => {
                    caller.remove("RA_JIT_CACHE");
                }
                Some(value) => {
                    caller.insert("RA_JIT_CACHE".into(), value.into());
                }
            }
            configure(&mut compiler, &caller, directory.path()).unwrap();
            assert_eq!(compiler, isolated());
        }
    }

    #[test]
    fn selected_s3_uses_explicit_sdk_inputs_without_inheriting_home() {
        let directory = tempfile::tempdir().unwrap();
        let caller = environment(&[
            ("RA_JIT_CACHE", "cache"),
            ("RA_JIT_REMOTE", "s3://bucket/prefix"),
            ("AWS_PROFILE", "build"),
            ("AWS_REGION", "region"),
            ("AWS_CONFIG_FILE", "aws/config"),
            ("AWS_SHARED_CREDENTIALS_FILE", "aws/credentials"),
            ("AWS_WEB_IDENTITY_TOKEN_FILE", "aws/token"),
            ("AWS_EC2_METADATA_DISABLED", "true"),
            ("HOME", "requester-home"),
            ("SSL_CERT_FILE", "certs.pem"),
        ]);
        let mut compiler = isolated();
        configure(&mut compiler, &caller, directory.path()).unwrap();
        for name in ["RA_JIT_REMOTE", "AWS_PROFILE", "AWS_REGION", "AWS_EC2_METADATA_DISABLED"] {
            assert_eq!(compiler[name], caller[name]);
        }
        for name in [
            "AWS_CONFIG_FILE",
            "AWS_SHARED_CREDENTIALS_FILE",
            "AWS_WEB_IDENTITY_TOKEN_FILE",
            "SSL_CERT_FILE",
        ] {
            assert_eq!(compiler[name], directory.path().join(&caller[name]));
        }
        assert_eq!(compiler["HOME"], "private-home");
    }

    #[test]
    fn server_addresses_and_certificate_search_lists_preserve_their_meaning() {
        let directory = tempfile::tempdir().unwrap();
        let cwd = directory.path();
        let certificates = [
            std::path::PathBuf::from("certs"),
            std::path::PathBuf::new(),
            cwd.join("absolute-certs"),
        ];
        for remote in ["blaze+http://127.0.0.1:8080", "https://cache.invalid/prefix"] {
            let mut caller = environment(&[
                ("RA_JIT_CACHE", "cache"),
                ("RA_JIT_REMOTE", remote),
                ("AWS_PROFILE", "unused-transport"),
            ]);
            caller.insert("SSL_CERT_DIR".into(), std::env::join_paths(&certificates).unwrap());
            let mut compiler = isolated();
            configure(&mut compiler, &caller, cwd).unwrap();
            assert_eq!(compiler["RA_JIT_REMOTE"], remote);
            assert_eq!(
                std::env::split_paths(&compiler["SSL_CERT_DIR"]).collect::<Vec<_>>(),
                vec![cwd.join("certs"), std::path::PathBuf::new(), cwd.join("absolute-certs")]
            );
            assert!(!compiler.contains_key("AWS_PROFILE"));
        }
    }

    #[cfg(unix)]
    #[test]
    fn native_cache_and_signing_paths_are_not_lossy() {
        use std::os::unix::ffi::OsStringExt as _;
        let directory = tempfile::tempdir().unwrap();
        let mut caller = environment(&[("RA_JIT_REMOTE", "shared")]);
        caller.insert("RA_JIT_CACHE".into(), OsString::from_vec(b"cache-\xff".to_vec()));
        caller.insert("RA_JIT_REMOTE_SIGN".into(), OsString::from_vec(b"sign-\xfe".to_vec()));
        let mut compiler = isolated();
        configure(&mut compiler, &caller, directory.path()).unwrap();
        for name in ["RA_JIT_CACHE", "RA_JIT_REMOTE_SIGN"] {
            assert_eq!(compiler[name], directory.path().join(&caller[name]));
        }
    }

    #[test]
    fn configured_shared_cache_publishes_and_fetches_in_fresh_processes() {
        // Store and fetcher are process singletons. Fresh children exercise the real
        // Shared::from_env boundary without mutating this test runner's environment.
        const CHILD: &str = "BLAZE_CACHE_SERVICE_TEST_CHILD";
        const KEY: [u8; 32] = [0x38; 32];
        const PAYLOAD: &[u8] = b"owned compiler shared-cache round trip";
        if let Some(mode) = std::env::var_os(CHILD) {
            ra_jit_store::built_from("owned-cache-service-regression");
            ra_jit_store::compiles_against("owned-cache-service-toolchain");
            let shared = crate::remote::Shared::from_env().expect("explicit cache service");
            shared.pull();
            let store = ra_jit_store::Store::global().expect("active local store");
            if mode == "writer" {
                store.insert(&KEY, PAYLOAD);
                assert!(!store.seal_checked().unwrap().is_empty());
                shared.push();
            } else {
                assert_eq!(mode, "reader");
                assert_eq!(store.get(&KEY).as_deref(), Some(PAYLOAD));
                // A read-only peer cannot publish its own new entry or manifest.
                store.insert(&[0x39; 32], b"read-only peer");
                store.seal_checked().unwrap();
                shared.push();
            }
            return;
        }
        let directory = tempfile::tempdir().unwrap();
        let remote = directory.path().join("remote");
        let test = concat!(
            module_path!(),
            "::configured_shared_cache_publishes_and_fetches_in_fresh_processes"
        );
        let test = test.strip_prefix("ra_jit::").unwrap_or(test);
        let run = |mode: &str| {
            let caller = environment(&[
                ("RA_JIT_CACHE", mode),
                ("RA_JIT_REMOTE", "remote"),
                ("RA_JIT_PROJECT", "owned-service"),
                ("RA_JIT_REMOTE_UNSIGNED", "1"),
                ("RA_JIT_REMOTE_LAZY", "1"),
                ("RA_JIT_CACHE_MAX", "16M"),
                ("RA_JIT_REMOTE_READONLY", if mode == "reader" { "1" } else { "0" }),
            ]);
            let mut compiler = isolated();
            configure(&mut compiler, &caller, directory.path()).unwrap();
            let output = Command::new(std::env::current_exe().unwrap())
                .args(["--exact", test, "--nocapture"])
                .env_clear()
                .envs(compiler)
                .env(CHILD, mode)
                .current_dir(directory.path())
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(
                String::from_utf8_lossy(&output.stdout).contains("1 passed"),
                "child test did not run: {}",
                String::from_utf8_lossy(&output.stdout)
            );
            output
        };
        run("writer");
        let manifest = remote.join("manifests/owned-service");
        let before = std::fs::read(&manifest).expect("writer published selected project");
        assert!(!before.is_empty());
        let packs = || {
            std::fs::read_dir(remote.join("packs"))
                .unwrap()
                .map(|entry| entry.unwrap().file_name())
                .collect::<std::collections::BTreeSet<_>>()
        };
        let published = packs();
        assert!(!published.is_empty());
        let reader = run("reader");
        assert!(
            String::from_utf8_lossy(&reader.stderr).contains("fetched for a build"),
            "{}",
            String::from_utf8_lossy(&reader.stderr)
        );
        assert_eq!(std::fs::read(manifest).unwrap(), before);
        assert_eq!(packs(), published);
    }
}
