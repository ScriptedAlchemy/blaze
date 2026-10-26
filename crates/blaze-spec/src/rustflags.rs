//! The flags rustc is given for every unit, read the way blaze can honour them.
//!
//! Cargo takes them from the first of `CARGO_ENCODED_RUSTFLAGS`, `RUSTFLAGS`, the
//! `[target.<triple>]` and `[target.'cfg(..)']` tables, and `build.rustflags`, and gives them to
//! every unit, host units included, when nothing is built for another target
//! ([`blaze_manifest::Config::extra_args`]). A spec keeps the flags blaze honours, each in one
//! spelling (`-Copt-level=3`, `--cfg=a="b"`), in their order. Every flag is in one of three
//! classes, tabulated in [`CODEGEN`] and [`OTHER`] with the evidence for each: honoured (it
//! changes what blaze writes, and blaze reads it), ignored (it changes nothing blaze writes), or
//! refused by name (it changes what rustc writes or links, and blaze does not read it).
//!
//! The evidence is `crates/ra-jit/src/plan.rs` (`Flags::parse` is the one place blaze reads
//! `-C` options for a build), `crates/ra-jit/src/link.rs` (the link line is fixed, from
//! `#[link]` and the scripts' output only), `crates/hir-ty-codegen` (the target CPU is the
//! platform's; no option reaches the code generators but `RA_JIT_*`) and
//! `crates/ra-jit/src/check.rs` (`--cap-lints` and `--force-warn` decide whether lints the
//! source denies fail a check).

use blaze_manifest::Config;
use blaze_manifest::config::{CompileKind, Flags};

use crate::Refusal;
use crate::assemble::Host;
use crate::flags::Class;
use crate::refuse::Reason;

/// One flag or `-C` option, its class and why.
#[derive(Clone, Copy, Debug)]
pub struct Rule {
    /// The flag as rustc spells it (`--cfg`, `-D`), or the name of a `-C` option.
    pub flag: &'static str,
    /// How blaze reads it.
    pub class: Class,
    /// Why it is in its class: the evidence.
    pub because: &'static str,
}

const fn rule(flag: &'static str, class: Class, because: &'static str) -> Rule {
    Rule { flag, class, because }
}

const HONOURED: Class = Class::Honoured;
const IGNORED: Class = Class::Ignored;

/// The `-C` options blaze knows. Any other is refused as [`Reason::FlagCodegen`].
pub const CODEGEN: &[Rule] = &[
    rule("opt-level", HONOURED, "plan.rs `Flags::parse` reads it over the profile's level; `s` and `z` are refused"),
    rule("debug-assertions", HONOURED, "plan.rs reads it over the profile's, and it sets the cfg"),
    rule("overflow-checks", HONOURED, "plan.rs reads it over the profile's, and it sets the cfg"),
    rule("panic", HONOURED, "plan.rs reads `unwind` and `abort` over the profile's strategy, and it sets the cfg"),
    rule("debuginfo", HONOURED, "plan.rs reads it over the profile's level of debug info"),
    rule("split-debuginfo", HONOURED, "plan.rs reads it over the profile's, which says where debug info is kept"),
    rule("strip", HONOURED, "plan.rs reads it over the profile's, which says whether debug info is taken out"),
    rule("force-frame-pointers", HONOURED, "plan.rs keeps every function's frame pointer when it is on, and no profile field names it"),
    rule("codegen-units", IGNORED, "blaze compiles one function at a time; no option of it reads units of a crate"),
    rule("incremental", IGNORED, "blaze keeps its own state; plan.rs and the code generators read no such option"),
    rule("embed-bitcode", IGNORED, "blaze writes no bitcode into the libraries it builds"),
    rule("save-temps", IGNORED, "only keeps rustc's intermediate files, which blaze does not write"),
    rule("lto", Class::Refused(Reason::FlagLto), "plan.rs refuses `-C lto` as the whole program would be compiled again at link time"),
    rule("linker-plugin-lto", Class::Refused(Reason::FlagLto), "plan.rs refuses it as `-C lto`"),
    rule("target-cpu", Class::Refused(Reason::FlagCpu), "the code generators assume the platform's CPU (`ExecPlatform.cpu`) and read no such option"),
    rule("target-feature", Class::Refused(Reason::FlagCpu), "the code generators read no target feature flag"),
    rule("link-arg", Class::Refused(Reason::FlagLink), "link.rs builds the link line from `#[link]` and the scripts' output only"),
    rule("link-args", Class::Refused(Reason::FlagLink), "link.rs builds the link line from `#[link]` and the scripts' output only"),
    rule("linker", Class::Refused(Reason::FlagLink), "link.rs runs the system's `cc`"),
    rule("linker-flavor", Class::Refused(Reason::FlagLink), "link.rs runs the system's `cc`"),
    rule("link-self-contained", Class::Refused(Reason::FlagLink), "link.rs links against the system's libraries only"),
    rule("link-dead-code", Class::Refused(Reason::FlagLink), "link.rs always links with `--gc-sections` or `-dead_strip`"),
    rule("prefer-dynamic", Class::Refused(Reason::FlagLink), "link.rs links the standard library statically"),
    rule("rpath", Class::Refused(Reason::FlagLink), "link.rs adds no run-path"),
];

