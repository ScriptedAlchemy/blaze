//! A hybrid build carries its completed script runs through its existing Cargo leg.
//! This is not a cache: the private directory belongs to one live request, disappears on
//! return, and is never offered to a later invocation.

use std::{collections::BTreeMap, io::Write as _, path::PathBuf, time::SystemTime};

use super::{Ran, Run};

const VARIABLE: &str = "RA_JIT_SCRIPT_CONTINUATION";

#[derive(serde::Serialize, serde::Deserialize)]
pub(crate) struct Completed {
    out_dir: PathBuf,
    real: PathBuf,
    cwd: PathBuf,
    run: Run,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    fingerprint: super::super::records::Fingerprint,
    began: std::time::SystemTime,
    inputs: Inputs,
}

/// State after the execution, including the watches it dropped. Cargo may change its
/// fingerprint's local rules before invoking the stand-in, so that fingerprint alone
/// cannot establish whether this execution still belongs to the inputs Cargo sees.
#[derive(serde::Serialize, serde::Deserialize)]
pub(super) struct Inputs {
    files: Vec<(PathBuf, Option<SystemTime>, Option<Vec<u8>>)>,
    variables: BTreeMap<String, Option<String>>,
    metadata: BTreeMap<String, String>,
    package: Option<String>,
    executable: (SystemTime, Vec<u8>),
}

impl Inputs {
    pub(super) fn of(
        ran: &Ran,
        env: &BTreeMap<String, String>,
        before: &blaze_build_script::Output,
        after: &blaze_build_script::Output,
        dependencies: &[(&str, u64, PathBuf)],
    ) -> Result<Self, String> {
        let paths: std::collections::BTreeSet<_> = before
            .rerun_if_changed
            .iter()
            .chain(&after.rerun_if_changed)
            .chain(dependencies.iter().map(|it| &it.2))
            .cloned()
            .collect();
        let files = paths.into_iter().map(|path| Self::file(ran.cwd.join(path))).collect();
        let variables = before
            .rerun_if_env_changed
            .iter()
            .chain(&after.rerun_if_env_changed)
            .map(|name| (name.clone(), env.get(name).cloned()))
            .collect();
        let metadata = Self::metadata(env);
        let package = (after.rerun_if_changed.is_empty() && after.rerun_if_env_changed.is_empty())
            .then(|| crate::scripts::path_package_fingerprint(&ran.cwd));
        let compiled = std::fs::metadata(&ran.real)
            .and_then(|it| it.modified())
            .map_err(|it| it.to_string())?;
        let executable = (compiled, super::file_digest(&ran.real).map_err(|it| it.to_string())?);
        Ok(Self { files, variables, metadata, package, executable })
    }

    fn file(path: PathBuf) -> (PathBuf, Option<SystemTime>, Option<Vec<u8>>) {
        let modified = super::super::records::written_below(&path);
        let digest =
            if path.is_dir() { super::tree_digest(&path) } else { super::file_digest(&path) };
        (path, modified, digest.ok())
    }

    fn metadata(env: &BTreeMap<String, String>) -> BTreeMap<String, String> {
        env.iter()
            .filter(|(name, _)| name.starts_with("DEP_"))
            .map(|it| (it.0.clone(), it.1.clone()))
            .collect()
    }

    fn unchanged(&self, ran: &Ran) -> bool {
        let env: BTreeMap<_, _> = ran.env.iter().cloned().collect();
        self.files.iter().all(|file| Self::file(file.0.clone()) == *file)
            && self.variables.iter().all(|(name, value)| env.get(name) == value.as_ref())
            && Self::metadata(&env) == self.metadata
            && self.package.as_ref().is_none_or(|package| {
                crate::scripts::path_package_fingerprint(&ran.cwd) == *package
            })
            && std::fs::metadata(&ran.real)
                .and_then(|it| it.modified())
                .is_ok_and(|it| it == self.executable.0)
            && super::file_digest(&ran.real).is_ok_and(|it| it == self.executable.1)
    }
}

