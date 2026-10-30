//! What a crate of a checkout is, by what is written in it: two checkouts with the same
//! sources have the same crate, and what was compiled for one is the code of the other.

use std::{
    path::Path,
    sync::{Mutex, OnceLock},
};

use hir_ty_codegen::{CARGO_HOME, ContentId, LibrarySources, moved, stored_as};
#[cfg(test)]
use ide_db::code_identity::hash_source_files;
use ide_db::code_identity::macro_producer_identity;
use ide_db::{
    RootDatabase,
    base_db::{Crate, SourceDatabase as _},
};
use rustc_hash::{FxHashMap, FxHashSet};
use sha2::{Digest, Sha256};
use triomphe::Arc;

type Hash = [u8; 32];

#[derive(Debug)]
pub(crate) enum EnvironmentValue {
    Text(String),
    Unset,
    NonUnicode,
}

pub(crate) type EnvironmentReads = Vec<(String, EnvironmentValue)>;

/// The crates of one build. An edit makes another crate of the one it is in and of every
/// crate of the checkout that depends on it.
#[derive(Default)]
pub(crate) struct Contents {
    crates: Mutex<FxHashMap<Crate, Option<ContentId>>>,
    source_hashes: Mutex<FxHashMap<Crate, Hash>>,
    environments: Mutex<FxHashMap<Crate, Hash>>,
    macro_consumers: Mutex<FxHashMap<Crate, bool>>,
    /// Where the build scripts of the crates hashed so far wrote to.
    places: Mutex<FxHashSet<String>>,
    /// Where cargo keeps the sources it fetched, once the crates were asked.
    cargo_home: std::sync::OnceLock<Option<String>>,
    /// Where the sources of the standard library are and were built, once asked.
    library: std::sync::OnceLock<Option<LibrarySources>>,
}

impl Contents {
    pub(crate) fn source_hash(&self, db: &RootDatabase, krate: Crate) -> Hash {
        if let Some(hash) = self.source_hashes.lock().unwrap_or_else(|e| e.into_inner()).get(&krate)
        {
            return *hash;
        }
        let hash = ide_db::code_identity::source_hash(db, krate);
        self.source_hashes.lock().unwrap_or_else(|e| e.into_inner()).insert(krate, hash);
        hash
    }

    /// A reexport may put a macro in scope without a direct proc-macro dependency.
    /// Their environment accesses are not observed, so their consumers retain the
    /// complete crate environment in their identity.
    fn uses_macros(&self, db: &RootDatabase, krate: Crate) -> bool {
        if let Some(&known) =
            self.macro_consumers.lock().unwrap_or_else(|e| e.into_inner()).get(&krate)
        {
            return known;
        }
        let data = krate.data(db);
        let uses = data.target_kind.is_proc_macro()
            || data.dependencies.iter().any(|dep| self.uses_macros(db, dep.crate_id));
        self.macro_consumers.lock().unwrap_or_else(|e| e.into_inner()).insert(krate, uses);
        uses
    }

    fn keyed_environment(&self, db: &RootDatabase, krate: Crate) -> ide_db::base_db::Env {
        let mut env = krate.env(db).clone();
        if !self.uses_macros(db, krate) {
            // These exact cache-service controls do not affect Rust semantics unless
            // source reads them. Such reads are hashed separately, before an entry is
            // admitted. This copy only forms an identity: expansion and producers keep
            // the original values, including native non-Unicode values.
            for key in [
                "RA_JIT_CACHE",
                "RA_JIT_CACHE_MAX",
                "RA_JIT_CACHE_SEAL_AT",
                "RA_JIT_CACHE_CHECK",
                "RA_JIT_REMOTE",
                "RA_JIT_PROJECT",
                "RA_JIT_REMOTE_READONLY",
                "RA_JIT_REMOTE_LAZY",
                "RA_JIT_REMOTE_PUSH_EVERY",
                "RA_JIT_REMOTE_JOBS",
                "RA_JIT_REMOTE_TIMEOUT",
                "RA_JIT_REMOTE_MAX",
                "RA_JIT_REMOTE_TRUST",
                "RA_JIT_REMOTE_UNSIGNED",
                "RA_JIT_REMOTE_SIGN",
            ] {
                env.set(key, "");
            }
        }
        env
    }

    fn normalized_environment(
        &self,
        db: &RootDatabase,
        krate: Crate,
        checkouts: (&Path, Option<&Path>),
        mut text: String,
    ) -> String {
        let env = krate.env(db);
        let manifest = env.get("CARGO_MANIFEST_DIR");
        let out = env.get("OUT_DIR");
        let mut places: Vec<_> =
            manifest.as_deref().map(|it| (it, "$PACKAGE")).into_iter().collect();
        places.extend(out.as_deref().map(|it| (it, "$OUT")));
        let (checkout, base) =
            (checkouts.0.to_string_lossy(), checkouts.1.map(Path::to_string_lossy));
        places.push((&*checkout, "$CHECKOUT"));
        places.extend(base.as_deref().map(|it| (it, "$CHECKOUT")));
        // A package and its generated files may be below the checkout. Replace the
        // longest path first, using the same convention as the existing crate key.
        places.sort_unstable_by_key(|(path, _)| std::cmp::Reverse(path.len()));
        for (path, stands_for) in places.into_iter().filter(|(path, _)| !path.is_empty()) {
            text = text.replace(path, stands_for);
        }
        if let Some(out) = out {
            self.places.lock().unwrap_or_else(|e| e.into_inner()).insert(out);
        }
        text
    }