/// The flags of rustc outside `-C` that blaze knows. Any other that starts with `-` is refused
/// as [`Reason::FlagUnknown`], and a word that does not as [`Reason::FlagInput`].
pub const OTHER: &[Rule] = &[
    rule("--cfg", HONOURED, "it sets a cfg of every unit, and blaze compiles the source with it"),
    rule("--cap-lints", HONOURED, "check.rs lets the lints the source denies fail a check unless it is given"),
    rule("--force-warn", HONOURED, "plan.rs reads it as `--cap-lints` does"),
    rule("-A", IGNORED, "a lint level, which changes no code; blaze applies the levels the source gives"),
    rule("-W", IGNORED, "a lint level, which changes no code; blaze applies the levels the source gives"),
    rule("-D", IGNORED, "a lint level, which changes no code; blaze applies the levels the source gives"),
    rule("-F", IGNORED, "a lint level, which changes no code; blaze applies the levels the source gives"),
    rule("--allow", IGNORED, "a lint level, which changes no code"),
    rule("--warn", IGNORED, "a lint level, which changes no code"),
    rule("--deny", IGNORED, "a lint level, which changes no code"),
    rule("--forbid", IGNORED, "a lint level, which changes no code"),
    rule("--check-cfg", IGNORED, "only names the cfgs the `unexpected_cfgs` lint expects"),
    rule("--color", IGNORED, "changes only whether messages are in colour"),
    rule("--diagnostic-width", IGNORED, "changes only how wide messages are written"),
    rule("-L", Class::Refused(Reason::FlagLink), "link.rs searches only where a script's output says"),
    rule("-l", Class::Refused(Reason::FlagLink), "link.rs links only what `#[link]` and the scripts name"),
    rule("--remap-path-prefix", Class::Refused(Reason::FlagRemap), "blaze embeds the source paths it was given and remaps none"),
    rule("-Z", Class::Refused(Reason::FlagUnstable), "no unstable option reaches blaze's code generators"),
    rule("--target", Class::Refused(Reason::FlagCargoOwns), "blaze builds for its own host only"),
    rule("--crate-type", Class::Refused(Reason::FlagCargoOwns), "cargo names the crate types of each unit"),
    rule("--crate-name", Class::Refused(Reason::FlagCargoOwns), "cargo names each unit's crate"),
    rule("--emit", Class::Refused(Reason::FlagCargoOwns), "cargo names what each unit writes"),
    rule("--out-dir", Class::Refused(Reason::FlagCargoOwns), "cargo names where each unit writes"),
    rule("--edition", Class::Refused(Reason::FlagCargoOwns), "each unit has the edition of its target"),
    rule("--extern", Class::Refused(Reason::FlagCargoOwns), "the units' dependencies are the spec's"),
    rule("--sysroot", Class::Refused(Reason::FlagCargoOwns), "the standard library is the toolchain's"),
    rule("--print", Class::Refused(Reason::FlagCargoOwns), "it makes rustc print instead of compile"),
    rule("--error-format", Class::Refused(Reason::FlagCargoOwns), "cargo reads rustc's messages in the format it asks for"),
    rule("--json", Class::Refused(Reason::FlagCargoOwns), "cargo reads rustc's messages in the format it asks for"),
    rule("--test", Class::Refused(Reason::FlagCargoOwns), "cargo says which units are tests"),
];

