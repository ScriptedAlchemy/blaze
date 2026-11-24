//! Sans-IO publication state machine for atomic file publication:
//!
//! 1. `Sync` the staged temporary file so its bytes are durable before it becomes visible.
//! 2. `Rename` the temporary file over the destination path.
//! 3. `SyncDir` the parent directory so the renamed entry is durable before acknowledging.
//!
//! Only after all three effects complete does the state machine reach [`Phase::Acknowledged`].
//! Any completion out of order or carrying an unexpected tag is refused and returns its owned
//! mapping token so the caller releases it cleanly.

#![blaze::sans_io]

/// Opaque correlation tag pairing an emitted [`Effect`] with its [`Completion`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Tag(pub(crate) u64);

/// Logical file handle inside a publication step.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct LogicalFile(pub(crate) u64);

/// Logical directory handle inside a publication step.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct LogicalDir(pub(crate) u64);

/// One durability effect requested by [`Publication`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Effect {
    /// Sync the staged file before exposing it under its final name.
    Sync { tag: Tag, file: LogicalFile },
    /// Atomically rename the staged file to the target file inside `dir`.
    Rename { tag: Tag, dir: LogicalDir, from: LogicalFile, to: LogicalFile },
    /// Sync the parent directory so the rename survives power loss.
    SyncDir { tag: Tag, dir: LogicalDir },
}

/// The payload completed by the driver for one [`Effect`].
#[derive(Debug)]
pub(crate) enum Done<M = ()> {
    /// `Sync` completed.
    Synced,
    /// `Rename` completed.
    Renamed,
    /// `SyncDir` completed.
    DirSynced,
    /// A mapped write completed (carrying an owned mapping resource that must be released on refusal).
    #[cfg(test)]
    MappedWrite(M),
    /// Keeps `M` referenced in non-test builds without allocating.
    #[cfg(not(test))]
    #[allow(dead_code)]
    Phantom(core::marker::PhantomData<M>),
}

/// One driver completion delivered back into [`Publication::complete`].
#[derive(Debug)]
pub(crate) struct Completion<M = ()> {
    /// Tag of the effect that finished.
    pub(crate) tag: Tag,
    /// Outcome of the effect (`Ok(done)` or `Err(code)`).
    pub(crate) result: Result<Done<M>, u32>,
}

/// Returned when [`Publication::complete`] refuses an unexpected or mismatched completion,
/// returning any owned mapping so it is dropped promptly rather than leaked or retained.
#[derive(Debug)]
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) struct RefusedCompletion<M = ()> {
    /// Tag of the refused completion.
    pub(crate) tag: Tag,
    /// Reason the completion was refused.
    pub(crate) reason: RefusalReason,
    /// Any owned mapping carried by the completion, handed back for release.
    pub(crate) mapping: Option<M>,
}

/// Why a completion was refused by [`Publication`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum RefusalReason {
    /// No effect is currently in flight.
    NoEffectInFlight,
    /// Tag did not match the in-flight effect's tag.
    UnexpectedTag { expected: Tag, actual: Tag },
    /// Completion kind did not match the expected effect kind.
    UnexpectedKind,
}

/// Current phase of a [`Publication`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Phase {
    /// Waiting for `Sync` on the staged file.
    SyncingFile,
    /// Waiting for `Rename` into the final path.
    Renaming,
    /// Waiting for `SyncDir` on the parent directory.
    SyncingDir,
    /// All durability barriers finished; safe to acknowledge to the caller.
    Acknowledged,
    /// A durability step failed; the caller must not be told the write succeeded.
    Failed(u32),
}

/// Planted durability bugs for the DST oracle (`store/repair_tests.rs`).
#[cfg(test)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Plant {
    /// Correct protocol: `Sync` -> `Rename` -> `SyncDir` -> `Acknowledged`.
    None,
    /// Skips `Sync` before `Rename` (allows torn unsynced bytes to become visible on crash).
    NoFileSync,
    /// Acknowledges immediately after `Rename` without waiting for `SyncDir`.
    EarlyAck,
}

/// Pure sans-IO state machine for durable atomic file publication.
#[derive(Debug)]
pub(crate) struct Publication {
    dir: LogicalDir,
    staging: LogicalFile,
    target: LogicalFile,
    phase: Phase,
    in_flight: Option<Effect>,
    next_tag: u64,
    #[cfg(test)]
    plant: Plant,
}

impl Publication {
    /// Starts a new publication for `staging -> target` in `dir`, returning the state machine
    /// and its first effect.
    pub(crate) fn start(
        dir: LogicalDir,
        staging: LogicalFile,
        target: LogicalFile,
        first_tag: u64,
    ) -> (Self, Effect) {
        let tag = Tag(first_tag);
        let first = Effect::Sync { tag, file: staging };
        let machine = Self {
            dir,
            staging,
            target,
            phase: Phase::SyncingFile,
            in_flight: Some(first),
            next_tag: first_tag.saturating_add(1),
            #[cfg(test)]
            plant: Plant::None,
        };
        (machine, first)
    }

