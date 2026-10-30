use std::path::{Path, PathBuf};

use anyhow::Context;
use paths::AbsPathBuf;
use project_model::ProjectManifest;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum LoadOrigin {
    #[default]
    Discover,
    CargoPackage,
}

#[derive(Clone, Debug)]
pub(crate) enum ProjectSource {
    Discover(PathBuf),
    Explicit(ProjectManifest),
}

#[derive(Clone, Debug)]
pub(crate) struct ProjectInput {
    pub(crate) project: PathBuf,
    pub(crate) source: ProjectSource,
}

impl ProjectInput {
    pub(crate) fn new(project: &Path, origin: LoadOrigin) -> anyhow::Result<Self> {
        let location = std::env::current_dir()?.join(project);
        let project = dunce::canonicalize(project)?;
        let source = match origin {
            LoadOrigin::Discover => ProjectSource::Discover(location),
            LoadOrigin::CargoPackage => {
                ProjectSource::Explicit(ProjectManifest::from_manifest_file(
                    AbsPathBuf::assert_utf8(project.join("Cargo.toml")),
                )?)
            }
        };
        Ok(Self { project, source })
    }

    pub(crate) fn manifest(&self) -> anyhow::Result<ProjectManifest> {
        match &self.source {
            ProjectSource::Discover(path) => {
                ProjectManifest::discover_single(&AbsPathBuf::assert_utf8(path.clone()))
            }
            ProjectSource::Explicit(manifest) => {
                std::fs::metadata(manifest.manifest_path())?;
                Ok(manifest.clone())
            }
        }
    }

