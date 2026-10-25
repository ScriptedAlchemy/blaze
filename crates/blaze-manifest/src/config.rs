//! `.cargo/config.toml` as cargo 1.99 reads it: the files from the current directory up and in
//! `$CARGO_HOME`, their `include`s, `--config` arguments and `CARGO_*` variables, merged by
//! cargo's rules (`src/context/mod.rs` and `config_value.rs` there).

use std::cell::OnceCell;
use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, anyhow, bail};
use rustc_hash::{FxHashMap, FxHashSet};
use url::Url;

use crate::paths;

/// Where a value was defined, which decides its priority and what a relative path in it is
/// relative to.
#[derive(Clone, Debug)]
pub enum Definition {
    BuiltIn,
    /// A config file, by its path.
    Path(PathBuf),
    /// An environment variable, by its name.
    Environment(String),
    /// `--config`, with the file's path when the argument named a file.
    Cli(Option<PathBuf>),
}

impl Definition {
    fn rank(&self) -> u8 {
        match self {
            Definition::BuiltIn => 0,
            Definition::Path(_) => 1,
            Definition::Environment(_) => 2,
            Definition::Cli(_) => 3,
        }
    }

    pub fn is_higher_priority(&self, other: &Definition) -> bool {
        self.rank() > other.rank()
    }

    /// The directory a relative path defined here is relative to: the one above `.cargo/` for a
    /// file, the current directory otherwise.
    pub fn root<'a>(&'a self, cwd: &'a Path) -> &'a Path {
        match self {
            Definition::Path(p) | Definition::Cli(Some(p)) => p.parent().unwrap().parent().unwrap(),
            Definition::Environment(_) | Definition::Cli(None) | Definition::BuiltIn => cwd,
        }
    }
}

impl fmt::Display for Definition {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Definition::Path(p) | Definition::Cli(Some(p)) => p.display().fmt(f),
            Definition::Environment(key) => write!(f, "environment variable `{key}`"),
            Definition::Cli(None) => write!(f, "--config cli option"),
            Definition::BuiltIn => write!(f, "default"),
        }
    }
}

/// A value and where it came from.
#[derive(Clone, Debug)]
pub struct Value<T> {
    pub val: T,
    pub definition: Definition,
}

/// A config value as cargo keeps it: TOML's types less floats and dates, each with its
/// definition.
#[derive(Clone, Debug)]
pub enum ConfigValue {
    Integer(i64, Definition),
    String(String, Definition),
    List(Vec<ConfigValue>, Definition),
    Table(FxHashMap<String, ConfigValue>, Definition),
    Boolean(bool, Definition),
}

use ConfigValue as CV;

/// Lists that replace rather than append when merged.
const NON_MERGEABLE_LISTS: &[&str] = &[
    "credential-alias.*",
    "doc.browser",
    "host.runner",
    "registries.*.credential-provider",
    "registry.credential-provider",
    "target.*.runner",
];

fn key_matches(parts: &[String], pattern: &str) -> bool {
    let mut parts = parts.iter();
    pattern.split('.').all(|pat| parts.next().is_some_and(|p| p == pat) || pat == "*")
        && parts.next().is_none()
}

fn is_nonmergeable_list(parts: &[String]) -> bool {
    NON_MERGEABLE_LISTS.iter().any(|l| key_matches(parts, l))
}

impl ConfigValue {
    fn from_toml(def: Definition, toml: toml::Value) -> anyhow::Result<ConfigValue> {
        let mut path = Vec::new();
        Self::from_toml_inner(def, toml, &mut path).with_context(|| {
            let mut key_path = String::new();
            let mut it = path.iter().rev().peekable();
            while let Some(k) = it.next() {
                match k {
                    KeyOrIdx::Key(s) => key_path.push_str(&escape_key_part(s)),
                    KeyOrIdx::Idx(i) => key_path.push_str(&format!("[{i}]")),
                }
                if matches!(it.peek(), Some(KeyOrIdx::Key(_))) {
                    key_path.push('.');
                }
            }
            format!("failed to parse config at `{key_path}`")
        })
    }

