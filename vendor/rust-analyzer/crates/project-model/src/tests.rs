use std::env::temp_dir;

use base_db::{CrateGraphBuilder, ProcMacroPaths, TargetKind};
use cargo_metadata::Metadata;
use cfg::{CfgAtom, CfgDiff};
use expect_test::{ExpectFile, expect_file};
use intern::sym;
use paths::{AbsPath, AbsPathBuf, Utf8Path, Utf8PathBuf};
use rustc_hash::FxHashMap;
use serde::de::DeserializeOwned;
use span::FileId;

use crate::{
    CargoWorkspace, CfgOverrides, ManifestPath, ProjectJson, ProjectJsonData, ProjectWorkspace,
    RustSourceWorkspaceConfig, Sysroot, WorkspaceBuildScripts, sysroot::RustLibSrcWorkspace,
    workspace::ProjectWorkspaceKind,
};

fn load_cargo(file: &str) -> (CrateGraphBuilder, ProcMacroPaths) {
    let project_workspace = load_workspace_from_metadata(file);
    to_crate_graph(project_workspace, &mut Default::default())
}

fn load_cargo_with_overrides(
    file: &str,
    cfg_overrides: CfgOverrides,
) -> (CrateGraphBuilder, ProcMacroPaths) {
    let project_workspace =
        ProjectWorkspace { cfg_overrides, ..load_workspace_from_metadata(file) };
    to_crate_graph(project_workspace, &mut Default::default())
}

fn load_workspace_from_metadata(file: &str) -> ProjectWorkspace {
    let meta: Metadata = get_test_json_file(file);
    let manifest_path =
        ManifestPath::try_from(AbsPathBuf::try_from(meta.workspace_root.clone()).unwrap()).unwrap();
    let cargo_workspace = CargoWorkspace::new(meta, manifest_path, Default::default(), false);
    ProjectWorkspace {
        kind: ProjectWorkspaceKind::Cargo {
            cargo: cargo_workspace,
            build_scripts: WorkspaceBuildScripts::default(),
            rustc: Err(None),
            error: None,
        },
        cfg_overrides: Default::default(),
        sysroot: Sysroot::empty(),
        rustc_cfg: Vec::new(),
        toolchain: None,
        target: Err("target_data_layout not loaded".into()),
        extra_includes: Vec::new(),
        set_test: true,
    }
}

fn load_rust_project(file: &str) -> (CrateGraphBuilder, ProcMacroPaths) {
    let data = get_test_json_file(file);
    to_crate_graph(rust_project_workspace(data), &mut Default::default())
}

fn rust_project_workspace(data: ProjectJsonData) -> ProjectWorkspace {
    let project = rooted_project_json(data);
    let sysroot = Sysroot::empty();
    ProjectWorkspace {
        kind: ProjectWorkspaceKind::Json(project),
        sysroot,
        rustc_cfg: Vec::new(),
        toolchain: None,
        target: Err("test has no target data".into()),
        cfg_overrides: Default::default(),
        extra_includes: Vec::new(),
        set_test: true,
    }
}

fn get_test_json_file<T: DeserializeOwned>(file: &str) -> T {
    let file = get_test_path(file);
    let data = std::fs::read_to_string(file).unwrap();
    let mut json = data.parse::<serde_json::Value>().unwrap();
    fixup_paths(&mut json);
    return serde_json::from_value(json).unwrap();

    fn fixup_paths(val: &mut serde_json::Value) {
        match val {
            serde_json::Value::String(s) => replace_root(s, true),
            serde_json::Value::Array(vals) => vals.iter_mut().for_each(fixup_paths),
            serde_json::Value::Object(kvals) => kvals.values_mut().for_each(fixup_paths),
            serde_json::Value::Null | serde_json::Value::Bool(_) | serde_json::Value::Number(_) => {
            }
        }
    }
}

fn replace_cargo(s: &mut String) {
    let path = toolchain::Tool::Cargo.path().to_string().escape_debug().collect::<String>();
    *s = s.replace(&path, "$CARGO$");
}

