//! Read one following unit's publication while the caller processes the current unit.
//! The carrier reads only. Admission, validation, and record installation stay in plan order.

use core::time::Duration;

use blaze_int::Int as _;
use blaze_own::{Vec, format_in};
use blaze_wire::{Digest, LocalPolicy, PackRef, WireAlloc, json::hex};

use crate::Error;
use crate::blobs::Blobs;
use crate::pack::BATCH_BYTES_TARGET;
use crate::sys::{Receiver, RecvTimeoutError, Sender, TcpDeadline, channel, scope};

use super::{Config, ExportBudget, published_packs};

pub(super) struct BufferedPack {
    pub(super) key: Digest,
    pub(super) bytes: Vec<u8, WireAlloc>,
}

pub(super) struct Publication {
    pub(super) packs: Vec<PackRef, WireAlloc>,
    pub(super) first: Option<BufferedPack>,
}

struct Request {
    spec: Digest,
    deadline: TcpDeadline,
}

pub(super) struct ReadAhead {
    ask: Sender<Request>,
    answers: Receiver<Result<Option<Publication>, Error>>,
    pending: Option<Digest>,
    timeout: Duration,
    alloc: WireAlloc,
}

impl ReadAhead {
    pub(super) fn before(&mut self, spec: Digest, next: Option<Digest>) -> Result<Option<Publication>, Error> {
        let ready = match self.pending.take() {
            Some(pending) if pending == spec => self.answers.recv_timeout(self.timeout)
                .map_err(|why| Error::Refused(format_in!(
                    self.alloc, "unit {}: awaiting the pending publication read: {why:?}", hex(&spec),
                )))?,
            Some(pending) => return Err(Error::Refused(format_in!(
                self.alloc, "unit {}: the pending publication read belongs to {}", hex(&spec), hex(&pending),
            ))),
            None => Ok(None),
        }?;
        if let Some(spec) = next {
            let deadline = TcpDeadline::after(self.timeout)?;
            self.ask.send(Request { spec, deadline }).map_err(|_| Error::Protocol(format_in!(
                self.alloc, "unit {}: the publication reader ended before accepting the request", hex(&spec),
            )))?;
            self.pending = Some(spec);
        }
        // A speculative miss is checked again on demand: a concurrent exporter may have
        // published between the two. A successful publication never falls back to compilation.
        Ok(ready)
    }
}

impl<'a> ExportBudget<'a> {
    /// A scoped carrier can buffer one manifest and one bounded pack. The caller retains
    /// one credit, returned only when it reaches that unit in plan order. Explicit local
    /// fallback reads in order so it cannot leave a speculative committed refusal behind.
    pub fn with_published_ahead<T>(
        config: &'a Config,
        run: impl for<'scope> FnOnce(&mut ExportBudget<'scope>) -> T,
    ) -> Result<T, Error> {
        if !matches!(config.local, LocalPolicy::Never) {
            return Ok(run(&mut Self::new(config)));
        }
        let blobs = &config.blobs;
        let alloc = blobs.alloc;
        let idle = blobs.timeout.max(Duration::from_secs(1));
        scope(alloc, |scope| {
            let (ask, requests) = channel::<Request>(alloc);
            let (deliver, answers) = channel(alloc);
            let worker = scope.spawn(move || {
                loop {
                    let request = match requests.recv_timeout(idle) {
                        Ok(request) => request,
                        Err(RecvTimeoutError::Timeout) => continue,
                        Err(RecvTimeoutError::Disconnected) => break,
                    };
                    if deliver.send(read_publication(blobs, request)).is_err() {
                        break;
                    }
                }
            })?;
            // Dropped before the join on both the normal and unwind paths.
            let mut budget = Self {
                config,
                pr: None,
                ahead: Some(ReadAhead { ask, answers, pending: None, timeout: blobs.timeout, alloc }),
            };
            let result = run(&mut budget);
            drop(budget);
            worker.join().map_err(|_| Error::Refused(format_in!(alloc, "the publication reader ended without an answer")))?;
            Ok(result)
        })
    }
}

fn read_publication(blobs: &Blobs, request: Request) -> Result<Option<Publication>, Error> {
    let alloc = blobs.alloc;
    let Some(list) = blobs.published_until(&request.spec, request.deadline, alloc)? else {
        return Ok(None);
    };
    let packs = published_packs(&request.spec, &list, alloc, alloc)?;
    drop(list);
    // An oversized single record is still read on demand.
    let first = if let Some(reference) = packs.iter().find(|reference| reference.size <= BATCH_BYTES_TARGET.widen_to::<u64>()) {
        let key = reference.pack;
        let bytes = blobs.pack_until(&key, reference.size, request.deadline, alloc)
            .map_err(|why| Error::Committed(format_in!(alloc, "pack {}: committed retrieval failed: {why}", hex(&key))))?
            .ok_or_else(|| Error::Committed(format_in!(alloc, "pack {} was committed and is gone", hex(&key))))?;
        Some(BufferedPack { key, bytes })
    } else {
        None
    };
    Ok(Some(Publication { packs, first }))
}
