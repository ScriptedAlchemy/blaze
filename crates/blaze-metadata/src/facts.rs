//! Resolver summaries are input facts, separate from the source trees a build compiles.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context as _, Result, ensure};
use blaze_resolve::{
    DepKind, Dependency, IndexSummary, MemRegistry, PackageId, Resolve, SourceId, Summary,
};
use serde::{Deserialize, Serialize};

/// Normalized facts for exactly the non-path packages in a workspace's lock, including
/// unused patches. A caller authenticates these facts with its captured request.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolverFacts {
    pub packages: Vec<PackageFacts>,
}

/// A logical package summary, independent of archive availability and cache locations.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct PackageFacts {
    pub source: String,
    pub name: String,
    pub version: String,
    pub dependencies: Vec<DependencyFacts>,
    pub features: BTreeMap<String, Vec<String>>,
    pub links: Option<String>,
    pub rust_version: Option<String>,
    pub checksum: Option<String>,
    pub pubtime: Option<String>,
}

/// A dependency as declared by its source, before previous-lock preferences are applied.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct DependencyFacts {
    pub source: String,
    pub registry: Option<String>,
    pub name: String,
    pub rename: Option<String>,
    /// `normal`, `build`, or `dev`.
    pub kind: String,
    /// None distinguishes an omitted path/Git requirement from an explicit `*`.
    pub requirement: Option<String>,
    pub features: Vec<String>,
    pub default_features: bool,
    pub optional: bool,
    pub public: bool,
    pub target: Option<String>,
}

fn key(id: PackageId) -> (String, String, String) {
    (id.source_id().as_url().to_string(), id.name().to_string(), id.version().to_string())
}

impl ResolverFacts {
    pub(crate) fn capture<'a>(
        lock: &'a Resolve,
        patches: impl Iterator<Item = &'a Summary>,
    ) -> Result<Self> {
        let mut summaries: BTreeMap<_, _> = lock
            .summaries()
            .values()
            .chain(patches)
            .filter(|summary| !summary.source_id().is_path())
            .map(|summary| (key(summary.package_id()), summary))
            .collect();
        let mut packages = Vec::new();
        for id in lock.sort().into_iter().chain(lock.unused_patches().iter().copied()) {
            if id.source_id().is_path() {
                continue;
            }
            let summary = summaries
                .remove(&key(id))
                .with_context(|| format!("no resolver summary for locked package {id}"))?;
            let mut facts = PackageFacts::of(summary);
            if let Some(checksum) = lock.checksums().get(&id).and_then(Option::as_ref) {
                facts.checksum = Some(checksum.clone());
            }
            packages.push(facts);
        }
        packages.sort();
        Ok(Self { packages })
    }

    pub(crate) fn add_to(&self, registry: &mut MemRegistry, lock: &Resolve) -> Result<()> {
        let mut expected: BTreeSet<_> = lock
            .sort()
            .into_iter()
            .chain(lock.unused_patches().iter().copied())
            .filter(|id| !id.source_id().is_path())
            .map(key)
            .collect();
        for package in &self.packages {
            let summary = package.summary()?;
            ensure!(
                !summary.source_id().is_path(),
                "captured resolver facts contain a path package"
            );
            ensure!(
                expected.remove(&key(summary.package_id())),
                "duplicate or unbound resolver facts for {} {} from {}",
                package.name,
                package.version,
                package.source
            );
            if let Some(checksum) =
                lock.checksums().get(&summary.package_id()).and_then(Option::as_ref)
            {
                ensure!(
                    package.checksum.as_ref() == Some(checksum),
                    "captured resolver checksum differs from Cargo.lock"
                );
            }
            registry.add(IndexSummary::Candidate(summary));
        }
        ensure!(expected.is_empty(), "captured resolver facts omit locked packages");
        Ok(())
    }

    pub(crate) fn validate_manifest(&self, summary: &Summary) -> Result<()> {
        let actual = PackageFacts::of(summary);
        let expected = self
            .packages
            .iter()
            .find(|package| {
                (&package.source, &package.name, &package.version)
                    == (&actual.source, &actual.name, &actual.version)
            })
            .context("selected source has no captured resolver facts")?;
        let normalized = |mut facts: PackageFacts| -> Result<PackageFacts> {
            facts.checksum = None;
            facts.pubtime = None;
            for dependency in &mut facts.dependencies {
                dependency.source = SourceId::from_url(&dependency.source)?
                    .with_git_precise(None)
                    .as_url()
                    .to_string();
            }
            facts.dependencies.sort();
            Ok(facts)
        };
        ensure!(
            normalized(actual)? == normalized(expected.clone())?,
            "selected source manifest differs from its captured resolver facts"
        );
        Ok(())
    }
}

