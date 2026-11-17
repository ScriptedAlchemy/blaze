//! Ownership of unpublished files, including across cancellation of an HTTP request.

use std::{
    fs::File,
    path::{Path, PathBuf},
};

use crate::error::Error;

/// Only a successful creator owns cleanup. A failed create must not unlink another writer's
/// staging file. The file precedes this guard in the returned tuple so cancellation also
/// closes it before trying to remove it on Windows.
pub(super) struct Temporary {
    path: PathBuf,
    lease: Option<File>,
}

impl Temporary {
    pub(super) fn create(path: PathBuf) -> std::io::Result<(File, Self)> {
        let mut options = File::options();
        options.read(true).write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600);
        }
        let file = options.open(&path)?;
        file.try_lock().map_err(std::io::Error::from)?;
        let writer = file.try_clone()?;
        Ok((writer, Self { path, lease: Some(file) }))
    }

    /// Blocking verification outlives a cancelled waiter. Move the cleanup owner into that
    /// work, rather than leaving it in the async request while a detached task uses its path.
    pub(super) fn blocking<T: Send + 'static>(
        self,
        work: impl FnOnce(&Path) -> Result<T, Error> + Send + 'static,
    ) -> tokio::task::JoinHandle<Result<T, Error>> {
        tokio::task::spawn_blocking(move || {
            let result = work(&self.path);
            drop(self);
            result
        })
    }
}

impl Drop for Temporary {
    fn drop(&mut self) {
        drop(self.lease.take());
        _ = std::fs::remove_file(&self.path);
    }
}