fn rule_of<'a>(table: &'a [Rule], flag: &str) -> Option<&'a Rule> {
    table.iter().find(|it| it.flag == flag)
}

/// What the `-C` options of the flags say of a unit's profile, which win over it as rustc's
/// last flag does.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Overrides {
    /// `-C opt-level` and `-O`: `0` to `3`.
    pub opt_level: Option<String>,
    /// `-C debug-assertions`.
    pub debug_assertions: Option<bool>,
    /// `-C overflow-checks`.
    pub overflow_checks: Option<bool>,
    /// `-C panic`: `abort` or `unwind`.
    pub panic: Option<String>,
    /// `-C debuginfo` and `-g`: 0 none, 1 line tables, 2 full.
    pub debug: Option<u8>,
    /// `-C split-debuginfo`: `off`, `packed` or `unpacked`.
    pub split_debuginfo: Option<String>,
    /// `-C strip`: `none`, `debuginfo` or `symbols`.
    pub strip: Option<String>,
}

/// The flags blaze honours, and what they say.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Rustflags {
    /// The honoured flags, one word each, in the order they were given.
    pub flags: Vec<String>,
    /// What the `-C` options say of the profile.
    pub overrides: Overrides,
    /// The cfgs of `--cfg`, as rustc prints them: `name` or `name="value"`.
    pub cfgs: Vec<String>,
}

impl Rustflags {
    /// The `--cfg` flags among [`Rustflags::flags`]: the part of them a warm front end depends
    /// on.
    pub fn cfg_flags(&self) -> Vec<String> {
        self.flags.iter().filter(|it| it.starts_with("--cfg=")).cloned().collect()
    }
}

/// The words cargo gives rustc for a unit built for the host, by its precedence, in the
/// environment and configuration `config` has.
pub fn words(config: &Config, host: &Host) -> Result<Vec<String>, Refusal> {
    config
        .extra_args(&[CompileKind::Host], &host.triple, Some(&host.cfg), &CompileKind::Host, Flags::Rust)
        .map_err(|error| Refusal::with(Reason::ConfigRejected, "rustflags", format!("{error:#}")))
}

/// The words cargo gives rustdoc: what a doc test would run with.
pub fn doc_words(config: &Config, host: &Host) -> Result<Vec<String>, Refusal> {
    config
        .extra_args(&[CompileKind::Host], &host.triple, Some(&host.cfg), &CompileKind::Host, Flags::Rustdoc)
        .map_err(|error| Refusal::with(Reason::ConfigRejected, "rustdocflags", format!("{error:#}")))
}

/// What a flag of `words` is read as: a long flag by its `=` or a short one by its first two
/// characters, with the value attached to it.
fn split(word: &str) -> (&str, Option<&str>) {
    if word.starts_with("--") {
        return match word.split_once('=') {
            Some((name, value)) => (name, Some(value)),
            None => (word, None),
        };
    }
    match word.char_indices().nth(2) {
        Some((at, _)) if word.starts_with('-') => (&word[..at], Some(&word[at..])),
        _ => (word, None),
    }
}

