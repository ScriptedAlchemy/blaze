//! An original invocation paired with a saved spec, outside build and job identity.
//!
//! Format 1 is a deterministic CBOR map with all six integer keys required. Key 0 is version 1,
//! 1 the spec digest, 2 the command (check=0, build=1, test=2, run=3), 3 the ordered original
//! words, 4 environment pairs sorted by name, and 5 config-file bindings sorted by word index.
//! A binding is a required-key map of 0 to its word index and 1 to its source-relative path.
//! It does not capture config discovery, includes or external source roots. The caller must
//! supply those inputs, re-resolve and compare the resulting spec before executing anything.

use std::{
    collections::BTreeMap,
    ffi::OsString,
    path::{Path, PathBuf},
};

use blaze_wire::{
    BuildSpec, ConfigEnvValue, Digest, Encoder, Error, ErrorKind, InputText, Limits, Named, Reader,
    SourcePath, Wire,
};

use crate::{
    Refusal,
    flags::{FLAGS, Request, Subcommand, Takes},
};

/// A CLI config file already captured in the source image.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConfigFile {
    /// Index of its `--config` or `--config=...` word, counting after the command.
    pub argument: u32,
    /// Source-root-relative file location, supplied by the input capturer.
    pub path: SourcePath,
}

impl Wire for ConfigFile {
    fn encode(&self, e: &mut Encoder) {
        e.map(2);
        e.uint(0);
        self.argument.encode(e);
        e.uint(1);
        self.path.encode(e);
    }

    fn decode(r: &mut Reader<'_>) -> Result<Self, Error> {
        r.map_start()?;
        r.field(0)?;
        let argument = u32::decode(r)?;
        r.field(1)?;
        let path = SourcePath::decode(r)?;
        r.map_end()?;
        Ok(Self { argument, path })
    }
}

/// A validated invocation witness. Its digest must never enter `BuildSpec` or `JobContent`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReplayRecipe {
    spec_digest: Digest,
    command: Subcommand,
    words: Vec<InputText>,
    environment: Vec<(String, ConfigEnvValue)>,
    config_files: Vec<ConfigFile>,
}

/// Native inputs for a child started with `env_clear`, before verification or compilation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Invocation {
    /// The original command.
    pub command: Subcommand,
    /// Original words with manifest and captured config-file operands relocated.
    pub args: Vec<OsString>,
    /// Saved workspace manifest located in the materialized source.
    pub manifest: PathBuf,
    /// Saved workspace-relative working directory located in the materialized source.
    pub cwd: PathBuf,
    /// An environment built from executor service values and captured resolution values.
    pub env: BTreeMap<String, OsString>,
}

/// Exact executor variables admitted from ambient state.
///
/// `docs/cargo.md` documents the memory, home and cache controls; `environment::RA_JIT_NEUTRAL`
/// documents the watchdog label. PATH, HOME, temporary directories, Rustup locations and Cargo
/// home/target cache locations belong to the executor. No compiler/config prefix is admitted.
/// The caller must supply isolated config discovery separately; HOME alone is not isolation.
pub const EXECUTOR_ENV: &[&str] = &[
    "PATH",
    "HOME",
    "TMPDIR",
    "CARGO_HOME",
    "CARGO_TARGET_DIR",
    "RUSTUP_HOME",
    "RUSTUP_TOOLCHAIN",
    "RA_JIT_HOME",
    "RA_JIT_CACHE",
    "RA_JIT_CACHE_MAX",
    "RA_JIT_MEMORY_MAX",
    "RA_JIT_WORKER_MEMORY",
    "RA_JIT_MEMORY_RESERVE",
    "RA_JIT_PRESSURE_GRACE_SECS",
    "RA_JIT_OWN_BASE_MEMORY",
    "RA_JIT_SPARE",
    "RA_JIT_RUN_ID",
];

impl ReplayRecipe {
    /// Capture the original UTF-8 CLI words and native resolution environment for `spec`.
    ///
    /// Flag syntax is validated by `Request::parse`. File `--config` operands must have explicit
    /// captured bindings, including filenames containing `=`. Other config operands must contain
    /// an inline assignment. TOML semantics and file contents are the resolver's responsibility.
    /// Compiler/toolchain overrides are refused; unrelated requester environment is omitted.
    /// Resolution values are non-transport `CARGO_*`, rustflags, the carried backend controls
    /// and `RA_JIT_SPLICE`. Spec-declared script/config environment is restored by the caller.
    /// Default wire budgets apply. This neither resolves the command nor proves spec equality.
    pub fn capture(
        spec: &BuildSpec,
        command: Subcommand,
        words: &[String],
        env: &BTreeMap<String, OsString>,
        config_files: &[ConfigFile],
    ) -> Result<Self, Refusal> {
        if command != command_of(spec) {
            return Err(Refusal::new("recipe command", "differs from the saved spec"));
        }
        let mut environment = Vec::new();
        for (name, value) in env {
            if forbidden(name) {
                return Err(Refusal::new(
                    name,
                    "compiler/toolchain overrides cannot enter a recipe",
                ));
            }
            if resolution(name) {
                environment.push((name.clone(), ConfigEnvValue::from_native(value)));
            }
        }
        let mut config_files = config_files.to_vec();
        config_files.sort_by_key(|file| file.argument);
        let recipe = Self {
            spec_digest: blaze_wire::digest_of(spec),
            command,
            words: words.iter().cloned().map(InputText).collect(),
            environment,
            config_files,
        };
        recipe.check()?;
        Self::decode(&blaze_wire::encode(&recipe), &Limits::default())
            .map_err(|error| Refusal::new("recipe", error.to_string()))
    }

