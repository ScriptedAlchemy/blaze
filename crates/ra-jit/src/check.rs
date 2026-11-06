//! `ra-jit --check <project> …`: the daemon's `cargo ra-jit check` of a build it has no
//! programs of. It loads the workspace as the worker of that build does and builds nothing:
//! each request on its socket is answered with what rust-analyzer finds in the crates of the
//! workspace, as a build says its errors. No rustc checks the source.

use std::{
    path::{Path, PathBuf},
    time::SystemTime,
};

use anyhow::Context as _;
use ide_db::{
    ChangeWithProcMacros, RootDatabase,
    base_db::{SourceDatabase as _, TargetKind},
};
use project_model::ProjectWorkspaceKind;
use rustc_hash::{FxHashMap, FxHashSet};
use vfs::Vfs;

use crate::{Args, Parts, Sources, Watched, WatchedFile};

#[path = "release_features.rs"]
mod release_features;

pub(crate) const EXPANSION_ERRORS: &[&str] =
    &["syntax-error", "macro-error", "macro-def-error", "non-unicode-env", "unresolved-macro-call"];

/// An included expression is allowed to be non-item source, but the expression
/// parser's ERROR recovery root is not evidence of a valid expression. That
/// parser can consume trailing items without reporting an error for them.
pub(crate) fn expression_input(text: &str, edition: syntax::Edition) -> bool {
    use syntax::ast::AstNode as _;
    let parse = syntax::ast::Expr::parse(text, edition);
    parse.errors().is_empty() && syntax::ast::Expr::cast(parse.syntax_node()).is_some()
}

#[cfg(test)]
mod syntax_tests {
    use super::*;

    #[test]
    fn cold_admission_checks_unreachable_bodies_and_cached_source_changes() {
        use test_fixture::WithFixture;

        let source = "fn main() {}\nfn broken() -> u32 { \"text\" }\n";
        let (mut db, file) =
            RootDatabase::with_single_file(&format!("//- /main.rs crate:app\n{source}"));
        let file = file.file_id(&db);
        let args = Args::parse(
            ["project", "--crate", "app", "--out", "app.o"].into_iter().map(str::to_owned),
        )
        .unwrap();
        let files = [WatchedFile {
            file: Watched::Source(file),
            path: PathBuf::from("/main.rs"),
            mtime: SystemTime::now(),
        }];
        let first = admission(&db, &args, &files)
            .unwrap()
            .expect("a cold build must reject a type error in an unreachable body");
        assert!(first.0.iter().any(|it| it.code.as_deref() == Some("E0308")));
        assert_eq!(admission(&db, &args, &files).unwrap(), Some(first));

        let mut fixed = ChangeWithProcMacros::default();
        fixed.change_file(file, Some("fn main() {}\nfn broken() -> u32 { 7 }\n".into()));
        db.apply_change(fixed);
        assert!(admission(&db, &args, &files).unwrap().is_none());
        assert!(admission(&db, &args, &files).unwrap().is_none());

        let mut broken = ChangeWithProcMacros::default();
        broken.change_file(file, Some(source.into()));
        db.apply_change(broken);
        assert!(admission(&db, &args, &files).unwrap().is_some());
    }

    #[test]
    fn cold_admission_checks_selected_dependencies_and_active_configuration() {
        use test_fixture::WithFixture;

        let args = Args::parse(
            ["project", "--crate", "app", "--out", "app.o"].into_iter().map(str::to_owned),
        )
        .unwrap();
        for (body, rejected) in [
            ("pub fn broken() -> u32 { \"text\" }", true),
            ("#[cfg(any())] pub fn broken() -> u32 { \"text\" }", false),
            ("pub fn broken() -> u32 { 7 }", false),
        ] {
            let fixture = format!(
                "//- /main.rs crate:app deps:dep\nfn main() {{ dep::value(); }}\n\
                 //- /dep.rs crate:dep\npub fn value() -> u32 {{ 1 }}\n{body}\n\
                 //- /unselected.rs crate:unselected\nfn broken() -> u32 {{ \"text\" }}\n"
            );
            let (db, _) = RootDatabase::with_many_files(&fixture);
            assert_eq!(admission(&db, &args, &[]).unwrap().is_some(), rejected, "{body}");
        }
    }

    #[test]
    fn cold_admission_uses_the_builds_lint_cap() {
        use test_fixture::WithFixture;

        let (db, _) = RootDatabase::with_single_file(
            "//- /main.rs crate:app\n#![deny(non_snake_case)]\nfn main() {}\npub fn Denied() {}\n",
        );
        let mut args = Args::parse(
            ["project", "--crate", "app", "--out", "app.o"].into_iter().map(str::to_owned),
        )
        .unwrap();
        assert!(admission(&db, &args, &[]).unwrap().is_some());
        args.lints_capped = true;
        assert!(admission(&db, &args, &[]).unwrap().is_none());
    }

