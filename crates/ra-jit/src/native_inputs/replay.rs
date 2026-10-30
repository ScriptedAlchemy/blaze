//! Admission of native linker reads against authenticated source, producer and platform trees.

use std::{
    collections::BTreeSet,
    ffi::{OsStr, OsString},
    path::{Path, PathBuf},
    sync::{Arc, LazyLock, Mutex},
};

use anyhow::{Context as _, bail, ensure};
use blaze_spec::{
    native::{Closure, Root},
    native_tree::{Kind, Tree},
};

static BINDINGS: LazyLock<Mutex<Option<Arc<Closure>>>> = LazyLock::new(|| Mutex::new(None));

pub(crate) fn set_replay_bindings(bytes: Option<&[u8]>) -> anyhow::Result<()> {
    let closure = bytes.map(Closure::from_bindings).transpose()?;
    *crate::lock(&BINDINGS) = closure.map(Arc::new);
    Ok(())
}

pub(crate) struct Guard {
    bindings: Arc<Closure>,
}

impl Guard {
    pub(crate) fn finish(self) -> anyhow::Result<()> {
        self.bindings.validate()
    }
}

pub(crate) fn before(
    before: &[OsString],
    after: &[OsString],
    env: &crate::link::Environment,
    apple: bool,
    generated: &[PathBuf],
) -> anyhow::Result<Option<Guard>> {
    let bindings = crate::lock(&BINDINGS).clone();
    if bindings.is_none() && !std::env::var("RA_JIT_SPEC_REPLAY").is_ok_and(|value| value == "1") {
        return Ok(None);
    }
    let bindings = bindings.context("saved replay has no authenticated native input bindings")?;
    bindings.validate()?;
    let mut closure = (*bindings).clone();
    for (at, path) in generated.iter().enumerate() {
        closure
            .roots
            .push(Root { name: format!("compiler-output/{at}"), tree: Tree::capture(path)? });
    }
    let cwd = std::env::current_dir()?;
    let args = before.iter().chain(after).cloned().collect();
    let mut collector = Collector::new(&closure, cwd, apple);
    if let Some(path) = env.get("LIBRARY_PATH") {
        let mut explicit: Vec<_> = std::env::split_paths(path).collect();
        explicit.append(&mut collector.search);
        collector.search = explicit;
    }
    collector.arguments(args)?;
    Ok(Some(Guard { bindings: Arc::new(closure) }))
}

enum LinkRead {
    Search(PathBuf),
    FrameworkSearch(PathBuf),
    Library(OsString),
    Framework(OsString),
    File(PathBuf, bool),
    Static(bool),
    Push,
    Pop,
}

struct Collector<'a> {
    bindings: &'a Closure,
    cwd: PathBuf,
    apple: bool,
    search: Vec<PathBuf>,
    frameworks: Vec<PathBuf>,
    active: BTreeSet<PathBuf>,
    seen: BTreeSet<PathBuf>,
}

impl<'a> Collector<'a> {
    fn new(bindings: &'a Closure, cwd: PathBuf, apple: bool) -> Self {
        let mut search = Vec::new();
        let mut frameworks = Vec::new();
        for root in &bindings.roots {
            if root.name.starts_with("linker/libraries/") {
                search.push(root.tree.root.clone());
            } else if root.name.starts_with("linker/frameworks/") {
                frameworks.push(root.tree.root.clone());
            } else if root.name == "sdk" {
                search
                    .extend([root.tree.root.join("usr/lib"), root.tree.root.join("usr/local/lib")]);
                frameworks.extend([
                    root.tree.root.join("System/Library/Frameworks"),
                    root.tree.root.join("Library/Frameworks"),
                ]);
            }
        }
        Self {
            bindings,
            cwd,
            apple,
            search,
            frameworks,
            active: BTreeSet::new(),
            seen: BTreeSet::new(),
        }
    }

