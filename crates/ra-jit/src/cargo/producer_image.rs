//! Immutable owned producer images. Publication follows stable before/after source
//! snapshots; loaded macro clients lease the generation through proc-macro-api.
//!
//! `<dir>/images/<recipe>/<image>/build-N`: a recipe is what was compiled and how (the
//! description, the environment, the standard library's source and the compiler), an image
//! one compilation of it, reused while the files it read are unchanged. Both are producer
//! generations: a recipe or image no one leases is collected when another is made.

use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::OsString,
    fs::File,
    path::{Path, PathBuf},
};

use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use super::own_script::{self, Planned};

type Files = BTreeMap<PathBuf, [u8; 32]>;

#[derive(Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Snapshot {
    #[serde(with = "native_paths")]
    files: Files,
    /// Keep the route by which source was read, even when both possible targets
    /// of a symlink already occur elsewhere in the input closure.
    #[serde(with = "native_bindings")]
    bindings: BTreeMap<PathBuf, PathBuf>,
    // Revisions detect writes/restores during compilation, but are not content
    // identity: restoring the source later may reuse the original immutable image.
    #[serde(skip)]
    revisions: BTreeMap<PathBuf, (Revision, Option<Revision>)>,
}

#[derive(Debug, PartialEq, Eq)]
struct Revision {
    len: u64,
    modified: Option<std::time::SystemTime>,
    #[cfg(unix)]
    native: (u64, u64, i64, i64),
}

impl Revision {
    fn of(metadata: &std::fs::Metadata) -> Self {
        Self {
            len: metadata.len(),
            modified: metadata.modified().ok(),
            #[cfg(unix)]
            native: {
                use std::os::unix::fs::MetadataExt as _;
                (metadata.dev(), metadata.ino(), metadata.ctime(), metadata.ctime_nsec())
            },
        }
    }
}

impl Snapshot {
    pub(crate) fn paths(&self) -> impl Iterator<Item = &PathBuf> {
        self.files.keys().chain(
            self.bindings
                .iter()
                .filter_map(|(route, target)| self.files.contains_key(target).then_some(route)),
        )
    }

    #[cfg(test)]
    fn same_content(&self, other: &Self) -> bool {
        self.files == other.files && self.bindings == other.bindings
    }
}

pub(crate) fn runtime_paths(sysroot: &Path) -> Result<BTreeSet<PathBuf>> {
    let mut inputs = Snapshot::default();
    walk(&sysroot.join("lib/rustlib/src/rust/library"), &mut inputs, &mut BTreeSet::new())?;
    walk(&std::env::current_exe()?, &mut inputs, &mut BTreeSet::new())?;
    Ok(inputs.paths().cloned().collect())
}

#[derive(Serialize, Deserialize)]
struct Record {
    /// The files the compilation read, by the path it read each through.
    #[serde(with = "native_paths")]
    inputs: BTreeMap<PathBuf, Read>,
    /// The entries of each directory it read a file in, which decide what a module is.
    #[serde(with = "native_paths")]
    directories: Files,
    #[serde(with = "native_paths")]
    outputs: Files,
    #[serde(with = "native_path")]
    library: PathBuf,
}

/// A file a compilation read: the file its path was then, and its content.
#[derive(Debug, PartialEq, Serialize, Deserialize)]
struct Read {
    #[serde(with = "native_path")]
    file: PathBuf,
    digest: [u8; 32],
}

impl Record {
    /// Whether the image in `generation` still is what compiling the source in `before` makes.
    fn still(&self, generation: &Path, before: &Snapshot) -> bool {
        self.library.starts_with(generation)
            && self.outputs.contains_key(&self.library)
            && self.outputs.len() == 4
            && self.outputs.keys().all(|it| it.starts_with(generation))
            && unchanged(&self.outputs)
            && self.inputs.iter().all(|(route, read)| {
                std::fs::canonicalize(route).is_ok_and(|it| it == read.file)
                    && before.files.get(&read.file) == Some(&read.digest)
            })
            && self.directories.iter().all(|(dir, was)| entries(dir).ok().as_ref() == Some(was))
    }
}

/// An admitted image, and the leases that keep it from collection while they are held:
/// for as long as the build that asked for it may load it.
pub(crate) struct Image {
    pub(crate) library: PathBuf,
    pub(crate) leases: Vec<File>,
}

