//! Editions, resolver versions and `cargo-features`, as cargo 1.99 knows them
//! (`src/workspace/features.rs` and `src/resolver/types.rs` there).

use std::fmt;
use std::str::FromStr;

use anyhow::bail;
use semver::Version;

/// The release of cargo whose behaviour this crate has, in the form cargo prints itself.
pub const CARGO_VERSION: &str = "1.99.0 (5f94df478 2026-08-27)";

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Edition {
    #[default]
    Edition2015,
    Edition2018,
    Edition2021,
    Edition2024,
    EditionFuture,
}

impl Edition {
    pub const LATEST_STABLE: Edition = Edition::Edition2024;
    pub const ALL: &'static [Edition] = &[
        Edition::Edition2015,
        Edition::Edition2018,
        Edition::Edition2021,
        Edition::Edition2024,
        Edition::EditionFuture,
    ];

    /// The release that stabilized it.
    pub fn first_version(&self) -> Option<Version> {
        match self {
            Edition::Edition2015 => None,
            Edition::Edition2018 => Some(Version::new(1, 31, 0)),
            Edition::Edition2021 => Some(Version::new(1, 56, 0)),
            Edition::Edition2024 => Some(Version::new(1, 85, 0)),
            Edition::EditionFuture => None,
        }
    }

    pub fn is_stable(&self) -> bool {
        *self != Edition::EditionFuture
    }

    pub fn default_resolve_behavior(&self) -> ResolveBehavior {
        if *self >= Edition::Edition2024 {
            ResolveBehavior::V3
        } else if *self >= Edition::Edition2021 {
            ResolveBehavior::V2
        } else {
            ResolveBehavior::V1
        }
    }
}

impl fmt::Display for Edition {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Edition::Edition2015 => "2015",
            Edition::Edition2018 => "2018",
            Edition::Edition2021 => "2021",
            Edition::Edition2024 => "2024",
            Edition::EditionFuture => "future",
        })
    }
}

