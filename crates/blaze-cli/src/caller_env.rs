//! One command's explicit build environment. Only the home-private authentication tag is
//! passed on as a cache input. Neither the key nor the caller's values are producer records.

use std::{
    fs::{self, File, OpenOptions}, io::{self, Read, Write},
    os::unix::fs::{MetadataExt, OpenOptionsExt}, path::{Path, PathBuf},
};
use core::fmt;

use blaze_build_script::env::CallerEnv;
use blaze_ids::{ContentId, Hasher};
use blaze_int::Int;
use blaze_sys::posix::{io::{OpenFlags, getentropy}, thread::geteuid};
use bstd::{digest::HmacSha256, env::{Env, VarName, VarNameError}, mem::Frame};

const KEY_NAME: &str = ".blaze-build-env-key";

#[derive(Debug)]
pub enum Error {
    Denied(VarName),
    BadName(VarNameError),
    Home,
    Key { path: PathBuf, source: io::Error },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Denied(name) => write!(f, "--build-env cannot read {name} from this request"),
            Self::BadName(error) => write!(f, "--build-env: {error}"),
            Self::Home => f.write_str("--build-env requires an absolute HOME for its private key"),
            Self::Key { path, source } => write!(f, "--build-env private key {}: {source}", path.display()),
        }
    }
}

fn read_key(path: &Path) -> io::Result<[u8; 32]> {
    let flags = OpenFlags::NONBLOCK.with(OpenFlags::NO_FOLLOW);
    let mut file = OpenOptions::new().read(true).custom_flags(flags.bits().cast_signed()).open(path)?;
    let meta = file.metadata()?;
    if !meta.is_file() || meta.uid() != geteuid() || meta.mode() & 0o077 != 0 || meta.len() != 32 {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "expected a private 32-byte regular file owned by this user"));
    }
    let mut key = [0; 32];
    file.read_exact(&mut key)?;
    Ok(key)
}

fn key(home: &Path) -> Result<[u8; 32], Error> {
    let path = home.join(KEY_NAME);
    match read_key(&path) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => publish_key(home, &path),
        found => found,
    }.map_err(|source| Error::Key { path, source })
}

fn publish_key(home: &Path, path: &Path) -> io::Result<[u8; 32]> {
    // Each contender publishes a complete key with link(2). A loser reads the installed key.
    // The temporary name uses independent entropy and reveals none of the key.
    let mut key = [0; 32];
    let mut nonce = [0; 16];
    let entropy = |buf: &mut [u8]| getentropy(buf).map_err(|e| io::Error::other(format!("getentropy: {e:?}")));
    entropy(&mut key)?;
    entropy(&mut nonce)?;
    let temp = home.join(format!(".blaze-build-env-{:032x}.tmp", u128::from_le_bytes(nonce)));
    let mut file = OpenOptions::new().write(true).create_new(true).mode(0o600).open(&temp)?;
    let publish = (|| {
        file.write_all(&key)?;
        file.sync_all()?;
        match fs::hard_link(&temp, path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Ok(()),
            Err(e) => Err(e),
        }
    })();
    fs::remove_file(&temp)?;
    publish?;
    File::open(home)?.sync_all()?;
    read_key(path)
}

fn present(key: &[u8; 32], name: &str, value: &str) -> ContentId {
    let mut h = HmacSha256::new(key);
    h.update(b"blaze caller build environment value 1\0");
    h.update(&name.len().widen_to::<u64>().to_le_bytes()).update(name.as_bytes()).update(value.as_bytes());
    ContentId(h.finish())
}

