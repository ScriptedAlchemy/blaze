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

#[derive(Serialize, Deserialize)]
struct Record {
    files: Files,
    outputs: Files,
    library: PathBuf,
}

fn digest(path: &Path) -> Result<[u8; 32]> {
    Ok(Sha256::digest(
        std::fs::read(path)
            .with_context(|| format!("reading producer input {}", path.display()))?,
    )
    .into())
}

fn walk(path: &Path, files: &mut Files, active: &mut BTreeSet<PathBuf>) -> Result<()> {
    let metadata = std::fs::metadata(path)?;
    if metadata.is_file() {
        files.insert(std::fs::canonicalize(path)?, digest(path)?);
    } else if metadata.is_dir() {
        let canonical = std::fs::canonicalize(path)?;
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
            walk(&entry, files, active)?;
        }
        active.remove(&canonical);
    } else {
        anyhow::bail!("unsupported producer source {}", path.display());
    }
    Ok(())
}

fn snapshot(sysroot: &Path, crates: &[Planned]) -> Result<Files> {
    let mut files = Files::new();
    let mut roots = BTreeSet::new();
    for krate in crates {
        if let Some(image) = &krate.proc_macro {
            files.insert(std::fs::canonicalize(image)?, digest(image)?);
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
        walk(&root, &mut files, &mut BTreeSet::new())?;
    }
    // A changed compiler cannot reuse an earlier producer even under the same version.
    let compiler = std::env::current_exe()?;
    files.insert(std::fs::canonicalize(&compiler)?, digest(&compiler)?);
    Ok(files)
}

fn unchanged(files: &Files) -> bool {
    files.iter().all(|(path, was)| digest(path).ok().as_ref() == Some(was))
}

/// No rebuild of an admitted immutable image. A missing/corrupt completed generation
/// fails rather than replacing bytes a live proc-macro client may still have mapped.
pub(crate) fn link(
    sysroot: &Path,
    crates: Vec<Planned>,
    dir: &Path,
    env: &[(OsString, OsString)],
) -> Result<PathBuf> {
    let description = own_script::planned_macro_description(sysroot, crates.clone())
        .map_err(anyhow::Error::msg)?;
    let before = snapshot(sysroot, &crates)?;
    let context =
        serde_json::to_vec(&(description, crate::process::Native::of(env.iter().cloned())))?;
    let mut hash = Sha256::new();
    hash.update(b"blaze owned producer image 1\0");
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
            kept.files == before
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
    let library = match own_script::link_macro_planned(sysroot, crates.clone(), &attempt, env) {
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
    let read = crate::dep_info::DepInfo::parse(&std::fs::read_to_string(&inputs)?);
    // Unknown external include inputs were not read before compilation. Build succeeds,
    // but do not certify an image for reuse from a post-build-only observation.
    if read.files.iter().any(|it| {
        std::fs::canonicalize(attempt.join(it)).ok().is_none_or(|path| !before.contains_key(&path))
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
    fn snapshots_detect_restored_mtime_changes_and_directory_cycles() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("lib.rs");
        std::fs::write(&file, b"ONE").unwrap();
        let time = std::fs::metadata(&file).unwrap().modified().unwrap();
        let mut files = Files::new();
        walk(dir.path(), &mut files, &mut BTreeSet::new()).unwrap();
        assert!(unchanged(&files));
        std::fs::write(&file, b"TWO").unwrap();
        File::options().write(true).open(&file).unwrap().set_modified(time).unwrap();
        assert!(!unchanged(&files));
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(dir.path(), dir.path().join("cycle")).unwrap();
            assert!(walk(dir.path(), &mut Files::new(), &mut BTreeSet::new()).is_err());
        }
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