    fn arguments(&mut self, args: Vec<OsString>) -> anyhow::Result<()> {
        let mut driver = Vec::new();
        self.expand_driver_responses(args, &mut driver, 0)?;
        let mut linker = Vec::new();
        let mut args = driver.into_iter();
        while let Some((arg, depth)) = args.next() {
            let bytes = arg.as_encoded_bytes();
            match bytes {
                b"-B" | b"-isysroot" | b"--sysroot" => {
                    let (value, _) = args.next().context("native driver flag needs a path")?;
                    self.bound(&self.cwd.join(value))?;
                }
                b"-arch" => {
                    args.next().context("native driver flag needs a value")?;
                }
                b"-L" | b"-F" | b"-l" | b"-framework" | b"-weak_framework" => {
                    linker.push(arg);
                    linker.push(args.next().context("native driver flag needs a value")?.0);
                }
                b"-Xlinker" => {
                    let (value, value_depth) = args.next().context("-Xlinker needs an argument")?;
                    self.expand_linker(vec![value], &mut linker, value_depth + 1)?;
                }
                b"-static" => linker.push("-Bstatic".into()),
                b"-nodefaultlibs" | b"-nostdlib" | b"-nostartfiles" | b"-pie" | b"-shared"
                | b"-dynamiclib" | b"-m64" | b"-m32" | b"-pthread" | b"-rdynamic" | b"-s" => {}
                _ if bytes.starts_with(b"-B") => {
                    self.bound(&self.cwd.join(native(&bytes[2..])?))?;
                }
                _ if bytes.starts_with(b"-Wl,") => {
                    let words = bytes[4..]
                        .split(|b| *b == b',')
                        .map(native)
                        .collect::<anyhow::Result<Vec<_>>>()?;
                    self.expand_linker(words, &mut linker, depth + 1)?;
                }
                _ if bytes.starts_with(b"-L")
                    || bytes.starts_with(b"-F")
                    || bytes.starts_with(b"-l")
                    || !bytes.starts_with(b"-") =>
                {
                    linker.push(arg)
                }
                _ if bytes.starts_with(b"-fuse-ld=") => {}
                _ => bail!("native driver input rule is not owned for {}", arg.to_string_lossy()),
            }
        }
        let reads = self.linker_arguments(linker)?;
        let mut search = Vec::new();
        let mut frameworks = Vec::new();
        for read in &reads {
            match read {
                LinkRead::Search(path) => search.push(path.clone()),
                LinkRead::FrameworkSearch(path) => frameworks.push(path.clone()),
                _ => {}
            }
        }
        search.append(&mut self.search);
        self.search = search;
        frameworks.append(&mut self.frameworks);
        self.frameworks = frameworks;
        let mut static_only = false;
        let mut modes = Vec::new();
        for read in reads {
            match read {
                LinkRead::Search(_) | LinkRead::FrameworkSearch(_) => {}
                LinkRead::Library(name) => self.library(&name, static_only)?,
                LinkRead::Framework(name) => self.framework(&name)?,
                LinkRead::File(path, inspect) => self.file(&path, inspect)?,
                LinkRead::Static(value) => static_only = value,
                LinkRead::Push => modes.push(static_only),
                LinkRead::Pop => {
                    static_only = modes.pop().context("native linker state stack is empty")?
                }
            }
        }
        ensure!(modes.is_empty(), "native linker state stack is not balanced");
        Ok(())
    }

