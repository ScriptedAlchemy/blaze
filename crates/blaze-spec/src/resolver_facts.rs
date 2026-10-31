//! Authenticated resolver summaries are independent of selected package source trees.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Result, ensure};
use blaze_metadata::{DependencyFacts, PackageFacts, ResolverFacts};

// Tuples give each version an exact field count. No filesystem location or package
// source byte is encoded: sources are authenticated separately by the snapshot.
type DependencyRecord = (
    (String, Option<String>, (String, Option<String>)),
    (String, Option<String>, (Vec<String>, bool)),
    (bool, bool, Option<String>),
);
type PackageRecord = (
    (String, String, String),
    (Vec<DependencyRecord>, Vec<(String, Vec<String>)>),
    ((Option<String>, Option<String>), (Option<String>, Option<String>)),
);

pub(crate) fn encode(facts: &ResolverFacts) -> Result<Vec<u8>> {
    let mut packages = facts.packages.clone();
    packages.sort();
    let mut identities = BTreeSet::new();
    let packages: Vec<PackageRecord> = packages
        .into_iter()
        .map(|mut package| {
            ensure!(
                identities.insert((
                    package.source.clone(),
                    package.name.clone(),
                    package.version.clone()
                )),
                "duplicate captured resolver package identity"
            );
            package.dependencies.sort();
            let dependencies = package
                .dependencies
                .into_iter()
                .map(|dep| {
                    (
                        (dep.source, dep.registry, (dep.name, dep.rename)),
                        (dep.kind, dep.requirement, (dep.features, dep.default_features)),
                        (dep.optional, dep.public, dep.target),
                    )
                })
                .collect();
            Ok((
                (package.source, package.name, package.version),
                (dependencies, package.features.into_iter().collect()),
                ((package.links, package.rust_version), (package.checksum, package.pubtime)),
            ))
        })
        .collect::<Result<_>>()?;
    Ok(blaze_wire::encode(&(1_u32, packages)))
}

pub(crate) fn decode(bytes: &[u8]) -> Result<ResolverFacts> {
    let (version, records): (u32, Vec<PackageRecord>) =
        blaze_wire::decode(bytes, &blaze_wire::Limits::DOCUMENTED)?;
    ensure!(version == 1, "unknown captured resolver facts version");
    let packages = records
        .into_iter()
        .map(
            |(
                (source, name, version),
                (dependencies, features),
                ((links, rust_version), (checksum, pubtime)),
            )| {
                let count = features.len();
                let features: BTreeMap<_, _> = features.into_iter().collect();
                ensure!(features.len() == count, "duplicate captured resolver feature");
                let dependencies = dependencies
                    .into_iter()
                    .map(
                        |(
                            (source, registry, (name, rename)),
                            (kind, requirement, (features, default_features)),
                            (optional, public, target),
                        )| DependencyFacts {
                            source,
                            registry,
                            name,
                            rename,
                            kind,
                            requirement,
                            features,
                            default_features,
                            optional,
                            public,
                            target,
                        },
                    )
                    .collect();
                Ok(PackageFacts {
                    source,
                    name,
                    version,
                    dependencies,
                    features,
                    links,
                    rust_version,
                    checksum,
                    pubtime,
                })
            },
        )
        .collect::<Result<_>>()?;
    let facts = ResolverFacts { packages };
    ensure!(encode(&facts)? == bytes, "captured resolver facts are not canonical");
    Ok(facts)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn package(name: &str) -> PackageFacts {
        PackageFacts {
            source: "registry+https://github.com/rust-lang/crates.io-index".into(),
            name: name.into(),
            version: "1.2.3".into(),
            dependencies: vec![DependencyFacts {
                source: "registry+https://example.invalid/index".into(),
                registry: Some("https://example.invalid/index".into()),
                name: "dependency".into(),
                rename: Some("renamed".into()),
                kind: "build".into(),
                requirement: Some("^1".into()),
                features: vec!["chosen".into()],
                default_features: false,
                optional: true,
                public: false,
                target: Some("cfg(unix)".into()),
            }],
            features: BTreeMap::from([("chosen".into(), vec!["dep:renamed".into()])]),
            links: Some("native".into()),
            rust_version: Some("1.80".into()),
            checksum: Some("01".repeat(32)),
            pubtime: Some("2026-01-01T00:00:00Z".into()),
        }
    }

    #[test]
    fn complete_summary_records_roundtrip_canonically_without_source_locations() {
        let facts = ResolverFacts { packages: vec![package("b"), package("a")] };
        let bytes = encode(&facts).unwrap();
        let decoded = decode(&bytes).unwrap();
        assert_eq!(decoded.packages[0].name, "a");
        assert_eq!(decoded.packages[1], package("b"));
        assert_eq!(encode(&decoded).unwrap(), bytes);
        let mut reversed = facts;
        reversed.packages.reverse();
        assert_eq!(encode(&reversed).unwrap(), bytes);
        assert!(decode(&bytes[..bytes.len() - 1]).is_err());
    }

    #[test]
    fn duplicate_package_identities_and_unknown_versions_are_rejected() {
        assert!(
            encode(&ResolverFacts { packages: vec![package("same"), package("same")] }).is_err()
        );
        let bytes = blaze_wire::encode(&(2_u32, Vec::<PackageRecord>::new()));
        assert!(decode(&bytes).is_err());
    }
}