pub(crate) fn project<'a>(names: &[&'a str], env: Option<&'a Env>, frame: &'a Frame) -> Result<CallerEnv<'a>, crate::CliError> {
    if names.is_empty() {
        return Ok(CallerEnv::default());
    }
    let read = |name: &str| {
        let name = VarName::new(name).map_err(|e| crate::CliError::BuildEnv(Error::BadName(e)))?;
        let Some(env) = env else { return Err(crate::CliError::BuildEnv(Error::Denied(name))) };
        env.get(&name).map_err(|e| crate::CliError::BuildEnv(Error::Denied(e.name)))
    };
    let home = read("HOME")?.map(Path::new).filter(|path| path.is_absolute()).ok_or(crate::CliError::BuildEnv(Error::Home))?;
    let key = key(home).map_err(crate::CliError::BuildEnv)?;
    let mut h = Hasher::new("blaze workspace build environment projection 1");
    let mut rows = blaze_own::Vec::with_capacity_in(names.len(), frame);
    for &name in names {
        let value = read(name)?;
        let fact = value.map_or_else(ContentId::default, |v| present(&key, name, v));
        h.part(name.as_bytes()).id(&fact);
        rows.push((name, value, fact));
    }
    CallerEnv::new(rows.into_bump_slice(), h.finish()).ok_or(crate::CliError::Usage("invalid --build-env grant"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use bstd::env::Vars;
    use blaze_sys::posix::{io::{CWD, openat}, proc::change_mode};

    #[test]
    fn caller_grants_read_only_named_values_and_key_absence_separately_from_empty() {
        let home = blaze_core::temp::TempDir::for_test("caller-env-project");
        let path = home.path().to_str().expect("UTF-8 home");
        let vars = Vars::from_pairs_in(&[("HOME", path), ("App", ""), ("SECRET", "unrelated-private-value")], blaze_own::testing::heap().shared());
        let env = Env::from_vars(vars).only(&["HOME", "App", "MISSING"].map(|name| VarName::new(name).expect("variable")));
        let frame = Frame::new(bstd::mem::Tag::Other).expect("frame");
        let default = project(&[], None, &frame).expect("no capability read or key created");
        assert!(home.path().read_dir().expect("empty home").next().is_none());
        let grant = project(&["App", "MISSING"], Some(&env), &frame).expect("never reads SECRET");
        let stable = project(&["App", "MISSING"], Some(&env), &frame).expect("reopens the same key");
        assert_eq!(grant.identity(), stable.identity());
        assert!(grant.iter().eq([("App", Some("")), ("MISSING", None)]));
        assert_eq!(grant.answer("MISSING"), blaze_drive::key::ABSENT);
        assert_eq!(grant.answer("SECRET"), default.answer("SECRET"));
        assert_ne!(grant.answer("App"), grant.answer("MISSING"));
        let worker = grant.copy_in(&frame);
        assert_eq!(worker.identity(), grant.identity());
        assert_eq!(worker.rows(), grant.rows());
        assert!(project(&["SECRET"], Some(&env), &frame).is_err());
        let key_path = home.join(KEY_NAME);
        assert_eq!(key_path.metadata().expect("key").mode() & 0o777, 0o600);
        let key = read_key(&key_path).expect("key");
        assert_ne!(present(&key, "A", "bc"), present(&key, "Ab", "c"));
        assert_ne!(present(&key, "App", ""), present(&key, "App", "first"));
        assert_ne!(present(&key, "App", "first"), present(&[0; 32], "App", "first"));
        for reserved in ["HOME", "PATH", "OUT_DIR", "CARGO_PKG_NAME", "DYLD_LIBRARY_PATH", "LD_PRELOAD", "RUSTC", "BLAZE_STORE", "CC", "NUM_JOBS"] {
            assert!(!CallerEnv::allows(reserved), "{reserved}");
        }
    }

    #[test]
    fn concurrent_first_grants_open_one_complete_private_key_and_reject_other_files() {
        let home = blaze_core::temp::TempDir::for_test("caller-env-key");
        let path = home.join(KEY_NAME);
        blaze_dist_local::sys::scope(blaze_own::testing::heap().shared(), |workers| {
            // All four publish, even if one installs its key before another gets CPU time.
            let handles = [(); 4].map(|()| workers.spawn(|| publish_key(home.path(), &path)).expect("spawn key contender"));
            let keys = handles.map(|h| h.join().expect("joined").expect("complete private key"));
            let installed = key(home.path()).expect("installed key");
            assert_eq!(keys, [installed; 4]);
        });
        assert_eq!(home.path().read_dir().expect("key home").count(), 1);
        let linked = blaze_core::temp::TempDir::for_test("caller-env-link");
        std::os::unix::fs::symlink(&path, linked.join(KEY_NAME)).expect("link to a valid private key");
        assert!(key(linked.path()).is_err());
        let mut buf = [0; 4096];
        let name = blaze_sys::Path::in_buffer(path.as_os_str().as_encoded_bytes(), &mut buf).expect("key path");
        let fd = openat(CWD, name, OpenFlags::READ.with(OpenFlags::CLOSE_ON_EXEC), 0).expect("open private key");
        change_mode(fd.fd(), 0o644).expect("plant public key");
        assert!(key(home.path()).is_err());
    }
}
