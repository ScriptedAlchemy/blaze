//! The files of a package as cargo 1.99 lists them for the fingerprint of a build script that
//! names nothing it reads: `PathSource::list_files` and `last_modified_file` of
//! `src/sources/path.rs` in rust-lang/cargo at 5f94df478.
//!
//! With no `package.include`, a package whose `Cargo.toml` the index of a git repository
//! around it tracks is listed from git: the files it tracks there, and those it does not that
//! git does not ignore, but none in `target/` git does not track and no untracked `Cargo.lock`.
//! Otherwise the directory is walked, following links, without its root's `target` and, when
//! there is no `include`, without any file or directory whose name starts with a dot. Either
//! way a directory with a `Cargo.toml` of its own is another package and left out, and every
//! file is then kept or left out by `package.include` or `package.exclude`, matched as lines of
//! a `.gitignore` are; `Cargo.toml` and `Cargo.lock` at the root are always kept. Nothing is
//! left out for a `CACHEDIR.TAG`, which cargo does not read.
//!
//! git's lists are what cargo's `gix` walk gives (`emit_tracked`, untracked files that match,
//! no ignored ones): `git ls-files --cached --others --exclude-standard`. git compiles no Rust.

use std::{
    io,
    path::{Path, PathBuf},
    process::Command,
    time::SystemTime,
};

/// The files of the package at `root` whose manifest has `include` and `exclude`, as cargo
/// lists them.
pub fn list_files(root: &Path, include: &[String], exclude: &[String]) -> io::Result<Vec<PathBuf>> {
    let no_include = include.is_empty();
    let repo = if no_include { git_repo(root) } else { None };
    let mut exclude_rules = Rules::default();
    if no_include && repo.is_none() {
        // rust-lang/cargo#7183: outside git, no hidden file.
        exclude_rules.add(".*");
    }
    for rule in exclude {
        exclude_rules.add(rule);
    }
    let mut include_rules = Rules::default();
    for rule in include {
        include_rules.add(rule);
    }
    let filter = |path: &Path, is_dir: bool| {
        let Ok(relative) = path.strip_prefix(root) else { return false };
        if relative == Path::new("Cargo.lock") || relative == Path::new("Cargo.toml") {
            return true;
        }
        if no_include {
            !exclude_rules.matched_path_or_any_parents(relative, is_dir)
        } else {
            // Include rules do not list directories: only files are matched.
            is_dir || include_rules.matched_path_or_any_parents(relative, false)
        }
    };
    let mut files = Vec::new();
    match repo {
        Some(_) => list_git(root, root, &filter, &mut files)?,
        None => walk(root, &mut files, true, &filter)?,
    }
    Ok(files)
}

/// The newest of `files`, as cargo's `last_modified_file` finds it: the greatest time a file
/// was written, the first of those with that time, and the epoch when there is none. A file
/// that cannot be read is of the epoch.
pub fn last_modified(files: &[PathBuf]) -> (SystemTime, PathBuf) {
    let mut newest = (SystemTime::UNIX_EPOCH, PathBuf::new());
    for file in files {
        let written =
            std::fs::metadata(file).and_then(|it| it.modified()).unwrap_or(SystemTime::UNIX_EPOCH);
        if written > newest.0 {
            newest = (written, file.clone());
        }
    }
    newest
}

/// The root of the git repository around `root` whose index tracks `root/Cargo.toml`, which
/// is when cargo lists the package from git (`discover_gix_repo`).
fn git_repo(root: &Path) -> Option<PathBuf> {
    let top = git(root, &["rev-parse", "--show-toplevel"])?;
    let top = git_path(top.strip_suffix(b"\n").unwrap_or(&top)).ok()?;
    git(root, &["ls-files", "--error-unmatch", "--", "Cargo.toml"])?;
    Some(top)
}

/// `git -z` writes filenames as bytes. Unix paths need not be UTF-8, and dropping one would
/// omit an input from a no-directive script's fingerprint.
fn git_path(bytes: &[u8]) -> io::Result<PathBuf> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt as _;
        Ok(PathBuf::from(std::ffi::OsStr::from_bytes(bytes)))
    }
    #[cfg(not(unix))]
    {
        std::str::from_utf8(bytes)
            .map(PathBuf::from)
            .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))
    }
}

/// What `git -C dir args` prints, when it succeeds.
fn git(dir: &Path, args: &[&str]) -> Option<Vec<u8>> {
    let mut command = Command::new("git");
    command.arg("-C").arg(dir).args(args);
    // A variable that points git at another repository or index is not the checkout's.
    for name in ["GIT_DIR", "GIT_WORK_TREE", "GIT_INDEX_FILE"] {
        command.env_remove(name);
    }
    let output = command.stdin(std::process::Stdio::null()).output().ok()?;
    output.status.success().then_some(output.stdout)
}

