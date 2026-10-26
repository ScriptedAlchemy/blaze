//! What the requester's environment may hold, and which of it a build script is given.
//!
//! A variable that changes what blaze builds is refused by name ([`REFUSED`]) unless the spec
//! carries it: the four settings of blaze's code generators are carried per unit
//! ([`crate::profile::Settings`]) and the variables a build script may read are carried in
//! `script_env`. A `RA_JIT_` variable is refused unless it is carried or is one of
//! [`RA_JIT_NEUTRAL`], the settings of where blaze keeps its state, its caches and remotes, its
//! memory budget and what it prints, which change nothing a build writes.
//!
//! **`script_env` is a fixed list, not a proof.** A build script sees the requester's value of
//! each variable [`script_env`] names, as cargo gives the script its own environment. A script
//! that reads any other variable sees the worker's, which is unset unless the worker sets it:
//! `-sys` crates read `<NAME>_DIR`, `<NAME>_LIB_DIR` and the like, and none of those can be named
//! in advance. A build whose script does is a build the spec does not describe, and nothing here
//! notices. `NUM_JOBS` and the other values cargo makes per run are not carried; `PATH` is not,
//! the platform finds the C compilers by the requester's `PATH` and digests them instead.

use std::collections::BTreeMap;

use crate::Refusal;
use crate::refuse::Reason;

/// A variable refused by name, with what it does and why blaze cannot carry it.
pub const REFUSED: &[(&str, Reason)] = &[
    ("RUSTC", Reason::EnvRustc),
    ("RUSTC_WRAPPER", Reason::EnvRustcWrapper),
    ("RUSTC_WORKSPACE_WRAPPER", Reason::EnvRustcWrapper),
    ("RUSTDOC", Reason::EnvRustdoc),
    ("RUSTUP_TOOLCHAIN", Reason::EnvToolchain),
    ("RA_JIT_OTHER_TOOLCHAIN", Reason::EnvToolchain),
    ("RUSTC_BOOTSTRAP", Reason::EnvBootstrap),
    ("RUSTC_OVERRIDE_VERSION_STRING", Reason::EnvVersionString),
];

/// Variables cargo reads that change nothing of what a build writes, left out of a spec:
/// `CARGO_INCREMENTAL` and `CARGO_TARGET_DIR` (where and how cargo writes), `CARGO_HOME` (where
/// cargo keeps packages, which the lock file's checksums pin), `CARGO_TERM_*`, `CARGO_LOG`,
/// `CARGO_HTTP_*` and `CARGO_NET_*` (what is printed and how packages are fetched).
pub const CARGO_IGNORED: &[&str] = &[
    "CARGO_INCREMENTAL",
    "CARGO_TARGET_DIR",
    "CARGO_HOME",
    "CARGO_TERM_",
    "CARGO_LOG",
    "CARGO_HTTP_",
    "CARGO_NET_",
];

/// The variables blaze reads for what its code generators do, carried per unit.
pub const RA_JIT_CARRIED: &[&str] =
    &["RA_JIT_BACKEND", "RA_JIT_OPT", "RA_JIT_EMIT", "RA_JIT_MIR_OPT"];

/// The `RA_JIT_` variables that change nothing a build writes, by name or, where the entry ends
/// in `_`, by what the name starts with, and why.
pub const RA_JIT_NEUTRAL: &[(&str, &str)] = &[
    ("RA_JIT_HOME", "where the daemon keeps its socket, lock, log and workers"),
    ("RA_JIT_PROJECT", "which project a remote cache files results under"),
    ("RA_JIT_PROJECTS", "which projects a remote cache files results under"),
    ("RA_JIT_CACHE", "where the store is and whether it is used; entries are content-addressed"),
    ("RA_JIT_CACHE_MAX", "a bound on the size of the store"),
    ("RA_JIT_CACHE_SEAL_AT", "when the store seals a pack"),
    ("RA_JIT_CACHE_CHECK", "compiles what a cache has too, to compare, and keeps the cache's"),
    ("RA_JIT_REMOTE", "the remote cache's address and settings"),
    ("RA_JIT_REMOTE_", "the remote cache's address and settings"),
    ("RA_JIT_FN_CACHE", "the cache of `#[ra_jit::cache]` results, which a program runs with"),
    ("RA_JIT_FN_CACHE_", "the cache of `#[ra_jit::cache]` results, which a program runs with"),
    ("RA_JIT_MEMORY", "the daemon's memory budget and its sampling"),
    ("RA_JIT_MEMORY_", "the daemon's memory budget and its sampling"),
    ("RA_JIT_OWN_BASE_MEMORY", "the daemon's soft base memory budget"),
    ("RA_JIT_RECLAIM", "how the daemon gives memory back"),
    ("RA_JIT_IDLE_SECS", "how long a daemon waits before it ends"),
    ("RA_JIT_SPARE", "whether the daemon keeps a spare worker"),
    ("RA_JIT_STATS", "what the daemon prints of itself"),
    ("RA_JIT_TRACE", "what the daemon traces"),
    ("RA_JIT_DUMP_", "what the code generators print of what they compile"),
    ("RA_JIT_SLOW_SWITCH_MS", "when the daemon says a switch of checkouts was slow"),
    ("RA_JIT_FUZZ_RUNS", "how many runs the fuzz tests make"),
];

