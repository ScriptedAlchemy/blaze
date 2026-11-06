//! Source/code identities over tracked inputs, including external includes and macros.

use std::path::Path;

use hir::db::HirDatabase;
use rustc_hash::FxHashSet;
use sha2::{Digest, Sha256};

use crate::base_db::Crate;

pub type Hash = [u8; 32];

fn input_hash(present: bool, contents: Hash) -> Hash {
    let mut hash = Sha256::new();
    hash.update([u8::from(present)]);
    hash.update(contents);
    hash.finalize().into()
}

/// Whether tracked native resolution keeps this selected file inside a declared root.
pub fn path_owned_by(db: &dyn HirDatabase, path: &str, root: &str) -> bool {
    let Ok(path) = base_db::AbsPathBuf::try_from(path) else { return false };
    db.path_ownership(&path)
        .is_some_and(|input| input.roots(db).iter().any(|owned| owned.as_str() == root))
}

/// Only declared package/output roots can supply portable include locators.
/// Every expanded occurrence must lead to a successful include of that root.
#[salsa::tracked(returns(ref))]
pub fn include_only_environment(db: &dyn HirDatabase, krate: Crate) -> FxHashSet<String> {
    use hir::input_owners::discover_current_inputs;
    let inputs = discover_current_inputs(db, krate);
    let resolutions = &inputs.resolutions;
    let reads = &inputs.environment;
    ["OUT_DIR", "CARGO_MANIFEST_DIR"]
        .into_iter()
        .filter_map(|key| {
            let root = krate.env(db).get(key)?;
            if !Path::new(&root).is_absolute() {
                return None;
            }
            let occurrences: Vec<_> =
                reads.iter().filter(|read| read.krate == krate && read.key == key).collect();
            (!occurrences.is_empty()
                && occurrences.iter().all(|read| {
                    read.include_input.is_some_and(|input| {
                        resolutions.iter().any(|resolution| {
                            resolution.krate == krate
                                && resolution.input == input
                                && resolution.present
                                && Path::new(resolution.path.as_str()).starts_with(&root)
                                && path_owned_by(db, resolution.path.as_str(), &root)
                        })
                    })
                }))
            .then(|| key.to_owned())
        })
        .collect()
}

fn source_role(db: &dyn HirDatabase, krate: Crate, path: &str) -> String {
    let env = krate.env(db);
    for (key, role) in [("OUT_DIR", "output"), ("CARGO_MANIFEST_DIR", "package")] {
        if let Some(root) = env.get(key)
            && Path::new(&root).is_absolute()
            && let Ok(relative) = Path::new(path).strip_prefix(&root)
            && path_owned_by(db, path, &root)
            && (key != "OUT_DIR" || include_only_environment(db, krate).contains(key))
        {
            return format!("{role}/{}", relative.to_string_lossy());
        }
    }
    if !env.contains_key("CARGO_MANIFEST_DIR") {
        let file = krate.data(db).root_file_id;
        let root = db.file_source_root(file).source_root_id(db);
        let sources = db.source_root(root).source_root(db);
        if let Some(parent) = sources
            .path_for_file(&file)
            .and_then(|path| path.as_path())
            .and_then(|path| path.parent())
            && let Ok(relative) = Path::new(path).strip_prefix(parent.as_str())
            && path_owned_by(db, path, parent.as_str())
        {
            return format!("package/{}", relative.to_string_lossy());
        }
    }
    format!("external/{path}")
}

/// CTFE can erase filename strings into lengths, comparisons, or layouts. These
/// reads must enter the key before a persistent entry is looked up; data relocation
/// after a hit cannot repair such scalars.
#[salsa::tracked(returns(copy))]
pub fn semantic_source_hash(db: &dyn HirDatabase, krate: Crate) -> Hash {
    use hir::input_owners::discover_identity_inputs;
    let inputs = discover_identity_inputs(db, krate);
    let unresolved = inputs.unresolved;
    let mut names: Vec<_> = inputs
        .source_names
        .iter()
        .map(|read| {
            let root = db.file_source_root(read.file).source_root_id(db);
            let root = db.source_root(root).source_root(db);
            (root.path_for_file(&read.file).map(ToString::to_string), read.name.clone())
        })
        .collect();
    names.sort();
    names.dedup();
    let mut hash = Sha256::new();
    hash.update(b"ra-jit semantic filenames 1");
    hash.update([u8::from(unresolved)]);
    for (path, name) in &names {
        hash.update([u8::from(path.is_some())]);
        let bytes = path.as_deref().unwrap_or_default().as_bytes();
        hash.update((bytes.len() as u64).to_le_bytes());
        hash.update(bytes);
        hash.update([u8::from(name.is_some())]);
        let bytes = name.as_deref().unwrap_or_default().as_bytes();
        hash.update((bytes.len() as u64).to_le_bytes());
        hash.update(bytes);
    }
    if names.iter().any(|(path, name)| path.is_none() || name.is_none()) {
        let mut paths = Vec::new();
        for dependency in krate.transitive_deps(db).into_iter().chain([krate]) {
            let root = db.file_source_root(dependency.data(db).root_file_id).source_root_id(db);
            let root = db.source_root(root).source_root(db);
            paths.extend(
                root.iter().filter_map(|file| root.path_for_file(&file).map(ToString::to_string)),
            );
            paths.extend(
                db.included_inputs(dependency).into_iter().map(|(path, _)| path.to_string()),
            );
        }
        paths.sort();
        paths.dedup();
        hash.update(b"unresolved constant context");
        for path in paths {
            hash.update((path.len() as u64).to_le_bytes());
            hash.update(path.as_bytes());
        }
    }
    hash.finalize().into()
}