/// `list_files_gix`: the files of the package at `package` that git lists in `root`, its
/// directory or a repository inside it.
fn list_git(
    package: &Path,
    root: &Path,
    filter: &dyn Fn(&Path, bool) -> bool,
    files: &mut Vec<PathBuf>,
) -> io::Result<()> {
    let args = ["ls-files", "-z", "-t", "--cached", "--others", "--exclude-standard", "--", "."];
    let listed = git(root, &args)
        .ok_or_else(|| io::Error::other(format!("git could not list {}", root.display())))?;
    let mut packages: Vec<PathBuf> = Vec::new();
    let mut seen = std::collections::BTreeSet::new();
    for entry in listed.split(|it| *it == 0).filter(|it| !it.is_empty()) {
        if entry.get(1) != Some(&b' ') {
            continue;
        }
        let tracked = entry[0] != b'?';
        let name = &entry[2..];
        let at = git_path(name.strip_suffix(b"/").unwrap_or(name))?;
        // `target/` is left out of the walk, and what the index tracks there is added back.
        let at_root = root == package;
        if !tracked && at_root && (at.starts_with("target") || at == Path::new("Cargo.lock")) {
            continue;
        }
        let path = root.join(&at);
        if !seen.insert(path.clone()) {
            continue;
        }
        if path != package.join("Cargo.toml")
            && path.file_name().is_some_and(|it| it == "Cargo.toml")
        {
            let dir = path.parent().unwrap_or(&path).to_owned();
            files.retain(|it| !it.starts_with(&dir));
            packages.push(dir);
            continue;
        }
        if packages.iter().any(|it| path.starts_with(it)) {
            continue;
        }
        if path.is_dir() {
            // A repository inside, or a link to a directory: walked with git when it is one.
            let top = git(&path, &["rev-parse", "--show-toplevel"]);
            let top = top.and_then(|it| git_path(it.strip_suffix(b"\n").unwrap_or(&it)).ok());
            let same =
                |a: &Path, b: &Path| std::fs::canonicalize(a).ok() == std::fs::canonicalize(b).ok();
            match top {
                Some(top) if same(&top, &path) => list_git(package, &path, filter, files)?,
                _ => walk(&path, files, false, filter)?,
            }
        } else if filter(&path, false) {
            files.push(path);
        }
    }
    Ok(())
}

/// `list_files_walk`: the files below `path`, following links, depth first.
fn walk(
    path: &Path,
    files: &mut Vec<PathBuf>,
    is_root: bool,
    filter: &dyn Fn(&Path, bool) -> bool,
) -> io::Result<()> {
    let mut visited: Vec<PathBuf> = Vec::new();
    walk_from(path, files, is_root, 0, filter, &mut visited)
}

fn walk_from(
    path: &Path,
    files: &mut Vec<PathBuf>,
    is_root: bool,
    depth: usize,
    filter: &dyn Fn(&Path, bool) -> bool,
    visited: &mut Vec<PathBuf>,
) -> io::Result<()> {
    let at_root = is_root && depth == 0;
    let metadata = match std::fs::metadata(path) {
        Ok(it) => it,
        // walkdir yields what it cannot read, filtered again; it is a file of the epoch.
        Err(_) if depth > 0 => {
            if filter(path, false) {
                files.push(path.to_owned());
            }
            return Ok(());
        }
        Err(err) => return Err(err),
    };
    let is_dir = metadata.is_dir();
    if !at_root && !filter(path, is_dir) {
        return Ok(());
    }
    if !is_dir {
        files.push(path.to_owned());
        return Ok(());
    }
    if !at_root && path.join("Cargo.toml").exists() {
        return Ok(());
    }
    if is_root && depth == 1 && path.file_name().is_some_and(|it| it == "target") {
        return Ok(());
    }
    // walkdir stops at a link back to a directory it is in.
    let real = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_owned());
    if visited.contains(&real) {
        return Ok(());
    }
    visited.push(real);
    let mut entries: Vec<PathBuf> = match std::fs::read_dir(path) {
        Ok(it) => it.filter_map(|it| Some(it.ok()?.path())).collect(),
        Err(_) => {
            visited.pop();
            return Ok(());
        }
    };
    entries.sort();
    for entry in entries {
        walk_from(&entry, files, is_root, depth + 1, filter, visited)?;
    }
    visited.pop();
    Ok(())
}