fn is_neutral(name: &str) -> bool {
    RA_JIT_NEUTRAL.iter().any(
        |(it, _)| {
            if it.ends_with('_') { name.starts_with(it) } else { name == *it }
        },
    ) || (name.starts_with("RA_JIT_") && name.ends_with("_PAUSE_MS"))
}

/// Refuses the first variable of `env` that changes what blaze builds and that a spec does not
/// carry.
pub fn check(env: &BTreeMap<String, String>) -> Result<(), Refusal> {
    for (name, reason) in REFUSED {
        // cargo takes an empty wrapper for none.
        let wrapper = name.ends_with("_WRAPPER");
        if env.get(*name).is_some_and(|value| !(wrapper && value.is_empty())) {
            return Err(Refusal::of(*reason, *name));
        }
    }
    for name in env.keys() {
        if name.starts_with("CARGO_UNSTABLE_") || name.starts_with("CARGO_HOST_") {
            return Err(Refusal::of(Reason::EnvUnstable, name.clone()));
        }
        let carried = RA_JIT_CARRIED.contains(&name.as_str());
        if name.starts_with("RA_JIT_") && !carried && !is_neutral(name) {
            return Err(Refusal::of(Reason::EnvRaJit, name.clone()));
        }
    }
    Ok(())
}

/// The C and C++ toolchain variables a build script may read, by name.
const SCRIPT_EXACT: &[&str] = &[
    "CC",
    "CXX",
    "AR",
    "RANLIB",
    "CFLAGS",
    "CXXFLAGS",
    "ARFLAGS",
    "CPPFLAGS",
    "LDFLAGS",
    "CXXSTDLIB",
    "CRATE_CC_NO_DEFAULTS",
    "CC_ENABLE_DEBUG_OUTPUT",
    "CC_KNOWN_WRAPPER_MAP",
    "CC_SHELL_ESCAPED_FLAGS",
    "SDKROOT",
    "DEVELOPER_DIR",
    "CPATH",
    "C_INCLUDE_PATH",
    "CPLUS_INCLUDE_PATH",
    "OBJC_INCLUDE_PATH",
    "LIBRARY_PATH",
    "PKG_CONFIG",
    "PKG_CONFIG_PATH",
    "PKG_CONFIG_LIBDIR",
    "PKG_CONFIG_SYSROOT_DIR",
    "PKG_CONFIG_ALLOW_CROSS",
    "PKG_CONFIG_ALL_STATIC",
    "PKG_CONFIG_ALL_DYNAMIC",
    "PKG_CONFIG_ALLOW_SYSTEM_CFLAGS",
    "PKG_CONFIG_ALLOW_SYSTEM_LIBS",
];

/// The variables the `cc` and `pkg-config` crates look up with the target's name too
/// (`CC_aarch64_apple_darwin`, `HOST_CC`, `TARGET_CC`, `PKG_CONFIG_PATH_x86_64_unknown_linux_gnu`).
const SCRIPT_TARGETED: &[&str] = &[
    "CC",
    "CXX",
    "AR",
    "RANLIB",
    "CFLAGS",
    "CXXFLAGS",
    "ARFLAGS",
    "CPPFLAGS",
    "PKG_CONFIG",
    "PKG_CONFIG_PATH",
    "PKG_CONFIG_LIBDIR",
    "PKG_CONFIG_SYSROOT_DIR",
    "PKG_CONFIG_ALLOW_CROSS",
];

/// Whether a build script of a build for `triple` may read the variable `name`.
pub fn is_script_var(name: &str, triple: &str) -> bool {
    if SCRIPT_EXACT.contains(&name) {
        return true;
    }
    // `MACOSX_DEPLOYMENT_TARGET` and the others of the SDKs.
    if name.strip_suffix("_DEPLOYMENT_TARGET").is_some_and(|it| {
        !it.is_empty() && it.bytes().all(|b| b.is_ascii_uppercase() || b.is_ascii_digit())
    }) {
        return true;
    }
    // `<NAME>_NO_PKG_CONFIG` of the `pkg-config` crate.
    if name.strip_suffix("_NO_PKG_CONFIG").is_some_and(|it| !it.is_empty()) {
        return true;
    }
    let underscored = triple.replace('-', "_");
    SCRIPT_TARGETED.iter().any(|base| {
        [triple, underscored.as_str()].iter().any(|it| name == format!("{base}_{it}"))
            || name == format!("HOST_{base}")
            || name == format!("TARGET_{base}")
    })
}

/// The variables of `env` a build script for `triple` may read, with the requester's values.
pub fn script_env(env: &BTreeMap<String, String>, triple: &str) -> Vec<(String, String)> {
    env.iter()
        .filter(|(name, _)| is_script_var(name, triple))
        .map(|(name, value)| (name.clone(), value.clone()))
        .collect()
}
