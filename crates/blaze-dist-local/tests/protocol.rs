//! Canonical framing/version vectors and exact response binding.

#![no_std]

use blaze_dist_local::protocol::*;
use blaze_own::Vec;
use blaze_wire::{
    Ack, Attempt, ExecutionId, JobContent, Limits, WireAlloc, WireString, decode, encode,
};

fn test_alloc() -> WireAlloc {
    blaze_own::testing::heap().shared()
}

#[test]
fn hello_golden_separates_transport_global_codec_and_terminal_versions() {
    let alloc = test_alloc();
    let value = ClientFrame::Hello(LocalHello {
        transport: 5,
        codec: 1,
        terminal: 2,
        nonce: [0; 32],
        epoch: None,
        resume: None,
    });
    let mut expected = Vec::from_slice_in(&[0x82, 0, 0xa6, 0, 5, 1, 1, 2, 2, 3, 0x58, 0x20], alloc);
    expected.extend_from_slice(&[0; 32]);
    expected.extend_from_slice(&[4, 0xf6, 5, 0xf6]);
    assert_eq!(encode(&value, alloc), expected);
    assert_eq!(decode::<ClientFrame>(&expected, &Limits::DOCUMENTED, alloc).unwrap(), value);
    let framed = blaze_dist_local::server::frame(&expected, alloc);
    assert_eq!(
        usize::try_from(u32::from_be_bytes(framed[..4].try_into().unwrap())).unwrap(),
        expected.len()
    );
    assert_eq!(&framed[4..], expected.as_slice());
}

#[test]
fn resume_hello_carries_the_issued_label_and_daemon_epoch_together() {
    let alloc = test_alloc();
    // The transport names a lifetime by the daemon's issued label, and creation by a nonce.
    // A resume carries both the label and the epoch; neither is the creation nonce.
    let created = ClientFrame::Hello(blaze_dist_local::protocol::hello([7; 32]));
    let resumed = ClientFrame::Hello(blaze_dist_local::protocol::resume([9; 32], [8; 32]));
    assert_ne!(encode(&created, alloc), encode(&resumed, alloc));
    let ClientFrame::Hello(hello) = resumed else { panic!() };
    assert_eq!(hello.nonce, [0; 32]);
    assert_eq!(hello.resume, Some([9; 32]));
    assert_eq!(hello.epoch, Some([8; 32]));
}

#[test]
fn carrier_envelopes_roundtrip_all_variants_without_json() {
    let alloc = test_alloc();
    let mut g = blaze_wire::Gen::new(0x177);
    for _ in 0..64 {
        let value = <ClientFrame as blaze_wire::Sample>::sample(&mut g, alloc);
        let bytes = encode(&value, alloc);
        assert_eq!(decode::<ClientFrame>(&bytes, &Limits::DOCUMENTED, alloc).unwrap(), value);
        let value = <ServerFrame as blaze_wire::Sample>::sample(&mut g, alloc);
        let bytes = encode(&value, alloc);
        assert_eq!(decode::<ServerFrame>(&bytes, &Limits::DOCUMENTED, alloc).unwrap(), value);
    }
}