    #[test]
    fn cold_admission_checks_stable_feature_attributes_and_bootstrap_policy() {
        use ide_db::base_db::{Env, set_process_env};
        use test_fixture::WithFixture;

        let (mut db, _) = RootDatabase::with_single_file(
            "//- /main.rs crate:gated\n#![feature(never_type)]\nfn main() {}\n",
        );
        let args = Args::parse(
            ["project", "--crate", "gated", "--out", "gated.o"].into_iter().map(str::to_owned),
        )
        .unwrap();
        for (bootstrap, rejected) in [
            (None, true),
            (Some("1"), false),
            (Some("-1"), true),
            (Some("gated"), false),
            (Some("other"), true),
            (Some("gated,other"), false),
            (Some("1,other"), true),
            (Some("-gated"), true),
            (Some(" gated"), true),
        ] {
            let environment =
                bootstrap.map(|value| ("RUSTC_BOOTSTRAP".to_owned(), value.to_owned())).into_iter();
            set_process_env(&mut db, Env::from_iter(environment));
            let said = admission(&db, &args, &[]).unwrap();
            assert_eq!(said.is_some(), rejected, "bootstrap {bootstrap:?}");
            if let Some(said) = said {
                assert!(said.0.iter().any(|it| it.code.as_deref() == Some("E0554")));
            }
        }

        set_process_env(&mut db, Env::from_iter([("RUSTC_BOOTSTRAP".into(), "1".into())]));
        let krate = ide_db::base_db::all_crates(&db)[0];
        db.set_env(krate, Env::from_iter([("RUSTC_BOOTSTRAP".into(), "other".into())]));
        assert!(admission(&db, &args, &[]).unwrap().is_some());
        db.set_env(krate, Env::from_iter([("RUSTC_BOOTSTRAP".into(), "gated".into())]));
        assert!(admission(&db, &args, &[]).unwrap().is_none());
        let mut raw = Env::default();
        raw.set_non_unicode("RUSTC_BOOTSTRAP");
        db.set_env(krate, raw);
        assert!(admission(&db, &args, &[]).unwrap().is_some());
    }

    #[test]
    fn cold_and_warm_admission_check_effective_feature_attributes() {
        use test_fixture::WithFixture;

        let (mut db, file) =
            RootDatabase::with_single_file("//- /main.rs crate:app\nfn main() {}\n");
        let file = file.file_id(&db);
        let args = Args::parse(
            ["project", "--crate", "app", "--out", "app.o"].into_iter().map(str::to_owned),
        )
        .unwrap();
        let files = [WatchedFile {
            file: Watched::Source(file),
            path: PathBuf::from("/main.rs"),
            mtime: SystemTime::now(),
        }];
        let sources = Sources { paths: [(file, PathBuf::from("/main.rs"))].into_iter().collect() };
        for (source, rejected) in [
            ("fn main() {}", false),
            ("#![cfg_attr(all(), feature(never_type))]\nfn main() {}", true),
            ("#![cfg_attr(any(), feature(never_type))]\nfn main() {}", false),
            ("#![feature(never_type)]\nfn main() {}", true),
            ("fn main() {}", false),
        ] {
            let mut changed = ChangeWithProcMacros::default();
            changed.change_file(file, Some(source.into()));
            db.apply_change(changed);
            let cold = admission(&db, &args, &files).unwrap();
            let warm = crate::broken_in(&db, &[file], &sources, &args.checkout, None, false);
            assert_eq!(cold.is_some(), rejected, "{source}");
            assert_eq!(warm, cold, "{source}");
        }
    }

    #[test]
    fn bootstrap_probe_uses_pinned_feature_status_and_does_not_loosen_gates() {
        for (features, bootstrap, accepts) in [
            (vec![], false, true),
            (vec!["proc_macro_span"], true, true),
            (vec!["never_type"], true, true),
            (vec!["test_accepted_feature"], true, true),
            (vec!["proc_macro_span"], false, false),
            (vec!["never_type_fallback"], true, false),
            (vec!["never_type_fallback"], false, false),
        ] {
            assert_eq!(
                probe_feature_policy(bootstrap, features.into_iter().map(str::to_owned))
                    .unwrap(),
                accepts
            );
        }
        assert!(
            probe_feature_policy(true, ["not_a_rust_feature_199".to_owned()])
                .unwrap_err()
                .to_string()
                .contains("cannot prove feature")
        );
    }

    #[test]
    fn bootstrap_probe_uses_effective_cfg_attr_feature_names() {
        use test_fixture::WithFixture;
        for (source, wanted) in [
            ("#![cfg_attr(all(), feature(proc_macro_span))]\nfn main() {}", true),
            ("#![cfg_attr(any(), feature(proc_macro_span))]\nfn main() {}", false),
            ("#![feature(proc_macro_span)]\nfn main() {}", true),
        ] {
            let (db, _) =
                RootDatabase::with_single_file(&format!("//- /main.rs crate:app\n{source}\n"));
            let krate = ide_db::base_db::all_crates(&db)[0];
            let names: Vec<String> =
                hir_def::unstable_features::UnstableFeatures::query(&db, krate)
                    .iter()
                    .map(|name| name.as_str().to_owned())
                    .collect();
            assert_eq!(names.iter().any(|name| name == "proc_macro_span"), wanted);
            assert!(probe_feature_policy(true, names).unwrap());
        }
    }

