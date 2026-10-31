//! A hybrid build carries its completed script runs through its existing Cargo leg.
//! This is not a cache: the private directory belongs to one live request, disappears on
//! return, and is never offered to a later invocation. The values of the variables a run read
//! are not in it, only their identities under a key this process made and gives cargo's leg
//! in its environment (docs/native-environment.md).

use std::{collections::BTreeMap, io::Write as _, path::PathBuf, time::SystemTime};

use super::{Ran, Run};

const VARIABLE: &str = "RA_JIT_SCRIPT_CONTINUATION";

/// The variable that gives cargo's leg the key of [`Inputs`]' identities, as hex.
pub(super) const KEY: &str = "RA_JIT_SCRIPT_CONTINUATION_KEY";

/// The key this process names the values of variables by in the runs it carries. A new one in
/// every process: the identities are never compared across requests.
static OWN_KEY: std::sync::LazyLock<[u8; 32]> = std::sync::LazyLock::new(|| {
    let mut key = [0; 32];
    getrandom::fill(&mut key).expect("entropy for a script continuation key");
    key
});

fn hex(key: &[u8; 32]) -> String {
    key.iter().map(|it| format!("{it:02x}")).collect()
}

/// The key of the identities in the runs this process was given: cargo's leg's, from [`KEY`],
/// or else this process's own. A key that is not the one they were made with makes every run
/// another's, and cargo runs it.
fn given_key() -> [u8; 32] {
    let given = std::env::var(KEY).ok().and_then(|it| {
        let bytes = it
            .as_bytes()
            .chunks_exact(2)
            .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).ok()?, 16).ok());
        let bytes: Option<Vec<u8>> = bytes.collect();
        bytes?.try_into().ok()
    });
    given.unwrap_or(*OWN_KEY)
}

fn identity(key: &[u8; 32], value: Option<&str>) -> Option<String> {
    crate::env_identity::Identity::from_key(*key).value(value)
}

/// A run this build made in cargo's `OUT_DIR`, which is cargo's once the build checked it.
pub(crate) struct Completed {
    carried: Carried,
    /// Cargo's fingerprint of the run, in cargo's format: it has the value of each variable
    /// the script said it reads (`rerun-if-env-changed`), as cargo writes it, so it goes only
    /// where cargo writes it, never into the private file of [`Carried`].
    fingerprint: super::super::records::Fingerprint,
    began: std::time::SystemTime,
}

/// What cargo's leg is given of a completed run, in the private file: what [`consume_in`]
/// compares and prints again, and no value of a variable.
#[derive(serde::Serialize, serde::Deserialize)]
struct Carried {
    out_dir: PathBuf,
    real: PathBuf,
    cwd: PathBuf,
    run: Run,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    inputs: Inputs,
}

/// State after the execution, including the watches it dropped. Cargo may change its
/// fingerprint's local rules before invoking the stand-in, so that fingerprint alone
/// cannot establish whether this execution still belongs to the inputs Cargo sees.
#[derive(serde::Serialize, serde::Deserialize)]
pub(super) struct Inputs {
    files: Vec<(PathBuf, Option<SystemTime>, Option<Vec<u8>>)>,
    /// What the runs it was built after printed, by content alone: cargo's leg, taking such a
    /// run made for this build as its own, writes the same lines there again, later.
    printed: Vec<(PathBuf, Option<Vec<u8>>)>,
    /// The identity of each variable's value ([`identity`]), or none when it was unset.
    variables: BTreeMap<String, Option<String>>,
    /// The identities of the values of the `DEP_*` variables.
    metadata: BTreeMap<String, Option<String>>,
    package: Option<String>,
    executable: (SystemTime, Vec<u8>),
}

