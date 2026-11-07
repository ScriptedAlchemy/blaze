//! Ownership of unpublished files, including across cancellation of an HTTP request.

use std::{
    fs::File,
    io::Write as _,
    path::{Path, PathBuf},
};

use ed25519_dalek::SigningKey;

use crate::error::{Error, failed};
use crate::store::temporary;

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

fn read_key(at: &Path) -> Result<SigningKey, Error> {
    let bytes =
        std::fs::read(at).map_err(|error| failed(&format!("reading {}", at.display()), error))?;
    let seed: [u8; 32] = bytes
        .as_slice()
        .try_into()
        .map_err(|_| Error::internal(format!("{} is not the 32 bytes of a key", at.display())))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(at, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(SigningKey::from_bytes(&seed))
}

/// Publish a complete key without replacing one another opener installed first. The inode
/// is synced before it gets the public name; readers never observe a partially written seed.
fn install_key(dir: &Path, seed: &[u8; 32]) -> Result<SigningKey, Error> {
    let at = dir.join("key");
    let partial = temporary(&at);
    let (mut file, _cleanup) = Temporary::create(partial.clone())
        .map_err(|error| failed(&format!("creating {}", partial.display()), error))?;
    let written = file.write_all(seed).and_then(|()| file.sync_all());
    // Close before either returning an error or dropping the cleanup owner on Windows.
    drop(file);
    written.map_err(|error| failed(&format!("writing {}", partial.display()), error))?;
    match std::fs::hard_link(&partial, &at) {
        Ok(()) => {
            #[cfg(unix)]
            File::open(dir)?.sync_all()?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(failed(&format!("installing {}", at.display()), error)),
    }
    // Even the losing initializer returns the installed key, never its discarded candidate.
    read_key(&at)
}

/// The server's persistent identity. Concurrent openers agree on one installed key.
pub(super) fn key_of(dir: &Path) -> Result<SigningKey, Error> {
    let at = dir.join("key");
    match std::fs::symlink_metadata(&at) {
        Ok(_) => read_key(&at),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let mut seed = [0; 32];
            getrandom::fill(&mut seed).map_err(|error| {
                Error::internal(format!("no randomness for the server's key: {error}"))
            })?;
            install_key(dir, &seed)
        }
        Err(error) => Err(failed(&format!("reading {}", at.display()), error)),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Barrier, mpsc};
    use std::time::Duration;

    use super::*;

    #[test]
    fn a_failed_create_never_removes_another_writers_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("upload.tmp");
        let (mut file, owner) = Temporary::create(path.clone()).unwrap();
        file.write_all(b"another writer's bytes").unwrap();
        drop(file);
        assert!(Temporary::create(path.clone()).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"another writer's bytes");
        drop(owner);
        assert!(!path.exists());
    }

    #[tokio::test]
    async fn cancelling_the_waiter_does_not_remove_a_verifiers_input() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("upload.tmp");
        let (file, owner) = Temporary::create(path.clone()).unwrap();
        drop(file);
        let (started, ready) = tokio::sync::oneshot::channel();
        let (release, proceed) = mpsc::channel();
        let task = owner.blocking(move |path| {
            started.send(()).unwrap();
            proceed.recv_timeout(Duration::from_secs(5)).unwrap();
            assert!(path.exists(), "the request removed a running verifier's input");
            Ok(())
        });
        let waiter = tokio::spawn(async move { task.await });
        ready.await.unwrap();
        waiter.abort();
        assert!(waiter.await.unwrap_err().is_cancelled());
        assert!(path.exists());
        release.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            while path.exists() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
    }

    #[test]
    fn racing_key_installers_return_the_same_persisted_identity() {
        let dir = tempfile::tempdir().unwrap();
        let barrier = Arc::new(Barrier::new(8));
        let writers: Vec<_> = (0..8u8)
            .map(|index| {
                let (root, barrier) = (dir.path().to_owned(), barrier.clone());
                std::thread::spawn(move || {
                    barrier.wait();
                    install_key(&root, &[index; 32]).unwrap().verifying_key().to_bytes()
                })
            })
            .collect();
        let keys: Vec<_> = writers.into_iter().map(|writer| writer.join().unwrap()).collect();
        let installed = key_of(dir.path()).unwrap().verifying_key().to_bytes();
        assert!(keys.iter().all(|key| *key == installed));
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(dir.path().join("key")).unwrap().permissions().mode();
            assert_eq!(mode & 0o077, 0, "new signing keys must not be group/world readable");
        }
    }
}
