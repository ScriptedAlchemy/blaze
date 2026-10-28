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
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Linked {
    inputs: Digest,
    output: Digest,
}

static LINKED_INPUTS: LazyLock<Mutex<BTreeMap<PathBuf, Linked>>> =
    LazyLock::new(|| Mutex::new(BTreeMap::new()));

#[cfg(unix)]
#[derive(Clone, Debug, PartialEq, Eq)]
struct OutputStamp {
    device: u64,
    inode: u64,
    size: u64,
    modified: (i64, i64),
    changed: (i64, i64),
}

#[cfg(unix)]
impl OutputStamp {
    fn of(metadata: &fs::Metadata) -> Self {
        use std::os::unix::fs::MetadataExt;
        Self {
            device: metadata.dev(),
            inode: metadata.ino(),
            size: metadata.len(),
            modified: (metadata.mtime(), metadata.mtime_nsec()),
            changed: (metadata.ctime(), metadata.ctime_nsec()),
        }
    }
}

#[cfg(unix)]
static OUTPUT_DIGESTS: LazyLock<Mutex<BTreeMap<PathBuf, (OutputStamp, Digest)>>> =
    LazyLock::new(|| Mutex::new(BTreeMap::new()));

#[derive(Default)]
struct Inputs {
    directives: Vec<Vec<u8>>,
    paths: BTreeSet<PathBuf>,
    native: BTreeSet<PathBuf>,
    bytes_only: BTreeSet<PathBuf>,
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
    output_changed: bool,
}

impl Pending {
    /// An untrackable input prevents reuse, but does not refuse a build the linker accepts.
    pub(crate) fn changed(&self) -> bool {
        self.changed
    }

    pub(crate) fn output_changed(&self) -> bool {
        self.output_changed
    }

    pub(crate) fn commit(self) -> anyhow::Result<()> {
        let Some(before) = self.before else { return Ok(()) };
        if self.inputs.digest().ok() != Some(before) {
            bail!("native link inputs changed while the executable was being linked");
        }
        let output = output_digest(&self.binary)?;
        crate::lock(&LINKED_INPUTS).insert(self.binary, Linked { inputs: before, output });
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
    let mut linked_crates = std::collections::HashSet::new();
    for root in &roots {
        let root: ide_db::base_db::Crate = (*root).into();
        for krate in root.transitive_deps(db) {
            if !linked_crates.insert(krate) {
                continue;
            }
            // Ordered, length-framed argv is a format, unlike a Debug rendering.
            let flags = &krate.data(db).codegen_flags;
            let mut encoded = Vec::new();
            encoded.extend_from_slice(&(flags.len() as u64).to_le_bytes());
            for flag in flags {
                encoded.extend_from_slice(&(flag.len() as u64).to_le_bytes());
                encoded.extend_from_slice(flag.as_bytes());
            }
            inputs.directives.push(encoded);
        }
    }
    for out in &out_dirs {
        if let Some(printed) = crate::cargo::script::printed(out) {
            inputs.paths.insert(printed);
        }
    }
    let cwd = std::env::current_dir()?;
    let sysroot = crate::SYSROOT.get().and_then(Option::as_deref);
    // Use the real native link builder, including attribute-declared libraries and ordering.
    // Rust objects and the daemon's mutable layout options are outputs, not native inputs.
    let (command, env) = crate::link::native_command(db, &roots, sysroot, binary)?;
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
        self.bytes_only.extend(bytes_only);
        self.bytes_only.extend(
            self.paths
                .iter()
                .filter(|path| path.extension().is_some_and(|ext| ext == "tbd"))
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
        program.wrote.is_some()
            && (now.is_none()
                || linked.is_none_or(|linked| {
                    Some(linked.inputs) != now || output_digest(&binary).ok() != Some(linked.output)
                }))
    })
}

