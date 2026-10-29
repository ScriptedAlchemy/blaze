//! Inactive packages need resolver facts, not compilable source archives.

use std::{
    path::PathBuf,
    time::{SystemTime, UNIX_EPOCH},
};

use blaze_metadata::units::{Command, TargetFilter, UnitsAsked, unit_graph_captured};
use blaze_metadata::{Asked, Build, CapturedResolution, TargetPlatform};

struct Fixture(PathBuf);
impl Fixture {
    fn write(&self, path: &str, bytes: impl AsRef<[u8]>) {
        let path = self.0.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, bytes).unwrap();
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn inactive_archive_and_manifest_are_unneeded_for_capture_profile_validation_and_closed_replay() {
    let nonce = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
    let f = Fixture(
        std::env::temp_dir().join(format!("blaze-resolver-facts-{}-{nonce}", std::process::id())),
    );
    std::fs::create_dir(&f.0).unwrap();
    f.write("Cargo.toml", "[package]\nname='app'\nversion='0.1.0'\nedition='2024'\n[dependencies]\nunused={version='1',optional=true}\n[profile.dev.package.unused]\nopt-level=2\n");
    f.write("src/lib.rs", "pub fn value() -> u8 { 1 }\n");
    f.write(
        ".cargo/config.toml",
        format!(
            "[source.crates-io]\nreplace-with='local'\n[source.local]\nlocal-registry={:?}\n",
            f.0.join("registry")
        ),
    );
    f.write("registry/index/un/us/unused", format!("{{\"name\":\"unused\",\"vers\":\"1.0.0\",\"deps\":[],\"features\":{{}},\"cksum\":\"{}\",\"yanked\":false,\"rust_version\":\"1.90\"}}\n", "ab".repeat(32)));
    let home = f.0.join("home");
    let asked = Asked {
        cargo_home: Some(home.clone()),
        cwd: &f.0,
        env: vec![],
        manifest_path: None,
        config: &[],
        features: &[],
        all_features: false,
        no_default_features: false,
        build: Some(Build { packages: &[], workspace: false, excludes: &[], dev_units: false }),
        platform: Some(TargetPlatform { triple: "x86_64-unknown-linux-gnu", cfg: &[] }),
        offline: true,
        locked: false,
        store: None,
        rust_release: Some(&|_, _, _| Some("1.99.0".to_owned())),
    };
    let units = UnitsAsked {
        asked: &asked,
        command: Command::Build,
        targets: &TargetFilter::default(),
        profile: None,
    };
    let original = unit_graph_captured(&units, None, None, None).unwrap();
    assert_eq!(original.graph.units.len(), 1);
    assert!(original.sources.is_empty());
    assert_eq!(original.facts.packages.len(), 1);
    assert_eq!(original.facts.packages[0].name, "unused");
    assert_eq!(original.facts.packages[0].rust_version.as_deref(), Some("1.90"));
    assert!(original.inputs.iter().all(|(path, _)| !path.starts_with(f.0.join("registry"))));
    assert!(!f.0.join("registry/unused-1.0.0.crate").exists());
    std::fs::remove_dir_all(f.0.join("registry")).unwrap();
    if home.exists() {
        std::fs::remove_dir_all(&home).unwrap();
    }
    let replay = unit_graph_captured(
        &units,
        Some(CapturedResolution { sources: &[], facts: &original.facts }),
        None,
        None,
    )
    .unwrap();
    assert_eq!(original.graph.units, replay.graph.units);
    assert_eq!(original.graph.roots, replay.graph.roots);
    assert_eq!(original.graph.lock, replay.graph.lock);
    assert_eq!(original.facts, replay.facts);
    assert!(!home.exists(), "closed replay must not consult or create a source cache");

    let unknown_release = Asked { rust_release: None, ..asked };
    let unknown_units = UnitsAsked {
        asked: &unknown_release,
        command: Command::Build,
        targets: &TargetFilter::default(),
        profile: None,
    };
    let error = unit_graph_captured(
        &unknown_units,
        Some(CapturedResolution { sources: &[], facts: &original.facts }),
        None,
        None,
    )
    .unwrap_err();
    assert!(error.to_string().contains("no workspace member declares a `rust-version`"));
    assert!(!home.exists(), "missing toolchain facts must not consult a source cache");
}
