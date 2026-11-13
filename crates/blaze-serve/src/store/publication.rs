//! Publish an already written, hidden file: sync its data, atomically replace the name,
//! sync the original destination directory, then acknowledge. No clock or retry is needed;
//! each of the three effects must finish successfully before the next is offered. A failed
//! barrier ends the request, even if the rename has already made the new file visible.
//!
//! Native IO maps the logical slots to the caller's original handle and paths. Sim maps
//! them to its files; staging uses Append, whose unsynced bytes can tear, never Write (an
//! already durable whole-file replacement). Sim's completed rename keeps the namespace
//! change even without a directory sync: the test explicitly explores losing that change.

#![blaze::sans_io]
#![deny(blaze::sans_io_core)]

use blaze_core::{Completion, Done, Effect, FileName, Tag};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Step {
    FileSync,
    Rename,
    DirectorySync,
    Acknowledged,
    Failed,
}

#[cfg(test)]
#[derive(Clone, Copy, Debug)]
pub(super) enum Plant {
    None,
    NoFileSync,
    EarlyAck,
}

pub(super) struct Publication {
    step: Step,
    staged: FileName,
    published: FileName,
    directory: FileName,
    #[cfg(test)]
    plant: Plant,
}

impl Publication {
    pub(super) const MAX_STEPS: usize = 3;

    pub(super) fn new() -> Self {
        Self {
            step: Step::FileSync,
            staged: FileName::new("objects/.staged").expect("a fixed logical name"),
            published: FileName::new("objects/published").expect("a fixed logical name"),
            directory: FileName::new("objects").expect("a fixed logical name"),
            #[cfg(test)]
            plant: Plant::None,
        }
    }

    #[cfg(test)]
    pub(super) fn planted(plant: Plant) -> Self {
        let mut publication = Self::new();
        publication.plant = plant;
        if matches!(plant, Plant::NoFileSync) {
            publication.step = Step::Rename;
        }
        publication
    }

    pub(super) fn step(&self) -> Step {
        self.step
    }

    pub(super) fn acknowledged(&self) -> bool {
        self.step == Step::Acknowledged
    }

    pub(super) fn effect(&self) -> Option<Effect> {
        Some(match self.step {
            Step::FileSync => Effect::Sync { file: self.staged.clone(), tag: Tag(0) },
            Step::Rename => Effect::Rename {
                from: self.staged.clone(),
                to: self.published.clone(),
                tag: Tag(1),
            },
            Step::DirectorySync => Effect::Sync { file: self.directory.clone(), tag: Tag(2) },
            Step::Acknowledged | Step::Failed => return None,
        })
    }

    pub(super) fn complete(&mut self, completion: Completion) -> Result<(), String> {
        let next = match (self.step, completion.tag, completion.done) {
            (Step::FileSync, Tag(0), Done::Synced) => Step::Rename,
            (Step::Rename, Tag(1), Done::Renamed) => Step::DirectorySync,
            (Step::DirectorySync, Tag(2), Done::Synced) => Step::Acknowledged,
            (step, _, done) => {
                self.step = Step::Failed;
                // alloc: an error names the logical publication slot and refused transition.
                return Err(format!("objects/published: {step:?} refused completion {done:?}"));
            }
        };
        self.step = next;
        #[cfg(test)]
        if self.step == Step::DirectorySync && matches!(self.plant, Plant::EarlyAck) {
            self.step = Step::Acknowledged;
        }
        Ok(())
    }
}