pub fn file_hash(db: &dyn HirDatabase, file: vfs::FileId) -> Hash {
    #[salsa::interned]
    struct IdentityFile {
        #[returns(copy)]
        file: vfs::FileId,
    }
    #[salsa::tracked(returns(copy))]
    fn hash<'db>(db: &'db dyn HirDatabase, file: IdentityFile<'db>) -> Hash {
        Sha256::digest(db.file_text(file.file(db)).text(db).as_bytes()).into()
    }
    hash(db, IdentityFile::new(db, file))
}

#[salsa::tracked(returns(copy))]
pub fn macro_producer_identity(db: &dyn HirDatabase, krate: Crate) -> Hash {
    let mut hash = Sha256::new();
    hash.update(b"ra-jit loaded macro producer 1");
    if let Some(macros) = hir::ProcMacros::get_for_crate(db, krate)
        && let Some(macros) = macros.macros()
    {
        let mut identities: Vec<_> = macros
            .iter()
            .map(|item| (item.name.as_str(), item.expander.content_identity()))
            .collect();
        identities.sort_unstable();
        for (name, identity) in identities {
            hash.update((name.len() as u64).to_le_bytes());
            hash.update(name.as_bytes());
            hash.update([u8::from(identity.is_some())]);
            if let Some(identity) = identity {
                hash.update(identity);
            }
        }
    }
    hash.finalize().into()
}

/// Source roots are conservative package boundaries. Relative names preserve
/// checkout portability; bytes distinguish mutable vendored registry sources.
#[salsa::tracked(returns(copy))]
pub fn direct_source_hash(db: &dyn HirDatabase, krate: Crate) -> Hash {
    hir::input_owners::discover_current_inputs(db, krate);
    let root = db.file_source_root(krate.data(db).root_file_id).source_root_id(db);
    let sources = db.source_root(root).source_root(db);
    let mut hashed: Vec<_> = sources
        .iter()
        .filter_map(|file| {
            let path = sources.path_for_file(&file)?.as_path()?.to_string();
            let hash = if db.has_file_text(file) {
                input_hash(true, file_hash(db, file))
            } else {
                // Register binary source-root files with the same tracked reader
                // and watcher used by include_bytes!, rather than read raw disk in
                // a memoized query without a dependency.
                let input = db.included_file(
                    krate,
                    crate::base_db::AnchoredPath { anchor: file, path: &path },
                );
                let bytes = input.and_then(|input| input.bytes(db).clone());
                input_hash(
                    bytes.is_some(),
                    Sha256::digest(bytes.as_deref().unwrap_or_default()).into(),
                )
            };
            Some((source_role(db, krate, &path), hash))
        })
        .collect();
    let current = hir::input_owners::current_input_paths(db, krate);
    hashed.extend(
        db.included_inputs(krate)
            .into_iter()
            .filter(|(path, _)| current.binary_search(path).is_ok())
            .map(|(path, bytes)| {
                let hash = input_hash(
                    bytes.is_some(),
                    Sha256::digest(bytes.as_deref().unwrap_or_default()).into(),
                );
                (source_role(db, krate, path.as_str()), hash)
            }),
    );
    hashed.sort_unstable();
    hashed.dedup();
    // Roles are already relative to separate declared roots. A common-ancestor
    // pass would erase the package/output distinction for a single-root crate.
    let mut hash = Sha256::new();
    hash.update(b"ra-jit source roles 1");
    for (path, bytes) in hashed {
        hash.update((path.len() as u64).to_le_bytes());
        hash.update(path.as_bytes());
        hash.update(bytes);
    }
    hash.update(semantic_source_hash(db, krate));
    hash.finalize().into()
}

#[salsa::tracked(returns(copy))]
pub fn source_hash(db: &dyn HirDatabase, krate: Crate) -> Hash {
    fn closure(db: &dyn HirDatabase, krate: Crate, seen: &mut FxHashSet<Crate>) -> Hash {
        if !seen.insert(krate) {
            return Sha256::digest(b"already hashed library dependency").into();
        }
        let mut hasher = Sha256::new();
        hasher.update(direct_source_hash(db, krate));
        hasher.update(macro_producer_identity(db, krate));
        let mut dependencies: Vec<_> = krate.data(db).dependencies.iter().collect();
        dependencies.sort_unstable_by(|a, b| a.name.as_str().cmp(b.name.as_str()));
        for dep in dependencies {
            hasher.update((dep.name.as_str().len() as u64).to_le_bytes());
            hasher.update(dep.name.as_str().as_bytes());
            hasher.update(closure(db, dep.crate_id, seen));
        }
        hasher.finalize().into()
    }
    closure(db, krate, &mut FxHashSet::default())
}

