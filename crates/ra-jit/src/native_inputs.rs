//! Native outputs of build scripts are inputs to a link, not just to Rust name resolution.
//!
//! Keep the last successfully linked identity, never the last observed one. Generated
//! archives may change without changing a directive, Rust source, size, or timestamp.
//!
//! Adapted from `.github/repairs/pr12/native_inputs.rs` of the branch `repair/pr12-native-inputs`
//! (7fc32028, the cloud lane's prepared module, read through Git and never applied): the identity
//! (a digest of the link line and of the content of each file it can name, absent ones too), the
//! pending-then-commit of a link, and the tests. What differs: a closure that cannot be collected
//! (a crate not loaded, a link line that cannot be built) is opaque, which links in full and refuses
//! nothing, where the prepared module failed the request.

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
    native: BTreeSet<PathBuf>,
    search: BTreeSet<PathBuf>,
    cwd: PathBuf,
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

/// The inputs of the link of the program `args` asks for, when blaze links it itself (a whole build's
/// `--exe`). One that cannot be said is opaque: the program is linked in full and not reused.
fn collect(db: &RootDatabase, args: &Args) -> Option<(PathBuf, Inputs)> {
    args.exe.as_ref()?;
    let binary = args.splice.as_ref().or(args.exe.as_ref())?;
    match collect_known(db, args, binary) {
        Ok(inputs) => Some((binary.clone(), inputs)),
        Err(error) => {
            eprintln!("native link inputs: not known ({error:#}): the program is linked in full");
            Some((binary.clone(), Inputs { opaque: true, ..Inputs::default() }))
        }
    }
}

fn collect_known(db: &RootDatabase, args: &Args, binary: &Path) -> anyhow::Result<Inputs> {
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
    let env = crate::link::Environment::of(db);
    inputs.link_inputs_env(&command, &cwd, &out_dirs, &env);
    for name in ["LIBRARY_PATH", "SDKROOT", "MACOSX_DEPLOYMENT_TARGET", "PATH"] {
        let mut field = name.as_bytes().to_vec();
        match env.get(name) {
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
    Ok(inputs)
}

impl Inputs {
    /// Track named native candidates, not every file in a search directory. Missing earlier
    /// candidates matter too: creating one can change which library the linker selects.
    #[cfg(test)]
    fn link_inputs(&mut self, command: &str, cwd: &Path, out_dirs: &BTreeSet<PathBuf>) {
        let env = crate::link::Environment::of(&RootDatabase::default());
        self.link_inputs_env(command, cwd, out_dirs, &env);
    }

    fn link_inputs_env(
        &mut self,
        command: &str,
        cwd: &Path,
        out_dirs: &BTreeSet<PathBuf>,
        env: &crate::link::Environment,
    ) {
        self.cwd = cwd.to_owned();
        let already = self.paths.clone();
        let mut bytes_only = BTreeSet::new();
        self.directives.push(command.as_bytes().to_vec());
        let Some(words) = shlex::split(command) else {
            self.opaque = true;
            return;
        };
        let words: Vec<String> = words
            .into_iter()
            .skip(1)
            .flat_map(|word| match word.strip_prefix("-Wl,") {
                Some(rest) => rest.split(',').map(str::to_owned).collect(),
                None => vec![word],
            })
            .collect();
        let mut search = out_dirs.clone();
        if let Some(paths) = env.get("LIBRARY_PATH") {
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
                    if next().is_none() {
                        self.opaque = true;
                    }
                }
                "-L" | "-F" => match next() {
                    Some(path) if word == "-L" => {
                        search.insert(cwd.join(path));
                    }
                    Some(path) => {
                        frameworks.insert(cwd.join(path));
                    }
                    None => self.opaque = true,
                },
                "-l" => match next() {
                    Some(name) => {
                        libraries.insert(name);
                    }
                    None => self.opaque = true,
                },
                "-framework" | "-weak_framework" => match next() {
                    Some(name) => {
                        framework_names.insert(name);
                    }
                    None => self.opaque = true,
                },
                "-T"
                | "--script"
                | "--version-script"
                | "--dynamic-list"
                | "-exported_symbols_list"
                | "-unexported_symbols_list"
                | "-order_file" => match next() {
                    Some(path) => {
                        let path = cwd.join(path);
                        if !matches!(word.as_str(), "-T" | "--script") {
                            bytes_only.insert(path.clone());
                        }
                        self.paths.insert(path);
                        // A full linker script can change the linker's implicit search,
                        // sysroot and output rules. Until those rules are interpreted in
                        // full, it links legally but does not advertise a reusable key.
                        self.opaque |= matches!(word.as_str(), "-T" | "--script");
                    }
                    None => self.opaque = true,
                },
                _ if word.starts_with("-L") && word.len() > 2 => {
                    search.insert(cwd.join(&word[2..]));
                }
                _ if word.starts_with("-F") && word.len() > 2 => {
                    frameworks.insert(cwd.join(&word[2..]));
                }
                _ if word.starts_with("-l") && word.len() > 2 => {
                    libraries.insert(word[2..].to_owned());
                }
                _ if word.starts_with('@') => {
                    self.paths.insert(cwd.join(&word[1..]));
                    // Linkers have different response grammars. Do not guess their input
                    // closure: this invocation links in full and does not advertise reuse.
                    self.opaque = true;
                }
                _ => {
                    let file = word
                        .split_once('=')
                        .filter(|(flag, _)| {
                            matches!(*flag, "--script" | "--version-script" | "--dynamic-list")
                        })
                        .map(|(_, path)| path);
                    if let Some(file) = file {
                        let path = cwd.join(file);
                        if !word.starts_with("--script=") {
                            bytes_only.insert(path.clone());
                        }
                        self.paths.insert(path);
                        self.opaque |= word.starts_with("--script=");
                    } else if !word.starts_with('-') {
                        let path = cwd.join(word);
                        let native = path.extension().is_some_and(|ext| {
                            matches!(
                                ext.to_str(),
                                Some("a" | "o" | "so" | "dylib" | "lib" | "obj" | "ld" | "tbd")
                            )
                        });
                        if native || path.is_file() {
                            self.paths.insert(path);
                        }
                    }
                }
            }
        }
        self.search = search.clone();
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
        self.native.extend(
            self.paths
                .difference(&already)
                .filter(|path| {
                    !bytes_only.contains(*path) && path.extension().is_none_or(|ext| ext != "tbd")
                })
                .cloned(),
        );
    }
}

