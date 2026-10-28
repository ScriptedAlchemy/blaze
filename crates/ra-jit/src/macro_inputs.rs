//! Compile-time producer content, computed before either cold or warm workspace loading.
//! This is local freshness, not portable identity: raw paths/configuration are preserved.

use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
};

use anyhow::Context as _;
use project_model::{CargoConfig, ProjectWorkspace, ProjectWorkspaceKind};
use sha2::{Digest as _, Sha256};

pub(crate) fn producers(workspace: &ProjectWorkspace) -> Vec<PathBuf> {
    let ProjectWorkspaceKind::Cargo { cargo, .. } = &workspace.kind else { return Vec::new() };
    let mut selected = BTreeSet::new();
    let mut pending: Vec<_> = cargo
        .packages()
        .filter(|&package| {
            cargo[package].targets.iter().any(|&target| cargo[target].kind.is_proc_macro())
        })
        .collect();
    while let Some(package) = pending.pop() {
        if selected.insert(package) {
            pending.extend(cargo[package].dependencies.iter().map(|dependency| dependency.pkg));
        }
    }
    let mut paths: Vec<_> = selected
        .into_iter()
        .map(|package| {
            let manifest: &Path = cargo[package].manifest.as_ref();
            manifest.parent().unwrap().to_owned()
        })
        .collect();
    paths.sort();
    paths.dedup();
    paths
}

fn field(hash: &mut Sha256, bytes: &[u8]) {
    hash.update((bytes.len() as u64).to_le_bytes());
    hash.update(bytes);
}

/// Error is distinct from no producer. Never turn an unreadable closure into an mtime target.
pub(crate) fn generation(
    workspace: &ProjectWorkspace,
    config: &CargoConfig,
) -> anyhow::Result<Option<String>> {
    let paths = producers(workspace);
    if paths.is_empty() {
        return Ok(None);
    }
    let mut hash = Sha256::new();
    field(&mut hash, b"blaze local macro producer inputs 2");
    // Preserve conservative raw context and target/features/toolchain selection.
    // Remove only the output placement selected by this same content operation.
    let mut input_config = config.clone();
    input_config.target_dir_config = Default::default();
    let mut env: Vec<_> = input_config.extra_env.drain().collect();
    env.sort();
    field(&mut hash, format!("{input_config:?}").as_bytes());
    field(&mut hash, format!("{env:?}").as_bytes());
    for path in paths {
        field(&mut hash, path.as_os_str().as_encoded_bytes());
        hash_tree(&path, &path, &mut BTreeSet::new(), &mut hash)
            .with_context(|| format!("reading macro producer inputs {}", path.display()))?;
    }
    Ok(Some(hash.finalize().iter().map(|byte| format!("{byte:02x}")).collect()))
}

/// Retain while the workspace is loaded: scripts can still use this generation's OUT_DIR.
pub(crate) struct Generation {
    path: PathBuf,
    _lease: std::fs::File,
}

impl Generation {
    #[cfg(test)]
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    pub(crate) fn prepare(
        workspace: &ProjectWorkspace,
        config: &mut CargoConfig,
    ) -> anyhow::Result<Option<Self>> {
        let Some(identity) = generation(workspace, config)? else { return Ok(None) };
        // Separate from earlier unmarked producer targets: never claim/delete those.
        let path = PathBuf::from(workspace.workspace_root().to_string())
            .join("target/rust-analyzer/producer-inputs")
            .join(identity);
        let lease = load_cargo::proc_macro_producer_lifetime::lease(&path)
            .with_context(|| format!("leasing macro input generation {}", path.display()))?;
        config.target_dir_config = project_model::TargetDirectoryConfig::Directory(
            paths::AbsPathBuf::assert_utf8(path.clone()).into(),
        );
        Ok(Some(Self { path, _lease: lease }))
    }

    pub(crate) fn collect(&self) -> std::io::Result<Vec<PathBuf>> {
        load_cargo::proc_macro_producer_lifetime::collect(self.path.parent().unwrap(), &self.path)
    }
}

/// Follow actual symlink input bytes, with an active-directory cycle check. A broken link,
/// partial walk, unreadable file or cycle is an error, not absence or a weak stat identity.
fn hash_tree(
    root: &Path,
    path: &Path,
    active: &mut BTreeSet<PathBuf>,
    hash: &mut Sha256,
) -> std::io::Result<()> {
    let metadata = std::fs::metadata(path)?;
    if metadata.is_dir() {
        let canonical = std::fs::canonicalize(path)?;
        if !active.insert(canonical.clone()) {
            return Err(std::io::Error::other(format!(
                "macro input directory cycle: {}",
                path.display()
            )));
        }
        let mut entries: Vec<_> = std::fs::read_dir(path)?
            .map(|entry| entry.map(|entry| entry.path()))
            .collect::<Result<_, _>>()?;
        entries.sort();
        for entry in entries {
            if entry.ends_with(".git")
                || path == root && entry.ends_with("target")
                || entry.join("CACHEDIR.TAG").exists()
                || entry.join("Cargo.toml").exists()
            {
                continue;
            }
            hash_tree(root, &entry, active, hash)?;
        }
        active.remove(&canonical);
    } else if metadata.is_file() {
        field(hash, path.strip_prefix(root).unwrap().as_os_str().as_encoded_bytes());
        field_from_reader(hash, std::fs::File::open(path)?)?;
    } else {
        return Err(std::io::Error::other(format!("unsupported macro input: {}", path.display())));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn digest(root: &Path) -> std::io::Result<[u8; 32]> {
        let mut hash = Sha256::new();
        hash_tree(root, root, &mut BTreeSet::new(), &mut hash)?;
        Ok(hash.finalize().into())
    }

    #[test]
    fn source_and_payload_edits_ignore_restored_metadata_and_generated_targets() {
        let root = std::env::temp_dir().join(format!("macro-input-bytes-{}", std::process::id()));
        std::fs::create_dir_all(root.join("target")).unwrap();
        let payload = root.join("payload.bin");
        std::fs::write(&payload, b"ONE").unwrap();
        let time = std::fs::metadata(&payload).unwrap().modified().unwrap();
        let before = digest(&root).unwrap();
        std::fs::write(&payload, b"TWO").unwrap();
        std::fs::File::options().write(true).open(&payload).unwrap().set_modified(time).unwrap();
        assert_ne!(before, digest(&root).unwrap());
        let current = digest(&root).unwrap();
        std::fs::write(root.join("target/generated"), b"output is not an input").unwrap();
        assert_eq!(current, digest(&root).unwrap());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn unreadable_broken_links_and_directory_cycles_are_errors() {
        use std::os::unix::fs::symlink;
        let root = std::env::temp_dir().join(format!("macro-input-errors-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        symlink(root.join("absent"), root.join("broken")).unwrap();
        assert!(digest(&root).is_err());
        std::fs::remove_file(root.join("broken")).unwrap();
        symlink(&root, root.join("cycle")).unwrap();
        assert!(digest(&root).is_err());
        std::fs::remove_file(root.join("cycle")).unwrap();
        let path = root.join("source");
        std::fs::write(&path, b"read").unwrap();
        let file = std::fs::File::options().write(true).open(&path).unwrap();
        let mut hash = Sha256::new();
        assert!(crate::macro_inputs::field_from_reader(&mut hash, file).is_err());
        std::fs::remove_dir_all(root).unwrap();
    }
}

fn field_from_reader(hash: &mut Sha256, mut reader: impl std::io::Read) -> std::io::Result<()> {
    let mut bytes = Vec::new();
    reader.read_to_end(&mut bytes)?;
    field(hash, &bytes);
    Ok(())
}
