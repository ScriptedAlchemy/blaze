//! Test selection and retries for an owned source build use the same policy as a daemon
//! build. Only the build context differs: admitted private artifacts replace Cargo state.

use std::{
    collections::BTreeMap,
    ffi::OsString,
    path::{Path, PathBuf},
    process::Command,
};

use anyhow::Context as _;
use blaze_spec::{
    execution::{Output, Program},
    replay::Prepared,
};

use super::super::{
    affected,
    cli::{self, Invocation},
    run, sandboxed,
};

pub(super) struct Policy {
    pub(super) asked: Invocation,
    root: PathBuf,
    cache: PathBuf,
    enabled: bool,
    passed_env: String,
    attempts: Option<String>,
    sandbox: Option<Result<(), String>>,
    executables: BTreeMap<u32, PathBuf>,
}

impl Policy {
    pub(super) fn new(
        prepared: &Prepared,
        cache: &Path,
        env: &BTreeMap<String, OsString>,
        programs: &[Program],
        presentation: Option<&Invocation>,
    ) -> anyhow::Result<Option<Self>> {
        let blaze_wire::Command::Test(run) = &prepared.resolved.spec.build.command else {
            return Ok(None);
        };
        let mut args = vec!["test".to_owned()];
        if run.no_fail_fast {
            args.push("--no-fail-fast".into());
        }
        if run.no_run {
            args.push("--no-run".into());
        }
        if presentation.is_some_and(|asked| asked.flags.quiet) {
            args.push("--quiet".into());
        }
        if presentation.is_some_and(|asked| asked.flags.verbose) {
            args.push("--verbose".into());
        }
        args.extend(run.filters.iter().cloned());
        args.push("--".into());
        args.extend(run.harness_args.iter().cloned());
        let asked = cli::of_cargo(&args).context("owned test command could not be represented")?;
        // The replay environment is reconstructed from recorded values. Never consult the
        // worker's ambient test switches, retry override or declared variable list here.
        let setting = |name: &str| {
            run.env
                .iter()
                .find(|(key, _)| key == name)
                .map(|(_, value)| value.clone())
                .or_else(|| env.get(name).and_then(|value| value.to_str()).map(str::to_owned))
        };
        let enabled = setting(affected::VAR).as_deref() == Some("affected");
        let sandbox = match setting(sandboxed::VAR).as_deref() {
            Some("off" | "0") => None,
            _ if enabled => Some(crate::sandbox::available()),
            _ => None,
        };
        Ok(Some(Self {
            asked,
            root: prepared.resolved.graph.root.clone(),
            cache: cache.join(super::hex(&blaze_wire::digest_of(&prepared.resolved.spec.config))),
            enabled,
            passed_env: setting(sandboxed::PASS).unwrap_or_default(),
            attempts: setting(affected::ATTEMPTS),
            sandbox,
            executables: programs
                .iter()
                .map(|program| (program.unit, program.executable.clone()))
                .collect(),
        }))
    }

    pub(super) fn reach(&self, program: &Program) -> bool {
        self.enabled && program.harness && program.output == Output::Executable
    }

    pub(super) fn run(
        &self,
        program: &Program,
        unit: &blaze_wire::UnitSpec,
        package: &str,
        package_dir: &Path,
        label: &str,
        binaries: &[(String, PathBuf)],
        inherited: &BTreeMap<String, OsString>,
        overrides: &BTreeMap<String, OsString>,
        mut command: impl FnMut(&Path) -> anyhow::Result<Command>,
    ) -> anyhow::Result<run::TestResult> {
        let executable =
            self.executables.get(&program.unit).context("test has no private artifact")?;
        // Unit identity carries source location, features and profiles, while function
        // contents belong in the reach hash compared with this unit's prior passes.
        let directory = self.cache.join(super::hex(&blaze_wire::digest_of(unit)));
        std::fs::create_dir_all(&directory)?;
        let lock = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(directory.join("lock"))?;
        lock.lock().context("locking owned test results")?;
        // A new compilation can never borrow an earlier executable's reach metadata.
        // The lock covers both this replacement and settlement of the resulting passes.
        if self.reach(program) {
            let from = executable
                .parent()
                .context("test artifact has no parent")?
                .join(crate::cargo::plan::REACH);
            let to = directory.join(crate::cargo::plan::REACH);
            match std::fs::read(&from) {
                Ok(bytes) => std::fs::write(&to, bytes)?,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    match std::fs::remove_file(&to) {
                        Ok(()) => {}
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                        Err(error) => return Err(error.into()),
                    }
                }
                Err(error) => return Err(error.into()),
            }
        }
        let identity = || {
            affected::programs_identity(
                binaries.iter().map(|(name, path)| (name.as_str(), path.as_path())),
            )
        };
        let before = identity();
        let mut explicit = Command::new(executable);
        explicit.env_clear().envs(overrides);
        let sandbox_env = sandboxed::env_owned(
            &explicit,
            &affected::declared_env_at(&self.root),
            &self.passed_env,
            inherited.iter().map(|(name, value)| (name.into(), value.clone())),
        );
        let context = run::TestRun {
            cache: affected::TestContext {
                root: &self.root,
                directory: Some(&directory),
                package,
                programs: before.clone(),
                enabled: self.enabled && program.harness,
                passed_env: &self.passed_env,
                inherit_env: false,
            },
            package_dir,
            executable,
            label,
            sandbox: self.sandbox.clone(),
            sandbox_env: Some(sandbox_env),
            attempts: affected::attempts_at(&self.root, self.attempts.as_deref()),
        };
        let result = run::test_with(&self.asked, &context, || command(executable), run::started);
        // Published CARGO_BIN_EXE paths can be replaced by another build while a test is
        // running. Such a run does not certify either generation as a reusable pass.
        if result.is_err() || !matches!((before, identity()), (Ok(a), Ok(b)) if a == b) {
            affected::forget_passes_at(&directory)?;
        }
        result
    }
}