/// Reads the words, refusing the first flag blaze cannot honour.
pub fn read(words: &[String]) -> Result<Rustflags, Refusal> {
    let mut out = Rustflags::default();
    let mut words = words.iter();
    while let Some(word) = words.next() {
        let (name, attached) = split(word);
        let name = match word.as_str() {
            "-O" => {
                codegen(&mut out, "opt-level=3", word)?;
                continue;
            }
            "-g" => {
                codegen(&mut out, "debuginfo=2", word)?;
                continue;
            }
            _ => name,
        };
        let value = |words: &mut std::slice::Iter<'_, String>| match attached {
            Some(value) => Ok(value.to_owned()),
            None => words.next().cloned().ok_or_else(|| {
                Refusal::with(Reason::FlagValue, word.clone(), "the flag needs a value")
            }),
        };
        match name {
            "-C" | "--codegen" => {
                let option = value(&mut words)?;
                codegen(&mut out, &option, word)?;
            }
            "--cfg" => {
                let spec = value(&mut words)?;
                let cfg = cfg(&spec, word)?;
                out.flags.push(format!("--cfg={cfg}"));
                out.cfgs.push(cfg);
            }
            "--cap-lints" => {
                let level = value(&mut words)?;
                if !matches!(level.as_str(), "allow" | "warn" | "deny" | "forbid") {
                    return Err(Refusal::with(Reason::FlagValue, word.clone(), "no such lint level"));
                }
                out.flags.push(format!("--cap-lints={level}"));
            }
            "--force-warn" => {
                let lint = value(&mut words)?;
                out.flags.push(format!("--force-warn={lint}"));
            }
            _ if name.starts_with('-') => match rule_of(OTHER, name) {
                Some(rule) => match rule.class {
                    Class::Refused(reason) => return Err(Refusal::of(reason, word.clone())),
                    // A lint level or a message setting: its value is the next word unless attached.
                    _ => {
                        value(&mut words)?;
                    }
                },
                None => return Err(Refusal::of(Reason::FlagUnknown, word.clone())),
            },
            _ => return Err(Refusal::of(Reason::FlagInput, word.clone())),
        }
    }
    Ok(out)
}