impl Inputs {
    pub(super) fn of(
        ran: &Ran,
        env: &BTreeMap<String, String>,
        before: &blaze_build_script::Output,
        after: &blaze_build_script::Output,
        edges: &[super::records::Edge<'_>],
    ) -> Result<Self, String> {
        let paths: std::collections::BTreeSet<_> =
            before.rerun_if_changed.iter().chain(&after.rerun_if_changed).cloned().collect();
        let files = paths.into_iter().map(|path| Self::file(ran.cwd.join(path))).collect();
        let printed = edges.iter().map(|it| Self::printed(it.output.clone())).collect();
        let variables = before
            .rerun_if_env_changed
            .iter()
            .chain(&after.rerun_if_env_changed)
            .map(|name| (name.clone(), identity(&OWN_KEY, env.get(name).map(String::as_str))))
            .collect();
        let metadata = Self::metadata(env, &OWN_KEY);
        let package = (after.rerun_if_changed.is_empty() && after.rerun_if_env_changed.is_empty())
            .then(|| crate::scripts::path_package_fingerprint(&ran.cwd));
        let compiled = std::fs::metadata(&ran.real)
            .and_then(|it| it.modified())
            .map_err(|it| it.to_string())?;
        let executable = (compiled, super::file_digest(&ran.real).map_err(|it| it.to_string())?);
        Ok(Self { files, printed, variables, metadata, package, executable })
    }

    fn file(path: PathBuf) -> (PathBuf, Option<SystemTime>, Option<Vec<u8>>) {
        let modified = super::super::records::written_below(&path);
        let digest =
            if path.is_dir() { super::tree_digest(&path) } else { super::file_digest(&path) };
        (path, modified, digest.ok())
    }

    fn printed(path: PathBuf) -> (PathBuf, Option<Vec<u8>>) {
        let digest = super::file_digest(&path).ok();
        (path, digest)
    }

    fn metadata(
        env: &BTreeMap<String, String>,
        key: &[u8; 32],
    ) -> BTreeMap<String, Option<String>> {
        env.iter()
            .filter(|(name, _)| name.starts_with("DEP_"))
            .map(|it| (it.0.clone(), identity(key, Some(it.1))))
            .collect()
    }

    fn unchanged(&self, ran: &Ran, key: &[u8; 32]) -> bool {
        let env: BTreeMap<_, _> = ran.env.iter().cloned().collect();
        self.files.iter().all(|file| Self::file(file.0.clone()) == *file)
            && self.printed.iter().all(|it| Self::printed(it.0.clone()) == *it)
            && self
                .variables
                .iter()
                .all(|(name, value)| identity(key, env.get(name).map(String::as_str)) == *value)
            && Self::metadata(&env, key) == self.metadata
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
    pub(super) fn out_dir(&self) -> &std::path::Path {
        &self.carried.out_dir
    }

    pub(super) fn run(&self) -> &Run {
        &self.carried.run
    }

    pub(super) fn fingerprint(&self) -> &super::super::records::Fingerprint {
        &self.fingerprint
    }

    /// Records the run as cargo's, once the build has checked it is the run it had: cargo's
    /// next build finds it and does not run the script again. Not when the directory has
    /// another run by then.
    pub(crate) fn record(&self) -> Result<(), String> {
        let unit = super::super::records::Unit::of(self.out_dir())?;
        let _held = unit.hold()?;
        let printed = std::fs::read(unit.output()).map_err(|it| it.to_string())?;
        if Run::left(&printed, self.out_dir())? != *self.run() {
            return Err(format!("{}: another run of the script", self.out_dir().display()));
        }
        unit.record(&self.fingerprint)
    }

    pub(super) fn new(
        ran: &Ran,
        out_dir: &std::path::Path,
        run: Run,
        output: std::process::Output,
        fingerprint: super::super::records::Fingerprint,
        began: std::time::SystemTime,
        inputs: Inputs,
    ) -> Self {
        let carried = Carried {
            out_dir: out_dir.to_owned(),
            real: ran.real.clone(),
            cwd: ran.cwd.clone(),
            run,
            stdout: output.stdout,
            stderr: output.stderr,
            inputs,
        };
        Self { carried, fingerprint, began }
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
                super::super::records::Unit::of(done.out_dir()).map_err(anyhow::Error::msg)?;
            let _held = unit.hold().map_err(anyhow::Error::msg)?;
            let run = Run::left(&std::fs::read(unit.output())?, done.out_dir())
                .map_err(anyhow::Error::msg)?;
            anyhow::ensure!(run == *done.run(), "the consumed script's output changed");
            let carried = &done.carried;
            unit.ran(done.began, &carried.stdout, &carried.stderr).map_err(anyhow::Error::msg)?;
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
        command.env_remove(VARIABLE).env_remove(KEY);
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
        let carried: Vec<&Carried> = completed.iter().map(|it| &it.carried).collect();
        std::fs::write(held.0.join("runs"), serde_json::to_vec(&carried)?)?;
        command.env(VARIABLE, &held.0).env(KEY, hex(&OWN_KEY));
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
    let completed: Vec<Carried> = serde_json::from_slice(&std::fs::read(dir.join("runs"))?)?;
    let Some((index, done)) = completed.iter().enumerate().find(|(_, it)| it.out_dir == out_dir)
    else {
        return Ok(false);
    };
    if done.real != ran.real || done.cwd != ran.cwd || !done.inputs.unchanged(ran, &given_key()) {
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

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn carried_inputs_hold_identities_of_values_never_the_values() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("script");
        std::fs::write(&real, "#!/bin/sh\n").unwrap();
        let ran = Ran {
            real,
            cwd: dir.path().to_owned(),
            env: vec![
                ("PR15_TOKEN".to_owned(), "synthetic-secret".to_owned()),
                ("DEP_PROBE_KEY".to_owned(), "synthetic-metadata".to_owned()),
            ],
        };
        let env: BTreeMap<_, _> = ran.env.iter().cloned().collect();
        let said = blaze_build_script::Output {
            rerun_if_env_changed: vec!["PR15_TOKEN".to_owned()],
            ..Default::default()
        };
        let inputs = Inputs::of(&ran, &env, &said, &said, &[]).unwrap();
        let text = String::from_utf8(serde_json::to_vec(&inputs).unwrap()).unwrap();
        assert!(!text.contains("synthetic-secret") && !text.contains("synthetic-metadata"));
        assert!(inputs.unchanged(&ran, &OWN_KEY));
        assert!(!inputs.unchanged(&ran, &[7; 32]), "another key is another value");
        let mut other = Ran { env: ran.env.clone(), ..ran };
        other.env[0].1 = "changed".to_owned();
        assert!(!inputs.unchanged(&other, &OWN_KEY));
    }
}
