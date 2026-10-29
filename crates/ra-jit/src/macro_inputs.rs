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

/// Provenance of cargo's supplied libraries. Resolution here is blaze's, not a workspace
/// load or a compiler query. Keep all platforms/dependency kinds conservatively: cargo
/// can compile host producers outside the program's target graph.
pub(crate) struct ArtifactInputs {
    pub(crate) libraries: std::collections::BTreeMap<PathBuf, [u8; 32]>,
    paths: Vec<PathBuf>,
    context: Vec<u8>,
    configuration: Vec<(PathBuf, Option<Vec<u8>>)>,
    root: PathBuf,
    identity: String,
    _lease: std::fs::File,
}

fn artifact_digest(paths: &[PathBuf], context: &[u8]) -> anyhow::Result<String> {
    let mut hash = Sha256::new();
    field(&mut hash, b"blaze supplied macro producer inputs 1");
    field(&mut hash, context);
    for path in paths {
        field(&mut hash, path.as_os_str().as_encoded_bytes());
        hash_tree(path, path, &mut BTreeSet::new(), &mut hash)
            .with_context(|| format!("reading macro producer inputs {}", path.display()))?;
    }
    Ok(hash.finalize().iter().map(|byte| format!("{byte:02x}")).collect())
}

impl ArtifactInputs {
    pub(crate) fn capture(
        project: &Path,
        selection: &crate::plan::Selection,
    ) -> anyhow::Result<Option<Self>> {
        Self::capture_in(project, selection, crate::process::vars().collect())
    }

    pub(crate) fn capture_in(
        project: &Path,
        selection: &crate::plan::Selection,
        env: Vec<(String, String)>,
    ) -> anyhow::Result<Option<Self>> {
        // Cargo may ask through a directory symlink while its compiler and daemon
        // work in the physical checkout. Resolve before metadata, config ancestry
        // and package paths enter this identity, on every capture path.
        let project = dunce::canonicalize(project)
            .with_context(|| format!("resolving macro producer project {}", project.display()))?;
        let project = project.as_path();
        let manifest = project.join("Cargo.toml");
        let producer_env = crate::cargo::producer_env_in(&env);
        let asked = blaze_metadata::Asked {
            cwd: project,
            env,
            manifest_path: Some(&manifest),
            config: &selection.config,
            features: &selection.features,
            all_features: selection.all_features,
            no_default_features: selection.no_default_features,
            build: None,
            platform: None,
            offline: selection.offline,
            locked: selection.locked,
            store: None,
        };
        let mut metadata: cargo_metadata::Metadata =
            serde_json::from_str(&blaze_metadata::metadata(&asked)?)?;
        let mut selected = BTreeSet::new();
        let mut pending: Vec<_> = metadata
            .packages
            .iter()
            .filter(|package| package.targets.iter().any(|target| target.is_proc_macro()))
            .map(|package| package.id.clone())
            .collect();
        let resolve =
            metadata.resolve.as_ref().context("macro provenance needs a resolve graph")?;
        while let Some(id) = pending.pop() {
            if selected.insert(id.clone()) {
                let node = resolve
                    .nodes
                    .iter()
                    .find(|node| node.id == id)
                    .context("macro producer absent from resolve graph")?;
                pending.extend(node.dependencies.iter().cloned());
            }
        }
        let mut paths: Vec<_> = metadata
            .packages
            .iter()
            .filter(|package| selected.contains(&package.id))
            .map(|package| package.manifest_path.parent().unwrap().as_std_path().to_owned())
            .collect();
        paths.sort();
        paths.dedup();
        if paths.is_empty() {
            return Ok(None);
        }
        // Selection includes profile, target, features and config. The worker's build
        // environment is stable across client and worker; output/safety env is not input.
        metadata.target_directory = Default::default();
        let context = serde_json::to_vec(&(selection, producer_env, &metadata))?;
        let mut configuration = Vec::new();
        let workspace_root = metadata.workspace_root.as_std_path();
        for path in [workspace_root.join("Cargo.toml"), workspace_root.join("Cargo.lock")] {
            configuration.push((path.clone(), Some(std::fs::read(path)?)));
        }
        for ancestor in project.ancestors() {
            for name in [".cargo/config", ".cargo/config.toml"] {
                let path = ancestor.join(name);
                match std::fs::read(&path) {
                    Ok(bytes) => configuration.push((path, Some(bytes))),
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                        configuration.push((path, None));
                    }
                    Err(error) => return Err(error.into()),
                }
            }
        }
        let context = serde_json::to_vec(&(context, &configuration))?;
        let identity = artifact_digest(&paths, &context)?;
        let root = metadata
            .workspace_root
            .as_std_path()
            // Artifact proof outlives a loaded generation and a daemon restart. It is
            // not a build output and must not share Generation::collect's namespace.
            .join("target/rust-analyzer/producer-records")
            .join(&identity);
        let lease = load_cargo::proc_macro_producer_lifetime::lease(&root)?;
        Ok(Some(Self {
            libraries: Default::default(),
            paths,
            context,
            configuration,
            root,
            identity,
            _lease: lease,
        }))
    }

    fn current(&self) -> anyhow::Result<bool> {
        for (path, before) in &self.configuration {
            let now = match std::fs::read(path) {
                Ok(bytes) => Some(bytes),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
                Err(error) => return Err(error.into()),
            };
            if &now != before {
                return Ok(false);
            }
        }
        Ok(artifact_digest(&self.paths, &self.context)? == self.identity)
    }

    /// This is evidence of a completed existing build, never permission to start one.
    pub(crate) fn record(&self, artifact: &cargo_metadata::Artifact) -> anyhow::Result<()> {
        if artifact.fresh || !artifact.target.is_proc_macro() || !self.current()? {
            return Ok(());
        }
        for library in artifact.filenames.iter().filter(|path| is_dylib(path.as_std_path())) {
            self.record_library(library.as_std_path())?;
        }
        Ok(())
    }

    /// The workspace wrapper has no Cargo message. It observes the same successful
    /// compilation directly; a later fresh message never establishes proof.
    pub(crate) fn record_library(&self, library: &Path) -> anyhow::Result<()> {
        if !self.current()? {
            return Ok(());
        }
        let bytes = std::fs::read(library)?;
        if !self.current()? {
            return Ok(());
        }
        let hash = Sha256::digest(&bytes);
        let name: String = hash.iter().map(|byte| format!("{byte:02x}")).collect();
        crate::replace(&self.root.join(format!("artifact-{name}")), self.identity.as_bytes())?;
        Ok(())
    }

    fn accepts(&mut self, artifact: &cargo_metadata::Artifact) -> anyhow::Result<()> {
        anyhow::ensure!(self.current()?, "macro producer changed during warm load");
        for library in artifact.filenames.iter().filter(|path| is_dylib(path.as_std_path())) {
            let hash = Sha256::digest(std::fs::read(library)?);
            let name: String = hash.iter().map(|byte| format!("{byte:02x}")).collect();
            let record = self.root.join(format!("artifact-{name}"));
            anyhow::ensure!(
                std::fs::read(record).ok().as_deref() == Some(self.identity.as_bytes()),
                "refusing warm load: proc-macro artifact {} has no matching producer record; \
                 cargo's result stands, no rebuild was requested",
                library
            );
            self.libraries.insert(library.as_std_path().to_owned(), hash.into());
        }
        Ok(())
    }
}