/// Reads the `-C` option `option` (`name=value`), which `word` was, into `out`.
fn codegen(out: &mut Rustflags, option: &str, word: &str) -> Result<(), Refusal> {
    let (name, value) = match option.split_once('=') {
        Some((name, value)) => (name, Some(value)),
        None => (option, None),
    };
    let name = name.replace('_', "-");
    let Some(rule) = rule_of(CODEGEN, &name) else {
        return Err(Refusal::of(Reason::FlagCodegen, format!("-C {option}")));
    };
    if let Class::Refused(reason) = rule.class {
        // `-C lto=off` is the absence of the flag.
        if reason == Reason::FlagLto && matches!(value, Some("off" | "no" | "n" | "false")) {
            return Ok(());
        }
        return Err(Refusal::of(reason, format!("-C {option}")));
    }
    if rule.class == IGNORED {
        return Ok(());
    }
    let bad = |why: &str| Refusal::with(Reason::FlagValue, format!("-C {option}"), why);
    let on = |value: Option<&str>| match value {
        None | Some("y" | "yes" | "on" | "true" | "") => Some(true),
        Some("n" | "no" | "off" | "false") => Some(false),
        Some(_) => None,
    };
    let normal = match name.as_str() {
        "opt-level" => match value {
            Some(level @ ("s" | "z")) => {
                return Err(Refusal::of(Reason::FlagOptSize, format!("-C opt-level={level}")));
            }
            Some(level @ ("0" | "1" | "2" | "3")) => {
                out.overrides.opt_level = Some(level.to_owned());
                format!("opt-level={level}")
            }
            _ => return Err(bad("expected 0, 1, 2 or 3")),
        },
        "debug-assertions" | "overflow-checks" => {
            let Some(on) = on(value) else { return Err(bad("expected yes or no")) };
            if name == "debug-assertions" {
                out.overrides.debug_assertions = Some(on);
            } else {
                out.overrides.overflow_checks = Some(on);
            }
            format!("{name}={}", if on { "yes" } else { "no" })
        }
        "panic" => match value {
            Some(strategy @ ("unwind" | "abort")) => {
                out.overrides.panic = Some(strategy.to_owned());
                format!("panic={strategy}")
            }
            _ => return Err(bad("expected unwind or abort")),
        },
        "debuginfo" => {
            let level = match value {
                Some("0" | "none") => 0,
                Some("1" | "limited" | "line-tables-only" | "line-directives-only") => 1,
                Some("2" | "full") => 2,
                _ => return Err(bad("expected 0, 1, 2, none, limited, line-tables-only, line-directives-only or full")),
            };
            out.overrides.debug = Some(level);
            format!("debuginfo={}", value.unwrap_or_default())
        }
        "split-debuginfo" => match value {
            Some(split @ ("off" | "packed" | "unpacked")) => {
                out.overrides.split_debuginfo = Some(split.to_owned());
                format!("split-debuginfo={split}")
            }
            _ => return Err(bad("expected off, packed or unpacked")),
        },
        "strip" => match value {
            Some(strip @ ("none" | "debuginfo" | "symbols")) => {
                out.overrides.strip = Some(strip.to_owned());
                format!("strip={strip}")
            }
            _ => return Err(bad("expected none, debuginfo or symbols")),
        },
        "force-frame-pointers" => match value {
            Some("always" | "non-leaf" | "may-omit") => format!("{name}={}", value.unwrap_or_default()),
            other => match on(other) {
                Some(on) => format!("{name}={}", if on { "yes" } else { "no" }),
                None => return Err(bad("expected yes, no, always, non-leaf or may-omit")),
            },
        },
        other => unreachable!("the honoured `-C` option `{other}` has no arm (read as `{word}`)"),
    };
    out.flags.push(format!("-C{normal}"));
    Ok(())
}

/// The cfgs rustc sets itself, which it refuses as `--cfg`, by name or by what it starts with.
const BUILTIN_CFGS: &[&str] = &[
    "debug_assertions",
    "overflow_checks",
    "ub_checks",
    "contract_checks",
    "fmt_debug",
    "relocation_model",
    "panic",
    "unix",
    "windows",
    "test",
    "emscripten_wasm_eh",
    "sanitize",
    "sanitizer_",
];

/// A `--cfg` value as rustc prints the cfg back: `name`, or `name="value"` with the value as
/// written between its quotes. rustc refuses what is neither, and the cfgs it sets itself.
fn cfg(spec: &str, word: &str) -> Result<String, Refusal> {
    let bad = |why: &str| Refusal::with(Reason::FlagCfg, format!("--cfg {spec}"), why);
    let is_name = |name: &str| {
        name.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_')
            && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
    };
    let (name, value) = match spec.split_once('=') {
        Some((name, value)) => (name.trim(), Some(value.trim())),
        None => (spec.trim(), None),
    };
    if !is_name(name) {
        return Err(bad(&format!("`{name}` is not a name (read from `{word}`)")));
    }
    let builtin = name.starts_with("target_")
        || BUILTIN_CFGS.iter().any(|it| name == *it || (it.ends_with('_') && name.starts_with(it)));
    if builtin {
        return Err(bad("rustc sets it itself and refuses it as a flag"));
    }
    match value {
        None => Ok(name.to_owned()),
        Some(value) => match value.strip_prefix('"').and_then(|it| it.strip_suffix('"')) {
            Some(inner) if !inner.contains('"') => Ok(format!("{name}=\"{inner}\"")),
            _ => Err(bad("the value is not a string in quotes with no quote inside")),
        },
    }
}
