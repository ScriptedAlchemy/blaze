//! Canonical framing/version vectors and exact response binding.

use blaze_dist_local::protocol::*;
use blaze_wire::{Limits, decode, encode};

#[test]
fn hello_golden_separates_transport_global_codec_and_terminal_versions() {
    let value = ClientFrame::Hello(LocalHello {
        transport: 2,
        codec: 1,
        terminal: 2,
        nonce: [0; 32],
        epoch: None,
        resume: None,
    });
    let mut expected = vec![0x82, 0, 0xa6, 0, 2, 1, 1, 2, 2, 3, 0x58, 0x20];
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
    // Transport 2 names a lifetime by the daemon's issued label, and creation by a nonce.
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