    fn from_toml_inner(
        def: Definition,
        toml: toml::Value,
        path: &mut Vec<KeyOrIdx>,
    ) -> anyhow::Result<ConfigValue> {
        match toml {
            toml::Value::String(val) => Ok(CV::String(val, def)),
            toml::Value::Boolean(b) => Ok(CV::Boolean(b, def)),
            toml::Value::Integer(i) => Ok(CV::Integer(i, def)),
            toml::Value::Array(val) => Ok(CV::List(
                val.into_iter()
                    .enumerate()
                    .map(|(i, toml)| {
                        CV::from_toml_inner(def.clone(), toml, path)
                            .inspect_err(|_| path.push(KeyOrIdx::Idx(i)))
                    })
                    .collect::<anyhow::Result<_>>()?,
                def,
            )),
            toml::Value::Table(val) => Ok(CV::Table(
                val.into_iter()
                    .map(|(key, value)| match CV::from_toml_inner(def.clone(), value, path) {
                        Ok(value) => Ok((key, value)),
                        Err(e) => {
                            path.push(KeyOrIdx::Key(key));
                            Err(e)
                        }
                    })
                    .collect::<anyhow::Result<_>>()?,
                def,
            )),
            v => bail!("unsupported TOML configuration type `{}`", v.type_str()),
        }
    }

    pub fn definition(&self) -> &Definition {
        match self {
            CV::Boolean(_, def)
            | CV::Integer(_, def)
            | CV::String(_, def)
            | CV::List(_, def)
            | CV::Table(_, def) => def,
        }
    }

    pub fn desc(&self) -> &'static str {
        match self {
            CV::Table(..) => "table",
            CV::List(..) => "array",
            CV::String(..) => "string",
            CV::Boolean(..) => "boolean",
            CV::Integer(..) => "integer",
        }
    }

    /// Merges `from` into `self`. With `force`, a value of the same priority replaces the old
    /// one and a list of the same priority goes after it.
    fn merge(&mut self, from: ConfigValue, force: bool) -> anyhow::Result<()> {
        self.merge_helper(from, force, &mut Vec::new())
    }

    fn merge_helper(
        &mut self,
        from: ConfigValue,
        force: bool,
        parts: &mut Vec<String>,
    ) -> anyhow::Result<()> {
        let is_higher_priority = from.definition().is_higher_priority(self.definition());
        match (self, from) {
            (CV::List(old, _), CV::List(mut new, _)) => {
                if is_nonmergeable_list(parts) {
                    if force || is_higher_priority {
                        std::mem::swap(&mut new, old);
                    }
                } else if force {
                    old.append(&mut new);
                } else {
                    new.append(old);
                    std::mem::swap(&mut new, old);
                }
                old.sort_by_key(|v| v.definition().rank());
            }
            (CV::Table(old, _), CV::Table(new, _)) => {
                for (key, value) in new {
                    match old.get_mut(&key) {
                        Some(entry) => {
                            let new_def = value.definition().clone();
                            parts.push(key.clone());
                            let result = entry.merge_helper(value, force, parts);
                            parts.pop();
                            result.with_context(|| {
                                format!(
                                    "failed to merge key `{key}` between {} and {new_def}",
                                    entry.definition(),
                                )
                            })?;
                        }
                        None => {
                            old.insert(key, value);
                        }
                    }
                }
            }
            (expected @ CV::List(..), found)
            | (expected @ CV::Table(..), found)
            | (expected, found @ CV::List(..))
            | (expected, found @ CV::Table(..)) => {
                return Err(anyhow!(
                    "failed to merge config value from `{}` into `{}`: expected {}, but found {}",
                    found.definition(),
                    expected.definition(),
                    expected.desc(),
                    found.desc()
                ));
            }
            (old, mut new) => {
                if force || new.definition().is_higher_priority(old.definition()) {
                    std::mem::swap(old, &mut new);
                }
            }
        }
        Ok(())
    }
}

enum KeyOrIdx {
    Key(String),
    Idx(usize),
}

fn escape_key_part(part: &str) -> std::borrow::Cow<'_, str> {
    let ok = !part.is_empty()
        && part.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'));
    if ok { part.into() } else { format!("{part:?}").into() }
}

/// The environment variable cargo reads for a key: `CARGO_` and the parts upper-cased, with
/// `-` and `.` as `_`.
pub fn env_key(key: &str) -> String {
    let mut env = String::from("CARGO");
    for part in key.split('.') {
        env.push('_');
        env.push_str(&part.replace('-', "_").to_uppercase());
    }
    env
}

