//! An additive, type-versioned terminal delivery, separate from v1 `JobEvent`.
//!
//! The v2 terminal endpoint returns one deterministic CBOR [`SubscriberTerminalV2`] map,
//! not an added variant or field in the v1 stream. Its nested [`TerminationV2::version`]
//! must be [`VERSION`]; consumers validate it and the outcome/reason pair before policy.
//! The existing `Limits::event`, depth and collection limits still apply.
//! This addition does not edit `BuildSpec` or `JobEvent`. Terminal version and the
//! enclosing wire crate's schema/digest version are separate: C2's schema-2 integration
//! changes general typed domains independently. These type names separate terminal
//! identities in whichever schema codec is selected. See docs/distributed.md for framing,
//! compatibility and the driver conversion boundary.

use crate::{Attempt, ExecutionId, Limits, Outcome, decode, wire_enum, wire_struct};

/// The version of this terminal-delivery contract, not the version of v1 build inputs.
pub const VERSION: u32 = 2;

wire_enum! {
    /// Why a subscription or execution terminated. Only `Reported` is a build/test verdict.
    TerminationReasonV2 {
        Reported => 0,
        NoLead => 1,
        LeadLostTwice => 2,
        Cancelled => 3,
        Deadline => 4,
        SubscriberDeadline => 5,
    }
}

wire_struct! {
    /// A terminal outcome with its classification. Never derive the reason from v1 Outcome.
    TerminationV2 {
        0 => version: u32,
        1 => outcome: Outcome,
        2 => reason: TerminationReasonV2,
    }
}

/// Why the terminal response cannot be used for policy.
#[derive(Debug)]
pub enum TerminalDecodeError {
    /// The body exceeds the negotiated event-byte limit.
    TooLarge,
    /// The CBOR body is not a canonical value of this schema.
    Cbor(crate::Error),
    /// Unsupported version or inconsistent outcome/reason pair.
    InvalidTermination,
}

/// Decode one complete terminal response, checking framing, size, schema and semantics.
/// Trailing bytes, unknown keys/variants and noncanonical CBOR are rejected by `decode`.
pub fn decode_terminal(
    bytes: &[u8],
    limits: &Limits,
) -> Result<SubscriberTerminalV2, TerminalDecodeError> {
    if bytes.len() > limits.event as usize {
        return Err(TerminalDecodeError::TooLarge);
    }
    let value: SubscriberTerminalV2 = decode(bytes, limits).map_err(TerminalDecodeError::Cbor)?;
    if !value.termination.is_valid() {
        return Err(TerminalDecodeError::InvalidTermination);
    }
    Ok(value)
}

impl TerminationV2 {
    /// Whether both the contract version and the outcome/reason combination are valid.
    pub fn is_valid(&self) -> bool {
        self.version == VERSION
            && match self.reason {
                TerminationReasonV2::Reported => {
                    matches!(self.outcome, Outcome::Passed | Outcome::Failed | Outcome::Refused)
                }
                TerminationReasonV2::NoLead => self.outcome == Outcome::Lost,
                TerminationReasonV2::LeadLostTwice => self.outcome == Outcome::Lost,
                TerminationReasonV2::Cancelled | TerminationReasonV2::Deadline => {
                    self.outcome == Outcome::Cancelled
                }
                TerminationReasonV2::SubscriberDeadline => self.outcome == Outcome::TimedOut,
            }
    }
}

wire_struct! {
    /// One subscriber's immutable terminal delivery, returned identically on retry.
    SubscriberTerminalV2 {
        0 => execution: ExecutionId,
        1 => attempt: Attempt,
        2 => subscriber: u64,
        3 => termination: TerminationV2,
    }
}
