//! The Rust release `cargo blaze` builds with: the one blaze compiles, whatever toolchain the
//! project names. blaze lays out types, names symbols and links as that release does, so cargo,
//! rustc and the daemon all run under it. Rust builds today what an older release built, so a
//! project pinned to an older stable builds the same; one that needs its own toolchain (a
//! nightly's features, a newer release's) fails with rustc's error and a line saying which
//! release built it, and never goes to cargo for it ([#269]).
//!
//! The release comes from, in order: what `RUSTC` names, or the rustc cargo would run when it
//! is that release; rustup's copy of it; blaze's own copy (`own.rs`), downloaded the first
//! time none of those has it.
//!
//! [#269]: https://github.com/indexable-inc/blaze/issues/269

mod own;

use std::{
    ffi::{OsStr, OsString},
    path::{Path, PathBuf},
    sync::OnceLock,
};

use super::state::rustc_says;

/// The release blaze compiles.
pub(crate) const OURS: &str = include_str!("../../../../harness/toolchain");

/// The project's own toolchain, as a failure names it, when blaze builds with its release in
/// its place.
static IN_PLACE_OF: OnceLock<(String, &'static str)> = OnceLock::new();

/// Where the Rust a command builds with came from.
pub(crate) enum Chosen {
    /// The rustc `RUSTC` names or cargo would run, at its sysroot.
    Given(PathBuf),
    Rustup(PathBuf),
    Own(PathBuf),
}

impl std::fmt::Display for Chosen {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let ours = OURS.trim();
        match self {
            Chosen::Given(dir) => write!(f, "Rust {ours}, the toolchain at {}", dir.display()),
            Chosen::Rustup(dir) => write!(f, "Rust {ours} from rustup, at {}", dir.display()),
            Chosen::Own(dir) => write!(f, "Rust {ours}, blaze's own copy at {}", dir.display()),
        }
    }
}

/// Has the cargo, rustc and daemon of this command be blaze's release. A rustc that is that
/// release is used as it is; rustup's copy is named in `RUSTUP_TOOLCHAIN`, which rustup's
/// proxies take over a toolchain file or an override, with its cargo in `CARGO`; `RUSTC`
/// names the selected compiler in every case. blaze's own
/// is named in `CARGO`, `RUSTC` and `RUSTDOC` and is first on `PATH`, for the processes this one
/// starts and not the user's shell. `Err` is why the command can't be built, said for the user
/// to act on. Before any thread starts: it sets the environment.
pub(crate) fn settle() -> Result<Chosen, String> {
    let chosen = choose()?;
    let (Chosen::Given(dir) | Chosen::Rustup(dir) | Chosen::Own(dir)) = &chosen;
    // Resolution uses the compiler selected here, even when PATH names a packaged launcher.
    // SAFETY: before any thread is started.
    unsafe {
        std::env::set_var(
            "RUSTC",
            dir.join("bin").join(format!("rustc{}", std::env::consts::EXE_SUFFIX)),
        );
    }
    Ok(chosen)
}

fn choose() -> Result<Chosen, String> {
    let ours = OURS.trim();
    let other = super::state::other_toolchain_allowed();
    let rustc = super::state::rustc_path();
    let version = version_of(&rustc);
    let shown = version.as_deref().unwrap_or("of no release it says");
    if other {
        let sysroot = sysroot_of(&rustc);
        with_library(&sysroot, shown)?;
        return Ok(Chosen::Given(sysroot));
    }
    let named = super::state::rustc_named().is_some();
    if version.as_deref() == Some(ours) {
        let sysroot = sysroot_of(&rustc);
        match with_library(&sysroot, ours) {
            Ok(()) => {
                rustup_named(&sysroot);
                return Ok(Chosen::Given(sysroot));
            }
            // What `RUSTC` names is used or refused; a rustc on the path without the library
            // leaves the release to rustup or blaze.
            Err(why) if named => return Err(why),
            Err(_) => {}
        }
    } else if named {
        return Err(format!(
            "`RUSTC` names rustc {shown}, and blaze builds with Rust {ours}, the release it \
             compiles: without `RUSTC` it builds with {ours} (`BLAZE_OTHER_TOOLCHAIN=1` builds \
             with {shown} anyway, laying its types out as {ours} does)"
        ));
    }
    if let Some(version) = version.filter(|it| it != ours) {
        let _ = IN_PLACE_OF.set((version, named_by()));
    }
    let rustup = rustup_home().and_then(|home| installed(&home, ours));
    if let Some(dir) = rustup.filter(|dir| with_library(dir, ours).is_ok()) {
        let name = dir.file_name().map_or_else(|| OsString::from(ours), OsStr::to_owned);
        let cargo = dir.join("bin").join(format!("cargo{}", std::env::consts::EXE_SUFFIX));
        // SAFETY: before any thread is started.
        unsafe {
            std::env::set_var("RUSTUP_TOOLCHAIN", name);
            std::env::set_var("CARGO", cargo);
        }
        return Ok(Chosen::Rustup(dir));
    }
    let Some(release) = own::Release::ours() else {
        return Err(format!(
            "blaze builds with Rust {ours}, which is not installed, and the Rust project \
             publishes no copy of it for this machine that blaze knows: install Rust {ours} with \
             its `rust-src` and put it first on `PATH`"
        ));
    };
    let Some(dir) = release.dir() else {
        return Err(format!(
            "blaze builds with Rust {ours} and keeps it in .blaze in your home directory, and \
             there is no home directory: `BLAZE_HOME` names where to keep it"
        ));
    };
    if !own::complete(&dir) {
        own::fetch(&release, &dir)?;
    }
    use_own(&dir);
    Ok(Chosen::Own(dir))
}