#[test]
fn retirement_has_an_exact_owned_assignment_schema_in_transport_five() {
    let alloc = test_alloc();
    assert_eq!(TRANSPORT_VERSION, 5);
    let value = Command::RetireAssignment(RetireAssignment {
        content: JobContent([0x11; 32]),
        executor: WireString::from_str_in("lead", alloc),
        execution: ExecutionId(7),
        attempt: Attempt(3),
        claimant: [0x22; 32],
    });
    // Command tag 17, followed by exactly the five ordered assignment/owner fields.
    let mut expected = Vec::from_slice_in(&[0x82, 17, 0xa5, 0, 0x58, 0x20], alloc);
    expected.extend_from_slice(&[0x11; 32]);
    expected.extend_from_slice(&[1, 0x64, b'l', b'e', b'a', b'd', 2, 7, 3, 3, 4, 0x58, 0x20]);
    expected.extend_from_slice(&[0x22; 32]);
    assert_eq!(encode(&value, alloc), expected);
    assert_eq!(decode::<Command>(&expected, &Limits::DOCUMENTED, alloc).unwrap(), value);

    // A retirement without its owner, or with an additional field, is not another shape
    // of the same request and must not acquire authority through a decoder default.
    let mut missing_owner = expected.clone();
    missing_owner[2] = 0xa4;
    missing_owner.truncate(missing_owner.len() - 35);
    assert!(decode::<Command>(&missing_owner, &Limits::DOCUMENTED, alloc).is_err());
    let mut extra = expected;
    extra[2] = 0xa6;
    extra.extend_from_slice(&[5, 0]);
    assert!(decode::<Command>(&extra, &Limits::DOCUMENTED, alloc).is_err());

    let request = ClientFrame::Request(Request {
        epoch: [0x33; 32],
        session: [0x44; 32],
        sequence: 9,
        command: value,
    });
    assert_eq!(
        decode::<ClientFrame>(&encode(&request, alloc), &Limits::DOCUMENTED, alloc).unwrap(),
        request
    );
    let ack = Answer::Ack(Ack {});
    assert_eq!(encode(&ack, alloc).as_slice(), &[0x82, 2, 0xa0]);
    let response = ServerFrame::Response(Response {
        epoch: [0x33; 32],
        session: [0x44; 32],
        sequence: 9,
        answer: ack,
    });
    assert_eq!(
        decode::<ServerFrame>(&encode(&response, alloc), &Limits::DOCUMENTED, alloc).unwrap(),
        response
    );
}

#[test]
fn owned_assignment_lookup_has_exact_request_and_answer_shapes() {
    let alloc = test_alloc();
    assert_eq!(TRANSPORT_VERSION, 5);
    let value = Command::OwnedAssignment(OwnedAssignment {
        content: JobContent([0x11; 32]),
        executor: WireString::from_str_in("lead", alloc),
        execution: ExecutionId(7),
        claimant: [0x22; 32],
    });
    // Lookup has no attempt to guess and no authority to claim another executor.
    // Command tag 18 names exactly content, slot, execution and claimant.
    let mut expected = Vec::from_slice_in(&[0x82, 18, 0xa4, 0, 0x58, 0x20], alloc);
    expected.extend_from_slice(&[0x11; 32]);
    expected.extend_from_slice(&[1, 0x64, b'l', b'e', b'a', b'd', 2, 7, 3, 0x58, 0x20]);
    expected.extend_from_slice(&[0x22; 32]);
    assert_eq!(encode(&value, alloc), expected);
    assert_eq!(decode::<Command>(&expected, &Limits::DOCUMENTED, alloc).unwrap(), value);
    let mut missing_owner = expected.clone();
    missing_owner[2] = 0xa3;
    missing_owner.truncate(missing_owner.len() - 35);
    assert!(decode::<Command>(&missing_owner, &Limits::DOCUMENTED, alloc).is_err());
    let mut extra = expected;
    extra[2] = 0xa5;
    extra.extend_from_slice(&[4, 3]);
    assert!(decode::<Command>(&extra, &Limits::DOCUMENTED, alloc).is_err());

    let answer = Answer::OwnedAssignment(OwnedAssignmentAnswer { attempt: Attempt(3) });
    let expected = [0x82u8, 13, 0xa1, 0, 3];
    assert_eq!(encode(&answer, alloc).as_slice(), &expected);
    assert_eq!(decode::<Answer>(&expected, &Limits::DOCUMENTED, alloc).unwrap(), answer);
    let malformed: [&[u8]; 3] = [
        &[0x82, 13, 0xa0],
        &[0x82, 13, 0xa1, 0, 0xf6],
        &[0x82, 13, 0xa2, 0, 3, 1, 0],
    ];
    for bytes in malformed {
        assert!(decode::<Answer>(bytes, &Limits::DOCUMENTED, alloc).is_err());
    }
    let request = ClientFrame::Request(Request {
        epoch: [0x33; 32],
        session: [0x44; 32],
        sequence: 9,
        command: value,
    });
    assert_eq!(
        decode::<ClientFrame>(&encode(&request, alloc), &Limits::DOCUMENTED, alloc).unwrap(),
        request
    );
    let response = ServerFrame::Response(Response {
        epoch: [0x33; 32],
        session: [0x44; 32],
        sequence: 9,
        answer,
    });
    assert_eq!(
        decode::<ServerFrame>(&encode(&response, alloc), &Limits::DOCUMENTED, alloc).unwrap(),
        response
    );
}
