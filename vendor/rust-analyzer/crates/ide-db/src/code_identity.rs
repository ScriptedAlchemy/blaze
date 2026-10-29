//! Source/code identities over tracked inputs, including external includes and macros.

use std::path::Path;

use hir::db::HirDatabase;
use rustc_hash::FxHashSet;
use sha2::{Digest, Sha256};

use crate::base_db::Crate;

pub type Hash = [u8; 32];

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
                file_hash(db, file)
            } else {
                // Register binary source-root files with the same tracked reader
                // and watcher used by include_bytes!, rather than read raw disk in
                // a memoized query without a dependency.
                let input = db.included_file(
                    krate,
                    crate::base_db::AnchoredPath { anchor: file, path: &path },
                );
                Sha256::digest(
                    input.and_then(|input| input.bytes(db).clone()).as_deref().unwrap_or_default(),
                )
                .into()
            };
            Some((path, hash))
        })
        .collect();
    hashed.extend(db.included_inputs(krate).into_iter().map(|(path, bytes)| {
        let mut hash = Sha256::new();
        hash.update([u8::from(bytes.is_some())]);
        hash.update(bytes.as_deref().unwrap_or_default());
        (path.as_str().to_owned(), hash.finalize().into())
    }));
    hashed.sort_unstable();
    hash_source_files(&hashed)
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