    pub(crate) fn location(&self) -> &Path {
        match &self.source {
            ProjectSource::Discover(path) => path,
            ProjectSource::Explicit(_) => &self.project,
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct CheckoutRoot {
    /// The latest request's input; `manifest` records the last successful load.
    pub(crate) input: ProjectInput,
    pub(crate) workspace: PathBuf,
    pub(crate) manifest: ProjectManifest,
}

impl CheckoutRoot {
    pub(crate) fn selects(&self, input: &ProjectInput) -> anyhow::Result<bool> {
        let selected = input.manifest()?;
        if std::mem::discriminant(&self.manifest) != std::mem::discriminant(&selected) {
            return Ok(false);
        }
        let location = |manifest: &ProjectManifest| -> anyhow::Result<PathBuf> {
            let path: &Path = manifest.manifest_path().as_ref();
            let directory = dunce::canonicalize(path.parent().context("manifest has no parent")?)?;
            Ok(directory.join(path.file_name().context("manifest has no file name")?))
        };
        let loaded = location(&self.manifest)?;
        let selected = location(&selected)?;
        if loaded == selected {
            return Ok(true);
        }
        Ok(
            match (loaded.strip_prefix(&self.input.project), selected.strip_prefix(&input.project))
            {
                (Ok(loaded), Ok(selected)) => loaded == selected,
                _ => false,
            },
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn package(root: &Path) {
        fs::create_dir_all(root).unwrap();
        fs::write(root.join("Cargo.toml"), "[package]\nname='selected'\nversion='0.1.0'\n")
            .unwrap();
    }

    fn loaded(input: ProjectInput) -> CheckoutRoot {
        let manifest = input.manifest().unwrap();
        CheckoutRoot { workspace: input.project.clone(), input, manifest }
    }

    #[test]
    fn explicit_cargo_does_not_discover_an_ancestor_json() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("member");
        package(&project);
        for contents in ["{\"crates\":[]}", "{"] {
            fs::write(dir.path().join("rust-project.json"), contents).unwrap();
            let cargo = ProjectInput::new(&project, LoadOrigin::CargoPackage).unwrap();
            assert!(matches!(cargo.manifest().unwrap(), ProjectManifest::CargoToml(_)));
            let discovered = ProjectInput::new(&project, LoadOrigin::Discover).unwrap();
            assert!(matches!(discovered.manifest().unwrap(), ProjectManifest::ProjectJson(_)));
        }
        fs::remove_file(project.join("Cargo.toml")).unwrap();
        let cargo = ProjectInput::new(&project, LoadOrigin::CargoPackage).unwrap();
        assert!(cargo.manifest().is_err());
    }

    #[test]
    #[cfg(unix)]
    fn discovery_retains_a_recognized_symlink_name() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("descriptor.json"), "{\"crates\":[]}").unwrap();
        let descriptor = dir.path().join("rust-project.json");
        std::os::unix::fs::symlink("descriptor.json", &descriptor).unwrap();
        let input = ProjectInput::new(&descriptor, LoadOrigin::Discover).unwrap();
        assert_eq!(input.location(), descriptor);
        assert_eq!(input.project, dunce::canonicalize(dir.path().join("descriptor.json")).unwrap());
        assert!(matches!(input.manifest().unwrap(), ProjectManifest::ProjectJson(_)));
    }

    #[test]
    #[cfg(unix)]
    fn directory_aliases_preserve_cargo_manifest_compatibility() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("actual/member");
        package(&project);
        std::os::unix::fs::symlink("actual", dir.path().join("alias")).unwrap();
        let aliased = dir.path().join("alias/member");
        let root = loaded(ProjectInput::new(&aliased, LoadOrigin::Discover).unwrap());
        assert!(
            root.selects(&ProjectInput::new(&project, LoadOrigin::CargoPackage).unwrap()).unwrap()
        );
    }

    #[test]
    #[cfg(unix)]
    fn shared_json_bytes_do_not_erase_different_relative_source_directories() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("descriptor.json"), "{\"crates\":[]}").unwrap();
        let paths = ["a", "b"].map(|name| {
            let directory = dir.path().join(name);
            fs::create_dir(&directory).unwrap();
            let path = directory.join("rust-project.json");
            std::os::unix::fs::symlink("../descriptor.json", &path).unwrap();
            path
        });
        let root = loaded(ProjectInput::new(&paths[0], LoadOrigin::Discover).unwrap());
        let incoming = ProjectInput::new(&paths[1], LoadOrigin::Discover).unwrap();
        assert_eq!(root.input.project, incoming.project);
        assert!(!root.selects(&incoming).unwrap());
    }

    #[test]
    fn compatibility_uses_the_manifest_loaded_before_discovery_changes() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("member");
        package(&project);
        let root = loaded(ProjectInput::new(&project, LoadOrigin::Discover).unwrap());
        assert!(
            root.selects(&ProjectInput::new(&project, LoadOrigin::CargoPackage).unwrap()).unwrap()
        );
        fs::write(dir.path().join("rust-project.json"), "{\"crates\":[]}").unwrap();
        assert!(
            !root.selects(&ProjectInput::new(&project, LoadOrigin::Discover).unwrap()).unwrap()
        );
        assert!(
            root.selects(&ProjectInput::new(&project, LoadOrigin::CargoPackage).unwrap()).unwrap()
        );
    }

    #[test]
    fn checkout_mapping_does_not_reuse_another_roots_absolute_manifest() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a/member");
        let b = dir.path().join("b/member");
        package(&a);
        package(&b);
        let root = loaded(ProjectInput::new(&a, LoadOrigin::CargoPackage).unwrap());
        assert!(root.selects(&ProjectInput::new(&b, LoadOrigin::CargoPackage).unwrap()).unwrap());
        for parent in [a.parent().unwrap(), b.parent().unwrap()] {
            fs::write(parent.join("rust-project.json"), "{\"crates\":[]}").unwrap();
        }
        let root = loaded(ProjectInput::new(&a, LoadOrigin::Discover).unwrap());
        assert!(!root.selects(&ProjectInput::new(&b, LoadOrigin::Discover).unwrap()).unwrap());
    }
}