impl PackageFacts {
    pub(crate) fn of(summary: &Summary) -> Self {
        let mut dependencies: Vec<_> =
            summary.dependencies().iter().map(DependencyFacts::of).collect();
        dependencies.sort();
        let features = summary
            .features()
            .iter()
            .map(|(name, values)| {
                let mut values: Vec<_> = values.iter().map(ToString::to_string).collect();
                values.sort();
                (name.to_string(), values)
            })
            .collect();
        Self {
            source: summary.source_id().as_url().to_string(),
            name: summary.name().to_string(),
            version: summary.version().to_string(),
            dependencies,
            features,
            links: summary.links().map(|value| value.to_string()),
            rust_version: summary.rust_version().map(ToString::to_string),
            checksum: summary.checksum().map(str::to_owned),
            pubtime: summary.pubtime().map(str::to_owned),
        }
    }

    fn summary(&self) -> Result<Summary> {
        let id = PackageId::try_new(&self.name, &self.version, SourceId::from_url(&self.source)?)?;
        let features = self
            .features
            .iter()
            .map(|(name, values)| {
                (name.as_str().into(), values.iter().map(|value| value.as_str().into()).collect())
            })
            .collect();
        let dependencies =
            self.dependencies.iter().map(DependencyFacts::dependency).collect::<Result<_>>()?;
        let mut summary = Summary::new(
            id,
            dependencies,
            &features,
            self.links.as_deref(),
            self.rust_version.as_deref().map(str::parse).transpose()?,
        )?;
        if let Some(checksum) = &self.checksum {
            ensure!(
                checksum.len() == 64 && checksum.bytes().all(|byte| byte.is_ascii_hexdigit()),
                "invalid captured registry checksum"
            );
            summary.set_checksum(checksum.clone());
        }
        if let Some(pubtime) = &self.pubtime {
            summary.set_pubtime(pubtime.clone());
        }
        Ok(summary)
    }
}

impl DependencyFacts {
    fn of(dependency: &Dependency) -> Self {
        let mut features: Vec<_> = dependency.features().iter().map(ToString::to_string).collect();
        features.sort();
        Self {
            source: dependency.source_id().as_url().to_string(),
            registry: dependency.registry_id().map(|source| source.as_url().to_string()),
            name: dependency.package_name().to_string(),
            rename: dependency.explicit_name_in_toml().map(|value| value.to_string()),
            kind: match dependency.kind() {
                DepKind::Normal => "normal",
                DepKind::Build => "build",
                DepKind::Development => "dev",
            }
            .to_owned(),
            requirement: dependency.specified_req().then(|| dependency.version_req().to_string()),
            features,
            default_features: dependency.uses_default_features(),
            optional: dependency.is_optional(),
            public: dependency.is_public(),
            target: dependency.platform().map(ToString::to_string),
        }
    }

    fn dependency(&self) -> Result<Dependency> {
        let mut dependency = Dependency::parse(
            &self.name,
            self.requirement.as_deref(),
            SourceId::from_url(&self.source)?,
        )?;
        dependency.set_kind(match self.kind.as_str() {
            "normal" => DepKind::Normal,
            "build" => DepKind::Build,
            "dev" => DepKind::Development,
            _ => anyhow::bail!("invalid captured dependency kind"),
        });
        if let Some(source) = &self.registry {
            dependency.set_registry_id(SourceId::from_url(source)?);
        }
        if let Some(name) = &self.rename {
            dependency.set_explicit_name_in_toml(name.as_str());
        }
        dependency.set_features(self.features.iter().map(String::as_str));
        dependency.set_default_features(self.default_features);
        dependency.set_optional(self.optional);
        dependency.set_public(self.public);
        dependency.set_platform(self.target.as_deref().map(str::parse).transpose()?);
        Ok(dependency)
    }
}