    #[test]
    fn fresh_compilation_checks_macro_errors_before_codegen() {
        use test_fixture::WithFixture;

        let args = Args::parse(
            ["project", "--crate", "app", "--out", "app.o"].into_iter().map(str::to_owned),
        )
        .unwrap();
        for source in [
            "compile_error!(\"admission failure\");",
            "macro_rules! fail { () => { compile_error!(\"admission failure\"); } } fail!();",
            "fn unused<T>() { compile_error!(\"admission failure\"); }",
        ] {
            let fixture = format!(
                "//- /main.rs crate:app\n#[rustc_builtin_macro]\nmacro_rules! compile_error {{ ($($t:tt)*) => {{}} }}\n{source}\n"
            );
            let (db, _) = RootDatabase::with_single_file(&fixture);
            let error = admission(&db, &args, &[]).unwrap();
            assert!(
                error.is_some_and(|it| serde_json::to_string(&it)
                    .unwrap()
                    .contains("admission failure")),
                "accepted {source}"
            );
        }
        let (db, _) = RootDatabase::with_single_file(
            "//- /main.rs crate:app\nmacro_rules! broken { () => { pub fn bad() { let = 1; } } } broken!();\n",
        );
        assert!(admission(&db, &args, &[]).unwrap().is_some());
        let (db, _) = RootDatabase::with_single_file(
            "//- /main.rs crate:app\n#[rustc_builtin_macro]\nmacro_rules! compile_error { ($($t:tt)*) => {} }\n#[cfg(any())] compile_error!(\"inactive\");\npub fn valid() {}\n",
        );
        assert!(admission(&db, &args, &[]).unwrap().is_none());
    }

    #[test]
    fn metadata_probe_hydrates_generated_includes() {
        use test_fixture::WithFixture;
        let directory = tempfile::tempdir().unwrap();
        let generated = directory.path().join("output");
        std::fs::create_dir(&generated).unwrap();
        let included = generated.join("level.in");
        std::fs::write(&included, "pub fn number() -> u32 { 7 }\n").unwrap();
        let (mut db, file) = RootDatabase::with_single_file(
            r#"//- /main.rs crate:app
#[rustc_builtin_macro]
macro_rules! include { () => {} }
#[rustc_builtin_macro]
macro_rules! concat { () => {} }
#[rustc_builtin_macro]
macro_rules! env { () => {} }
include!(concat!(env!("OUT_DIR"), "/level.in"));
"#,
        );
        let file = file.file_id(&db);
        let path = directory.path().join("lib.rs");
        let text = db.file_text(file).text(&db).to_string();
        std::fs::write(&path, &text).unwrap();
        let at = vfs::VfsPath::from(vfs::AbsPathBuf::assert_utf8(path.clone()));
        let mut vfs = Vfs::default();
        vfs.set_file_contents(at.clone(), Some(text.into_bytes()));
        assert_eq!(vfs.file_id(&at).unwrap().0, file);
        let mut root = vfs::file_set::FileSet::default();
        root.insert(file, at);
        let mut change = ChangeWithProcMacros::default();
        change.set_roots(vec![ide_db::base_db::SourceRoot::new_local(root)]);
        db.apply_change(change);
        let krate = ide_db::base_db::all_crates(&db)[0];
        db.set_env(
            krate,
            ide_db::base_db::Env::from_iter([(
                "OUT_DIR".to_owned(),
                generated.to_str().unwrap().to_owned(),
            )]),
        );
        let files = source_files(&mut db, &mut vfs, &[krate]);
        assert!(files.iter().any(|file| file.path == included), "generated source was not loaded");
        let diagnostics =
            hard_diagnostics(&db, &ide_diagnostics::DiagnosticsConfig::test_sample(), file, krate);
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
    }

