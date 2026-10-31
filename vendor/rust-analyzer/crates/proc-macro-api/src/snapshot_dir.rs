//! The explicit, worker-owned snapshot layout, shared by allocation and post-wait cleanup.

use std::{
    fs, io,
    io::Write,
    path::{Path, PathBuf},
};

pub fn worker_dir(root: &Path, pid: u32) -> PathBuf {
    root.join("macro-snapshots").join(pid.to_string())
}

fn plain_dir(path: &Path) -> bool {
    fs::symlink_metadata(path)
        .is_ok_and(|metadata| metadata.is_dir() && !metadata.file_type().is_symlink())
}

pub(crate) fn prepare(root: &Path, pid: u32) -> io::Result<PathBuf> {
    let dir = worker_dir(root, pid);
    // Reject existing symlinks before create_dir_all/set_permissions follows them.
    for ancestor in dir.ancestors().take(3) {
        if fs::symlink_metadata(ancestor).is_ok_and(|metadata| metadata.file_type().is_symlink()) {
            return Err(io::Error::other("symlink in owned macro snapshot directory"));
        }
    }
    fs::create_dir_all(&dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700))?;
    }
    let marker = dir.join("owner");
    match fs::File::options().write(true).create_new(true).open(&marker) {
        Ok(mut file) => write!(file, "{pid}")?,
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            if !fs::symlink_metadata(&marker)?.is_file()
                || fs::symlink_metadata(&marker)?.file_type().is_symlink()
                || fs::read_to_string(&marker)? != pid.to_string()
            {
                return Err(io::Error::other("macro snapshot owner mismatch"));
            }
        }
        Err(error) => return Err(error),
    }
    Ok(dir)
}

/// Call only after killing and waiting for the worker and its process group.
/// Foreign/unmarked/symlink directories are not removed.
pub fn remove_after_worker_exit(root: &Path, pid: u32) -> io::Result<()> {
    let dir = worker_dir(root, pid);
    let marker = dir.join("owner");
    if plain_dir(root)
        && plain_dir(&root.join("macro-snapshots"))
        && plain_dir(&dir)
        && fs::symlink_metadata(&marker)
            .is_ok_and(|metadata| metadata.is_file() && !metadata.file_type().is_symlink())
        && fs::read_to_string(&marker).ok().as_deref() == Some(&pid.to_string())
    {
        fs::remove_dir_all(dir)?;
    }
    Ok(())
}
