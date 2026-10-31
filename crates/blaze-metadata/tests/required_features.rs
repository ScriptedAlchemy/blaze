//! Target selection must use the resolved dependency features, as Cargo 1.99 does.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use blaze_metadata::{Asked, Build, TargetPlatform, Targets};

struct Fixture(PathBuf);

impl Fixture {
    fn new(dependency: &str, required: &str, extra: &str) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "blaze-required-features-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed),
        ));
        std::fs::create_dir(&path).unwrap();
        let fixture = Self(path);
        fixture.write(
            "Cargo.toml",
            &format!(
                "[package]\nname='app'\nversion='0.1.0'\nedition='2021'\n\
                 [[bin]]\nname='app'\npath='src/main.rs'\nrequired-features=['{required}']\n\
                 {dependency}\n{extra}\n",
            ),
        );
        fixture.write("src/main.rs", "fn main() {}\n");
        fixture.write(
            "dep/Cargo.toml",
            "[package]\nname='dep'\nversion='0.1.0'\nedition='2021'\n[features]\nextra=[]\n",
        );
        fixture.write("dep/src/lib.rs", "pub fn f() {}\n");
        fixture
    }

    fn write(&self, path: &str, text: &str) {
        let path = self.0.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    fn asked<'a>(&'a self, features: &'a [String]) -> Asked<'a> {
        Asked {
            cwd: &self.0,
            env: vec![("CARGO_HOME".into(), self.0.join("home").to_str().unwrap().into())],
            manifest_path: None,
            config: &[],
            features,
            all_features: false,
            no_default_features: false,
            build: Some(Build { packages: &[], workspace: false, excludes: &[], dev_units: false }),
            platform: None,
            offline: true,
            locked: false,
            store: None,
            rust_release: None,
        }
    }

    fn assert_selected(&self, features: &[String], selected: bool) {
        let asked = self.asked(features);
        let host = TargetPlatform { triple: "x86_64-unknown-linux-gnu", cfg: &[] };
        let (json, units, _) = blaze_metadata::build_units(&asked, &host).unwrap();
        let metadata: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(!metadata["resolve"]["nodes"].as_array().unwrap().is_empty(), selected);
        assert_eq!(!units.is_empty(), selected);
        let (_, programs) = blaze_metadata::programs(&asked, Targets::default()).unwrap();
        assert_eq!(programs.len(), usize::from(selected));
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn dependency_feature_gates_program_metadata_and_owned_units() {
    let fixture = Fixture::new("[dependencies]\ndep={path='dep'}", "dep/extra", "");
    fixture.assert_selected(&[], false);
    fixture.assert_selected(&["dep/extra".into()], true);
    fixture.assert_selected(&[], false);
}

#[test]
fn renamed_optional_dependency_must_be_enabled_with_its_feature() {
    let fixture = Fixture::new(
        "[dependencies]\nrenamed={package='dep',path='dep',optional=true}",
        "renamed/extra",
        "[features]\nenable=['dep:renamed']\nfull=['renamed/extra']",
    );
    fixture.assert_selected(&[], false);
    fixture.assert_selected(&["enable".into()], false);
    fixture.assert_selected(&["full".into()], true);
}

#[test]
fn explicit_target_reports_missing_dependency_features_even_without_any_build_units() {
    let fixture = Fixture::new("[dependencies]\ndep={path='dep'}", "dep/extra", "");
    let error =
        blaze_metadata::programs(&fixture.asked(&[]), Targets { bin: Some("app"), example: None })
            .unwrap_err();
    assert!(error.to_string().contains("requires the features: `dep/extra`"), "{error:#}");
}

#[test]
fn library_does_not_enable_its_feature_gated_binary() {
    let fixture = Fixture::new("[dependencies]\ndep={path='dep'}", "dep/extra", "");
    fixture.write("src/lib.rs", "pub fn lib() {}\n");
    let asked = fixture.asked(&[]);
    let host = TargetPlatform { triple: "x86_64-unknown-linux-gnu", cfg: &[] };
    let (_, units, _) = blaze_metadata::build_units(&asked, &host).unwrap();
    assert!(!units.is_empty());
    assert!(blaze_metadata::programs(&asked, Targets::default()).unwrap().1.is_empty());
}

#[test]
fn required_feature_can_come_from_an_enabled_build_dependency() {
    let fixture =
        Fixture::new("[build-dependencies]\ndep={path='dep',features=['extra']}", "dep/extra", "");
    fixture.write("build.rs", "fn main() {}\n");
    fixture.assert_selected(&[], true);
}

#[test]
fn proc_macro_dependency_features_come_from_the_host_domain() {
    let fixture = Fixture::new("[dependencies]\ndep={path='dep'}", "dep/extra", "");
    fixture.write(
        "dep/Cargo.toml",
        "[package]\nname='dep'\nversion='0.1.0'\nedition='2021'\n[lib]\nproc-macro=true\n[features]\nextra=[]\n",
    );
    fixture.assert_selected(&[], false);
    fixture.assert_selected(&["dep/extra".into()], true);
}

#[test]
fn inactive_target_dependency_does_not_enable_a_binary() {
    let fixture = Fixture::new(
        "[target.'cfg(target_os = \"windows\")'.dependencies]\ndep={path='dep',features=['extra']}",
        "dep/extra",
        "",
    );
    let cfg = ["target_os=\"linux\"".parse().unwrap()];
    let mut asked = fixture.asked(&[]);
    asked.platform = Some(TargetPlatform { triple: "x86_64-unknown-linux-gnu", cfg: &cfg });
    let host = TargetPlatform { triple: "x86_64-unknown-linux-gnu", cfg: &cfg };
    let (_, units, _) = blaze_metadata::build_units(&asked, &host).unwrap();
    assert!(units.is_empty());
    assert!(blaze_metadata::programs(&asked, Targets::default()).unwrap().1.is_empty());
}

#[test]
fn cross_target_programs_and_metadata_use_host_dependency_features() {
    let fixture = Fixture::new(
        "[target.'cfg(target_os = \"linux\")'.build-dependencies]\ndep={path='dep',features=['extra']}",
        "dep/extra",
        "",
    );
    fixture.write("build.rs", "fn main() {}\n");
    let host_cfg = ["target_os=\"linux\"".parse().unwrap()];
    let target_cfg = ["target_os=\"windows\"".parse().unwrap()];
    let host = TargetPlatform { triple: "x86_64-unknown-linux-gnu", cfg: &host_cfg };
    let mut asked = fixture.asked(&[]);
    asked.platform = Some(TargetPlatform { triple: "x86_64-pc-windows-gnu", cfg: &target_cfg });
    let (_, units, _) = blaze_metadata::build_units(&asked, &host).unwrap();
    assert!(!units.is_empty());
    let (_, programs) =
        blaze_metadata::programs_for_host(&asked, Targets::default(), &host).unwrap();
    assert_eq!(programs.len(), 1);
    let (json, _) = blaze_metadata::metadata_with_sources_for_host(&asked, &host).unwrap();
    let metadata: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert!(!metadata["resolve"]["nodes"].as_array().unwrap().is_empty());
}