    fn environment_hash(
        &self,
        db: &RootDatabase,
        krate: Crate,
        environment_reads: &dyn Fn(Crate) -> EnvironmentReads,
    ) -> Hash {
        if let Some(&known) =
            self.environments.lock().unwrap_or_else(|e| e.into_inner()).get(&krate)
        {
            return known;
        }
        // Discovery covers bodies, consts, discriminants, type signatures, and generic
        // defaults. Identity must not depend on which lazy owner was requested first.
        hir::input_owners::discover_current_inputs(db, krate);
        let mut hasher = Sha256::new();
        let mut field = |bytes: &[u8]| {
            hasher.update((bytes.len() as u64).to_le_bytes());
            hasher.update(bytes);
        };
        field(b"ra-jit observed environment 1");
        let reads = environment_reads(krate);
        // Observed values remain exact. Even a checkout path can be used for a
        // compile-time length or comparison, leaving no relocatable string behind.
        field(&(reads.len() as u64).to_le_bytes());
        for (key, value) in reads {
            field(key.as_bytes());
            match value {
                EnvironmentValue::Unset => field(&[0]),
                EnvironmentValue::NonUnicode => field(&[1]),
                EnvironmentValue::Text(value) => {
                    field(&[2]);
                    field(value.as_bytes());
                }
            }
        }
        let macros = self.uses_macros(db, krate);
        field(&[u8::from(macros)]);
        if macros {
            field(format!("{:?}", krate.env(db)).as_bytes());
        }
        // A caller may inline a library body, including its expanded environment
        // constants. Source bytes and a library's linkage id alone do not cover these.
        let mut dependencies: Vec<_> = krate.data(db).dependencies.iter().collect();
        dependencies.sort_unstable_by(|a, b| a.name.as_str().cmp(b.name.as_str()));
        for dependency in dependencies {
            field(dependency.name.as_str().as_bytes());
            field(&self.environment_hash(db, dependency.crate_id, environment_reads));
        }
        let hash = hasher.finalize().into();
        self.environments.lock().unwrap_or_else(|e| e.into_inner()).insert(krate, hash);
        hash
    }

    /// `None` for a crate that depends on one `stable_crate_id` has nothing for and that is
    /// not of the checkout.
    pub(crate) fn id(
        &self,
        db: &RootDatabase,
        checkouts: (&Path, Option<&Path>),
        krate: Crate,
        stable_crate_id: &dyn Fn(Crate) -> Option<u64>,
        environment_reads: &dyn Fn(Crate) -> EnvironmentReads,
    ) -> Option<ContentId> {
        if let Some(&known) = self.crates.lock().unwrap_or_else(|e| e.into_inner()).get(&krate) {
            return known;
        }
        let id = self.hash(db, checkouts, krate, stable_crate_id, environment_reads);
        self.crates.lock().unwrap_or_else(|e| e.into_inner()).insert(krate, id);
        id
    }

    /// Where cargo keeps the sources it fetched, when the crates that are not of the
    /// checkout are under one such directory: what is stored names it the same on every
    /// machine. With none, or with more than one, nothing is renamed.
    pub(crate) fn cargo_home(&self, db: &RootDatabase) -> Option<String> {
        let found = || {
            let crates = ide_db::base_db::all_crates(db);
            let outside = crates.iter().filter(|it| !it.data(db).origin.is_local());
            let roots = outside.filter_map(|&it| crate::crate_root_file(db, it.into()));
            let homes: FxHashSet<String> =
                roots.filter_map(|it| Some(cargo_home_of(&it)?.to_str()?.to_owned())).collect();
            let mut homes = homes.into_iter();
            homes.next().filter(|_| homes.next().is_none())
        };
        self.cargo_home.get_or_init(found).clone()
    }

    /// Where the standard library's sources are installed, and the `/rustc/<commit>` they
    /// were built in, when the toolchain they are in tells its commit.
    pub(crate) fn library_sources(&self, db: &RootDatabase) -> Option<LibrarySources> {
        const INSTALLED: &str = "lib/rustlib/src/rust";
        let found = || {
            let crates = ide_db::base_db::all_crates(db);
            let of_library = crates.iter().filter(|it| it.data(db).origin.is_lang());
            let mut roots = of_library.filter_map(|&it| crate::crate_root_file(db, it.into()));
            let toolchain =
                roots.find_map(|it| Some(it.to_str()?.split_once(INSTALLED)?.0.to_owned()))?;
            let commit = crate::rustc_commit(&toolchain)?;
            let here = format!("{toolchain}{INSTALLED}");
            Some(LibrarySources { here, built_in: format!("/rustc/{commit}") })
        };
        self.library.get_or_init(found).clone()
    }

    /// Whether data of a crate hashed here has in it where the checkout or the output of a
    /// build script is, as `env!("CARGO_MANIFEST_DIR")` puts it there: code with such data is
    /// of this checkout alone, whatever the crate is named by.
    pub(crate) fn names_place(&self, checkouts: (&Path, Option<&Path>), bytes: &[u8]) -> bool {
        let within =
            |place: &[u8]| !place.is_empty() && bytes.windows(place.len()).any(|it| it == place);
        within(checkouts.0.as_os_str().as_encoded_bytes())
            || checkouts.1.is_some_and(|it| within(it.as_os_str().as_encoded_bytes()))
            || self
                .places
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .iter()
                .any(|it| within(it.as_bytes()))
    }