    #[test]
    fn owned_check_hydrates_generated_source_and_records_it() {
        use std::collections::BTreeMap;
        let directory = tempfile::tempdir().unwrap();
        let sysroot = directory.path().join("toolchain");
        crate::answers::tests::synthetic_toolchain(&sysroot);
        for name in ["core", "std"] {
            let root = sysroot.join("lib/rustlib/src/rust/library").join(name).join("src");
            std::fs::create_dir_all(&root).unwrap();
            std::fs::write(root.join("lib.rs"), "").unwrap();
        }
        let out = directory.path().join("generated");
        std::fs::create_dir(&out).unwrap();
        let included = out.join("level.in");
        std::fs::write(&included, "pub fn number() -> u32 { 7 }\n").unwrap();
        let source = directory.path().join("lib.rs");
        std::fs::write(
            &source,
            r#"#![no_std]
#[rustc_builtin_macro]
macro_rules! include { () => {} }
#[rustc_builtin_macro]
macro_rules! concat { () => {} }
#[rustc_builtin_macro]
macro_rules! env { () => {} }
include!(concat!(env!("OUT_DIR"), "/level.in"));
pub fn value() -> u32 { number() }
"#,
        )
        .unwrap();
        let plan = crate::cargo::own_script::planned_description(
            &sysroot,
            vec![crate::cargo::own_script::Planned {
                out_dir: None,
                workspace_member: false,
                name: "included".into(),
                root: source,
                edition: "2021".into(),
                cfg: vec![],
                codegen_flags: vec![],
                env: BTreeMap::from([("OUT_DIR".into(), out.display().to_string())]),
                deps: vec![],
                proc_macro: None,
            }],
        );
        let mut plan: serde_json::Value = serde_json::from_str(&plan).unwrap();
        plan["compiler_mode"] = serde_json::json!(true);
        plan["crates"][0]["build"] = serde_json::json!({"target_kind":"lib", "label":"included", "build_file": directory.path().join("Cargo.toml")});
        let project = directory.path().join("rust-project.json");
        std::fs::write(&project, serde_json::to_vec(&plan).unwrap()).unwrap();
        let output = directory.path().join("checked.json");
        let trace = directory.path().join("inputs.d");
        let args = || {
            Args::parse(
                [
                    project.display().to_string(),
                    "--crate".into(),
                    "included".into(),
                    "--out".into(),
                    output.display().to_string(),
                ]
                .into_iter(),
            )
            .unwrap()
        };
        once(args(), &trace).unwrap();
        assert!(output.is_file());
        assert!(
            std::fs::read(&trace)
                .unwrap()
                .windows(included.as_os_str().as_encoded_bytes().len())
                .any(|part| part == included.as_os_str().as_encoded_bytes()),
            "input trace omitted generated source"
        );
        std::fs::remove_file(&output).unwrap();
        std::fs::write(&included, "pub fn number() -> u32 { \"wrong\" }\n").unwrap();
        assert!(once(args(), &trace).unwrap_err().to_string().contains("E0308"));
        assert!(!output.exists(), "failed check published an acceptance artifact");
    }

    #[test]
    fn trailing_item_recovery_does_not_hide_a_dependency_syntax_error() {
        let text = r#"include!(concat!(env!("OUT_DIR"), "/shared.rs"));
pub fn value() -> u8 { 1 }
pub fn broken() -> u8 { 1 + }
"#;
        assert!(!syntax::SourceFile::parse(text, syntax::Edition::Edition2021).errors().is_empty());
        assert!(!expression_input(text, syntax::Edition::Edition2021));
        assert!(expression_input("1 + 2", syntax::Edition::Edition2021));
        assert!(expression_input("include!(\"value.rs\")", syntax::Edition::Edition2021));
        assert!(!expression_input("1 +", syntax::Edition::Edition2021));
    }
}

/// Syntax and expansion failures must be reported before codegen/linking, even
/// when the crate has no functions or the failing body is never instantiated.
/// Walk semantic modules rather than all loaded source-root files.
pub(crate) fn admission(
    db: &RootDatabase,
    args: &Args,
    files: &[WatchedFile],
) -> anyhow::Result<Option<crate::errors::Said>> {
    let mut checked = Vec::new();
    let mut modules = Vec::new();
    let mut seen = FxHashSet::default();
    for name in args.crates() {
        for krate in crate::crates_named(db, name, args)? {
            for id in ide_db::base_db::Crate::from(krate).transitive_deps(db) {
                if id.data(db).origin.is_lang() || !seen.insert(id) {
                    continue;
                }
                let krate = hir::Crate::from(id);
                let mut files: Vec<_> = krate
                    .modules(db)
                    .into_iter()
                    .map(|module| {
                        module.definition_source_file_id(db).original_file(db).file_id(db)
                    })
                    .collect();
                files.sort_unstable();
                files.dedup();
                checked.extend(&files);
                modules.push((id, files));
            }
        }
    }
    checked.sort_unstable();
    checked.dedup();
    let paths = files
        .iter()
        .filter_map(|it| match it.file {
            Watched::Source(id) => Some((id, it.path.clone())),
            Watched::Included(_) => None,
        })
        .collect();
    let sources = Sources { paths };
    if let Some(error) = crate::unparsed(db, &checked, &sources, &args.checkout) {
        return Ok(Some(crate::errors::Said::one(error)));
    }
    let mut errors = Vec::new();
    for (krate, files) in modules {
        let within = Some(std::sync::Arc::new(FxHashSet::from_iter([krate])));
        if let Some(said) =
            crate::broken_in(db, &files, &sources, &args.checkout, within, !args.lints_capped)
        {
            for error in said.0 {
                if !errors.contains(&error) {
                    errors.push(error);
                }
            }
        }
    }
    Ok((!errors.is_empty()).then_some(crate::errors::Said(errors)))
}

