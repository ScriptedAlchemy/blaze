//! Frozen dependency artifacts (`docs/glancer.md`, item 1; #40): the def map, the impls and
//! the lang items of each crate whose source cannot change, written to the store once and read
//! back instead of resolving and finding them again.
//!
//! A crate is frozen when its source is the toolchain's (the standard library) or a
//! registry's or git checkout's, and every crate it depends on is frozen too. Its key is a
//! Merkle hash: where the source is below the toolchain, registry or checkout, its edition,
//! kind, crate attributes, cfg, environment, and the names and keys of its dependencies; the store adds the toolchain
//! and ra-jit's own build. `OUT_DIR` counts by its place below the target directory and by what
//! the build script wrote there, so the same crate in another checkout has the same key.
//!
//! `RA_JIT_FROZEN` says what is done with what the store has: by default it is served;
//! `check` resolves and finds as well and fails the build on any difference, `verify` writes
//! what was read and fails on any difference, `off` neither reads nor writes.

use std::sync::Arc;
use std::time::{Duration, Instant};

use ide_db::base_db::Crate;
use rustc_hash::{FxHashMap, FxHashSet};
use sha2::{Digest, Sha256};

use hir_def::nameres::frozen::{self as hir_frozen, Bytes, Frozen, Mode};
use hir_ty::method_resolution::frozen as impls_frozen;

use crate::front_image;

/// What a build did with what is frozen of the crates it resolved.
#[derive(Default)]
pub(crate) struct Served {
    /// The key of each crate that can be frozen.
    keys: FxHashMap<Crate, [u8; 32]>,
    /// The crates each [`Part`] of which came from the store, in the order of [`Part::ALL`].
    served: [Vec<Crate>; 3],
    /// The parts the store has but which name a file that is not loaded.
    unreadable: usize,
    mode: Option<Mode>,
    /// The image `RA_JIT_FRONT_IMAGE` names, when it was mapped.
    image: Option<Arc<front_image::Image>>,
    /// How many of each part came out of the image, and the bytes of each part served or
    /// written, in the order of [`Part::ALL`].
    from_image: [usize; 3],
    bytes: [u64; 3],
    /// What reading the parts took, and what all of `serve` took.
    fetch_took: Duration,
    serve_took: Duration,
}

fn mode() -> Option<Mode> {
    match std::env::var("RA_JIT_FROZEN").as_deref() {
        Ok("off" | "0") => None,
        Ok("check") => Some(Mode::Check),
        Ok("verify") => Some(Mode::Verify),
        _ => Some(Mode::Serve),
    }
}

/// Where a crate's source is when it is one that cannot change: the part of the path from
/// the toolchain's source, the registry's or a git checkout's on.
fn immutable(path: &str) -> Option<&str> {
    ["/lib/rustlib/src/rust/", "/registry/src/", "/git/checkouts/"]
        .iter()
        .find_map(|marker| path.find(marker).map(|at| &path[at + marker.len()..]))
}

/// What stands for the directory a build script wrote to in a frozen crate's key: the part of
/// it from cargo's `build` directory on, whose `<package>-<hash>` cargo names by the package,
/// its features, profile and target, and not by where the checkout is.
fn build_output(out: &str) -> String {
    let path = out.replace('\\', "/");
    match path.rfind("/build/") {
        Some(at) => format!("<target>{}", &path[at..]),
        None => "<out>".to_owned(),
    }
}

/// The key of `krate`, when it and every crate it depends on cannot change.
fn key(
    db: &ide_db::RootDatabase,
    krate: Crate,
    keys: &mut FxHashMap<Crate, Option<[u8; 32]>>,
) -> Option<[u8; 32]> {
    if let Some(&it) = keys.get(&krate) {
        return it;
    }
    // A cycle, which only dev-dependencies make, is not frozen.
    keys.insert(krate, None);
    let found = key_of(db, krate, keys);
    keys.insert(krate, found);
    found
}

