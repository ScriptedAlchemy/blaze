//! Native outputs of build scripts are inputs to a link, not just to Rust name resolution.
//!
//! Keep the last successfully linked identity, never the last observed one. Generated
//! archives may change without changing a directive, Rust source, size, or timestamp.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::{self, Read},
    path::{Path, PathBuf},
    sync::{LazyLock, Mutex},
};

use anyhow::bail;
use sha2::{Digest as _, Sha256};

use crate::{Args, Program, RootDatabase};

type Digest = [u8; 32];
static LINKED_INPUTS: LazyLock<Mutex<BTreeMap<PathBuf, Digest>>> =
    LazyLock::new(|| Mutex::new(BTreeMap::new()));

#[derive(Default)]
struct Inputs {
    directives: Vec<Vec<u8>>,
    paths: BTreeSet<PathBuf>,
    opaque: bool,
}

/// A link's proposed identity. Dropping it after a failed build publishes nothing.
pub(crate) struct Pending {
    binary: PathBuf,
    inputs: Inputs,
    before: Option<Digest>,
    changed: bool,
}

impl Pending {
    /// An untrackable input prevents reuse, but does not refuse a build the linker accepts.
    pub(crate) fn changed(&self) -> bool {
        self.changed
    }

    pub(crate) fn commit(self) -> anyhow::Result<()> {
        let Some(before) = self.before else { return Ok(()) };
        if self.inputs.digest().ok() != Some(before) {
            bail!("native link inputs changed while the executable was being linked");
        }
        crate::lock(&LINKED_INPUTS).insert(self.binary, before);
        Ok(())
    }
}

fn collect(db: &RootDatabase, args: &Args) -> anyhow::Result<Option<(PathBuf, Inputs)>> {
    if args.exe.is_none() {
        return Ok(None);
    }
    let Some(binary) = args.splice.as_ref().or(args.exe.as_ref()) else { return Ok(None) };
    let mut roots = Vec::new();
    for name in args.crates() {
        roots.extend(crate::crates_named(db, name, args)?);
    }
    let mut out_dirs = BTreeSet::new();
    for &root in &roots {
        let root: ide_db::base_db::Crate = root.into();
        for krate in std::iter::once(root).chain(root.transitive_deps(db)) {
            if !krate.data(db).origin.is_lang()
                && let Some(out) = krate.env(db).get("OUT_DIR")
            {
                out_dirs.insert(PathBuf::from(out));
            }
        }
    }
    let mut inputs = Inputs::default();
    for out in &out_dirs {
        if let Some(printed) = crate::cargo::script::printed(out) {
            inputs.paths.insert(printed);
        }
    }
    let cwd = std::env::current_dir()?;
    let sysroot = crate::SYSROOT.get().and_then(Option::as_deref);
    // Use the real native link builder, including attribute-declared libraries and ordering.
    // Do not put the Rust objects this build writes into its own input fingerprint.
    let command = crate::link::command(db, &roots, &[], sysroot, binary)?;
    inputs.link_inputs(&command, &cwd, &out_dirs);
    for name in ["LIBRARY_PATH", "SDKROOT", "MACOSX_DEPLOYMENT_TARGET", "PATH"] {
        let mut field = name.as_bytes().to_vec();
        match std::env::var_os(name) {
            Some(value) => {
                field.push(1);
                field.extend_from_slice(value.as_encoded_bytes());
            }
            None => field.push(0),
        }
        inputs.directives.push(field);
    }
    // A native search directory may also contain this build's outputs. They are not inputs.
    for path in std::iter::once(&args.out)
        .chain(args.patch.iter())
        .chain(args.exe.iter())
        .chain(args.splice.iter())
        .chain(args.prebuilt.iter())
    {
        inputs.paths.remove(&cwd.join(path));
    }
    Ok(Some((binary.clone(), inputs)))
}