pub(crate) fn channel_error(
    db: &RootDatabase,
    krate: ide_db::base_db::Crate,
) -> Option<(ide_db::FileRange, &'static str, String)> {
    use syntax::ast::{AstNode as _, HasAttrs as _};

    if krate.data(db).origin.is_lang() {
        return None;
    }
    let source = hir::Crate::from(krate);
    if !source.feature_attributes_on_stable(db) {
        return None;
    }
    let environment = krate.env(db);
    let bootstrap = if environment.is_non_unicode("RUSTC_BOOTSTRAP") {
        None
    } else {
        environment
            .get("RUSTC_BOOTSTRAP")
            .or_else(|| ide_db::base_db::process_env_var(db, "RUSTC_BOOTSTRAP"))
    };
    let name = source.display_name(db).map(|name| name.to_string());
    let allowed = bootstrap.as_deref().is_some_and(|value| {
        value == "1"
            || name.as_deref().is_some_and(|name| value.split(',').any(|entry| entry == name))
    });
    if allowed {
        return None;
    }
    let range = match source.root_module(db).definition_source(db).value {
        hir::ModuleSource::SourceFile(file) => file
            .attrs()
            .find(|attr| attr.simple_name().as_deref() == Some("feature"))
            .map(|attr| attr.syntax().text_range()),
        _ => None,
    }
    .unwrap_or_else(|| syntax::TextRange::empty(0.into()));
    Some((
        ide_db::FileRange { file_id: source.root_file(db), range },
        "E0554",
        "`#![feature]` may not be used on the stable release channel".to_owned(),
    ))
}

fn hard_diagnostics(
    db: &RootDatabase,
    config: &ide_diagnostics::DiagnosticsConfig,
    file: vfs::FileId,
    krate: ide_db::base_db::Crate,
) -> Vec<ide_diagnostics::Diagnostic> {
    hir::attach_db(db, || {
        ide_diagnostics::semantic_diagnostics_within(
            db,
            config,
            &ide_db::assists::AssistResolveStrategy::None,
            file,
            Some(std::sync::Arc::new(FxHashSet::from_iter([krate]))),
        )
        .into_iter()
        .filter(|it| it.severity == ide_db::Severity::Error)
        .filter(|it| {
            matches!(
                it.code,
                ide_diagnostics::DiagnosticCode::RustcHardError(_)
                    | ide_diagnostics::DiagnosticCode::SyntaxError
            ) || EXPANSION_ERRORS.contains(&it.code.as_str())
        })
        .collect()
    })
}

fn source_files(
    db: &mut RootDatabase,
    vfs: &mut Vfs,
    crates: &[ide_db::base_db::Crate],
) -> Vec<WatchedFile> {
    let mut files = WatchedFile::sources(vfs, db, SystemTime::now());
    let sources: Vec<_> = files
        .iter()
        .filter_map(|it| match it.file {
            Watched::Source(file) => Some((file, &*it.path)),
            Watched::Included(_) => None,
        })
        .collect();
    let mut tree = crate::tree::Tree::new(db, std::mem::take(vfs), &sources);
    crate::hydrate_includes(db, &mut tree, &mut files, |db| {
        crate::resolve_all(db, crates, crate::cores())
    });
    *vfs = std::mem::take(tree.vfs());
    files
}

/// Source probes must not mistake an unrecognized or removed feature for a
/// successful bootstrap compilation. cfg_attr has already been expanded by
/// the crate def map; the catalog describes the pinned Rust 1.99 release.
fn probe_feature_policy(
    bootstrap: bool,
    feature_names: impl IntoIterator<Item = String>,
) -> anyhow::Result<bool> {
    let mut names: Vec<String> = feature_names.into_iter().collect();
    names.sort_unstable();
    names.dedup();
    if names.is_empty() {
        return Ok(true);
    }
    if !bootstrap {
        eprintln!("error[E0554]: `#![feature]` may not be used on the stable release channel");
        return Ok(false);
    }
    for name in names {
        match release_features::classify(&name) {
            release_features::Status::Active | release_features::Status::Accepted => {}
            release_features::Status::Removed => {
                eprintln!("error[E0557]: feature `{name}` has been removed in Rust 1.99.0");
                return Ok(false);
            }
            release_features::Status::Unknown => {
                anyhow::bail!(
                    "owned Rust 1.99.0 feature catalog cannot prove feature `{name}`;                      refusing the bootstrap probe rather than accepting an unknown feature"
                );
            }
        }
    }
    Ok(true)
}