mod native_path {
    use super::*;

    pub(super) fn serialize<S: serde::Serializer>(
        path: &Path,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        crate::process::os_bytes(path.as_os_str()).serialize(serializer)
    }

    pub(super) fn deserialize<'de, D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> Result<PathBuf, D::Error> {
        crate::process::from_os_bytes(Vec::<u8>::deserialize(deserializer)?)
            .map(PathBuf::from)
            .map_err(serde::de::Error::custom)
    }
}

mod native_paths {
    use super::*;

    pub(super) fn serialize<V: Serialize, S: serde::Serializer>(
        paths: &BTreeMap<PathBuf, V>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        paths
            .iter()
            .map(|(path, value)| (crate::process::os_bytes(path.as_os_str()), value))
            .collect::<Vec<_>>()
            .serialize(serializer)
    }

    pub(super) fn deserialize<'de, V: Deserialize<'de>, D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> Result<BTreeMap<PathBuf, V>, D::Error> {
        let mut paths = BTreeMap::new();
        for (path, value) in Vec::<(Vec<u8>, V)>::deserialize(deserializer)? {
            let path = crate::process::from_os_bytes(path).map_err(serde::de::Error::custom)?;
            if paths.insert(path.into(), value).is_some() {
                return Err(serde::de::Error::custom("duplicate producer input path"));
            }
        }
        Ok(paths)
    }
}

mod native_bindings {
    use super::*;

    pub(super) fn serialize<S: serde::Serializer>(
        paths: &BTreeMap<PathBuf, PathBuf>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        let values: BTreeMap<_, _> = paths
            .iter()
            .map(|(path, target)| (path.clone(), crate::process::os_bytes(target.as_os_str())))
            .collect();
        native_paths::serialize(&values, serializer)
    }

    pub(super) fn deserialize<'de, D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> Result<BTreeMap<PathBuf, PathBuf>, D::Error> {
        native_paths::deserialize::<Vec<u8>, _>(deserializer)?
            .into_iter()
            .map(|(path, target)| {
                crate::process::from_os_bytes(target)
                    .map(|target| (path, target.into()))
                    .map_err(serde::de::Error::custom)
            })
            .collect()
    }
}

fn digest(path: &Path) -> Result<[u8; 32]> {
    Ok(Sha256::digest(
        std::fs::read(path)
            .with_context(|| format!("reading producer input {}", path.display()))?,
    )
    .into())
}

fn walk(path: &Path, inputs: &mut Snapshot, active: &mut BTreeSet<PathBuf>) -> Result<()> {
    let link = std::fs::symlink_metadata(path)?;
    let (metadata, link) = if link.file_type().is_symlink() {
        (std::fs::metadata(path)?, Some(Revision::of(&link)))
    } else {
        (link, None)
    };
    let canonical = std::fs::canonicalize(path)?;
    inputs.bindings.insert(path.to_owned(), canonical.clone());
    inputs.revisions.insert(path.to_owned(), (Revision::of(&metadata), link));
    if metadata.is_file() {
        inputs.files.insert(canonical, digest(path)?);
    } else if metadata.is_dir() {
        anyhow::ensure!(active.insert(canonical.clone()), "producer source directory cycle");
        let mut entries = std::fs::read_dir(path)?
            .map(|it| it.map(|it| it.path()))
            .collect::<std::io::Result<Vec<_>>>()?;
        entries.sort();
        for entry in entries {
            if entry.ends_with(".git")
                || entry.ends_with("target")
                || entry.join("CACHEDIR.TAG").is_file()
            {
                continue;
            }
            walk(&entry, inputs, active)?;
        }
        active.remove(&canonical);
    } else {
        anyhow::bail!("unsupported producer source {}", path.display());
    }
    Ok(())
}