    /// Starts a publication with a planted durability bug for DST oracle testing.
    #[cfg(test)]
    pub(crate) fn start_planted(
        dir: LogicalDir,
        staging: LogicalFile,
        target: LogicalFile,
        first_tag: u64,
        plant: Plant,
    ) -> (Self, Effect) {
        let tag = Tag(first_tag);
        let (phase, first) = match plant {
            Plant::NoFileSync => (
                Phase::Renaming,
                Effect::Rename { tag, dir, from: staging, to: target },
            ),
            Plant::None | Plant::EarlyAck => (
                Phase::SyncingFile,
                Effect::Sync { tag, file: staging },
            ),
        };
        let machine = Self {
            dir,
            staging,
            target,
            phase,
            in_flight: Some(first),
            next_tag: first_tag.saturating_add(1),
            plant,
        };
        (machine, first)
    }

    /// Current publication phase.
    pub(crate) const fn phase(&self) -> Phase {
        self.phase
    }

    /// Currently in-flight effect, if any.
    #[cfg(test)]
    pub(crate) const fn in_flight(&self) -> Option<Effect> {
        self.in_flight
    }

    /// Advances the state machine with one driver completion.
    ///
    /// Returns `Ok(Some(next_effect))` when another effect must run, `Ok(None)` when a terminal
    /// phase (`Acknowledged` or `Failed`) is reached, or `Err(RefusedCompletion)` if the
    /// completion did not match the in-flight effect (handing back any owned mapping).
    pub(crate) fn complete<M>(
        &mut self,
        completion: Completion<M>,
    ) -> Result<Option<Effect>, RefusedCompletion<M>> {
        let Some(expected) = self.in_flight else {
            return Err(RefusedCompletion {
                tag: completion.tag,
                reason: RefusalReason::NoEffectInFlight,
                mapping: take_mapping(completion.result),
            });
        };
        let expected_tag = effect_tag(expected);
        if completion.tag != expected_tag {
            return Err(RefusedCompletion {
                tag: completion.tag,
                reason: RefusalReason::UnexpectedTag { expected: expected_tag, actual: completion.tag },
                mapping: take_mapping(completion.result),
            });
        }
        let done = match completion.result {
            Ok(done) => done,
            Err(code) => {
                self.in_flight = None;
                self.phase = Phase::Failed(code);
                return Ok(None);
            }
        };

        let valid_kind = matches!(
            (expected, &done),
            (Effect::Sync { .. }, Done::Synced)
                | (Effect::Rename { .. }, Done::Renamed)
                | (Effect::SyncDir { .. }, Done::DirSynced)
        );
        if !valid_kind {
            return Err(RefusedCompletion {
                tag: completion.tag,
                reason: RefusalReason::UnexpectedKind,
                mapping: take_done_mapping(done),
            });
        }

        self.in_flight = None;
        match self.phase {
            Phase::SyncingFile => {
                let tag = self.alloc_tag();
                let next = Effect::Rename {
                    tag,
                    dir: self.dir,
                    from: self.staging,
                    to: self.target,
                };
                self.phase = Phase::Renaming;
                self.in_flight = Some(next);
                Ok(Some(next))
            }
            Phase::Renaming => {
                #[cfg(test)]
                if self.plant == Plant::EarlyAck {
                    self.phase = Phase::Acknowledged;
                    return Ok(None);
                }
                let tag = self.alloc_tag();
                let next = Effect::SyncDir { tag, dir: self.dir };
                self.phase = Phase::SyncingDir;
                self.in_flight = Some(next);
                Ok(Some(next))
            }
            Phase::SyncingDir => {
                self.phase = Phase::Acknowledged;
                Ok(None)
            }
            Phase::Acknowledged | Phase::Failed(_) => Ok(None),
        }
    }

    fn alloc_tag(&mut self) -> Tag {
        let tag = Tag(self.next_tag);
        self.next_tag = self.next_tag.saturating_add(1);
        tag
    }
}

const fn effect_tag(effect: Effect) -> Tag {
    match effect {
        Effect::Sync { tag, .. } | Effect::Rename { tag, .. } | Effect::SyncDir { tag, .. } => tag,
    }
}

fn take_mapping<M>(result: Result<Done<M>, u32>) -> Option<M> {
    match result {
        Ok(done) => take_done_mapping(done),
        Err(_) => None,
    }
}

fn take_done_mapping<M>(done: Done<M>) -> Option<M> {
    match done {
        #[cfg(test)]
        Done::MappedWrite(mapping) => Some(mapping),
        Done::Synced | Done::Renamed | Done::DirSynced => None,
        #[cfg(not(test))]
        Done::Phantom(_) => None,
    }
}