fn key_of(
    db: &ide_db::RootDatabase,
    krate: Crate,
    keys: &mut FxHashMap<Crate, Option<[u8; 32]>>,
) -> Option<[u8; 32]> {
    let root = crate::crate_root_file(db, krate.into())?;
    let root = root.to_str()?;
    let source = immutable(root)?;
    // The directory the crate's paths are under, which its environment names.
    let package = &root[..root.len() - source.len()];
    let data = krate.data(db);
    let mut hasher = Sha256::new();
    let mut field = |bytes: &[u8]| {
        hasher.update((bytes.len() as u64).to_le_bytes());
        hasher.update(bytes);
    };
    field(b"ra-jit frozen def map 5");
    field(source.as_bytes());
    field(format!("{:?} {:?} {:?}", data.edition, data.target_kind, data.crate_attrs).as_bytes());
    let mut cfgs: Vec<String> =
        krate.cfg_options(db).into_iter().map(|it| it.to_string()).collect();
    cfgs.sort_unstable();
    field(cfgs.join("\n").as_bytes());
    // A build script writes below the target directory of the checkout that ran it, so what it
    // wrote stands for where: two checkouts share the crate when its script wrote the same.
    let env = krate.env(db);
    let out = env.get("OUT_DIR");
    let out_stands_for = out.as_deref().map(build_output);
    // `CARGO` is wherever the cargo found on `PATH` is, which differs between a build script's
    // compile and the daemon's, and no crate's names are resolved by it.
    let mut vars: Vec<String> = env
        .iter()
        .filter(|(name, _)| *name != "CARGO")
        .map(|(name, value)| {
            let value = match (&out, &out_stands_for) {
                (Some(out), Some(stands_for)) => value.replace(out.as_str(), stands_for),
                _ => value.to_owned(),
            };
            format!("{name}={}", value.replace(package, "<source>/"))
        })
        .collect();
    vars.sort_unstable();
    field(vars.join("\n").as_bytes());
    if let Some(out) = &out {
        for (path, hash) in crate::content::written_to(db, krate, out) {
            field(path.as_bytes());
            field(&hash);
        }
    }
    let deps: Vec<_> =
        data.dependencies.iter().map(|it| (it.crate_id, it.name.to_string())).collect();
    for (dep, name) in deps {
        let dep_key = key(db, dep, keys)?;
        field(name.as_bytes());
        field(&dep_key);
    }
    Some(hasher.finalize().into())
}

/// What is frozen of a crate, each under its own key: the def map's is the crate's key.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Part {
    DefMap,
    Impls,
    LangItems,
}

impl Part {
    const ALL: [Part; 3] = [Part::DefMap, Part::Impls, Part::LangItems];

    fn name(self) -> &'static str {
        match self {
            Part::DefMap => "def maps",
            Part::Impls => "impls",
            Part::LangItems => "lang items",
        }
    }

    /// The key of this part of the crate whose key is `key`.
    fn key(self, key: &[u8; 32]) -> [u8; 32] {
        let tag: &[u8] = match self {
            Part::DefMap => return *key,
            Part::Impls => b"ra-jit frozen impls 1",
            Part::LangItems => b"ra-jit frozen lang items 1",
        };
        let mut hasher = Sha256::new();
        hasher.update(tag);
        hasher.update(key);
        hasher.finalize().into()
    }

    /// Why `bytes` cannot be read in this database: a file they name is not loaded.
    fn unreadable(self, db: &ide_db::RootDatabase, krate: Crate, bytes: &[u8]) -> Option<String> {
        if self != Part::Impls {
            return hir_frozen::unreadable(db, krate, bytes);
        }
        // The impls are two parts, each naming its files.
        let len = bytes.get(..4).and_then(|it| <[u8; 4]>::try_from(it).ok());
        let Some(len) = len else { return Some("no length".to_owned()) };
        let first = 4 + u32::from_le_bytes(len) as usize;
        let (Some(traits), Some(inherent)) = (bytes.get(4..first), bytes.get(first..)) else {
            return Some("a part past the end".to_owned());
        };
        [traits, inherent].into_iter().find_map(|part| hir_frozen::unreadable(db, krate, part))
    }

    fn set(self, db: &mut ide_db::RootDatabase, maps: FxHashMap<Crate, Frozen>) {
        match self {
            Part::DefMap => hir_frozen::set(db, maps),
            Part::Impls => impls_frozen::set(db, maps),
            Part::LangItems => hir_frozen::set_lang_items(db, maps),
        }
    }

    fn encode(self, db: &ide_db::RootDatabase, krate: Crate) -> Result<Vec<u8>, String> {
        hir::attach_db(db, || match self {
            Part::DefMap => hir_frozen::encode(db, krate),
            Part::Impls => impls_frozen::encode(db, krate),
            Part::LangItems => hir_frozen::encode_lang_items(db, krate),
        })
    }

    /// Records a difference between what was read and what is live; the def map's is
    /// checked as it is resolved.
    fn check(self, db: &ide_db::RootDatabase, krate: Crate) {
        hir::attach_db(db, || match self {
            Part::DefMap => {}
            Part::Impls => impls_frozen::check(db, krate),
            Part::LangItems => hir_frozen::check_lang_items(db, krate),
        })
    }
}

