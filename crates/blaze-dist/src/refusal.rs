//! Why the coordinator refused a call: a typed reason in the code, one `WireError` on the wire.
//!
//! The wire's error has a code and a message (section 8). Several refusals share code 6
//! (conflict), and an executor must tell them apart: a dead lease asks for a new one, a moved
//! attempt stops, a join that came before the lead tries again. So each refusal's message starts
//! with a stable token and a colon, which [`Refusal::of`] reads back.

use blaze_wire::{ErrorCode, WireError};

/// A refusal of a work or job call.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Refusal {
    /// The holder has not joined this attempt, or was dropped as lost: it joins again.
    NotJoined,
    /// A helper's root set differs from the provisional one and the lead has not joined: the
    /// helper tries again after the lead has fixed the execution's.
    NotYet,
    /// The executor's front end differs from the lead's: it is refused for this attempt.
    FrontEndDiffers,
    /// The call names an attempt that is not the execution's.
    AttemptMoved,
    /// The result's lease generation is not the holder's live one: refused whole.
    DeadLease,
    /// The execution has ended, or its compile phase is over.
    Ended,
    /// No such execution.
    NoExecution,
    /// The caller is not the lease's holder.
    NotHolder,
    /// A batch's sequence number was used before with other content.
    SeqReused,
    /// The call does not follow the protocol's shape: what is wrong.
    Malformed(String),
    /// Past a bound: the queue is full and the caller retries after the time given.
    Busy,
    /// No such subscriber.
    NoSubscriber,
}

impl Refusal {
    /// The token the message starts with.
    pub fn token(&self) -> &'static str {
        match self {
            Refusal::NotJoined => "not-joined",
            Refusal::NotYet => "not-yet",
            Refusal::FrontEndDiffers => "front-end-differs",
            Refusal::AttemptMoved => "attempt-moved",
            Refusal::DeadLease => "dead-lease",
            Refusal::Ended => "ended",
            Refusal::NoExecution => "no-execution",
            Refusal::NotHolder => "not-holder",
            Refusal::SeqReused => "seq-reused",
            Refusal::Malformed(_) => "malformed",
            Refusal::Busy => "busy",
            Refusal::NoSubscriber => "no-subscriber",
        }
    }

    /// The section 8 code it is answered with.
    pub fn code(&self) -> ErrorCode {
        match self {
            Refusal::NotJoined | Refusal::NoExecution | Refusal::NoSubscriber => {
                ErrorCode::NotFound
            }
            Refusal::NotYet
            | Refusal::FrontEndDiffers
            | Refusal::AttemptMoved
            | Refusal::DeadLease
            | Refusal::Ended => ErrorCode::Conflict,
            Refusal::NotHolder => ErrorCode::NotAllowed,
            Refusal::SeqReused | Refusal::Malformed(_) => ErrorCode::Malformed,
            Refusal::Busy => ErrorCode::Busy,
        }
    }

    /// The error on the wire. `retry_after_ms` is set where waiting helps: a join before the
    /// lead's, and a full queue.
    pub fn to_wire(&self, retry_after_ms: Option<u64>) -> WireError {
        let text = match self {
            Refusal::NotJoined => "the holder has not joined this attempt",
            Refusal::NotYet => "the lead has not joined and the provisional root set differs",
            Refusal::FrontEndDiffers => "the executor's front end differs",
            Refusal::AttemptMoved => "the execution's attempt moved",
            Refusal::DeadLease => "a dead lease generation",
            Refusal::Ended => "the execution has ended",
            Refusal::NoExecution => "no such execution",
            Refusal::NotHolder => "the caller is not the lease's holder",
            Refusal::SeqReused => "a batch number used before with other content",
            Refusal::Malformed(what) => what.as_str(),
            Refusal::Busy => "the queue is full",
            Refusal::NoSubscriber => "no such subscriber",
        };
        let error = WireError::new(self.code(), format!("{}: {text}", self.token()));
        match retry_after_ms {
            Some(ms) => error.retry_after(ms),
            None => error,
        }
    }

    /// The refusal a wire error names, by its token; `None` for an error this crate did not
    /// write.
    pub fn of(error: &WireError) -> Option<Refusal> {
        let (token, rest) = error.message.split_once(": ").unwrap_or((&error.message, ""));
        Some(match token {
            "not-joined" => Refusal::NotJoined,
            "not-yet" => Refusal::NotYet,
            "front-end-differs" => Refusal::FrontEndDiffers,
            "attempt-moved" => Refusal::AttemptMoved,
            "dead-lease" => Refusal::DeadLease,
            "ended" => Refusal::Ended,
            "no-execution" => Refusal::NoExecution,
            "not-holder" => Refusal::NotHolder,
            "seq-reused" => Refusal::SeqReused,
            "malformed" => Refusal::Malformed(rest.to_owned()),
            "busy" => Refusal::Busy,
            "no-subscriber" => Refusal::NoSubscriber,
            _ => return None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::Refusal;

    #[test]
    fn every_refusal_reads_back() {
        for refusal in [
            Refusal::NotJoined,
            Refusal::NotYet,
            Refusal::FrontEndDiffers,
            Refusal::AttemptMoved,
            Refusal::DeadLease,
            Refusal::Ended,
            Refusal::NoExecution,
            Refusal::NotHolder,
            Refusal::SeqReused,
            Refusal::Malformed("items: 3 for 4 processed".into()),
            Refusal::Busy,
            Refusal::NoSubscriber,
        ] {
            let wire = refusal.to_wire(Some(5));
            assert_eq!(Refusal::of(&wire), Some(refusal.clone()), "{wire:?}");
            assert_eq!(wire.code(), Some(refusal.code()));
        }
    }
}