/// Has every process this one starts run blaze's own toolchain at `dir`: cargo and the daemon
/// from it, and a rustc, rustdoc or cargo a build script finds on the path, or reaches through a
/// rustup proxy, from it too.
fn use_own(dir: &Path) {
    let bin = dir.join("bin");
    let exe = |name: &str| bin.join(format!("{name}{}", std::env::consts::EXE_SUFFIX));
    let path = std::env::var_os("PATH");
    let path = std::iter::once(bin.clone()).chain(path.iter().flat_map(std::env::split_paths));
    // SAFETY: before any thread is started.
    unsafe {
        std::env::set_var("CARGO", exe("cargo"));
        std::env::set_var("RUSTDOC", exe("rustdoc"));
        std::env::set_var("RUSTUP_TOOLCHAIN", dir);
        if let Ok(path) = std::env::join_paths(path) {
            std::env::set_var("PATH", path);
        }
    }
}

/// Names rustup's toolchain at `sysroot` one way however it was named (`1.99.0`,
/// `1.99.0-<host>`, a path), since the name is part of which build this is.
fn rustup_named(sysroot: &Path) {
    let home = rustup_home().and_then(|it| dunce::canonicalize(it.join("toolchains")).ok());
    let dir = dunce::canonicalize(sysroot).ok();
    let name = dir.filter(|dir| home.is_some_and(|home| dir.parent() == Some(&*home)));
    if std::env::var_os("RUSTUP_TOOLCHAIN").is_some()
        && let Some(name) = name.as_ref().and_then(|it| it.file_name())
    {
        // SAFETY: before any thread is started.
        unsafe { std::env::set_var("RUSTUP_TOOLCHAIN", name) };
    }
}

/// What a build that failed says after its errors: which release built it, when it was not
/// the project's own, since the failure may be what one has and the other has not.
pub(crate) fn failed_note() -> Option<String> {
    let (version, named) = IN_PLACE_OF.get()?;
    Some(format!(
        "blaze built this with Rust {}, the release it compiles, in place of {version}, {named}; \
         an error {version} would not give comes from that difference",
        OURS.trim()
    ))
}

pub(crate) fn note_failed() {
    if let Some(note) = failed_note() {
        super::say::note(&note);
    }
}

/// The release `rustc` says it is, as `1.99.0` or `1.101.0-nightly`.
fn version_of(rustc: &OsStr) -> Option<String> {
    rustc_says(rustc, &["--version"])?.split_whitespace().nth(1).map(str::to_owned)
}

fn sysroot_of(rustc: &OsStr) -> PathBuf {
    PathBuf::from(rustc_says(rustc, &["--print=sysroot"]).unwrap_or_default())
}

/// What chose the project's toolchain, as rustup tells the programs its proxies start.
fn named_by() -> &'static str {
    match std::env::var("RUSTUP_TOOLCHAIN_SOURCE").as_deref() {
        Ok("toolchain-file") => "which the project's rust-toolchain file names",
        Ok("path-override" | "override") => "which `rustup override` names for the project",
        Ok("cli") => "which `+toolchain` names",
        Ok("env") => "which `RUSTUP_TOOLCHAIN` names",
        Ok("default") => "rustup's default toolchain",
        _ => "the toolchain cargo was started with",
    }
}