pub(crate) fn invalidate(db: &RootDatabase, programs: &mut [Program]) {
    for program in programs {
        let Some((binary, inputs)) = collect(db, &program.args) else { continue };
        let now = inputs.digest().ok();
        let linked = crate::lock(&LINKED_INPUTS).get(&binary).copied();
        let inputs_same = linked.is_some_and(|linked| now == Some(linked.inputs));
        if inputs_same {
            if linked.is_some_and(|linked| output_digest(&binary).ok() == Some(linked.output)) {
                continue;
            }
            // Another worker may have published a different machine's executable to this
            // path. Retain compiled queries and objects: only publication/link reuse is lost.
            program.forget_link_baseline(Some(&binary));
            continue;
        }
        if program.wrote.is_none() {
            continue;
        }
        // The old identity must not become reusable after a failed A -> B -> A relink.
        crate::lock(&LINKED_INPUTS).remove(&binary);
        program.forget_link_baseline(Some(&binary));
        program.moved = true;
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
    let changed =
        before.is_none() || previous.is_none_or(|previous| Some(previous.inputs) != before);
    let output_changed =
        previous.is_some_and(|previous| output_digest(&binary).ok() != Some(previous.output));
    Pending { binary, inputs, before, changed, output_changed }
}

pub(crate) fn output_digest(path: &Path) -> io::Result<Digest> {
    #[cfg(unix)]
    {
        // A successful link can overlap the client's rename/chmod publication. Retry
        // that bounded transition; never cache a digest observed during a write.
        for _ in 0..8 {
            let stamp = OutputStamp::of(&fs::metadata(path)?);
            if let Some((cached, digest)) = crate::lock(&OUTPUT_DIGESTS).get(path)
                && *cached == stamp
            {
                return Ok(*digest);
            }
            let mut file = fs::File::open(path)?;
            if OutputStamp::of(&file.metadata()?) == stamp {
                let digest = hash_output(&mut file)?;
                if OutputStamp::of(&file.metadata()?) == stamp
                    && OutputStamp::of(&fs::metadata(path)?) == stamp
                {
                    let mut cache = crate::lock(&OUTPUT_DIGESTS);
                    if cache.len() >= 1024 && !cache.contains_key(path) {
                        cache.pop_first();
                    }
                    cache.insert(path.to_owned(), (stamp, digest));
                    return Ok(digest);
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        Err(io::Error::other("publication did not settle while hashing its artifact"))
    }
    #[cfg(not(unix))]
    {
        // Timestamps alone cannot certify unchanged content; retain full checking here.
        hash_output(&mut fs::File::open(path)?)
    }
}

fn hash_output(file: &mut fs::File) -> io::Result<Digest> {
    let mut hash = Sha256::new();
    let mut buffer = [0; 64 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hash.update(&buffer[..read]);
    }
    Ok(hash.finalize().into())
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
                !self.bytes_only.contains(path),
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
    inspect: bool,
) -> io::Result<()> {
    let meta = match fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            frame(hash, b"missing");
            return Ok(());
        }
        Err(error) => return Err(error),
    };
    let meta = if meta.file_type().is_symlink() {
        frame(hash, b"symlink");
        let target = fs::read_link(path)?;
        frame(hash, target.as_os_str().as_encoded_bytes());
        // Keep the path passed to the linker. A thin archive's relative members are
        // resolved beside that path, including when the archive itself is a symlink.
        match fs::metadata(path) {
            Ok(meta) => meta,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                frame(hash, b"missing-target");
                return Ok(());
            }
            Err(error) => return Err(error),
        }
    } else {
        meta
    };
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
        let mut thin = None;
        loop {
            let read = file.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            if first {
                first = false;
                let bytes = &buffer[..read];
                // Follow the content, not the basename: INPUT may name an extensionless
                // script, or a .so symlink may resolve to one. Ordinary binary archives and
                // objects stream; thin archives retain their table of external members.
                let binary = native_binary(bytes);
                if inspect && bytes.starts_with(b"!<thin>\n") {
                    thin = Some(Vec::new());
                }
                let textual = inspect
                    && !binary
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
            if let Some(thin) = &mut thin {
                thin.extend_from_slice(&buffer[..read]);
            }
            file_hash.update(&buffer[..read]);
        }
        frame(hash, &file_hash.finalize());
        if let Some(bytes) = thin {
            let archive = object::read::archive::ArchiveFile::parse(bytes.as_slice())
                .map_err(io::Error::other)?;
            let dir = path.parent().unwrap_or(cwd);
            for member in archive.members() {
                let member = member.map_err(io::Error::other)?;
                let name = member.name();
                if !member.is_thin() || name.is_empty() {
                    return Err(io::Error::other("native thin archive member is opaque"));
                }
                #[cfg(unix)]
                let name = {
                    use std::os::unix::ffi::OsStrExt as _;
                    Path::new(std::ffi::OsStr::from_bytes(name))
                };
                #[cfg(not(unix))]
                let name = Path::new(std::str::from_utf8(name).map_err(io::Error::other)?);
                frame(hash, b"thin-member");
                frame(hash, name.as_os_str().as_encoded_bytes());
                hash_path(&dir.join(name), hash, active, cwd, search, false, true)?;
            }
        }
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
                    hash_path(&candidate, hash, active, cwd, &dirs, true, true)?;
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
        hash_path(&entry.path(), hash, active, cwd, search, false, true)?;
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

    /// Exercise the real native command builder for Apple without invoking a linker or
    /// making the host's target or environment part of the fixture.
    fn apple_program(dir: &Path) -> (RootDatabase, Args) {
        use test_fixture::WithFixture;

        let out = dir.join("out");
        fs::create_dir(&out).unwrap();
        fs::write(out.join("libgenerated.a"), b"!<arch>\nA").unwrap();
        fs::write(
            dir.join("output"),
            format!("cargo:rustc-link-search=native={}\n", out.display()),
        )
        .unwrap();
        let (mut db, _) = RootDatabase::with_single_file(&format!(
            r#"
//- /main.rs crate:app cfg:target_vendor=apple,target_arch=aarch64 env:OUT_DIR={}
#[link(name = "generated", kind = "static")]
unsafe extern "C" {{ fn generated(); }}
fn main() {{}}
"#,
            out.display()
        ));
        ide_db::base_db::set_process_env(&mut db, Default::default());
        let args = Args::parse(
            [
                dir.display().to_string(),
                "--crate".to_owned(),
                "app".to_owned(),
                "--out".to_owned(),
                dir.join("base.o").display().to_string(),
                "--patch".to_owned(),
                dir.join("patch.o").display().to_string(),
                "--exe".to_owned(),
                dir.join("program").display().to_string(),
            ]
            .into_iter(),
        )
        .unwrap();
        (db, args)
    }

    #[test]
    fn apple_headerpad_growth_preserves_native_input_identity() {
        let dir = tempfile::tempdir().unwrap();
        let (db, args) = apple_program(dir.path());
        let binary = args.exe.as_ref().unwrap();
        let headerpad = crate::cargo::plan::headerpad_file(binary);
        let inputs = collect_known(&db, &args, binary).unwrap();
        let before = inputs.digest().expect("Apple native inputs are known before linking");
        assert!(!headerpad.exists(), "collecting inputs must not write link outputs");
        let link = pending(binary.clone(), inputs);
        assert!(link.changed());

        let roots = crate::crates_named(&db, &args.krate, &args).unwrap();
        let command = crate::link::command(&db, &roots, &[], None, binary).unwrap();
        let words = shlex::split(&command).unwrap();
        assert!(words.iter().any(|word| word == "-Wl,-no_fixup_chains"));
        assert!(words.contains(&format!("@{}", headerpad.display())));
        assert!(headerpad.exists(), "the actual link still writes its layout options");
        crate::cargo::plan::write_headerpad(binary, 0x800000).unwrap();
        fs::write(binary, b"linked program").unwrap();
        link.commit().expect("layout can grow during a successful link");
        assert_eq!(collect_known(&db, &args, binary).unwrap().digest().unwrap(), before);
        let unchanged = begin(&db, &args).unwrap();
        assert!(!unchanged.changed(), "layout output does not force a native relink");
        assert!(!unchanged.output_changed());
        unchanged.commit().unwrap();
        crate::lock(&LINKED_INPUTS).remove(binary);
    }

    #[test]
    fn apple_native_libraries_and_user_layout_flags_still_invalidate() {
        let dir = tempfile::tempdir().unwrap();
        let (db, args) = apple_program(dir.path());
        let binary = args.exe.as_ref().unwrap();
        fs::write(binary, b"linked program").unwrap();
        let initial = collect_known(&db, &args, binary).unwrap();
        let before = initial.digest().unwrap();
        pending(binary.clone(), initial).commit().unwrap();

        fs::write(dir.path().join("out/libgenerated.a"), b"!<arch>\nB").unwrap();
        let changed = begin(&db, &args).unwrap();
        assert!(changed.changed(), "attribute-declared native libraries remain inputs");
        assert_ne!(changed.before, Some(before));
        changed.commit().unwrap();

        let native = collect_known(&db, &args, binary).unwrap().digest().unwrap();
        fs::write(
            dir.path().join("output"),
            format!(
                "cargo:rustc-link-search=native={}\ncargo:rustc-link-arg=-Wl,-headerpad,0x200000\n",
                dir.path().join("out").display()
            ),
        )
        .unwrap();
        let inputs = collect_known(&db, &args, binary).unwrap();
        assert!(
            inputs
                .directives
                .iter()
                .any(|line| { String::from_utf8_lossy(line).contains("-Wl,-headerpad,0x200000") })
        );
        assert_ne!(inputs.digest().unwrap(), native, "user layout flags remain inputs");
        let changed = pending(binary.clone(), inputs);
        assert!(changed.changed());
        changed.commit().unwrap();
        crate::lock(&LINKED_INPUTS).remove(binary);
    }

    #[test]
    fn apple_user_response_files_remain_opaque_even_at_the_owned_headerpad_path() {
        let dir = tempfile::tempdir().unwrap();
        let (db, args) = apple_program(dir.path());
        let binary = args.exe.as_ref().unwrap();
        for response in [dir.path().join("native.rsp"), crate::cargo::plan::headerpad_file(binary)]
        {
            fs::write(&response, "-Wl,-headerpad,0x100000\n").unwrap();
            fs::write(
                dir.path().join("output"),
                format!("cargo:rustc-link-arg=@{}\n", response.display()),
            )
            .unwrap();
            let inputs = collect_known(&db, &args, binary).unwrap();
            assert!(inputs.digest().is_err(), "user response grammars remain unknown");
            let link = pending(binary.clone(), inputs);
            assert!(link.changed());
            link.commit().unwrap();
            assert!(!crate::lock(&LINKED_INPUTS).contains_key(binary));
        }
    }

    fn inputs(path: &Path) -> Inputs {
        Inputs {
            directives: vec![b"rustc-link-lib=static=generated".to_vec()],
            paths: BTreeSet::from([path.to_owned()]),
            opaque: false,
            ..Inputs::default()
        }
    }

    fn thin_archive(member: &[u8]) -> Vec<u8> {
        let mut names = member.to_vec();
        names.extend_from_slice(b"/\n");
        if names.len() % 2 != 0 {
            names.push(b'\n');
        }
        let mut archive = format!(
            "!<thin>\n{:<16}{:<12}{:<6}{:<6}{:<8}{:<10}`\n",
            "//",
            "",
            "",
            "",
            "",
            names.len()
        )
        .into_bytes();
        archive.extend_from_slice(&names);
        archive.extend_from_slice(b"/0              0           0     0     644     8         `\n");
        archive
    }

    #[test]
    fn thin_archive_members_are_native_inputs_even_when_the_archive_is_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("objects")).unwrap();
        let archive = dir.path().join("libgenerated.a");
        let member = dir.path().join("objects/generated.o");
        let bytes = thin_archive(b"objects/generated.o");
        fs::write(&archive, &bytes).unwrap();
        fs::write(&member, b"\x7fELFAAAA").unwrap();
        let stamp = fs::metadata(&member).unwrap().modified().unwrap();
        let before = inputs(&archive).digest().unwrap();
        fs::write(&member, b"\x7fELFBBBB").unwrap();
        fs::File::options().write(true).open(&member).unwrap().set_modified(stamp).unwrap();
        assert_eq!(fs::read(&archive).unwrap(), bytes);
        assert_ne!(inputs(&archive).digest().unwrap(), before);
        fs::write(&member, b"\x7fELFAAAA").unwrap();
        assert_eq!(inputs(&archive).digest().unwrap(), before);
        fs::remove_file(&member).unwrap();
        assert_ne!(inputs(&archive).digest().unwrap(), before);
    }

    #[cfg(unix)]
    #[test]
    fn thin_archive_members_resolve_beside_the_linked_symlink() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let actual = dir.path().join("actual");
        let alias = dir.path().join("alias");
        fs::create_dir(&actual).unwrap();
        fs::create_dir(&alias).unwrap();
        fs::write(actual.join("libgenerated.a"), thin_archive(b"generated.o")).unwrap();
        fs::write(actual.join("generated.o"), b"\x7fELFAAAA").unwrap();
        fs::write(alias.join("generated.o"), b"\x7fELFBBBB").unwrap();
        let archive = alias.join("libgenerated.a");
        symlink("../actual/libgenerated.a", &archive).unwrap();
        let before = inputs(&archive).digest().unwrap();
        fs::write(alias.join("generated.o"), b"\x7fELFCCCC").unwrap();
        assert_ne!(inputs(&archive).digest().unwrap(), before);
        fs::write(alias.join("generated.o"), b"\x7fELFBBBB").unwrap();
        fs::write(actual.join("generated.o"), b"\x7fELFDDDD").unwrap();
        assert_eq!(inputs(&archive).digest().unwrap(), before);
    }

    #[test]
    fn malformed_or_cyclic_thin_archives_do_not_certify_reuse() {
        let dir = tempfile::tempdir().unwrap();
        let archive = dir.path().join("libgenerated.a");
        fs::write(&archive, b"!<thin>\nbroken header").unwrap();
        assert!(inputs(&archive).digest().is_err());
        fs::write(&archive, thin_archive(b"other.a")).unwrap();
        fs::write(dir.path().join("other.a"), thin_archive(b"libgenerated.a")).unwrap();
        assert!(inputs(&archive).digest().is_err());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn thin_archive_member_paths_preserve_non_utf8_bytes() {
        use std::os::unix::ffi::OsStrExt as _;

        let dir = tempfile::tempdir().unwrap();
        let name = b"generated-\xff.o";
        let member = dir.path().join(std::ffi::OsStr::from_bytes(name));
        let archive = dir.path().join("libgenerated.a");
        fs::write(&archive, thin_archive(name)).unwrap();
        fs::write(&member, b"\x7fELFAAAA").unwrap();
        let before = inputs(&archive).digest().unwrap();
        fs::write(&member, b"\x7fELFBBBB").unwrap();
        assert_ne!(inputs(&archive).digest().unwrap(), before);
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
        fs::write(dir.path().join("exports.map"), "{ global: INPUT_entry; local: *; };").unwrap();
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
        fs::write(&binary, b"linked A").unwrap();
        let a = inputs(&archive).digest().unwrap();
        let linked = Linked { inputs: a, output: output_digest(&binary).unwrap() };
        crate::lock(&LINKED_INPUTS).insert(binary.clone(), linked);
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
        assert_eq!(crate::lock(&LINKED_INPUTS).remove(&binary), Some(linked));
    }

    #[test]
    fn another_workers_output_is_not_the_registered_machine_publication() {
        let dir = tempfile::tempdir().unwrap();
        let archive = dir.path().join("native.a");
        let binary = dir.path().join("program");
        fs::write(&archive, b"A").unwrap();
        fs::write(&binary, b"+machine").unwrap();
        pending(binary.clone(), inputs(&archive)).commit().unwrap();
        let registered = crate::lock(&LINKED_INPUTS).get(&binary).copied().unwrap();
        let stamp = fs::metadata(&binary).unwrap().modified().unwrap();
        fs::write(&binary, b"-machine").unwrap();
        fs::File::options().write(true).open(&binary).unwrap().set_modified(stamp).unwrap();
        let link = pending(binary.clone(), inputs(&archive));
        assert!(!link.changed(), "the native and machine inputs did not change");
        assert!(link.output_changed(), "equal size/mtime cannot bless another executable");
        fs::write(&binary, b"+machine").unwrap();
        link.commit().unwrap();
        assert_eq!(crate::lock(&LINKED_INPUTS).remove(&binary), Some(registered));
    }

    #[cfg(unix)]
    #[test]
    fn unchanged_large_publications_reuse_the_digest_without_reading_the_file() {
        use std::io::Write;
        let dir = tempfile::tempdir().unwrap();
        let binary = dir.path().join("large-program");
        let mut file = fs::File::create(&binary).unwrap();
        let chunk = vec![0x5a; 1024 * 1024];
        for _ in 0..201 {
            file.write_all(&chunk).unwrap();
        }
        drop(file);
        let mut old_ms = Vec::new();
        let mut new_ms = Vec::new();
        let expected = output_digest(&binary).unwrap();
        for _ in 0..5 {
            let start = std::time::Instant::now();
            for _ in 0..3 {
                assert_eq!(hash_output(&mut fs::File::open(&binary).unwrap()).unwrap(), expected);
            }
            old_ms.push(start.elapsed().as_secs_f64() * 1000.0);
            let start = std::time::Instant::now();
            for _ in 0..3 {
                assert_eq!(output_digest(&binary).unwrap(), expected);
            }
            new_ms.push(start.elapsed().as_secs_f64() * 1000.0);
        }
        eprintln!(
            "publication no-op 201MiB, 3 identity checks, n=5: uncached_ms={old_ms:?}; cached_ms={new_ms:?}"
        );
        let stamp = OutputStamp::of(&fs::metadata(&binary).unwrap());
        assert_eq!(crate::lock(&OUTPUT_DIGESTS).get(&binary).unwrap().0, stamp);
        assert!(new_ms.iter().sum::<f64>() < old_ms.iter().sum::<f64>());
    }

    #[cfg(unix)]
    #[test]
    fn publication_cache_rehashes_renamed_and_same_timestamp_outputs() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("output");
        fs::write(&path, b"one").unwrap();
        let first = output_digest(&path).unwrap();
        let old = OutputStamp::of(&fs::metadata(&path).unwrap());
        let time = fs::metadata(&path).unwrap().modified().unwrap();
        fs::write(&path, b"two").unwrap();
        fs::File::options().write(true).open(&path).unwrap().set_modified(time).unwrap();
        assert_ne!(OutputStamp::of(&fs::metadata(&path).unwrap()), old);
        assert_ne!(output_digest(&path).unwrap(), first);
        let replacement = dir.path().join("replacement");
        fs::write(&replacement, b"one").unwrap();
        fs::File::options().write(true).open(&replacement).unwrap().set_modified(time).unwrap();
        fs::rename(replacement, &path).unwrap();
        assert_ne!(OutputStamp::of(&fs::metadata(&path).unwrap()).inode, old.inode);
        assert_eq!(output_digest(&path).unwrap(), first);
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