/// The configuration of one invocation: what cargo's `GlobalContext` holds of it.
#[derive(Debug)]
pub struct Config {
    cwd: PathBuf,
    home: PathBuf,
    env: BTreeMap<String, String>,
    /// The files are read at the first value asked for, and again at the next while they fail:
    /// a broken config file is an error only to what reads configuration, as in cargo.
    values: OnceCell<FxHashMap<String, ConfigValue>>,
}

impl Config {
    /// The configuration seen from `cwd` with the variables of `env` and the `--config`
    /// arguments `cli`, as `cargo` started there would. `--config` reads the files at once.
    pub fn load(
        cwd: &Path,
        env: impl IntoIterator<Item = (String, String)>,
        cli: &[String],
    ) -> anyhow::Result<Config> {
        let env: BTreeMap<String, String> = env.into_iter().collect();
        let home = cargo_home(cwd, &env)?;
        let mut config = Config { cwd: cwd.to_path_buf(), home, env, values: OnceCell::new() };
        if !cli.is_empty() {
            config.merge_cli_args(cli)?;
        }
        Ok(config)
    }

    fn values(&self) -> anyhow::Result<&FxHashMap<String, ConfigValue>> {
        if let Some(values) = self.values.get() {
            return Ok(values);
        }
        let values = self.load_values()?;
        Ok(self.values.get_or_init(|| values))
    }

    /// The configuration this process sees: its current directory and environment.
    pub fn from_process(cli: &[String]) -> anyhow::Result<Config> {
        let cwd =
            std::env::current_dir().context("couldn't get the current directory of the process")?;
        Config::load(&cwd, std::env::vars(), cli)
    }

    pub fn cwd(&self) -> &Path {
        &self.cwd
    }

    /// `$CARGO_HOME`, or `~/.cargo`.
    pub fn home(&self) -> &Path {
        &self.home
    }

    pub fn env(&self, key: &str) -> Option<&str> {
        self.env.get(key).map(String::as_str)
    }

    fn load_values(&self) -> anyhow::Result<FxHashMap<String, ConfigValue>> {
        let mut cfg = CV::Table(FxHashMap::default(), Definition::BuiltIn);
        self.walk_tree(&self.cwd, &self.home, |path| {
            let value = self.load_file(path, &mut FxHashSet::default(), true, false)?;
            cfg.merge(value, false)
                .with_context(|| format!("failed to merge configuration at `{}`", path.display()))
        })
        .context("could not load Cargo configuration")?;
        match cfg {
            CV::Table(map, _) => Ok(map),
            _ => unreachable!(),
        }
    }

    /// The config files cargo reads, nearest first: `.cargo/config` or `.cargo/config.toml` in
    /// `pwd` and each directory above it, then the one in `home` unless that was one of them.
    fn walk_tree(
        &self,
        pwd: &Path,
        home: &Path,
        mut walk: impl FnMut(&Path) -> anyhow::Result<()>,
    ) -> anyhow::Result<()> {
        let mut seen_dir = FxHashSet::default();
        let stop = self.env("__CARGO_TEST_ROOT").map(PathBuf::from);
        for current in paths::ancestors(pwd, stop.as_deref()) {
            let config_root = current.join(".cargo");
            if let Some(path) = get_file_path(&config_root, "config") {
                walk(&path)?;
            }
            let canonical_root = config_root.canonicalize().unwrap_or(config_root);
            seen_dir.insert(canonical_root);
        }
        let canonical_home = home.canonicalize().unwrap_or(home.to_path_buf());
        if !seen_dir.contains(&canonical_home)
            && !seen_dir.contains(home)
            && let Some(path) = get_file_path(home, "config")
        {
            walk(&path)?;
        }
        Ok(())
    }

    fn load_file(
        &self,
        path: &Path,
        seen: &mut FxHashSet<PathBuf>,
        includes: bool,
        cli: bool,
    ) -> anyhow::Result<ConfigValue> {
        if !seen.insert(path.to_path_buf()) {
            bail!("config `include` cycle detected with path `{}`", path.display());
        }
        let contents = std::fs::read_to_string(path)
            .with_context(|| format!("failed to read configuration file `{}`", path.display()))?;
        let toml: toml::Table = contents.parse().with_context(|| {
            format!("could not parse TOML configuration in `{}`", path.display())
        })?;
        let def =
            if cli { Definition::Cli(Some(path.into())) } else { Definition::Path(path.into()) };
        let value = CV::from_toml(def, toml::Value::Table(toml)).with_context(|| {
            format!("failed to load TOML configuration from `{}`", path.display())
        })?;
        if includes { self.load_includes(value, seen, cli) } else { Ok(value) }
    }