/// Lines of a `.gitignore`, as the `ignore` crate's `Gitignore` that cargo builds matches
/// them: the last that matches decides, `!` turns one into a match that keeps.
#[derive(Default)]
struct Rules {
    rules: Vec<Rule>,
}

struct Rule {
    pattern: Vec<String>,
    negated: bool,
    dir_only: bool,
    /// Matched against the whole path from the root, not only a name.
    anchored: bool,
}

impl Rules {
    fn add(&mut self, line: &str) {
        let mut line = line.trim_end();
        if line.is_empty() || line.starts_with('#') {
            return;
        }
        let negated = line.starts_with('!');
        if negated {
            line = &line[1..];
        }
        if let Some(rest) = line.strip_prefix('\\') {
            line = rest;
        }
        let dir_only = line.ends_with('/');
        let line = line.trim_end_matches('/');
        let anchored = line.trim_start_matches('/').contains('/') || line.starts_with('/');
        let line = line.trim_start_matches('/');
        if line.is_empty() {
            return;
        }
        let pattern = line.split('/').map(str::to_owned).collect();
        self.rules.push(Rule { pattern, negated, dir_only, anchored });
    }

    /// Whether `path`, or a directory it is in, is matched and not kept by a later `!`.
    fn matched_path_or_any_parents(&self, path: &Path, is_dir: bool) -> bool {
        let parts: Vec<String> =
            path.components().map(|it| it.as_os_str().to_string_lossy().into_owned()).collect();
        for end in (1..=parts.len()).rev() {
            let dir = end < parts.len() || is_dir;
            if let Some(ignored) = self.matched(&parts[..end], dir) {
                // The path itself, or the nearest directory around it that a rule decides.
                return ignored;
            }
        }
        false
    }

    fn matched(&self, parts: &[String], is_dir: bool) -> Option<bool> {
        let mut decided = None;
        for rule in &self.rules {
            if rule.dir_only && !is_dir {
                continue;
            }
            let hit = if rule.anchored {
                segments(&rule.pattern, parts)
            } else {
                parts.last().is_some_and(|name| glob(&rule.pattern[0], name))
            };
            if hit {
                decided = Some(!rule.negated);
            }
        }
        decided
    }
}

/// Whether the path segments `parts` match the pattern's, `**` being any number of them.
fn segments(pattern: &[String], parts: &[String]) -> bool {
    match pattern.split_first() {
        None => parts.is_empty(),
        Some((first, rest)) if first == "**" => {
            (0..=parts.len()).any(|skip| segments(rest, &parts[skip..]))
        }
        Some((first, rest)) => parts
            .split_first()
            .is_some_and(|(part, others)| glob(first, part) && segments(rest, others)),
    }
}

/// Whether `name` matches the glob `pattern` of one segment: `*`, `?`, `[...]` and `\`.
fn glob(pattern: &str, name: &str) -> bool {
    let pattern: Vec<char> = pattern.chars().collect();
    let name: Vec<char> = name.chars().collect();
    glob_at(&pattern, &name)
}

fn glob_at(pattern: &[char], name: &[char]) -> bool {
    match pattern.split_first() {
        None => name.is_empty(),
        Some(('*', rest)) => (0..=name.len()).any(|skip| glob_at(rest, &name[skip..])),
        Some(('?', rest)) => !name.is_empty() && glob_at(rest, &name[1..]),
        Some(('[', rest)) => {
            let Some(close) = rest.iter().skip(1).position(|it| *it == ']').map(|it| it + 1) else {
                return name.first() == Some(&'[') && glob_at(rest, &name[1..]);
            };
            let Some(&c) = name.first() else { return false };
            let (negate, class) = match rest[0] {
                '!' | '^' => (true, &rest[1..close]),
                _ => (false, &rest[..close]),
            };
            let mut hit = false;
            let mut at = 0;
            while at < class.len() {
                if at + 2 < class.len() && class[at + 1] == '-' {
                    hit |= class[at] <= c && c <= class[at + 2];
                    at += 3;
                } else {
                    hit |= class[at] == c;
                    at += 1;
                }
            }
            hit != negate && glob_at(&rest[close + 1..], &name[1..])
        }
        Some(('\\', rest)) if !rest.is_empty() => {
            name.first() == Some(&rest[0]) && glob_at(&rest[1..], &name[1..])
        }
        Some((c, rest)) => name.first() == Some(c) && glob_at(rest, &name[1..]),
    }
}

#[cfg(test)]
mod tests;