/// A metadata-only compiler probe checks all source bodies, including unreachable
/// ones. It loads an owned source description, not a Cargo workspace or rmeta.
pub(crate) fn source(args: Args, bootstrap: bool) -> anyhow::Result<bool> {
    crate::process_wide();
    let config = crate::cargo_config(&args);
    let load = crate::LoadCargoConfig { load_out_dirs_from_check: false, ..crate::load_config() };
    let (mut db, mut vfs, _) = crate::load(
        &[&args.project],
        &config,
        load,
        args.selection.clone(),
        None,
        None,
        args.project_origin,
    )?;
    ide_db::base_db::set_process_env(&mut db, crate::process::own());
    let crates = crate::crates_named(&db, &args.krate, &args)?;
    anyhow::ensure!(crates.len() == 1, "the source probe must identify one crate");
    let krate = crates[0];
    let id = ide_db::base_db::Crate::from(krate);
    // Query effective (including cfg_attr-expanded) feature names, not the
    // source's raw feature tokens. Inactive cfg_attr must not gate a probe.
    let features = hir_def::unstable_features::UnstableFeatures::query(&db, id);
    if !probe_feature_policy(
        bootstrap,
        features.iter().map(|feature| feature.as_str().to_owned()),
    )? {
        return Ok(false);
    }
    let files = source_files(&mut db, &mut vfs, &[id]);
    let paths: FxHashMap<_, _> = files
        .iter()
        .filter_map(|it| match it.file {
            Watched::Source(id) => Some((id, it.path.clone())),
            Watched::Included(_) => None,
        })
        .collect();
    let mut checked: Vec<_> = krate
        .modules(&db)
        .into_iter()
        .map(|module| module.definition_source_file_id(&db).original_file(&db).file_id(&db))
        .collect();
    checked.sort_unstable();
    checked.dedup();
    let sources = Sources { paths };
    let diagnostics = ide_diagnostics::DiagnosticsConfig::test_sample();
    let mut undecided = Vec::new();
    for &file in &checked {
        let (inferred, hard): (Vec<_>, Vec<_>) =
            hard_diagnostics(&db, &diagnostics, file, id).into_iter().partition(|it| {
                let code = it.code.as_str();
                INFERRED.contains(&code) || crate::BORROWCK.contains(&code)
            });
        undecided
            .extend(inferred.into_iter().map(|it| format!("{}: {}", it.code.as_str(), it.message)));
        let hard: Vec<_> = hard.into_iter().map(|it| it.message).collect();
        if !hard.is_empty() {
            for message in hard {
                eprintln!("error: {message}");
            }
            return Ok(false);
        }
    }
    let within = Some(std::sync::Arc::new(FxHashSet::from_iter([id])));
    // What rests on inferred types or the owned borrow checker is undecided above.
    let except: Vec<&str> = INFERRED.iter().chain(crate::BORROWCK).copied().collect();
    let broken =
        crate::broken_in_except(&db, &checked, &sources, &args.checkout, within, false, &except);
    let refused = hir::attach_db(&db, || {
        crate::refusal(&db, &checked, &sources, &args.checkout, broken, None)
    });
    if let Some(refused) = refused {
        eprintln!("{refused}");
        return Ok(false);
    }
    let acceptance = hir::attach_db(&db, || hir::compiler_acceptance::check(&db, id));
    if !acceptance.errors.is_empty() {
        for error in &acceptance.errors {
            eprintln!("error: {error}");
        }
        return Ok(false);
    }
    // Mismatched types rustc certainly rejects are in `acceptance.errors`; the rest are
    // undecided, a refusal and never a "no".
    undecided.extend(acceptance.unsupported.iter().cloned());
    anyhow::ensure!(
        undecided.is_empty(),
        "owned compiler probe acceptance is not proven: {}",
        undecided.join("; ")
    );
    Ok(true)
}

/// rust-analyzer's errors that rest on the types it inferred: of expressions, of method and
/// field lookups through them, of the traits it solved and the exhaustiveness of matches on
/// them. A gap in its inference or coercions reports one where rustc reports none, so one
/// is not proof that rustc rejects a probe; nor is an error of the owned borrow checker.
const INFERRED: [&str; 25] = [
    "E0004", "E0005", "E0033", "E0054", "E0057", "E0061", "E0133", "E0277", "E0282", "E0308",
    "E0559", "E0599", "E0600", "E0604", "E0605", "E0606", "E0607", "E0608", "E0614", "E0616",
    "E0618", "E0620", "E0624", "E0641", "E0804",
];

/// What a checker answers when the workspace changed in a way only loading it again takes
/// in: a source file came or went, a manifest changed, or the source of a build script or a
/// proc macro did, which run as they were built.
pub(crate) const RELOAD: &str = "reload";