    fn load_includes(
        &self,
        mut value: ConfigValue,
        seen: &mut FxHashSet<PathBuf>,
        cli: bool,
    ) -> anyhow::Result<ConfigValue> {
        let includes = include_paths(&mut value)?;
        let mut root = CV::Table(FxHashMap::default(), value.definition().clone());
        for include in includes {
            let base = match &include.def {
                Definition::Path(p) | Definition::Cli(Some(p)) => p.parent().unwrap(),
                _ => &self.cwd,
            };
            let abs_path = paths::normalize_path(&base.join(&include.path));
            if include.optional && !abs_path.exists() {
                continue;
            }
            self.load_file(&abs_path, seen, true, cli)
                .and_then(|include| root.merge(include, true))
                .with_context(|| {
                    format!(
                        "failed to load config include `{}` from `{}`",
                        include.path.display(),
                        include.def
                    )
                })?;
        }
        root.merge(value, true)?;
        Ok(root)
    }

    fn merge_cli_args(&mut self, cli: &[String]) -> anyhow::Result<()> {
        let mut loaded = CV::Table(FxHashMap::default(), Definition::Cli(None));
        for arg in cli {
            let arg_as_path = self.cwd.join(arg);
            let table = if !arg.is_empty() && arg_as_path.exists() {
                self.load_file(&arg_as_path, &mut FxHashSet::default(), true, true).with_context(
                    || format!("failed to load config from `{}`", arg_as_path.display()),
                )?
            } else {
                let doc = cli_dotted_keys(arg)?;
                for secret in ["token", "secret-key"] {
                    if doc.get("registry").and_then(|v| v.get(secret)).is_some() {
                        bail!(
                            "registry.{secret} cannot be set through --config for security reasons"
                        );
                    }
                    if let Some((k, _)) = doc
                        .get("registries")
                        .and_then(|v| v.as_table())
                        .and_then(|t| t.iter().find(|(_, v)| v.get(secret).is_some()))
                    {
                        bail!(
                            "registries.{k}.{secret} cannot be set through --config for security reasons"
                        );
                    }
                }
                let value = CV::from_toml(Definition::Cli(None), toml::Value::Table(doc))
                    .with_context(|| format!("failed to convert --config argument `{arg}`"))?;
                self.load_includes(value, &mut FxHashSet::default(), true)
                    .context("failed to load --config include")?
            };
            loaded
                .merge(table, true)
                .with_context(|| format!("failed to merge --config argument `{arg}`"))?;
        }
        self.values()?;
        let values = self.values.take().unwrap_or_default();
        let mut root = CV::Table(values, Definition::BuiltIn);
        root.merge(loaded, true)?;
        match root {
            CV::Table(map, _) => self.values = OnceCell::from(map),
            _ => unreachable!(),
        }
        Ok(())
    }

    /// The merged value at `key` from files and `--config`, without the environment.
    pub fn get_cv(&self, key: &str) -> anyhow::Result<Option<ConfigValue>> {
        let parts: Vec<&str> = key.split('.').collect();
        let Some(mut val) = self.values()?.get(parts[0]) else { return Ok(None) };
        for (i, part) in parts.iter().enumerate().skip(1) {
            match val {
                CV::Table(map, _) => match map.get(*part) {
                    Some(v) => val = v,
                    None => return Ok(None),
                },
                other => bail!(
                    "expected table for configuration key `{}`, but found {} in {}",
                    parts[..i].join("."),
                    other.desc(),
                    other.definition()
                ),
            }
        }
        Ok(Some(val.clone()))
    }