    fn hash(
        &self,
        db: &RootDatabase,
        checkouts: (&Path, Option<&Path>),
        krate: Crate,
        stable_crate_id: &dyn Fn(Crate) -> Option<u64>,
        environment_reads: &dyn Fn(Crate) -> EnvironmentReads,
    ) -> Option<ContentId> {
        let data = krate.data(db);
        let mut hasher = Sha256::new();
        let mut field = |bytes: &[u8]| {
            hasher.update((bytes.len() as u64).to_le_bytes());
            hasher.update(bytes);
        };
        field(b"ra-jit crate 4");
        let name = krate.extra_data(db).display_name.as_ref().map(|it| it.to_string());
        field(name.unwrap_or_default().as_bytes());
        field(format!("{:?} {}", data.edition, data.target_kind.is_proc_macro()).as_bytes());
        field(format!("{:?}", data.codegen_flags).as_bytes());
        for attr in &data.crate_attrs {
            field(attr.as_bytes());
        }
        let mut cfgs: Vec<String> =
            krate.cfg_options(db).into_iter().map(|it| it.to_string()).collect();
        cfgs.sort_unstable();
        field(cfgs.join("\n").as_bytes());

        // The files of a source root can be of more crates than this one: two checkouts
        // whose build scripts write to one directory have their packages in one root.
        let env = krate.env(db);
        let manifest = env.get("CARGO_MANIFEST_DIR")?;
        let out = env.get("OUT_DIR");
        let root = db.file_source_root(data.root_file_id).source_root_id(db);
        let root = db.source_root(root).source_root(db);
        let mut files: Vec<(Written, String, Hash)> = Vec::new();
        for file in root.iter() {
            let Some(path) = root.path_for_file(&file).and_then(|it| it.as_path()) else {
                continue;
            };
            let path = path.to_string();
            let path = Path::new(&path);
            let (by, relative) = if let Ok(relative) = path.strip_prefix(&manifest) {
                (Written::InPackage, relative)
            } else if let Some(Ok(relative)) = out.as_ref().map(|out| path.strip_prefix(out)) {
                (Written::ByBuildScript, relative)
            } else {
                continue;
            };
            let hash = file_hash(db, file, path)?;
            files.push((by, relative.to_string_lossy().into_owned(), hash));
        }
        files.sort_unstable();
        for (by, path, hash) in &files {
            field(&[*by as u8]);
            field(path.as_bytes());
            field(hash);
        }
        // Discover reads before keying the environment, including values supplied by
        // Cargo or a build script instead of by the caller's process.
        field(&self.environment_hash(db, krate, environment_reads));
        let env = format!("{:?}", self.keyed_environment(db, krate));
        field(self.normalized_environment(db, krate, checkouts, env).as_bytes());
        // What `include_bytes!` and `include_str!` in it read that no source root has, as it
        // was read: known once the crate is listed, which asking its reads had it be.
        let mut read: Vec<(Written, String, Hash)> = db
            .included_by(krate)
            .into_iter()
            .map(|(path, bytes)| {
                let path = Path::new(path.as_str());
                let (by, path) = if let Ok(relative) = path.strip_prefix(&manifest) {
                    (Written::InPackage, relative)
                } else if let Some(Ok(relative)) = out.as_ref().map(|out| path.strip_prefix(out)) {
                    (Written::ByBuildScript, relative)
                } else {
                    (Written::Elsewhere, path)
                };
                let hash = Sha256::digest(bytes.as_deref().unwrap_or_default()).into();
                (by, path.to_string_lossy().into_owned(), hash)
            })
            .collect();
        read.sort_unstable();
        for (by, path, hash) in &read {
            field(&[*by as u8]);
            field(path.as_bytes());
            field(hash);
        }

        // In the order of their names: the order they are listed in is the build's.
        let mut dependencies: Vec<_> = data.dependencies.iter().collect();
        dependencies.sort_unstable_by(|a, b| a.name.as_str().cmp(b.name.as_str()));
        for dependency in dependencies {
            let on = dependency.crate_id;
            field(dependency.name.as_str().as_bytes());
            field(&macro_producer_identity(db, on));
            match on.data(db).origin.is_local() {
                true => field(&self.id(db, checkouts, on, stable_crate_id, environment_reads)?.0),
                false => {
                    field(&Self::outside(db, on, stable_crate_id(on)));
                    field(&self.source_hash(db, on));
                }
            }
        }
        Some(ContentId(hasher.finalize().into()))
    }

    /// What stands for a crate from outside the checkout that rustc's build of the program
    /// has no id for, one that only exports others' items or a proc macro: where its source is,
    /// what it is configured with and what it is built on. `known` gives rustc's id of a crate,
    /// if there is one, and where its source starts.
    pub(crate) fn library(
        &self,
        db: &RootDatabase,
        krate: Crate,
        root: &Path,
        checkout: &Path,
        known: &dyn Fn(Crate) -> Option<(Option<u64>, std::path::PathBuf)>,
        environment_reads: &dyn Fn(Crate) -> EnvironmentReads,
    ) -> ContentId {
        if let Some(&Some(id)) = self.crates.lock().unwrap_or_else(|e| e.into_inner()).get(&krate) {
            return id;
        }
        let mut hasher = Sha256::new();
        let mut field = |bytes: &[u8]| {
            hasher.update((bytes.len() as u64).to_le_bytes());
            hasher.update(bytes);
        };
        field(b"ra-jit library 8");
        field(format!("{:?}", krate.data(db).codegen_flags).as_bytes());
        field(Self::version(db, krate).as_bytes());
        field(&self.source_hash(db, krate));
        // Where cargo fetched it to is under another directory on another machine, and so is
        // a crate beside the checkout, which the path that reaches it from there names.
        let root = root.to_string_lossy();
        let home = self.cargo_home(db);
        let there = home
            .and_then(|home| moved(&root, &home, CARGO_HOME))
            .or_else(|| stored_as(&root, &checkout.to_string_lossy()));
        field(there.as_deref().unwrap_or(&root).as_bytes());
        let mut cfgs: Vec<String> =
            krate.cfg_options(db).into_iter().map(|it| it.to_string()).collect();
        cfgs.sort_unstable();
        field(cfgs.join("\n").as_bytes());
        field(&self.environment_hash(db, krate, environment_reads));
        let mut dependencies: Vec<_> = krate.data(db).dependencies.iter().collect();
        dependencies.sort_unstable_by(|a, b| a.name.as_str().cmp(b.name.as_str()));
        for dependency in dependencies {
            let on = dependency.crate_id;
            field(dependency.name.as_str().as_bytes());
            field(&macro_producer_identity(db, on));
            match known(on) {
                Some((None, root)) => {
                    field(&self.library(db, on, &root, checkout, known, environment_reads).0)
                }
                Some((id, _)) => {
                    field(&Self::outside(db, on, id));
                    field(&self.source_hash(db, on));
                }
                None => {
                    field(&Self::outside(db, on, None));
                    field(&self.source_hash(db, on));
                }
            }
        }
        let id = ContentId(hasher.finalize().into());
        self.crates.lock().unwrap_or_else(|e| e.into_inner()).insert(krate, Some(id));
        id
    }