fn replace_root(s: &mut String, direction: bool) {
    if direction {
        let root = if cfg!(windows) { r#"C:\\ROOT\"# } else { "/ROOT/" };
        *s = s.replace("$ROOT$", root)
    } else {
        let root = if cfg!(windows) { r#"C:\\\\ROOT\\"# } else { "/ROOT/" };
        *s = s.replace(root, "$ROOT$")
    }
}

fn get_test_path(file: &str) -> Utf8PathBuf {
    let base = Utf8PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    base.join("test_data").join(file)
}

fn rooted_project_json(data: ProjectJsonData) -> ProjectJson {
    let mut root = "$ROOT$".to_owned();
    replace_root(&mut root, true);
    let path = Utf8Path::new(&root);
    let base = AbsPath::assert(path);
    ProjectJson::new(None, base, data)
}

fn to_crate_graph(
    project_workspace: ProjectWorkspace,
    file_map: &mut FxHashMap<AbsPathBuf, FileId>,
) -> (CrateGraphBuilder, ProcMacroPaths) {
    to_crate_graph_with_env(project_workspace, file_map, &Default::default())
}

fn to_crate_graph_with_env(
    project_workspace: ProjectWorkspace,
    file_map: &mut FxHashMap<AbsPathBuf, FileId>,
    extra_env: &FxHashMap<String, Option<String>>,
) -> (CrateGraphBuilder, ProcMacroPaths) {
    project_workspace.to_crate_graph(
        &mut {
            |path| {
                let len = file_map.len() + 1;
                Some(*file_map.entry(path.to_path_buf()).or_insert(FileId::from_raw(len as u32)))
            }
        },
        extra_env,
    )
}

fn check_crate_graph(crate_graph: CrateGraphBuilder, expect: ExpectFile) {
    let mut crate_graph = format!("{crate_graph:#?}");

    replace_root(&mut crate_graph, false);
    replace_cargo(&mut crate_graph);
    expect.assert_eq(&crate_graph);
}

#[test]
fn cargo_hello_world_project_model_with_wildcard_overrides() {
    let cfg_overrides = CfgOverrides {
        global: CfgDiff::new(Vec::new(), vec![CfgAtom::Flag(sym::test)]),
        selective: Default::default(),
    };
    let (crate_graph, _proc_macros) =
        load_cargo_with_overrides("hello-world-metadata.json", cfg_overrides);
    check_crate_graph(
        crate_graph,
        expect_file![
            "../test_data/output/cargo_hello_world_project_model_with_wildcard_overrides.txt"
        ],
    )
}

#[test]
fn cargo_hello_world_project_model_with_selective_overrides() {
    let cfg_overrides = CfgOverrides {
        global: Default::default(),
        selective: std::iter::once((
            "libc".to_owned(),
            CfgDiff::new(Vec::new(), vec![CfgAtom::Flag(sym::test)]),
        ))
        .collect(),
    };
    let (crate_graph, _proc_macros) =
        load_cargo_with_overrides("hello-world-metadata.json", cfg_overrides);
    check_crate_graph(
        crate_graph,
        expect_file![
            "../test_data/output/cargo_hello_world_project_model_with_selective_overrides.txt"
        ],
    )
}

#[test]
fn cargo_hello_world_project_model() {
    let (crate_graph, _proc_macros) = load_cargo("hello-world-metadata.json");
    check_crate_graph(
        crate_graph,
        expect_file!["../test_data/output/cargo_hello_world_project_model.txt"],
    )
}

#[test]
fn rust_project_hello_world_project_model() {
    let (crate_graph, _proc_macros) = load_rust_project("hello-world-project.json");
    check_crate_graph(
        crate_graph,
        expect_file!["../test_data/output/rust_project_hello_world_project_model.txt"],
    );
}

#[test]
fn rust_project_labeled_project_model() {
    let (crate_graph, _) = load_rust_project("labeled-project.json");
    let krate = &crate_graph[crate_graph.iter().next().unwrap()];
    assert_eq!(krate.basic.target_kind, TargetKind::Bin);
    assert!(krate.cfg_options.check_atom(&CfgAtom::Flag(sym::rust_analyzer)));
    assert!(krate.cfg_options.check_atom(&CfgAtom::Flag(sym::test)));
}

fn compiler_project() -> serde_json::Value {
    let mut crates = Vec::new();
    for (name, kind, is_proc_macro, cfg) in [
        ("program", "bin", false, vec!["saved", "panic=\"abort\""]),
        ("tests", "test", false, vec!["saved", "test", "panic=\"unwind\""]),
        ("library", "lib", false, vec![]),
        ("macros", "lib", true, vec!["saved", "proc_macro"]),
    ] {
        crates.push(serde_json::json!({
            "display_name": name,
            "root_module": format!("{name}.rs"),
            "edition": "2021",
            "deps": [],
            "cfg": cfg,
            "codegen_flags": ["-Copt-level=3"],
            "env": {"DECLARED": "saved", "CARGO_UNSTABLE_CHECKSUM_FRESHNESS": "captured"},
            "is_workspace_member": true,
            "is_proc_macro": is_proc_macro,
            "build": {"label": name, "build_file": "BUILD", "target_kind": kind},
        }));
    }
    serde_json::json!({"compiler_mode": true, "crates": crates})
}

#[test]
fn compiler_unit_identity_preserves_distinct_instances_of_the_same_source() {
    let mut json = compiler_project();
    let mut shared = json["crates"][2].clone();
    shared["root_module"] = "common.rs".into();
    json["crates"] = serde_json::Value::Array(
        (0..3)
            .map(|id| {
                let mut krate = shared.clone();
                krate["blaze_native_unit"] = id.into();
                krate
            })
            .collect(),
    );
    let workspace = rust_project_workspace(serde_json::from_value(json).unwrap());
    let (graph, _) = to_crate_graph(workspace, &mut Default::default());
    let units: Vec<_> = graph.iter().map(|id| graph[id].extra.blaze_native_unit).collect();
    assert_eq!(units, [Some(0), Some(1), Some(2)]);
    let files: std::collections::HashSet<_> =
        graph.iter().map(|id| graph[id].basic.root_file_id).collect();
    assert_eq!(files.len(), 1);
}

#[test]
fn compiler_project_preserves_exact_cfg_env_and_target_kinds() {
    let mut json = compiler_project();
    // Compiler cfgs are already complete for this target. Loading the graph must
    // not ask another compiler for a replacement target configuration.
    json["crates"][2]["target"] = "saved-target-with-no-external-query".into();
    for set_test in [false, true] {
        let mut workspace = rust_project_workspace(serde_json::from_value(json.clone()).unwrap());
        workspace.set_test = set_test;
        workspace.rustc_cfg = ["panic=\"unwind\"", "debug_assertions", "overflow_checks"]
            .map(|cfg| crate::parse_cfg(cfg).unwrap())
            .into();
        workspace.cfg_overrides = CfgOverrides {
            global: CfgDiff::new(
                vec![crate::parse_cfg("editor_global").unwrap()],
                vec![crate::parse_cfg("saved").unwrap()],
            ),
            selective: FxHashMap::from_iter([(
                "program".into(),
                CfgDiff::new(vec![crate::parse_cfg("editor_selective").unwrap()], vec![]),
            )]),
        };
        let extra_env = FxHashMap::from_iter([
            ("DECLARED".into(), Some("editor override".into())),
            ("CARGO_UNSTABLE_CHECKSUM_FRESHNESS".into(), Some("false".into())),
            ("EDITOR_ONLY".into(), Some("not a source input".into())),
        ]);
        let (graph, _) = to_crate_graph_with_env(workspace, &mut Default::default(), &extra_env);
        assert_eq!(graph.iter().count(), 4);
        for id in graph.iter() {
            let krate = &graph[id];
            let name = krate.extra.display_name.as_ref().unwrap().canonical_name().as_str();
            let (kind, cfg): (_, &[_]) = match name {
                "program" => (TargetKind::Bin, &["saved", "panic=\"abort\""]),
                "tests" => (TargetKind::Test, &["saved", "test", "panic=\"unwind\""]),
                "library" => (TargetKind::Lib { is_proc_macro: false }, &[]),
                "macros" => (TargetKind::Lib { is_proc_macro: true }, &["saved", "proc_macro"]),
                _ => unreachable!(),
            };
            let expected = cfg.iter().map(|it| crate::parse_cfg(it).unwrap()).collect();
            assert_eq!(krate.cfg_options, expected, "{name}");
            assert_eq!(krate.basic.target_kind, kind, "{name}");
            assert_eq!(krate.basic.codegen_flags, ["-Copt-level=3"], "{name}");
            assert_eq!(krate.env.get("DECLARED").as_deref(), Some("saved"), "{name}");
            assert_eq!(
                krate.env.get("CARGO_UNSTABLE_CHECKSUM_FRESHNESS").as_deref(),
                Some("captured"),
                "{name}"
            );
            assert!(!krate.env.contains_key("EDITOR_ONLY"), "{name}");
        }
    }
}

#[test]
fn compiler_project_preserves_non_unicode_environment_as_present_but_not_text() {
    let mut json = compiler_project();
    json["crates"][0]["blaze_invalid_env"] = serde_json::json!(["NATIVE_VALUE"]);
    let (graph, _) = to_crate_graph(
        rust_project_workspace(serde_json::from_value(json).unwrap()),
        &mut Default::default(),
    );
    let root = graph
        .iter()
        .find(|&id| {
            graph[id].extra.display_name.as_ref().unwrap().canonical_name().as_str() == "program"
        })
        .unwrap();
    let env = &graph[root].env;
    assert!(env.contains_key("NATIVE_VALUE"));
    assert!(env.is_non_unicode("NATIVE_VALUE"));
    assert_eq!(env.get("NATIVE_VALUE"), None);
    assert!(!env.is_non_unicode("ABSENT"));
    let mut overlaid = env.clone();
    overlaid.set("NATIVE_VALUE", "text override");
    assert!(!overlaid.is_non_unicode("NATIVE_VALUE"));
    assert_eq!(overlaid.get("NATIVE_VALUE").as_deref(), Some("text override"));
    assert_ne!(format!("{env:?}"), format!("{:?}", base_db::Env::default()));
}

#[test]
fn compiler_project_accepts_cargo_cfg_whitespace() {
    let mut json = compiler_project();
    json["crates"][0]["cfg"] = serde_json::json!([
        "target_abi = \"\"",
        " \ttarget_os\t=\t\"linux\" \n",
        "custom = \" a = b \"",
        " \tunix \n",
    ]);
    let data: ProjectJsonData = serde_json::from_value(json).unwrap();
    let roundtrip = serde_json::to_value(&data).unwrap();
    assert_eq!(
        roundtrip["crates"][0]["cfg"],
        serde_json::json!(["target_abi=\"\"", "target_os=\"linux\"", "custom=\" a = b \"", "unix"]),
    );
    assert_eq!(serde_json::from_value::<ProjectJsonData>(roundtrip).unwrap(), data);
    let (graph, _) = to_crate_graph(rust_project_workspace(data), &mut Default::default());
    let program = graph.iter().find(|&id| graph[id].basic.target_kind == TargetKind::Bin).unwrap();
    let expected = ["target_abi=\"\"", "target_os=\"linux\"", "custom=\" a = b \"", "unix"]
        .map(|cfg| crate::parse_cfg(cfg).unwrap())
        .into_iter()
        .collect();
    assert_eq!(graph[program].cfg_options, expected);
}

#[test]
fn compiler_project_rejects_malformed_cfg_values() {
    for cfg in [
        "target_abi = ",
        "target_abi = \"",
        "target_abi=\"",
        "target_abi = linux",
        "target_abi = \"linux",
        "target_abi = linux\"",
        "target_abi = \"linux\" trailing",
    ] {
        let mut json = compiler_project();
        json["crates"][0]["cfg"] = serde_json::json!([cfg]);
        let error = serde_json::from_value::<ProjectJsonData>(json).unwrap_err();
        assert!(error.to_string().contains("value should be in quotes"), "{cfg}: {error}");
    }
}

#[test]
fn analyzer_project_retains_cfg_and_environment_overrides() {
    let mut json = compiler_project();
    json.as_object_mut().unwrap().remove("compiler_mode");
    let mut workspace = rust_project_workspace(serde_json::from_value(json).unwrap());
    workspace.rustc_cfg = vec![crate::parse_cfg("target_default").unwrap()];
    workspace.cfg_overrides.global = CfgDiff::new(
        vec![crate::parse_cfg("editor_global").unwrap()],
        vec![crate::parse_cfg("saved").unwrap()],
    );
    let extra_env = FxHashMap::from_iter([("DECLARED".into(), Some("editor override".into()))]);
    let (graph, _) = to_crate_graph_with_env(workspace, &mut Default::default(), &extra_env);
    for id in graph.iter() {
        let krate = &graph[id];
        for flag in ["rust_analyzer", "test", "target_default", "editor_global"] {
            assert!(krate.cfg_options.check_atom(&crate::parse_cfg(flag).unwrap()), "{flag}");
        }
        assert!(!krate.cfg_options.check_atom(&crate::parse_cfg("saved").unwrap()));
        assert_eq!(krate.env.get("DECLARED").as_deref(), Some("editor override"));
    }
}

#[test]
fn compiler_project_rejects_missing_or_inconsistent_compiler_intent() {
    for missing in ["cfg", "build"] {
        let mut json = compiler_project();
        json["crates"][0].as_object_mut().unwrap().remove(missing);
        let error = serde_json::from_value::<ProjectJsonData>(json.clone()).unwrap_err();
        assert!(error.to_string().contains("compiler_mode"), "{error}");
        json["compiler_mode"] = false.into();
        let legacy: ProjectJsonData = serde_json::from_value(json).unwrap();
        let serialized = serde_json::to_value(&legacy).unwrap();
        assert!(serialized["crates"][0]["cfg"].is_array());
        assert_eq!(serde_json::from_value::<ProjectJsonData>(serialized).unwrap(), legacy);
    }
    let mut json = compiler_project();
    json["crates"][0]["cfg_groups"] = serde_json::json!(["unresolved"]);
    assert!(serde_json::from_value::<ProjectJsonData>(json).is_err());
    let mut json = compiler_project();
    json["crates"][0]["build"].as_object_mut().unwrap().remove("target_kind");
    assert!(serde_json::from_value::<ProjectJsonData>(json).is_err());
    for kind in ["bin", "test"] {
        let mut json = compiler_project();
        json["crates"][3]["build"]["target_kind"] = kind.into();
        let error = serde_json::from_value::<ProjectJsonData>(json).unwrap_err();
        assert!(error.to_string().contains("proc-macro"), "{error}");
    }
    let json = compiler_project();
    let data: ProjectJsonData = serde_json::from_value(json).unwrap();
    let roundtrip = serde_json::to_value(&data).unwrap();
    assert_eq!(roundtrip["compiler_mode"], true);
    assert_eq!(serde_json::from_value::<ProjectJsonData>(roundtrip).unwrap(), data);
}

#[test]
fn rust_project_cfg_groups() {
    let (crate_graph, _proc_macros) = load_rust_project("cfg-groups.json");
    check_crate_graph(crate_graph, expect_file!["../test_data/output/rust_project_cfg_groups.txt"]);
}

#[test]
fn rust_project_crate_attrs() {
    let (crate_graph, _proc_macros) = load_rust_project("crate-attrs.json");
    check_crate_graph(
        crate_graph,
        expect_file!["../test_data/output/rust_project_crate_attrs.txt"],
    );
}

#[test]
fn crate_graph_dedup_identical() {
    let (mut crate_graph, proc_macros) = load_cargo("regex-metadata.json");

    let (d_crate_graph, mut d_proc_macros) = (crate_graph.clone(), proc_macros.clone());

    crate_graph.extend(d_crate_graph.clone(), &mut d_proc_macros);
    assert!(crate_graph.iter().eq(d_crate_graph.iter()));
    assert_eq!(proc_macros, d_proc_macros);
}

#[test]
fn crate_graph_dedup() {
    let mut file_map = Default::default();

    let ripgrep_workspace = load_workspace_from_metadata("ripgrep-metadata.json");
    let (mut crate_graph, _proc_macros) = to_crate_graph(ripgrep_workspace, &mut file_map);
    assert_eq!(crate_graph.iter().count(), 71);

    let regex_workspace = load_workspace_from_metadata("regex-metadata.json");
    let (regex_crate_graph, mut regex_proc_macros) = to_crate_graph(regex_workspace, &mut file_map);
    assert_eq!(regex_crate_graph.iter().count(), 50);

    crate_graph.extend(regex_crate_graph, &mut regex_proc_macros);
    assert_eq!(crate_graph.iter().count(), 108);
}

#[test]
fn smoke_test_real_sysroot_cargo() {
    let file_map = &mut FxHashMap::<AbsPathBuf, FileId>::default();
    let meta: Metadata = get_test_json_file("hello-world-metadata.json");
    let manifest_path =
        ManifestPath::try_from(AbsPathBuf::try_from(meta.workspace_root.clone()).unwrap()).unwrap();
    let cargo_workspace = CargoWorkspace::new(meta, manifest_path, Default::default(), false);
    let mut sysroot = Sysroot::discover(
        AbsPath::assert(Utf8Path::new(env!("CARGO_MANIFEST_DIR"))),
        &Default::default(),
    );
    let cwd = AbsPathBuf::assert_utf8(temp_dir().join("smoke_test_real_sysroot_cargo"));
    std::fs::create_dir_all(&cwd).unwrap();
    let loaded_sysroot = sysroot
        .load_workspace(&RustSourceWorkspaceConfig::default_cargo(), false, &|_| ())
        .unwrap();
    if let Some(loaded_sysroot) = loaded_sysroot {
        sysroot.set_workspace(loaded_sysroot);
    }
    assert!(
        matches!(sysroot.workspace(), RustLibSrcWorkspace::Workspace { .. }),
        "got {}",
        sysroot.workspace()
    );
    let project_workspace = ProjectWorkspace {
        kind: ProjectWorkspaceKind::Cargo {
            cargo: cargo_workspace,
            build_scripts: WorkspaceBuildScripts::default(),
            rustc: Err(None),
            error: None,
        },
        sysroot,
        rustc_cfg: Vec::new(),
        cfg_overrides: Default::default(),
        toolchain: None,
        target: Err("target_data_layout not loaded".into()),
        extra_includes: Vec::new(),
        set_test: true,
    };
    project_workspace.to_crate_graph(
        &mut {
            |path| {
                let len = file_map.len();
                Some(*file_map.entry(path.to_path_buf()).or_insert(FileId::from_raw(len as u32)))
            }
        },
        &Default::default(),
    );
}

#[test]
fn script_output_identity_is_independent_of_an_out_dir_environment_override() {
    let mut workspace = load_workspace_from_metadata("hello-world-metadata.json");
    let ProjectWorkspaceKind::Cargo { cargo, build_scripts, .. } = &mut workspace.kind else {
        unreachable!()
    };
    let package = cargo.packages().find(|&package| cargo[package].is_member).unwrap();
    let out = AbsPathBuf::assert_utf8(std::env::temp_dir().join("producer-output-identity"));
    let override_value = "overridden-by-script";
    let message = serde_json::json!({
        "reason": "build-script-executed", "package_id": cargo[package].id.repr,
        "linked_libs": [], "linked_paths": [], "cfgs": [],
        "env": [["OUT_DIR", override_value]], "out_dir": out.as_str(),
    })
    .to_string();
    *build_scripts = WorkspaceBuildScripts::of_messages(cargo, [message.as_str()]);
    assert!(build_scripts.error().is_none());
    let (graph, _) = to_crate_graph(workspace, &mut Default::default());
    let owners: Vec<_> =
        graph.iter().filter(|&id| graph[id].extra.blaze_build_script_out_dir.is_some()).collect();
    assert!(!owners.is_empty());
    for owner in owners {
        assert_eq!(graph[owner].extra.blaze_build_script_out_dir.as_ref(), Some(&out));
        assert_eq!(graph[owner].env.get("OUT_DIR").as_deref(), Some(override_value));
    }
}