    /// Decode canonical format 1 CBOR, rejecting missing/unknown fields and budget violations.
    pub fn decode(bytes: &[u8], limits: &Limits) -> Result<Self, Error> {
        blaze_wire::decode(bytes, limits)
    }

    /// The saved BuildSpec digest this witness is paired with, independent of its own digest.
    pub fn spec_digest(&self) -> Digest {
        self.spec_digest
    }

    /// The exact words supplied at capture, including original path spellings and `--` tails.
    pub fn original_words(&self) -> impl ExactSizeIterator<Item = &str> {
        self.words.iter().map(|word| word.0.as_str())
    }

    /// Materialize native inputs for the paired spec without inheriting ambient compiler state.
    ///
    /// Every original manifest operand is removed and one saved manifest is prepended. Bound
    /// config-file operands are replaced in place with native source-image paths. Other words
    /// retain their order. Literal/native environment tags are preserved without path guessing.
    /// The caller must use `env_clear`, isolate config sources, restore any spec-declared job
    /// environment and verify resolution before passing these inputs to an owned executor.
    pub fn materialize(
        &self,
        spec: &BuildSpec,
        source_root: &Path,
        ambient: &BTreeMap<String, OsString>,
    ) -> Result<Invocation, Refusal> {
        if self.spec_digest != blaze_wire::digest_of(spec) {
            return Err(Refusal::new("recipe spec", "digest differs from the saved BuildSpec"));
        }
        if self.command != command_of(spec) {
            return Err(Refusal::new("recipe command", "differs from the saved spec"));
        }
        let (manifest, cwd) = crate::run::locations(spec, source_root)?;
        let words: Vec<String> = self.words.iter().map(|word| word.0.clone()).collect();
        let operands = operands(self.command, &words, &self.config_files)?;
        let mut args = vec![OsString::from("--manifest-path"), manifest.clone().into_os_string()];
        let mut at = 0;
        for operand in operands {
            args.extend(words[at..operand.start].iter().map(OsString::from));
            if let Some(path) = operand.config {
                args.push("--config".into());
                args.push(
                    source_root.join(path.to_native().map_err(wire_refusal)?).into_os_string(),
                );
            }
            at = operand.end;
        }
        args.extend(words[at..].iter().map(OsString::from));
        let mut env = BTreeMap::new();
        for name in EXECUTOR_ENV {
            if let Some(value) = ambient.get(*name) {
                let value = ConfigEnvValue::from_native(value)
                    .materialize(Path::new("."))
                    .map_err(wire_refusal)?;
                env.insert((*name).to_owned(), value);
            }
        }
        for (name, value) in &self.environment {
            env.insert(name.clone(), value.materialize(Path::new(".")).map_err(wire_refusal)?);
        }
        Ok(Invocation { command: self.command, args, manifest, cwd, env })
    }

    fn check(&self) -> Result<(), Refusal> {
        let words: Vec<String> = self.words.iter().map(|word| word.0.clone()).collect();
        if words.iter().any(|word| word.contains('\0')) {
            return Err(Refusal::new("recipe words", "contain NUL"));
        }
        operands(self.command, &words, &self.config_files)?;
        let mut previous: Option<&str> = None;
        for (name, value) in &self.environment {
            if name.is_empty()
                || name.contains(['=', '\0'])
                || previous.is_some_and(|previous| previous >= name.as_str())
                || !resolution(name)
                || matches!(value, ConfigEnvValue::WorkspaceRelative(_))
            {
                return Err(Refusal::new("recipe environment", "invalid name, order or value tag"));
            }
            value.materialize(Path::new(".")).map_err(wire_refusal)?;
            previous = Some(name);
        }
        Ok(())
    }
}

impl Named for ReplayRecipe {
    const NAME: &'static str = "ReplayRecipe";
}

impl Wire for ReplayRecipe {
    fn encode(&self, e: &mut Encoder) {
        e.map(6);
        e.uint(0);
        e.uint(1);
        e.uint(1);
        self.spec_digest.encode(e);
        e.uint(2);
        e.uint(match self.command {
            Subcommand::Check => 0,
            Subcommand::Build => 1,
            Subcommand::Test => 2,
            Subcommand::Run => 3,
        });
        e.uint(3);
        self.words.encode(e);
        e.uint(4);
        self.environment.encode(e);
        e.uint(5);
        self.config_files.encode(e);
    }