    /// The value at `key` with its environment variable taken into account, as cargo's
    /// `get_cv_with_env`: the variable wins over files, a list gets the variable's words
    /// appended.
    pub fn get_cv_with_env(&self, key: &str) -> anyhow::Result<Option<ConfigValue>> {
        let cv = self.get_cv(key)?;
        let env_key = env_key(key);
        let env = self.env(&env_key);
        let env_def = Definition::Environment(env_key.clone());
        let use_env = match (&cv, env) {
            (Some(CV::List(..)), Some(_)) => true,
            (Some(cv), Some(_)) => env_def.is_higher_priority(cv.definition()),
            (None, Some(_)) => true,
            _ => false,
        };
        if !use_env {
            return Ok(cv);
        }
        let env = env.unwrap();
        if env == "true" {
            Ok(Some(CV::Boolean(true, env_def)))
        } else if env == "false" {
            Ok(Some(CV::Boolean(false, env_def)))
        } else if let Ok(i) = env.parse::<i64>() {
            Ok(Some(CV::Integer(i, env_def)))
        } else {
            match cv {
                Some(CV::List(mut list, def)) => {
                    self.env_list(key, &mut list);
                    Ok(Some(CV::List(list, def)))
                }
                _ => Ok(Some(CV::String(env.to_owned(), env_def))),
            }
        }
    }

    fn env_list(&self, key: &str, output: &mut Vec<ConfigValue>) {
        let env_key = env_key(key);
        let Some(env_val) = self.env(&env_key) else { return };
        let env_def = Definition::Environment(env_key.clone());
        let parts: Vec<String> = key.split('.').map(str::to_owned).collect();
        if is_nonmergeable_list(&parts) {
            if output.first().is_some_and(|o| o.definition().is_higher_priority(&env_def)) {
                return;
            }
            output.clear();
        }
        output
            .extend(env_val.split_whitespace().map(|s| CV::String(s.to_owned(), env_def.clone())));
        output.sort_by_key(|v| v.definition().rank());
    }

    pub fn get_string(&self, key: &str) -> anyhow::Result<Option<Value<String>>> {
        match self.get_cv_with_env(key)? {
            None => Ok(None),
            Some(CV::String(val, definition)) => Ok(Some(Value { val, definition })),
            Some(other) => Err(expected("a string", key, &other)),
        }
    }

    pub fn get_bool(&self, key: &str) -> anyhow::Result<Option<Value<bool>>> {
        match self.get_cv_with_env(key)? {
            None => Ok(None),
            Some(CV::Boolean(val, definition)) => Ok(Some(Value { val, definition })),
            Some(other) => Err(expected("true/false", key, &other)),
        }
    }

    /// A path relative to where it was defined.
    pub fn get_relative_path(&self, key: &str) -> anyhow::Result<Option<ConfigRelativePath>> {
        Ok(self.get_string(key)?.map(ConfigRelativePath))
    }

    /// The `build.target-dir` of `CARGO_TARGET_DIR` or the config, as cargo's
    /// `GlobalContext::target_dir`.
    pub fn target_dir(&self) -> anyhow::Result<Option<PathBuf>> {
        if let Some(dir) = self.env("CARGO_TARGET_DIR") {
            if dir.is_empty() {
                bail!(
                    "the target directory is set to an empty string in the `CARGO_TARGET_DIR` environment variable"
                )
            }
            return Ok(Some(self.cwd.join(dir)));
        }
        let Some(val) = self.get_file_relative_path("build.target-dir")? else { return Ok(None) };
        if val.raw_value().is_empty() {
            bail!("the target directory is set to an empty string in {}", val.0.definition)
        }
        Ok(Some(val.resolve_path(&self.cwd)))
    }

