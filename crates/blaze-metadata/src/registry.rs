//! The resolver's source queries, retaining logical source ids across replacements.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::sync::Arc;

use anyhow::{Context as _, Result};
use blaze_manifest::manifest::{EitherManifest, read_manifest};
use blaze_manifest::metadata::export_package;
use blaze_registry::index::{IndexLine, Status};
use blaze_resolve::{
    DepKind, Dependency, IndexSummary, InternedString, MemRegistry, PackageId, QueryKind, Registry,
    SourceId, Summary,
};
use rustc_hash::FxHashMap;

pub(crate) struct SourcesRegistry<'a> {
    paths: MemRegistry,
    sources: &'a blaze_registry::Sources,
    context: &'a Arc<blaze_registry::Context>,
    manifests: &'a blaze_manifest::Context,
    packages: RefCell<FxHashMap<String, Vec<Summary>>>,
}

impl<'a> SourcesRegistry<'a> {
    pub(crate) fn new(
        paths: MemRegistry,
        sources: &'a blaze_registry::Sources,
        context: &'a Arc<blaze_registry::Context>,
        manifests: &'a blaze_manifest::Context,
    ) -> Self {
        Self { paths, sources, context, manifests, packages: RefCell::default() }
    }

    fn source_packages(
        &self,
        source: SourceId,
        from: &blaze_registry::SourceId,
    ) -> Result<Vec<Summary>> {
        let key = source.as_url().to_string();
        if let Some(packages) = self.packages.borrow().get(&key) {
            return Ok(packages.clone());
        }
        let mut packages = Vec::new();
        let mut dirs = Vec::new();
        match from.kind() {
            blaze_registry::SourceKind::Directory => {
                for package in self.sources.directory(from)?.packages()? {
                    dirs.push((package.root.clone(), source, package.checksum.clone()));
                }
            }
            blaze_registry::SourceKind::Git(_) => {
                let _lock = blaze_registry::PackageCacheLock::acquire(&self.context.cargo_home)?;
                let checkout =
                    blaze_registry::git::fetch(self.context, from, source.precise_git_fragment())?;
                let precise = source.with_git_precise(Some(checkout.rev.clone()));
                for package in checkout.packages()? {
                    dirs.push((package.dir, precise, None));
                }
            }
            _ => unreachable!("registry and path sources are queried separately"),
        }
        for (dir, id, checksum) in dirs {
            let path = dir.join("Cargo.toml");
            let manifest_source =
                crate::manifest_source(&id.as_url().to_string(), &self.manifests.config)?;
            let EitherManifest::Real(manifest) =
                read_manifest(&path, manifest_source, self.manifests)?
            else {
                continue;
            };
            let mut exported = export_package(&path, &manifest);
            exported.set_source(id.as_url().to_string());
            let mut summary = crate::summary::read(&serde_json::to_value(exported)?)?.summary;
            // A path inside a git checkout is part of the same git source.
            if id.is_git() {
                summary = summary.map_dependencies(|mut dep| {
                    if dep.source_id().is_path() {
                        dep.set_source_id(id);
                    }
                    dep
                });
            }
            if let Some(checksum) = checksum {
                summary.set_checksum(checksum);
            }
            packages.push(summary);
        }
        self.packages.borrow_mut().insert(key, packages.clone());
        Ok(packages)
    }
}