pub(crate) fn snapshot(sysroot: &Path, crates: &[Planned]) -> Result<Snapshot> {
    let mut inputs = Snapshot::default();
    let mut roots = BTreeSet::new();
    for krate in crates {
        if let Some(image) = &krate.proc_macro {
            walk(image, &mut inputs, &mut BTreeSet::new())?;
        }
        let package = krate
            .root
            .ancestors()
            .skip(1)
            .find(|it| it.join("Cargo.toml").is_file())
            .or_else(|| krate.root.parent())
            .context("producer source without a parent")?;
        roots.insert(package.to_owned());
        if let Some(out) = &krate.out_dir {
            roots.insert(out.clone());
        }
    }
    // Source std is part of an owned image, including local toolchain source edits.
    roots.insert(sysroot.join("lib/rustlib/src/rust/library"));
    for root in roots {
        walk(&root, &mut inputs, &mut BTreeSet::new())?;
    }
    // A changed compiler cannot reuse an earlier producer even under the same version.
    let compiler = std::env::current_exe()?;
    walk(&compiler, &mut inputs, &mut BTreeSet::new())?;
    Ok(inputs)
}

fn unchanged(files: &Files) -> bool {
    files.iter().all(|(path, was)| digest(path).ok().as_ref() == Some(was))
}

/// The names in `dir`, in order: a file that comes or goes there can change which file a
/// module is, without changing any file the compilation read.
fn entries(dir: &Path) -> Result<[u8; 32]> {
    let mut names = std::fs::read_dir(dir)?
        .map(|it| it.map(|it| crate::process::os_bytes(&it.file_name())))
        .collect::<std::io::Result<Vec<_>>>()?;
    names.sort();
    let mut hash = Sha256::new();
    for name in names {
        hash.update((name.len() as u64).to_le_bytes());
        hash.update(name);
    }
    Ok(hash.finalize().into())
}

/// What is compiled and how, but not the content of the crates' own source, which the
/// record of each image of it checks: the description, the environment, and the standard
/// library's source and the compiler, which no record lists.
fn recipe(
    sysroot: &Path,
    description: &str,
    env: &[(OsString, OsString)],
    before: &Snapshot,
) -> Result<String> {
    let context = serde_json::to_vec(&(
        image_description(description)?,
        crate::process::Native::of(env.iter().cloned()),
    ))?;
    let library = std::fs::canonicalize(sysroot.join("lib/rustlib/src/rust/library"))?;
    let compiler = std::fs::canonicalize(std::env::current_exe()?)?;
    anyhow::ensure!(before.files.contains_key(&compiler), "the producer snapshot has no compiler");
    let mut hash = Sha256::new();
    hash.update(b"blaze owned producer image 4\0");
    hash.update((context.len() as u64).to_le_bytes());
    hash.update(context);
    for (path, digest) in &before.files {
        if path.starts_with(&library) || *path == compiler {
            let path = crate::process::os_bytes(path.as_os_str());
            hash.update((path.len() as u64).to_le_bytes());
            hash.update(path);
            hash.update(digest);
        }
    }
    Ok(hash.finalize().iter().map(|it| format!("{it:02x}")).collect())
}

// The random trace key protects one invocation's private input report. It is
// transport entropy, not a source, native environment or codegen input.
fn image_description(description: &str) -> Result<String> {
    let mut project: serde_json::Value = serde_json::from_str(description)?;
    project
        .as_object_mut()
        .context("invalid owned producer description")?
        .remove(crate::dep_info::TRACE_KEY_FIELD);
    Ok(serde_json::to_string(&project)?)
}

/// No rebuild of an admitted immutable image. A missing/corrupt completed image is not
/// reused, and a new one is made beside it rather than over bytes a live proc-macro client
/// may still have mapped.
pub(crate) fn link(
    sysroot: &Path,
    crates: Vec<Planned>,
    dir: &Path,
    env: &[(OsString, OsString)],
    mode: own_script::SourceMode<'_>,
) -> Result<Image> {
    let description = own_script::planned_macro_description(sysroot, crates.clone())
        .map_err(anyhow::Error::msg)?;
    let description = own_script::with_mode(description, mode).map_err(anyhow::Error::msg)?;
    let before = snapshot(sysroot, &crates)?;
    let key = recipe(sysroot, &description, env, &before)?;
    stored(&dir.join("images"), &key, &before, |attempt| {
        let library = own_script::link_macro_planned(sysroot, crates.clone(), attempt, env, mode)
            .map_err(anyhow::Error::msg)?;
        anyhow::ensure!(
            snapshot(sysroot, &crates)? == before,
            "producer source changed during owned compilation; image not admitted"
        );
        let bytes = std::fs::read(attempt.join("producer.inputs"))?;
        let read = match mode {
            own_script::SourceMode::Legacy | own_script::SourceMode::Cargo { .. } => {
                crate::dep_info::DepInfo::parse(std::str::from_utf8(&bytes)?).files
            }
            own_script::SourceMode::Captured { .. } => {
                crate::dep_info::ReplayTrace::decode(&bytes)?.files
            }
        };
        Ok(Built { library, read, dep_info: attempt.join(format!("{}.d", crates[0].name)) })
    })
    .and_then(|(image, reused)| {
        anyhow::ensure!(
            !reused || snapshot(sysroot, &crates)? == before,
            "producer source changed while reusing its image"
        );
        Ok(image)
    })
}