    fn linker_arguments(&self, args: Vec<OsString>) -> anyhow::Result<Vec<LinkRead>> {
        let mut reads = Vec::new();
        let mut args = args.into_iter();
        while let Some(arg) = args.next() {
            let bytes = arg.as_encoded_bytes();
            let mut value = || args.next().context("native linker flag needs a value");
            match bytes {
                b"-L" => reads.push(LinkRead::Search(self.cwd.join(value()?))),
                b"-F" => reads.push(LinkRead::FrameworkSearch(self.cwd.join(value()?))),
                b"-l" => reads.push(LinkRead::Library(value()?)),
                b"-framework" | b"-weak_framework" => reads.push(LinkRead::Framework(value()?)),
                b"-Bstatic" | b"-static" => reads.push(LinkRead::Static(true)),
                b"-Bdynamic" => reads.push(LinkRead::Static(false)),
                b"--push-state" => reads.push(LinkRead::Push),
                b"--pop-state" => reads.push(LinkRead::Pop),
                b"-T" | b"--script" => reads.push(LinkRead::File(self.cwd.join(value()?), true)),
                b"--version-script"
                | b"--dynamic-list"
                | b"-exported_symbols_list"
                | b"-unexported_symbols_list"
                | b"-order_file" => {
                    reads.push(LinkRead::File(self.cwd.join(value()?), false));
                }
                b"-arch"
                | b"-z"
                | b"-u"
                | b"--undefined"
                | b"-rpath"
                | b"--rpath"
                | b"-soname"
                | b"--soname"
                | b"-install_name"
                | b"-compatibility_version"
                | b"-current_version"
                | b"--exclude-libs"
                | b"-headerpad" => {
                    value()?;
                }
                b"-isysroot" | b"--sysroot" => {
                    self.bound(&self.cwd.join(value()?))?;
                }
                b"-Bsymbolic"
                | b"-Bsymbolic-functions"
                | b"--as-needed"
                | b"--no-as-needed"
                | b"--whole-archive"
                | b"--no-whole-archive"
                | b"--gc-sections"
                | b"--eh-frame-hdr"
                | b"-dead_strip"
                | b"-s" => {}
                _ if bytes.starts_with(b"-L") => {
                    reads.push(LinkRead::Search(self.cwd.join(native(&bytes[2..])?)))
                }
                _ if bytes.starts_with(b"-F") => {
                    reads.push(LinkRead::FrameworkSearch(self.cwd.join(native(&bytes[2..])?)))
                }
                _ if bytes.starts_with(b"-l") => {
                    reads.push(LinkRead::Library(native(&bytes[2..])?))
                }
                _ if bytes.starts_with(b"--version-script=")
                    || bytes.starts_with(b"--dynamic-list=") =>
                {
                    let path = bytes.splitn(2, |b| *b == b'=').nth(1).unwrap();
                    reads.push(LinkRead::File(self.cwd.join(native(path)?), false));
                }
                _ if bytes.starts_with(b"--script=") => {
                    reads.push(LinkRead::File(self.cwd.join(native(&bytes[9..])?), true))
                }
                _ if !bytes.starts_with(b"-") => {
                    reads.push(LinkRead::File(self.cwd.join(arg), true))
                }
                _ => bail!("native linker input rule is not owned for {}", arg.to_string_lossy()),
            }
        }
        Ok(reads)
    }

    fn response(&mut self, path: &[u8]) -> anyhow::Result<Vec<OsString>> {
        let path = self.cwd.join(native(path)?);
        self.file(&path, false)?;
        let bytes = std::fs::read(&path)?;
        shlex::bytes::split(&bytes)
            .context("invalid native linker response file")?
            .into_iter()
            .map(|word| native(&word))
            .collect()
    }

    fn expand_driver_responses(
        &mut self,
        args: Vec<OsString>,
        out: &mut Vec<(OsString, usize)>,
        depth: usize,
    ) -> anyhow::Result<()> {
        ensure!(depth < 64, "native linker response nesting is too deep");
        for arg in args {
            if let Some(path) = arg.as_encoded_bytes().strip_prefix(b"@") {
                let words = self.response(path)?;
                self.expand_driver_responses(words, out, depth + 1)?;
            } else {
                out.push((arg, depth));
            }
        }
        Ok(())
    }

    fn expand_linker(
        &mut self,
        args: Vec<OsString>,
        out: &mut Vec<OsString>,
        depth: usize,
    ) -> anyhow::Result<()> {
        ensure!(depth < 64, "native linker response nesting is too deep");
        for arg in args {
            if let Some(path) = arg.as_encoded_bytes().strip_prefix(b"@") {
                let words = self.response(path)?;
                self.expand_linker(words, out, depth + 1)?;
            } else {
                out.push(arg);
            }
        }
        Ok(())
    }

    fn bound(&self, path: &Path) -> anyhow::Result<blaze_spec::native_tree::Entry> {
        self.bindings
            .entry(path)
            .with_context(|| format!("unbound native input {}", path.display()))
    }