pub(crate) fn stale(db: &RootDatabase, programs: &[Program]) -> bool {
    programs.iter().any(|program| {
        let Some((binary, inputs)) = collect(db, &program.args) else { return false };
        // A program that was never linked by this worker has nothing to be stale against.
        let now = inputs.digest().ok();
        let linked = crate::lock(&LINKED_INPUTS).get(&binary).copied();
        program.wrote.is_some() && (now.is_none() || linked != now)
    })
}

pub(crate) fn invalidate(db: &RootDatabase, programs: &mut [Program]) {
    for program in programs {
        let Some((binary, inputs)) = collect(db, &program.args) else { continue };
        let now = inputs.digest().ok();
        if now.is_some() && crate::lock(&LINKED_INPUTS).get(&binary).copied() == now {
            continue;
        }
        if program.wrote.is_none() {
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
}

/// Prevent a native change from being installed as a Rust-only splice.
pub(crate) fn forget_binary(binary: &Path) {
    crate::splicer::claim(binary, true);
    crate::splicer::forget(binary);
    crate::lock(&crate::LINKED).remove(binary);
}

pub(crate) fn begin(db: &RootDatabase, args: &Args) -> Option<Pending> {
    collect(db, args).map(|(binary, inputs)| pending(binary, inputs))
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
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "native input closure is not known",
            ));
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
            hash_path(
                path,
                &mut hash,
                &mut BTreeSet::new(),
                &self.cwd,
                &self.search,
                self.native.contains(path),
            )?;
        }
        Ok(hash.finalize().into())
    }
}

fn frame(hash: &mut Sha256, bytes: &[u8]) {
    hash.update((bytes.len() as u64).to_le_bytes());
    hash.update(bytes);
}