    fn decode(r: &mut Reader<'_>) -> Result<Self, Error> {
        r.map_start()?;
        r.field(0)?;
        let at = r.position();
        if u32::decode(r)? != 1 {
            return Err(invalid(at, "ReplayRecipe format 1"));
        }
        r.field(1)?;
        let spec_digest = Digest::decode(r)?;
        r.field(2)?;
        let command = match u8::decode(r)? {
            0 => Subcommand::Check,
            1 => Subcommand::Build,
            2 => Subcommand::Test,
            3 => Subcommand::Run,
            _ => return Err(invalid(r.position(), "a replay command")),
        };
        r.field(3)?;
        let words = Vec::<InputText>::decode(r)?;
        r.field(4)?;
        let environment = Vec::<(String, ConfigEnvValue)>::decode(r)?;
        r.field(5)?;
        let config_files = Vec::<ConfigFile>::decode(r)?;
        r.map_end()?;
        let recipe = Self { spec_digest, command, words, environment, config_files };
        recipe.check().map_err(|_| invalid(at, "a valid replay recipe"))?;
        Ok(recipe)
    }
}

fn invalid(at: usize, expected: &'static str) -> Error {
    Error { kind: ErrorKind::UnexpectedType { expected }, at }
}

fn wire_refusal(error: Error) -> Refusal {
    Refusal::new("recipe", error.to_string())
}

fn command_of(spec: &BuildSpec) -> Subcommand {
    match &spec.command {
        blaze_wire::Command::Check => Subcommand::Check,
        blaze_wire::Command::Build => Subcommand::Build,
        blaze_wire::Command::Run(_) => Subcommand::Run,
        blaze_wire::Command::Test(_) => Subcommand::Test,
    }
}

fn forbidden(name: &str) -> bool {
    crate::environment::REFUSED.iter().any(|(entry, _)| name == *entry)
        || matches!(
            name,
            "CARGO_BUILD_RUSTC"
                | "CARGO_BUILD_RUSTC_WRAPPER"
                | "CARGO_BUILD_RUSTC_WORKSPACE_WRAPPER"
                | "CARGO_BUILD_RUSTDOC"
        )
}

fn resolution(name: &str) -> bool {
    if forbidden(name) || EXECUTOR_ENV.contains(&name) || crate::environment::cargo_ignored(name) {
        return false;
    }
    (name.starts_with("CARGO_")
        && !name.ends_with("_TOKEN")
        && !name.ends_with("_CREDENTIAL_PROVIDER")
        && !name.ends_with("_SECRET_KEY"))
        || matches!(name, "RUSTFLAGS" | "RUSTDOCFLAGS" | "RA_JIT_SPLICE")
        || crate::environment::RA_JIT_CARRIED.contains(&name)
}

struct Operand<'a> {
    start: usize,
    end: usize,
    config: Option<&'a SourcePath>,
}

fn operands<'a>(
    command: Subcommand,
    words: &[String],
    files: &'a [ConfigFile],
) -> Result<Vec<Operand<'a>>, Refusal> {
    Request::parse(command, words)?;
    if files.windows(2).any(|pair| pair[0].argument >= pair[1].argument)
        || files.iter().any(|file| file.path.as_str() == ".")
    {
        return Err(Refusal::new(
            "recipe config files",
            "duplicate, unsorted or directory binding",
        ));
    }
    let mut out = Vec::new();
    let (mut at, mut used) = (0, 0);
    while at < words.len() && words[at] != "--" {
        let word = &words[at];
        let mut end = at + 1;
        if let Some(long) = word.strip_prefix("--") {
            let (name, attached) = long.split_once('=').map_or((long, None), |(n, v)| (n, Some(v)));
            if let Some(flag) = FLAGS.iter().find(|flag| flag.long == name) {
                if flag.takes == Takes::Value && attached.is_none() {
                    end += 1;
                }
                if matches!(name, "manifest-path" | "config") {
                    let value = attached.unwrap_or_else(|| words[at + 1].as_str());
                    if value.is_empty() {
                        return Err(Refusal::new(word, "empty path/config operand"));
                    }
                    if name == "manifest-path" {
                        out.push(Operand { start: at, end, config: None });
                    } else if let Some(file) =
                        files.get(used).filter(|file| file.argument as usize == at)
                    {
                        out.push(Operand { start: at, end, config: Some(&file.path) });
                        used += 1;
                    } else if !value.contains('=') {
                        return Err(Refusal::new(
                            word,
                            "file --config requires a captured ConfigFile binding",
                        ));
                    }
                }
            }
        } else if word.starts_with('-') {
            for (offset, short) in word[1..].char_indices() {
                if let Some(flag) = FLAGS.iter().find(|flag| flag.short == Some(short))
                    && flag.takes != Takes::Nothing
                {
                    if flag.takes == Takes::Value && 1 + offset + short.len_utf8() == word.len() {
                        end += 1;
                    }
                    break;
                }
            }
        }
        at = end;
    }
    if used != files.len() {
        return Err(Refusal::new(
            "recipe config files",
            "binding does not name a --config operand",
        ));
    }
    Ok(out)
}
