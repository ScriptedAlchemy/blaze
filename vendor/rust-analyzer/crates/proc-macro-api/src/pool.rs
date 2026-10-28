//! A pool of proc-macro server processes
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

use rayon::iter::{IntoParallelIterator, ParallelIterator};

use crate::{LibrarySnapshot, MacroDylib, ProcMacro, ServerError, process::ProcMacroServerProcess};

#[derive(Debug, Clone)]
pub(crate) struct ProcMacroServerPool {
    workers: Arc<[ProcMacroServerProcess]>,
    version: u32,
    loaded: Arc<Mutex<HashMap<paths::AbsPathBuf, PreparedLibrary>>>,
}

#[derive(Debug, Clone)]
pub(crate) struct PreparedLibrary {
    snapshot: Arc<LibrarySnapshot>,
    pub(crate) content: [u8; 32],
}

impl PreparedLibrary {
    pub(crate) fn copy(
        dylib: &MacroDylib,
        root: Option<&std::path::Path>,
    ) -> Result<Self, ServerError> {
        let (snapshot, content) = LibrarySnapshot::copy(&dylib.path, root).map_err(|error| {
            ServerError { message: "copying macro library".into(), io: Some(Arc::new(error)) }
        })?;
        Ok(Self { snapshot: Arc::new(snapshot), content })
    }
}

impl ProcMacroServerPool {
    pub(crate) fn new(workers: Vec<ProcMacroServerProcess>) -> Self {
        let version = workers[0].version();
        Self { workers: workers.into(), version, loaded: Default::default() }
    }

    pub(crate) fn replaces(&self, path: &paths::AbsPathBuf, content: [u8; 32]) -> bool {
        self.loaded.lock().unwrap().get(path).is_some_and(|old| old.content != content)
    }
}

impl ProcMacroServerPool {
    pub(crate) fn exited(&self) -> Option<&ServerError> {
        self.workers.iter().find_map(|worker| worker.exited())
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

    pub(crate) fn load_dylib(
        self: &Arc<Self>,
        dylib: &MacroDylib,
        prepared: PreparedLibrary,
    ) -> Result<Vec<ProcMacro>, ServerError> {
        let _span = tracing::info_span!("ProcMacroServer::load_dylib").entered();

        let original_dylib_path = Arc::new(dylib.path.clone());
        // Reuse the immutable pathname for unchanged bytes. Native loaders cannot
        // unload individual images; duplicates must not accumulate within an epoch.
        let prepared = {
            let mut loaded = self.loaded.lock().unwrap();
            let entry = loaded.entry(dylib.path.clone()).or_insert_with(|| prepared.clone());
            if entry.content != prepared.content {
                // The non-replayable test transport cannot rotate pools, but still
                // must use a new immutable pathname for changed bytes.
                *entry = prepared;
            }
            entry.clone()
        };
        let dylib_path =
            Arc::new(paths::AbsPathBuf::assert_utf8(prepared.snapshot.snapshot_path()));
        let dylib_content = Some(prepared.content);

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
                _snapshot: prepared.snapshot.clone(),
            })
            .collect())
    }

    pub(crate) fn version(&self) -> u32 {
        self.version
    }
}