fn rustup_home() -> Option<PathBuf> {
    if let Some(home) = std::env::var_os("RUSTUP_HOME") {
        return Some(PathBuf::from(home));
    }
    let home = std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" })?;
    Some(PathBuf::from(home).join(".rustup")).filter(|it| it.is_dir())
}

/// The directory rustup installed `release` for this host in: `1.99.0-aarch64-apple-darwin`.
fn installed(home: &Path, release: &str) -> Option<PathBuf> {
    let prefix = format!("{release}-");
    let cargo = format!("cargo{}", std::env::consts::EXE_SUFFIX);
    let mut found: Vec<PathBuf> = std::fs::read_dir(home.join("toolchains"))
        .ok()?
        .flatten()
        .filter(|it| it.file_name().to_str().is_some_and(|name| name.starts_with(&prefix)))
        .map(|it| it.path())
        .filter(|it| it.join("bin").join(&cargo).is_file())
        .collect();
    found.sort();
    let host = |dir: &PathBuf| {
        let name = dir.file_name().and_then(|it| it.to_str()).unwrap_or_default();
        let os = match std::env::consts::OS {
            "macos" => "apple-darwin",
            os => os,
        };
        name[prefix.len()..].starts_with(std::env::consts::ARCH) && name.contains(os)
    };
    found.iter().find(|it| host(it)).or(found.first()).cloned()
}

/// `Ok` when the toolchain at `sysroot` has the source of its standard library, which the
/// daemon compiles.
fn with_library(sysroot: &Path, release: &str) -> Result<(), String> {
    if sysroot.join("lib/rustlib/src/rust/library").is_dir() {
        return Ok(());
    }
    Err(format!(
        "Rust {release} has no source of its standard library, which blaze compiles: \
         `rustup component add rust-src --toolchain {release}` adds it"
    ))
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn settled_compiler_drives_resolution_behind_packaged_proxies() {
        const CHILD: &str = "BLAZE_TEST_SETTLED_RESOLUTION";
        if let Some(branch) = std::env::var_os(CHILD) {
            let chosen = settle().unwrap();
            assert_eq!(matches!(chosen, Chosen::Given(_)), branch == "given");
            let cwd = std::env::current_dir().unwrap();
            let json = blaze_metadata::metadata(&blaze_metadata::Asked {
                cwd: &cwd,
                env: std::env::vars().collect(),
                manifest_path: None,
                config: &[],
                features: &[],
                all_features: false,
                no_default_features: false,
                build: None,
                platform: None,
                offline: true,
                locked: false,
                store: None,
                rust_release: Some(&crate::answers::rust_release),
            })
            .unwrap();
            let metadata: serde_json::Value = serde_json::from_str(&json).unwrap();
            assert_eq!(metadata["packages"][0]["name"], "settled-toolchain");
            return;
        }

        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("rustup");
        let installed = home.join("toolchains/1.99.0-aarch64-apple-darwin");
        crate::answers::tests::synthetic_toolchain(&installed);
        std::fs::create_dir_all(installed.join("lib/rustlib/src/rust/library")).unwrap();
        let cargo = installed.join("bin/cargo");
        std::fs::write(&cargo, "this cargo must not execute").unwrap();
        let proxies = root.path().join("packaged/bin");
        std::fs::create_dir_all(&proxies).unwrap();
        std::fs::write(proxies.join("rustc"), "#!/bin/sh\nexit 97\n").unwrap();
        let project = root.path().join("project");
        std::fs::create_dir_all(project.join("src")).unwrap();
        std::fs::write(
            project.join("Cargo.toml"),
            "[package]\nname='settled-toolchain'\nversion='0.1.0'\nedition='2024'\n",
        )
        .unwrap();
        std::fs::write(project.join("src/lib.rs"), "").unwrap();
        for branch in ["rustup", "given"] {
            let mut command = std::process::Command::new(std::env::current_exe().unwrap());
            command
                .args([
                    "--exact",
                    "cargo::toolchain::tests::settled_compiler_drives_resolution_behind_packaged_proxies",
                    "--nocapture",
                ])
                .env_clear()
                .env(CHILD, branch)
                .env("HOME", root.path())
                .env("USERPROFILE", root.path())
                .env("RUSTUP_HOME", &home)
                .env("PATH", &proxies)
                .env("RUST_BACKTRACE", "0")
                .current_dir(&project);
            if branch == "given" {
                command.env("CARGO", &cargo);
            }
            let output = command.output().unwrap();
            assert!(
                output.status.success(),
                "{branch}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
    }
}