impl Inputs {
    /// Track named native candidates, not every file in a search directory. Missing earlier
    /// candidates matter too: creating one can change which library the linker selects.
    fn link_inputs(&mut self, command: &str, cwd: &Path, out_dirs: &BTreeSet<PathBuf>) {
        self.directives.push(command.as_bytes().to_vec());
        let Some(words) = shlex::split(command) else {
            self.opaque = true;
            return;
        };
        let words: Vec<String> = words.into_iter().skip(1).flat_map(|word| {
            match word.strip_prefix("-Wl,") {
                Some(rest) => rest.split(',').map(str::to_owned).collect(),
                None => vec![word],
            }
        }).collect();
        let mut search = out_dirs.clone();
        if let Some(paths) = std::env::var_os("LIBRARY_PATH") {
            search.extend(std::env::split_paths(&paths).map(|path| cwd.join(path)));
        }
        let mut frameworks = BTreeSet::new();
        let mut libraries = BTreeSet::new();
        let mut framework_names = BTreeSet::new();
        let mut at = 0;
        while at < words.len() {
            let word = &words[at];
            at += 1;
            let mut next = || {
                let found = words.get(at).cloned();
                at += usize::from(found.is_some());
                found
            };
            match word.as_str() {
                "-o" | "--output" | "-Map" | "--Map" | "-map" | "-dependency_info" => {
                    if next().is_none() { self.opaque = true; }
                }
                "-L" | "-F" => match next() {
                    Some(path) if word == "-L" => { search.insert(cwd.join(path)); }
                    Some(path) => { frameworks.insert(cwd.join(path)); }
                    None => self.opaque = true,
                },
                "-l" => match next() {
                    Some(name) => { libraries.insert(name); }
                    None => self.opaque = true,
                },
                "-framework" | "-weak_framework" => match next() {
                    Some(name) => { framework_names.insert(name); }
                    None => self.opaque = true,
                },
                "-T" | "--script" | "--version-script" | "--dynamic-list"
                | "-exported_symbols_list" | "-unexported_symbols_list" | "-order_file" => {
                    match next() {
                        Some(path) => { self.paths.insert(cwd.join(path)); }
                        None => self.opaque = true,
                    }
                }
                _ if word.starts_with("-L") && word.len() > 2 => { search.insert(cwd.join(&word[2..])); }
                _ if word.starts_with("-F") && word.len() > 2 => { frameworks.insert(cwd.join(&word[2..])); }
                _ if word.starts_with("-l") && word.len() > 2 => { libraries.insert(word[2..].to_owned()); }
                _ if word.starts_with('@') => {
                    self.paths.insert(cwd.join(&word[1..]));
                    // Linkers have different response grammars. Do not guess their input
                    // closure: this invocation links in full and does not advertise reuse.
                    self.opaque = true;
                }
                _ => {
                    let file = word.split_once('=').filter(|(flag, _)| {
                        matches!(*flag, "--script" | "--version-script" | "--dynamic-list")
                    }).map(|(_, path)| path);
                    if let Some(file) = file {
                        self.paths.insert(cwd.join(file));
                    } else if !word.starts_with('-') {
                        let path = cwd.join(word);
                        let native = path.extension().is_some_and(|ext| {
                            matches!(ext.to_str(), Some("a" | "o" | "so" | "dylib" | "lib" | "obj" | "ld" | "tbd"))
                        });
                        if native || path.is_file() {
                            self.paths.insert(path);
                        }
                    }
                }
            }
        }
        for dir in search {
            for name in &libraries {
                if let Some(name) = name.strip_prefix(':') {
                    self.paths.insert(dir.join(name));
                } else {
                    for suffix in ["a", "so", "dylib", "tbd"] {
                        self.paths.insert(dir.join(format!("lib{name}.{suffix}")));
                    }
                    self.paths.insert(dir.join(format!("{name}.lib")));
                }
            }
        }
        for dir in frameworks {
            for name in &framework_names {
                let framework = dir.join(format!("{name}.framework"));
                self.paths.insert(framework.join(name));
                self.paths.insert(framework.join(format!("{name}.tbd")));
            }
        }
    }
}

