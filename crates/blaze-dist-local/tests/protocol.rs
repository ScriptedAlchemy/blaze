//! Canonical framing/version vectors and exact response binding.

use blaze_dist_local::protocol::*;
use blaze_wire::{Attempt, ExecutionId, JobContent, Limits, decode, encode};

#[test]
fn hello_golden_separates_transport_global_codec_and_terminal_versions() {
    let value = ClientFrame::Hello(LocalHello {
        transport: 5,
        codec: 1,
        terminal: 2,
        nonce: [0; 32],
        epoch: None,
        resume: None,
    });
    let mut expected = vec![0x82, 0, 0xa6, 0, 5, 1, 1, 2, 2, 3, 0x58, 0x20];
    expected.extend([0; 32]);
    expected.extend([4, 0xf6, 5, 0xf6]);
    assert_eq!(encode(&value), expected);
    assert_eq!(decode::<ClientFrame>(&expected, &Limits::DOCUMENTED).unwrap(), value);
    let framed = blaze_dist_local::server::frame(&expected);
    assert_eq!(u32::from_be_bytes(framed[..4].try_into().unwrap()) as usize, expected.len());
    assert_eq!(&framed[4..], expected);
}

#[test]
fn resume_hello_carries_the_issued_label_and_daemon_epoch_together() {
    // The transport names a lifetime by the daemon's issued label, and creation by a nonce.
    // A resume carries both the label and the epoch; neither is the creation nonce.
    let created = ClientFrame::Hello(blaze_dist_local::protocol::hello([7; 32]));
    let resumed = ClientFrame::Hello(blaze_dist_local::protocol::resume([9; 32], [8; 32]));
    assert_ne!(encode(&created), encode(&resumed));
    let ClientFrame::Hello(hello) = resumed else { panic!() };
    assert_eq!(hello.nonce, [0; 32]);
    assert_eq!(hello.resume, Some([9; 32]));
    assert_eq!(hello.epoch, Some([8; 32]));
}

#[test]
fn carrier_envelopes_roundtrip_all_variants_without_json() {
    let mut g = blaze_wire::Gen::new(0x177);
    for _ in 0..64 {
        let value = <ClientFrame as blaze_wire::Sample>::sample(&mut g);
        let bytes = encode(&value);
        assert_eq!(decode::<ClientFrame>(&bytes, &Limits::DOCUMENTED).unwrap(), value);
        let value = <ServerFrame as blaze_wire::Sample>::sample(&mut g);
        let bytes = encode(&value);
        assert_eq!(decode::<ServerFrame>(&bytes, &Limits::DOCUMENTED).unwrap(), value);
    }
}

#[test]
fn retirement_has_an_exact_owned_assignment_schema_in_transport_five() {
    assert_eq!(TRANSPORT_VERSION, 5);
    let value = Command::RetireAssignment(RetireAssignment {
        content: JobContent([0x11; 32]),
        executor: "lead".to_owned(),
        execution: ExecutionId(7),
        attempt: Attempt(3),
        claimant: [0x22; 32],
    });
    // Command tag 17, followed by exactly the five ordered assignment/owner fields.
    let mut expected = vec![0x82, 17, 0xa5, 0, 0x58, 0x20];
    expected.extend([0x11; 32]);
    expected.extend([1, 0x64, b'l', b'e', b'a', b'd', 2, 7, 3, 3, 4, 0x58, 0x20]);
    expected.extend([0x22; 32]);
    assert_eq!(encode(&value), expected);
    assert_eq!(decode::<Command>(&expected, &Limits::DOCUMENTED).unwrap(), value);

    // A retirement without its owner, or with an additional field, is not another shape
    // of the same request and must not acquire authority through a decoder default.
    let mut missing_owner = expected.clone();
    missing_owner[2] = 0xa4;
    missing_owner.truncate(missing_owner.len() - 35);
    assert!(decode::<Command>(&missing_owner, &Limits::DOCUMENTED).is_err());
    let mut extra = expected;
    extra[2] = 0xa6;
    extra.extend([5, 0]);
    assert!(decode::<Command>(&extra, &Limits::DOCUMENTED).is_err());

    let request = ClientFrame::Request(Request {
        epoch: [0x33; 32],
        session: [0x44; 32],
        sequence: 9,
        command: value,
    });
    assert_eq!(
        decode::<ClientFrame>(&encode(&request), &Limits::DOCUMENTED).unwrap(),
        request
    );
    let ack = Answer::Ack(Ack {});
    assert_eq!(encode(&ack), vec![0x82, 2, 0xa0]);
    let response = ServerFrame::Response(Response {
        epoch: [0x33; 32],
        session: [0x44; 32],
        sequence: 9,
        answer: ack,
    });
    assert_eq!(
        decode::<ServerFrame>(&encode(&response), &Limits::DOCUMENTED).unwrap(),
        response
    );
}

#[test]
fn owned_assignment_lookup_has_exact_request_and_answer_shapes() {
    assert_eq!(TRANSPORT_VERSION, 5);
    let value = Command::OwnedAssignment(OwnedAssignment {
        content: JobContent([0x11; 32]),
        executor: "lead".to_owned(),
        execution: ExecutionId(7),
        claimant: [0x22; 32],
    });
    // Lookup has no attempt to guess and no authority to claim another executor.
    // Command tag 18 names exactly content, slot, execution and claimant.
    let mut expected = vec![0x82, 18, 0xa4, 0, 0x58, 0x20];
    expected.extend([0x11; 32]);
    expected.extend([1, 0x64, b'l', b'e', b'a', b'd', 2, 7, 3, 0x58, 0x20]);
    expected.extend([0x22; 32]);
    assert_eq!(encode(&value), expected);
    assert_eq!(decode::<Command>(&expected, &Limits::DOCUMENTED).unwrap(), value);
    let mut missing_owner = expected.clone();
    missing_owner[2] = 0xa3;
    missing_owner.truncate(missing_owner.len() - 35);
    assert!(decode::<Command>(&missing_owner, &Limits::DOCUMENTED).is_err());
    let mut extra = expected;
    extra[2] = 0xa5;
    extra.extend([4, 3]);
    assert!(decode::<Command>(&extra, &Limits::DOCUMENTED).is_err());

    let answer = Answer::OwnedAssignment(OwnedAssignmentAnswer { attempt: Attempt(3) });
    let expected = vec![0x82, 13, 0xa1, 0, 3];
    assert_eq!(encode(&answer), expected);
    assert_eq!(decode::<Answer>(&expected, &Limits::DOCUMENTED).unwrap(), answer);
    for malformed in [
        vec![0x82, 13, 0xa0],
        vec![0x82, 13, 0xa1, 0, 0xf6],
        vec![0x82, 13, 0xa2, 0, 3, 1, 0],
    ] {
        assert!(decode::<Answer>(&malformed, &Limits::DOCUMENTED).is_err());
    }
    let request = ClientFrame::Request(Request {
        epoch: [0x33; 32],
        session: [0x44; 32],
        sequence: 9,
        command: value,
    });
    assert_eq!(decode::<ClientFrame>(&encode(&request), &Limits::DOCUMENTED).unwrap(), request);
    let response = ServerFrame::Response(Response {
        epoch: [0x33; 32],
        session: [0x44; 32],
        sequence: 9,
        answer,
    });
    assert_eq!(decode::<ServerFrame>(&encode(&response), &Limits::DOCUMENTED).unwrap(), response);
}