/// Has name resolution, finding impls and finding lang items read what the store has of
/// `crates` and their dependencies, or what the image `RA_JIT_FRONT_IMAGE` names has.
pub(crate) fn serve(db: &mut ide_db::RootDatabase, crates: &[Crate]) -> Served {
    let mode = mode();
    if mode.is_none() && !census_wanted() {
        return Served::default();
    }
    let started = Instant::now();
    let mut all: FxHashMap<Crate, Option<[u8; 32]>> = FxHashMap::default();
    for &krate in crates {
        for dep in krate.transitive_deps(db) {
            key(db, dep, &mut all);
        }
    }
    let keys: FxHashMap<Crate, [u8; 32]> =
        all.into_iter().filter_map(|(krate, key)| Some((krate, key?))).collect();
    // With nothing frozen, the keys are kept for the census to tell the dependencies apart.
    let Some(mode) = mode else { return Served { keys, ..Served::default() } };
    let mut served = Served { keys, mode: Some(mode), ..Served::default() };
    let store = ra_jit_store::Store::global();
    let image = front_image::open().map(Arc::new);
    if store.is_none() && image.is_none() {
        return served;
    }
    for (index, part) in Part::ALL.into_iter().enumerate() {
        let mut maps = FxHashMap::default();
        for (&krate, key) in &served.keys {
            // A part is read only with the def map it was found from.
            if part != Part::DefMap && !served.served[0].contains(&krate) {
                continue;
            }
            let fetched = Instant::now();
            let from_image = image.as_ref().and_then(|it| it.get(index as u8, &part.key(key)));
            let bytes = match from_image {
                Some(bytes) => {
                    served.from_image[index] += 1;
                    bytes
                }
                None => match store.and_then(|it| it.get(&part.key(key))) {
                    Some(bytes) => Bytes::from(bytes),
                    None => continue,
                },
            };
            served.fetch_took += fetched.elapsed();
            if part.unreadable(db, krate, &bytes).is_some() {
                served.unreadable += 1;
                continue;
            }
            served.bytes[index] += bytes.len() as u64;
            maps.insert(krate, Frozen { bytes, mode });
            served.served[index].push(krate);
        }
        part.set(db, maps);
    }
    if let Some(image) = &image {
        front_image::watch(image);
    }
    served.image = image;
    served.serve_took = started.elapsed();
    served
}

/// Writes to the store what can be frozen of the crates that were resolved, and fails on a
/// difference between what was frozen and what is live.
pub(crate) fn store(db: &ide_db::RootDatabase, served: &Served) -> anyhow::Result<()> {
    if served.mode.is_none() {
        if census_wanted() {
            census(db, served);
        }
        return Ok(());
    }
    if served.mode != Some(Mode::Serve) {
        for (index, part) in Part::ALL.into_iter().enumerate() {
            served.served[index].iter().for_each(|&krate| part.check(db, krate));
        }
    }
    let differences = hir_frozen::differences();
    if !differences.is_empty() {
        anyhow::bail!("what was frozen differs:\n{}", differences.join("\n"));
    }
    let Some(store) = served.mode.and(ra_jit_store::Store::global()) else { return Ok(()) };
    let mut stored = 0;
    let mut encoded = [0u64; 3];
    let mut refused: FxHashMap<String, usize> = FxHashMap::default();
    for (index, part) in Part::ALL.into_iter().enumerate() {
        for (&krate, key) in &served.keys {
            let key = part.key(key);
            if served.served[index].contains(&krate) || store.has(&key) {
                continue;
            }
            match part.encode(db, krate) {
                Ok(bytes) => {
                    if stored == 0 {
                        store.settled();
                    }
                    store.insert(&key, &bytes);
                    encoded[index] += bytes.len() as u64;
                    stored += 1;
                }
                Err(why) => *refused.entry(format!("{} with {why}", part.name())).or_default() += 1,
            }
        }
    }
    if stored > 0 {
        store.flush();
    }
    let from: Vec<String> = Part::ALL
        .iter()
        .zip(&served.served)
        .map(|(part, crates)| format!("{} of {}", part.name(), crates.len()))
        .collect();
    let mut said = format!(
        "frozen, of {} crates: {} from the store, {stored} written",
        served.keys.len(),
        from.join(", ")
    );
    if served.unreadable > 0 {
        said += &format!(", {} naming a file not loaded", served.unreadable);
    }
    if !refused.is_empty() {
        let mut refused: Vec<_> = refused.into_iter().collect();
        refused.sort_unstable_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        let why: Vec<String> = refused.iter().map(|(why, n)| format!("{n} {why}")).collect();
        said += &format!(", not frozen: {}", why.join(", "));
    }
    eprintln!("{said}");
    front_image_report(db, served, store, encoded);
    Ok(())
}

