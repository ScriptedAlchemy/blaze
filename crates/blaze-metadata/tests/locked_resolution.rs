//! The locked contract compares dependency graphs, independent of TOML formatting.

use std::path::{Path, PathBuf};

use blaze_metadata::Asked;

struct Scratch(PathBuf);

impl Scratch {
    fn new() -> Self {
        let path = std::env::temp_dir()
            .join(format!("blaze-metadata-compact-lock-{}", std::process::id()));
        std::fs::create_dir(&path).unwrap();
        Self(path)
    }

    fn write(&self, path: &str, text: &str) {
        let path = self.0.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn metadata(root: &Path) -> anyhow::Result<(String, Vec<(PathBuf, Vec<u8>)>)> {
    let home = root.join("home");
    let asked = Asked {
        cwd: root,
        env: vec![("CARGO_HOME".into(), home.to_str().unwrap().into())],
        manifest_path: None,
        config: &[],
        features: &[],
        all_features: false,
        no_default_features: false,
        build: None,
        platform: None,
        offline: true,
        locked: true,
        store: None,
    };
    blaze_metadata::metadata_with_sources(&asked)
}

#[test]
fn locked_resolution_accepts_equivalent_toml_but_rejects_changed_edges() {
    let scratch = Scratch::new();
    scratch.write(
        "Cargo.toml",
        "[package]\nname='app'\nversion='0.1.0'\nedition='2021'\n[dependencies]\ndep={path='dep'}\n",
    );
    scratch.write("src/main.rs", "fn main() { dep::value(); }\n");
    scratch.write("dep/Cargo.toml", "[package]\nname='dep'\nversion='0.1.0'\nedition='2021'\n");
    scratch.write("dep/src/lib.rs", "pub fn value() {}\n");
    // Cargo --locked accepts this ordering, compact array and missing generated header.
    let text = "version = 4\n[[package]]\nname='dep'\nversion='0.1.0'\n[[package]]\nname='app'\nversion='0.1.0'\ndependencies=['dep']\n";
    scratch.write("Cargo.lock", text);
    let (json, sources) = metadata(&scratch.0).unwrap();
    let lock_path = scratch.0.join("Cargo.lock");
    assert_eq!(std::fs::read_to_string(&lock_path).unwrap(), text);
    assert!(sources.contains(&(lock_path.clone(), text.as_bytes().to_vec())));
    let json: serde_json::Value = serde_json::from_str(&json).unwrap();
    let packages = json["packages"].as_array().unwrap();
    let app = packages.iter().find(|package| package["name"] == "app").unwrap();
    let dep = packages.iter().find(|package| package["name"] == "dep").unwrap();
    let nodes = json["resolve"]["nodes"].as_array().unwrap();
    let app_node = nodes.iter().find(|node| node["id"] == app["id"]).unwrap();
    assert_eq!(app_node["dependencies"], serde_json::json!([dep["id"]]));

    let changed = text.replace("dependencies=['dep']\n", "");
    scratch.write("Cargo.lock", &changed);
    let error = metadata(&scratch.0).unwrap_err();
    assert!(format!("{error:#}").contains("cannot update the lock file"), "{error:#}");
    assert_eq!(std::fs::read_to_string(&lock_path).unwrap(), changed);
}