pub(crate) fn stale(db: &RootDatabase, programs: &[Program]) -> anyhow::Result<bool> {
    for program in programs {
        if let Some((binary, inputs)) = collect(db, &program.args)? {
            let now = inputs.digest().ok();
            if now.is_none() || crate::lock(&LINKED_INPUTS).get(&binary).copied() != now {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

pub(crate) fn invalidate(db: &RootDatabase, programs: &mut [Program]) -> anyhow::Result<()> {
    for program in programs {
        let Some((binary, inputs)) = collect(db, &program.args)? else { continue };
        let now = inputs.digest().ok();
        if now.is_some() && crate::lock(&LINKED_INPUTS).get(&binary).copied() == now {
            continue;
        }
        // The old identity must not become reusable after a failed A -> B -> A relink.
        crate::lock(&LINKED_INPUTS).remove(&binary);
        forget_binary(&binary);
        program.known.forget_base();
        program.whole = None;
        program.moved = true;
        program.unbuilt.stale = true;
        program.unbuilt.unlinked = true;
        program.args.link_cmd = None;
    }
    Ok(())
}

/// Prevent a native change from being installed as a Rust-only splice.
pub(crate) fn forget_binary(binary: &Path) {
    crate::splicer::claim(binary, true);
    crate::splicer::forget(binary);
    crate::lock(&crate::LINKED).remove(binary);
}

pub(crate) fn begin(db: &RootDatabase, args: &Args) -> anyhow::Result<Option<Pending>> {
    Ok(collect(db, args)?.map(|(binary, inputs)| pending(binary, inputs)))
}

fn pending(binary: PathBuf, inputs: Inputs) -> Pending {
    let before = inputs.digest().ok();
    // A failed attempt cannot leave an old identity available for A -> B -> A reuse.
    let previous = crate::lock(&LINKED_INPUTS).remove(&binary);
    let changed = before.is_none() || previous != before;
    Pending { binary, inputs, before, changed }
}

impl Inputs {
    fn digest(&self) -> io::Result<Digest> {
        if self.opaque {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "native input closure is not known"));
        }
        let mut hash = Sha256::new();
        frame(&mut hash, b"blaze/native-link-inputs/v1");
        frame(&mut hash, &(self.directives.len() as u64).to_le_bytes());
        for line in &self.directives {
            frame(&mut hash, line);
        }
        frame(&mut hash, &(self.paths.len() as u64).to_le_bytes());
        for path in &self.paths {
            frame(&mut hash, path.as_os_str().as_encoded_bytes());
            hash_path(path, &mut hash, &mut BTreeSet::new())?;
        }
        Ok(hash.finalize().into())
    }
}

fn frame(hash: &mut Sha256, bytes: &[u8]) {
    hash.update((bytes.len() as u64).to_le_bytes());
    hash.update(bytes);
}

fn hash_path(path: &Path, hash: &mut Sha256, active: &mut BTreeSet<PathBuf>) -> io::Result<()> {
    let meta = match fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            frame(hash, b"missing");
            return Ok(());
        }
        Err(error) => return Err(error),
    };
    if meta.file_type().is_symlink() {
        frame(hash, b"symlink");
        let target = fs::read_link(path)?;
        frame(hash, target.as_os_str().as_encoded_bytes());
        match path.canonicalize() {
            Ok(target) => return hash_path(&target, hash, active),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                frame(hash, b"missing-target");
                return Ok(());
            }
            Err(error) => return Err(error),
        }
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        frame(hash, &(meta.permissions().mode() & 0o7777).to_le_bytes());
    }
    if meta.is_file() {
        frame(hash, b"file");
        let mut file_hash = Sha256::new();
        let mut file = fs::File::open(path)?;
        let mut buffer = [0; 64 * 1024];
        loop {
            let read = file.read(&mut buffer)?;
            if read == 0 { break; }
            file_hash.update(&buffer[..read]);
        }
        frame(hash, &file_hash.finalize());
        return Ok(());
    }
    if !meta.is_dir() {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "native input is not a regular file or directory"));
    }
    frame(hash, b"directory");
    let real = path.canonicalize()?;
    if !active.insert(real.clone()) {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "native input directory cycle"));
    }
    let mut entries = fs::read_dir(path)?.collect::<io::Result<Vec<_>>>()?;
    entries.sort_by_key(|entry| entry.file_name());
    frame(hash, &(entries.len() as u64).to_le_bytes());
    for entry in entries {
        frame(hash, entry.file_name().as_encoded_bytes());
        hash_path(&entry.path(), hash, active)?;
    }
    active.remove(&real);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inputs(path: &Path) -> Inputs {
        Inputs { directives: vec![b"rustc-link-lib=static=generated".to_vec()], paths: BTreeSet::from([path.to_owned()]), opaque: false }
    }

    #[test]
    fn a_search_directory_does_not_hash_the_link_outputs() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("libgenerated.a"), b"native").unwrap();
        let mut inputs = Inputs::default();
        inputs.link_inputs("cc -L. -lgenerated -o program", dir.path(), &BTreeSet::new());
        let before = inputs.digest().unwrap();
        fs::write(dir.path().join("program"), b"new executable").unwrap();
        fs::write(dir.path().join("ra-jit.o"), b"new Rust object").unwrap();
        fs::write(dir.path().join("unrelated.log"), b"progress").unwrap();
        assert_eq!(inputs.digest().unwrap(), before);
    }

    #[test]
    fn an_earlier_native_candidate_appearing_invalidates_the_link() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("first")).unwrap();
        fs::create_dir(dir.path().join("second")).unwrap();
        fs::write(dir.path().join("second/libvalue.a"), b"B").unwrap();
        let mut inputs = Inputs::default();
        inputs.link_inputs("cc -Lfirst -Lsecond -lvalue -o program", dir.path(), &BTreeSet::new());
        let before = inputs.digest().unwrap();
        fs::write(dir.path().join("first/libvalue.a"), b"A").unwrap();
        assert_ne!(inputs.digest().unwrap(), before);
    }

    #[test]
    fn opaque_response_inputs_disable_reuse_without_refusing_the_link() {
        let dir = tempfile::tempdir().unwrap();
        let mut inputs = Inputs::default();
        inputs.link_inputs("cc -Wl,@native.rsp -o program", dir.path(), &BTreeSet::new());
        assert!(inputs.digest().is_err());
        let binary = dir.path().join("program");
        let pending = pending(binary.clone(), inputs);
        assert!(pending.changed());
        pending.commit().unwrap();
        assert!(!crate::lock(&LINKED_INPUTS).contains_key(&binary));
    }

    #[test]
    fn same_size_and_timestamp_different_archive_bytes_are_not_reused() {
        let dir = tempfile::tempdir().unwrap();
        let archive = dir.path().join("libgenerated.a");
        fs::write(&archive, b"first").unwrap();
        let stamp = fs::metadata(&archive).unwrap().modified().unwrap();
        let before = inputs(dir.path()).digest().unwrap();
        fs::write(&archive, b"other").unwrap();
        fs::File::options().write(true).open(&archive).unwrap().set_times(fs::FileTimes::new().set_modified(stamp)).unwrap();
        assert_ne!(inputs(dir.path()).digest().unwrap(), before);
        fs::write(&archive, b"first").unwrap();
        assert_eq!(inputs(dir.path()).digest().unwrap(), before);
    }

    #[test]
    fn deletion_recreation_and_directive_only_changes_invalidate() {
        let dir = tempfile::tempdir().unwrap();
        let archive = dir.path().join("native.a");
        let absent = inputs(&archive).digest().unwrap();
        fs::write(&archive, b"native").unwrap();
        let present = inputs(&archive).digest().unwrap();
        assert_ne!(absent, present);
        fs::remove_file(&archive).unwrap();
        assert_eq!(inputs(&archive).digest().unwrap(), absent);
        fs::write(&archive, b"native").unwrap();
        assert_eq!(inputs(&archive).digest().unwrap(), present);
        let mut changed = inputs(&archive);
        changed.directives.push(b"rustc-link-arg=-Wl,new-option".to_vec());
        assert_ne!(changed.digest().unwrap(), present);
    }

    #[test]
    fn failed_or_changed_links_do_not_commit_an_identity() {
        let dir = tempfile::tempdir().unwrap();
        let archive = dir.path().join("native.a");
        let binary = dir.path().join("program");
        fs::write(&archive, b"A").unwrap();
        let a = inputs(&archive).digest().unwrap();
        crate::lock(&LINKED_INPUTS).insert(binary.clone(), a);
        let failed = pending(binary.clone(), inputs(&archive));
        assert!(!failed.changed());
        drop(failed);
        assert!(!crate::lock(&LINKED_INPUTS).contains_key(&binary));
        let changed = pending(binary.clone(), inputs(&archive));
        assert!(changed.changed());
        fs::write(&archive, b"B").unwrap();
        assert!(changed.commit().is_err());
        assert!(!crate::lock(&LINKED_INPUTS).contains_key(&binary));
        fs::write(&archive, b"A").unwrap();
        let restored = pending(binary.clone(), inputs(&archive));
        assert!(restored.changed());
        assert!(!crate::lock(&LINKED_INPUTS).contains_key(&binary));
        restored.commit().unwrap();
        assert_eq!(crate::lock(&LINKED_INPUTS).remove(&binary), Some(a));
    }

    #[cfg(unix)]
    #[test]
    fn symlink_targets_and_cycles_are_not_silently_ignored() {
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a"), b"same").unwrap();
        fs::write(dir.path().join("b"), b"same").unwrap();
        let link = dir.path().join("link");
        symlink("a", &link).unwrap();
        let before = inputs(&link).digest().unwrap();
        fs::remove_file(&link).unwrap();
        symlink("b", &link).unwrap();
        assert_ne!(inputs(&link).digest().unwrap(), before);
        fs::remove_file(&link).unwrap();
        symlink(".", &link).unwrap();
        assert!(inputs(dir.path()).digest().is_err());
    }
}