fn megabytes(bytes: impl Into<u64>) -> String {
    format!("{:.1}", bytes.into() as f64 / (1 << 20) as f64)
}

/// Writes the image `RA_JIT_FRONT_IMAGE` names when it was not mapped, and says what the
/// frozen parts and the image hold (`RA_JIT_FRONT_STATS`), in the worker's log. `encoded` is
/// the bytes of each part written to the store just now.
fn front_image_report(
    db: &ide_db::RootDatabase,
    served: &Served,
    store: &ra_jit_store::Store,
    encoded: [u64; 3],
) {
    let path = front_image::path();
    if let Some(path) = path.as_ref().filter(|_| served.image.is_none()) {
        let started = Instant::now();
        let mut parts = Vec::new();
        for (index, part) in Part::ALL.into_iter().enumerate() {
            for key in served.keys.values() {
                let key = part.key(key);
                if let Some(bytes) = store.get(&key) {
                    parts.push((index as u8, key, bytes));
                }
            }
        }
        let count = parts.len();
        let bytes: usize = parts.iter().map(|it| it.2.len()).sum();
        match front_image::write(path, parts) {
            Ok(size) => eprintln!(
                "front-end image: wrote {count} parts, {} MB of them in a {} MB file, to {} in {:.1?}",
                megabytes(bytes as u64),
                megabytes(size as u64),
                path.display(),
                started.elapsed()
            ),
            Err(error) => eprintln!("front-end image: {} not written: {error}", path.display()),
        }
    }
    if !front_image::stats() && path.is_none() {
        return;
    }
    let held: Vec<String> = Part::ALL
        .iter()
        .enumerate()
        .map(|(index, part)| {
            format!("{} {} MB", part.name(), megabytes(served.bytes[index] + encoded[index]))
        })
        .collect();
    eprintln!("front-end parts: {}", held.join(", "));
    eprintln!(
        "front-end serve: {:.0?} in all, {:.0?} of it reading parts, {} of {} parts from the image",
        served.serve_took,
        served.fetch_took,
        served.from_image.iter().sum::<usize>(),
        served.served.iter().map(Vec::len).sum::<usize>(),
    );
    if let Some(image) = &served.image {
        let (parts, bytes) = image.totals();
        let (present, pages) = image.in_memory().unwrap_or((0, 0));
        eprintln!(
            "front-end image: {} mapped, {parts} parts, {} MB of them in a {} MB file, {present} of {pages} pages in memory, process holds {} MB",
            image.path().display(),
            megabytes(bytes),
            megabytes(image.size() as u64),
            crate::memory::resident().map(|it| it.to_string()).unwrap_or_default(),
        );
    }
    if census_wanted() {
        census(db, served);
    }
}

fn census_wanted() -> bool {
    std::env::var("RA_JIT_FRONT_STATS").as_deref() == Ok("census")
}

/// What the front end holds of each kind of crate, for docs/research/front-end-image.md:
/// `RA_JIT_FRONT_STATS=census`. A crate is frozen when `serve` could freeze it, a dependency
/// when its source is the toolchain's, a registry's or a git checkout's, and the workspace's
/// when it is neither.
fn census(db: &ide_db::RootDatabase, served: &Served) {
    use ide_db::base_db::SourceDatabase as _;
    #[derive(Default)]
    struct Count {
        crates: usize,
        modules: usize,
        files: usize,
        text: u64,
    }
    const CLASSES: [&str; 3] = ["frozen", "dependency, not frozen", "workspace"];
    let mut counts: [Count; 3] = Default::default();
    hir::attach_db(db, || {
        for krate in ide_db::base_db::all_crates(db).iter().copied() {
            let class = if served.keys.contains_key(&krate) {
                0
            } else if crate::crate_root_file(db, krate.into())
                .as_deref()
                .and_then(|it| it.to_str())
                .is_some_and(|it| immutable(it).is_some())
            {
                1
            } else {
                2
            };
            let def_map = hir_def::nameres::crate_def_map(db, krate);
            let mut files = FxHashSet::default();
            let mut modules = 0;
            for (_, data) in def_map.modules(db) {
                modules += 1;
                if let Some(file) = data.origin.file_id() {
                    files.insert(file.file_id(db));
                }
            }
            let count = &mut counts[class];
            count.crates += 1;
            count.modules += modules;
            count.files += files.len();
            count.text +=
                files.iter().map(|&it| db.file_text(it).text(db).len() as u64).sum::<u64>();
        }
    });
    for (name, count) in CLASSES.iter().zip(&counts) {
        eprintln!(
            "front-end census: {name}: {} crates, {} modules, {} files, {} MB of source text",
            count.crates,
            count.modules,
            count.files,
            megabytes(count.text),
        );
    }
}