/// What a compilation in an attempt directory made, and the files it says it read.
struct Built {
    library: PathBuf,
    read: Vec<String>,
    dep_info: PathBuf,
}

/// The image of `recipe` under `root` for the source in `before`: one whose record says it
/// still is, or the one `build` makes in a new attempt directory, recorded when what it read
/// is known, and whether it was one made before. Recipes and images no one leases are
/// collected.
fn stored(
    root: &Path,
    recipe: &str,
    before: &Snapshot,
    build: impl FnOnce(&Path) -> Result<Built>,
) -> Result<(Image, bool)> {
    std::fs::create_dir_all(root)?;
    // Separate from generation/GC locks: one writer per image store, held in this command.
    let lock = File::options()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(root.join(".build"))?;
    lock.lock()?;
    let lifetime = |path: &Path| load_cargo::proc_macro_producer_lifetime::lease(path);
    let recipe = root.join(recipe);
    let mut leases = vec![lifetime(&recipe)?];
    // Another compiler, environment or description: no build that could load one leases it.
    load_cargo::proc_macro_producer_lifetime::collect(root, &recipe)?;
    let mut images = std::fs::read_dir(&recipe)?
        .map(|it| it.map(|it| it.path()))
        .collect::<std::io::Result<Vec<_>>>()?;
    images.retain(|it| it.join("complete.json").is_file());
    images.sort();
    for image in images {
        // Collection or invalidation may have taken it since it was listed.
        let Ok(lease) = lifetime(&image) else { continue };
        let Ok(kept) = std::fs::read(image.join("complete.json"))
            .map_err(anyhow::Error::from)
            .and_then(|it| Ok(serde_json::from_slice::<Record>(&it)?))
        else {
            continue;
        };
        let native = kept.library.parent().map(|it| it.join("producer.native.json"));
        if kept.still(&image, before)
            && native.is_some_and(|it| crate::native_inputs::image_still(&it))
        {
            load_cargo::proc_macro_producer_lifetime::collect(&recipe, &image)?;
            leases.push(lease);
            return Ok((Image { library: kept.library, leases }, true));
        }
    }
    let generation = (0u64..)
        .map(|it| recipe.join(it.to_string()))
        .find(|it| !it.exists())
        .context("no producer image directory")?;
    leases.push(lifetime(&generation)?);
    // Images of source that is no more, which no build leases.
    load_cargo::proc_macro_producer_lifetime::collect(&recipe, &generation)?;
    // Uncacheable includes and crashes get another attempt directory. Never compile
    // over bytes a reader could still have loaded from an earlier attempt.
    let attempt = (0u64..)
        .map(|it| generation.join(format!("build-{it}")))
        .find(|it| !it.exists())
        .context("no producer attempt directory")?;
    let Built { library, read: files, dep_info } = match build(&attempt) {
        Ok(built) => built,
        Err(error) => {
            load_cargo::proc_macro_producer_lifetime::invalidate(&generation)?;
            return Err(error);
        }
    };
    let image = Image { library: library.clone(), leases };
    let project: serde_json::Value =
        serde_json::from_slice(&std::fs::read(attempt.join("rust-project.json"))?)?;
    // Unknown external include inputs were not read before compilation. Build succeeds,
    // but do not certify an image for reuse from a post-build-only observation.
    let mut read = BTreeMap::new();
    for file in &files {
        let Ok(route) = crate::compiler_json::physical_path(&project, &attempt.join(file)) else {
            return Ok((image, false));
        };
        let Some((file, digest)) = std::fs::canonicalize(&route)
            .ok()
            .and_then(|file| before.files.get(&file).map(|digest| (file, *digest)))
        else {
            return Ok((image, false));
        };
        read.insert(route, Read { file, digest });
    }
    let native = attempt.join("producer.native.json");
    if !crate::native_inputs::image_still(&native) {
        return Ok((image, false));
    }
    let directories = read
        .keys()
        .filter_map(|route| route.parent())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .map(|dir| Ok((dir.to_owned(), entries(dir)?)))
        .collect::<Result<Files>>()?;
    let outputs = [library.clone(), attempt.join("producer.inputs"), dep_info, native]
        .into_iter()
        .map(|path| Ok((path.clone(), digest(&path)?)))
        .collect::<Result<Files>>()?;
    let kept = Record { inputs: read, directories, outputs, library };
    let pending = generation.join("complete.pending");
    std::fs::write(&pending, serde_json::to_vec(&kept)?)?;
    File::open(&pending)?.sync_all()?;
    std::fs::rename(pending, generation.join("complete.json"))?;
    Ok((image, false))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trace_entropy_does_not_change_producer_identity() {
        let mode = own_script::SourceMode::Captured {
            profile: None,
            units: None,
            runtime: None,
            bindings: None,
            naming: None,
        };
        let described = serde_json::json!({"crates": [{"env": {"INPUT": "one"}}]}).to_string();
        let first = own_script::with_mode(described.clone(), mode).unwrap();
        let second = own_script::with_mode(described, mode).unwrap();
        assert_ne!(first, second, "each trace must have independent protection");
        let identity = image_description(&first).unwrap();
        assert_eq!(identity, image_description(&second).unwrap());
        let mut changed: serde_json::Value = serde_json::from_str(&second).unwrap();
        changed["crates"][0]["env"]["INPUT"] = serde_json::json!("two");
        assert_ne!(identity, image_description(&changed.to_string()).unwrap());
        changed[blaze_wire::native::UNIT_FIELD] = serde_json::json!([1, 2, 3]);
        assert_ne!(
            image_description(&second).unwrap(),
            image_description(&changed.to_string()).unwrap()
        );
    }

    #[test]
    fn snapshots_bind_actual_output_trees_independently_of_out_dir_environment() {
        let dir = tempfile::tempdir().unwrap();
        let package = dir.path().join("package");
        let sysroot = dir.path().join("sysroot");
        let out = dir.path().join("generated");
        std::fs::create_dir(&package).unwrap();
        std::fs::create_dir_all(sysroot.join("lib/rustlib/src/rust/library")).unwrap();
        std::fs::create_dir(&out).unwrap();
        std::fs::write(package.join("Cargo.toml"), "[package]").unwrap();
        std::fs::write(package.join("lib.rs"), "pub fn f() {}").unwrap();
        let mut source = Planned {
            out_dir: None,
            workspace_member: false,
            name: "fixture".into(),
            root: package.join("lib.rs"),
            edition: "2021".into(),
            cfg: Vec::new(),
            codegen_flags: Vec::new(),
            env: [("OUT_DIR".into(), dir.path().join("missing-marker").display().to_string())]
                .into_iter()
                .collect(),
            deps: Vec::new(),
            proc_macro: None,
        };
        snapshot(&sysroot, &[source.clone()]).unwrap();
        source.out_dir = Some(out.clone());
        std::fs::write(out.join("generated.rs"), "one").unwrap();
        let before = snapshot(&sysroot, &[source.clone()]).unwrap();
        std::fs::write(out.join("generated.rs"), "two").unwrap();
        assert_ne!(before, snapshot(&sysroot, &[source]).unwrap());
    }

    #[cfg(unix)]
    #[test]
    fn records_preserve_raw_source_and_image_path_bytes() {
        use std::os::unix::ffi::OsStringExt as _;
        let raw = |bytes: &[u8]| PathBuf::from(OsString::from_vec(bytes.to_vec()));
        let source = raw(b"/source-\xff/main.rs");
        let alias = raw(b"/alias-\xfe/main.rs");
        let image = raw(b"/generation-\xfd/macro.so");
        let record = Record {
            inputs: BTreeMap::from([(
                alias.clone(),
                Read { file: source.clone(), digest: [7; 32] },
            )]),
            directories: BTreeMap::from([(raw(b"/alias-\xfe"), [9; 32])]),
            outputs: BTreeMap::from([(image.clone(), [8; 32])]),
            library: image.clone(),
        };
        let bytes = serde_json::to_vec(&record).unwrap();
        let decoded: Record = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(decoded.inputs, record.inputs);
        assert_eq!(decoded.directories, record.directories);
        assert_eq!(decoded.outputs, record.outputs);
        assert_eq!(decoded.library, image);
        let alternate = Record { library: raw(b"/generation-\xfc/macro.so"), ..decoded };
        assert_ne!(serde_json::to_vec(&alternate).unwrap(), bytes);
    }

    #[cfg(unix)]
    #[test]
    fn snapshots_detect_retargeted_source_aliases() {
        use std::os::unix::fs::symlink;

        for directory in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let first = dir.path().join("first");
            let second = dir.path().join("second");
            let alias = dir.path().join("selected");
            if directory {
                std::fs::create_dir(&first).unwrap();
                std::fs::create_dir(&second).unwrap();
            }
            let file = |path: &Path| {
                if directory { path.join("lib.rs") } else { path.to_owned() }
            };
            std::fs::write(file(&first), b"FIRST").unwrap();
            std::fs::write(file(&second), b"SECOND").unwrap();
            symlink(&first, &alias).unwrap();
            let read = || {
                let mut files = Snapshot::default();
                walk(dir.path(), &mut files, &mut BTreeSet::new()).unwrap();
                files
            };
            let before = read();
            assert_eq!(read(), before);
            std::fs::remove_file(&alias).unwrap();
            symlink(&second, &alias).unwrap();
            assert_eq!(std::fs::read(file(&alias)).unwrap(), b"SECOND");
            assert_ne!(read(), before, "source alias retargeting must change the image key");
        }
    }

    #[test]
    fn snapshots_detect_restored_mtime_changes_and_directory_cycles() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("lib.rs");
        std::fs::write(&file, b"ONE").unwrap();
        let time = std::fs::metadata(&file).unwrap().modified().unwrap();
        let mut files = Snapshot::default();
        walk(dir.path(), &mut files, &mut BTreeSet::new()).unwrap();
        assert!(unchanged(&files.files));
        std::fs::write(&file, b"TWO").unwrap();
        File::options().write(true).open(&file).unwrap().set_modified(time).unwrap();
        assert!(!unchanged(&files.files));
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(dir.path(), dir.path().join("cycle")).unwrap();
            assert!(walk(dir.path(), &mut Snapshot::default(), &mut BTreeSet::new()).is_err());
        }
    }

    #[cfg(unix)]
    #[test]
    fn restored_source_bytes_keep_the_key_but_not_an_in_flight_revision() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("lib.rs");
        std::fs::write(&file, b"FIRST").unwrap();
        let stamp = std::fs::metadata(&file).unwrap().modified().unwrap();
        let capture = || {
            let mut snapshot = Snapshot::default();
            walk(dir.path(), &mut snapshot, &mut BTreeSet::new()).unwrap();
            snapshot
        };
        let before = capture();
        std::thread::sleep(std::time::Duration::from_millis(10));
        std::fs::write(&file, b"OTHER").unwrap();
        std::fs::write(&file, b"FIRST").unwrap();
        File::options().write(true).open(&file).unwrap().set_modified(stamp).unwrap();
        let after = capture();
        assert!(before.same_content(&after));
        assert_eq!(serde_json::to_vec(&before).unwrap(), serde_json::to_vec(&after).unwrap());
        assert_ne!(before, after, "A-to-B-to-A writes must invalidate an in-flight build");
    }

    /// A fake compilation of `source`'s `lib.rs` into `attempt`, as the owned one leaves it.
    fn built(source: &Path, attempt: &Path, made: &std::cell::Cell<u32>) -> Result<Built> {
        made.set(made.get() + 1);
        std::fs::create_dir_all(attempt)?;
        let library = attempt.join("libmacro.dylib");
        std::fs::write(&library, std::fs::read(source.join("lib.rs"))?)?;
        let lib = source.join("lib.rs");
        std::fs::write(attempt.join("producer.inputs"), format!("out: {}\n", lib.display()))?;
        std::fs::write(attempt.join("macro.d"), b"out:\n")?;
        std::fs::write(attempt.join("rust-project.json"), b"{}")?;
        crate::native_inputs::write_empty_image(&attempt.join("producer.native.json"))?;
        Ok(Built {
            library,
            read: vec![lib.display().to_string()],
            dep_info: attempt.join("macro.d"),
        })
    }

    fn images(recipe: &Path) -> Vec<PathBuf> {
        let mut found: Vec<_> = std::fs::read_dir(recipe)
            .unwrap()
            .map(|it| it.unwrap().path())
            .filter(|it| it.is_dir() && it.join("complete.json").is_file())
            .collect();
        found.sort();
        found
    }

    /// An image is the source it read: a file it did not read keeps it, a file it read makes
    /// a new one, and an image or recipe no build leases is collected when another is made.
    #[test]
    fn images_follow_what_they_read_and_unleased_ones_are_collected() {
        let dir = tempfile::tempdir().unwrap();
        let source = std::fs::canonicalize(dir.path()).unwrap().join("source");
        std::fs::create_dir(&source).unwrap();
        std::fs::write(source.join("lib.rs"), b"ONE").unwrap();
        std::fs::write(source.join("README.md"), b"read me").unwrap();
        let root = dir.path().join("images");
        let made = std::cell::Cell::new(0);
        let capture = || {
            let mut snapshot = Snapshot::default();
            walk(&source, &mut snapshot, &mut BTreeSet::new()).unwrap();
            snapshot
        };
        let link = |recipe: &str| {
            stored(&root, recipe, &capture(), |attempt| built(&source, attempt, &made)).unwrap().0
        };
        let first = link("r");
        assert_eq!(std::fs::read(&first.library).unwrap(), b"ONE");
        drop(first.leases);
        std::fs::write(source.join("README.md"), b"changed, but never read").unwrap();
        let again = link("r");
        assert_eq!((made.get(), &again.library), (1, &first.library), "an unread file is no input");
        drop(again.leases);

        std::fs::write(source.join("lib.rs"), b"TWO").unwrap();
        let second = link("r");
        assert_eq!(made.get(), 2);
        assert_eq!(std::fs::read(&second.library).unwrap(), b"TWO");
        assert!(!first.library.exists(), "an unleased image of old source is collected");
        assert_eq!(images(&root.join("r")).len(), 1);

        // A leased image stays while another is made and collected beside it.
        std::fs::write(source.join("lib.rs"), b"THREE").unwrap();
        let third = link("r");
        assert!(second.library.exists(), "a leased image is never collected");
        drop(second.leases);
        std::fs::write(source.join("new.rs"), b"").unwrap();
        let fourth = link("r");
        assert_eq!(made.get(), 4, "a file that comes beside a read one may be a module");
        assert!(third.library.exists());
        drop(third.leases);
        assert_eq!(images(&root.join("r")).len(), 2);

        // Another recipe: the earlier one goes when no build leases it.
        drop(fourth.leases);
        let other = link("s");
        assert!(!root.join("r").exists(), "an unleased recipe is collected");
        assert_eq!(images(&root.join("s")).len(), 1);
        drop(other);
    }

    #[test]
    fn leased_generation_is_not_collected_while_an_image_reader_uses_it() {
        let dir = tempfile::tempdir().unwrap();
        let first = dir.path().join("a".repeat(64));
        let second = dir.path().join("b".repeat(64));
        let reader = load_cargo::proc_macro_producer_lifetime::lease(&first).unwrap();
        let writer = load_cargo::proc_macro_producer_lifetime::lease(&second).unwrap();
        std::fs::write(first.join("image"), b"first").unwrap();
        assert!(
            load_cargo::proc_macro_producer_lifetime::collect(dir.path(), &second)
                .unwrap()
                .is_empty()
        );
        assert_eq!(std::fs::read(first.join("image")).unwrap(), b"first");
        drop(reader);
        assert_eq!(
            load_cargo::proc_macro_producer_lifetime::collect(dir.path(), &second).unwrap(),
            [first]
        );
        drop(writer);
    }
}
