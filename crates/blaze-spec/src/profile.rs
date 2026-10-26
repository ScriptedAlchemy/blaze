//! A unit's profile as the spec's [`ProfileSpec`]: what rustc would end up with once the
//! profile's settings and the `-C` flags of the rustflags are applied, and what blaze's own
//! backend and MIR level are.
//!
//! The settings that change what a unit's code is are carried: `opt-level`, `debug`,
//! `debug-assertions`, `overflow-checks`, `panic`, `strip` and `split-debuginfo`. Two are
//! ignored, with the reason in [`IGNORED`]: `codegen-units` and `incremental`, which blaze,
//! compiling one function at a time with its own state, never reads. The rest is refused by
//! name: `lto`, `codegen-backend`, the profile's `rustflags`, `trim-paths` and `rpath = true`.
//!
//! `backend`, `backend_opt`, `backend_emit` and `mir_opt` are not in the profile: blaze reads
//! them from `RA_JIT_BACKEND`, `RA_JIT_OPT`, `RA_JIT_EMIT` and `RA_JIT_MIR_OPT` by the rules of
//! `hir-ty-codegen`'s `Backend::read_env` and `mir_opt_level`, and they are the same on every
//! unit of a build.

use std::collections::BTreeMap;

use blaze_manifest::profiles::{DebugInfo, Lto, PanicStrategy, Profile, Strip, StripInner};
use blaze_wire::ProfileSpec;
use cargo_util_schemas::manifest::TomlDebugInfo;

use crate::Refusal;
use crate::refuse::Reason;
use crate::rustflags::Overrides;

/// The profile settings blaze ignores, and why.
pub const IGNORED: &[(&str, &str)] = &[
    (
        "codegen-units",
        "blaze compiles one function at a time and reads no count of units of a crate",
    ),
    (
        "incremental",
        "blaze keeps its own state and reads no setting of cargo's incremental compilation",
    ),
    ("hint-mostly-unused", "a hint to rustc about build speed, which changes no code"),
];

/// blaze's own settings of what its code generators do, from the requester's environment.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Settings {
    /// 0 Cranelift, 1 LLVM.
    pub backend: u8,
    /// LLVM's passes, 0 to 3; 0 under Cranelift.
    pub backend_opt: u8,
    /// LLVM's code selection, 0 optimized, 1 fast; 0 under Cranelift.
    pub backend_emit: u8,
    /// The MIR optimisation level, 0 to 2.
    pub mir_opt: u8,
}

const BACKEND: &str = "RA_JIT_BACKEND";
const OPTIMIZATION: &str = "RA_JIT_OPT";
const SELECTION: &str = "RA_JIT_EMIT";
const MIR_OPT: &str = "RA_JIT_MIR_OPT";

impl Settings {
    /// What `env` asks of blaze's code generators, by the rules `Backend::read_env` and
    /// `mir_opt_level` read it with: a value they would refuse or fall back from is refused
    /// here, so that a spec never says what a worker would not do. `llvm` is whether this blaze
    /// was built with the llvm backend.
    pub fn of(env: &BTreeMap<String, String>, llvm: bool) -> Result<Settings, Refusal> {
        let bad = |name: &str, value: &str, expected: &str| {
            Refusal::with(
                Reason::EnvRaJitValue,
                format!("{name}={value}"),
                format!("expected {expected}"),
            )
        };
        let passes = match env.get(OPTIMIZATION).map(String::as_str) {
            Some("0") => 0,
            Some("1") => 1,
            Some("2") | None => 2,
            Some("3") => 3,
            Some(other) => return Err(bad(OPTIMIZATION, other, "0, 1, 2 or 3")),
        };
        // Code selection is optimized unless the passes are off, where it is fast, as the
        // backend decides it.
        let fast = match env.get(SELECTION).map(String::as_str) {
            Some("fast") => true,
            Some("optimized") | None => passes == 0,
            Some(other) => return Err(bad(SELECTION, other, "`fast` or `optimized`")),
        };
        let backend = match env.get(BACKEND).map(String::as_str) {
            Some("cranelift") | None => 0,
            Some("llvm") if llvm => 1,
            Some("llvm") => return Err(Refusal::of(Reason::EnvLlvm, format!("{BACKEND}=llvm"))),
            Some(other) => return Err(bad(BACKEND, other, "`cranelift` or `llvm`")),
        };
        let mir_opt = match env.get(MIR_OPT) {
            None => 0,
            Some(asked) => match asked.parse::<u8>() {
                Ok(level) if level <= 2 => level,
                _ => return Err(bad(MIR_OPT, asked, "0, 1 or 2")),
            },
        };
        let llvm = backend == 1;
        Ok(Settings {
            backend,
            backend_opt: if llvm { passes } else { 0 },
            backend_emit: if llvm { u8::from(fast) } else { 0 },
            mir_opt,
        })
    }
}