    /// What stands for a crate from outside the checkout in the hash of one that is built on
    /// it: its name and version, and rustc's id of it when rustc built something of it. The
    /// id is 64 bits and rustc's to choose, so it is never all that tells two crates apart.
    fn outside(db: &RootDatabase, krate: Crate, rustc_id: Option<u64>) -> Hash {
        let mut hasher = Sha256::new();
        let named = Self::version(db, krate);
        hasher.update((named.len() as u64).to_le_bytes());
        hasher.update(named.as_bytes());
        hasher.update(format!("{:?}", krate.data(db).codegen_flags).as_bytes());
        match rustc_id {
            Some(id) => hasher.update([&[1][..], &id.to_le_bytes()].concat()),
            None => hasher.update([0]),
        }
        hasher.finalize().into()
    }

    /// The name and version of a crate, which one from a registry is the same by wherever
    /// it is.
    fn version(db: &RootDatabase, krate: Crate) -> String {
        let extra = krate.extra_data(db);
        let name = extra.display_name.as_ref().map(|it| it.to_string());
        format!("{name:?} {:?}", extra.version)
    }
}

/// What a build script wrote to `out` that the source root of `krate` holds: the path of each
/// file below `out` and the hash of what is in it, in the order of their paths.
pub(crate) fn written_to(
    db: &RootDatabase,
    krate: Crate,
    out: &str,
) -> Option<Vec<(String, Hash)>> {
    let root = db.file_source_root(krate.data(db).root_file_id).source_root_id(db);
    let root = db.source_root(root).source_root(db);
    let mut files = Vec::new();
    for file in root.iter() {
        let Some(path) = root.path_for_file(&file).and_then(|it| it.as_path()) else {
            continue;
        };
        let path = Path::new(path.as_str());
        let Ok(relative) = path.strip_prefix(out) else { continue };
        files.push((relative.to_string_lossy().into_owned(), file_hash(db, file, path)?));
    }
    files.sort_unstable();
    Some(files)
}

/// The hash of the file `file` at `path`. Native non-text inputs are read from
/// the active filesystem view. A read error supplies no reusable identity.
fn file_hash(db: &RootDatabase, file: vfs::FileId, path: &Path) -> Option<Hash> {
    if db.has_file_text(file) {
        Some(FileHashes::of(db, file))
    } else {
        Some(Sha256::digest(std::fs::read(db.on_disk(path)).ok()?).into())
    }
}

/// The directory cargo keeps what it fetched in, by a file it fetched: sources of a registry
/// are in its `registry/src`, those of git in its `git/checkouts`.
fn cargo_home_of(path: &Path) -> Option<&Path> {
    let fetched = |dir: &&Path| dir.ends_with("registry/src") || dir.ends_with("git/checkouts");
    path.ancestors().filter(fetched).last()?.parent()?.parent()
}

/// Where a file of a crate is, by what its path is hashed from.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Written {
    InPackage,
    ByBuildScript,
    /// Read by `include_bytes!` or `include_str!` from anywhere else: by its whole path.
    Elsewhere,
}

/// The hash of each file as it was last read, for as long as the daemon runs: a build hashes
/// only what was edited since the one before.
pub(crate) struct FileHashes {
    files: FxHashMap<vfs::FileId, (Arc<str>, Hash)>,
}

static HASHES: OnceLock<Mutex<FileHashes>> = OnceLock::new();

impl FileHashes {
    fn of(db: &RootDatabase, file: vfs::FileId) -> Hash {
        let hashes = HASHES.get_or_init(|| Mutex::new(FileHashes { files: FxHashMap::default() }));
        let text = db.file_text(file).text(db);
        let mut hashes = hashes.lock().unwrap_or_else(|e| e.into_inner());
        // The text it was hashed from is kept, so no other text is ever where it is.
        if let Some((known, hash)) = hashes.files.get(&file)
            && Arc::ptr_eq(known, text)
        {
            return *hash;
        }
        let hash = ide_db::code_identity::file_hash(db, file);
        hashes.files.insert(file, (text.clone(), hash));
        hash
    }

