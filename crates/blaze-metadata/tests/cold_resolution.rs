//! Fresh resolution, previous-lock preferences and locked/offline source contracts.

use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use blaze_metadata::Asked;
use serde_json::{Value, json};

struct Fixture(PathBuf);

impl Fixture {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "blaze-cold-resolution-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed),
        ));
        std::fs::create_dir(&path).unwrap();
        let fixture = Self(path);
        fixture.write("src/main.rs", "fn main() {}\n");
        fixture.app("");
        fixture
    }

    fn write(&self, path: &str, text: &str) {
        let path = self.0.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    fn app(&self, deps: &str) {
        self.write(
            "Cargo.toml",
            &format!(
                "[package]\nname='app'\nversion='0.1.0'\nedition='2021'\n[dependencies]\n{deps}\n",
            ),
        );
    }

    fn registry(&self) {
        std::fs::create_dir_all(self.0.join("registry/index")).unwrap();
        self.write(
            ".cargo/config.toml",
            &format!(
                "[source.crates-io]\nreplace-with='local'\n[source.local]\nlocal-registry={:?}\n",
                self.0.join("registry"),
            ),
        );
    }

    fn publish(&self, name: &str, version: &str, deps: &[(&str, &str)], yanked: bool) {
        let dependencies =
            deps.iter().map(|(name, req)| format!("{name}={req:?}\n")).collect::<String>();
        let manifest = format!(
            "[package]\nname={name:?}\nversion={version:?}\nedition='2021'\n[dependencies]\n{dependencies}",
        );
        let mut tar = tar::Builder::new(Vec::new());
        for (path, text) in [("Cargo.toml", manifest.as_str()), ("src/lib.rs", "pub fn f() {}\n")] {
            let mut header = tar::Header::new_gnu();
            header.set_size(text.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            tar.append_data(&mut header, format!("{name}-{version}/{path}"), text.as_bytes())
                .unwrap();
        }
        let mut zip = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        zip.write_all(&tar.into_inner().unwrap()).unwrap();
        let bytes = zip.finish().unwrap();
        let checksum = blaze_registry::download::checksum(&bytes);
        std::fs::write(self.0.join(format!("registry/{name}-{version}.crate")), bytes).unwrap();
        let index =
            self.0.join("registry/index").join(blaze_registry::index::dep_path(name, false));
        std::fs::create_dir_all(index.parent().unwrap()).unwrap();
        let line = json!({
            "name": name, "vers": version, "deps": deps.iter().map(|(name, req)| json!({
                "name": name, "req": req, "features": [], "optional": false,
                "default_features": true, "kind": "normal", "target": null,
            })).collect::<Vec<_>>(), "features": {}, "cksum": checksum, "yanked": yanked,
        });
        writeln!(
            std::fs::OpenOptions::new().create(true).append(true).open(index).unwrap(),
            "{line}"
        )
        .unwrap();
    }

    fn resolve(
        &self,
        locked: bool,
        offline: bool,
    ) -> anyhow::Result<(Value, Vec<(PathBuf, Vec<u8>)>)> {
        let asked = Asked {
            cargo_home: None,
            cwd: &self.0,
            env: vec![("CARGO_HOME".into(), self.0.join("home").to_str().unwrap().into())],
            manifest_path: None,
            config: &[],
            features: &[],
            all_features: false,
            no_default_features: false,
            build: None,
            platform: None,
            offline,
            locked,
            store: None,
        };
        let (json, sources) = blaze_metadata::metadata_with_sources(&asked)?;
        Ok((serde_json::from_str(&json)?, sources))
    }

    fn lock(&self) -> String {
        std::fs::read_to_string(self.0.join("Cargo.lock")).unwrap()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn version(metadata: &Value, name: &str) -> String {
    metadata["packages"].as_array().unwrap().iter().find(|p| p["name"] == name).unwrap()["version"]
        .as_str()
        .unwrap()
        .into()
}

#[test]
fn fresh_path_graph_writes_and_certifies_its_lock() {
    let fixture = Fixture::new();
    fixture.app("dep={path='dep'}");
    fixture.write("dep/Cargo.toml", "[package]\nname='dep'\nversion='0.1.0'\nedition='2021'\n");
    fixture.write("dep/src/lib.rs", "pub fn f() {}\n");
    let (metadata, sources) = fixture.resolve(false, true).unwrap();
    assert_eq!(metadata["packages"].as_array().unwrap().len(), 2);
    let lock = fixture.lock();
    assert_eq!(sources.iter().filter(|(p, _)| *p == fixture.0.join("Cargo.lock")).count(), 1);
    assert!(sources.contains(&(fixture.0.join("Cargo.lock"), lock.as_bytes().to_vec())));
    fixture.resolve(true, true).unwrap();
    assert_eq!(fixture.lock(), lock);
    fixture.write("dep/Cargo.toml", "[package]\nname='dep'\nversion='0.2.0'\nedition='2021'\n");
    assert!(fixture.resolve(true, true).unwrap_err().to_string().contains("cannot update"));
    assert_eq!(fixture.lock(), lock);
    let (_, sources) = fixture.resolve(false, true).unwrap();
    assert!(fixture.lock().contains("0.2.0"));
    let locks: Vec<_> =
        sources.iter().filter(|(p, _)| *p == fixture.0.join("Cargo.lock")).collect();
    assert_eq!(locks.len(), 1);
    assert_eq!(locks[0].1, fixture.lock().as_bytes());
}

#[test]
fn missing_malformed_and_unreadable_locks_are_distinct() {
    let fixture = Fixture::new();
    assert!(fixture.resolve(true, true).unwrap_err().to_string().contains("cannot create"));
    assert!(!fixture.0.join("Cargo.lock").exists());
    fixture.write("Cargo.lock", "invalid [lock\n");
    assert!(fixture.resolve(false, true).unwrap_err().to_string().contains("failed to parse"));
    assert_eq!(fixture.lock(), "invalid [lock\n");
    std::fs::remove_file(fixture.0.join("Cargo.lock")).unwrap();
    std::fs::create_dir(fixture.0.join("Cargo.lock")).unwrap();
    assert!(fixture.resolve(false, true).unwrap_err().to_string().contains("failed to read"));
}

#[test]
fn registry_versions_transitives_and_previous_preferences() {
    let fixture = Fixture::new();
    fixture.registry();
    fixture.publish("leaf", "1.0.0", &[], false);
    fixture.publish("leaf", "2.0.0", &[], false);
    fixture.publish("middle", "1.0.0", &[("leaf", "1")], false);
    fixture.publish("middle", "1.1.0", &[("leaf", "2")], false);
    fixture.publish("middle", "1.2.0", &[], true);
    fixture.app("middle='1'");
    let (metadata, _) = fixture.resolve(false, true).unwrap();
    assert_eq!(version(&metadata, "middle"), "1.1.0");
    assert_eq!(version(&metadata, "leaf"), "2.0.0");
    assert!(fixture.lock().contains("registry+https://github.com/rust-lang/crates.io-index"));
    fixture.publish("middle", "1.3.0", &[("leaf", "1")], false);
    let lock = fixture.lock();
    assert_eq!(version(&fixture.resolve(false, true).unwrap().0, "middle"), "1.1.0");
    assert_eq!(fixture.lock(), lock);
    fixture.app("middle='1.3'");
    assert!(fixture.resolve(true, true).is_err());
    assert_eq!(fixture.lock(), lock);
    let (metadata, _) = fixture.resolve(false, true).unwrap();
    assert_eq!(version(&metadata, "middle"), "1.3.0");
    assert_eq!(version(&metadata, "leaf"), "1.0.0");
}

#[test]
fn offline_local_registry_reports_missing_archive_after_writing_lock() {
    let fixture = Fixture::new();
    fixture.registry();
    fixture.publish("dep", "1.0.0", &[], false);
    fixture.publish("dep", "1.1.0", &[], false);
    std::fs::remove_file(fixture.0.join("registry/dep-1.1.0.crate")).unwrap();
    fixture.app("dep='1'");
    assert!(fixture.resolve(false, true).is_err());
    assert!(fixture.lock().contains("version = \"1.1.0\""));
}

#[test]
fn registry_resolution_backtracks_across_transitive_constraints() {
    let fixture = Fixture::new();
    fixture.registry();
    fixture.publish("unified", "1.0.0", &[], false);
    fixture.publish("unified", "1.1.0", &[], false);
    fixture.publish("branch", "1.0.0", &[("unified", "=1.0.0")], false);
    fixture.publish("branch", "1.1.0", &[("unified", "=1.1.0")], false);
    fixture.app("branch='1'\nunified='=1.0.0'");
    let (metadata, _) = fixture.resolve(false, true).unwrap();
    assert_eq!(version(&metadata, "branch"), "1.0.0");
    assert_eq!(version(&metadata, "unified"), "1.0.0");
}

#[test]
fn read_permission_failures_are_not_treated_as_missing_locks() {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let fixture = Fixture::new();
        fixture.write("Cargo.lock", "version = 4\n");
        let lock = fixture.0.join("Cargo.lock");
        std::fs::set_permissions(&lock, std::fs::Permissions::from_mode(0)).unwrap();
        if std::fs::read(&lock).is_ok() {
            std::fs::set_permissions(&lock, std::fs::Permissions::from_mode(0o600)).unwrap();
            return;
        }
        let result = fixture.resolve(false, true);
        std::fs::set_permissions(&lock, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(result.unwrap_err().to_string().contains("failed to read"));
        assert_eq!(fixture.lock(), "version = 4\n");
    }
}