/// The [`ProfileSpec`] of `profile` with the `-C` flags `flags` over it and the backend
/// `settings`. `of` says which unit it is, for a refusal that names a setting of it.
pub fn spec(
    profile: &Profile,
    flags: &Overrides,
    settings: &Settings,
    of: &str,
) -> Result<ProfileSpec, Refusal> {
    let name = &profile.name;
    let setting = |key: &str, value: &str| format!("profile.{name}.{key} = {value}");
    let here = || format!("in the profile of {of}");
    match &profile.lto {
        Lto::Off | Lto::Bool(false) => {}
        Lto::Bool(true) => return Err(Refusal::with(Reason::Lto, setting("lto", "true"), here())),
        Lto::Named(named) => {
            return Err(Refusal::with(Reason::Lto, setting("lto", &format!("{named:?}")), here()));
        }
    }
    if let Some(backend) = &profile.codegen_backend {
        let what = setting("codegen-backend", &format!("{backend:?}"));
        return Err(Refusal::with(Reason::CodegenBackend, what, here()));
    }
    if !profile.rustflags.is_empty() {
        let what = setting("rustflags", &format!("{:?}", profile.rustflags));
        return Err(Refusal::with(Reason::ProfileRustflags, what, here()));
    }
    if let Some(trim) = &profile.trim_paths {
        let shown = serde_json::to_string(trim).unwrap_or_else(|_| format!("{trim:?}"));
        return Err(Refusal::with(Reason::TrimPaths, setting("trim-paths", &shown), here()));
    }
    if profile.rpath {
        return Err(Refusal::with(Reason::Rpath, setting("rpath", "true"), here()));
    }

    let opt_level = match flags.opt_level.as_deref().unwrap_or(&profile.opt_level) {
        level @ ("0" | "1" | "2" | "3") => level.as_bytes()[0] - b'0',
        level @ ("s" | "z") => {
            let what = setting("opt-level", &format!("{level:?}"));
            return Err(Refusal::with(Reason::OptLevelSize, what, here()));
        }
        other => {
            let what = setting("opt-level", &format!("{other:?}"));
            return Err(Refusal::with(Reason::ProfileValue, what, here()));
        }
    };

    // cargo names the checks to rustc only where they differ from what rustc has for the
    // level by itself, so a level among the flags moves the ones cargo left unnamed
    // (`plan.rs`, `Profile::with`).
    let unoptimized = profile.opt_level == "0";
    let named_assertions =
        (profile.debug_assertions != unoptimized).then_some(profile.debug_assertions);
    let named_overflow =
        (profile.overflow_checks != profile.debug_assertions).then_some(profile.overflow_checks);
    let unoptimized = flags.opt_level.as_ref().map_or(unoptimized, |it| it == "0");
    let debug_assertions = flags.debug_assertions.or(named_assertions).unwrap_or(unoptimized);
    let overflow_checks = flags.overflow_checks.or(named_overflow).unwrap_or(debug_assertions);

    let panic = match (&flags.panic, profile.panic) {
        (Some(strategy), _) => u8::from(strategy == "abort"),
        (None, PanicStrategy::Unwind) => 0,
        (None, PanicStrategy::Abort) => 1,
        (None, PanicStrategy::ImmediateAbort) => {
            return Err(Refusal::with(
                Reason::PanicImmediateAbort,
                setting("panic", "\"immediate-abort\""),
                here(),
            ));
        }
    };

    let debug = match flags.debug {
        Some(level) => level,
        None => match &profile.debuginfo {
            // The unit graph weakens a host unit's debug info to none unless it is shared with
            // the target's, which it finalizes.
            DebugInfo::Deferred(info) => match info {
                TomlDebugInfo::None => 0,
                TomlDebugInfo::Full => 2,
                _ => 1,
            },
            DebugInfo::Resolved(TomlDebugInfo::None) => 0,
            DebugInfo::Resolved(
                TomlDebugInfo::LineDirectivesOnly
                | TomlDebugInfo::LineTablesOnly
                | TomlDebugInfo::Limited,
            ) => 1,
            DebugInfo::Resolved(TomlDebugInfo::Full) => 2,
        },
    };

    let strip = match (&flags.strip, &profile.strip) {
        (Some(named), _) => named_strip(named),
        // A strip cargo deferred is its own choice for a build without debug info, which has
        // nothing the setting would take out.
        (None, Strip::Deferred(_) | Strip::Resolved(StripInner::None)) => Some(0),
        (None, Strip::Resolved(StripInner::Named(named))) => named_strip(named),
    };
    let Some(strip) = strip else {
        let what = setting("strip", &format!("{:?}", profile.strip.into_inner()));
        return Err(Refusal::with(Reason::ProfileValue, what, here()));
    };

    let split = flags.split_debuginfo.as_deref().or(profile.split_debuginfo.as_deref());
    let split_debuginfo = match split {
        None | Some("off") => 0,
        Some("packed") => 1,
        Some("unpacked") => 2,
        Some(other) => {
            let what = setting("split-debuginfo", &format!("{other:?}"));
            return Err(Refusal::with(Reason::ProfileValue, what, here()));
        }
    };

    Ok(ProfileSpec {
        name: name.clone(),
        opt_level,
        debug,
        debug_assertions,
        overflow_checks,
        panic,
        strip,
        split_debuginfo,
        backend: settings.backend,
        mir_opt: settings.mir_opt,
        backend_opt: settings.backend_opt,
        backend_emit: settings.backend_emit,
    })
}

fn named_strip(named: &str) -> Option<u8> {
    match named {
        "none" => Some(0),
        "debuginfo" => Some(1),
        "symbols" => Some(2),
        _ => None,
    }
}