    /// How many files are hashed and kept, for `memory::Own`: none when none was.
    pub(crate) fn held() -> usize {
        HASHES.get().map(|it| it.lock().unwrap_or_else(|e| e.into_inner()).files.len()).unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn environment_fixture(
        body: &str,
        library: bool,
        macro_dependency: bool,
        values: &[(&str, &str)],
    ) -> (RootDatabase, [Crate; 2]) {
        use test_fixture::WithFixture;
        let library = if library { "library" } else { "" };
        let macros = if macro_dependency { "deps:macros" } else { "" };
        let fixture = format!(
            "//- /checkout/app/lib.rs crate:app deps:dep\n\
             pub fn caller() -> &'static str {{ dep::value() }}\n\
             //- /checkout/dep/lib.rs crate:dep {library} {macros} new_source_root:local\n\
             #[rustc_builtin_macro]\nmacro_rules! env {{ () => {{}} }}\n\
             #[rustc_builtin_macro]\nmacro_rules! option_env {{ () => {{}} }}\n\
             {body}\n\
             //- /checkout/macros/lib.rs crate:proc-macro:macros new_source_root:local\n\
             pub fn macro_producer() {{}}\n"
        );
        let (mut db, files) = RootDatabase::with_many_files(&fixture);
        let mut change = ide_db::ChangeWithProcMacros::default();
        change.set_roots(
            ["app", "dep", "macros"]
                .into_iter()
                .zip(&files)
                .map(|(name, file)| {
                    let mut files = vfs::file_set::FileSet::default();
                    files.insert(
                        file.file_id(&db),
                        vfs::VfsPath::from(paths::AbsPathBuf::assert_utf8(
                            format!("/checkout/{name}/lib.rs").into(),
                        )),
                    );
                    ide_db::base_db::SourceRoot::new_local(files)
                })
                .collect(),
        );
        db.apply_change(change);
        let crates = ide_db::base_db::all_crates(&db);
        let named = |name: &str| {
            *crates
                .iter()
                .find(|krate| {
                    krate.extra_data(&db).display_name.as_ref().unwrap().to_string() == name
                })
                .unwrap()
        };
        let [app, dep, macros] = ["app", "dep", "macros"].map(named);
        for (name, krate) in [("app", app), ("dep", dep), ("macros", macros)] {
            let mut env = krate.env(&db).clone();
            env.set("CARGO_MANIFEST_DIR", format!("/checkout/{name}"));
            if krate == dep {
                for &(name, value) in values {
                    env.set(name, value);
                }
            }
            db.set_env(krate, env);
        }
        (db, [app, dep])
    }

    fn environment_ids(
        db: &RootDatabase,
        [app, dep]: [Crate; 2],
        dependency_first: bool,
        discover_first: bool,
    ) -> [Hash; 2] {
        hir::attach_db(db, || {
            if discover_first {
                hir::input_owners::discover_current_inputs(db, dep);
            } else {
                assert!(db.env_read().is_empty(), "identity is the first body consumer");
            }
            let table = crate::symbols::SymbolTable::default();
            let known = hir_ty_codegen::Rejections::default();
            let listed = crate::Listed::default();
            let contents = Contents::default();
            let mut args = crate::Args::parse(
                ["/checkout", "--crate", "app", "--out", "app.o"].into_iter().map(str::to_owned),
            )
            .unwrap();
            args.checkout = Path::new("/checkout").to_owned();
            let resolver = crate::Resolver::of(db, &table, &args, &known, &contents, &listed);
            use hir_ty_codegen::SymbolResolver as _;
            if dependency_first {
                let dep_id = resolver.content_id(dep).unwrap().0;
                [resolver.content_id(app).unwrap().0, dep_id]
            } else {
                [resolver.content_id(app).unwrap().0, resolver.content_id(dep).unwrap().0]
            }
        })
    }

    #[test]
    fn unused_cache_service_environment_does_not_split_plain_source_identity() {
        let identity = |value| {
            let (db, crates) = environment_fixture(
                "pub fn value() -> &'static str { \"same source\" }",
                false,
                false,
                &[
                    ("RA_JIT_CACHE", value),
                    ("RA_JIT_REMOTE", value),
                    ("RA_JIT_PROJECT", value),
                    ("RA_JIT_REMOTE_READONLY", value),
                ],
            );
            environment_ids(&db, crates, false, false)
        };
        assert_eq!(identity("worker-one"), identity("worker-two"));
        let arbitrary = |value| {
            let (db, crates) = environment_fixture(
                "pub fn value() -> &'static str { \"same source\" }",
                false,
                false,
                &[("ARBITRARY_BUILD_INPUT", value)],
            );
            environment_ids(&db, crates, false, false)
        };
        assert_ne!(arbitrary("one"), arbitrary("two"), "other inputs remain conservative");
    }

    #[test]
    fn library_environment_reads_invalidate_both_library_and_dependent_content() {
        let identity = |value, dependency_first, discover_first| {
            let (db, crates) = environment_fixture(
                "pub fn value() -> &'static str { env!(\"RA_JIT_REMOTE\") }",
                true,
                false,
                &[("RA_JIT_REMOTE", value)],
            );
            let ids = environment_ids(&db, crates, dependency_first, discover_first);
            assert!(
                db.env_read()
                    .iter()
                    .any(|&(of, ref key)| { of == crates[1] && key == "RA_JIT_REMOTE" })
            );
            hir::attach_db(&db, || {
                let declared = crate::declared_in(&db, crates[1].into());
                let body = hir_def::expr_store::body::Body::of(&db, declared.functions[0].into());
                assert!(format!("{body:?}").contains(value), "expanded body must retain the value");
            });
            ids
        };
        let one = identity("worker-one", false, false);
        let two = identity("worker-two", false, false);
        assert_ne!(one[1], two[1], "the library body embeds the changed crate variable");
        assert_ne!(one[0], two[0], "a caller may inline the changed library body");
        for dependency_first in [false, true] {
            for discover_first in [false, true] {
                assert_eq!(one, identity("worker-one", dependency_first, discover_first));
                assert_eq!(two, identity("worker-two", dependency_first, discover_first));
            }
        }
    }

    #[test]
    fn lazy_environment_owners_are_discovered_before_the_first_content_identity() {
        for body in [
            "pub fn value() -> &'static str { env!(\"RA_JIT_REMOTE\") }",
            "const UNUSED: &str = env!(\"RA_JIT_REMOTE\"); pub fn value() -> &'static str { \"ok\" }",
            "#[repr(u8)] enum E { X = env!(\"RA_JIT_REMOTE\").as_bytes()[0] } pub fn value() -> &'static str { \"ok\" }",
            "type A = [u8; env!(\"RA_JIT_REMOTE\").len()]; pub fn value() -> &'static str { \"ok\" }",
            "struct A([u8; env!(\"RA_JIT_REMOTE\").len()]); pub fn value() -> &'static str { \"ok\" }",
            "struct A<const N: usize = { env!(\"RA_JIT_REMOTE\").len() }>; pub fn value() -> &'static str { \"ok\" }",
        ] {
            for library in [false, true] {
                let identity = |value, dependency_first, discover_first| {
                    let (db, crates) =
                        environment_fixture(body, library, false, &[("RA_JIT_REMOTE", value)]);
                    let result = environment_ids(&db, crates, dependency_first, discover_first);
                    assert!(
                        db.env_read()
                            .iter()
                            .any(|&(of, ref key)| { of == crates[1] && key == "RA_JIT_REMOTE" }),
                        "cold discovery must visit {body}"
                    );
                    result
                };
                let one = identity("one", false, false);
                let two = identity("other", false, false);
                assert_ne!(one[0], two[0]);
                assert_ne!(one[1], two[1]);
                for dependency_first in [false, true] {
                    for discover_first in [false, true] {
                        assert_eq!(one, identity("one", dependency_first, discover_first));
                        assert_eq!(two, identity("other", dependency_first, discover_first));
                    }
                }
            }
        }
    }

    #[test]
    fn observed_environment_uses_crate_precedence_and_distinguishes_missing_native_values() {
        let identity = |overlaid: Option<EnvironmentValue>, process: &str| {
            let (mut db, crates) = environment_fixture(
                "pub fn value() -> &'static str { option_env!(\"BUILD_INPUT\").unwrap_or(\"absent\") }",
                true,
                false,
                &[],
            );
            let mut env = crates[1].env(&db).clone();
            let unset = matches!(overlaid, Some(EnvironmentValue::Unset));
            match overlaid {
                Some(EnvironmentValue::Text(value)) => env.set("BUILD_INPUT", value),
                Some(EnvironmentValue::NonUnicode) => env.set_non_unicode("BUILD_INPUT"),
                Some(EnvironmentValue::Unset) | None => {}
            }
            db.set_env(crates[1], env);
            if !unset {
                ide_db::base_db::set_process_env(
                    &mut db,
                    [("BUILD_INPUT".to_owned(), process.to_owned())].into_iter().collect(),
                );
            }
            environment_ids(&db, crates, false, false)
        };
        let explicit = || Some(EnvironmentValue::Text("crate-value".to_owned()));
        assert_eq!(identity(explicit(), "caller-one"), identity(explicit(), "caller-two"));
        assert_ne!(identity(None, "caller-one"), identity(None, "caller-two"));
        let missing = identity(Some(EnvironmentValue::Unset), "ignored");
        let empty = identity(Some(EnvironmentValue::Text(String::new())), "caller");
        let native = identity(Some(EnvironmentValue::NonUnicode), "caller");
        assert_ne!(missing, empty);
        assert_ne!(missing, native);
        assert_ne!(empty, native);
        assert_ne!(native, identity(None, "caller"), "a native crate value must not fall through");
    }

    #[test]
    fn codegen_attribute_environment_is_discovered_before_the_first_content_identity() {
        for body in [
            "#[unsafe(export_name = env!(\"RA_JIT_REMOTE\"))] pub fn value() -> &'static str { \"ok\" }",
            "#[unsafe(link_section = env!(\"RA_JIT_REMOTE\"))] pub fn value() -> &'static str { \"ok\" }",
            "unsafe extern \"C\" { #[link_name = env!(\"RA_JIT_REMOTE\")] fn foreign(); } pub fn value() -> &'static str { \"ok\" }",
            "#[linkage = env!(\"RA_JIT_REMOTE\")] static VALUE: u8 = 1; pub fn value() -> &'static str { \"ok\" }",
        ] {
            for library in [false, true] {
                let identity = |value, dependency_first, discover_first| {
                    let (db, crates) =
                        environment_fixture(body, library, false, &[("RA_JIT_REMOTE", value)]);
                    let ids = environment_ids(&db, crates, dependency_first, discover_first);
                    assert!(
                        db.env_read()
                            .iter()
                            .any(|&(of, ref key)| { of == crates[1] && key == "RA_JIT_REMOTE" }),
                        "codegen attribute discovery must visit {body}"
                    );
                    ids
                };
                let one = identity("weak", false, false);
                let two = identity("external", false, false);
                assert_ne!(one[0], two[0]);
                assert_ne!(one[1], two[1]);
                assert_eq!(one, identity("weak", true, true));
                assert_eq!(two, identity("external", true, true));
            }
        }
    }

    #[test]
    fn observed_paths_are_semantic_values_even_when_their_prefix_is_relocatable() {
        let identity = |manifest| {
            let (db, crates) = environment_fixture(
                "pub fn value() -> &'static str { env!(\"CARGO_MANIFEST_DIR\") }",
                true,
                false,
                &[("CARGO_MANIFEST_DIR", manifest)],
            );
            environment_ids(&db, crates, false, false)
        };
        assert_ne!(identity("/first/dep"), identity("/another/depth/dep"));
    }

    #[test]
    fn proc_macro_consumers_keep_their_full_environment_through_reexports() {
        let identity = |value: &str, library: bool, at_caller: bool| {
            let (mut db, crates) = environment_fixture(
                "pub fn value() -> &'static str { \"same source\" }",
                library,
                true,
                &[],
            );
            let krate = crates[usize::from(!at_caller)];
            let mut env = krate.env(&db).clone();
            env.set("RA_JIT_REMOTE", value);
            db.set_env(krate, env);
            environment_ids(&db, crates, false, false)
        };
        for library in [false, true] {
            for at_caller in [false, true] {
                let one = identity("one", library, at_caller);
                let two = identity("other", library, at_caller);
                assert_ne!(one[0], two[0], "indirect macro consumers cannot discard env inputs");
                if !at_caller {
                    assert_ne!(one[1], two[1]);
                }
            }
        }
    }

    #[test]
    fn binary_file_identity_reads_the_active_filesystem_view() {
        let work = tempfile::tempdir().unwrap();
        let base = work.path().join("base");
        let active = work.path().join("active");
        std::fs::create_dir(&base).unwrap();
        std::fs::create_dir(&active).unwrap();
        let name = "payload.bin";
        let stale = [0xfe, 0, 3];
        let current = [0xff, 0, 7];
        std::fs::write(base.join(name), stale).unwrap();
        std::fs::write(active.join(name), current).unwrap();
        let db = RootDatabase::default();
        let file = vfs::FileId::from_raw(1);
        assert!(!db.has_file_text(file));
        db.set_view(&base, &active, Vec::new());
        let path = base.join(name);
        assert_eq!(std::fs::read(db.on_disk(&path)).unwrap(), current);
        assert_eq!(file_hash(&db, file, &path), Some(<Hash>::from(Sha256::digest(current))));
        assert_ne!(file_hash(&db, file, &path), Some(<Hash>::from(Sha256::digest(stale))));
        std::fs::remove_file(active.join(name)).unwrap();
        assert_eq!(
            file_hash(&db, file, &path),
            None,
            "missing active bytes are not empty or stale bytes"
        );
    }

    #[test]
    fn an_out_of_vfs_payload_invalidates_its_library_and_dependent_caller() {
        check_payload_owner("pub fn value() -> u8 { include_bytes!(\"payload.bin\")[0] }", false);
    }

    #[test]
    fn included_bytes_advance_mir_in_one_database() {
        check_payload_owner(
            "pub fn value() -> &'static [u8; 1] { include_bytes!(\"payload.bin\") }",
            false,
        );
    }

    #[test]
    fn lazy_const_and_type_owners_discover_payloads_before_any_identity_lookup() {
        for body in [
            "#[repr(u8)] enum E { X = include_bytes!(\"payload.bin\")[0] } pub fn value() -> u8 { E::X as u8 }",
            "#[repr(u8)] enum E { X = include_str!(\"payload.bin\").as_bytes()[0] } pub fn value() -> u8 { E::X as u8 }",
            "type A = [u8; include_bytes!(\"payload.bin\")[0] as usize]; pub fn value() -> u8 { 1 }",
            "struct A([u8; include_bytes!(\"payload.bin\")[0] as usize]); pub fn value() -> u8 { 1 }",
            "struct A<const N: usize = { include_bytes!(\"payload.bin\")[0] as usize }>; pub fn value() -> u8 { 1 }",
        ] {
            for caller_first in [false, true] {
                check_payload_owner(body, caller_first);
            }
        }
    }

    fn check_payload_owner(body: &str, caller_first: bool) {
        use test_fixture::WithFixture;
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let top = std::env::temp_dir().join(format!(
            "c4-payload-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::create_dir_all(top.join("vendor/dep/src")).unwrap();
        let payload = top.join("vendor/dep/src/payload.bin");
        let fixture = format!(
            "//- {}/app.rs crate:app deps:dep\nfn caller() -> u8 {{ dep::value() + 10 }}\n\
             //- {}/vendor/dep/src/lib.rs crate:dep library new_source_root:library\n\
             #[rustc_builtin_macro]\nmacro_rules! include_bytes {{ () => {{}} }}\n\
             #[rustc_builtin_macro]\nmacro_rules! include_str {{ () => {{}} }}\n\
             {body}\n",
            top.display(),
            top.display(),
        );
        let hash = |byte: u8| {
            std::fs::write(&payload, [byte]).unwrap();
            let (mut db, files) = RootDatabase::with_many_files(&fixture);
            // WithFixture uses virtual paths. Replace the file sets with real paths so
            // included_file resolves the on-disk payload through its production reader.
            let mut change = ide_db::ChangeWithProcMacros::default();
            let mut app_files = vfs::file_set::FileSet::default();
            let mut dep_files = vfs::file_set::FileSet::default();
            app_files.insert(
                files[0].file_id(&db),
                vfs::VfsPath::from(paths::AbsPathBuf::assert_utf8(top.join("app.rs"))),
            );
            dep_files.insert(
                files[1].file_id(&db),
                vfs::VfsPath::from(paths::AbsPathBuf::assert_utf8(
                    top.join("vendor/dep/src/lib.rs"),
                )),
            );
            change.set_roots(vec![
                ide_db::base_db::SourceRoot::new_local(app_files),
                ide_db::base_db::SourceRoot::new_library(dep_files),
            ]);
            db.apply_change(change);
            let first = hir::attach_db(&db, || {
                let crates = ide_db::base_db::all_crates(&db);
                let find = |name: &str| {
                    *crates
                        .iter()
                        .find(|krate| {
                            krate.extra_data(&db).display_name.as_ref().unwrap().to_string() == name
                        })
                        .unwrap()
                };
                let (dep, app) = (find("dep"), find("app"));
                assert!(db.included_by(dep).is_empty());
                let contents = Contents::default();
                let caller_hash = caller_first.then(|| contents.source_hash(&db, app));
                let dep_hash = contents.source_hash(&db, dep);
                assert_eq!(db.included_by(dep)[0].1.as_deref(), Some(&[byte][..]));
                let app_hash = caller_hash.unwrap_or_else(|| contents.source_hash(&db, app));
                assert_eq!(dep_hash, ide_db::code_identity::source_hash(&db, dep));
                assert_eq!(app_hash, ide_db::code_identity::source_hash(&db, app));
                (dep_hash, app_hash)
            });
            let path = paths::AbsPathBuf::assert_utf8(payload.clone());
            let bodies = |db: &RootDatabase| {
                let dep = *ide_db::base_db::all_crates(db)
                    .iter()
                    .find(|krate| {
                        krate.extra_data(db).display_name.as_ref().unwrap().to_string() == "dep"
                    })
                    .unwrap();
                let declared = crate::declared_in(db, dep.into());
                declared
                    .functions
                    .iter()
                    .map(|&id| {
                        use hir_ty::db::HirDatabase as _;
                        (
                            format!("{:?}", hir_def::expr_store::body::Body::of(db, id.into())),
                            format!("{:?}", db.runtime_mir_body(id.into())),
                        )
                    })
                    .collect::<Vec<_>>()
            };
            let linkage_and_content = |db: &RootDatabase| {
                let dep = *ide_db::base_db::all_crates(db)
                    .iter()
                    .find(|krate| {
                        krate.extra_data(db).display_name.as_ref().unwrap().to_string() == "dep"
                    })
                    .unwrap();
                let table = crate::symbols::SymbolTable::default();
                let known = hir_ty_codegen::Rejections::default();
                let listed = crate::Listed::default();
                let contents = Contents::default();
                let mut args = crate::Args::parse(
                    [top.to_str().unwrap(), "--crate", "app", "--out", "app.o"]
                        .into_iter()
                        .map(str::to_owned),
                )
                .unwrap();
                args.checkout = top.clone();
                let resolver = crate::Resolver::of(db, &table, &args, &known, &contents, &listed);
                use hir_ty_codegen::SymbolResolver as _;
                (resolver.own_crate_id(dep), resolver.content_id(dep).map(|id| id.0))
            };
            let old_identity = hir::attach_db(&db, || linkage_and_content(&db));
            let old_bodies = hir::attach_db(&db, || bodies(&db));
            std::fs::write(&payload, [byte + 10]).unwrap();
            assert!(db.reload_included_file(&path));
            let new_bodies = hir::attach_db(&db, || bodies(&db));
            let new_identity = hir::attach_db(&db, || linkage_and_content(&db));
            assert_eq!(old_identity.0, new_identity.0, "native linkage must survive an input edit");
            assert_ne!(
                old_identity.1, new_identity.1,
                "stored code must retain current content identity"
            );
            if body.contains("pub fn value() -> u8 { include_bytes!") {
                assert_ne!(old_bodies, new_bodies, "lowered body must depend on included bytes");
            }
            if body.contains("-> &'static [u8; 1]") {
                assert!(!old_bodies[0].1.starts_with("Err"), "{}", old_bodies[0].1);
                assert_ne!(old_bodies[0].1, new_bodies[0].1, "MIR must depend on included bytes");
            }
            let second = hir::attach_db(&db, || {
                let crates = ide_db::base_db::all_crates(&db);
                let app = *crates
                    .iter()
                    .find(|krate| {
                        krate.extra_data(&db).display_name.as_ref().unwrap().to_string() == "app"
                    })
                    .unwrap();
                Contents::default().source_hash(&db, app)
            });
            assert_ne!(first.1, second, "same database must advance included-input identity");
            let dep = *ide_db::base_db::all_crates(&db)
                .iter()
                .find(|krate| {
                    krate.extra_data(&db).display_name.as_ref().unwrap().to_string() == "dep"
                })
                .unwrap();
            let current = hir::attach_db(&db, || crate::current_input_paths(&db, dep));
            assert!(current.contains(&path));
            let replacement = "pub fn value() -> u8 { 42 }\n";
            db.set_file_text_with_durability(
                files[1].file_id(&db),
                replacement,
                ide_db::base_db::salsa::Durability::HIGH,
            );
            std::fs::remove_file(&payload).unwrap();
            assert!(db.reload_included_file(&path));
            let current = hir::attach_db(&db, || crate::current_input_paths(&db, dep));
            assert!(current.is_empty(), "obsolete input must be retracted");
            assert!(db.included_by(dep).iter().any(|(old, bytes)| old == &path && bytes.is_none()));
            assert!(hir::attach_db(&db, || crate::missing_input_of(&db, dep)).is_none());
            for source in [
                "#[rustc_builtin_macro]\nmacro_rules! include_bytes { () => {} }\npub fn value() -> &'static [u8; 1] { include_bytes!(\"payload.bin\") }",
                "#[rustc_builtin_macro]\nmacro_rules! include_bytes { () => {} }\n#[cfg(any())] pub fn value() -> &'static [u8; 1] { include_bytes!(\"payload.bin\") }",
            ] {
                db.set_file_text_with_durability(
                    files[1].file_id(&db),
                    source,
                    ide_db::base_db::salsa::Durability::HIGH,
                );
                hir::attach_db(&db, || {
                    crate::current_input_paths(&db, dep);
                    let missing = crate::missing_input_of(&db, dep);
                    if source.contains("cfg(any())") {
                        assert!(missing.is_none());
                    } else {
                        assert_eq!(missing, Some(path.clone()));
                    }
                });
            }
            let new_source = "#[rustc_builtin_macro]\nmacro_rules! include_bytes { () => {} }\npub fn value() -> &'static [u8; 1] { include_bytes!(\"new-missing.bin\") }";
            db.set_file_text_with_durability(
                files[1].file_id(&db),
                new_source,
                ide_db::base_db::salsa::Durability::HIGH,
            );
            assert!(
                !db.included_by(dep)
                    .iter()
                    .any(|(path, _)| path.as_str().ends_with("new-missing.bin"))
            );
            hir::attach_db(&db, || {
                // Normal source/body discovery registers a new missing input before
                // the fast validation probe. An empty historical map is not an
                // exemption from normal compiler include expansion.
                crate::prepare_changed_input_owners(&db, &[files[1].file_id(&db)]);
                assert_eq!(
                    crate::missing_input_of(&db, dep),
                    Some(paths::AbsPathBuf::assert_utf8(
                        top.join("vendor/dep/src/new-missing.bin")
                    ))
                );
            });
            first
        };
        let one = hash(1);
        let other = hash(2);
        assert_ne!(one.0, other.0);
        assert_ne!(one.1, other.1);
        std::fs::remove_dir_all(top).unwrap();
    }

    #[test]
    fn equal_vendor_places_share_only_equal_source_bytes() {
        let files = |home: &str, body: &[u8]| {
            vec![
                (format!("{home}/vendor/dep/src/lib.rs"), Sha256::digest(body).into()),
                (format!("{home}/vendor/dep/src/other.rs"), Sha256::digest(b"same").into()),
            ]
        };
        let one = hash_source_files(&files("/home/a/repo", b"fn value() -> u32 { 1 }"));
        let other = hash_source_files(&files("/Users/b/repo", b"fn value() -> u32 { 1 }"));
        let changed = hash_source_files(&files("/Users/b/repo", b"fn value() -> u32 { 2 }"));
        assert_eq!(one, other);
        assert_ne!(one, changed);
    }

    #[test]
    fn the_cargo_home_is_told_from_a_file_cargo_fetched() {
        fn home(path: &str) -> Option<&str> {
            cargo_home_of(Path::new(path))?.to_str()
        }
        let registry = "/home/one/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f";
        assert_eq!(home(&format!("{registry}/dep-0.1.0/src/lib.rs")), Some("/home/one/.cargo"));
        assert_eq!(home("/srv/c/git/checkouts/dep-1a2b/3c4d/src/lib.rs"), Some("/srv/c"));
        // A crate that vendors another has the outermost such directory as cargo's.
        let nested = format!("{registry}/dep-0.1.0/registry/src/inner/lib.rs");
        assert_eq!(home(&nested), Some("/home/one/.cargo"));
        assert_eq!(home("/home/one/checkout/src/lib.rs"), None);
    }
}