/// Check an authenticated source description once, preserving the same diagnostics
/// as the daemon checker and the compiler's complete replay input trace.
pub(crate) fn once(args: crate::Args, trace: &Path) -> anyhow::Result<()> {
    crate::process_wide();
    let read_from = SystemTime::now();
    let (mut db, mut vfs, checkouts) = crate::load(
        &[&args.project],
        &crate::cargo_config(&args),
        crate::load_config(),
        args.selection.clone(),
        None,
        None,
        args.project_origin,
    )?;
    ide_db::base_db::set_process_env(&mut db, crate::process::own());
    let Parts { root, lints_capped, .. } = checkouts.parts(&args.project)?;
    let members: FxHashSet<_> = ide_db::base_db::all_crates(&db)
        .iter()
        .copied()
        .filter(|&krate| !hir::Crate::from(krate).origin(&db).is_lang())
        .collect();
    let _ = source_files(&mut db, &mut vfs, &members.iter().copied().collect::<Vec<_>>());
    let mut checker =
        Checker::new(&db, &vfs, &checkouts, &members, &[], read_from, root, lints_capped);
    checker.checked = members
        .iter()
        .flat_map(|&krate| hir::Crate::from(krate).modules(&db))
        .map(|module| module.definition_source_file_id(&db).original_file(&db).file_id(&db))
        .collect();
    checker.checked.sort_unstable();
    checker.checked.dedup();
    let verdict = checker.verdict(&db);
    anyhow::ensure!(verdict == "ok", "{verdict}");
    crate::write_inputs(&db, &args, &vfs, &args.out, trace)?;
    std::fs::write(&args.out, b"{\"checked\":true}\n")?;
    Ok(())
}

/// Loads the workspace of `args` and answers the check requests on its socket.
pub(crate) fn serve(args: Args) -> anyhow::Result<()> {
    let socket = args.socket.clone().context("--check needs --socket")?;
    crate::process_wide();
    let read_from = SystemTime::now();
    let cargo_config = crate::cargo_config(&args);
    let projects = [args.project.as_path()];
    let selection = args.selection.clone();
    let (mut db, mut vfs, checkouts) = crate::load(
        &projects,
        &cargo_config,
        crate::load_config(),
        selection,
        None,
        None,
        args.project_origin,
    )?;
    ide_db::base_db::set_process_env(&mut db, crate::process::own());
    let Parts { root, members, lints_capped, .. } = checkouts.parts(&args.project)?;
    let _ = source_files(&mut db, &mut vfs, &members.iter().copied().collect::<Vec<_>>());
    let excludes = &args.selection.excludes;
    let mut checker =
        Checker::new(&db, &vfs, &checkouts, &members, excludes, read_from, root, lints_capped);
    let served = match crate::socket::Served::bind(&socket) {
        Ok(served) => served,
        Err(crate::socket::NotServed::Taken) => {
            anyhow::bail!("another checker serves {}", socket.display())
        }
        Err(crate::socket::NotServed::Failed(error)) => {
            anyhow::bail!("listening on {}: {error}", socket.display())
        }
    };
    eprintln!("checking {} files of {} crates", checker.checked.len(), checker.crates);
    for (stream, _open) in served.incoming() {
        if crate::socket::read_request(&stream).is_err() {
            continue;
        }
        let reply = match checker.refresh(&mut db) {
            Some(reload) => {
                eprintln!("{RELOAD}: {reload}");
                RELOAD.to_owned()
            }
            None => checker.verdict(&db),
        };
        let _ = std::io::Write::write_all(&mut &stream, format!("{reply}\n").as_bytes());
        if reply == RELOAD {
            break;
        }
    }
    Ok(())
}

struct Checker {
    /// The source files of the workspace as they were last read.
    files: Vec<WatchedFile>,
    /// The files of the crates checked, which are the workspace's.
    checked: Vec<vfs::FileId>,
    crates: usize,
    /// The files of the build scripts and proc macros of the workspace.
    ran: FxHashSet<vfs::FileId>,
    /// Where new source files of the crates checked would be.
    dirs: Vec<PathBuf>,
    /// Every file loaded, by path.
    loaded: FxHashSet<PathBuf>,
    /// The manifests, the lock file and cargo's configuration, as they were when loaded.
    manifests: FxHashMap<PathBuf, Option<SystemTime>>,
    sources: Sources,
    root: PathBuf,
    lints_capped: bool,
}

