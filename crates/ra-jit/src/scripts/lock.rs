//! Serialize workers that compile and run scripts in the same target directory. The lock
//! covers their freshness decisions as well as their writes; it is released when the worker
//! exits even if a script fails or the worker is killed.

use std::{fs::File, io, path::Path};

pub(super) fn acquire(dir: &Path) -> io::Result<File> {
    std::fs::create_dir_all(dir)?;
    let lock = File::options()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(dir.join(".lock"))?;
    lock.lock()?;
    Ok(lock)
}

#[cfg(test)]
mod tests {
    use std::{sync::mpsc, time::Duration};

    use super::*;

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
