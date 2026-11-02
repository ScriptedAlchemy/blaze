//! Cargo's native runtime search path, shared by producer and application processes.

use std::{
    collections::BTreeSet,
    ffi::{OsStr, OsString},
    path::{Path, PathBuf},
};

use anyhow::Context as _;

pub(crate) fn variable(triple: &str) -> &'static str {
    if triple.contains("windows") {
        "PATH"
    } else if triple.contains("apple") {
        "DYLD_FALLBACK_LIBRARY_PATH"
    } else {
        "LD_LIBRARY_PATH"
    }
}

pub(crate) fn path<'a>(
    libraries: &[PathBuf],
    profile: &Path,
    sysroot_lib: &Path,
    triple: &str,
    inherited: impl Fn(&str) -> Option<&'a OsStr>,
) -> anyhow::Result<OsString> {
    let prior: Vec<_> = inherited(variable(triple))
        .map(|value| std::env::split_paths(value).collect())
        .unwrap_or_default();
    let mut paths = libraries.to_vec();
    paths.extend([profile.join("deps"), profile.to_owned(), sysroot_lib.to_owned()]);
    paths.extend(prior.iter().cloned());
    if triple.contains("apple") && prior.is_empty() {
        if let Some(home) = inherited("HOME") {
            paths.push(Path::new(home).join("lib"));
        }
        paths.extend([PathBuf::from("/usr/local/lib"), PathBuf::from("/usr/lib")]);
    }
    let mut seen = BTreeSet::new();
    paths.retain(|path| seen.insert(path.clone()));
    std::env::join_paths(paths).context("joining Cargo runtime library paths")
}

pub(crate) fn search_directory(path: &Path) -> &Path {
    let bytes = path.as_os_str().as_encoded_bytes();
    for prefix in [b"native=".as_slice(), b"crate=", b"dependency=", b"framework=", b"all="] {
        if let Some(tail) = bytes.strip_prefix(prefix) {
            // The slice starts after an ASCII '=' boundary in this same native string.
            return Path::new(unsafe { OsStr::from_encoded_bytes_unchecked(tail) });
        }
    }
    path
}

pub(crate) fn artifact_directories(
    output: &blaze_build_script::output::Output,
) -> impl Iterator<Item = PathBuf> + '_ {
    output.library_paths.iter().filter_map(|path| match path {
        blaze_build_script::output::LibraryPath::CargoArtifact(path) => {
            Some(search_directory(path).to_owned())
        }
        blaze_build_script::output::LibraryPath::External(_) => None,
    })
}
