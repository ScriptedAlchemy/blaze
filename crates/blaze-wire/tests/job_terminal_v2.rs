//! Isolated v2 terminal vectors; no v1 shape or digest changes.

use blaze_wire::job_terminal_v2::{
    SubscriberTerminalV2, TerminalDecodeError, TerminationReasonV2, TerminationV2, decode_terminal,
};
use blaze_wire::{Attempt, ExecutionId, Limits, Outcome, encode};

fn terminal() -> SubscriberTerminalV2 {
    SubscriberTerminalV2 {
        execution: ExecutionId(1),
        attempt: Attempt(2),
        subscriber: 3,
        termination: TerminationV2 {
            version: 2,
            outcome: Outcome::Failed,
            reason: TerminationReasonV2::NoLead,
        },
    }
}

#[test]
fn terminal_v2_golden_and_roundtrip() {
    let value = terminal();
    // {0:1, 1:2, 2:3, 3:{0:2, 1:[1,null], 2:[1,null]}}
    let golden = [0xa4, 0, 1, 1, 2, 2, 3, 3, 0xa3, 0, 2, 1, 0x82, 1, 0xf6, 2, 0x82, 1, 0xf6];
    assert_eq!(encode(&value), golden);
    assert_eq!(decode_terminal(&golden, &Limits::DOCUMENTED).unwrap(), value);
}

#[test]
fn terminal_v2_rejects_version_semantics_size_and_trailing_frames() {
    let mut value = terminal();
    value.termination.version = 1;
    assert!(matches!(
        decode_terminal(&encode(&value), &Limits::DOCUMENTED),
        Err(TerminalDecodeError::InvalidTermination)
    ));
    value.termination.version = 2;
    value.termination.outcome = Outcome::Passed;
    assert!(matches!(
        decode_terminal(&encode(&value), &Limits::DOCUMENTED),
        Err(TerminalDecodeError::InvalidTermination)
    ));
    let bytes = encode(&terminal());
    let limits = Limits { event: bytes.len() as u32 - 1, ..Limits::DOCUMENTED };
    assert!(matches!(decode_terminal(&bytes, &limits), Err(TerminalDecodeError::TooLarge)));
    let mut trailing = bytes.clone();
    trailing.push(0);
    assert!(decode_terminal(&trailing, &Limits::DOCUMENTED).is_err());
    // Append an unknown field to the otherwise canonical map.
    let mut unknown = bytes;
    unknown[0] = 0xa5;
    unknown.extend([4, 0]);
    assert!(decode_terminal(&unknown, &Limits::DOCUMENTED).is_err());
}

#[test]
fn terminal_v2_every_reason_roundtrips() {
    for (reason, outcome) in [
        (TerminationReasonV2::Reported, Outcome::Passed),
        (TerminationReasonV2::Reported, Outcome::Failed),
        (TerminationReasonV2::Reported, Outcome::Refused),
        (TerminationReasonV2::NoLead, Outcome::Failed),
        (TerminationReasonV2::LeadLostTwice, Outcome::Lost),
        (TerminationReasonV2::Cancelled, Outcome::Cancelled),
        (TerminationReasonV2::Deadline, Outcome::Cancelled),
        (TerminationReasonV2::SubscriberDeadline, Outcome::TimedOut),
    ] {
        let mut value = terminal();
        value.termination.reason = reason;
        value.termination.outcome = outcome;
        assert_eq!(decode_terminal(&encode(&value), &Limits::DOCUMENTED).unwrap(), value);
    }
}