    /// `build.build-dir` with its templates filled in, as cargo's `GlobalContext::build_dir`.
    pub fn build_dir(&self, workspace_manifest_path: &Path) -> anyhow::Result<Option<PathBuf>> {
        let Some(val) = self.get_file_relative_path("build.build-dir")? else { return Ok(None) };
        let root = workspace_manifest_path.parent().unwrap();
        let replacements = [
            (
                "{workspace-root}",
                root.to_str().context("workspace root was not valid utf-8")?.to_owned(),
            ),
            (
                "{cargo-cache-home}",
                self.home.to_str().context("cargo home was not valid utf-8")?.to_owned(),
            ),
        ];
        if val.raw_value().contains("{workspace-path-hash}") {
            // cargo's hash of the path is its `StableHasher` over `Path`'s `Hash`, not ported yet.
            bail!(
                "`{{workspace-path-hash}}` in build.build-dir (defined in {}) is not supported by blaze yet",
                val.0.definition
            );
        }
        let mut value = val.raw_value().to_owned();
        for (from, to) in &replacements {
            value = value.replace(from, to);
        }
        if let (Some(open), Some(close)) = (value.find('{'), value.rfind('}'))
            && open < close
        {
            let variable = &value[open + 1..close];
            let names = ["workspace-root", "cargo-cache-home", "workspace-path-hash"];
            let suggestion = crate::edit_distance::closest_msg(
                variable,
                names.iter(),
                |k| k,
                "template variable",
            );
            let suggestion = if suggestion.is_empty() {
                format!(
                    "\n\nhelp: available template variables are {}",
                    names.iter().map(|v| format!("`{{{v}}}`")).collect::<Vec<_>>().join(", ")
                )
            } else {
                suggestion
            };
            bail!(
                "unexpected variable `{variable}` in build.build-dir path `{}`{suggestion}",
                val.raw_value()
            );
        }
        if value.contains('{') {
            bail!("unexpected opening bracket `{{` in build.build-dir path `{}`", val.raw_value());
        }
        if value.contains('}') {
            bail!("unexpected closing bracket `}}` in build.build-dir path `{}`", val.raw_value());
        }
        if val.raw_value().is_empty() {
            bail!("the build directory is set to an empty string in {}", val.0.definition)
        }
        Ok(Some(val.0.definition.root(&self.cwd).join(value)))
    }

    fn get_file_relative_path(&self, key: &str) -> anyhow::Result<Option<ConfigRelativePath>> {
        self.get_relative_path(key)
    }

    /// `registries.<name>.index`, as cargo's `GlobalContext::get_registry_index`.
    pub fn registry_index(&self, registry: &str) -> anyhow::Result<Url> {
        cargo_util_schemas::manifest::RegistryName::new(registry)?;
        let Some(index) = self.get_string(&format!("registries.{registry}.index"))? else {
            bail!("registry index was not found in any configuration: `{registry}`");
        };
        self.resolve_registry_index(&index).with_context(|| {
            format!("invalid index URL for registry `{registry}` defined in {}", index.definition)
        })
    }

    fn resolve_registry_index(&self, index: &Value<String>) -> anyhow::Result<Url> {
        let base = index.definition.root(&self.cwd).join("truncated-by-url_with_base");
        crate::source::into_url(&index.val)?;
        let base = Url::from_file_path(&base).ok();
        let url = Url::options()
            .base_url(base.as_ref())
            .parse(&index.val)
            .map_err(|e| anyhow!("invalid url `{}`: {}", index.val, e))?;
        if url.password().is_some() {
            bail!("registry URLs may not contain passwords");
        }
        Ok(url)
    }

    /// Fails if `registry.index` is set, as cargo does since it stopped supporting it.
    pub fn check_registry_index_not_set(&self) -> anyhow::Result<()> {
        if self.get_string("registry.index")?.is_some() {
            bail!(
                "the `registry.index` config value is no longer supported\n\
                 Use `[source]` replacement to alter the default index for crates.io."
            );
        }
        Ok(())
    }
}

fn expected(ty: &str, key: &str, val: &ConfigValue) -> anyhow::Error {
    anyhow!(
        "invalid configuration for key `{key}`\nexpected {ty}, but found a {} for `{key}` in {}",
        val.desc(),
        val.definition()
    )
}

/// A path in the configuration, relative to the directory of what defined it.
#[derive(Clone, Debug)]
pub struct ConfigRelativePath(pub Value<String>);

impl ConfigRelativePath {
    pub fn raw_value(&self) -> &str {
        &self.0.val
    }

    pub fn resolve_path(&self, cwd: &Path) -> PathBuf {
        self.0.definition.root(cwd).join(&self.0.val)
    }
}

struct Include {
    path: PathBuf,
    def: Definition,
    optional: bool,
}