pub fn hash_source_files(files: &[(String, Hash)]) -> Hash {
    let common = files.first().map(|(path, _)| Path::new(path).parent().unwrap());
    let common = common.and_then(|first| {
        first.ancestors().find(|dir| files.iter().all(|(path, _)| Path::new(path).starts_with(dir)))
    });
    let mut hasher = Sha256::new();
    hasher.update(b"ra-jit library sources 1");
    for (path, hash) in files {
        let relative = common.and_then(|dir| Path::new(path).strip_prefix(dir).ok());
        let name = relative.unwrap_or(Path::new(path)).to_string_lossy();
        hasher.update((name.len() as u64).to_le_bytes());
        hasher.update(name.as_bytes());
        hasher.update(hash);
    }
    hasher.finalize().into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::base_db::SourceDatabase as _;

    struct GeneratedFixture {
        db: crate::RootDatabase,
        krate: Crate,
        directory: std::path::PathBuf,
    }

    impl Drop for GeneratedFixture {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.directory).unwrap();
        }
    }

    fn generated_fixture(
        package: &str,
        output: &str,
        body: &str,
        generated: &str,
    ) -> GeneratedFixture {
        use test_fixture::WithFixture;
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let directory = std::env::temp_dir().join(format!(
            "include-identity-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        let package =
            directory.join(package.trim_start_matches('/')).to_string_lossy().into_owned();
        let output = directory.join(output.trim_start_matches('/')).to_string_lossy().into_owned();
        let source = format!(
            "//- {package}/src/lib.rs crate:fixture\n\
             #[rustc_builtin_macro] macro_rules! include {{ () => {{}} }}\n\
             #[rustc_builtin_macro] macro_rules! env {{ () => {{}} }}\n\
             #[rustc_builtin_macro] macro_rules! concat {{ () => {{}} }}\n\
             #[rustc_builtin_macro] macro_rules! file {{ () => {{}} }}\n\
             #[rustc_builtin_macro] macro_rules! include_str {{ () => {{}} }}\n\
             {body}\n//- {output}/generated.rs\n{generated}\n"
        );
        let (mut db, files) = crate::RootDatabase::with_many_files(&source);
        let mut set = vfs::file_set::FileSet::default();
        for (file, path) in
            files.iter().zip([format!("{package}/src/lib.rs"), format!("{output}/generated.rs")])
        {
            std::fs::create_dir_all(Path::new(&path).parent().unwrap()).unwrap();
            std::fs::write(&path, db.file_text(file.file_id(&db)).text(&db).as_bytes()).unwrap();
            set.insert(
                file.file_id(&db),
                vfs::VfsPath::from(base_db::AbsPathBuf::assert_utf8(path.into())),
            );
        }
        let mut change = crate::ChangeWithProcMacros::default();
        change.set_roots(vec![base_db::SourceRoot::new_local(set)]);
        db.apply_change(change);
        let krate = base_db::all_crates(&db)[0];
        let mut env = krate.env(&db).clone();
        env.set("CARGO_MANIFEST_DIR", &package);
        env.set("OUT_DIR", &output);
        db.set_env(krate, env);
        GeneratedFixture { db, krate, directory }
    }

    fn generated_identity(package: &str, output: &str, body: &str, generated: &str) -> Hash {
        let fixture = generated_fixture(package, output, body, generated);
        direct_source_hash(&fixture.db, fixture.krate)
    }

    #[test]
    fn generated_includes_have_separate_package_and_output_roots() {
        let body = "include!(concat!(env!(\"OUT_DIR\"), \"/generated.rs\"));";
        let first = generated_identity(
            "/one/package",
            "/one/build/out",
            body,
            "pub fn value() -> u8 { 1 }",
        );
        assert_eq!(
            first,
            generated_identity(
                "/other/deep/package",
                "/elsewhere/out",
                body,
                "pub fn value() -> u8 { 1 }"
            )
        );
        assert_ne!(
            first,
            generated_identity(
                "/other/deep/package",
                "/elsewhere/out",
                body,
                "pub fn value() -> u8 { 2 }"
            )
        );
        assert_eq!(
            generated_identity(
                "/one/package",
                "/one/package/target/first/out",
                body,
                "pub fn value() -> u8 { 1 }"
            ),
            generated_identity(
                "/other/package",
                "/other/package/target/another/out",
                body,
                "pub fn value() -> u8 { 1 }"
            ),
        );
    }

    #[test]
    fn generated_file_macro_keeps_contextual_filename_identity() {
        let body = "include!(concat!(env!(\"OUT_DIR\"), \"/generated.rs\"));";
        assert_ne!(
            generated_identity(
                "/one/package",
                "/one/out",
                body,
                "pub const LENGTH: usize = file!().len();"
            ),
            generated_identity(
                "/other/package",
                "/another/output",
                body,
                "pub const LENGTH: usize = file!().len();"
            ),
        );
    }

    #[test]
    fn include_locator_provenance_preserves_value_uses_and_errors() {
        let include = "include!(concat!(env!(\"OUT_DIR\"), \"/generated.rs\"));";
        for body in
            [include.to_owned(), format!("macro_rules! load {{ () => {{ {include} }} }} load!();")]
        {
            let fixture =
                generated_fixture("/package", "/output", &body, "pub fn value() -> u8 { 1 }");
            let (db, krate) = (&fixture.db, fixture.krate);
            assert!(include_only_environment(db, krate).contains("OUT_DIR"), "{body}");
            assert!(
                db.env_read().iter().any(|(of, key)| *of == krate && key == "OUT_DIR"),
                "legacy dependency read remains exact"
            );
        }
        for (body, generated) in [
            (format!("{include} pub fn length() -> usize {{ env!(\"OUT_DIR\").len() }}"), "pub fn value() -> u8 { 1 }"),
            (include.to_owned(), "pub fn length() -> usize { env!(\"OUT_DIR\").len() }"),
            ("include!(concat!(env!(\"OUT_DIR\"), \"/missing.rs\"));".to_owned(), "pub fn value() -> u8 { 1 }"),
            ("macro_rules! duplicate { ($path:expr) => { include!($path); pub const PATH: &str = $path; } } duplicate!(concat!(env!(\"OUT_DIR\"), \"/generated.rs\"));".to_owned(), "pub fn value() -> u8 { 1 }"),
        ] {
            let fixture = generated_fixture("/package", "/output", &body, generated);
            let (db, krate) = (&fixture.db, fixture.krate);
            assert!(!include_only_environment(db, krate).contains("OUT_DIR"), "{body}: {generated}");
        }
    }

    #[test]
    fn removed_value_use_retracts_without_erasing_dependency_reads() {
        let semantic = "pub fn length() -> usize { env!(\"OUT_DIR\").len() }";
        let body = format!("include!(concat!(env!(\"OUT_DIR\"), \"/generated.rs\")); {semantic}");
        let mut fixture =
            generated_fixture("/package", "/output", &body, "pub fn value() -> u8 { 1 }");
        let (db, krate) = (&mut fixture.db, fixture.krate);
        assert!(!include_only_environment(db, krate).contains("OUT_DIR"));
        let file = krate.data(db).root_file_id;
        let source = db.file_text(file).text(db).replace(semantic, "");
        db.set_file_text(file, &source);
        assert!(include_only_environment(db, krate).contains("OUT_DIR"));
        assert!(db.env_read().iter().any(|(of, key)| *of == krate && key == "OUT_DIR"));
    }

    #[test]
    fn returned_macro_reads_replay_and_retract_attribute_value_uses() {
        let attribute = r#"#[unsafe(export_name = concat!(env!("OUT_DIR"), "symbol"))]"#;
        let body = format!(
            r#"include!(concat!(env!("OUT_DIR"), "/generated.rs")); {attribute} pub fn exported() {{}}"#
        );
        let mut fixture =
            generated_fixture("/package", "/output", &body, "pub fn value() -> u8 { 1 }");
        let (db, krate) = (&mut fixture.db, fixture.krate);
        let first = hir::input_owners::discover_current_inputs(db, krate).clone();
        assert_eq!(&first, hir::input_owners::discover_current_inputs(db, krate));
        let reads: Vec<_> = first.environment.iter().filter(|read| read.key == "OUT_DIR").collect();
        assert_eq!(reads.len(), 2);
        assert!(reads.iter().any(|read| read.include_input.is_some()));
        assert!(reads.iter().any(|read| read.include_input.is_none()));
        assert!(!include_only_environment(db, krate).contains("OUT_DIR"));
        let file = krate.data(db).root_file_id;
        let source = db.file_text(file).text(db).replace(attribute, "");
        db.set_file_text(file, &source);
        let current = hir::input_owners::discover_current_inputs(db, krate);
        assert_eq!(current.environment.iter().filter(|read| read.key == "OUT_DIR").count(), 1);
        assert!(include_only_environment(db, krate).contains("OUT_DIR"));
        assert!(db.env_read().iter().any(|(of, key)| *of == krate && key == "OUT_DIR"));
    }

    #[test]
    fn runtime_file_macro_keeps_typed_relocation_available() {
        let body = "include!(concat!(env!(\"OUT_DIR\"), \"/generated.rs\"));";
        assert_eq!(
            generated_identity(
                "/one/package",
                "/one/out",
                body,
                "pub fn value() -> &'static str { file!() }"
            ),
            generated_identity(
                "/other/package",
                "/another/output",
                body,
                "pub fn value() -> &'static str { file!() }"
            ),
        );
    }

    #[test]
    fn literal_array_lengths_do_not_make_generated_sources_contextual() {
        let body = "include!(concat!(env!(\"OUT_DIR\"), \"/generated.rs\"));";
        let source = "pub fn value() -> [u8; 1] { [7] }";
        assert_eq!(
            generated_identity("/one/package", "/one/out", body, source),
            generated_identity("/other/package", "/another/output", body, source),
        );
    }

    #[test]
    fn type_level_filename_observations_keep_layout_contextual() {
        let body = "include!(concat!(env!(\"OUT_DIR\"), \"/generated.rs\"));";
        for generated in [
            "pub struct Width([u8; file!().len()]); pub const LENGTH: usize = core::mem::size_of::<Width>();",
            "pub fn value() -> [u8; file!().len()] { loop {} }",
            "pub struct Width<const N: usize = { file!().len() }>([u8; N]);",
        ] {
            assert_ne!(
                generated_identity("/one/package", "/one/out", body, generated),
                generated_identity("/other/package", "/another/output", body, generated),
                "{generated}",
            );
        }
    }

    #[test]
    fn dependency_anonymous_type_context_reaches_the_callers_source_identity() {
        use test_fixture::WithFixture;
        for body in ["let _: [u8; file!().len()]; 1", "size_of::<[u8; file!().len()]>()"] {
            let source = format!(
                r#"
//- /app/main.rs crate:app deps:dep
pub fn run() -> usize {{ dep::width() }}
//- /dep/lib.rs crate:dep
#[rustc_builtin_macro] macro_rules! file {{ () => {{}} }}
#[rustc_intrinsic] fn size_of<T>() -> usize;
pub const fn width() -> usize {{ {body} }}
"#
            );
            let (mut db, files) = crate::RootDatabase::with_many_files(&source);
            let named = |name: &str| {
                *base_db::all_crates(&db)
                    .iter()
                    .find(|krate| {
                        krate.extra_data(&db).display_name.as_ref().unwrap().to_string() == name
                    })
                    .unwrap()
            };
            let app = named("app");
            let dep = named("dep");
            let file = files[1].file_id(&db);
            let mut hashes = Vec::new();
            for name in ["dependency/short.rs", "dependency/a-much-longer-name.rs"] {
                hir::mir::location::name_sources(&mut db, move |_, source| {
                    Some(if source == file { name.to_owned() } else { "app/main.rs".to_owned() })
                });
                let current = hir::input_owners::discover_identity_inputs(&db, dep);
                assert!(current.unresolved, "the nonliteral type constant keeps exact context");
                assert!(
                    current
                        .source_names
                        .iter()
                        .any(|read| read.file == file && read.name.as_deref() == Some(name))
                );
                hashes.push((
                    semantic_source_hash(&db, app),
                    semantic_source_hash(&db, dep),
                    source_hash(&db, app),
                ));
            }
            assert_eq!(hashes[0].0, hashes[1].0, "the caller has no filename observation");
            assert_ne!(hashes[0].1, hashes[1].1, "the dependency owns the anonymous constant");
            assert_ne!(hashes[0].2, hashes[1].2, "dependency content enters its caller's key");
        }
    }

    #[test]
    fn macro_numeric_array_bounds_do_not_bind_source_filenames() {
        let body = "include!(concat!(env!(\"OUT_DIR\"), \"/generated.rs\"));";
        let source = "macro_rules! width { () => { 2 + 2 } } const WIDTH: usize = width!(); pub fn value() -> [u8; WIDTH] { [0; WIDTH] }";
        assert_eq!(
            generated_identity("/one/package", "/one/out", body, source),
            generated_identity("/other/package", "/another/output", body, source),
        );
    }

    #[test]
    fn numeric_constant_dependency_proofs_respect_scopes_and_generics() {
        let body = "include!(concat!(env!(\"OUT_DIR\"), \"/generated.rs\"));";
        for source in [
            "mod values { pub const WIDTH: usize = 4; } use values::WIDTH as BOUND; pub fn value() -> [u8; BOUND] { [0; BOUND] }",
            "const WIDTH: usize = 4; pub fn value() -> [u8; 8] { const WIDTH: usize = 8; [0; WIDTH] }",
            "pub fn value<const N: usize>() -> [u8; N] { [0; N] }",
            "const WIDTH: usize = (2u64 * 2) as usize; pub fn value() -> [u8; WIDTH] { [0; WIDTH] }",
            "const MUL: f64 = 1.0 / (1u64 << 63) as f64; pub fn value() -> f64 { MUL }",
            "pub const fn width() -> usize { 4 } pub fn value() -> [u8; width()] { [0; width()] }",
            "pub const fn width<T>() -> usize { 4 } pub fn value() -> [u8; width::<u8>()] { [0; width::<u8>()] }",
        ] {
            assert_eq!(
                generated_identity("/one/package", "/one/out", body, source),
                generated_identity("/other/package", "/another/output", body, source),
                "{source}",
            );
        }
    }

    #[test]
    fn generic_type_layout_queries_do_not_observe_source_names() {
        let body = "include!(concat!(env!(\"OUT_DIR\"), \"/generated.rs\"));";
        let source = "#[rustc_intrinsic] fn align_of<T>() -> usize; trait Properties { const ALIGN: usize = align_of::<Self>(); } impl<T> Properties for T {} const fn alignment<T>() -> usize { <T as Properties>::ALIGN } struct Array<T>(T); trait Pointable { const ALIGN: usize; } impl<T> Pointable for Array<T> { const ALIGN: usize = alignment::<Array<T>>(); } pub fn value() -> u8 { 1 }";
        assert_eq!(
            generated_identity("/one/package", "/one/out", body, source),
            generated_identity("/other/package", "/another/output", body, source),
        );
    }

    #[test]
    fn required_associated_constants_do_not_observe_declaration_filenames() {
        let body = "include!(concat!(env!(\"OUT_DIR\"), \"/generated.rs\"));";
        for source in [
            "pub trait Vector { const BYTES: usize; const ALIGN: usize; } impl Vector for u8 { const BYTES: usize = 16; const ALIGN: usize = Self::BYTES - 1; } pub fn value() -> u8 { 1 }",
            "pub trait Vector { const BYTES: usize; } pub struct Width<V>(V); impl<V: Vector> Width<V> { const LOOP_SIZE: usize = 2 * V::BYTES; } pub fn value() -> u8 { 1 }",
        ] {
            assert_eq!(
                generated_identity("/one/package", "/one/out", body, source),
                generated_identity("/other/package", "/another/output", body, source),
                "{source}",
            );
        }
    }

    #[test]
    fn associated_constant_implementation_context_reaches_generic_callers() {
        use test_fixture::WithFixture;
        for (declaration, value, contextual) in
            ["const BYTES: usize;", "const BYTES: usize = 8;"].into_iter().flat_map(|declaration| {
                [("16", false), ("file!().len()", true)]
                    .map(|(value, contextual)| (declaration, value, contextual))
            })
        {
            let source = format!(
                r#"
//- /app/main.rs crate:app deps:dep,values
pub fn run() -> usize {{ dep::width::<values::Value>() }}
//- /dep/lib.rs crate:dep
pub trait Vector {{ {declaration} }}
pub struct Double<T>(T);
impl<T: Vector> Double<T> {{ pub const BYTES: usize = 2 * T::BYTES; }}
pub fn width<T: Vector>() -> usize {{ Double::<T>::BYTES }}
//- /values/lib.rs crate:values deps:dep
#[rustc_builtin_macro] macro_rules! file {{ () => {{}} }}
pub struct Value;
impl dep::Vector for Value {{ const BYTES: usize = {value}; }}
"#
            );
            let (mut db, files) = crate::RootDatabase::with_many_files(&source);
            let named = |name: &str| {
                *base_db::all_crates(&db)
                    .iter()
                    .find(|krate| {
                        krate.extra_data(&db).display_name.as_ref().unwrap().to_string() == name
                    })
                    .unwrap()
            };
            let [app, dep, values] = ["app", "dep", "values"].map(named);
            let file = files[2].file_id(&db);
            let mut hashes = Vec::new();
            for name in ["values/short.rs", "values/a-much-longer-name.rs"] {
                hir::mir::location::name_sources(&mut db, move |_, source| {
                    Some(if source == file { name.to_owned() } else { "unchanged.rs".to_owned() })
                });
                let generic = hir::input_owners::discover_identity_inputs(&db, dep);
                assert!(!generic.unresolved);
                assert!(generic.source_names.is_empty());
                let implementation = hir::input_owners::discover_identity_inputs(&db, values);
                assert_eq!(!implementation.source_names.is_empty(), contextual);
                hashes.push((
                    semantic_source_hash(&db, dep),
                    semantic_source_hash(&db, values),
                    source_hash(&db, app),
                ));
            }
            assert_eq!(hashes[0].0, hashes[1].0, "the generic definition has no filename read");
            assert_eq!(
                hashes[0].1 != hashes[1].1,
                contextual,
                "the implementation owns its context"
            );
            assert_eq!(
                hashes[0].2 != hashes[1].2,
                contextual,
                "the caller retains the implementation's context"
            );
        }
    }

    #[test]
    fn declarative_macros_cannot_hide_filename_dependent_type_roots() {
        let body = "include!(concat!(env!(\"OUT_DIR\"), \"/generated.rs\"));";
        for source in [
            "macro_rules! length { () => { file!().len() } } const WIDTH: usize = length!(); pub struct Value([u8; WIDTH]);",
            "macro_rules! forward { ($value:expr) => { $value } } const WIDTH: usize = forward!(file!().len()); pub struct Value([u8; WIDTH]);",
            "macro_rules! name { () => { concat!(file!(), \".generated\") } } const NAME: &str = name!(); pub struct Value([u8; NAME.len()]);",
            "trait Size { const WIDTH: usize; } struct Type; impl Size for Type { const WIDTH: usize = file!().len(); } pub struct Value([u8; <Type as Size>::WIDTH]);",
            "struct Type<T>(T); impl<T> Type<T> { const WIDTH: usize = file!().len(); } pub struct Value([u8; Type::<u8>::WIDTH]);",
            "pub trait Size { const VALUE: [u8; file!().len()]; }",
            "pub trait Size { const WIDTH: usize = file!().len(); }",
            "const fn align_of<T>() -> usize { file!().len() } struct Value<T>(T); impl<T> Value<T> { const ALIGN: usize = align_of::<T>(); }",
            "#[rustc_intrinsic] fn size_of<T>() -> usize; const WIDTH: usize = size_of::<[u8; file!().len()]>();",
        ] {
            assert_ne!(
                generated_identity("/one/package", "/one/out", body, source),
                generated_identity("/other/package", "/another/output", body, source),
                "{source}",
            );
        }
    }

    #[test]
    fn named_numeric_array_bounds_do_not_bind_source_filenames() {
        let body = "include!(concat!(env!(\"OUT_DIR\"), \"/generated.rs\"));";
        let source = "const WIDTH: usize = 4; pub fn value() -> [u8; WIDTH] { [0; WIDTH] }";
        let identity = |package, output| {
            let mut fixture = generated_fixture(package, output, body, source);
            hir::mir::location::name_sources(&mut fixture.db, |db, file| {
                let root = db.file_source_root(file).source_root_id(db);
                db.source_root(root).source_root(db).path_for_file(&file).map(ToString::to_string)
            });
            let reads =
                hir::input_owners::discover_identity_inputs(&fixture.db, fixture.krate).clone();
            let semantic = semantic_source_hash(&fixture.db, fixture.krate);
            (direct_source_hash(&fixture.db, fixture.krate), semantic, reads)
        };
        let first = identity("/one/package", "/one/out");
        let second = identity("/other/package", "/another/output");
        assert_eq!(first.1, second.1, "first differing leaf: {:#?} / {:#?}", first.2, second.2);
        assert_eq!(first.0, second.0);
        assert!(first.2.source_names.is_empty());
        assert!(!first.2.unresolved);
    }

    #[test]
    fn parent_components_cannot_certify_output_ownership() {
        let body = "include!(concat!(env!(\"OUT_DIR\"), \"/../generated.rs\"));";
        let mut fixture =
            generated_fixture("/package", "/output", body, "pub fn value() -> u8 { 1 }");
        let krate = fixture.krate;
        let mut env = krate.env(&fixture.db).clone();
        let output = Path::new(&env.get("OUT_DIR").unwrap()).join("nested");
        std::fs::create_dir(&output).unwrap();
        env.set("OUT_DIR", output.to_string_lossy());
        fixture.db.set_env(krate, env);
        assert!(!include_only_environment(&fixture.db, krate).contains("OUT_DIR"));
    }

    #[cfg(unix)]
    #[test]
    fn symlink_escape_with_equal_bytes_invalidates_ownership() {
        let body = "include!(concat!(env!(\"OUT_DIR\"), \"/generated.rs\"));";
        let mut fixture =
            generated_fixture("/package", "/output", body, "pub fn value() -> u8 { 1 }");
        let krate = fixture.krate;
        assert!(include_only_environment(&fixture.db, krate).contains("OUT_DIR"));
        let path = Path::new(&krate.env(&fixture.db).get("OUT_DIR").unwrap()).join("generated.rs");
        let outside = fixture.directory.join("outside.rs");
        std::fs::rename(&path, &outside).unwrap();
        std::os::unix::fs::symlink(&outside, &path).unwrap();
        let path = base_db::AbsPathBuf::assert_utf8(path);
        assert!(
            fixture.db.reload_included_file(&path),
            "ownership must change even when bytes do not"
        );
        assert!(!include_only_environment(&fixture.db, krate).contains("OUT_DIR"));
        assert!(source_role(&fixture.db, krate, path.as_str()).starts_with("external/"));
    }

    #[cfg(unix)]
    #[test]
    fn shallower_view_does_not_borrow_ownership_from_native_ancestors() {
        let body = "include!(concat!(env!(\"OUT_DIR\"), \"/subdir/generated.rs\"));";
        let mut fixture = generated_fixture("/a", "/a/subdir", body, "pub fn value() -> u8 { 1 }");
        let krate = fixture.krate;
        let mut env = krate.env(&fixture.db).clone();
        let package = env.get("CARGO_MANIFEST_DIR").unwrap();
        let logical = Path::new(&package).join("subdir");
        env.set("OUT_DIR", &package);
        fixture.db.set_env(krate, env);
        let physical = fixture.directory.join("b");
        std::fs::create_dir(&physical).unwrap();
        let outside = fixture.directory.join("outside.rs");
        std::fs::rename(logical.join("generated.rs"), &outside).unwrap();
        std::os::unix::fs::symlink(&outside, physical.join("generated.rs")).unwrap();
        fixture.db.set_view(&logical, &physical, Vec::new());
        assert!(!include_only_environment(&fixture.db, krate).contains("OUT_DIR"));
        assert!(
            source_role(&fixture.db, krate, logical.join("generated.rs").to_str().unwrap())
                .starts_with("external/")
        );
    }

    #[test]
    fn nested_path_payload_bytes_remain_exact() {
        let identity = |package, output| {
            let body = "include!(include_str!(concat!(env!(\"OUT_DIR\"), \"/locator.txt\")));";
            let fixture = generated_fixture(package, output, body, "pub fn value() -> u8 { 1 }");
            let output = fixture.krate.env(&fixture.db).get("OUT_DIR").unwrap();
            std::fs::write(
                Path::new(&output).join("locator.txt"),
                format!("{output}/generated.rs"),
            )
            .unwrap();
            let hash = direct_source_hash(&fixture.db, fixture.krate);
            assert!(include_only_environment(&fixture.db, fixture.krate).contains("OUT_DIR"));
            hash
        };
        assert_ne!(
            identity("/one/package", "/one/output"),
            identity("/another/package", "/another/output")
        );
    }

    #[test]
    fn included_text_identity_is_independent_of_vfs_materialization() {
        let body = "pub fn value() -> &'static str { include_str!(concat!(env!(\"OUT_DIR\"), \"/generated.rs\")) }";
        let identity = |in_vfs: bool| {
            let mut fixture = generated_fixture("/package", "/output", body, "payload");
            if !in_vfs {
                let file = fixture.krate.data(&fixture.db).root_file_id;
                let root = fixture.db.file_source_root(file).source_root_id(&fixture.db);
                let root = fixture.db.source_root(root).source_root(&fixture.db);
                let mut files = vfs::file_set::FileSet::default();
                files.insert(file, root.path_for_file(&file).unwrap().clone());
                let mut change = crate::ChangeWithProcMacros::default();
                change.set_roots(vec![base_db::SourceRoot::new_local(files)]);
                fixture.db.apply_change(change);
            }
            direct_source_hash(&fixture.db, fixture.krate)
        };
        assert_eq!(identity(true), identity(false));
    }

    #[test]
    fn included_text_identity_uses_vfs_edits_over_historical_disk_bytes() {
        let body = "pub fn value() -> &'static str { include_str!(concat!(env!(\"OUT_DIR\"), \"/generated.rs\")) }";
        let mut fixture = generated_fixture("/package", "/output", body, "disk");
        let krate = fixture.krate;
        let file = krate.data(&fixture.db).root_file_id;
        let root = fixture.db.file_source_root(file).source_root_id(&fixture.db);
        let root = fixture.db.source_root(root).source_root(&fixture.db);
        let entries: Vec<_> =
            root.iter().map(|file| (file, root.path_for_file(&file).unwrap().clone())).collect();
        let payload = entries.iter().find(|(id, _)| *id != file).unwrap().0;
        let roots = |db: &mut crate::RootDatabase, hydrated| {
            let mut files = vfs::file_set::FileSet::default();
            for (id, path) in &entries {
                if hydrated || *id == file {
                    files.insert(*id, path.clone());
                }
            }
            let mut change = crate::ChangeWithProcMacros::default();
            change.set_roots(vec![base_db::SourceRoot::new_local(files)]);
            db.apply_change(change);
        };
        roots(&mut fixture.db, false);
        let external = direct_source_hash(&fixture.db, krate);
        let old = fixture.db.included_by(krate)[0].1.clone();
        roots(&mut fixture.db, true);
        assert_eq!(external, direct_source_hash(&fixture.db, krate));
        fixture.db.set_file_text(payload, "vfs-only");
        let changed = direct_source_hash(&fixture.db, krate);
        assert_ne!(external, changed);
        assert_eq!(fixture.db.included_by(krate)[0].1, old);
        let mut fresh = generated_fixture("/package", "/output", body, "vfs-only");
        let file = fresh.krate.data(&fresh.db).root_file_id;
        let root = fresh.db.file_source_root(file).source_root_id(&fresh.db);
        let root = fresh.db.source_root(root).source_root(&fresh.db);
        let payload = root.iter().find(|id| *id != file).unwrap();
        fresh.db.set_file_text(payload, "vfs-only");
        assert_eq!(changed, direct_source_hash(&fresh.db, fresh.krate));
    }

    #[test]
    fn source_identity_tracks_relocation_and_bytes() {
        let files = |root: &str, body: &[u8]| {
            vec![(format!("{root}/vendor/src/lib.rs"), Sha256::digest(body).into())]
        };
        assert_eq!(
            hash_source_files(&files("/a/project", b"one")),
            hash_source_files(&files("/b/project", b"one"))
        );
        assert_ne!(
            hash_source_files(&files("/a/project", b"one")),
            hash_source_files(&files("/a/project", b"two"))
        );
    }

    #[test]
    fn file_identity_is_a_revision_tracked_query() {
        use test_fixture::WithFixture;
        let (mut db, file) = crate::RootDatabase::with_single_file("pub fn value() -> u8 { 1 }");
        let file = file.file_id(&db);
        let before = file_hash(&db, file);
        assert_eq!(before, file_hash(&db, file));
        db.set_file_text(file, "pub fn value() -> u8 { 2 }");
        assert_ne!(before, file_hash(&db, file));
    }
}