impl Checker {
    #[expect(clippy::too_many_arguments)]
    fn new(
        db: &RootDatabase,
        vfs: &Vfs,
        checkouts: &crate::Checkouts,
        members: &FxHashSet<ide_db::base_db::Crate>,
        excludes: &[String],
        read_from: SystemTime,
        root: PathBuf,
        lints_capped: bool,
    ) -> Checker {
        let files = WatchedFile::sources(vfs, db, read_from);
        let paths: FxHashMap<_, _> = files
            .iter()
            .filter_map(|it| match it.file {
                Watched::Source(id) => Some((id, it.path.clone())),
                Watched::Included(_) => None,
            })
            .collect();
        let mut ran_roots = FxHashSet::default();
        // The crates of a package `--exclude` names are not checked.
        let mut excluded_roots = FxHashSet::default();
        let mut manifests = FxHashMap::default();
        for workspace in checkouts.loaded.workspaces() {
            let ProjectWorkspaceKind::Cargo { cargo, .. } = &workspace.kind else { continue };
            let root: &Path = cargo.workspace_root().as_ref();
            for file in ["Cargo.lock", ".cargo/config.toml", ".cargo/config"] {
                manifests.insert(root.join(file), None);
            }
            for package in cargo.packages().filter(|it| cargo[*it].is_local) {
                manifests.insert(cargo[package].manifest.to_path_buf().into(), None);
                let excluded = excludes.contains(&cargo[package].name);
                for &target in &cargo[package].targets {
                    let target = &cargo[target];
                    if excluded {
                        excluded_roots.insert(PathBuf::from(target.root.clone()));
                    }
                    let runs = matches!(
                        target.kind,
                        TargetKind::BuildScript | TargetKind::Lib { is_proc_macro: true }
                    );
                    if runs {
                        ran_roots.insert(PathBuf::from(target.root.clone()));
                    }
                }
            }
        }
        for (path, when) in &mut manifests {
            *when = modified(path);
        }
        let path_of = |file: vfs::FileId| -> Option<PathBuf> {
            Some(vfs.file_path(file).as_path()?.to_path_buf().into())
        };
        let (mut checked, mut ran, mut dirs, mut crates) =
            (Vec::new(), FxHashSet::default(), Vec::new(), 0);
        hir::attach_db(db, || {
            for &krate in members {
                let krate = hir::Crate::from(krate);
                if !krate.origin(db).is_local() {
                    continue;
                }
                let root_file = krate.root_file(db);
                if path_of(root_file).is_some_and(|it| excluded_roots.contains(&it)) {
                    continue;
                }
                let runs = path_of(root_file).is_some_and(|it| ran_roots.contains(&it));
                let mut of_it: Vec<vfs::FileId> = krate
                    .modules(db)
                    .into_iter()
                    .map(|it| it.definition_source_file_id(db).original_file(db).file_id(db))
                    .collect();
                of_it.sort_unstable();
                of_it.dedup();
                match runs {
                    true => ran.extend(of_it),
                    false => {
                        crates += 1;
                        if let Some(dir) = path_of(root_file).as_deref().and_then(Path::parent) {
                            dirs.push(dir.to_owned());
                        }
                        checked.extend(of_it);
                    }
                }
            }
        });
        checked.sort_unstable();
        checked.dedup();
        dirs.sort();
        dirs.dedup();
        // A directory below another is walked with it.
        let outer: Vec<PathBuf> = dirs
            .iter()
            .filter(|dir| !dirs.iter().any(|it| it != *dir && dir.starts_with(it)))
            .cloned()
            .collect();
        let loaded =
            vfs.iter().filter_map(|(_, path)| Some(path.as_path()?.to_path_buf().into())).collect();
        Checker {
            files,
            checked,
            crates,
            ran,
            dirs: outer,
            loaded,
            manifests,
            sources: Sources { paths },
            root,
            lints_capped,
        }
    }

    /// Takes in the source as it is now, or says why only loading it again does.
    fn refresh(&mut self, db: &mut RootDatabase) -> Option<String> {
        for (path, when) in &self.manifests {
            if modified(path) != *when {
                return Some(format!("{} changed", path.display()));
            }
        }
        if let Some(new) = self.dirs.iter().find_map(|dir| new_source(dir, &self.loaded)) {
            return Some(format!("{} is new", new.display()));
        }
        let mut change = ChangeWithProcMacros::default();
        for watched in &mut self.files {
            let Watched::Source(id) = watched.file else { continue };
            let Some(now) = modified(&watched.path) else {
                return Some(format!("{} is gone", watched.path.display()));
            };
            if now == watched.mtime {
                continue;
            }
            watched.mtime = now;
            let Ok(text) = crate::source_on_disk(&watched.path) else {
                return Some(format!("{} can't be read", watched.path.display()));
            };
            if *text == **db.file_text(id).text(db) {
                continue;
            }
            if self.ran.contains(&id) {
                return Some(format!("{} runs as it was built", watched.path.display()));
            }
            change.change_file(id, Some(text));
        }
        db.apply_change(change);
        None
    }

    /// `ok`, or the errors in the source as rustc writes them.
    fn verdict(&self, db: &RootDatabase) -> String {
        let broken = crate::broken_in(
            db,
            &self.checked,
            &self.sources,
            &self.root,
            None,
            !self.lints_capped,
        );
        let refused = hir::attach_db(db, || {
            crate::refusal(db, &self.checked, &self.sources, &self.root, broken, None)
        });
        match refused {
            Some(refused) => refused.reply(),
            None => "ok".to_owned(),
        }
    }
}

fn modified(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path).and_then(|it| it.modified()).ok()
}

/// A Rust source file below `dir` that was not loaded. What a build made is not looked in.
fn new_source(dir: &Path, loaded: &FxHashSet<PathBuf>) -> Option<PathBuf> {
    let mut dirs = vec![dir.to_owned()];
    while let Some(dir) = dirs.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name();
            let name = name.to_string_lossy();
            match entry.file_type() {
                Ok(kind) if kind.is_dir() => {
                    if !name.starts_with('.') && name != "target" {
                        dirs.push(path);
                    }
                }
                Ok(_) if name.ends_with(".rs") && !loaded.contains(&path) => return Some(path),
                _ => {}
            }
        }
    }
    None
}
