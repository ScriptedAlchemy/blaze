//! A pool of proc-macro server processes
use std::sync::Arc;

use rayon::iter::{IntoParallelIterator, ParallelIterator};

use crate::{LibrarySnapshot, MacroDylib, ProcMacro, ServerError, process::ProcMacroServerProcess};

#[derive(Debug, Clone)]
pub(crate) struct ProcMacroServerPool {
    workers: Arc<[ProcMacroServerProcess]>,
    version: u32,
}

impl ProcMacroServerPool {
    pub(crate) fn new(workers: Vec<ProcMacroServerProcess>) -> Self {
        let version = workers[0].version();
        Self { workers: workers.into(), version }
    }
}

impl ProcMacroServerPool {
    pub(crate) fn exited(&self) -> Option<&ServerError> {
        for worker in &*self.workers {
            worker.exited()?;
        }
        self.workers[0].exited()
    }

    pub(crate) fn pick_process(&self) -> Result<&ProcMacroServerProcess, ServerError> {
        let mut best: Option<&ProcMacroServerProcess> = None;
        let mut best_load = u32::MAX;

        for w in self.workers.iter().filter(|w| w.exited().is_none()) {
            let load = w.number_of_active_req();

            if load == 0 {
                return Ok(w);
            }

            if load < best_load {
                best = Some(w);
                best_load = load;
            }
        }

        best.ok_or_else(|| ServerError {
            message: "all proc-macro server workers have exited".into(),
            io: None,
        })
    }

    pub(crate) fn load_dylib(&self, dylib: &MacroDylib) -> Result<Vec<ProcMacro>, ServerError> {
        let _span = tracing::info_span!("ProcMacroServer::load_dylib").entered();

        let original_dylib_path = Arc::new(dylib.path.clone());
        // The server may cache native loaded libraries by pathname/mtime. Give
        // this immutable loaded generation its own file, retained by its handles.
        let snapshot = LibrarySnapshot::copy(&dylib.path)
        .map_err(|error| ServerError { message: error.to_string(), io: None })?;
        let dylib_path = Arc::new(paths::AbsPathBuf::assert_utf8(snapshot.snapshot_path()));
        let snapshot = Arc::new(snapshot);
        use sha2::{Digest as _, Sha256};
        let bytes = std::fs::read(dylib_path.as_path())
            .map_err(|error| ServerError { message: error.to_string(), io: None })?;
        let dylib_content = Some(Sha256::digest(bytes).into());

        let (first, rest) = self.workers.split_first().expect("worker pool must not be empty");

        let macros = first
            .find_proc_macros(&dylib_path)?
            .map_err(|e| ServerError { message: e, io: None })?;

        rest.into_par_iter()
            .map(|worker| {
                worker
                    .find_proc_macros(&dylib_path)?
                    .map(|_| ())
                    .map_err(|e| ServerError { message: e, io: None })
            })
            .collect::<Result<(), _>>()?;

        Ok(macros
            .into_iter()
            .map(|(name, kind)| ProcMacro {
                pool: self.clone(),
                name: name.into(),
                kind,
                dylib_path: dylib_path.clone(),
                dylib_content,
                original_dylib_path: original_dylib_path.clone(),
                _snapshot: snapshot.clone(),
            })
            .collect())
    }

    pub(crate) fn version(&self) -> u32 {
        self.version
    }
}
