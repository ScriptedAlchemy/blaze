//! Immutable owned producer images. Publication follows stable before/after source
//! snapshots; loaded macro clients lease the generation through proc-macro-api.

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
    files: Snapshot,
    #[serde(with = "native_paths")]
    outputs: Files,
    #[serde(with = "native_path")]
    library: PathBuf,
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
        if let Some(out) = krate.env.get("OUT_DIR") {
            roots.insert(out.into());
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

/// No rebuild of an admitted immutable image. A missing/corrupt completed generation
/// fails rather than replacing bytes a live proc-macro client may still have mapped.
pub(crate) fn link(
    sysroot: &Path,
    crates: Vec<Planned>,
    dir: &Path,
    env: &[(OsString, OsString)],
    mode: own_script::SourceMode<'_>,
) -> Result<PathBuf> {
    let description = own_script::planned_macro_description(sysroot, crates.clone())
        .map_err(anyhow::Error::msg)?;
    let description = own_script::with_mode(description, mode).map_err(anyhow::Error::msg)?;
    let before = snapshot(sysroot, &crates)?;
    let context = serde_json::to_vec(&(
        image_description(&description)?,
        crate::process::Native::of(env.iter().cloned()),
    ))?;
    let mut hash = Sha256::new();
    hash.update(b"blaze owned producer image 3\0");
    hash.update((context.len() as u64).to_le_bytes());
    hash.update(context);
    hash.update(serde_json::to_vec(&before)?);
    let key = hash.finalize().iter().map(|it| format!("{it:02x}")).collect::<String>();
    let root = dir.join("images");
    std::fs::create_dir_all(&root)?;
    // Separate from generation/GC locks: one writer per image store, held in this command.
    let lock = File::options()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(root.join(".build"))?;
    lock.lock()?;
    let generation = root.join(key);
    let _lease = load_cargo::proc_macro_producer_lifetime::lease(&generation)?;
    let record = generation.join("complete.json");
    if record.exists() {
        let kept: Record = serde_json::from_slice(&std::fs::read(&record)?)?;
        anyhow::ensure!(
            kept.files.same_content(&before)
                && kept.library.starts_with(&generation)
                && kept.outputs.contains_key(&kept.library)
                && kept.outputs.len() == 4
                && kept.outputs.keys().all(|it| it.starts_with(&generation))
                && unchanged(&kept.outputs),
            "completed owned producer image is corrupt; no leased image was replaced"
        );
        if crate::native_inputs::image_still(
            &kept.library.parent().context("image directory")?.join("producer.native.json"),
        ) {
            anyhow::ensure!(
                snapshot(sysroot, &crates)? == before,
                "producer source changed while reusing its image"
            );
            return Ok(kept.library);
        }
    }
    // Uncacheable includes and crashes get another attempt directory. Never compile
    // over bytes a reader could still have loaded from an earlier attempt.
    let attempt = (0u64..)
        .map(|it| generation.join(format!("build-{it}")))
        .find(|it| !it.exists())
        .context("no producer attempt directory")?;
    let library = match own_script::link_macro_planned(sysroot, crates.clone(), &attempt, env, mode)
    {
        Ok(library) => library,
        Err(error) => {
            load_cargo::proc_macro_producer_lifetime::invalidate(&generation)?;
            return Err(anyhow::Error::msg(error));
        }
    };
    let after = snapshot(sysroot, &crates)?;
    if after != before {
        load_cargo::proc_macro_producer_lifetime::invalidate(&generation)?;
        anyhow::bail!("producer source changed during owned compilation; image not admitted");
    }
    let inputs = attempt.join("producer.inputs");
    let bytes = std::fs::read(&inputs)?;
    let files = match mode {
        own_script::SourceMode::Legacy => {
            crate::dep_info::DepInfo::parse(std::str::from_utf8(&bytes)?).files
        }
        own_script::SourceMode::Captured { .. } => {
            crate::dep_info::ReplayTrace::decode(&bytes)?.files
        }
    };
    let project: serde_json::Value =
        serde_json::from_slice(&std::fs::read(attempt.join("rust-project.json"))?)?;
    // Unknown external include inputs were not read before compilation. Build succeeds,
    // but do not certify an image for reuse from a post-build-only observation.
    if files.iter().any(|it| {
        crate::compiler_json::physical_path(&project, &attempt.join(it))
            .and_then(|path| Ok(std::fs::canonicalize(path)?))
            .ok()
            .is_none_or(|path| !before.files.contains_key(&path))
    }) {
        return Ok(library);
    }
    let native = attempt.join("producer.native.json");
    if !crate::native_inputs::image_still(&native) {
        return Ok(library);
    }
    let dep_info = attempt.join(format!("{}.d", crates[0].name));
    let outputs = [library.clone(), inputs, dep_info, native]
        .into_iter()
        .map(|path| Ok((path.clone(), digest(&path)?)))
        .collect::<Result<Files>>()?;
    let kept = Record { files: before, outputs, library: library.clone() };
    let pending = generation.join("complete.pending");
    std::fs::write(&pending, serde_json::to_vec(&kept)?)?;
    File::open(&pending)?.sync_all()?;
    std::fs::rename(pending, record)?;
    Ok(library)
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

    #[cfg(unix)]
    #[test]
    fn records_preserve_raw_source_and_image_path_bytes() {
        use std::os::unix::ffi::OsStringExt as _;
        let raw = |bytes: &[u8]| PathBuf::from(OsString::from_vec(bytes.to_vec()));
        let source = raw(b"/source-\xff/main.rs");
        let alias = raw(b"/alias-\xfe/main.rs");
        let image = raw(b"/generation-\xfd/macro.so");
        let before = Snapshot {
            files: BTreeMap::from([(source.clone(), [7; 32])]),
            bindings: BTreeMap::from([(alias.clone(), source.clone())]),
            ..Snapshot::default()
        };
        let record = Record {
            files: before,
            outputs: BTreeMap::from([(image.clone(), [8; 32])]),
            library: image.clone(),
        };
        let bytes = serde_json::to_vec(&record).unwrap();
        let decoded: Record = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(decoded.files.files, record.files.files);
        assert_eq!(decoded.files.bindings, record.files.bindings);
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
