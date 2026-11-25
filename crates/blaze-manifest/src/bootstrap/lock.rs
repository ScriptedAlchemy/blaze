//! Build-program dependency pins, traversed once under the caller's source corrections.

use blaze_own::{AVec, AString, Alloc, Vec};

use crate::lock::{Checksum, Lock, LockId, LockSource};

#[derive(Clone, Copy, Debug)]
pub enum Refused {
    NotLocked,
    Unpinned { package: usize },
    OutsideLock { package: usize },
}

/// Sorted `name version pin` lines for the requested roots and their transitive closure.
/// `None` takes every direct dependency; a list selects only those build dependencies.
pub fn closure_lines<'a, P: AsRef<str>>(
    lock: &Lock<'_>, name: &str, version: &str, build_dependencies: Option<&[&str]>,
    alloc: Alloc<'a>, pin: impl FnMut(&str, &str, &str) -> P,
) -> Result<AVec<'a, AString<'a>>, Refused> {
    let root = lock.named(name).find(|id| blaze_own::format_in!(alloc, "{}", lock.package(*id).version).as_str() == version).ok_or(Refused::NotLocked)?;
    closure_at(lock, root, build_dependencies, &[], alloc, pin)
}

/// A source snapshot matched to one path package in this exact lock. Its digest contains no
/// checkout path and includes workspace inheritance through bootstrap::path_source.
#[derive(Clone, Copy)]
pub struct PathPin {
    pub package: LockId,
    pub files: super::HexDigest,
}

/// Exact lock entries read by the closure, including its root. Callers capture path sources
/// for these entries before hashing, rather than snapshotting unrelated workspace packages.
pub fn closure_packages<'a>(lock: &Lock<'_>, root: LockId, build_dependencies: Option<&[&str]>, alloc: Alloc<'a>) -> Result<AVec<'a, LockId>, Refused> {
    let mut seen = Vec::new_in(alloc);
    seen.resize(lock.packages.len(), false);
    *seen.get_mut(root.index()).ok_or(Refused::OutsideLock { package: root.index() })? = true;
    let mut stack = Vec::new_in(alloc);
    let mut found = Vec::new_in(alloc);
    stack.push(root);
    found.push(root);
    while let Some(at) = stack.pop() {
        for &dep in &lock.package(at).deps {
            let package = lock.packages.get(dep.index()).ok_or(Refused::OutsideLock { package: dep.index() })?;
            if at == root && build_dependencies.is_some_and(|names| !names.contains(&package.name.as_str())) { continue; }
            let visited = seen.get_mut(dep.index()).ok_or(Refused::OutsideLock { package: dep.index() })?;
            if !*visited { *visited = true; stack.push(dep); found.push(dep); }
        }
    }
    Ok(found)
}

/// The root's exact package ID avoids selecting an equal-name/version registry package.
/// A path dependency must have a captured source pin; no machine path enters its identity.
pub fn closure_at<'a, P: AsRef<str>>(
    lock: &Lock<'_>, root: LockId, build_dependencies: Option<&[&str]>, paths: &[PathPin],
    alloc: Alloc<'a>, mut pin: impl FnMut(&str, &str, &str) -> P,
) -> Result<AVec<'a, AString<'a>>, Refused> {
    let packages = closure_packages(lock, root, build_dependencies, alloc)?;
    let mut lines = Vec::new_in(alloc);
    for id in packages {
        if id == root { continue; }
        let at = id.index();
        let package = lock.package(id);
        if matches!(package.source, LockSource::Path) {
            let source = paths.iter().find(|path| path.package == id).ok_or(Refused::Unpinned { package: at })?;
            lines.push(blaze_own::format_in!(alloc, "{} {} files {}", package.name, package.version, source.files.as_str()));
            continue;
        }
        let version = blaze_own::format_in!(alloc, "{}", package.version);
        let raw = match (&package.checksum, &package.source) {
            (Checksum::Sha256(sum), _) => sum.as_str(),
            (Checksum::Absent, LockSource::Path) => return Err(Refused::Unpinned { package: at }),
            (Checksum::Absent, LockSource::Registry { .. } | LockSource::Git { .. }) => package.source_text.as_str(),
        };
        let corrected = pin(package.name.as_str(), version.as_str(), raw);
        lines.push(blaze_own::format_in!(alloc, "{} {} {}", package.name, version, corrected.as_ref()));
    }
    lines.sort_unstable();
    Ok(lines)
}

/// SHA-256 of the sorted lines, with one newline between lines and no trailing newline.
pub fn digest(lines: &[AString<'_>]) -> super::HexDigest {
    let mut hash = blaze_sha256::Sha256::new();
    for (at, line) in lines.iter().enumerate() {
        if at != 0 { hash.update(b"\n"); }
        hash.update(line.as_bytes());
    }
    super::HexDigest::from_sha256(hash.finish())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_pins_are_matched_to_exact_lock_packages() {
        let alloc = blaze_own::testing::heap().alloc();
        let text = b"version = 4\n[[package]]\nname = \"app\"\nversion = \"0.1.0\"\nsource = \"registry+https://example.invalid/index\"\nchecksum = \"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\"\n[[package]]\nname = \"app\"\nversion = \"0.1.0\"\ndependencies = [\"cc\"]\n[[package]]\nname = \"cc\"\nversion = \"0.1.0\"\n";
        let lock = Lock::read(text, alloc).expect("lock");
        let root = lock.named("app").find(|id| matches!(lock.package(*id).source, LockSource::Path)).expect("path app");
        let registry = lock.named("app").find(|id| matches!(lock.package(*id).source, LockSource::Registry { .. })).expect("registry app");
        let cc = lock.named("cc").next().expect("cc");
        let pins = [PathPin { package: cc, files: crate::bootstrap::HexDigest::of(b"cc sources") }];
        let lines = closure_at(&lock, root, Some(&["cc"]), &pins, alloc, |_, _, pin| blaze_own::String::from_str_in(pin, alloc)).expect("source-pinned closure");
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].as_str(), blaze_own::format_in!(alloc, "cc 0.1.0 files {}", pins[0].files.as_str()).as_str());
        assert!(closure_at(&lock, registry, None, &[], alloc, |_, _, pin| blaze_own::String::from_str_in(pin, alloc)).expect("registry root").is_empty());
        assert!(matches!(closure_at(&lock, root, Some(&["cc"]), &[], alloc, |_, _, pin| blaze_own::String::from_str_in(pin, alloc)), Err(Refused::Unpinned { package }) if package == cc.index()));
        let wrong = [PathPin { package: root, files: pins[0].files }];
        assert!(matches!(closure_at(&lock, root, Some(&["cc"]), &wrong, alloc, |_, _, pin| blaze_own::String::from_str_in(pin, alloc)), Err(Refused::Unpinned { package }) if package == cc.index()));
    }
}