fn hash_path(
    path: &Path,
    hash: &mut Sha256,
    active: &mut BTreeSet<PathBuf>,
    cwd: &Path,
    search: &BTreeSet<PathBuf>,
    script: bool,
) -> io::Result<()> {
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
            Ok(target) => return hash_path(&target, hash, active, cwd, search, script),
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
        let real = path.canonicalize()?;
        if !active.insert(real.clone()) {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "native input script cycle"));
        }
        frame(hash, b"file");
        let mut file_hash = Sha256::new();
        let mut file = fs::File::open(path)?;
        let mut buffer = [0; 64 * 1024];
        let mut first = true;
        let mut text = None;
        loop {
            let read = file.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            if first {
                first = false;
                let bytes = &buffer[..read];
                // Follow the content, not the basename: INPUT may name an extensionless
                // script, or a .so symlink may resolve to one. Binary archives/objects
                // keep the single streaming pass and never allocate a whole-file buffer.
                let binary = native_binary(bytes);
                let textual = !binary
                    && (script
                        || path
                            .extension()
                            .is_some_and(|ext| matches!(ext.to_str(), Some("so" | "ld")))
                        || ["INPUT", "GROUP", "INCLUDE"].iter().any(|word| {
                            bytes.windows(word.len()).any(|bytes| bytes == word.as_bytes())
                        }));
                if textual {
                    text = Some(Vec::new());
                }
            }
            if let Some(text) = &mut text {
                text.extend_from_slice(&buffer[..read]);
            }
            file_hash.update(&buffer[..read]);
        }
        frame(hash, &file_hash.finalize());
        if let Some(bytes) = text {
            let text = std::str::from_utf8(&bytes).map_err(io::Error::other)?;
            let dependencies = script_inputs(text)?;
            let mut dirs = search.clone();
            dirs.insert(cwd.to_owned());
            if let Some(parent) = path.parent() {
                dirs.insert(parent.to_owned());
            }
            for (dependency, _) in dependencies {
                let mut candidates = BTreeSet::new();
                if dependency.starts_with('=') || dependency.starts_with("$SYSROOT") {
                    return Err(io::Error::other("native linker script sysroot is opaque"));
                }
                if let Some(name) = dependency.strip_prefix("-l") {
                    for dir in &dirs {
                        if let Some(exact) = name.strip_prefix(':') {
                            candidates.insert(dir.join(exact));
                        } else {
                            for suffix in ["so", "a", "dylib", "tbd"] {
                                candidates.insert(dir.join(format!("lib{name}.{suffix}")));
                            }
                            candidates.insert(dir.join(format!("{name}.lib")));
                        }
                    }
                } else if Path::new(&dependency).is_absolute() {
                    candidates.insert(PathBuf::from(&dependency));
                } else {
                    candidates.extend(dirs.iter().map(|dir| dir.join(&dependency)));
                }
                frame(hash, dependency.as_bytes());
                for candidate in candidates {
                    frame(hash, candidate.as_os_str().as_encoded_bytes());
                    hash_path(&candidate, hash, active, cwd, &dirs, true)?;
                }
            }
        }
        active.remove(&real);
        return Ok(());
    }
    if !meta.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "native input is not a regular file or directory",
        ));
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
        hash_path(&entry.path(), hash, active, cwd, search, false)?;
    }
    active.remove(&real);
    Ok(())
}

fn native_binary(bytes: &[u8]) -> bool {
    bytes.starts_with(b"\x7fELF")
        || bytes.starts_with(b"!<arch>\n")
        || bytes.starts_with(b"!<thin>\n")
        || bytes.get(..4).is_some_and(|magic| {
            matches!(
                magic,
                b"\xfe\xed\xfa\xce"
                    | b"\xce\xfa\xed\xfe"
                    | b"\xfe\xed\xfa\xcf"
                    | b"\xcf\xfa\xed\xfe"
                    | b"\xca\xfe\xba\xbe"
                    | b"\xbe\xba\xfe\xca"
            )
        })
        || bytes.starts_with(b"MZ")
        || bytes.get(..2).is_some_and(|machine| {
            matches!(machine, b"\x64\x86" | b"\x4c\x01" | b"\x64\xaa" | b"\0\0")
        })
}