impl Completed {
    pub(super) fn new(
        ran: &Ran,
        out_dir: &std::path::Path,
        run: Run,
        output: std::process::Output,
        fingerprint: super::super::records::Fingerprint,
        began: std::time::SystemTime,
        inputs: Inputs,
    ) -> Self {
        Self {
            out_dir: out_dir.to_owned(),
            real: ran.real.clone(),
            cwd: ran.cwd.clone(),
            run,
            stdout: output.stdout,
            stderr: output.stderr,
            fingerprint,
            began,
            inputs,
        }
    }
}

pub(crate) struct Continuation(PathBuf);

impl Continuation {
    /// Cargo's leg is still this build, not the next one. Its process recalculated local
    /// state from the new output before consuming the run. Keep the fingerprint that the
    /// actual execution made, so a later invocation observes Cargo's transition rule.
    pub(crate) fn finish(&self, completed: &[Completed]) -> anyhow::Result<()> {
        for (index, done) in completed.iter().enumerate() {
            if !self.0.join(format!("used-{index}")).exists() {
                continue;
            }
            let unit =
                super::super::records::Unit::of(&done.out_dir).map_err(anyhow::Error::msg)?;
            let _held = unit.hold().map_err(anyhow::Error::msg)?;
            let run = Run::left(&std::fs::read(unit.output())?, &done.out_dir)
                .map_err(anyhow::Error::msg)?;
            anyhow::ensure!(run == done.run, "the consumed script's output changed");
            unit.ran(done.began, &done.stdout, &done.stderr).map_err(anyhow::Error::msg)?;
            unit.record(&done.fingerprint).map_err(anyhow::Error::msg)?;
        }
        Ok(())
    }

    #[cfg(test)]
    pub(super) fn path(&self) -> PathBuf {
        self.0.clone()
    }

    pub(crate) fn for_command(
        command: &mut std::process::Command,
        completed: &[Completed],
    ) -> anyhow::Result<Option<Self>> {
        command.env_remove(VARIABLE);
        if completed.is_empty() {
            return Ok(None);
        }
        let base = std::env::temp_dir();
        let dir = loop {
            let dir = base.join(format!(
                "ra-jit-script-build-{}-{}",
                std::process::id(),
                super::super::units::stamp_now()
            ));
            match std::fs::create_dir(&dir) {
                Ok(()) => break dir,
                Err(it) if it.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(it) => return Err(it.into()),
            }
        };
        let held = Self(dir);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&held.0, std::fs::Permissions::from_mode(0o700))?;
        }
        std::fs::write(held.0.join("runs"), serde_json::to_vec(completed)?)?;
        command.env(VARIABLE, &held.0);
        Ok(Some(held))
    }
}

impl Drop for Continuation {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

pub(super) fn consume(ran: &Ran, out_dir: &std::path::Path) -> anyhow::Result<bool> {
    let Some(dir) = std::env::var_os(VARIABLE).map(PathBuf::from) else { return Ok(false) };
    consume_in(ran, out_dir, &dir)
}

pub(super) fn consume_in(
    ran: &Ran,
    out_dir: &std::path::Path,
    dir: &std::path::Path,
) -> anyhow::Result<bool> {
    let completed: Vec<Completed> = serde_json::from_slice(&std::fs::read(dir.join("runs"))?)?;
    let Some((index, done)) = completed.iter().enumerate().find(|(_, it)| it.out_dir == out_dir)
    else {
        return Ok(false);
    };
    if done.real != ran.real || done.cwd != ran.cwd || !done.inputs.unchanged(ran) {
        return Ok(false);
    }
    // Cargo owns the target locks while running this stand-in. Do not acquire them again.
    let run = Run::left(
        &std::fs::read(
            super::printed(out_dir).ok_or_else(|| anyhow::anyhow!("script has no output"))?,
        )?,
        out_dir,
    )
    .map_err(anyhow::Error::msg)?;
    if run != done.run {
        return Ok(false);
    }
    let used = dir.join(format!("used-{index}"));
    std::fs::File::options().write(true).create_new(true).open(used)?;
    std::io::stdout().write_all(&done.stdout)?;
    std::io::stderr().write_all(&done.stderr)?;
    Ok(true)
}