fn is_dylib(path: &Path) -> bool {
    matches!(path.extension().and_then(|it| it.to_str()), Some("dylib" | "so" | "dll"))
}

/// Every message consumer, including the streaming loader, uses the same gate.
pub(crate) fn validate_artifacts(
    project: &Path,
    selection: &crate::plan::Selection,
    lines: &[String],
) -> anyhow::Result<Option<ArtifactInputs>> {
    let artifacts: Vec<cargo_metadata::Artifact> = lines
        .iter()
        .filter_map(|line| match serde_json::from_str::<cargo_metadata::Message>(line) {
            Ok(cargo_metadata::Message::CompilerArtifact(artifact))
                if artifact.target.is_proc_macro() =>
            {
                Some(artifact)
            }
            _ => None,
        })
        .collect();
    if artifacts.is_empty() {
        return Ok(None);
    }
    let mut inputs = ArtifactInputs::capture(project, selection)?
        .context("refusing warm load: proc-macro artifact outside the producer closure")?;
    for artifact in artifacts {
        inputs.accepts(&artifact)?;
    }
    Ok(Some(inputs))
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
    pub(crate) artifacts: Option<ArtifactInputs>,
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
        Ok(Some(Self { path, _lease: lease, artifacts: None }))
    }

    pub(crate) fn collect(&self) -> std::io::Result<Vec<PathBuf>> {
        load_cargo::proc_macro_producer_lifetime::collect(self.path.parent().unwrap(), &self.path)
    }

    pub(crate) fn verify(
        &self,
        workspace: &ProjectWorkspace,
        config: &CargoConfig,
    ) -> anyhow::Result<()> {
        let expected = self.path.file_name().unwrap().to_str().unwrap();
        match generation(workspace, config) {
            Ok(Some(actual)) if actual == expected => Ok(()),
            result => {
                load_cargo::proc_macro_producer_lifetime::invalidate(&self.path)?;
                match result {
                    Err(error) => Err(error)
                        .context("macro producer changed or became unreadable during build"),
                    _ => {
                        anyhow::bail!("macro producer changed during build; generation invalidated")
                    }
                }
            }
        }
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