/// The dependency-only GNU ld script grammar. Other commands can change linker search
/// or layout and are deliberately opaque, not guessed. Quoted paths and comments follow
/// ld's token boundaries; an incomplete script never provides a reusable key.
fn script_inputs(text: &str) -> io::Result<Vec<(String, bool)>> {
    let bad = || io::Error::other("native linker script grammar is opaque");
    let mut tokens = Vec::new();
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            ch if ch.is_whitespace() || ch == ',' || ch == ';' => {}
            '(' | ')' => tokens.push(ch.to_string()),
            '/' if chars.peek() == Some(&'*') => {
                chars.next();
                let mut previous = '\0';
                loop {
                    let next = chars.next().ok_or_else(bad)?;
                    if previous == '*' && next == '/' {
                        break;
                    }
                    previous = next;
                }
            }
            '"' => {
                let mut token = String::new();
                loop {
                    match chars.next().ok_or_else(bad)? {
                        '"' => break,
                        ch => token.push(ch),
                    }
                }
                tokens.push(token);
            }
            ch => {
                let mut token = ch.to_string();
                while chars
                    .peek()
                    .is_some_and(|ch| !ch.is_whitespace() && !matches!(ch, '(' | ')' | ',' | ';'))
                {
                    token.push(chars.next().unwrap());
                }
                tokens.push(token);
            }
        }
    }
    let mut inputs = Vec::new();
    let mut at = 0;
    while at < tokens.len() {
        match tokens[at].as_str() {
            "INCLUDE" => {
                at += 1;
                let file = tokens.get(at).ok_or_else(bad)?;
                if matches!(file.as_str(), "(" | ")") {
                    return Err(bad());
                }
                inputs.push((file.clone(), true));
                at += 1;
            }
            "INPUT" | "GROUP" => {
                at += 1;
                if tokens.get(at).map(String::as_str) != Some("(") {
                    return Err(bad());
                }
                at += 1;
                let mut depth = 1;
                while depth != 0 {
                    let token = tokens.get(at).ok_or_else(bad)?;
                    at += 1;
                    match token.as_str() {
                        ")" => depth -= 1,
                        "AS_NEEDED" => {
                            if tokens.get(at).map(String::as_str) != Some("(") {
                                return Err(bad());
                            }
                            at += 1;
                            depth += 1;
                        }
                        "(" => return Err(bad()),
                        file => inputs.push((file.to_owned(), false)),
                    }
                }
            }
            _ => return Err(bad()),
        }
    }
    if inputs.is_empty() {
        return Err(bad());
    }
    Ok(inputs)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inputs(path: &Path) -> Inputs {
        Inputs {
            directives: vec![b"rustc-link-lib=static=generated".to_vec()],
            paths: BTreeSet::from([path.to_owned()]),
            opaque: false,
            ..Inputs::default()
        }
    }

    #[test]
    fn a_search_directory_does_not_hash_the_link_outputs() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("libgenerated.a"), b"!<arch>\nnative").unwrap();
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
        fs::write(dir.path().join("second/libvalue.a"), b"!<arch>\nB").unwrap();
        let mut inputs = Inputs::default();
        inputs.link_inputs("cc -Lfirst -Lsecond -lvalue -o program", dir.path(), &BTreeSet::new());
        let before = inputs.digest().unwrap();
        fs::write(dir.path().join("first/libvalue.a"), b"!<arch>\nA").unwrap();
        assert_ne!(inputs.digest().unwrap(), before);
    }

    #[test]
    fn linker_scripts_hash_input_group_and_included_archive_closures() {
        let dir = tempfile::tempdir().unwrap();
        let proxy = dir.path().join("libproxy.so");
        let archive = dir.path().join("impl.a");
        let include = dir.path().join("inner");
        fs::write(&archive, b"!<arch>\nA").unwrap();
        for script in
            ["INPUT(impl.a)", "/* GNU ld script */ GROUP( AS_NEEDED(impl.a) )", "INCLUDE inner"]
        {
            fs::write(&proxy, script).unwrap();
            fs::write(&include, "INPUT(impl.a)").unwrap();
            let tracked = inputs(&proxy);
            let before = tracked.digest().unwrap();
            fs::write(&archive, b"!<arch>\nB").unwrap();
            assert_ne!(tracked.digest().unwrap(), before, "{script}");
            fs::write(&archive, b"!<arch>\nA").unwrap();
            assert_eq!(tracked.digest().unwrap(), before, "{script}");
        }
        fs::write(&include, "INPUT(other.a)").unwrap();
        let tracked = inputs(&proxy);
        let before = tracked.digest().unwrap();
        fs::write(dir.path().join("other.a"), b"!<arch>\nC").unwrap();
        assert_ne!(tracked.digest().unwrap(), before);
        fs::write(&include, "INCLUDE libproxy.so").unwrap();
        assert!(tracked.digest().is_err(), "an included script cycle cannot be reused");
        fs::write(&proxy, "SEARCH_DIR(\"elsewhere\") INPUT(impl.a)").unwrap();
        assert!(tracked.digest().is_err(), "uninterpreted search rules are opaque");
    }

    #[test]
    fn native_search_and_identity_use_the_request_environment() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a");
        let b = dir.path().join("b");
        fs::create_dir(&a).unwrap();
        fs::create_dir(&b).unwrap();
        fs::write(a.join("libchoice.a"), b"!<arch>\nA").unwrap();
        fs::write(b.join("libchoice.a"), b"!<arch>\nB").unwrap();
        let mut db = RootDatabase::default();
        let request = |path: &Path| {
            [("LIBRARY_PATH".to_owned(), path.display().to_string())].into_iter().collect()
        };
        ide_db::base_db::set_process_env(&mut db, request(&a));
        let first = crate::link::Environment::of(&db);
        ide_db::base_db::set_process_env(&mut db, request(&b));
        let second = crate::link::Environment::of(&db);
        let mut input_a = Inputs::default();
        input_a.link_inputs_env("cc -lchoice -o program", dir.path(), &BTreeSet::new(), &first);
        let mut input_b = Inputs::default();
        input_b.link_inputs_env("cc -lchoice -o program", dir.path(), &BTreeSet::new(), &second);
        assert!(input_a.paths.contains(&a.join("libchoice.a")));
        assert!(!input_a.paths.contains(&b.join("libchoice.a")));
        assert!(input_b.paths.contains(&b.join("libchoice.a")));
        assert!(!input_b.paths.contains(&a.join("libchoice.a")));
        assert_ne!(input_a.digest().unwrap(), input_b.digest().unwrap());
    }

    #[cfg(unix)]
    #[test]
    fn extensionless_input_scripts_and_symlink_targets_have_transitive_identity() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let proxy = dir.path().join("libproxy.so");
        let inner = dir.path().join("inner");
        let archive = dir.path().join("impl.a");
        fs::write(&inner, "INPUT(impl.a)").unwrap();
        fs::write(&archive, b"!<arch>\nA").unwrap();
        for linked in [false, true] {
            if linked {
                fs::remove_file(&proxy).unwrap();
                symlink("inner", &proxy).unwrap();
            } else {
                fs::write(&proxy, "GROUP(inner)").unwrap();
            }
            let before = inputs(&proxy).digest().unwrap();
            fs::write(&archive, b"!<arch>\nB").unwrap();
            assert_ne!(inputs(&proxy).digest().unwrap(), before);
            fs::write(&archive, b"!<arch>\nA").unwrap();
            assert_eq!(inputs(&proxy).digest().unwrap(), before);
        }
    }

    #[test]
    fn linker_maps_and_text_stubs_are_byte_inputs_not_dependency_scripts() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("exports.map"), "{ global: entry; local: *; };").unwrap();
        fs::write(dir.path().join("libchoice.tbd"), "--- !tapi-tbd\nexports: []\n").unwrap();
        let mut inputs = Inputs::default();
        inputs.link_inputs(
            "cc -L. -lchoice -Wl,--version-script=exports.map -o program",
            dir.path(),
            &BTreeSet::new(),
        );
        let before = inputs.digest().unwrap();
        fs::write(dir.path().join("exports.map"), "{ global: other; local: *; };").unwrap();
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
        fs::File::options()
            .write(true)
            .open(&archive)
            .unwrap()
            .set_times(fs::FileTimes::new().set_modified(stamp))
            .unwrap();
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