impl Registry for SourcesRegistry<'_> {
    async fn query(
        &self,
        dep: &Dependency,
        kind: QueryKind,
        f: &mut dyn FnMut(IndexSummary),
    ) -> Result<()> {
        if dep.source_id().is_path() {
            return self.paths.query(dep, kind, f).await;
        }
        let source = dep.source_id();
        let (physical, precise) = blaze_registry::SourceId::from_url(&source.as_url().to_string())?;
        let from = self.sources.map().replacement(&physical, precise.is_some())?;
        let normalized = |name: &str| name.replace('-', "_");
        let matches_name = |name: &str| match kind {
            QueryKind::AlternativeNames | QueryKind::Normalized => {
                normalized(name) == normalized(dep.package_name().as_str())
            }
            _ => name == dep.package_name().as_str(),
        };
        if matches!(
            from.kind(),
            blaze_registry::SourceKind::Directory | blaze_registry::SourceKind::Git(_)
        ) {
            for summary in self.source_packages(source, &from)? {
                if matches_name(summary.name().as_str())
                    && (matches!(kind, QueryKind::AlternativeNames | QueryKind::Normalized)
                        || dep.matches(&summary))
                {
                    f(IndexSummary::Candidate(summary));
                }
            }
            return Ok(());
        }
        let registry = self.sources.registry(&from)?;
        // A fresh choice needs current versions. A locked query can use the existing cache.
        if !dep.is_locked() && !registry.is_updated() && !self.context.net.offline {
            registry.invalidate();
        }
        let matches = |line: &IndexLine| {
            matches_name(&line.entry.name)
                && (matches!(kind, QueryKind::AlternativeNames | QueryKind::Normalized)
                    || dep.version_req().matches(&line.entry.version))
        };
        let mut summaries = registry.summaries(dep.package_name().as_str())?;
        if dep.is_locked()
            && !self.context.net.offline
            && !registry.is_updated()
            && !summaries.all().any(|line| matches(&line))
        {
            registry.invalidate();
            summaries = registry.summaries(dep.package_name().as_str())?;
        }
        let mut lines: Vec<_> = summaries.all().filter(&matches).collect();
        // Cargo prefers cached archives offline, but still resolves an uncached package
        // when no matching archive is available, reporting the failure during fetching.
        if self.context.net.offline && !registry.is_local() {
            let available = |line: &IndexLine| {
                registry.has_crate(&line.entry.name, &line.entry.version, &line.entry.checksum)
            };
            if lines.iter().any(available) {
                lines.retain(available);
            }
        }
        for line in lines {
            let status = line.status;
            let id = PackageId::new(
                InternedString::new(&line.entry.name),
                line.entry.version.clone(),
                source,
            );
            let summary = match index_summary(line, source) {
                Ok(summary) => summary,
                Err(_) => {
                    if kind == QueryKind::RejectedVersions {
                        f(IndexSummary::Invalid(Summary::new(
                            id,
                            vec![],
                            &BTreeMap::new(),
                            None::<&str>,
                            None,
                        )?));
                    }
                    continue;
                }
            };
            let selected = match status {
                Status::Candidate => IndexSummary::Candidate(summary),
                Status::Yanked => IndexSummary::Yanked(summary),
                Status::Invalid => IndexSummary::Invalid(summary),
                Status::Unsupported(version) => IndexSummary::Unsupported(summary, version),
            };
            if matches!(selected, IndexSummary::Candidate(_) | IndexSummary::Yanked(_))
                || kind == QueryKind::RejectedVersions
            {
                f(selected);
            }
        }
        Ok(())
    }

    fn describe_source(&self, source: SourceId) -> String {
        source.to_string()
    }

    fn is_replaced(&self, source: SourceId) -> bool {
        blaze_registry::SourceId::from_url(&source.as_url().to_string()).is_ok_and(
            |(id, precise)| {
                self.sources.map().replacement(&id, precise.is_some()).is_ok_and(|from| from != id)
            },
        )
    }
}

fn index_summary(line: IndexLine, source: SourceId) -> Result<Summary> {
    let entry = line.entry;
    let id = PackageId::new(InternedString::new(&entry.name), entry.version, source);
    let deps = entry
        .deps
        .into_iter()
        .map(|dep| {
            let from = dep
                .registry
                .as_ref()
                .map(|id| SourceId::for_registry(id.url()))
                .transpose()?
                .unwrap_or(source);
            let mut d = Dependency::parse(&dep.package, Some(&dep.req.to_string()), from)?;
            if dep.name_in_toml != dep.package {
                d.set_explicit_name_in_toml(&dep.name_in_toml);
            }
            d.set_kind(match dep.kind {
                blaze_registry::index::DepKind::Normal => DepKind::Normal,
                blaze_registry::index::DepKind::Development => DepKind::Development,
                blaze_registry::index::DepKind::Build => DepKind::Build,
            });
            d.set_optional(dep.optional)
                .set_default_features(dep.default_features)
                .set_features(dep.features.iter().map(String::as_str))
                .set_public(dep.public);
            d.set_platform(dep.platform);
            Ok(d)
        })
        .collect::<Result<Vec<_>>>()?;
    let features: BTreeMap<_, _> = entry
        .features
        .into_iter()
        .map(|(name, values)| {
            (InternedString::new(&name), values.iter().map(|it| InternedString::new(it)).collect())
        })
        .collect();
    let rust_version = entry.rust_version.map(|it| it.to_string().parse()).transpose()?;
    let mut summary = Summary::new(id, deps, &features, entry.links.as_deref(), rust_version)
        .with_context(|| format!("invalid index summary for {id}"))?;
    summary.set_checksum(entry.checksum);
    if let Some(time) = entry.pubtime {
        summary.set_pubtime(time);
    }
    Ok(summary)
}