impl FromStr for Edition {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> anyhow::Result<Edition> {
        match s {
            "2015" => Ok(Edition::Edition2015),
            "2018" => Ok(Edition::Edition2018),
            "2021" => Ok(Edition::Edition2021),
            "2024" => Ok(Edition::Edition2024),
            "future" => Ok(Edition::EditionFuture),
            s if s.parse().is_ok_and(|y: u16| y > 2024 && y < 2050) => bail!(
                "this version of Cargo is older than the `{s}` edition, \
                 and only supports `2015`, `2018`, `2021`, and `2024` editions."
            ),
            s => bail!(
                "supported edition values are `2015`, `2018`, `2021`, or `2024`, \
                 but `{s}` is unknown"
            ),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ResolveBehavior {
    V1,
    V2,
    V3,
}

impl ResolveBehavior {
    pub fn from_manifest(resolver: &str) -> anyhow::Result<ResolveBehavior> {
        match resolver {
            "1" => Ok(ResolveBehavior::V1),
            "2" => Ok(ResolveBehavior::V2),
            "3" => Ok(ResolveBehavior::V3),
            s => bail!(
                "`resolver` setting `{s}` is not valid, valid options are \"1\", \"2\" or \"3\""
            ),
        }
    }

    pub fn to_manifest(&self) -> &'static str {
        match self {
            ResolveBehavior::V1 => "1",
            ResolveBehavior::V2 => "2",
            ResolveBehavior::V3 => "3",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Status {
    Stable,
    Unstable,
    Removed,
}

/// A feature of `cargo-features`: its name in the manifest, status, the release that
/// stabilized or removed it, and the page of cargo's book about it.
#[derive(Debug)]
pub struct Feature {
    name: &'static str,
    status: Status,
    version: &'static str,
    docs: &'static str,
}

macro_rules! features {
    ($(($status:ident, $name:ident, $version:expr, $docs:expr),)*) => {
        const FEATURES: &[Feature] = &[
            $(Feature {
                name: stringify!($name),
                status: features!(@status $status),
                version: $version,
                docs: $docs,
            },)*
        ];

        #[allow(non_upper_case_globals)]
        impl Feature {
            $(pub const $name: &'static Feature = &Feature {
                name: stringify!($name),
                status: features!(@status $status),
                version: $version,
                docs: $docs,
            };)*
        }
    };
    (@status stable) => { Status::Stable };
    (@status unstable) => { Status::Unstable };
    (@status removed) => { Status::Removed };
}

features! {
    (stable, test_dummy_stable, "1.0", ""),
    (unstable, test_dummy_unstable, "", "reference/unstable.html"),
    (stable, alternative_registries, "1.34", "reference/registries.html"),
    (stable, edition, "1.31", "reference/manifest.html#the-edition-field"),
    (stable, rename_dependency, "1.31", "reference/specifying-dependencies.html#renaming-dependencies-in-cargotoml"),
    (removed, publish_lockfile, "1.37", "reference/unstable.html#publish-lockfile"),
    (stable, profile_overrides, "1.41", "reference/profiles.html#overrides"),
    (stable, default_run, "1.37", "reference/manifest.html#the-default-run-field"),
    (unstable, metabuild, "", "reference/unstable.html#metabuild"),
    (unstable, public_dependency, "", "reference/unstable.html#public-dependency"),
    (stable, named_profiles, "1.57", "reference/profiles.html#custom-profiles"),
    (stable, resolver, "1.51", "reference/resolver.html#resolver-versions"),
    (stable, strip, "1.58", "reference/profiles.html#strip-option"),
    (stable, rust_version, "1.56", "reference/manifest.html#the-rust-version-field"),
    (stable, edition2021, "1.56", "reference/manifest.html#the-edition-field"),
    (unstable, per_package_target, "", "reference/unstable.html#per-package-target"),
    (unstable, codegen_backend, "", "reference/unstable.html#codegen-backend"),
    (unstable, different_binary_name, "", "reference/unstable.html#different-binary-name"),
    (unstable, profile_rustflags, "", "reference/unstable.html#profile-rustflags-option"),
    (stable, workspace_inheritance, "1.64", "reference/unstable.html#workspace-inheritance"),
    (stable, edition2024, "1.85", "reference/manifest.html#the-edition-field"),
    (unstable, trim_paths, "", "reference/unstable.html#profile-trim-paths-option"),
    (unstable, open_namespaces, "", "reference/unstable.html#open-namespaces"),
    (unstable, path_bases, "", "reference/unstable.html#path-bases"),
    (unstable, unstable_editions, "", "reference/unstable.html#unstable-editions"),
    (unstable, multiple_build_scripts, "", "reference/unstable.html#multiple-build-scripts"),
    (unstable, panic_immediate_abort, "", "reference/unstable.html#panic-immediate-abort"),
}

const SEE_CHANNELS: &str = "See https://doc.rust-lang.org/book/appendix-07-nightly-rust.html for more information \
     about Rust release channels.";

/// The `cargo-features` of a manifest. blaze is the stable release, so no unstable feature is
/// ever enabled: one listed is refused as stable cargo refuses it.
#[derive(Clone, Debug, Default)]
pub struct Features {
    is_local: bool,
    enabled: Vec<String>,
}

impl Features {
    pub fn new(
        features: &[String],
        warnings: &mut Vec<String>,
        is_local: bool,
    ) -> anyhow::Result<Features> {
        Self::for_channel(features, warnings, is_local, "stable")
    }

    /// Cargo checks manifest feature gates against this invocation's channel. Bootstrap
    /// resolves the installed std workspace on a nightly channel even in a stable release.
    pub fn for_channel(
        features: &[String],
        warnings: &mut Vec<String>,
        is_local: bool,
        channel: &str,
    ) -> anyhow::Result<Features> {
        let mut seen = Vec::new();
        let nightly = matches!(channel, "nightly" | "dev");
        for name in features {
            let Some(feature) = FEATURES.iter().find(|f| f.name.replace('_', "-") == *name) else {
                let mut msg = format!("unknown Cargo.toml feature `{name}`\n\n");
                if name.contains('_') {
                    msg.push_str("Feature names must use '-' instead of '_'.\n");
                } else {
                    msg.push_str(
                        "See https://doc.rust-lang.org/nightly/cargo/reference/unstable.html for more information.\n",
                    );
                }
                bail!(msg)
            };
            if seen.contains(&name) {
                bail!("the cargo feature `{name}` has already been activated");
            }
            seen.push(name);
            let see_docs = format!(
                "See {} for more information about using this feature.",
                cargo_docs_link(feature.docs)
            );
            match feature.status {
                Status::Stable => warnings.push(format!(
                    "the cargo feature `{name}` has been stabilized in the {} release and is no \
                     longer necessary to be listed in the manifest\n  {see_docs}",
                    feature.version
                )),
                Status::Unstable if nightly => {}
                Status::Unstable => bail!(
                    "the cargo feature `{name}` requires a nightly version of Cargo, but this is \
                     the `stable` channel\n{SEE_CHANNELS}\n{see_docs}"
                ),
                Status::Removed => {
                    let mut msg = format!(
                        "the cargo feature `{name}` has been removed in the {} release\n\n",
                        feature.version
                    );
                    if is_local {
                        msg.push_str("Remove the feature from Cargo.toml to remove this error.\n");
                    } else {
                        msg.push_str(&format!(
                            "This package cannot be used with this version of Cargo, as the \
                             unstable feature `{name}` is no longer supported.\n"
                        ));
                    }
                    msg.push_str(&see_docs);
                    msg.push('\n');
                    bail!(msg)
                }
            }
        }
        Ok(Features { is_local, enabled: if nightly { features.to_vec() } else { Vec::new() } })
    }

    /// Fails as cargo does for a manifest that uses an unstable feature: none is enabled.
    pub fn require(&self, feature: &Feature) -> anyhow::Result<()> {
        self.require_with_hint(feature, None)
    }

    pub fn require_with_hint(&self, feature: &Feature, hint: Option<&str>) -> anyhow::Result<()> {
        if feature.status == Status::Stable
            || self.enabled.iter().any(|it| *it == feature.name.replace('_', "-"))
        {
            return Ok(());
        }
        let _ = self.is_local;
        let name = feature.name.replace('_', "-");
        let mut msg = format!(
            "feature `{name}` is required\n\n\
             The package requires the Cargo feature called `{name}`, but that feature is not \
             stabilized in this version of Cargo ({CARGO_VERSION}).\n"
        );
        msg.push_str(
            "Consider trying a newer version of Cargo (this may require the nightly release).\n",
        );
        msg.push_str(&format!(
            "See https://doc.rust-lang.org/nightly/cargo/{} for more information about the status of this feature.\n",
            feature.docs
        ));
        if let Some(hint) = hint {
            msg.push_str(hint);
            msg.push('\n');
        }
        bail!("{msg}")
    }
}

fn cargo_docs_link(path: &str) -> String {
    format!("https://doc.rust-lang.org/cargo/{path}")
}

#[cfg(test)]
mod channel_tests {
    use super::*;

    #[test]
    fn unstable_manifest_gates_follow_the_invocations_channel() {
        let names = vec!["profile-rustflags".to_owned(), "public-dependency".to_owned()];
        assert!(Features::for_channel(&names, &mut Vec::new(), true, "stable").is_err());
        let enabled = Features::for_channel(&names, &mut Vec::new(), true, "nightly").unwrap();
        assert!(enabled.require(Feature::profile_rustflags).is_ok());
        assert!(enabled.require(Feature::public_dependency).is_ok());
        assert!(enabled.require(Feature::metabuild).is_err());
        assert!(
            Features::for_channel(&["not-a-feature".to_owned()], &mut Vec::new(), true, "nightly")
                .is_err()
        );
    }
}