fn include_paths(cv: &mut ConfigValue) -> anyhow::Result<Vec<Include>> {
    let CV::Table(table, _) = cv else { unreachable!() };
    let includes = match table.remove("include") {
        Some(CV::List(list, _)) => list
            .into_iter()
            .enumerate()
            .map(|(idx, cv)| match cv {
                CV::String(s, def) => Ok(Include { path: s.into(), def, optional: false }),
                CV::Table(mut table, def) => {
                    let path = match table.remove("path") {
                        Some(CV::String(s, _)) => s,
                        Some(other) => bail!(
                            "expected a string, but found {} at `include[{idx}].path` in `{def}`",
                            other.desc()
                        ),
                        None => bail!("missing field `path` at `include[{idx}]` in `{def}`"),
                    };
                    let optional = match table.remove("optional") {
                        Some(CV::Boolean(b, _)) => b,
                        Some(other) => bail!(
                            "expected a boolean, but found {} at `include[{idx}].optional` in `{def}`",
                            other.desc()
                        ),
                        None => false,
                    };
                    Ok(Include { path: path.into(), def, optional })
                }
                other => bail!(
                    "expected a string or table, but found {} at `include[{idx}]` in {}",
                    other.desc(),
                    other.definition(),
                ),
            })
            .collect::<anyhow::Result<Vec<_>>>()?,
        Some(other) => bail!(
            "expected a list of strings or a list of tables, but found {} at `include` in `{}",
            other.desc(),
            other.definition()
        ),
        None => return Ok(Vec::new()),
    };
    for include in &includes {
        if include.path.extension() != Some(std::ffi::OsStr::new("toml")) {
            bail!(
                "expected a config include path ending with `.toml`, but found `{}` from `{}`",
                include.path.display(),
                include.def,
            )
        }
        if let Some(path) = include.path.to_str() {
            if path.contains(['*', '?', '[', ']']) {
                bail!(
                    "expected a config include path without glob patterns, but found `{}` from `{}`",
                    include.path.display(),
                    include.def,
                )
            }
            if path.contains(['{', '}']) {
                bail!(
                    "expected a config include path without template braces, but found `{}` from `{}`",
                    include.path.display(),
                    include.def,
                )
            }
        }
    }
    Ok(includes)
}

/// `config` if it exists, else `config.toml`: cargo prefers the old name when both are there.
fn get_file_path(dir: &Path, name: &str) -> Option<PathBuf> {
    let possible = dir.join(name);
    if std::fs::metadata(&possible).is_ok() {
        return Some(possible);
    }
    let with_extension = dir.join(format!("{name}.toml"));
    with_extension.exists().then_some(with_extension)
}

/// A `--config KEY=VALUE` argument, which must be one dotted key and a value.
fn cli_dotted_keys(arg: &str) -> anyhow::Result<toml::Table> {
    const HELP: &str =
        "--config argument `{arg}` was not a TOML dotted key expression (such as `build.jobs = 2`)";
    let fail = || anyhow!(HELP.replace("{arg}", arg));
    let table: toml::Table = arg.parse().map_err(|e: toml::de::Error| {
        anyhow!("failed to parse value from --config argument `{arg}` as a dotted key expression")
            .context(e.to_string())
    })?;
    // Exactly one key path ending in a value that is not a table.
    let mut current = &table;
    loop {
        if current.len() != 1 {
            return Err(fail());
        }
        let (_, value) = current.iter().next().unwrap();
        match value {
            toml::Value::Table(t) => current = t,
            _ => break,
        }
    }
    if arg.contains('\n') || arg.trim_start().starts_with('[') {
        return Err(fail());
    }
    Ok(table)
}

/// `$CARGO_HOME` relative to `cwd`, or `.cargo` in the home directory, as the `home` crate
/// cargo uses finds it.
fn cargo_home(cwd: &Path, env: &BTreeMap<String, String>) -> anyhow::Result<PathBuf> {
    match env.get("CARGO_HOME").filter(|h| !h.is_empty()) {
        Some(home) => Ok(cwd.join(home)),
        None => {
            let dir = home_dir(env)
                .ok_or_else(|| anyhow!("Cargo couldn't find your home directory. This probably means that $HOME was not set."))?;
            Ok(dir.join(".cargo"))
        }
    }
}

fn home_dir(env: &BTreeMap<String, String>) -> Option<PathBuf> {
    if cfg!(windows) {
        env.get("USERPROFILE")
            .filter(|h| !h.is_empty())
            .map(PathBuf::from)
            .or_else(std::env::home_dir)
    } else {
        env.get("HOME").filter(|h| !h.is_empty()).map(PathBuf::from).or_else(std::env::home_dir)
    }
}