    fn library(&mut self, name: &OsStr, static_only: bool) -> anyhow::Result<()> {
        let candidates: Vec<_> = if let Some(exact) = name.as_encoded_bytes().strip_prefix(b":") {
            vec![native(exact)?]
        } else {
            let suffixes: &[&str] = if static_only {
                &["a"]
            } else if self.apple {
                &["tbd", "dylib", "a"]
            } else {
                &["so", "a"]
            };
            suffixes
                .iter()
                .map(|suffix| {
                    let mut file = OsString::from("lib");
                    file.push(name);
                    file.push(format!(".{suffix}"));
                    file
                })
                .collect()
        };
        for directory in self.search.clone() {
            for file in &candidates {
                let path = directory.join(file);
                if matches!(self.bound(&path)?.kind, Kind::File { .. }) {
                    return self.file(&path, true);
                }
            }
        }
        bail!("native library {} is absent from the bound search trees", name.to_string_lossy())
    }

    fn framework(&mut self, name: &OsStr) -> anyhow::Result<()> {
        let mut bundle = name.to_owned();
        bundle.push(".framework");
        for directory in self.frameworks.clone() {
            let root = directory.join(&bundle);
            let mut stub = name.to_owned();
            stub.push(".tbd");
            for file in [OsString::from(name), stub] {
                let path = root.join(file);
                if matches!(self.bound(&path)?.kind, Kind::File { .. }) {
                    return self.file(&path, false);
                }
            }
        }
        bail!("native framework {} is absent from the bound search trees", name.to_string_lossy())
    }

    fn file(&mut self, path: &Path, inspect: bool) -> anyhow::Result<()> {
        let expected = self.bound(path)?;
        ensure!(
            matches!(expected.kind, Kind::File { .. }),
            "native input {} is not a bound file",
            path.display()
        );
        ensure!(!self.active.contains(path), "native input cycle at {}", path.display());
        if !self.seen.insert(path.to_owned()) {
            return Ok(());
        }
        self.active.insert(path.to_owned());
        let current = Tree::capture(path)?;
        self.bindings
            .admit(path, current.entry(Path::new("")).context("native observation is empty")?)?;
        if inspect {
            let bytes = std::fs::read(path)?;
            if bytes.starts_with(b"!<thin>\n") {
                let archive = object::read::archive::ArchiveFile::parse(bytes.as_slice())?;
                for member in archive.members() {
                    let member = member?;
                    ensure!(member.is_thin(), "opaque native thin archive member");
                    self.file(
                        &path.parent().unwrap_or(&self.cwd).join(native(member.name())?),
                        true,
                    )?;
                }
            } else if !super::native_binary(&bytes)
                && path.extension().is_none_or(|ext| ext != "tbd")
            {
                let text =
                    std::str::from_utf8(&bytes).context("native linker script is not UTF-8")?;
                for (input, include) in super::script_inputs(text)? {
                    if let Some(name) = input.strip_prefix("-l") {
                        self.library(OsStr::new(name), false)?;
                    } else if Path::new(&input).is_absolute() {
                        self.file(Path::new(&input), true)?;
                    } else {
                        ensure!(
                            !input.starts_with('=') && !input.starts_with("$SYSROOT"),
                            "native script sysroot input is not owned: {input}"
                        );
                        let mut dirs = Vec::new();
                        if !include {
                            dirs.push(path.parent().unwrap_or(&self.cwd).to_owned());
                        }
                        dirs.push(self.cwd.clone());
                        dirs.extend(self.search.clone());
                        let mut selected = None;
                        for dir in dirs {
                            let candidate = dir.join(&input);
                            if matches!(self.bound(&candidate)?.kind, Kind::File { .. }) {
                                selected = Some(candidate);
                                break;
                            }
                        }
                        self.file(
                            &selected.with_context(|| {
                                format!("native script input {input} is absent")
                            })?,
                            true,
                        )?;
                    }
                }
            }
        }
        self.active.remove(path);
        Ok(())
    }
}

fn native(bytes: &[u8]) -> anyhow::Result<OsString> {
    ensure!(!bytes.contains(&0), "native linker argument contains a NUL");
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStringExt;
        Ok(OsString::from_vec(bytes.to_vec()))
    }
    #[cfg(not(unix))]
    {
        Ok(String::from_utf8(bytes.to_vec())?.into())
    }
}

#[cfg(test)]
mod tests;
