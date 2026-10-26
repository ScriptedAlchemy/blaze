//! Serialize workers that compile and run scripts in the same target directory. The lock
//! covers their freshness decisions as well as their writes; it is released when the worker
//! exits even if a script fails or the worker is killed.

use std::{fs::File, io, path::Path};

pub(super) struct Held(File);

impl Drop for Held {
    fn drop(&mut self) {
        // Closing our descriptor alone may leave a flock held by a concurrent fork that
        // inherited the open description and has not reached exec yet.
        let _ = self.0.unlock();
    }
}

pub(super) fn acquire(dir: &Path) -> io::Result<Held> {
    std::fs::create_dir_all(dir)?;
    let lock = File::options()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(dir.join(".lock"))?;
    lock.lock()?;
    Ok(Held(lock))
}

#[cfg(test)]
mod tests {
    use std::{sync::mpsc, time::Duration};

    use super::*;

    #[cfg(unix)]
    #[test]
    fn a_duplicated_description_does_not_keep_a_finished_workers_lock() {
        let dir =
            std::env::temp_dir().join(format!("blaze-script-lock-dup-{}", std::process::id()));
        let first = acquire(&dir).unwrap();
        // A concurrent fork can inherit the open description until exec closes it. Keep
        // one alive with dup to check release without depending on process timing.
        let inherited = first.0.try_clone().unwrap();
        drop(first);
        let next = File::open(dir.join(".lock")).unwrap();
        assert!(next.try_lock().is_ok(), "the finished worker released its lock");
        drop(next);
        drop(inherited);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn another_worker_waits_until_the_first_finishes_its_records() {
        let dir = std::env::temp_dir().join(format!("blaze-script-lock-{}", std::process::id()));
        let first = acquire(&dir).unwrap();
        let (started, start) = mpsc::channel();
        let (acquired, next) = mpsc::channel();
        let other = dir.clone();
        let worker = std::thread::spawn(move || {
            started.send(()).unwrap();
            let _held = acquire(&other).unwrap();
            acquired.send(()).unwrap();
        });
        start.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(matches!(
            next.recv_timeout(Duration::from_millis(100)),
            Err(mpsc::RecvTimeoutError::Timeout)
        ));
        drop(first);
        next.recv_timeout(Duration::from_secs(5)).unwrap();
        worker.join().unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
